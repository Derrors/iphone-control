//! 启动手机上的 WebDriverAgent（XCTest runner）。
//!
//! 用 idevice 的 `XCUITestService` 跑测试计划；它自己经 usbmuxd 建 CoreDeviceProxy 隧道，和设备层的隧道互不影响。
//! 就绪判定 = 本进程持有的 runner 任务还活着 **且** `/status` ready（手机上残留的 WDA 在 runner 退出后还会响应几十秒）。
//! 启动时还要等新 runner 开始执行测试计划后才问 `/status`：在那之前应答的只可能是残留的旧 WDA。
//!
//! 实验（`ICTL_WDA_DETACHED=1`）：守护进程在后台启动 runner、不持有 XCTest 会话（见 `ictl_device::Tunnel::launch_in_background`），
//! 这里只用 [`WdaRunner::attach`] 等 `/status` ready。这样的 runner 不随主机进程退出，就绪判定只看 `/status`。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use idevice::{
    IdeviceError, IdeviceService,
    dvt::xctest::{TestConfig, XCUITestService, listener::XCUITestListener},
    installation_proxy::InstallationProxyClient,
    provider::IdeviceProvider,
};
use tokio::task::JoinHandle;

use ictl_device::LogStream;

use crate::http::DeviceTransport;
use crate::{Result, WdaClient, WdaError, is_automation_pin_error};

pub const WDA_PORT: u16 = 8100;

/// runner 等「Enable UI Automation」密码的时长是 60 秒（runner 自己的超时），多留 5 秒收它的报错
const AUTOMATION_WAIT: Duration = Duration::from_secs(65);
/// 连接 installation_proxy、查 runner 的安装信息的时限（idevice 读应答没有超时）
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);
/// 一直没收到「开始执行测试计划」回调时，过了这么久也开始问 `/status`：
/// 万一某个 iOS 版本上 idevice 解不出这个回调，不能因此永远起不来
const PLAN_START_FALLBACK: Duration = Duration::from_secs(30);
/// testmanagerd 开始启用 automation mode 后这么久还没完成，才在日志里说 runner 在等密码（已授权时同一秒就完成）
const AUTOMATION_PROMPT_AFTER: Duration = Duration::from_secs(3);

/// runner 调试日志里和「Enable UI Automation」相关的信号。
/// 手机重启后 runner 一启动就进入 `enabling automation mode` 等待（手机上弹密码框），60 秒没输密码报
/// `Timed out while enabling automation mode.`。这个报错经 `_XCT_initializationForUITestingDidFailWithError:`
/// 发回，idevice 解码失败（只给出 unexpected response，不调回调），所以从调试日志里认
#[derive(Debug, PartialEq)]
enum AutomationSignal {
    /// 开始等 automation mode（已授权时会马上结束）
    Waiting,
    /// 等待结束（成功或超时都有这一行）
    Done,
    /// UI 测试初始化失败，消息里带原因
    Failed,
}

/// 手机系统日志里 testmanagerd 关于 automation mode 的信号。后台启动的 runner 没有 XCTest 会话，收不到上面那些
/// runner 调试日志（它们不进系统日志），只能看 testmanagerd 的：已授权时「Enabling…」和「Finished…」在同一秒；
/// 手机重启后要先在手机上输密码，输之前一直没有「Finished」。字符串来自开发者镜像 27A5228h 里的 testmanagerd
fn testmanagerd_signal(msg: &str) -> Option<AutomationSignal> {
    if msg.to_ascii_lowercase().starts_with("failed to enable automation mode") {
        Some(AutomationSignal::Failed)
    } else if msg.starts_with("Enabling Automation Mode") {
        Some(AutomationSignal::Waiting)
    } else if msg.starts_with("Finished enabling Automation Mode") {
        Some(AutomationSignal::Done)
    } else {
        None
    }
}

/// 后台读系统日志的任务，丢掉就停
struct Watcher(JoinHandle<()>);

impl Drop for Watcher {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 读手机系统日志，把 testmanagerd 的 automation mode 信号记进 `progress`
fn watch_automation(mut log: LogStream, progress: Arc<Mutex<Progress>>) -> Watcher {
    Watcher(tokio::spawn(async move {
        while let Ok(e) = log.next().await {
            if !e.process.ends_with("/testmanagerd") {
                continue;
            }
            let signal = testmanagerd_signal(&e.message);
            if signal.is_some() {
                tracing::debug!("testmanagerd: {}", e.message);
            }
            let mut p = progress.lock().unwrap();
            match signal {
                Some(AutomationSignal::Waiting) => {
                    p.automation_wait.get_or_insert_with(tokio::time::Instant::now);
                }
                Some(AutomationSignal::Done) => p.automation_wait = None,
                Some(AutomationSignal::Failed) => p.errors.push(e.message),
                None => {}
            }
        }
    }))
}

fn automation_signal(msg: &str) -> Option<AutomationSignal> {
    if msg.contains("Failed to initialize for UI testing") {
        Some(AutomationSignal::Failed)
    } else if msg.contains("entering wait loop") && msg.contains("enabling automation mode") {
        Some(AutomationSignal::Waiting)
    } else if msg.contains("cleaning up enabling automation mode") {
        Some(AutomationSignal::Done)
    } else {
        None
    }
}

/// 设备拒绝启动 runner（见 [`launch_hint`]）：还没信任开发者，或签 WDA 的证书已被吊销。守护进程据此判断要不要重签
pub fn launch_refused(msg: &str) -> bool {
    msg.contains("unexpected response")
}

/// 手机拒绝启动 runner 时回的是 NSError，但 idevice 收到的是没解开的归档（字段都是 `Uid(n)` 引用），读不出文字，
/// 只报 `unexpected response`。首次安装后最常见的原因是还没信任开发者（iPad mini 5 / iPadOS 26.6.1 真机：
/// 点「信任」之前这样报，之后下一次就能启动）；用过的设备则可能是签 WDA 的证书变了（给另一台设备重签时换了证书，
/// 同一台 iPad 上点 runner 图标提示「无法验证 App」，重签这台后就能启动），两种都在报错里提示一句
fn launch_hint(msg: String) -> String {
    if launch_refused(&msg) {
        ictl_l10n::trf!(
            "{msg}(第一次在这台设备上装 WDA 的:到 设置 → 通用 → VPN 与设备管理 里信任你的 Apple ID 开发者,需要联网;已经信任过的,可能是签名证书变了:运行 iphone-control sign 重签这台)",
            "{msg} (first WDA install on this device: trust your Apple ID developer under Settings → General → VPN & Device Management, which needs internet; already trusted: the signing certificate may have changed, run iphone-control sign to re-sign this device)"
        )
    } else {
        msg
    }
}

/// XCTest 过程中收集的信息：报错文字（识别密码框超时用），以及 runner 是否正在等 automation mode
#[derive(Default)]
struct Progress {
    errors: Vec<String>,
    automation_wait: Option<tokio::time::Instant>,
    /// 新 runner 已开始执行测试计划（WDA 的 HTTP 服务在测试里启动，一定晚于它）
    plan_started: bool,
}

struct ErrorListener(Arc<Mutex<Progress>>);

impl ErrorListener {
    fn push(&self, s: &str) {
        self.0.lock().unwrap().errors.push(s.to_owned());
    }
}

// 其余回调只记 debug 日志，用于排查启动卡住
impl XCUITestListener for ErrorListener {
    async fn log_message(&mut self, message: &str) -> std::result::Result<(), IdeviceError> {
        tracing::debug!("xctest log: {message}");
        Ok(())
    }

    async fn log_debug_message(&mut self, message: &str) -> std::result::Result<(), IdeviceError> {
        tracing::debug!("xctest debug: {message}");
        let mut p = self.0.lock().unwrap();
        match automation_signal(message) {
            Some(AutomationSignal::Waiting) => p.automation_wait = Some(tokio::time::Instant::now()),
            Some(AutomationSignal::Done) => p.automation_wait = None,
            Some(AutomationSignal::Failed) => p.errors.push(message.to_owned()),
            None => {}
        }
        Ok(())
    }

    async fn did_begin_initializing_for_ui_testing(&mut self) -> std::result::Result<(), IdeviceError> {
        tracing::debug!("{}", ictl_l10n::log_tr!("xctest: 开始初始化 UI 测试", "xctest: initializing the UI test"));
        Ok(())
    }

    async fn test_bundle_ready(&mut self) -> std::result::Result<(), IdeviceError> {
        tracing::debug!("xctest: test bundle ready");
        Ok(())
    }

    async fn test_runner_ready_with_capabilities(&mut self) -> std::result::Result<(), IdeviceError> {
        tracing::debug!("xctest: runner ready");
        Ok(())
    }

    async fn did_begin_executing_test_plan(&mut self) -> std::result::Result<(), IdeviceError> {
        tracing::debug!("{}", ictl_l10n::log_tr!("xctest: 开始执行测试计划", "xctest: running the test plan"));
        self.0.lock().unwrap().plan_started = true;
        Ok(())
    }

    async fn initialization_for_ui_testing_did_fail(
        &mut self,
        description: &str,
    ) -> std::result::Result<(), IdeviceError> {
        self.push(description);
        Ok(())
    }

    async fn did_fail_to_bootstrap(&mut self, description: &str) -> std::result::Result<(), IdeviceError> {
        self.push(description);
        Err(IdeviceError::UnexpectedResponse(format!("test runner failed to bootstrap: {description}")))
    }
}

pub struct WdaRunner {
    /// None：后台启动的 runner，本进程没有 XCTest 会话（[`WdaRunner::attach`]）
    task: Option<JoinHandle<std::result::Result<(), IdeviceError>>>,
}

impl WdaRunner {
    /// 启动 runner 并等到 `/status` ready。`bundle_id` 是签名后带团队 ID 的 xctrunner Bundle ID。
    pub async fn start(
        provider: Arc<dyn IdeviceProvider>,
        bundle_id: &str,
        timeout: Duration,
    ) -> Result<(Self, WdaClient<DeviceTransport>)> {
        let lookup = async {
            let mut ip =
                InstallationProxyClient::connect(&*provider).await.map_err(|e| WdaError::Unavailable(e.to_string()))?;
            TestConfig::from_installation_proxy(&mut ip, bundle_id, None).await.map_err(|e| match e {
                IdeviceError::AppNotInstalled => WdaError::NotInstalled(bundle_id.to_owned()),
                other => WdaError::StartFailed(other.to_string()),
            })
        };
        let cfg = tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await.map_err(|_| {
            WdaError::StartFailed(ictl_l10n::trf!(
                "查 WDA 安装信息超时（{} 秒）",
                "querying the WDA install info timed out ({} s)",
                LOOKUP_TIMEOUT.as_secs()
            ))
        })??;

        let progress = Arc::new(Mutex::new(Progress::default()));
        let task = {
            let (provider, progress) = (provider.clone(), progress.clone());
            tokio::spawn(
                async move { XCUITestService::new(provider).run(cfg, &mut ErrorListener(progress), None).await },
            )
        };
        let mut runner = Self { task: Some(task) };
        let client = WdaClient::new(DeviceTransport::new(provider, WDA_PORT));

        let failure = |extra: Option<String>| {
            let p = progress.lock().unwrap();
            let mut text = p.errors.clone();
            text.extend(extra);
            let joined = text.join("; ");
            // 报错文字没收到、但 runner 停在等 automation mode 也算
            if is_automation_pin_error(&joined) || p.automation_wait.is_some() {
                WdaError::NeedsAutomationPin
            } else {
                WdaError::StartFailed(launch_hint(joined))
            }
        };

        let t0 = tokio::time::Instant::now();
        let mut deadline = t0 + timeout;
        let mut announced = false;
        let mut fallback_logged = false;
        loop {
            // runner 在等手机上输密码：等满它自己的 60 秒，期间输了密码就能直接就绪
            if let Some(since) = progress.lock().unwrap().automation_wait {
                deadline = deadline.max(since + AUTOMATION_WAIT);
                if !announced {
                    announced = true;
                    tracing::info!(
                        "{}",
                        ictl_l10n::log_tr!(
                            "runner 在等手机上的「Enable UI Automation」密码",
                            "the runner is waiting for the “Enable UI Automation” passcode on the phone"
                        )
                    );
                }
            }
            let task = runner.task.as_mut().expect("XCTest 会话");
            if task.is_finished() {
                let r = task.await;
                let msg = match r {
                    Ok(Ok(())) => ictl_l10n::tr!("runner 提前结束", "the runner ended early").to_owned(),
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                };
                return Err(failure(Some(msg)));
            }
            let plan_started = progress.lock().unwrap().plan_started;
            if !plan_started && t0.elapsed() >= PLAN_START_FALLBACK && !fallback_logged {
                fallback_logged = true;
                tracing::info!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "{} 秒内没收到 runner 开始执行测试计划的回调，改为直接查 /status",
                        "No callback that the runner started the test plan within {} seconds; querying /status directly",
                        PLAN_START_FALLBACK.as_secs()
                    )
                );
            }
            if (plan_started || fallback_logged) && client.ready().await.unwrap_or(false) {
                return Ok((runner, client));
            }
            if tokio::time::Instant::now() >= deadline {
                runner.stop();
                return Err(
                    match failure(Some(
                        ictl_l10n::tr!("WDA 在限定时间内没有就绪", "WDA didn't become ready in time").to_owned(),
                    )) {
                        WdaError::StartFailed(m) => WdaError::NotReadyInTime(m),
                        e => e,
                    },
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// 接上一个本进程没有 XCTest 会话的 runner（后台启动的，或上一个守护进程启动后留下的）：等 `/status` ready。
    ///
    /// `log`：启动 runner **之前**打开的手机系统日志流。有它就能认出 runner 卡在「Enable UI Automation」密码框
    /// （testmanagerd 开始启用 automation mode 后迟迟没完成），像 [`Self::start`] 一样等满 runner 的 60 秒、
    /// 等不到报 [`WdaError::NeedsAutomationPin`]；没有就只能按超时报错
    pub async fn attach(
        provider: Arc<dyn IdeviceProvider>,
        timeout: Duration,
        log: Option<LogStream>,
    ) -> Result<(Self, WdaClient<DeviceTransport>)> {
        let client = WdaClient::new(DeviceTransport::new(provider, WDA_PORT));
        let watching = log.is_some();
        let progress = Arc::new(Mutex::new(Progress::default()));
        let _watcher = log.map(|l| watch_automation(l, progress.clone()));
        let mut deadline = tokio::time::Instant::now() + timeout;
        let mut announced = false;
        loop {
            let (wait, failed) = {
                let p = progress.lock().unwrap();
                (p.automation_wait, p.errors.first().cloned())
            };
            // testmanagerd 启用 automation mode 失败：runner 起不来了（多半是密码框等超时或被取消）
            if let Some(e) = failed {
                tracing::info!("testmanagerd: {e}");
                return Err(WdaError::NeedsAutomationPin);
            }
            if let Some(since) = wait {
                deadline = deadline.max(since + AUTOMATION_WAIT);
                if !announced && since.elapsed() >= AUTOMATION_PROMPT_AFTER {
                    announced = true;
                    tracing::info!(
                        "{}",
                        ictl_l10n::log_tr!(
                            "后台启动的 runner 在等手机上的「Enable UI Automation」密码",
                            "the runner launched in the background is waiting for the “Enable UI Automation” passcode on the phone"
                        )
                    );
                }
            }
            if client.ready().await.unwrap_or(false) {
                return Ok((Self { task: None }, client));
            }
            if tokio::time::Instant::now() >= deadline {
                if wait.is_some() {
                    return Err(WdaError::NeedsAutomationPin);
                }
                return Err(WdaError::NotReadyInTime(if watching {
                    ictl_l10n::trf!(
                        "后台启动的 WDA {} 秒内没有就绪",
                        "WDA launched in the background wasn't ready within {} s",
                        timeout.as_secs()
                    )
                } else {
                    ictl_l10n::trf!(
                        "后台启动的 WDA {} 秒内没有就绪（手机重启后第一次启动要在手机上输入密码，允许 UI 自动化）",
                        "WDA launched in the background wasn't ready within {} s (after a reboot the phone asks for its passcode to allow UI automation)",
                        timeout.as_secs()
                    )
                }));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// runner 任务是否还活着。死了就算手机上的 WDA 还在响应也不能用。
    /// 后台启动的 runner 没有任务可看，总是 true，健康只看 `/status`
    pub fn alive(&self) -> bool {
        self.task.as_ref().is_none_or(|t| !t.is_finished())
    }

    /// 是不是后台启动、本进程没有 XCTest 会话的 runner
    pub fn detached(&self) -> bool {
        self.task.is_none()
    }

    /// 停掉 runner（即丢掉它，见 Drop）
    pub fn stop(self) {}
}

/// 丢掉就停：守护进程的后台启动任务可能在 start() 中途被取消，不能留下一个没人管的 XCTest 会话。
/// 后台启动的 runner 不停（它本来就不依赖本进程），要停得结束手机上的进程
impl Drop for WdaRunner {
    fn drop(&mut self) {
        if let Some(t) = &self.task {
            t.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_developer_hint_only_for_unexpected_response() {
        let m = launch_hint("unexpected response from device: unexpected response".into());
        assert!(m.contains("VPN 与设备管理") && m.contains("iphone-control sign"), "{m}");
        assert_eq!(launch_hint("WDA 在限定时间内没有就绪".into()), "WDA 在限定时间内没有就绪");
        // 守护进程据此决定要不要查证书、自动重签：带不带提示都认
        assert!(launch_refused("unexpected response from device: unexpected response"));
        assert!(launch_refused(&m));
        assert!(!launch_refused("WDA 在限定时间内没有就绪"));
    }

    #[test]
    fn automation_signals_from_runner_debug_log() {
        // iOS 27.0.1 手机重启后真机日志（去掉时间和进程号前缀）
        let wait =
            "<XCTWaiter: 0x1089cebb0> entering wait loop for 60.00s with expectations: `enabling automation mode`";
        let done = "cleaning up enabling automation mode";
        let fail = "Failed to initialize for UI testing: Error Domain=com.apple.dt.XCTest.XCTFuture Code=1000 \
                    \"Timed out while enabling automation mode.\" UserInfo={NSLocalizedDescription=Timed out while enabling automation mode.}";
        assert_eq!(automation_signal(wait), Some(AutomationSignal::Waiting));
        assert_eq!(automation_signal(done), Some(AutomationSignal::Done));
        assert_eq!(automation_signal(fail), Some(AutomationSignal::Failed));
        assert!(is_automation_pin_error(fail));
        // 创建 future 那一行不算开始等待（已授权时也有）
        assert_eq!(automation_signal("Creating future for 'enabling automation mode' with timeout 60.00"), None);
        assert_eq!(automation_signal("Loading test bundle at: file:///..."), None);
    }

    #[test]
    fn automation_signals_from_testmanagerd_log() {
        // iOS 27.0.1 真机系统日志里 testmanagerd 的原文（已授权时这两行在同一秒）
        assert_eq!(testmanagerd_signal("Enabling Automation Mode..."), Some(AutomationSignal::Waiting));
        assert_eq!(testmanagerd_signal("Finished enabling Automation Mode"), Some(AutomationSignal::Done));
        // 失败的格式串来自开发者镜像 27A5228h 里的 testmanagerd
        for m in [
            "Failed to enable automation mode for test session with pid 3869: Error Domain=x Code=1",
            "Failed to enable Automation Mode: Error Domain=x Code=1",
        ] {
            assert_eq!(testmanagerd_signal(m), Some(AutomationSignal::Failed), "{m}");
        }
        for m in [
            "Test session with pid 3869 requesting automation mode",
            "Automation already enabled, nothing to do.",
            "Disabling Automation Mode...",
            "Finished disabling Automation Mode",
            "Added Automation Mode client <XCTestSession: 0x7a828cabc0> pid 3869. Total clients: 1",
        ] {
            assert_eq!(testmanagerd_signal(m), None, "{m}");
        }
    }
}
