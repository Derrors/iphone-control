//! `select`: changing the device this instance controls (docs/dev/multi-instance-plan.md 4.1).
//!
//! With the selection latch held throughout (no task is opened meanwhile, no other selection):
//! 1. the new device's lease is taken; someone else holding it is an error naming them, and nothing changes;
//! 2. with the device lock, the gate closes ([`crate::admission`]): nothing new is let in;
//! 3. long work under way (re-signing, an install) isn't waited for: the switch goes back at once, saying which;
//! 4. without the device lock, the streams end, the background start of WDA or of native control is stopped and
//!    waited for, and the rest drains, within [`DRAIN`];
//! 5. what is still held then: the switch goes back (the new lease let go, the gate open again, the old device and
//!    selection kept);
//! 6. with the device lock again, the old device's session closes (the tunnel, the HID session: its leftover
//!    "viewed" goes with it);
//! 7. the registry is read again (the default instance may not take a device being given to a named one), and the
//!    selection is written, failing which the switch goes back; the selection generation moves on;
//! 8. the old lease goes, the new one is the instance's, the gate opens and the new device is brought up.

use std::time::{Duration, Instant};

use crate::core::{Daemon, Inner, device_label};
use crate::devices::resolve_device;
use crate::lease::{Claim, Lease};

/// How long a switch waits for the work let in on the old device to end
pub(crate) const DRAIN: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(15) };
/// How long a daemon that is stopping waits for it (within systemd's `TimeoutStopSec=30`)
pub(crate) const EXIT_DRAIN: Duration = Duration::from_secs(20);
/// How long it waits for a probe (another instance reading the device's details) to let go of the new one
const PROBE_WAIT: Duration = Duration::from_secs(2);

type Result<T> = crate::error::Result<T>;

/// The caller's going away ([`crate::core::CallCtx::gone`])
type Gone = Option<tokio::sync::oneshot::Receiver<()>>;

/// The caller went away (`select` aborted at Ctrl-C): nothing more is done, and nothing written
fn left(gone: &mut Gone) -> bool {
    gone.as_mut().is_some_and(|g| g.try_recv().is_ok())
}

fn cancelled() -> crate::error::Error {
    ictl_l10n::tr!("调用方已取消，没有切换", "The caller cancelled; not switched").into()
}

/// `lock`, unless the caller goes away first
async fn lock_or_gone<'a, T>(
    lock: &'a tokio::sync::Mutex<T>,
    gone: &mut Gone,
) -> Result<tokio::sync::MutexGuard<'a, T>> {
    match gone {
        Some(g) => tokio::select! {
            biased;
            l = lock.lock() => Ok(l),
            Ok(()) = &mut *g => Err(cancelled()),
        },
        None => Ok(lock.lock().await),
    }
}

impl Daemon {
    /// `select <device>` or `select auto`: what to tell the caller
    /// `gone`: the caller going away, checked while waiting and before anything is written
    pub(crate) async fn select(&self, query: &str, gone: &mut Gone) -> Result<String> {
        let _latch = lock_or_gone(&self.select_latch, gone).await?;
        // checked again under the latch: the task check before it was made without
        if let Some(why) = self.tasks.switch_refused() {
            return Err(why.into());
        }
        let mut g = lock_or_gone(&self.inner, gone).await?;
        // unit tests set the devices themselves: a host's usbmuxd (macOS has one) would list none
        #[cfg(not(test))]
        if let Ok(d) = ictl_device::list_devices().await {
            self.note_connected(&mut g, d).await;
        }
        let q = query.trim();
        if q.eq_ignore_ascii_case("auto") || q == ictl_l10n::tr!("自动", "automatic") {
            return self.select_auto(&mut g);
        }
        let infos = self.connected_infos(&g);
        let target = infos[resolve_device(&infos, q)?].clone();
        if let Some(u) = &self.cfg.udid
            && !u.eq_ignore_ascii_case(&target.udid)
        {
            return Err(ictl_l10n::trf!(
                "IPHONE_UDID 固定为 {u};要换设备,先改 ~/.config/iphone-control/env 里的 IPHONE_UDID,再重启守护进程",
                "IPHONE_UDID pins {u}; to switch devices, change IPHONE_UDID in ~/.config/iphone-control/env and restart the daemon"
            ).into());
        }
        self.may_take(&target.udid)?;
        let label = device_label(&target);
        let same = g.lease.as_ref().is_some_and(|l| l.is_for(&target.udid))
            || g.device.as_ref().is_some_and(|d| d.udid.eq_ignore_ascii_case(&target.udid));
        if same {
            if left(gone) {
                return Err(cancelled());
            }
            self.persist_selection(&mut g, &target.udid)?;
            return Ok(ictl_l10n::trf!(
                "已经在控制 {label},之后固定控制这台",
                "Already controlling {label}; it stays selected from now on"
            ));
        }
        if let Some(by) = self.controlled_elsewhere(&g, &target.udid) {
            return Err(held_by(&label, &by).into());
        }
        let usb = g
            .connected
            .iter()
            .find(|d| d.udid == target.udid)
            .cloned()
            .ok_or(ictl_l10n::tr!("设备刚刚断开了", "the device just disconnected"))?;
        // 1. the new lease, without the device lock (a probe may hold it for a moment)
        let holder = self.holder(&g).with_device(Some(&target));
        drop(g);
        let lease = self.claim_for_switch(&usb.udid, holder, &label).await?;
        let g = lock_or_gone(&self.inner, gone).await?;
        if !g.connected.iter().any(|d| d.udid == usb.udid) {
            return Err(ictl_l10n::tr!("设备刚刚断开了", "the device just disconnected").into());
        }
        let g = if g.device.is_some() { self.drain_old(g).await? } else { g };
        let mut g = g;
        // 7. the caller still there, the registry again, then the selection on disk: otherwise back to the old device
        //    (prepared again by the next round, its lease still held)
        let committed = if left(gone) {
            Err(cancelled())
        } else {
            self.may_take(&usb.udid).and_then(|()| self.persist_selection(&mut g, &usb.udid))
        };
        if let Err(e) = committed {
            self.publish(&g);
            self.gate.reopen();
            return Err(e);
        }
        // 8. the old lease goes with the new one taken; the snapshot says the new device before anything is let in
        //    again (a task opened, or a stream, right after this answer goes by it)
        self.took(&mut g, lease);
        self.online(&mut g, &usb).await;
        self.describe_lease(&mut g);
        self.publish(&g);
        self.gate.reopen();
        Ok(self.switched(&g, &target, &label))
    }

    /// The token a selection compare-and-set is made against ([`crate::protocol::Request::PinCurrent`]): this start's
    /// id and the selection generation. A restart makes every earlier one stale, whatever the generation
    pub(crate) fn selection_token(&self) -> String {
        format!("{}:{}", self.boot_id, self.selection_generation.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// `instance add`'s compare-and-set (docs/dev/multi-instance-plan.md 4.4): the default instance selects for good
    /// the device it controls or has selected now, if that is `udid` and nothing changed since `token`. Under the
    /// selection latch, so no switch is under way and none starts before it is written
    pub(crate) async fn pin_current(&self, udid: &str, token: &str) -> Result<String> {
        if self.cfg.instance.is_some() {
            return Err(ictl_l10n::tr!("只有默认实例要固定设备", "Only the default instance pins its device").into());
        }
        let _latch = self.select_latch.lock().await;
        self.gate.check_open()?;
        let mut g = self.inner.lock().await;
        if self.selection_token() != token {
            return Err(ictl_l10n::tr!(
                "默认实例的设备选择在确认之后变了（或者它重启过），没有固定；重新运行 instance add",
                "The default instance's device selection changed since it was confirmed (or it restarted); nothing was pinned. Run instance add again"
            )
            .into());
        }
        let current = g.device.as_ref().map(|d| d.udid.clone()).or_else(|| self.pinned(&g));
        if !current.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(udid)) {
            return Err(ictl_l10n::trf!(
                "默认实例现在控制的不是 {udid}，没有固定",
                "The default instance doesn't control {udid} now; nothing was pinned"
            )
            .into());
        }
        let udid = current.unwrap_or_default();
        if g.persisted.selected_udid.as_deref() != Some(udid.as_str()) && self.cfg.udid.is_none() {
            let before = g.persisted.selected_udid.replace(udid.clone());
            if let Err(e) = g.persisted.try_save(&self.cfg.state_file()) {
                g.persisted.selected_udid = before;
                return Err(not_saved(&e).into());
            }
        }
        // moved on even when it was selected already: the token confirmed is spent
        self.selection_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.selection_token())
    }

    /// `select auto`, which a default instance sharing the host with named ones doesn't have
    fn select_auto(&self, g: &mut Inner) -> Result<String> {
        if let Some(u) = &self.cfg.udid {
            return Err(ictl_l10n::trf!(
                "IPHONE_UDID 固定为 {u};要改回自动选,先从 ~/.config/iphone-control/env 里删掉它,再重启守护进程",
                "IPHONE_UDID pins {u}; to go back to automatic, remove it from ~/.config/iphone-control/env and restart the daemon"
            ).into());
        }
        if self.restricted() {
            return Err(ictl_l10n::tr!(
                "有命名实例时默认实例不能自动选设备（会选到别的实例的）：用 iphone-control select <UDID> 选定一台",
                "With named instances the default instance can't choose a device by itself (it could take another instance's): select one with iphone-control select <UDID>"
            )
            .into());
        }
        let before = g.persisted.selected_udid.take();
        if let Err(e) = g.persisted.try_save(&self.cfg.state_file()) {
            g.persisted.selected_udid = before;
            return Err(not_saved(&e).into());
        }
        self.selection_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(match g.info.as_ref().filter(|_| g.device.is_some()) {
            Some(i) => ictl_l10n::trf!("已改回自动选;正在控制 {},它断开时换成别的接着的设备", "Back to automatic; controlling {}, switches to another connected device when it disconnects", device_label(i)),
            None if g.connected.is_empty() => ictl_l10n::tr!("已改回自动选;现在没接设备,接上后自动控制它", "Back to automatic; no device connected now, it takes over the next one plugged in").into(),
            None => ictl_l10n::tr!("已改回自动选;10 秒内接管接着的设备,先用 iphone-control status 或 iphone_status 确认就绪", "Back to automatic; it takes over a connected device within 10 seconds, check readiness with iphone-control status or iphone_status first").into(),
        })
    }

    /// The default instance may not take a device a named instance has or is getting. Read now, every time (an
    /// instance may be under way); a registry that can't be read refuses
    fn may_take(&self, udid: &str) -> Result<()> {
        if self.cfg.instance.is_some() {
            return Ok(());
        }
        let registry = self.cfg.registry_dir();
        let scan = crate::instance::scan(&registry).map_err(|e| {
            ictl_l10n::trf!(
                "读不了实例登记（{}）：{e}；没有切换",
                "The instance registry can't be read ({}): {e}; not switched",
                registry.display()
            )
        })?;
        match scan.reserved_by(udid) {
            Some(n) => Err(ictl_l10n::trf!(
                "这台设备登记给了实例 {n}，默认实例不能控制它",
                "This device is registered to the instance {n}; the default instance can't control it"
            )
            .into()),
            None => Ok(()),
        }
    }

    /// The selection written, on disk, or an error and nothing changed. The selection generation moves on
    fn persist_selection(&self, g: &mut Inner, udid: &str) -> Result<()> {
        if self.cfg.udid.is_some() {
            return Ok(());
        }
        let before = g.persisted.selected_udid.replace(udid.to_owned());
        if before.as_deref() == Some(udid) {
            return Ok(());
        }
        if let Err(e) = g.persisted.try_save(&self.cfg.state_file()) {
            g.persisted.selected_udid = before;
            return Err(not_saved(&e).into());
        }
        self.selection_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Step 1: the new device's lease. A probe holding it is waited for a moment; an instance holding it is an error
    async fn claim_for_switch(&self, udid: &str, holder: crate::lease::Holder, label: &str) -> Result<Lease> {
        let deadline = Instant::now() + PROBE_WAIT;
        loop {
            match crate::lease::try_claim(&self.cfg.devices_dir(), udid, holder.clone()) {
                Ok(Claim::Got(l)) => return Ok(l),
                // this process's own, held by work still going on there since it was let go of
                Ok(Claim::Held(Some(h))) if h.is_own(self.cfg.instance_name()) => {
                    return Err(ictl_l10n::trf!(
                        "{label} 上还有之前的操作在进行（比如重签），稍后再试",
                        "Work from before is still under way on {label} (a re-signing, say); try again shortly"
                    )
                    .into());
                }
                Ok(Claim::Held(Some(h))) if h.instance_name().is_some() => {
                    return Err(held_by(label, h.instance_name().unwrap_or_default()).into());
                }
                Ok(Claim::Held(_)) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Ok(Claim::Held(_)) => {
                    return Err(ictl_l10n::trf!(
                        "{label} 正被另一个进程使用，稍后再试",
                        "{label} is in use by another process; try again shortly"
                    )
                    .into());
                }
                Err(e) => {
                    return Err(ictl_l10n::trf!("拿不到 {label} 的租约：{e}", "Can't take {label}'s lease: {e}").into());
                }
            }
        }
    }

    /// Steps 2 to 6 for the device controlled now: the gate closed, the work on it ended or waited for, its session
    /// closed. `Err`: back as it was (the gate open, the device kept), saying what is under way. On success the gate
    /// stays closed, for the caller to open
    async fn drain_old<'a>(
        &'a self,
        mut g: tokio::sync::MutexGuard<'a, Inner>,
    ) -> Result<tokio::sync::MutexGuard<'a, Inner>> {
        // 2. closed under the device lock: a request that gets the lock after this finds it so
        let long = self.gate.close();
        // 3. long work isn't waited for (v1 doesn't queue a switch behind it)
        if !long.is_empty() {
            self.gate.reopen();
            let what: Vec<&str> = long.iter().map(|l| l.describe()).collect();
            return Err(ictl_l10n::trf!(
                "当前设备上{}，没有切换；做完后再切换",
                "{} on the current device; not switched. Switch once it is done",
                what.join(ictl_l10n::tr!("、", ", "))
            )
            .into());
        }
        let starting = g.starting.take();
        let preparing = g.preparing.take();
        drop(g);
        // 4. without the device lock: work let in may need it to finish
        self.gate.revoke_streams();
        // aborted, they end at their next await: waited for, so nothing of them is left on the old device
        if let Some(h) = starting {
            h.abort();
            let _ = h.await;
        }
        if let Some(h) = preparing {
            h.abort();
            let _ = h.await;
        }
        if let Err(left) = self.gate.drain(DRAIN).await {
            // 5. back: the old device, its lease and the selection as they were
            self.gate.reopen();
            return Err(ictl_l10n::trf!(
                "当前设备上还有 {left} 个操作在进行（{} 秒内没结束），没有切换；稍后再试",
                "{left} operations are still under way on the current device (not done within {} s); not switched. Try again shortly",
                DRAIN.as_secs()
            )
            .into());
        }
        // 6. the old device's session closes
        let mut g = self.inner.lock().await;
        if g.device.is_some() {
            g.offline_target = None;
            g.device = None;
            g.info = None;
            self.forget_device(&mut g).await;
        }
        Ok(g)
    }

    fn switched(&self, g: &Inner, target: &ictl_device::DeviceInfo, label: &str) -> String {
        let pin = if self.cfg.passcode_file_for(&target.udid).is_file() {
            ""
        } else {
            ictl_l10n::tr!(
                ";这台没设自动解锁密码,锁屏时要人工解锁(在这台连着时运行 iphone-control passcode set)",
                "; this device has no auto-unlock passcode, so unlock it by hand when locked (run iphone-control passcode set while it is connected)"
            )
        };
        let wait = if self.cfg.ios_backend.drives_natively(Some(target)) {
            ictl_l10n::tr!("原生控制几秒就绪", "native control is ready in seconds")
        } else {
            ictl_l10n::tr!("WDA 起来要 10~60 秒", "WDA takes 10-60 seconds to start")
        };
        ictl_l10n::trf!(
            "已切换到 {label},之后固定控制这台。状态:{};{wait},先用 iphone-control status 或 iphone_status 确认就绪{pin}",
            "Switched to {label}; it stays selected from now on. State: {}; {wait}, check readiness with iphone-control status or iphone_status first{pin}",
            g.state.describe()
        )
    }
}

fn held_by(label: &str, by: &str) -> String {
    ictl_l10n::trf!(
        "{label} 由实例 {by} 控制，不能切换过去",
        "{label} is controlled by the instance {by}; can't switch to it"
    )
}

fn not_saved(e: &std::io::Error) -> String {
    ictl_l10n::trf!(
        "选定的设备没能写进 state.json（{e}），没有切换",
        "The selected device couldn't be written to state.json ({e}); not switched"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ictl_device::{Device, UsbDevice};

    use crate::core::Daemon;
    use crate::lease::{Claim, Holder, try_claim};

    const A: &str = "00008130-00000000000000AA";
    const B: &str = "00008020-00000000000000BB";

    fn usb(udid: &str, id: u32) -> UsbDevice {
        UsbDevice { udid: udid.into(), device_id: id }
    }

    /// A daemon controlling A, with B connected too
    async fn controlling_a(tag: &str) -> (Arc<Daemon>, std::path::PathBuf) {
        let (d, dir) = crate::core::tests::test_daemon(tag);
        let mut g = d.inner.lock().await;
        g.connected = vec![usb(A, 1), usb(B, 2)];
        let Claim::Got(l) = try_claim(&d.cfg.devices_dir(), A, d.holder(&g)).unwrap() else { panic!("free") };
        d.took(&mut g, l);
        g.device = Some(Device::from_usb(&usb(A, 1)));
        drop(g);
        (d, dir)
    }

    fn free(d: &Daemon, udid: &str) -> bool {
        matches!(try_claim(&d.cfg.devices_dir(), udid, Holder::probe("test")).unwrap(), Claim::Got(_))
    }

    async fn local<F: std::future::Future>(f: F) -> F::Output {
        tokio::task::LocalSet::new().run_until(f).await
    }

    /// Switched: the new lease held, the old one let go, the selection written and its generation moved on, the gate
    /// open again
    #[tokio::test]
    async fn a_switch_moves_the_lease_and_the_selection() {
        local(async {
            let (d, dir) = controlling_a("switch-ok").await;
            // a caller gone while it waited: nothing switched, nothing written
            let (tx, rx) = tokio::sync::oneshot::channel();
            tx.send(()).unwrap();
            assert!(d.select(B, &mut Some(rx)).await.is_err());
            assert_eq!(d.inner.lock().await.persisted.selected_udid, None);
            assert!(free(&d, B));
            let said = d.select(B, &mut None).await.unwrap();
            assert!(said.contains(B), "{said}");
            // the snapshot says B as the answer comes back: a task or a stream opened right after goes by it
            assert_eq!(d.controlled_udid().as_deref(), Some(B));
            let g = d.inner.lock().await;
            assert!(g.lease.as_ref().is_some_and(|l| l.is_for(B)));
            assert_eq!(g.persisted.selected_udid.as_deref(), Some(B));
            drop(g);
            assert!(free(&d, A), "the old lease went");
            // instance add's compare-and-set: against the token of now only, for the device controlled
            let token = d.selection_token();
            assert!(d.pin_current(A, &token).await.is_err(), "not the device controlled");
            let next = d.pin_current(B, &token).await.unwrap();
            assert_ne!(next, token);
            assert!(d.pin_current(B, &token).await.is_err(), "a token is spent once");
            assert!(d.pin_current(B, &format!("other-boot:{}", d.identity().selection_generation)).await.is_err());
            assert!(!free(&d, B));
            assert_eq!(d.identity().selection_generation, 2);
            d.gate.check_open().unwrap();
            std::fs::remove_dir_all(dir).unwrap();
        })
        .await;
    }

    /// A device waited for in the background (a probe held it) and got after another one was chosen is let go of,
    /// not held idle
    #[tokio::test]
    async fn a_claim_overtaken_by_another_device_lets_go() {
        local(async {
            let (d, dir) = crate::core::tests::test_daemon("claim-overtaken");
            let Claim::Got(l) = try_claim(&d.cfg.devices_dir(), A, Holder::instance("default")).unwrap() else {
                panic!()
            };
            *d.claim_for.lock().unwrap() = Some(A.into());
            *d.claimed.lock().unwrap() = Some(l);
            let mut g = d.inner.lock().await;
            g.connected = vec![usb(B, 2)];
            assert!(matches!(d.round_target(&mut g).await, crate::claim::Round::Device(u) if u.udid == B));
            drop(g);
            assert!(free(&d, A), "let go of");
            assert!(d.claim_for.lock().unwrap().is_none());
            std::fs::remove_dir_all(dir).unwrap();
        })
        .await;
    }

    /// Work let in on a device keeps its lease held after the instance let go of it (the device gone in automatic
    /// mode, say), and no other device is taken until that work ends (docs/dev/multi-instance-plan.md 10.5)
    #[tokio::test]
    async fn work_on_a_device_let_go_of_keeps_its_lease_until_it_ends() {
        local(async {
            let (d, dir) = controlling_a("hold-after-let-go").await;
            let resign = d.gate.admit(Some(crate::admission::Long::Resign)).unwrap();
            let mut g = d.inner.lock().await;
            d.let_go(&mut g).await;
            assert!(!free(&d, A), "still held by the re-signing");
            g.connected = vec![usb(B, 2)];
            assert!(matches!(d.round_target(&mut g).await, crate::claim::Round::Offline));
            assert!(g.lease.is_none(), "B isn't taken while A's work goes on");
            drop(resign);
            assert!(free(&d, A));
            assert!(matches!(d.round_target(&mut g).await, crate::claim::Round::Device(u) if u.udid == B));
            drop(g);
            std::fs::remove_dir_all(dir).unwrap();
        })
        .await;
    }

    /// Long work under way isn't waited for: refused at once, and nothing changed; short work that doesn't end in
    /// time: the same, after the deadline
    #[tokio::test]
    async fn a_switch_goes_back_when_work_on_the_old_device_goes_on() {
        local(async {
            let (d, dir) = controlling_a("switch-busy").await;
            let resign = d.gate.admit(Some(crate::admission::Long::Resign)).unwrap();
            let e = d.select(B, &mut None).await.unwrap_err().to_string();
            assert!(e.contains(crate::admission::Long::Resign.describe()), "{e}");
            drop(resign);
            let short = d.gate.admit(None).unwrap();
            let t = std::time::Instant::now();
            let e = d.select(B, &mut None).await.unwrap_err().to_string();
            assert!(t.elapsed() >= super::DRAIN, "waited for it first");
            assert!(e.contains('1'), "{e}");
            assert!(short.current(), "the old lease is still the one held");
            let g = d.inner.lock().await;
            assert!(g.lease.as_ref().is_some_and(|l| l.is_for(A)) && g.device.is_some());
            assert_eq!(g.persisted.selected_udid, None);
            drop(g);
            assert!(free(&d, B), "the new lease was let go");
            d.gate.check_open().unwrap();
            std::fs::remove_dir_all(dir).unwrap();
        })
        .await;
    }

    /// Another instance's device is refused, naming it; the selection on disk failing to be written goes back to the
    /// old device's lease
    #[tokio::test]
    async fn a_switch_to_another_instances_device_or_unsaved_goes_back() {
        local(async {
            let (d, dir) = controlling_a("switch-refused").await;
            let Claim::Got(theirs) = try_claim(&d.cfg.devices_dir(), B, Holder::instance("ipad")).unwrap() else {
                panic!()
            };
            let e = d.select(B, &mut None).await.unwrap_err().to_string();
            assert!(e.contains("ipad"), "{e}");
            drop(theirs);
            // state.json can't be replaced: a directory where its temporary file goes
            std::fs::create_dir_all(d.cfg.state_file().with_extension("json.tmp")).unwrap();
            let e = d.select(B, &mut None).await.unwrap_err().to_string();
            assert!(e.contains("state.json"), "{e}");
            let g = d.inner.lock().await;
            assert!(g.lease.as_ref().is_some_and(|l| l.is_for(A)));
            assert_eq!(g.persisted.selected_udid, None);
            drop(g);
            assert!(free(&d, B));
            assert_eq!(d.identity().selection_generation, 0);
            std::fs::remove_dir_all(dir).unwrap();
        })
        .await;
    }

    /// The default instance while a named one is registered or being added: no `select auto`, not that one's device,
    /// and a round without a device selected takes none
    #[tokio::test]
    async fn the_default_instance_beside_named_ones_takes_only_its_own() {
        local(async {
            let dir = std::env::temp_dir().join(format!("ictl-core-restricted-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let mut cfg = crate::Config::from_env(&dir);
            cfg.registry = dir.join("instances");
            let d = Daemon::new(cfg);
            d.inner.lock().await.connected = vec![usb(A, 1), usb(B, 2)];
            // not registered yet: automatic, the first one
            let mut g = d.inner.lock().await;
            assert!(matches!(d.round_target(&mut g).await, crate::claim::Round::Device(u) if u.udid == A));
            // controlling A when a named instance appears (`instance add` about to pin it): A is kept
            g.device = Some(Device::from_usb(&usb(A, 1)));
            drop(g);
            std::fs::create_dir_all(&d.cfg.registry).unwrap();
            let p = crate::instance::Pending { name: "ipad".into(), udid: B.into(), port: 8203, started: 1 };
            std::fs::write(d.cfg.registry.join("ipad.pending"), serde_json::to_vec(&p).unwrap()).unwrap();
            let mut g = d.inner.lock().await;
            assert!(matches!(d.round_target(&mut g).await, crate::claim::Round::Device(u) if u.udid == A));
            // gone: none other is chosen
            g.device = None;
            drop(g);
            assert!(d.select("auto", &mut None).await.is_err());
            let e = d.select(B, &mut None).await.unwrap_err().to_string();
            assert!(e.contains("ipad"), "{e}");
            let mut g = d.inner.lock().await;
            assert!(matches!(d.round_target(&mut g).await, crate::claim::Round::Held));
            assert_eq!(g.state, crate::machine::DeviceState::NeedsSelection);
            assert!(g.lease.is_none(), "let go of");
            drop(g);
            assert!(free(&d, A));
            std::fs::remove_dir_all(dir).unwrap();
        })
        .await;
    }

    /// A selected device another instance holds: the round says so and keeps none of it; once let go, the next round
    /// takes it. A case of its UDID doesn't make it another device
    #[tokio::test]
    async fn a_selected_device_held_elsewhere_is_waited_for() {
        local(async {
            let (d, dir) = crate::core::tests::test_daemon("claimed-elsewhere");
            let mut g = d.inner.lock().await;
            g.connected = vec![usb(A, 1)];
            g.persisted.selected_udid = Some(A.to_ascii_lowercase());
            let Claim::Got(theirs) = try_claim(&d.cfg.devices_dir(), A, Holder::instance("phone")).unwrap() else {
                panic!()
            };
            assert!(matches!(d.round_target(&mut g).await, crate::claim::Round::Held));
            assert_eq!(g.state, crate::machine::DeviceState::ClaimedElsewhere { by: "phone".into() });
            drop(theirs);
            assert!(matches!(d.round_target(&mut g).await, crate::claim::Round::Device(_)));
            assert!(g.lease.as_ref().is_some_and(|l| l.is_for(A)));
            drop(g);
            std::fs::remove_dir_all(dir).unwrap();
        })
        .await;
    }
}
