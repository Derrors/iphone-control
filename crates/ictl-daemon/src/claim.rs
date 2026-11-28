//! Which device the supervision round controls, under which lease (docs/dev/multi-instance-plan.md 4.1).
//!
//! - Selected (`IPHONE_UDID`, `select`, or a named instance's own device): only that one. Another instance holding
//!   it: [`DeviceState::ClaimedElsewhere`], tried again every round. Disconnected: the lease is kept, so plugged back
//!   in it isn't taken by someone else meanwhile (D11: never another device silently).
//! - Automatic (only without named instances): the device in use while it is connected, else the first one no one
//!   holds. Disconnected: the lease goes, another may be chosen.
//! - The default instance while named instances are registered or being added: only the device selected for it;
//!   none selected is [`DeviceState::NeedsSelection`].
//!
//! The registry is read every round (one `readdir`): an instance being added is seen at once.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ictl_device::UsbDevice;

use crate::core::{Daemon, Inner};
use crate::lease::{Claim, Holder};
use crate::machine::DeviceState;

/// How long a lease a probe holds is waited for in the background, and how often it is tried
const PROBE_WAIT: Duration = Duration::from_secs(2);
const PROBE_RETRY: Duration = Duration::from_millis(200);

/// What this round controls ([`Daemon::round_target`])
pub(crate) enum Round {
    /// This device, its lease held
    Device(UsbDevice),
    /// None connected that it may take: offline
    Offline,
    /// None, and the state says why ([`DeviceState::ClaimedElsewhere`], [`DeviceState::NeedsSelection`]), or a probe
    /// has the selected one for a moment: not offline
    Held,
}

/// What a round found for a device it would control
enum Leased {
    Yes,
    /// Another instance controls it
    Elsewhere(String),
    /// Not now: a probe holds it (taken in the background, see [`Daemon::claim_later`]) or it couldn't be tried
    NotNow,
}

impl Daemon {
    /// Whether this is the default instance while named ones are registered or being added: then it controls only
    /// the device selected for it. A registry that can't be read counts as having some
    pub(crate) fn restricted(&self) -> bool {
        self.cfg.instance.is_none() && self.cfg.shared_with_others()
    }

    /// How this instance's lease describes it
    pub(crate) fn holder(&self, g: &Inner) -> Holder {
        Holder::instance(self.cfg.instance_name()).with_device(g.info.as_ref())
    }

    /// The device this round controls, with its lease taken
    pub(crate) async fn round_target(self: &Arc<Self>, g: &mut Inner) -> Round {
        let pinned = self.pinned(g);
        if pinned.is_none() && self.restricted() {
            self.let_go(g).await;
            self.set_state(g, DeviceState::NeedsSelection);
            return Round::Held;
        }
        let udids: Vec<&str> = g.connected.iter().map(|d| d.udid.as_str()).collect();
        // just started, nothing controlled yet: in automatic mode go on with the last device that was ready (kept in
        // state.json), not whichever usbmuxd happens to list first
        let current = g.device.as_ref().map(|d| d.udid.as_str()).or(g.persisted.screen_udid.as_deref());
        let first = crate::machine::pick_device(&udids, pinned.as_deref(), current);
        // automatic: the others in turn, when the first is someone else's
        let order: Vec<usize> = match (first, &pinned) {
            (None, _) => Vec::new(),
            (Some(i), Some(_)) => vec![i],
            (Some(i), None) => std::iter::once(i).chain((0..g.connected.len()).filter(|&j| j != i)).collect(),
        };
        let (mut elsewhere, mut not_now) = (None, false);
        for i in order {
            let d = g.connected[i].clone();
            match self.lease_for(g, &d.udid) {
                Leased::Yes => {
                    self.read_leased(g, &d).await;
                    return Round::Device(d);
                }
                Leased::Elsewhere(by) => elsewhere = elsewhere.or(Some(by)),
                Leased::NotNow => not_now = true,
            }
        }
        match (elsewhere, &pinned) {
            // the one selected is someone else's: say so, and keep none of it
            (Some(by), Some(_)) => {
                self.let_go(g).await;
                self.set_state(g, DeviceState::ClaimedElsewhere { by });
                Round::Held
            }
            (None, Some(_)) if not_now => Round::Held,
            _ => Round::Offline,
        }
    }

    /// After an offline round: in automatic mode the lease goes with the device; selected, it stays (plugged back in,
    /// the device is still this instance's)
    pub(crate) fn offline_lease(&self, g: &mut Inner) {
        if self.pinned(g).is_none()
            && let Some(l) = g.lease.take()
        {
            tracing::info!("let go of the lease of {}", l.udid());
            self.gate.revoke_streams();
            self.gate.advance();
        }
    }

    /// This instance's lease of `udid`, taken now if no one holds it
    fn lease_for(self: &Arc<Self>, g: &mut Inner, udid: &str) -> Leased {
        if g.lease.as_ref().is_some_and(|l| l.is_for(udid)) {
            return Leased::Yes;
        }
        let waited = self.claimed.lock().unwrap_or_else(|p| p.into_inner()).take();
        let claim = match waited {
            Some(l) if l.is_for(udid) => Ok(Claim::Got(l)),
            _ => crate::lease::try_claim(&self.cfg.devices_dir(), udid, self.holder(g)),
        };
        match claim {
            Ok(Claim::Got(lease)) => {
                self.took(g, lease);
                Leased::Yes
            }
            Ok(Claim::Held(Some(h))) if h.instance_name().is_some() => {
                Leased::Elsewhere(h.instance_name().unwrap_or_default().to_owned())
            }
            Ok(Claim::Held(_)) => {
                self.claim_later(udid.to_owned(), self.holder(g));
                Leased::NotNow
            }
            Err(e) => {
                tracing::warn!("taking the lease of {udid} failed: {e}");
                Leased::NotNow
            }
        }
    }

    /// A lease taken, in place of the one held before: what is let in from now on belongs to it, and whatever another
    /// instance left on the phone is looked for again (docs/dev/multi-instance-plan.md 4.6)
    pub(crate) fn took(&self, g: &mut Inner, lease: crate::lease::Lease) {
        let udid = lease.udid().to_owned();
        if let Some(old) = g.lease.replace(lease) {
            tracing::info!("let go of the lease of {}", old.udid());
        }
        let generation = self.gate.advance();
        tracing::info!("took the lease of {udid} (generation {generation})");
        if crate::hidmark::is_left(&self.cfg.hid_marker_dir(), &udid) {
            g.hid_orphans.entry(udid).or_insert(0);
        }
    }

    /// A probe (another instance reading the device's details for a moment) holds the lease: it is tried again in the
    /// background for a short while, without holding up the round or the requests behind it; the next round takes it
    fn claim_later(self: &Arc<Self>, udid: String, holder: Holder) {
        if self.claiming.swap(true, Ordering::SeqCst) {
            return;
        }
        let d = self.clone();
        tokio::task::spawn_local(async move {
            let deadline = tokio::time::Instant::now() + PROBE_WAIT;
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(PROBE_RETRY).await;
                match crate::lease::try_claim(&d.cfg.devices_dir(), &udid, holder.clone()) {
                    Ok(Claim::Got(l)) => {
                        *d.claimed.lock().unwrap_or_else(|p| p.into_inner()) = Some(l);
                        d.wake.notify_one();
                        break;
                    }
                    Ok(Claim::Held(Some(h))) if !h.is_probe() => break,
                    Ok(Claim::Held(_)) => {}
                    Err(_) => break,
                }
            }
            d.claiming.store(false, Ordering::SeqCst);
        });
    }

    /// No device and no lease any more: the session goes, the streams end, work let in under the lease is no longer
    /// current. For a device gone or not to be controlled; a switch to another one goes through
    /// `Daemon::select` (`crate::switch`), which waits for that work
    pub(crate) async fn let_go(&self, g: &mut Inner) {
        if g.device.is_some() {
            g.offline_target = Some(ictl_l10n::in_own_lang(|| crate::notify::Target::of(g.info.as_ref())));
            g.device = None;
            g.info = None;
            self.forget_device(g).await;
        }
        if let Some(l) = g.lease.take() {
            tracing::info!("let go of the lease of {}", l.udid());
            self.gate.revoke_streams();
            self.gate.advance();
        }
    }

    /// The lease's hint follows what is known of the device
    pub(crate) fn describe_lease(&self, g: &mut Inner) {
        let holder = self.holder(g);
        if let Some(l) = g.lease.as_mut()
            && let Err(e) = l.describe(holder)
        {
            tracing::debug!("describing the lease of {} failed: {e}", l.udid());
        }
    }
}
