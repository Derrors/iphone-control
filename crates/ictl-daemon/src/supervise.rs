//! 巡检和后台任务：每 10 秒推进状态机、后台启动 WDA、重签（含证书出问题时自动重签）、Apple ID 会话检查。

use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use ictl_device::{Device, DeviceError, LogStream, Rotation, ScreenSize, Tunnel, UsbDevice};
use ictl_wda::{WdaClient, WdaError, WdaRunner, http::DeviceTransport};

use crate::core::{Daemon, DetachedWda, DeviceProfile, Inner, PerDevice, close_hid};
use crate::machine::{DeviceState, WdaStartHint};
use crate::notify::Event;

/// Three 10-second supervisor intervals. A missing background sample within this
/// window can leave a verified gesture running, but can never authorize a new one.
const DISPLAY_SAMPLE_GRACE: Duration = Duration::from_secs(30);
/// 正常 2～10 秒就绪。手机重启后未授权 automation 时，testmanagerd 握手每次卡 30 秒、要重试一两次，
/// 60 秒常常等不到 runner 启动，也就认不出「需要输入密码」
const WDA_START_TIMEOUT: Duration = Duration::from_secs(120);
/// Waiting for the “Enable UI Automation” passcode: how long one launch waits for the prompt to come up. The prompt
/// stays about 45 s and only some launches show it, so short attempts close together keep it on the phone most of
/// the time (it was up for about 45 s in every 3 minutes, and the user had to catch it). A launch that shows it
/// waits for the passcode as long as the prompt stays
const PIN_ATTEMPT: Duration = Duration::from_secs(25);
/// HID 会话空闲这么久就关掉。它持有屏幕媒体流，开着时 iOS 当作屏幕被远程查看：不会自动锁屏，
/// 还会弹「viewed remotely」通知。重开约 0.5 秒
pub(crate) const HID_IDLE: Duration = Duration::from_secs(30);
/// 连续这么多轮列不出设备才当作离线
const LIST_ERRORS_OFFLINE: u32 = 3;
/// 开发者镜像连续挂载失败这么多次就清缓存重新下载
const DDI_FAILURES_INVALIDATE: u32 = 3;
/// 整个重签（登录会话、注册设备、签名、上传安装）的时限：苹果服务器或安装卡住时不能永远「正在重签」
const RESIGN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Apple ID 会话检查的时限
const SESSION_CHECK_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// 换了设备或重新接上后，这么久内就检查一次要不要重签（离线期间可能过期了，不能等满 RESIGN_CHECK_HOURS）
const RESIGN_CHECK_AFTER_CONNECT: Duration = Duration::from_secs(60);
/// 因为证书自动重签同一台设备的最短间隔：重签也治不好时（例如其实是没信任开发者）不反复签
pub(crate) const CERT_HEAL_EVERY: Duration = Duration::from_secs(6 * 3600);
/// 实验开关：接上已在运行的后台 WDA 时等 `/status` 的时限。它本该马上应答，等不到就当它坏了，下一次重新启动
const ADOPT_TIMEOUT: Duration = Duration::from_secs(15);

/// 实验开关（`ICTL_WDA_DETACHED`）下，这次的 WDA 是怎么来的
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
        /// 实验开关开着时：后台 WDA 是接上的还是新启动的
        detached: Option<Detached>,
    },
    NeedsUnlock,
    /// 开发者镜像没准备好。`mount_failed`：是挂载这一步失败（才可能是缓存坏了）；
    /// 建隧道、查询、下载失败都是 false
    Ddi {
        error: String,
        mount_failed: bool,
    },
    Wda(WdaError),
    /// 实验开关：手机拒绝后台启动 WDA（多半是证书被吊销或还没信任开发者）。和 XCTest 会话启动时的
    /// `unexpected response` 一样处理：查证书，有问题就自动重签
    Refused(String),
}

impl PerDevice {
    fn starting_wda(&self) -> DeviceState {
        DeviceState::WdaStarting {
            last_error: None,
            hint: self.awaiting_developer_trust.then_some(WdaStartHint::TrustDeveloper),
        }
    }

    fn wda_failed(&mut self, error: String, refused: bool, missing: bool) -> DeviceState {
        let explicit_trust = ictl_wda::runner::developer_trust_required(&error);
        let hint = if missing {
            self.awaiting_developer_trust = false;
            Some(WdaStartHint::InstallWda)
        } else if explicit_trust || (refused && self.awaiting_developer_trust) {
            self.awaiting_developer_trust = true;
            Some(WdaStartHint::TrustDeveloper)
        } else if refused {
            Some(WdaStartHint::LaunchRefused)
        } else {
            None
        };
        DeviceState::WdaStarting { last_error: Some(error), hint }
    }
}

/// `work`, then a supervision round right away ([`Daemon::wake`])
async fn then_wake<T>(wake: Arc<tokio::sync::Notify>, work: impl std::future::Future<Output = T>) -> T {
    let r = work.await;
    wake.notify_one();
    r
}

impl Daemon {
    // ------------------------------------------------------------------ 状态机

    /// 每 10 秒一次。有请求在用设备就跳过这一轮（巡检不排队，下一轮再来）
    pub async fn tick(self: Arc<Self>) {
        let Ok(_lease) = crate::maintenance::shared() else {
            self.heartbeat.beat();
            return;
        };
        let Ok(mut g) = self.inner.try_lock() else { return };
        let g = &mut *g;

        if g.tunnel.as_ref().is_some_and(Tunnel::hid_open)
            && g.screen_viewers == 0
            && g.hid_used.is_none_or(|t| t.elapsed() >= HID_IDLE)
        {
            close_hid(g).await;
            tracing::info!(
                "{}",
                ictl_l10n::log_tr!(
                    "HID 会话空闲，已关闭（停止屏幕媒体流）",
                    "HID session idle, closed it (screen media stream stopped)"
                )
            );
        }
        self.poll_start(g).await;

        let devices = match ictl_device::list_devices().await {
            Ok(d) => {
                g.list_errors = 0;
                d
            }
            Err(e) => {
                // usbmuxd 抖一下就当离线会拆掉整个会话：连续几轮都失败才算
                g.list_errors += 1;
                tracing::debug!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "列设备失败（第 {} 次）：{e}",
                        "Listing devices failed (attempt {}): {e}",
                        g.list_errors
                    )
                );
                if g.list_errors < LIST_ERRORS_OFFLINE {
                    self.publish(g);
                    return;
                }
                Vec::new()
            }
        };
        self.note_connected(g, devices).await;
        let pinned = self.pinned(g);
        let udids: Vec<&str> = g.connected.iter().map(|d| d.udid.as_str()).collect();
        // just started, nothing controlled yet: in automatic mode go on with the last device that was ready (kept in
        // state.json), not whichever usbmuxd happens to list first
        let current = g.device.as_ref().map(|d| d.udid.as_str()).or(g.persisted.screen_udid.as_deref());
        let target = crate::machine::pick_device(&udids, pinned.as_deref(), current).map(|i| g.connected[i].clone());

        match target {
            None => self.offline(g).await,
            Some(d) => {
                self.online(g, &d).await;
                self.adopt_legacy_passcode(g);
            }
        }

        let now = Instant::now();
        if now >= g.next_resign {
            g.next_resign = now + self.cfg.resign_check;
            if g.device.is_some() && g.persisted.wda_bundle.is_some() {
                let d = self.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = d.resign_now(false).await {
                        let mut g = d.inner.lock().await;
                        d.notify(&mut g, Event::ResignFailed(e));
                    }
                });
            }
        }
        if now >= g.next_session_check {
            g.next_session_check = now + self.cfg.session_check;
            tokio::task::spawn_local(self.clone().check_session());
        }
        self.web_housekeeping(g.device.as_ref().map(|d| d.udid.as_str()));
        Self::log_housekeeping(g);
        self.publish(g);
    }

    /// Returns the rotation actually observed in this sample and whether the published view changed.
    pub(crate) async fn sample_display(&self, g: &mut Inner, gesture_start: bool) -> (Option<Rotation>, bool) {
        let previous = g.dev.display.as_ref().map(|v| v.rotation);
        let device = g.device.clone();
        let Some(wda) = self.wda(g) else { return (None, self.update_display(g, None).await) };
        let (size, rotation) = wda
            .display_sample(previous, gesture_start, async { device?.interface_rotation().await.ok().flatten() })
            .await;
        let changed = self.record_display_sample(g, size, rotation, gesture_start).await;
        (rotation, changed)
    }

    async fn record_display_sample(
        &self,
        g: &mut Inner,
        size: Option<ScreenSize>,
        rotation: Option<Rotation>,
        gesture_start: bool,
    ) -> bool {
        if let Some(r) = rotation {
            g.dev.last_rotation = Some((Instant::now(), r));
        }
        // Other clients use the last known size for HID even on WDA versions without /rotation.
        if let (Some(size), Some(device)) = (size, &g.device) {
            let udid = device.udid.clone();
            if g.persisted.record_screen(&udid, size) {
                g.persisted.save(&self.cfg.state_file());
            }
        }
        let sample = size.zip(rotation);
        if sample.is_none()
            && !gesture_start
            && g.dev.display_verified_at.is_some_and(|at| at.elapsed() < DISPLAY_SAMPLE_GRACE)
            && let Some(view) = &g.dev.display
            && size.is_none_or(|s| (s.width, s.height) == (view.width, view.height))
            && rotation.is_none_or(|r| r.degrees() == view.rotation)
        {
            return false;
        }
        self.update_display(g, sample).await
    }

    async fn update_display(&self, g: &mut Inner, sample: Option<(ScreenSize, Rotation)>) -> bool {
        g.dev.display_verified_at = sample.map(|_| Instant::now());
        if !crate::core::DisplayGeometry::update(&mut g.dev.display, sample) {
            return false;
        }
        if g.dev.touch_rotation.is_some() {
            close_hid(g).await;
        } else if let Some((size, _)) = sample
            && let Some(hid) = g.tunnel.as_mut().and_then(Tunnel::open_hid)
        {
            hid.screen = size;
        }
        true
    }

    async fn offline(&self, g: &mut Inner) {
        let now = Instant::now();
        if g.device.take().is_some() {
            self.log.log(ictl_l10n::log_tr!("手机离线", "Phone offline"));
            g.offline_target = Some(ictl_l10n::in_own_lang(|| crate::notify::Target::of(g.info.as_ref())));
            // 名字、型号、iOS 版本是这台的：留着的话状态、提示文字会把它当成还在控制的设备
            g.info = None;
            self.forget_device(g).await;
            g.offline_since = Some(now);
        }
        self.set_state(g, DeviceState::Absent);
        let since = *g.offline_since.get_or_insert(now);
        let alert = self.cfg.offline_alert;
        if !alert.is_zero() && now.duration_since(since) >= alert {
            self.notify(g, Event::DeviceOffline { minutes: alert.as_secs() / 60 });
        }
    }

    pub(crate) async fn online(&self, g: &mut Inner, seen: &UsbDevice) {
        g.info = g.infos.get(&seen.udid).and_then(|entry| entry.value.clone());
        // 只比 UDID 不够：断开几秒又接上（短于离线判定、落在两轮巡检之间）时 UDID 不变，
        // usbmuxd 却换了编号，旧句柄的每个连接都报 bad device，永远恢复不了
        if !g.device.as_ref().is_some_and(|d| d.is_connection(seen)) {
            match g.device.take() {
                Some(old) if old.udid == seen.udid => self.log.log(&ictl_l10n::log_trf!(
                    "手机重新接入（usbmuxd 编号 {} → {}），重建连接",
                    "Phone reconnected (usbmuxd id {} → {}), reconnecting",
                    old.device_id(),
                    seen.device_id
                )),
                Some(old) => self.log.log(&ictl_l10n::log_trf!(
                    "改为控制 {}（原来是 {}）",
                    "Now controlling {} (was {})",
                    seen.udid,
                    old.udid
                )),
                None => {}
            }
            // 同一台重新接入也清（App 列表重读很便宜，日志流经的旧连接已经断了）
            self.forget_device(g).await;
            let d = Device::from_usb(seen);
            let model =
                g.info.as_ref().map(|i| format!(" {} iOS {}", i.product_type, i.product_version)).unwrap_or_default();
            self.log.log(&ictl_l10n::log_trf!("手机在线{model}", "Phone online{model}"));
            g.device = Some(d);
            g.backoff.reset();
            g.next_resign = g.next_resign.min(Instant::now() + RESIGN_CHECK_AFTER_CONNECT);
            if let Some(since) = g.offline_since.take()
                && g.dedup.was_sent("offline")
            {
                g.dedup.clear("offline");
                self.notify(g, Event::DeviceOnline { minutes: since.elapsed().as_secs() / 60 });
            }
        }
        let device = g.device.clone().expect("刚设置过");

        match device.paired().await {
            Ok(true) => {
                if g.dev.device_profile == DeviceProfile::NotChecked {
                    self.refresh_device_profile(g).await;
                }
            }
            Ok(false) => {
                self.update_display(g, None).await;
                return self.set_state(g, DeviceState::NeedsTrust);
            }
            // usbmuxd 出错：这一轮不下结论
            Err(e) => {
                return tracing::debug!(
                    "{}",
                    ictl_l10n::log_trf!("查配对记录失败：{e}", "Checking the pairing record failed: {e}")
                );
            }
        }
        if g.state != DeviceState::Ready && !device.developer_mode_enabled().await.unwrap_or(true) {
            return self.set_state(g, DeviceState::DeveloperModeOff);
        }
        let Some(bundle) = g.persisted.wda_bundle.clone() else {
            return self.set_state(g, DeviceState::NeedsSigning);
        };

        // WDA 健康：本进程的 runner 还活着，且 /status ready
        let runner_alive = g.runner.as_ref().is_some_and(|r| r.alive());
        let ready = match (&g.wda, runner_alive) {
            (Some(w), true) => w.ready().await.unwrap_or(false),
            _ => false,
        };
        // /status 不经 XCTest：WDA 半死时它照样 ready。经 XCTest 的请求（工具调用的、这里探测的）接连没有应答就当它卡死
        if ready && self.xctest_stalled(g).await {
            return self.restart_stalled_wda(g).await;
        }
        if ready && g.wda.as_ref().is_some_and(|w| w.unanswered() > 0) {
            // 刚有请求没应答、还没到判定卡死：先不采样屏幕（同样经 XCTest，每次还要白等 10 秒）
            return;
        }
        if ready {
            g.dev.awaiting_developer_trust = false;
            g.wda_misses.reset();
            self.sample_display(g, false).await;
            if g.state != DeviceState::Ready {
                if g.state == DeviceState::NeedsAutomationPin || g.dedup.was_sent("pin") {
                    g.dedup.clear("pin");
                    self.notify(g, Event::WdaRecovered);
                }
                self.set_state(g, DeviceState::Ready);
                g.backoff.reset();
            }
            return;
        }
        if !self.confirm_wda_loss(g, runner_alive).await {
            return;
        }
        if g.runner.is_some() {
            self.log.log(ictl_l10n::log_tr!("WDA 不可用", "WDA unavailable"));
            // 隧道和 HID 也一起丢：WDA 掉线常常是底层连接断了（例如 usbmuxd 重启），
            // 留着旧隧道，下次查镜像就报 socket io failed 并白白退避 60 秒
            self.drop_session(g).await;
        }
        g.wda_misses.reset();
        if g.starting.is_some() || !g.backoff.ready(Instant::now()) {
            return;
        }
        self.spawn_start(g, device, bundle);
    }

    /// WDA 卡死了没有：经 XCTest 的请求连续 [`ictl_wda::client::STALL_LIMIT`] 次没有应答（[`WdaClient::stalled`]）。
    /// 还没到就再探测一次（[`WdaClient::xctest_responsive`]），算进同一个计数。WDA 回了错误、或者应答了，都算活着
    async fn xctest_stalled(&self, g: &mut Inner) -> bool {
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
        self.log.log(&ictl_l10n::log_trf!(
            "WDA 卡死：/status 正常，但经 XCTest 的请求连续 {} 次没有应答，重新启动 WDA",
            "WDA is stuck: /status answers, but {} requests in a row through XCTest went unanswered; restarting WDA",
            ictl_wda::client::STALL_LIMIT
        ));
        // 后台启动的那个不能再接上：忘掉它，重新后台启动时会先结束它
        if g.persisted.detached_wda.take().is_some() {
            g.persisted.save(&self.cfg.state_file());
        }
        self.update_display(g, None).await;
        self.drop_session(g).await;
        let (Some(device), Some(bundle)) = (g.device.clone(), g.persisted.wda_bundle.clone()) else { return };
        if g.starting.is_none() && g.backoff.ready(Instant::now()) {
            self.spawn_start(g, device, bundle);
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
    fn spawn_start(&self, g: &mut Inner, device: Device, bundle: String) {
        // 正在等密码、等解锁时保持这个状态，免得每次重试都在两个状态间来回切、刷日志
        let waiting_pin = g.state == DeviceState::NeedsAutomationPin;
        if !matches!(g.state, DeviceState::NeedsAutomationPin | DeviceState::NeedsUnlock) {
            self.set_state(g, g.dev.starting_wda());
        }
        let start_timeout = if waiting_pin { PIN_ATTEMPT } else { WDA_START_TIMEOUT };
        let detached = self.cfg.wda_detached;
        // while waiting for the passcode it is launched again every half minute: once in the event log is enough
        if !waiting_pin {
            self.log.log(if detached {
                ictl_l10n::log_tr!(
                    "启动 WDA（实验：后台启动，不持有 XCTest 会话）",
                    "Starting WDA (experimental: launched in the background, no XCTest session)"
                )
            } else {
                ictl_l10n::log_tr!("启动 WDA", "Starting WDA")
            });
        }
        let known = g.persisted.detached_wda.as_ref().filter(|d| d.udid == device.udid).map(|d| d.pid);
        let (cache, url, user_dir) = (self.cfg.ddi_cache(), self.cfg.ddi_url.clone(), self.cfg.ddi_dir.clone());
        let Ok(lease) = crate::maintenance::shared() else { return };
        let wake = self.wake.clone();
        g.starting = Some(tokio::task::spawn_local(then_wake(wake, async move {
            let _lease = lease;
            // 手机重启后开发者镜像可能掉了；已挂载时只是一次查询。错误带上「是不是挂载这一步」。
            // 镜像好了再在同一条隧道上处理后台 WDA（见 prepare_detached）
            // 挂上的镜像版本、后台 WDA 的处理结果、启动前打开的系统日志流
            type Ready = (Option<String>, Result<Option<Detached>, DeviceError>, Option<LogStream>);
            let ddi: Result<Ready, (DeviceError, bool)> = async {
                let mut t = device.open_tunnel().await.map_err(|e| (e, false))?;
                let r = async {
                    if t.ddi_mounted().await.map_err(|e| (e, false))?.is_some() {
                        return Ok(None);
                    }
                    let dir = match &user_dir {
                        Some(d) => ictl_device::ddi::require_complete(d),
                        None => ictl_device::ddi::ensure_cache(&cache, &url).await,
                    }
                    .map_err(|e| (e, false))?;
                    Ok(Some(t.mount_ddi(&dir).await.map_err(|e| (e, true))?))
                }
                .await;
                let r = match r {
                    Ok(m) => {
                        // 后台启动前先开系统日志流：认「Enable UI Automation」密码框靠它（见 WdaRunner::attach）
                        let log = if detached { open_log(&device).await } else { None };
                        Ok((m, prepare_detached(&mut t, detached, known, &bundle).await, log))
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
                    return StartOutcome::Refused(e.to_string());
                }
                Err(e) => {
                    return StartOutcome::Wda(WdaError::StartFailed(ictl_l10n::trf!(
                        "后台启动 WDA 失败：{e}",
                        "Launching WDA in the background failed: {e}"
                    )));
                }
            };
            let started = match prepared {
                Some(Detached::Adopted(_)) => WdaRunner::attach(device.provider(), ADOPT_TIMEOUT, None).await,
                Some(Detached::Launched(_)) => WdaRunner::attach(device.provider(), start_timeout, log).await,
                None => WdaRunner::start(device.provider(), &bundle, start_timeout).await,
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
    async fn poll_start(self: &Arc<Self>, g: &mut Inner) {
        if !g.starting.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        let h = g.starting.take().expect("刚检查过");
        let outcome = h.await.unwrap_or_else(|e| {
            StartOutcome::Wda(WdaError::StartFailed(ictl_l10n::trf!("启动任务异常：{e}", "Start task failed: {e}")))
        });
        // 后台启动被拒绝和 XCTest 会话启动被拒绝（unexpected response）走同一条路：记失败，查证书
        match outcome {
            StartOutcome::Ready { runner, mut client, mounted, detached } => {
                if let Some(v) = mounted {
                    self.log.log(&ictl_l10n::log_trf!("开发者镜像已挂载 {v}", "Developer disk image mounted {v}"));
                }
                match detached {
                    Some(Detached::Adopted(pid)) => self.log.log(&ictl_l10n::log_trf!(
                        "接上手机上已在运行的后台 WDA（进程 {pid}），没有重新启动",
                        "Attached to the background WDA already running on the phone (pid {pid}) without restarting it"
                    )),
                    Some(Detached::Launched(pid)) => self.log.log(&ictl_l10n::log_trf!(
                        "已在后台启动 WDA（进程 {pid}，不持有 XCTest 会话）",
                        "Launched WDA in the background (pid {pid}, no XCTest session)"
                    )),
                    None => {}
                }
                let record = detached
                    .zip(g.device.as_ref())
                    .map(|(d, dev)| DetachedWda { udid: dev.udid.clone(), pid: d.pid() });
                if g.persisted.detached_wda != record {
                    g.persisted.detached_wda = record;
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
                if mount_failed && self.cfg.ddi_dir.is_none() {
                    g.dev.ddi_failures += 1;
                    if g.dev.ddi_failures >= DDI_FAILURES_INVALIDATE {
                        self.log.log(&ictl_l10n::log_trf!("开发者镜像连续 {} 次挂载失败，清掉缓存，下次重新下载", "Mounting the developer disk image failed {} times in a row, clearing the cache to download it again", g.dev.ddi_failures));
                        ictl_device::ddi::invalidate(&self.cfg.ddi_cache());
                        g.dev.ddi_failures = 0;
                    }
                }
                g.backoff.failed(Instant::now());
                self.set_state(
                    g,
                    DeviceState::WdaStarting {
                        last_error: Some(ictl_l10n::trf!("开发者镜像：{e}", "Developer disk image: {e}")),
                        hint: None,
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
                    StartOutcome::Refused(m) => (WdaError::StartFailed(m), true),
                    StartOutcome::Wda(e) => (e, false),
                    _ => unreachable!("上面只匹配了这两种"),
                };
                g.backoff.failed(Instant::now());
                // 接上的后台 WDA 没应答：忘掉它，下一次重新后台启动（会先结束它）
                if self.cfg.wda_detached && g.persisted.detached_wda.take().is_some() {
                    g.persisted.save(&self.cfg.state_file());
                }
                // StartFailed 自己就以「WDA 启动失败：」开头，日志和状态里再加一遍就重复了
                let missing = matches!(e, WdaError::NotInstalled(_));
                let e = match e {
                    WdaError::StartFailed(m) | WdaError::NotReadyInTime(m) => m,
                    e => e.to_string(),
                };
                self.log.log(&ictl_l10n::log_trf!("WDA 启动失败：{e}", "WDA failed to start: {e}"));
                let refused = refused_in_background || ictl_wda::runner::launch_refused(&e);
                let state = g.dev.wda_failed(e, refused, missing);
                self.set_state(g, state);
                if refused {
                    self.maybe_heal_certificate(g);
                }
            }
        }
    }

    /// 设备拒绝启动 WDA（只报 unexpected response）时，看是不是证书的问题，是就自动重签这台（见 `ictl_sign` 的模块说明）：
    /// 设备上的 WDA 不是本机的证书签的（别的机器、旧的数据目录签的，签它的证书多半被吊销了），或本机的证书已不在团队里
    /// （别处新建证书时把它吊销了）。没有证书问题的证据时保留启动诊断。
    /// 同一台 [`CERT_HEAL_EVERY`] 内只自动重签一次
    fn maybe_heal_certificate(self: &Arc<Self>, g: &mut Inner) {
        let Some(udid) = g.device.as_ref().map(|d| d.udid.clone()) else { return };
        if g.resigning || g.persisted.wda_bundle.is_none() {
            return;
        }
        if g.cert_heals.get(&udid).is_some_and(|t| t.elapsed() < CERT_HEAL_EVERY) {
            return;
        }
        g.cert_heals.insert(udid.clone(), Instant::now());
        tokio::task::spawn_local(self.clone().heal_certificate(udid, g.dev.foreign_wda));
    }

    async fn heal_certificate(self: Arc<Self>, udid: String, foreign: Option<bool>) {
        let Ok(_lease) = crate::maintenance::shared() else { return };
        if self.account().selected_email().await.is_err() {
            return;
        }
        let why = if foreign == Some(true) {
            ictl_l10n::tr!(
                "这台设备上的 WDA 不是本机的签名证书签的（别的机器或旧的数据目录签的，签它的证书多半已被吊销）",
                "the WDA on this device wasn't signed with this machine's certificate (another machine or an old data directory signed it, and that certificate was most likely revoked)"
            )
        } else {
            match self.check_own_certificate().await {
                Some(false) => ictl_l10n::tr!(
                    "本机的签名证书已不在 Apple ID 的团队里（别处新建证书时把它吊销了）",
                    "this machine's signing certificate is no longer in the Apple ID's team (it was revoked when a certificate was created elsewhere)"
                ),
                // 没查出证书问题，保留设备的启动错误供诊断
                _ => return,
            }
        };
        {
            let mut g = self.inner.lock().await;
            // 查证书期间切到了别的设备，或用户已经在重签了
            if g.device.as_ref().map(|d| d.udid.as_str()) != Some(udid.as_str()) || g.resigning {
                return;
            }
            self.log.log(&ictl_l10n::log_trf!(
                "WDA 起不来：{why}，自动重签这台设备",
                "WDA won't start: {why}; re-signing this device automatically"
            ));
            self.set_state(
                &mut g,
                DeviceState::WdaStarting {
                    last_error: Some(ictl_l10n::trf!(
                        "{why}。正在自动重签这台设备，约 1–2 分钟，保持设备解锁",
                        "{why}. Re-signing this device automatically, about 1–2 minutes; keep it unlocked"
                    )),
                    hint: None,
                },
            );
            self.publish(&g);
        }
        if let Err(e) = self.resign_now(true).await {
            let mut g = self.inner.lock().await;
            self.notify(&mut g, Event::ResignFailed(e));
        }
    }

    /// 联网查本机的签名证书还在不在团队里，记进 `own_cert`（doctor 显示）。登录不了、查不了返回 None
    async fn check_own_certificate(&self) -> Option<bool> {
        let account = self.account();
        // plume_core 的 future 不是 Send，放到阻塞线程里 block_on（同重签）；也不持锁
        let r = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                tokio::time::timeout(SESSION_CHECK_TIMEOUT, async {
                    account.session().await?.own_certificate_listed().await
                })
                .await
            })
        })
        .await;
        match r {
            Ok(Ok(Ok(Some(listed)))) => {
                self.inner.lock().await.own_cert = Some((Instant::now(), listed));
                Some(listed)
            }
            Ok(Ok(Err(e))) => {
                tracing::warn!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "查本机的签名证书失败：{e}",
                        "Checking this machine's signing certificate failed: {e}"
                    )
                );
                None
            }
            _ => None,
        }
    }

    // ------------------------------------------------------------------ 重签与会话

    async fn check_session(self: Arc<Self>) {
        let Ok(_lease) = crate::maintenance::shared() else { return };
        let account = self.account();
        // plume_core 的 future 不是 Send，放到阻塞线程里 block_on（同重签）；也不持锁
        // 会话好的话顺便查本机的签名证书还在不在团队里（同一次登录）
        let r: Result<Option<bool>, Option<String>> = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                let check = async {
                    let s = account.session().await?;
                    Ok::<_, ictl_sign::SignError>(match s.own_certificate_listed().await {
                        Ok(listed) => listed,
                        // 会话是好的，只是这一步没查成：不算会话失效，记一笔
                        Err(e) => {
                            tracing::warn!(
                                "{}",
                                ictl_l10n::log_trf!(
                                    "查本机的签名证书失败：{e}",
                                    "Checking this machine's signing certificate failed: {e}"
                                )
                            );
                            None
                        }
                    })
                };
                match tokio::time::timeout(SESSION_CHECK_TIMEOUT, check).await {
                    Ok(Ok(listed)) => Ok(listed),
                    Ok(Err(ictl_sign::SignError::NotLoggedIn)) => Err(None),
                    Ok(Err(e)) => Err(Some(e.to_string())),
                    Err(_) => Err(Some(ictl_l10n::trf!(
                        "{} 分钟内没有完成",
                        "not finished within {} minutes",
                        SESSION_CHECK_TIMEOUT.as_secs() / 60
                    ))),
                }
            })
        })
        .await
        .unwrap_or_else(|e| Err(Some(ictl_l10n::trf!("会话检查任务异常：{e}", "Session check task failed: {e}"))));
        let mut g = self.inner.lock().await;
        match r {
            Ok(listed) => {
                g.dedup.clear("session");
                if let Some(l) = listed {
                    g.own_cert = Some((Instant::now(), l));
                    if !l {
                        self.log.log(ictl_l10n::log_tr!(
                            "本机的签名证书已不在 Apple ID 的团队里（别处新建证书时把它吊销了）：用它签的 WDA 起不来，会自动重签",
                            "This machine's signing certificate is no longer in the Apple ID's team (revoked when a certificate was created elsewhere): WDA signed with it won't start and gets re-signed automatically"
                        ));
                    }
                }
            }
            Err(None) => {}
            Err(Some(e)) => {
                self.log
                    .log(&ictl_l10n::trf!("Apple ID 会话检查失败：{e}", "Checking the Apple ID session failed: {e}"));
                self.notify(&mut g, Event::SessionExpired(e));
            }
        }
    }

    /// 检查描述文件并按需重签（`force` 立即重签）。分三段：持锁取参数 → 不持锁签名、安装（几十秒）→ 持锁写结果
    pub(crate) async fn resign_now(self: &Arc<Self>, force: bool) -> Result<String, String> {
        let _lease = crate::maintenance::shared().map_err(|e| e.to_string())?;
        let (device, name) = {
            let mut g = self.inner.lock().await;
            let g = &mut *g;
            // 看这台手机上装的那份，不看最近一次签名：给别的手机签过之后，这台装的还是更早签的、更早到期
            if !force && g.persisted.wda_bundle.is_some() {
                self.refresh_device_profile(g).await;
                if g.dev.device_profile == DeviceProfile::Missing {
                    // 自动重签不替没装过的手机注册设备（占 Apple ID 的设备名额），交给用户决定
                    return Ok(
                        ictl_l10n::tr!("这台手机上没有装当前 Apple ID 签的 WDA,不自动重签;要装就运行 iphone-control setup", "No WDA signed by the current Apple ID on this phone, not re-signing automatically; run iphone-control setup to install it").into()
                    );
                }
                if let Some(left) = g.dev.device_profile.left(&g.persisted)
                    && left > self.cfg.resign_min
                {
                    return Ok(ictl_l10n::trf!(
                        "描述文件还剩 {} 小时,无需重签",
                        "{} hours left on the provisioning profile, no need to re-sign",
                        left.as_secs() / 3600
                    ));
                }
            }
            if g.resigning {
                return Ok(ictl_l10n::tr!(
                    "正在重签,稍后用 iphone-control status 查看结果",
                    "Re-signing, check the result later with iphone-control status"
                )
                .into());
            }
            let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
            let name = g
                .info
                .as_ref()
                .map(|i| i.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "iphone-control".into());
            g.resigning = true;
            self.log.log(ictl_l10n::log_tr!("开始重签", "Re-signing started"));
            (device, name)
        };
        let signed_udid = device.udid.clone();
        let account = self.account();
        let (wda_dir, unsigned, signed_path) =
            (self.cfg.wda_dir(), self.cfg.wda_unsigned.clone(), self.cfg.wda_signed());
        let fetch = self.cfg.wda_unsigned_is_cache() && !unsigned.is_file();
        if fetch {
            self.log.log(&ictl_l10n::log_trf!(
                "还没有未签名的 WDA 包，先下载 {}",
                "No unsigned WDA package yet, downloading {} first",
                crate::wda_pkg::WDA_VERSION
            ));
        }
        // plume_core 的签名 future 不是 Send（持有 &mut dyn FnMut 和非 Send 的私钥），放到阻塞线程里 block_on
        let r = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current()
                .block_on(tokio::time::timeout(RESIGN_TIMEOUT, async move {
                    if fetch {
                        crate::wda_pkg::ensure(&unsigned).await?;
                    }
                    let session = account.session().await.map_err(|e| e.to_string())?;
                    session.ensure_device(&device.udid, &name).await.map_err(|e| e.to_string())?;
                    std::fs::create_dir_all(&wda_dir).map_err(|e| e.to_string())?;
                    let tmp = wda_dir.join("WDA-signing.ipa");
                    let signed = session.sign_ipa(&unsigned, &tmp, &device.udid).await.map_err(|e| e.to_string())?;
                    // Read the target device before installing the new profile. An unavailable profile list
                    // leaves the first-install hint unknown and must not block a working installation.
                    let first_for_team =
                        match tokio::time::timeout(Duration::from_secs(2), device.installed_profiles()).await {
                            Ok(Ok(profiles)) => first_install_for_team(&profiles, &signed.team_id),
                            _ => false,
                        };
                    device.install(&tmp).await.map_err(|e| {
                        ictl_l10n::trf!(
                            "安装到手机失败(手机锁屏?)：{e}",
                            "Installing on the phone failed (phone locked?): {e}"
                        )
                    })?;
                    std::fs::rename(&tmp, &signed_path).map_err(|e| e.to_string())?;
                    Ok((signed, first_for_team))
                }))
                .unwrap_or_else(|_| {
                    Err(ictl_l10n::trf!(
                        "{} 分钟内没有完成(苹果服务器或安装卡住)",
                        "not finished within {} minutes (Apple's servers or the install got stuck)",
                        RESIGN_TIMEOUT.as_secs() / 60
                    ))
                })
        })
        .await
        .unwrap_or_else(|e| Err(ictl_l10n::trf!("签名任务异常退出：{e}", "The signing task exited abnormally: {e}")));

        let mut g = self.inner.lock().await;
        let g = &mut *g;
        g.resigning = false;
        let r = match r {
            Ok((s, first_for_team)) => {
                let same_device = g.device.as_ref().is_some_and(|d| d.udid == signed_udid);
                let trust_pending = first_for_team
                    || (same_device
                        && g.dev.awaiting_developer_trust
                        && g.persisted.team_id.as_deref() == Some(s.team_id.as_str()));
                if s.new_certificate {
                    self.log.log(if s.revoked_certificate {
                        ictl_l10n::log_tr!(
                            "签名新建了证书，团队里的证书满了，吊销了一张旧的：用它签的 WDA 在所有设备上都起不来了",
                            "Signing created a new certificate and revoked an old one (the team was full): WDA signed with it won't start on any device"
                        )
                    } else {
                        ictl_l10n::log_tr!(
                            "签名新建了证书（本机没有私钥，或原来的证书已不在团队里）",
                            "Signing created a new certificate (no local key, or the previous one is no longer in the team)"
                        )
                    });
                    self.notify(g, Event::CertificateChanged { revoked: s.revoked_certificate });
                }
                // 刚用它签成功，本机的证书肯定在团队里
                g.own_cert = Some((Instant::now(), true));
                let exp = s.profile.expires.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                g.persisted.team_id = Some(s.team_id);
                g.persisted.wda_bundle = Some(s.bundle_id);
                g.persisted.profile_expires = Some(exp);
                g.persisted.record_resign(now_secs(), None);
                g.persisted.save(&self.cfg.state_file());
                // 新包装上后旧的 runner 会失效，下一轮巡检重新拉起。签名期间切到了别的设备就不动它的会话
                if same_device {
                    self.drop_session(g).await;
                    self.refresh_device_profile(g).await;
                    g.backoff.reset();
                    g.dev.awaiting_developer_trust = trust_pending;
                    self.set_state(g, g.dev.starting_wda());
                }
                g.dedup.clear("resign");
                let expires = crate::eventlog::format_utc(exp);
                // 事件日志按守护进程的语言，返回给调用方的按请求的语言
                self.log.log(&ictl_l10n::log_trf!(
                    "重签完成,新描述文件到期 {expires} UTC",
                    "Re-signing done, the new provisioning profile expires {expires} UTC"
                ));
                Ok(installed_message(&expires, trust_pending))
            }
            Err(e) => {
                g.persisted.record_resign(now_secs(), Some(e.clone()));
                g.persisted.save(&self.cfg.state_file());
                self.log.log(&ictl_l10n::log_trf!("重签失败：{e}", "Re-signing failed: {e}"));
                Err(e)
            }
        };
        self.publish(g);
        r
    }
}

fn first_install_for_team(profiles: &[Vec<u8>], team: &str) -> bool {
    profiles.iter().all(|raw| {
        let Some(profile) = ictl_sign::Profile::parse(raw) else { return false };
        !profile.get_task_allow || (!profile.team_ids.is_empty() && profile.team_ids.iter().all(|id| id != team))
    })
}

fn installed_message(expires: &str, trust_pending: bool) -> String {
    let mut text = ictl_l10n::trf!(
        "重签完成,新描述文件到期 {expires} UTC",
        "Re-signing done, the new provisioning profile expires {expires} UTC"
    );
    if trust_pending {
        text.push('\n');
        text.push_str(WdaStartHint::TrustDeveloper.message());
    }
    text
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// 实验开关（`ICTL_WDA_DETACHED`）的那一步，在挂镜像的隧道上做：
/// - 开着：`known` 那个进程还是 WDA 就直接接上；否则在后台启动一个（会先结束已在运行的 WDA，包括 XCTest 会话启动的）
/// - 关着：结束之前后台启动、还在运行的那个。它不随守护进程退出，留着就一直占着 8100 端口
async fn prepare_detached(
    t: &mut Tunnel,
    detached: bool,
    known: Option<u64>,
    bundle: &str,
) -> Result<Option<Detached>, DeviceError> {
    fn is_wda(procs: &[(u64, String)], pid: u64) -> bool {
        procs.iter().any(|(p, path)| *p == pid && path.contains("WebDriverAgentRunner"))
    }
    match (detached, known) {
        (true, Some(pid)) if is_wda(&t.processes().await?, pid) => Ok(Some(Detached::Adopted(pid))),
        (true, _) => Ok(Some(Detached::Launched(t.launch_in_background(bundle).await?))),
        (false, Some(pid)) => {
            if is_wda(&t.processes().await.unwrap_or_default(), pid) {
                tracing::info!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "结束之前后台启动的 WDA（进程 {pid}）",
                        "Stopping the WDA launched in the background earlier (pid {pid})"
                    )
                );
                if let Err(e) = t.kill(pid).await {
                    tracing::warn!("kill {pid}: {e}");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Ok(None)
        }
        (false, None) => Ok(None),
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

#[cfg(test)]
mod install_tests {
    use super::*;

    fn profile(team: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0"?><plist version="1.0"><dict>
            <key>ExpirationDate</key><date>2030-01-01T00:00:00Z</date>
            <key>TeamIdentifier</key><array><string>{team}</string></array>
            <key>Entitlements</key><dict><key>application-identifier</key><string>LEGACY.other-app</string>
            <key>get-task-allow</key><true/></dict></dict></plist>"#
        )
        .into_bytes()
    }

    #[test]
    fn trust_guidance_requires_install_or_explicit_device_evidence() {
        use WdaStartHint::{LaunchRefused, TrustDeveloper};
        let mut device = PerDevice::default();
        let start = device.starting_wda();
        assert_eq!(start.wda_start_hint(), None);
        let refused = device.wda_failed("unexpected response".into(), true, false);
        assert_eq!(refused.wda_start_hint(), Some(LaunchRefused));
        assert_eq!(device.starting_wda(), start);
        let explicit = device.wda_failed("Needs Explicit User Trust".into(), false, false);
        assert_eq!(explicit.wda_start_hint(), Some(TrustDeveloper));
        let installed = DeviceState::WdaStarting { last_error: None, hint: Some(TrustDeveloper) };
        assert_eq!(device.starting_wda(), installed);
        assert_eq!(device.wda_failed("unexpected response".into(), true, false).wda_start_hint(), Some(TrustDeveloper));
        assert_eq!(device.wda_failed("image download failed".into(), false, false).wda_start_hint(), None);
        assert_eq!(device.starting_wda(), installed);
        assert_eq!(
            device.wda_failed("WDA missing".into(), false, true).wda_start_hint(),
            Some(WdaStartHint::InstallWda)
        );
        assert_eq!(device.starting_wda(), start);
        assert_eq!(DeviceState::NeedsTrust.wda_start_hint(), None); // USB pairing
        assert_eq!(DeviceState::Ready.wda_start_hint(), None);
    }

    #[test]
    fn install_guidance_uses_profiles_on_the_target_device() {
        assert!(first_install_for_team(&[], "TEAM"));
        assert!(first_install_for_team(&[profile("OTHER")], "TEAM"));
        assert!(!first_install_for_team(&[profile("TEAM")], "TEAM"));
        assert!(!first_install_for_team(&[b"unknown profile".to_vec()], "TEAM"));
        let hint = WdaStartHint::TrustDeveloper.message();
        assert!(installed_message("2030-01-01", true).ends_with(hint));
        assert!(!installed_message("2030-01-01", false).contains(hint));
    }

    #[tokio::test]
    async fn busy_sign_and_device_switch_do_not_issue_install_instructions() {
        let (daemon, dir) = crate::core::tests::test_daemon("wda-trust");
        {
            let mut g = daemon.inner.lock().await;
            g.resigning = true;
            g.dev.awaiting_developer_trust = true;
            g.state = DeviceState::WdaStarting { last_error: None, hint: Some(WdaStartHint::TrustDeveloper) };
            daemon.forget_device(&mut g).await;
            assert_eq!(g.state.wda_start_hint(), None);
            assert!(!g.dev.awaiting_developer_trust);
        }
        for force in [true, false] {
            let busy = daemon.resign_now(force).await.unwrap();
            assert!(!busy.contains(WdaStartHint::TrustDeveloper.message()));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use crate::core::tests::test_daemon;

    const SIZE: ScreenSize = ScreenSize { width: 393., height: 852. };

    #[tokio::test]
    async fn passive_display_sample_absence_only_holds_the_last_verified_view_temporarily() {
        let (d, dir) = test_daemon("display-grace");
        let mut g = d.inner.lock().await;
        d.record_display_sample(&mut g, Some(SIZE), Some(Rotation::Portrait), false).await;
        let id = g.dev.display.as_ref().unwrap().id.clone();
        g.dev.touch_rotation = Some(Rotation::Portrait);

        // A timeout/unknown sample during a drag should not tear down the video or finger.
        assert!(!d.record_display_sample(&mut g, Some(SIZE), None, false).await);
        assert_eq!(g.dev.display.as_ref().unwrap().id, id);
        assert_eq!(g.dev.touch_rotation, Some(Rotation::Portrait));
        assert!(!d.record_display_sample(&mut g, Some(SIZE), Some(Rotation::Portrait), false).await);

        // A fresh rotation alone does not extend the validity of an old complete sample.
        g.dev.display_verified_at = Some(Instant::now() - DISPLAY_SAMPLE_GRACE);
        assert!(d.record_display_sample(&mut g, None, Some(Rotation::Portrait), false).await);
        assert!(g.dev.display.is_none());
        assert!(g.dev.touch_rotation.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn changed_or_strict_display_sample_invalidates_immediately() {
        let (d, dir) = test_daemon("display-incomplete");
        let mut g = d.inner.lock().await;
        let resized = ScreenSize { width: 360., height: 780. };
        for (size, rotation, start) in
            [(None, Some(Rotation::TopToLeft), false), (Some(resized), None, false), (Some(SIZE), None, true)]
        {
            d.record_display_sample(&mut g, Some(SIZE), Some(Rotation::Portrait), false).await;
            g.dev.touch_rotation = Some(Rotation::Portrait);
            assert!(d.record_display_sample(&mut g, size, rotation, start).await);
            assert!(g.dev.display.is_none());
            assert!(g.dev.touch_rotation.is_none());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
