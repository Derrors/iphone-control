//! The daily Apple ID session check, run by one instance for all of them (docs/dev/multi-instance-plan.md 4.2).
//!
//! Every daemon runs this task, apart from the device and its lock. Each round, an instance that may lead tries the
//! leader's lock (`<data root>/account-check.lock`, never waited for); whoever holds it checks when the shared state
//! says it is due (`account/session-check.json`), so a check isn't run twice a day by several instances, and another
//! takes over when the leader stops or no longer may lead (it lets go at the end of that round).
//!
//! May lead: there is an account, the anisette state isn't from another address than the one configured (nor of
//! unknown origin while it may not be used), and the instance uses WDA (a phone driven natively needs no Apple ID).
//!
//! A round records the credentials generation it started with. A result worked out with credentials that changed
//! meanwhile (signed in again) is dropped, and the check is due again at once. Before notifying that the session
//! expired, the round claims the notification in the state (by generation and round): a leader taking over never sends
//! it again; a crash between the two may miss one, which the next day's check sends. A round the maintenance lease
//! isn't to be had for (an update) isn't run, and stays due.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::resign::{SessionCheckOutcome, now_secs};
use crate::core::Daemon;

/// How often an instance looks at whether it leads and whether a check is due
const ROUND_EVERY: Duration = Duration::from_secs(60);
/// The first look after a start: the device comes first
const FIRST_AFTER: Duration = Duration::from_secs(90);
const STATE_FILE: &str = "session-check.json";
/// Claimed notifications kept
const CLAIMS_KEPT: usize = 16;

/// The shared state of the checks
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CheckState {
    /// When the next check is due (Unix seconds)
    pub due: u64,
    /// The last round started
    pub round: u64,
    /// The credentials generation it started with
    pub generation: u64,
    /// The last result, as logged, and when
    #[serde(default)]
    pub last: Option<String>,
    #[serde(default)]
    pub last_at: Option<u64>,
    /// The (generation, round) of the session-expired notifications claimed
    #[serde(default)]
    pub notified: Vec<(u64, u64)>,
}

impl CheckState {
    pub(crate) fn load(dir: &Path) -> Self {
        std::fs::read(dir.join(STATE_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub(crate) fn save(&self, dir: &Path) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        ictl_unlock::durable::write_atomic(dir, STATE_FILE, &data, 0o600)
    }

    /// Claims the notification of `(generation, round)`; false when it was claimed already
    pub(crate) fn claim(&mut self, generation: u64, round: u64) -> bool {
        if self.notified.contains(&(generation, round)) {
            return false;
        }
        self.notified.push((generation, round));
        let extra = self.notified.len().saturating_sub(CLAIMS_KEPT);
        self.notified.drain(..extra);
        true
    }
}

/// The leader's lock, if no one else holds it
fn try_lead(path: &Path) -> Option<File> {
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path).ok()?;
    // SAFETY: a valid descriptor, owned by `f` for as long as the lock is held
    (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
}

impl Daemon {
    /// The task: started with the daemon, ends with it
    pub(crate) async fn lead_session_checks(self: Arc<Self>) {
        tokio::time::sleep(FIRST_AFTER).await;
        let mut lead = None;
        loop {
            lead = self.session_round(lead).await;
            tokio::time::sleep(ROUND_EVERY).await;
        }
    }

    /// Whether this instance may lead the checks now
    async fn may_lead(&self) -> bool {
        use ictl_sign::account::OriginCheck;
        let account = self.account();
        let origin_ok = match account.anisette_origin() {
            Ok(OriginCheck::Fresh | OriginCheck::Known) => true,
            Ok(OriginCheck::Unknown) => !account.require_known_origin,
            Ok(OriginCheck::Mismatch { .. }) | Err(_) => false,
        };
        if !origin_ok || account.selected_email().await.is_err() {
            return false;
        }
        let g = self.inner.lock().await;
        if self.cfg.shared_with_others() {
            // shared by several instances: the check is everyone's, its alert sent by the leader. One that couldn't
            // send it (no notification configured) doesn't lead, so it can't take the check from one that can
            self.cfg.notify.any()
        } else {
            // alone: as before, only for a device that uses WDA (a phone driven natively needs no Apple ID)
            wda_checks_session(g.persisted.wda_bundle.is_some(), g.device.is_some(), self.native(&g))
        }
    }

    /// One round: the lock held from before (or taken now), returned when this instance still leads
    async fn session_round(self: &Arc<Self>, lead: Option<File>) -> Option<File> {
        if !self.may_lead().await {
            return None;
        }
        let lead = lead.or_else(|| try_lead(&self.cfg.account_check_lock()))?;
        let dir = self.cfg.account_dir();
        let mut st = CheckState::load(&dir);
        if now_secs() < st.due {
            return Some(lead);
        }
        // an update under way: not run this round, still due
        let Ok(maintenance) = crate::maintenance::shared() else { return Some(lead) };
        let account = self.account();
        let g0 = match account.credentials_generation().await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("reading the credentials generation failed: {e}");
                return Some(lead);
            }
        };
        st.round += 1;
        st.generation = g0;
        let round = st.round;
        if let Err(e) = st.save(&dir) {
            tracing::warn!("saving the session check's state failed: {e}");
            return Some(lead);
        }
        // the lock goes with the check: a daemon stopping meanwhile doesn't let go of it before the check ends
        let (outcome, lead) = self.check_session_with(lead).await;
        drop(maintenance);
        // the generation checked and the result written (with the notification claimed) in one step, under the
        // account lock: a sign-in again can't come in between; the device lock and the notification after it
        let (notify, keep) = match settle(&account, &dir, g0, round, &outcome, self.cfg.session_check).await {
            Ok(Settled::Stale) => return lead,
            Ok(Settled::Done { notify, keep }) => (notify, keep),
            Err(e) => {
                tracing::warn!("recording the session check's result failed: {e}");
                return lead;
            }
        };
        {
            let mut g = self.inner.lock().await;
            self.apply_session_check(&mut g, outcome, Instant::now(), notify);
        }
        lead.filter(|_| keep)
    }
}

/// A lone instance checks the session as before this version did: with WDA installed, unless the device in use is
/// driven natively
fn wda_checks_session(has_bundle: bool, device: bool, native: bool) -> bool {
    has_bundle && !(device && native)
}

/// What [`settle`] did with a check's result
#[derive(Debug, PartialEq, Eq)]
enum Settled {
    /// The credentials changed since the check began (signed in again): dropped, checked again at once
    Stale,
    /// Recorded; `notify`: the session-expired notification is this round's to send (claimed); `keep`: still leads
    Done { notify: bool, keep: bool },
}

/// A check's result recorded, with the account lock held: the credentials generation read again (with its digest)
/// and compared with the one the check began with, then the result, the next due time and the notification's claim
/// written in one save. Nothing else is waited for with the lock held
async fn settle(
    account: &ictl_sign::AccountDir,
    dir: &Path,
    g0: u64,
    round: u64,
    outcome: &SessionCheckOutcome,
    every: Duration,
) -> std::io::Result<Settled> {
    let lock = account.lock().await.map_err(std::io::Error::other)?;
    let now = account.generation_locked(&lock).map_err(std::io::Error::other)?;
    let mut st = CheckState::load(dir);
    if now != g0 {
        st.due = now_secs();
        st.save(dir)?;
        return Ok(Settled::Stale);
    }
    let (next, keep) = outcome.schedule(every);
    let notify = matches!(outcome, SessionCheckOutcome::Expired(_)) && st.claim(g0, round);
    st.due = now_secs() + next.as_secs();
    st.last = Some(outcome.describe());
    st.last_at = Some(now_secs());
    // a claim that isn't on disk isn't sent: a leader taking over would send it again
    st.save(dir)?;
    drop(lock);
    Ok(Settled::Done { notify, keep })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> (ictl_sign::AccountDir, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ictl-session-settle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (ictl_sign::AccountDir { dir: dir.clone(), anisette_url: None, require_known_origin: false }, dir)
    }

    /// A result of the credentials of before (signed in again after the check began, before its result is
    /// recorded) is dropped and checked again; one of the credentials of now is recorded with its claim, once
    #[tokio::test]
    async fn a_result_is_recorded_only_for_the_credentials_it_checked() {
        let (a, dir) = account();
        let every = Duration::from_secs(24 * 3600);
        let expired = SessionCheckOutcome::Expired("code 1100".into());
        let g0 = a.credentials_generation().await.unwrap();
        // signed in again between the check and its result
        std::fs::write(dir.join("accounts.json"), b"{\"a\":2}").unwrap();
        assert_eq!(settle(&a, &dir, g0, 1, &expired, every).await.unwrap(), Settled::Stale);
        let st = CheckState::load(&dir);
        assert!(st.notified.is_empty() && st.due <= now_secs(), "nothing claimed, due again");
        let g1 = a.credentials_generation().await.unwrap();
        assert_eq!(settle(&a, &dir, g1, 2, &expired, every).await.unwrap(), Settled::Done { notify: true, keep: true });
        assert_eq!(
            settle(&a, &dir, g1, 2, &expired, every).await.unwrap(),
            Settled::Done { notify: false, keep: true }
        );
        // the account lock is held while it is recorded: a sign-in's commit waits for it
        let held = a.lock().await.unwrap();
        let t = tokio::spawn({
            let (a, dir) = (a.clone(), dir.clone());
            async move { settle(&a, &dir, g1, 3, &SessionCheckOutcome::Ok(None), every).await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!t.is_finished(), "waits for the account lock");
        drop(held);
        assert_eq!(t.await.unwrap(), Settled::Done { notify: false, keep: true });
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A notification is claimed once per generation and round, across leaders (the state is read again), and only a
    /// bounded number are kept
    #[test]
    fn a_notification_is_claimed_once() {
        let dir = std::env::temp_dir().join(format!("ictl-session-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut st = CheckState::load(&dir);
        assert_eq!(st, CheckState::default());
        assert!(st.claim(3, 7));
        st.save(&dir).unwrap();
        let mut next_leader = CheckState::load(&dir);
        assert!(!next_leader.claim(3, 7), "claimed by the leader before");
        assert!(next_leader.claim(3, 8), "another round");
        assert!(next_leader.claim(4, 7), "another generation");
        for r in 0..40 {
            next_leader.claim(5, r);
        }
        assert_eq!(next_leader.notified.len(), CLAIMS_KEPT);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A lone instance checks only as before: with WDA, not for a phone driven natively (no Apple ID needed there)
    #[test]
    fn a_lone_instance_checks_as_before() {
        assert!(wda_checks_session(true, true, false));
        assert!(wda_checks_session(true, false, false), "no device connected: checked, an expiry found early");
        assert!(!wda_checks_session(true, true, true), "driven natively");
        assert!(!wda_checks_session(false, true, false), "WDA never installed");
    }

    /// One leader at a time; it is free again once the holder lets go
    #[test]
    fn one_instance_leads() {
        let p = std::env::temp_dir().join(format!("ictl-lead-{}", uuid::Uuid::new_v4()));
        let a = try_lead(&p).expect("free");
        assert!(try_lead(&p).is_none(), "held");
        drop(a);
        assert!(try_lead(&p).is_some());
        std::fs::remove_file(p).unwrap();
    }
}
