//! Native iOS 27+ device supervision: DDI preparation, accessibility handshake, and periodic liveness checks.

use std::time::{Duration, Instant};

use ictl_device::{Device, DeviceError};

use crate::core::{Daemon, Inner, drop_tunnel};
use crate::detached::{self, StoppedWda};
use crate::machine::DeviceState;

use super::{ensure_ddi, then_wake};

/// 原生控制就绪以后，这么久查一次开发者镜像还挂着、无障碍服务还在（[`Liveness`]）：巡检平时只看屏幕方向，
/// 镜像掉了（别的工具卸了它）或隧道坏了也一直报「就绪」，要等下一个操作失败才知道
const LIVENESS_EVERY: Duration = Duration::from_secs(5 * 60);
/// 只在这么久没人操作时查（查一次要锁着设备）
const LIVENESS_QUIET: Duration = Duration::from_secs(30);
/// 接连这么多次查不成（不是查到没挂，是查不出来）就当隧道坏了，重新准备
const LIVENESS_MISSES: u32 = 2;
/// 查一次最多等这么久
const LIVENESS_WAIT: Duration = Duration::from_secs(8);

/// What a check that native control still works found, and what to do ([`liveness`])
#[derive(Debug, PartialEq)]
pub(crate) enum Liveness {
    Alive,
    /// Couldn't tell: asked again at the next round
    Unsure,
    /// Prepared again from the start (the image mounted, the accessibility service connected)
    Prepare(String),
}

/// `found`: whether the image is mounted with the accessibility service listed, or the error of asking; `misses`:
/// checks in a row that couldn't tell, this one included when it couldn't
pub(crate) fn liveness(found: Result<bool, String>, misses: &mut u32) -> Liveness {
    match found {
        Ok(true) => {
            *misses = 0;
            Liveness::Alive
        }
        Ok(false) => {
            *misses = 0;
            Liveness::Prepare(
                ictl_l10n::tr!(
                    "开发者镜像或无障碍服务不在了，重新准备",
                    "The developer disk image or the accessibility service is gone, preparing again"
                )
                .into(),
            )
        }
        Err(e) => {
            *misses += 1;
            if *misses < LIVENESS_MISSES {
                return Liveness::Unsure;
            }
            *misses = 0;
            Liveness::Prepare(ictl_l10n::trf!(
                "接连几次查不到开发者镜像（{e}），重新准备",
                "Couldn't check the developer disk image several times in a row ({e}), preparing again"
            ))
        }
    }
}

impl Daemon {
    /// [`Self::online`] for a phone driven natively: the developer disk image mounted in the background (as before
    /// WDA starts), then the accessibility service's handshake and a first read of the screen
    pub(super) async fn native_online(&self, g: &mut Inner, device: Device) {
        if g.preparing.as_ref().is_some_and(|h| h.is_finished()) {
            let h = g.preparing.take().expect("刚检查过");
            let r = h.await.unwrap_or_else(|e| {
                Err((DeviceError::Idevice(ictl_l10n::trf!("准备任务异常：{e}", "Preparing task failed: {e}")), false))
            });
            match r {
                Ok((mounted, stopped)) => {
                    g.dev.ddi_failures = 0;
                    if let Some((udid, s)) = stopped {
                        self.detached_stopped(g, &udid, s);
                    }
                    if let Some(v) = mounted {
                        self.log.log(&ictl_l10n::log_trf!("开发者镜像已挂载 {v}", "Developer disk image mounted {v}"));
                        // a tunnel opened before the image was mounted doesn't list the accessibility service
                        drop_tunnel(g).await;
                        self.restart_ax_after_mount(g, &device).await;
                    }
                    match Box::pin(self.native_check(g, &device)).await {
                        Ok(()) => {
                            self.log.log(ictl_l10n::log_tr!(
                                "原生控制就绪（无障碍服务、HID，不用 WDA）",
                                "Native control ready (accessibility service and HID, no WDA)"
                            ));
                            g.dev.alive_checked = Some(Instant::now());
                            g.dev.alive_misses = 0;
                            g.backoff.reset();
                            self.set_state(g, DeviceState::Ready);
                            self.sample_display(g, false).await;
                        }
                        Err(e) => {
                            self.log.log(&ictl_l10n::log_trf!("准备手机失败：{e}", "Preparing the phone failed: {e}"));
                            g.backoff.failed(Instant::now());
                            self.set_state(g, DeviceState::Preparing { last_error: Some(e) });
                        }
                    }
                }
                Err((DeviceError::PasswordProtected, _)) => {
                    g.backoff.retry_after(Instant::now(), crate::machine::UNLOCK_RETRY);
                    self.set_state(g, DeviceState::NeedsUnlock);
                }
                Err((e, mount_failed)) => {
                    self.ddi_failed(g, mount_failed);
                    g.backoff.failed(Instant::now());
                    let e = ictl_l10n::trf!("开发者镜像：{e}", "Developer disk image: {e}");
                    self.log.log(&ictl_l10n::log_trf!("准备手机失败：{e}", "Preparing the phone failed: {e}"));
                    self.set_state(g, DeviceState::Preparing { last_error: Some(e) });
                }
            }
            return;
        }
        if g.state == DeviceState::Ready {
            self.sample_display(g, false).await;
            let due = g.dev.alive_checked.is_none_or(|t| t.elapsed() >= LIVENESS_EVERY);
            // ready still: nothing more this round. Gone: prepared again right away, below
            if !due || self.acting_now(LIVENESS_QUIET) || !self.native_gone(g, &device).await {
                return;
            }
        }
        if g.preparing.is_some() || !g.backoff.ready(Instant::now()) {
            return;
        }
        // retrying: the last error stays up until this round's outcome
        if !matches!(g.state, DeviceState::Preparing { .. }) {
            self.set_state(g, DeviceState::Preparing { last_error: None });
        }
        let legacy = ictl_device::ddi::legacy(g.info.as_ref().map_or(0, |i| i.major_version()));
        let (cache, url, user_dir) =
            (self.cfg.ddi_cache_for(legacy), self.cfg.ddi_url.clone(), self.cfg.ddi_dir.clone());
        // a WDA launched in the background before (WDA control earlier): not wanted now, ended once the image is up
        // (AppService needs it)
        let known = match crate::adopt::read(&self.cfg.adopt_dir(), &device.udid) {
            Ok(Some(r)) => Some((r.pid, r.executable)),
            _ => g.persisted.detached_for(&device.udid).map(|pid| (pid, None)),
        };
        let Ok(lease) = crate::maintenance::shared() else { return };
        let wake = self.wake.clone();
        g.preparing = Some(tokio::task::spawn_local(then_wake(wake, async move {
            let _lease = lease;
            let mut t = device.open_tunnel().await.map_err(|e| (e, false))?;
            let r = ensure_ddi(&device, &mut t, legacy, &cache, &url, user_dir.as_deref()).await;
            let stopped = match (&r, known) {
                (Ok(_), Some((pid, exe))) => {
                    Some((device.udid.clone(), detached::stop(&mut t, pid, exe.as_deref()).await))
                }
                _ => None,
            };
            t.close().await;
            r.map(|m| (m, stopped))
        })));
    }

    /// What became of the WDA launched in the background earlier on `udid`, a phone driven natively: forgotten once
    /// it is gone; one that may still run stays recorded (tried again the next time the phone is prepared)
    fn detached_stopped(&self, g: &mut Inner, udid: &str, s: StoppedWda) {
        let pid = match s {
            StoppedWda::Ended(pid) => {
                self.log.log(&ictl_l10n::log_trf!(
                    "已结束之前后台启动的 WDA（进程 {pid}）：原生控制不用它",
                    "Stopped the WDA launched in the background earlier (pid {pid}): native control doesn't use it"
                ));
                pid
            }
            StoppedWda::NotRunning(pid) => pid,
            StoppedWda::Failed(pid, e) => {
                self.log.log(&ictl_l10n::log_trf!(
                    "结束之前后台启动的 WDA（进程 {pid}）失败：{e}",
                    "Stopping the WDA launched in the background earlier (pid {pid}) failed: {e}"
                ));
                return;
            }
        };
        if let Ok(Some(r)) = crate::adopt::read(&self.cfg.adopt_dir(), udid)
            && r.pid == pid
            && let Err(e) = crate::adopt::remove(&self.cfg.adopt_dir(), udid)
        {
            tracing::warn!("removing the background WDA record of {udid} failed: {e}");
        }
        if g.persisted.forget_detached(udid, Some(pid)) {
            g.persisted.save(&self.cfg.state_file());
        }
    }

    /// Whether native control, ready until now, no longer works: the developer disk image went (unmounted by
    /// something else) or the tunnel can't be used. Then the phone is back to being prepared, and true
    async fn native_gone(&self, g: &mut Inner, device: &Device) -> bool {
        g.dev.alive_checked = Some(Instant::now());
        let legacy = ictl_device::ddi::legacy(g.info.as_ref().map_or(0, |i| i.major_version()));
        let found = tokio::time::timeout(LIVENESS_WAIT, async {
            let t = self.ensure_tunnel(g, device).await.map_err(|e| e.to_string())?;
            if !t.ax_supported() {
                return Ok(false);
            }
            let mounted =
                if legacy { device.legacy_ddi_mounted().await } else { t.ddi_mounted().await.map(|v| v.is_some()) };
            mounted.map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|_| Err(ictl_l10n::tr!("查询超时", "the check timed out").into()));
        tracing::debug!("native liveness: {found:?}");
        match liveness(found, &mut g.dev.alive_misses) {
            Liveness::Alive => false,
            Liveness::Unsure => {
                // asked again at the next round, not in five minutes
                g.dev.alive_checked = None;
                false
            }
            Liveness::Prepare(why) => {
                self.log.log(&ictl_l10n::log_trf!("原生控制：{why}", "Native control: {why}"));
                // the tunnel lists the services as they were when it was opened
                drop_tunnel(g).await;
                self.set_state(g, DeviceState::Preparing { last_error: Some(why) });
                true
            }
        }
    }

    /// The image was mounted again while the phone kept running (it had gone, [`Self::native_gone`]): axauditd, the
    /// process behind the accessibility service, may still be the one started from the image before, which then
    /// answers every read with «no frontmost app» (iPhone 15 Pro Max, iOS 27.2.1), and preparing would fail for good.
    /// Ended, the next connection starts it from the image now mounted; after a reboot none runs yet and nothing is
    /// ended
    async fn restart_ax_after_mount(&self, g: &mut Inner, device: &Device) {
        let ended = match self.ensure_tunnel(g, device).await {
            Ok(t) => t.restart_ax_daemon().await,
            Err(e) => Err(e),
        };
        match ended {
            Ok(0) => {}
            Ok(n) => self.log.log(&ictl_l10n::log_trf!(
                "开发者镜像重新挂上了，结束了之前的 axauditd（{n} 个进程），无障碍服务会重新启动",
                "The developer disk image was mounted again: ended the axauditd from before ({n} processes), the accessibility service starts again"
            )),
            Err(e) => tracing::info!("restarting axauditd after mounting the image: {e}"),
        }
    }

    /// The accessibility service answers and the screen can be read (which records its size)
    async fn native_check(&self, g: &mut Inner, device: &Device) -> Result<(), String> {
        for attempt in 0..2 {
            let t = self.ensure_tunnel(g, device).await.map_err(|e| e.to_string())?;
            if t.ax_supported() {
                break;
            }
            // the tunnel lists the services as they were when it was opened
            drop_tunnel(g).await;
            if attempt == 1 {
                return Err(ictl_l10n::tr!(
                    "手机没有提供无障碍服务（remoteAXService）：开发者镜像可能没挂上",
                    "The phone offers no accessibility service (remoteAXService): the developer disk image may not be mounted"
                )
                .into());
            }
        }
        match Box::pin(self.ax_read(g, crate::tools::AX_WAIT)).await {
            Ok(_) => Ok(()),
            // the screen can't be read at all while preparing: axauditd may be one left from an image mounted before
            // (it answers «no frontmost app» for good), also when the mount wasn't seen here (it finished after its
            // time limit, or something else mounted it). Ended once (not again within `AX_RESTART_EVERY`), read again
            Err(e) => {
                let why = e.to_string();
                if !Box::pin(self.restart_stuck_ax(g, device, &why)).await {
                    return Err(why);
                }
                Box::pin(self.ax_read(g, crate::tools::AX_WAIT)).await.map(|_| ()).map_err(|e| e.to_string())
            }
        }
    }
}
