//! 启动手机上的 WebDriverAgent（XCTest runner）。
//!
//! 用 idevice 的 `XCUITestService` 跑测试计划；它自己经 usbmuxd 建 CoreDeviceProxy 隧道，和设备层的隧道互不影响。
//! 就绪判定 = 本进程持有的 runner 任务还活着 **且** `/status` ready（手机上残留的 WDA 在 runner 退出后还会响应几十秒）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use idevice::{
    IdeviceError, IdeviceService,
    dvt::xctest::{TestConfig, XCUITestService, listener::XCUITestListener},
    installation_proxy::InstallationProxyClient,
    provider::IdeviceProvider,
};
use tokio::task::JoinHandle;

use crate::http::DeviceTransport;
use crate::{Result, WdaClient, WdaError, is_automation_pin_error};

pub const WDA_PORT: u16 = 8100;

/// runner 等「Enable UI Automation」密码的时长是 60 秒（runner 自己的超时），多留 5 秒收它的报错
const AUTOMATION_WAIT: Duration = Duration::from_secs(65);

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

/// XCTest 过程中收集的信息：报错文字（识别密码框超时用），以及 runner 是否正在等 automation mode
#[derive(Default)]
struct Progress {
    errors: Vec<String>,
    automation_wait: Option<tokio::time::Instant>,
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
        tracing::debug!("xctest: 开始初始化 UI 测试");
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
        tracing::debug!("xctest: 开始执行测试计划");
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
    pub bundle_id: String,
    task: JoinHandle<std::result::Result<(), IdeviceError>>,
}

impl WdaRunner {
    /// 启动 runner 并等到 `/status` ready。`bundle_id` 是签名后带团队 ID 的 xctrunner Bundle ID。
    pub async fn start(
        provider: Arc<dyn IdeviceProvider>,
        bundle_id: &str,
        timeout: Duration,
    ) -> Result<(Self, WdaClient<DeviceTransport>)> {
        let mut ip =
            InstallationProxyClient::connect(&*provider).await.map_err(|e| WdaError::Unavailable(e.to_string()))?;
        let cfg = TestConfig::from_installation_proxy(&mut ip, bundle_id, None).await.map_err(|e| match e {
            IdeviceError::AppNotInstalled => WdaError::NotInstalled(bundle_id.to_owned()),
            other => WdaError::StartFailed(other.to_string()),
        })?;

        let progress = Arc::new(Mutex::new(Progress::default()));
        let task = {
            let (provider, progress) = (provider.clone(), progress.clone());
            tokio::spawn(
                async move { XCUITestService::new(provider).run(cfg, &mut ErrorListener(progress), None).await },
            )
        };
        let runner = Self { bundle_id: bundle_id.to_owned(), task };
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
                WdaError::StartFailed(joined)
            }
        };

        let mut deadline = tokio::time::Instant::now() + timeout;
        let mut announced = false;
        loop {
            // runner 在等手机上输密码：等满它自己的 60 秒，期间输了密码就能直接就绪
            if let Some(since) = progress.lock().unwrap().automation_wait {
                deadline = deadline.max(since + AUTOMATION_WAIT);
                if !announced {
                    announced = true;
                    tracing::info!("runner 在等手机上的「Enable UI Automation」密码");
                }
            }
            if runner.task.is_finished() {
                let r = runner.task.await;
                let msg = match r {
                    Ok(Ok(())) => "runner 提前结束".to_owned(),
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                };
                return Err(failure(Some(msg)));
            }
            if client.ready().await.unwrap_or(false) {
                return Ok((runner, client));
            }
            if tokio::time::Instant::now() >= deadline {
                runner.task.abort();
                return Err(failure(Some("WDA 在限定时间内没有就绪".to_owned())));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// runner 任务是否还活着。死了就算手机上的 WDA 还在响应也不能用
    pub fn alive(&self) -> bool {
        !self.task.is_finished()
    }

    pub fn stop(self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
