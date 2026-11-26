//! WebDriverAgent background launch, stall detection, recovery, and device log streaming.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ictl_device::{Device, DeviceError, LogStream, Tunnel};
use ictl_wda::{WdaClient, WdaError, WdaRunner, http::DeviceTransport};

use crate::core::{Daemon, Inner};
use crate::detached;
use crate::machine::DeviceState;
use crate::notify::Event;

use super::{ensure_ddi, then_wake};

/// 正常 2～10 秒就绪。手机重启后未授权 automation 时，testmanagerd 握手每次卡 30 秒、要重试一两次，
/// 60 秒常常等不到 runner 启动，也就认不出「需要输入密码」
const WDA_START_TIMEOUT: Duration = Duration::from_secs(120);
/// Waiting for the “Enable UI Automation” passcode: how long one launch waits for the prompt to come up. The prompt
/// stays about 45 s and only some launches show it, so short attempts close together keep it on the phone most of
/// the time (it was up for about 45 s in every 3 minutes, and the user had to catch it). A launch that shows it
/// waits for the passcode as long as the prompt stays
const PIN_ATTEMPT: Duration = Duration::from_secs(25);

/// 接上已在运行的后台 WDA 时等 `/status` 的时限。它本该马上应答，等不到就当它坏了，下一次重新启动
const ADOPT_TIMEOUT: Duration = Duration::from_secs(15);

/// 这次的 WDA 是怎么来的（守护进程总在后台启动它、不持有 XCTest 会话，见 [`crate::detached`]）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detached {
    /// 之前后台启动的、还在运行，直接接上
    Adopted(u64),
    /// 这次后台启动的
    Launched(u64),
}

impl Detached {
    fn pid(self) -> u64 {
        match self {
            Self::Adopted(p) | Self::Launched(p) => p,
        }
    }
}

/// 后台启动 WDA 的结果
pub(crate) enum StartOutcome {
    Ready {
        runner: WdaRunner,
        client: WdaClient<DeviceTransport>,
        mounted: Option<String>,
        /// 后台 WDA 是接上的还是新启动的
        detached: Detached,
    },
    NeedsUnlock,
    /// 开发者镜像没准备好。`mount_failed`：是挂载这一步失败（才可能是缓存坏了）；
    /// 建隧道、查询、下载失败都是 false
    Ddi {
        error: String,
        mount_failed: bool,
    },
    Wda(WdaError),
    /// 后台启动：手机拒绝后台启动 WDA（多半是证书被吊销或还没信任开发者）。和 XCTest 会话启动时的
    /// `unexpected response` 一样处理：查证书，有问题就自动重签
    Refused(String),
}

impl Daemon {
    /// 按客户端记录的连续无应答或长请求后的失败短探测判断是否需恢复。
    /// 尚未判定时再探测一次；正常回答或远端错误都表明仍有响应，会清零计数。
    pub(super) async fn xctest_stalled(&self, g: &mut Inner) -> bool {
        let Some(w) = g.wda.as_ref() else { return false };
        if !w.stalled()
            && let Err(e) = w.xctest_responsive().await
            && e.no_answer()
        {
            tracing::debug!(
                "{}",
                ictl_l10n::log_trf!(
                    "WDA 经 XCTest 的探测没有应答：{e}",
                    "WDA did not answer a probe that goes through XCTest: {e}"
                )
            );
        }
        w.stalled()
    }

    /// WDA 卡死（[`WdaClient::stalled`]）：丢掉会话，重新启动。巡检和每个工具请求开头都会检查：
    /// 工具调用接连不断时巡检抢不到设备，不能只靠它
    pub(crate) async fn restart_stalled_wda(&self, g: &mut Inner) {
        let Some(reason) = g.wda.as_ref().and_then(|w| w.stall_reason()) else { return };
        let why = reason.describe();
        // 请求入口也会走这里，未必刚检查过 /status，不宣称它此刻正常。
        self.log.log(&ictl_l10n::log_trf!(
            "WDA 卡死判定：{why}，重新启动 WDA",
            "WDA is considered stuck: {why}; restarting WDA"
        ));
        // 后台启动的那个不能再接上：忘掉它，重新后台启动时会先结束它
        self.forget_detached_here(g);
        self.update_display(g, None).await;
        self.drop_session(g).await;
        let (Some(device), Some(bundle)) = (g.device.clone(), g.persisted.wda_bundle.clone()) else { return };
        if g.starting.is_none() && g.backoff.ready(Instant::now()) {
            self.spawn_start(g, device, bundle);
        }
    }

    /// Forgets the background WDA recorded for the device in use (it didn't answer): the next background launch
    /// replaces it
    pub(super) fn forget_detached_here(&self, g: &mut Inner) {
        if let Some(udid) = g.device.as_ref().map(|d| d.udid.clone())
            && g.persisted.forget_detached(&udid, None)
        {
            g.persisted.save(&self.cfg.state_file());
        }
    }

    /// Keep an active view and finger through the same transient status misses as the runner.
    pub(crate) async fn confirm_wda_loss(&self, g: &mut Inner, runner_alive: bool) -> bool {
        if runner_alive && !g.wda_misses.hit() {
            return false;
        }
        self.update_display(g, None).await;
        true
    }

    /// 在后台启动 WDA：挂开发者镜像（单独开一条隧道，用完关掉）→ 启动 runner 等就绪。
    /// 最长约 2 分钟（等「Enable UI Automation」密码时），期间不持锁：工具调用照常走 HID 后备路线
    pub(super) fn spawn_start(&self, g: &mut Inner, device: Device, bundle: String) {
        // 正在等密码、等解锁时保持这个状态，免得每次重试都在两个状态间来回切、刷日志
        let waiting_pin = g.state == DeviceState::NeedsAutomationPin;
        if !matches!(g.state, DeviceState::NeedsAutomationPin | DeviceState::NeedsUnlock) {
            self.set_state(g, DeviceState::WdaStarting { last_error: None });
        }
        let start_timeout = if waiting_pin { PIN_ATTEMPT } else { WDA_START_TIMEOUT };
        // while waiting for the passcode it is launched again every half minute: once in the event log is enough
        if !waiting_pin {
            self.log.log(ictl_l10n::log_tr!(
                "启动 WDA（后台启动，不持有 XCTest 会话）",
                "Starting WDA (launched in the background, no XCTest session)"
            ));
        }
        let known = g.persisted.adoptable_for(&device.udid);
        if known.is_none()
            && let Some(pid) = g.persisted.detached_for(&device.udid)
        {
            self.log.log(&ictl_l10n::log_trf!(
                "手机上的后台 WDA（进程 {pid}）是旧版本启动的，对手机的所有网卡开放：重新启动，只听手机本机",
                "The background WDA on the phone (pid {pid}) was launched by an older version and listens on every interface of the phone: launching it again, listening on the phone's loopback only"
            ));
        }
        let legacy = ictl_device::ddi::legacy(g.info.as_ref().map_or(0, |i| i.major_version()));
        let (cache, url, user_dir) =
            (self.cfg.ddi_cache_for(legacy), self.cfg.ddi_url.clone(), self.cfg.ddi_dir.clone());
        let Ok(lease) = crate::maintenance::shared() else { return };
        let wake = self.wake.clone();
        g.starting = Some(tokio::task::spawn_local(then_wake(wake, async move {
            let _lease = lease;
            // 手机重启后开发者镜像可能掉了；已挂载时只是一次查询。错误带上「是不是挂载这一步」。
            // 镜像好了再在同一条隧道上处理后台 WDA（见 prepare_detached）
            // 挂上的镜像版本、后台 WDA 的处理结果、启动前打开的系统日志流
            type Ready = (Option<String>, Result<Detached, DeviceError>, Option<LogStream>);
            let ddi: Result<Ready, (DeviceError, bool)> = async {
                let mut t = device.open_tunnel().await.map_err(|e| (e, false))?;
                let r = ensure_ddi(&device, &mut t, legacy, &cache, &url, user_dir.as_deref()).await;
                let r = match r {
                    Ok(m) => {
                        if m.is_some() {
                            // a tunnel opened before the image was mounted doesn't list its services
                            // (dtservicehub): a new one, so WDA starts on the first attempt
                            t.close().await;
                            t = device.open_tunnel().await.map_err(|e| (e, false))?;
                        }
                        // 后台启动前先开系统日志流：认「Enable UI Automation」密码框靠它（见 WdaRunner::attach）
                        let log = open_log(&device).await;
                        Ok((m, prepare_detached(&mut t, known, &bundle).await, log))
                    }
                    Err(e) => Err(e),
                };
                t.close().await;
                r
            }
            .await;
            let (mounted, prepared, log) = match ddi {
                Ok(v) => v,
                Err((DeviceError::PasswordProtected, _)) => return StartOutcome::NeedsUnlock,
                Err((e, mount_failed)) => return StartOutcome::Ddi { error: e.to_string(), mount_failed },
            };
            let prepared = match prepared {
                Ok(p) => p,
                Err(e @ DeviceError::LaunchRefused(_)) => {
                    return StartOutcome::Refused(ictl_l10n::trf!(
                        "后台启动 WDA 失败：{e}（第一次在这台设备上装 WDA 的：到 设置 → 通用 → VPN 与设备管理 里信任你的 Apple ID 开发者；已经信任过的，多半是签名证书被吊销了：运行 iphone-control sign 重签这台）",
                        "Launching WDA in the background failed: {e} (first WDA install on this device: trust your Apple ID developer under Settings → General → VPN & Device Management; already trusted: the signing certificate was most likely revoked, run iphone-control sign to re-sign this device)"
                    ));
                }
                Err(e) => {
                    return StartOutcome::Wda(WdaError::StartFailed(ictl_l10n::textf!(
                        "后台启动 WDA 失败：{e}",
                        "Launching WDA in the background failed: {e}"
                    )));
                }
            };
            let started = match prepared {
                Detached::Adopted(_) => WdaRunner::attach(device.provider(), ADOPT_TIMEOUT, None).await,
                Detached::Launched(_) => WdaRunner::attach(device.provider(), start_timeout, log).await,
            };
            match started {
                Ok((runner, client)) => StartOutcome::Ready { runner, client, mounted, detached: prepared },
                // still waiting for the passcode; this launch just didn't bring the prompt up: try again soon
                Err(WdaError::NotReadyInTime(_)) if waiting_pin => StartOutcome::Wda(WdaError::NeedsAutomationPin),
                Err(e) => StartOutcome::Wda(e),
            }
        })));
    }

    /// 后台启动做完了就收结果
    pub(super) async fn poll_start(self: &Arc<Self>, g: &mut Inner) {
        if !g.starting.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        let h = g.starting.take().expect("刚检查过");
        let outcome = h.await.unwrap_or_else(|e| {
            StartOutcome::Wda(WdaError::StartFailed(ictl_l10n::textf!("启动任务异常：{e}", "Start task failed: {e}")))
        });
        // 后台启动被拒绝和 XCTest 会话启动被拒绝（unexpected response）走同一条路：记失败，查证书
        match outcome {
            StartOutcome::Ready { runner, mut client, mounted, detached } => {
                if let Some(v) = mounted {
                    self.log.log(&ictl_l10n::log_trf!("开发者镜像已挂载 {v}", "Developer disk image mounted {v}"));
                }
                match detached {
                    Detached::Adopted(pid) => self.log.log(&ictl_l10n::log_trf!(
                        "接上手机上已在运行的后台 WDA（进程 {pid}），没有重新启动",
                        "Attached to the background WDA already running on the phone (pid {pid}) without restarting it"
                    )),
                    Detached::Launched(pid) => self.log.log(&ictl_l10n::log_trf!(
                        "已在后台启动 WDA（进程 {pid}，不持有 XCTest 会话）",
                        "Launched WDA in the background (pid {pid}, no XCTest session)"
                    )),
                }
                // this device's record only: the other devices' background WDAs keep running and are attached to
                // again when switched back to
                if let Some(udid) = g.device.as_ref().map(|d| d.udid.clone())
                    && g.persisted.record_detached(&udid, detached.pid())
                {
                    g.persisted.save(&self.cfg.state_file());
                }
                g.dev.ddi_failures = 0;
                g.runner = Some(runner);
                // the live viewer's picture at 60 fps, so it follows the pointer (see `WdaClient::fast_stream`)
                client.fast_stream();
                g.wda = Some(client);
                // 状态在下面 online() 的健康检查里变成 Ready
            }
            StartOutcome::NeedsUnlock => {
                // 不退避的话，紧接着的 online() 会立刻重新启动，状态被改回「正在启动」，用户看不到要解锁
                g.backoff.retry_after(Instant::now(), crate::machine::UNLOCK_RETRY);
                self.set_state(g, DeviceState::NeedsUnlock);
            }
            StartOutcome::Ddi { error: e, mount_failed } => {
                // 只有挂载失败才可能是缓存坏了；ICTL_DDI_DIR 是用户自己的文件，从不删
                self.ddi_failed(g, mount_failed);
                g.backoff.failed(Instant::now());
                self.set_state(
                    g,
                    DeviceState::WdaStarting {
                        last_error: Some(ictl_l10n::trf!("开发者镜像：{e}", "Developer disk image: {e}")),
                    },
                );
            }
            StartOutcome::Wda(WdaError::NeedsAutomationPin) => {
                g.backoff.waiting_for_pin(Instant::now());
                self.set_state(g, DeviceState::NeedsAutomationPin);
                self.notify(g, Event::NeedsAutomationPin);
            }
            failed @ (StartOutcome::Wda(_) | StartOutcome::Refused(_)) => {
                // 后台启动被拒绝和 XCTest 会话启动被拒绝（unexpected response）走同一条路：记失败，查证书
                let (e, refused_in_background) = match failed {
                    StartOutcome::Refused(m) => (WdaError::StartFailed(m.into()), true),
                    StartOutcome::Wda(e) => (e, false),
                    _ => unreachable!("上面只匹配了这两种"),
                };
                g.backoff.failed(Instant::now());
                // 接上的后台 WDA 没应答：忘掉它，下一次重新后台启动（会先结束它）
                self.forget_detached_here(g);
                // StartFailed 自己就以「WDA 启动失败：」开头，日志和状态里再加一遍就重复了
                let e = match e {
                    WdaError::StartFailed(m) | WdaError::NotReadyInTime(m) => m.to_string(),
                    e => e.to_string(),
                };
                self.log.log(&ictl_l10n::log_trf!("WDA 启动失败：{e}", "WDA failed to start: {e}"));
                let refused = refused_in_background || ictl_wda::runner::launch_refused(&e);
                self.set_state(g, DeviceState::WdaStarting { last_error: Some(e) });
                if refused {
                    self.maybe_heal_certificate(g);
                }
            }
        }
    }
}

/// 在挂镜像的隧道上准备 WDA：`known`（这台设备上记下的后台 WDA）那个进程还是 WDA 就直接接上；否则在后台启动一个
/// （会先结束已在运行的 WDA，包括旧版本用 XCTest 会话启动的）
async fn prepare_detached(t: &mut Tunnel, known: Option<u64>, bundle: &str) -> Result<Detached, DeviceError> {
    match known {
        Some(pid) if detached::is_wda(&t.processes().await?, pid) => Ok(Detached::Adopted(pid)),
        _ => Ok(Detached::Launched(t.launch_in_background(bundle, &ictl_wda::runner::launch_env()).await?)),
    }
}

/// 打开手机系统日志流；打不开只记一笔（后台启动照样进行，只是认不出密码框）
async fn open_log(device: &Device) -> Option<LogStream> {
    match device.log_stream().await {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!(
                    "打不开手机系统日志，认不出「Enable UI Automation」密码框：{e}",
                    "Couldn't open the phone's log, so the “Enable UI Automation” passcode prompt can't be detected: {e}"
                )
            );
            None
        }
    }
}
