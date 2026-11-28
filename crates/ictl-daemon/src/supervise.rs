//! 巡检和后台任务：每 10 秒推进状态机、后台启动 WDA、重签（含证书出问题时自动重签）、Apple ID 会话检查。

pub(crate) mod native;
pub(crate) mod resign;
mod session;
mod wda;

use std::sync::Arc;
use std::time::{Duration, Instant};

use ictl_device::{Device, DeviceError, Rotation, ScreenSize, Tunnel, UsbDevice};

use crate::claim::Round;
use crate::core::{Daemon, DeviceProfile, Inner, Route, close_hid};
use crate::machine::DeviceState;
use crate::notify::Event;

pub(crate) use wda::StartOutcome;

/// Three 10-second supervisor intervals. A missing background sample within this
/// window can leave a verified gesture running, but can never authorize a new one.
const DISPLAY_SAMPLE_GRACE: Duration = Duration::from_secs(30);
/// How long after an action an old screen size isn't measured again ([`Daemon::sample_display`]): someone tapping
/// or swiping may well go on, and the measurement would sit in WDA's queue before their next gesture
const ACTION_QUIET: Duration = Duration::from_secs(3);

/// HID 会话空闲这么久就关掉。它持有屏幕媒体流，开着时 iOS 当作屏幕被远程查看：不会自动锁屏，
/// 还会弹「viewed remotely」通知。重开约 0.5 秒
pub(crate) const HID_IDLE: Duration = Duration::from_secs(30);
/// 原生控制：无障碍会话空闲这么久就关掉。开着时手机为我们开着无障碍运行时，每次界面变化都推一棵树；重开约 1 秒
pub(crate) const AX_IDLE: Duration = Duration::from_secs(60);
/// 连续这么多轮列不出设备才当作离线
const LIST_ERRORS_OFFLINE: u32 = 3;
/// 开发者镜像连续挂载失败这么多次就清缓存重新下载
const DDI_FAILURES_INVALIDATE: u32 = 3;

/// 换了设备或重新接上后，这么久内就检查一次要不要重签（离线期间可能过期了，不能等满 RESIGN_CHECK_HOURS）
const RESIGN_CHECK_AFTER_CONNECT: Duration = Duration::from_secs(60);

/// `work`, then a supervision round right away ([`Daemon::wake`])
async fn then_wake<T>(wake: Arc<tokio::sync::Notify>, work: impl std::future::Future<Output = T>) -> T {
    let r = work.await;
    wake.notify_one();
    r
}

impl Daemon {
    // ------------------------------------------------------------------ 状态机

    /// 每 10 秒一次。有请求在用设备就跳过这一轮（巡检不排队，下一轮再来）。返回这一轮跑了没有（拿不到设备锁时
    /// 没跑：被唤醒的一轮就再来，见 `lib.rs` 的主循环）
    pub async fn tick(self: Arc<Self>) -> bool {
        let Ok(_lease) = crate::maintenance::shared() else {
            self.heartbeat.beat();
            return true;
        };
        let Ok(mut g) = self.inner.try_lock() else { return false };
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
        if g.ax_used.is_none_or(|t| t.elapsed() >= AX_IDLE)
            && let Some(t) = g.tunnel.as_mut().filter(|t| t.ax_open())
        {
            t.close_ax().await;
            tracing::info!("{}", ictl_l10n::log_tr!("无障碍会话空闲，已关闭", "Accessibility session idle, closed it"));
        }
        self.restore_pasteboard(g, true).await;
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
                    return true;
                }
                Vec::new()
            }
        };
        self.note_connected(g, devices).await;
        // a device is switched to by `select` alone ([`Daemon::switch_to`]): not while it is under way
        if self.gate.check_open().is_ok() {
            match self.round_target(g).await {
                Round::Offline => {
                    self.offline(g).await;
                    self.offline_lease(g);
                }
                Round::Held => {}
                Round::Device(d) => {
                    self.online(g, &d).await;
                    self.describe_lease(g);
                    self.adopt_legacy_passcode(g);
                    self.clear_left_viewed(g).await;
                }
            }
        }

        let now = Instant::now();
        let upkeep = wda_upkeep(g.device.is_some(), self.native(g), g.persisted.wda_bundle.is_some());
        // 不该做的这一轮不算数：换到用 WDA 的设备后下一轮就做，不用再等一个周期
        if now >= g.next_resign && upkeep.resign {
            g.next_resign = now + self.cfg.resign_check;
            let d = self.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = d.resign_now(false).await {
                    let mut g = d.inner.lock().await;
                    d.notify(&mut g, Event::ResignFailed(e.to_string()));
                }
            });
        }
        self.web_housekeeping(g.device.as_ref().map(|d| d.udid.as_str()));
        Self::log_housekeeping(g);
        self.publish(g);
        true
    }

    /// Returns the rotation actually observed in this sample and whether the published view changed.
    pub(crate) async fn sample_display(&self, g: &mut Inner, gesture_start: bool) -> (Option<Rotation>, bool) {
        let previous = g.dev.display.as_ref().map(|v| v.rotation);
        let device = g.device.clone();
        // an old size is measured again only while nobody acts: on an iPad mini 5 that takes WDA 0.95 s
        let refresh = !gesture_start && !self.acting_now(ACTION_QUIET);
        let wda = match self.route(g) {
            Route::Wda(w) => w,
            // 原生控制：方向问 SpringBoard（lockdown 服务），尺寸是读界面时记下的（竖放）
            Route::Native => {
                let rotation = self.native_rotation(g).await;
                let size = g.persisted.screen_for(device.as_ref().map(|d| d.udid.as_str()));
                let changed = self.record_display_sample(g, size, rotation, gesture_start).await;
                return (rotation, changed);
            }
            Route::Hid | Route::Unavailable => return (None, self.update_display(g, None).await),
        };
        let (size, rotation) = Box::pin(
            wda.display_sample(previous, refresh, async { device?.interface_rotation().await.ok().flatten() }),
        )
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
        // iOS 27 起默认原生控制：不签名、不启动 WDA（docs/dev/ios27-native.md）
        if self.native(g) {
            return Box::pin(self.native_online(g, device)).await;
        }
        let bundle = match g.persisted.wda_bundle.clone() {
            Some(b) => b,
            None if !g.dev.wda_takeover_tried => {
                g.dev.wda_takeover_tried = true;
                match self.take_over_wda(g, &device).await {
                    Some(b) => b,
                    None => return self.set_state(g, DeviceState::NeedsSigning),
                }
            }
            None => return self.set_state(g, DeviceState::NeedsSigning),
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

    /// A mount that failed may be a broken cache: after a few in a row it is downloaded again (`ICTL_DDI_DIR` is the
    /// user's own and never deleted)
    fn ddi_failed(&self, g: &mut Inner, mount_failed: bool) {
        if mount_failed && self.cfg.ddi_dir.is_none() {
            g.dev.ddi_failures += 1;
            if g.dev.ddi_failures >= DDI_FAILURES_INVALIDATE {
                self.log.log(&ictl_l10n::log_trf!("开发者镜像连续 {} 次挂载失败，清掉缓存，下次重新下载", "Mounting the developer disk image failed {} times in a row, clearing the cache to download it again", g.dev.ddi_failures));
                // the cache of the image this phone takes (the classic personalized one on iOS 17–18)
                let legacy = ictl_device::ddi::legacy(g.info.as_ref().map_or(0, |i| i.major_version()));
                ictl_device::ddi::invalidate(&self.cfg.ddi_cache_for(legacy), ictl_device::ddi::files_for(legacy));
                g.dev.ddi_failures = 0;
            }
        }
    }

    /// The phone in use was left "viewed remotely" by an earlier daemon that ended with a HID session open
    /// ([`crate::hidmark`]): once it is ready, a session opened and closed clears that. A session open now clears it
    /// when it closes; after [`crate::hidmark::TRIES`] failed tries it is left alone
    async fn clear_left_viewed(&self, g: &mut Inner) {
        let Some(udid) = g.device.as_ref().map(|d| d.udid.clone()) else { return };
        let Some(&tries) = g.hid_orphans.get(&udid) else { return };
        if g.state != DeviceState::Ready
            || !crate::core::hid_supported(g.info.as_ref())
            || g.tunnel.as_ref().is_some_and(Tunnel::hid_open)
        {
            return;
        }
        match Box::pin(self.hid(g)).await {
            Ok(_) => {
                // removes the marker and the entry (`close_hid`)
                close_hid(g).await;
                self.log.log(ictl_l10n::log_tr!(
                    "上次守护进程退出时没有停掉屏幕媒体流（卡死或被结束）：已清掉手机上残留的「正在被查看」",
                    "The last daemon ended without stopping the screen media stream (stuck or killed): cleared the phone's leftover \"viewed remotely\""
                ));
            }
            Err(e) => {
                tracing::warn!("clearing the leftover HID state of {udid} failed: {e}");
                if tries + 1 >= crate::hidmark::TRIES {
                    g.hid_orphans.remove(&udid);
                    crate::hidmark::clear(&self.cfg.hid_marker_dir(), &udid);
                } else {
                    g.hid_orphans.insert(udid, tries + 1);
                }
            }
        }
    }
}

/// 定时的 WDA 维护（自动重签、Apple ID 会话检查）这一轮该不该做
#[derive(Debug, PartialEq, Eq)]
struct Upkeep {
    resign: bool,
}

/// 只为用 WDA 的设备做：在用的设备走原生控制时都不做（iOS 27 原生不要 Apple ID，见 docs/dev/ios27-native.md）。
/// 没装过 WDA（`has_bundle` 为 false）也不做。会话检查在没连手机时照做，提早发现会话失效
fn wda_upkeep(device: bool, native: bool, has_bundle: bool) -> Upkeep {
    let wda = has_bundle && !(device && native);
    Upkeep { resign: wda && device }
}

/// 手机重启后开发者镜像可能掉了：没挂就挂上（返回挂上的版本），已挂载时只是一次查询。
/// 错误带上「是不是挂载这一步」（才可能是缓存坏了）。`legacy`: iOS 17–18 (see [`ictl_device::ddi::legacy`]), `cache` is that
/// image's cache; cryptexd isn't asked on that path
async fn ensure_ddi(
    device: &Device,
    t: &mut Tunnel,
    legacy: bool,
    cache: &std::path::Path,
    url: &str,
    user_dir: Option<&std::path::Path>,
) -> Result<Option<String>, (DeviceError, bool)> {
    if legacy {
        if device.legacy_ddi_mounted().await.map_err(|e| (e, false))? {
            return Ok(None);
        }
    } else if t.ddi_mounted().await.map_err(|e| (e, false))?.is_some() {
        return Ok(None);
    }
    let files = ictl_device::ddi::files_for(legacy);
    let dir = match user_dir {
        Some(d) => ictl_device::ddi::require_complete(d, files),
        None if legacy => ictl_device::ddi::ensure_cache(cache, &ictl_device::ddi::personalized_url(url), files).await,
        None => ictl_device::ddi::ensure_cache(cache, url, files).await,
    }
    .map_err(|e| (e, false))?;
    if legacy {
        device.mount_legacy_ddi(&dir).await.map_err(|e| (e, true))?;
        return Ok(Some("personalized".to_owned()));
    }
    Ok(Some(t.mount_ddi(&dir).await.map_err(|e| (e, true))?))
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

#[cfg(test)]
mod upkeep_tests {
    use super::native::{Liveness, liveness};
    use super::*;

    /// Native control found gone is prepared again: the image unmounted (or the service not listed) at once, a
    /// check that can't tell only after a second one in a row; one that works again clears the count
    #[test]
    fn native_control_found_gone_is_prepared_again() {
        let mut misses = 0;
        assert_eq!(liveness(Ok(true), &mut misses), Liveness::Alive);
        assert!(matches!(liveness(Ok(false), &mut misses), Liveness::Prepare(_)));
        assert_eq!(liveness(Err("timeout".into()), &mut misses), Liveness::Unsure);
        assert_eq!(liveness(Ok(true), &mut misses), Liveness::Alive);
        assert_eq!(misses, 0, "an answer clears the count");
        assert_eq!(liveness(Err("timeout".into()), &mut misses), Liveness::Unsure);
        assert!(matches!(liveness(Err("timeout".into()), &mut misses), Liveness::Prepare(w) if w.contains("timeout")));
        assert_eq!(misses, 0, "and starts again after preparing");
    }

    #[test]
    fn wda_upkeep_is_only_for_devices_that_use_wda() {
        // a WDA device with WDA installed: both
        assert_eq!(wda_upkeep(true, false, true), Upkeep { resign: true });
        // the device in use is driven natively: neither, even with a WDA bundle left from another device
        assert_eq!(wda_upkeep(true, true, true), Upkeep { resign: false });
        // no phone: nothing to re-sign for, the Apple ID session is still checked
        assert_eq!(wda_upkeep(false, false, true), Upkeep { resign: false });
        // never installed WDA (native only): no Apple ID upkeep at all
        assert_eq!(wda_upkeep(true, false, false), Upkeep { resign: false });
        assert_eq!(wda_upkeep(false, false, false), Upkeep { resign: false });
    }

    /// A `resign` failure inside `spawn_blocking` answers the caller in the request's language while recording the
    /// event log and `state.json` in the daemon's own language (never mixing or leaking the caller's language)
    #[tokio::test]
    async fn resign_error_follows_caller_language_without_polluting_daemon_log() {
        let dir = std::env::temp_dir().join(format!("ictl-resign-lang-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let mut cfg = crate::Config::from_env(&dir);
        cfg.wda_unsigned = dir.join("unsigned.ipa");
        std::fs::write(&cfg.wda_unsigned, b"stub").unwrap();
        let d = crate::Daemon::new(cfg);
        {
            let mut g = d.inner.lock().await;
            g.device = Some(Device::from_usb(&ictl_device::UsbDevice { udid: "TEST-UDID".into(), device_id: 1 }));
        }

        let own = ictl_l10n::own_lang();
        let other = if own == ictl_l10n::Lang::En { ictl_l10n::Lang::Zh } else { ictl_l10n::Lang::En };
        let err = ictl_l10n::scope(other, d.resign_now(true)).await.unwrap_err().to_string();

        let (want_other, want_own) = if own == ictl_l10n::Lang::En {
            ("还没有登录 Apple ID", "Not signed in to an Apple ID yet")
        } else {
            ("Not signed in to an Apple ID yet", "还没有登录 Apple ID")
        };
        assert!(err.contains(want_other), "caller got: {err}");
        assert!(!err.contains(want_own), "caller mixed languages: {err}");

        let log = std::fs::read_to_string(dir.join("logs/daemon.log")).unwrap();
        assert!(log.contains(want_own), "daemon log got: {log}");
        assert!(!log.contains(want_other), "daemon log polluted by caller language: {log}");

        let state = d.inner.lock().await.persisted.resign_result.clone().unwrap();
        let recorded = state.error.unwrap();
        assert!(recorded.contains(want_own), "state.json got: {recorded}");
        assert!(!recorded.contains(want_other), "state.json polluted by caller language: {recorded}");

        std::fs::remove_dir_all(dir).unwrap();
    }
}
