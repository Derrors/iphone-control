//! The last resort when the daemon wedges, plus persistent abnormal-exit / watchdog self-awareness with anti-storm
//! alert throttling.
//!
//! ## Watchdog
//! Everything that finishes work under the device lock (a request, a full supervision round) goes through
//! [`crate::Daemon`]'s `publish`, which beats the heartbeat. A round that finds the lock busy skips without beating,
//! so a stuck lock holder shows up as silence. The check runs on its own thread, so a blocked runtime is caught too.
//! The service manager (systemd `Restart=on-failure`, launchd `KeepAlive`) starts a fresh daemon. The exit can't
//! close a HID session first (the stuck request holds the lock it is behind): the fresh daemon clears what that
//! leaves on the phone ([`crate::hidmark`]).
//!
//! ## Abnormal-exit self-awareness and anti-storm alerts
//! While running, the daemon records its live marker in `run-state.json` (independent of `state.json` and the device
//! lock so the watchdog thread can write it safely even when the device lock is wedged). On a clean shutdown
//! (`SIGTERM`/`SIGINT`), the marker is cleared after [`crate::Daemon::shutdown`] completes. Right before
//! `std::process::exit(1)`, the watchdog records its exit reason and silence duration in the same file.
//!
//! On the next startup (once the single-instance lock is held):
//! - If a `watchdog` record is present, the previous daemon was killed by the watchdog;
//! - Otherwise if a `running` marker is still present, the previous daemon exited unexpectedly (crash, OOM, `SIGKILL`,
//!   or power loss);
//! - Otherwise the previous shutdown was clean (or this is the first start), and nothing is reported.
//!
//! Every abnormal restart is logged to `daemon.log`. External notifications (Bark / Telegram) use a persisted
//! [`ALERT_COOLDOWN`] (30 min) so a crash loop cannot flood the user; suppressed restarts are counted and summarized
//! once cooldown expires (within [`RECENT_WINDOW`], 1 hour).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::eventlog::EventLog;
use crate::notify::{Event, RestartReason};

/// Longer than any legitimate hold of the device lock (starting WDA, the slowest thing, is a few minutes)
const STUCK_AFTER: Duration = Duration::from_secs(600);
const CHECK_EVERY: Duration = Duration::from_secs(30);
/// Minimum interval between external abnormal-restart alerts persisted across daemon restarts (anti-storm cooldown).
pub(crate) const ALERT_COOLDOWN: Duration = Duration::from_secs(30 * 60);
/// Rolling window (seconds) for counting recent abnormal restarts and keeping suppressed counts relevant.
const RECENT_WINDOW: u64 = 3600;
/// Cap on persisted recent-restart timestamps so the state file stays tiny even under extreme restart loops.
const MAX_RECENT_TIMESTAMPS: usize = 1024;
/// Small backward clock adjustment (NTP) kept inside cooldown; larger future skew resets stale timestamps.
const CLOCK_SKEW_TOLERANCE: u64 = 60;

#[derive(Debug, Clone)]
pub(crate) struct Heartbeat {
    start: Instant,
    /// Seconds after `start` of the last beat
    last: Arc<AtomicU64>,
}

impl Heartbeat {
    pub(crate) fn new() -> Self {
        Self { start: Instant::now(), last: Arc::new(AtomicU64::new(0)) }
    }

    pub(crate) fn beat(&self) {
        self.last.store(self.start.elapsed().as_secs(), Ordering::Relaxed);
    }

    fn silent_for(&self) -> Duration {
        Duration::from_secs(self.start.elapsed().as_secs().saturating_sub(self.last.load(Ordering::Relaxed)))
    }
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedRunState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    running: Option<RunningInstance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watchdog: Option<WatchdogExit>,
    #[serde(default)]
    alert: AlertThrottle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RunningInstance {
    pid: u32,
    started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WatchdogExit {
    pid: u32,
    silent_secs: u64,
    at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct AlertThrottle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_sent_at: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    suppressed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    suppressed_last_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recent_restarts: Vec<u64>,
}

/// Finding returned by [`RunState::on_startup`] when the previous daemon did not shut down cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AbnormalRestart {
    pub reason: RestartReason,
    pub previous_pid: u32,
    /// Abnormal restarts recorded within the last hour (including this one).
    pub recent: u32,
    /// `Some(total_suppressed)` when an external notification was withheld due to active cooldown.
    pub suppressed_by_cooldown: Option<u32>,
    /// Notification event to dispatch asynchronously (`None` if no channels are configured or suppressed by cooldown).
    pub notify: Option<Event>,
}

/// Coordinates persistent daemon lifecycle markers between startup, clean shutdown, and the watchdog thread.
pub(crate) struct RunState {
    path: PathBuf,
    io: Mutex<()>,
    clean_shutdown: AtomicBool,
}

impl RunState {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, io: Mutex::new(()), clean_shutdown: AtomicBool::new(false) }
    }

    pub(crate) fn is_clean_shutdown(&self) -> bool {
        self.clean_shutdown.load(Ordering::SeqCst)
    }

    /// Inspects the previous run state, updates throttle/live markers on disk, and returns an [`AbnormalRestart`] if
    /// the previous daemon died unexpectedly or was killed by the watchdog.
    pub(crate) fn on_startup(&self, notify_configured: bool) -> Option<AbnormalRestart> {
        self.on_startup_at(now_secs(), std::process::id(), notify_configured)
    }

    pub(crate) fn on_startup_at(&self, now: u64, pid: u32, notify_configured: bool) -> Option<AbnormalRestart> {
        let _guard = self.io.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = load_state(&self.path);

        let previous = if let Some(w) = state.watchdog.take() {
            Some((RestartReason::Watchdog { silent_secs: w.silent_secs }, w.pid))
        } else {
            state.running.as_ref().map(|r| (RestartReason::Unexpected, r.pid))
        };

        // Prune expired rolling timestamps on every startup so steady daemons leave the file clean.
        state.alert.recent_restarts.retain(|&t| within_window(t, now, RECENT_WINDOW));
        if state.alert.suppressed > 0
            && state.alert.suppressed_last_at.is_none_or(|t| !within_window(t, now, RECENT_WINDOW))
        {
            state.alert.suppressed = 0;
            state.alert.suppressed_last_at = None;
        }
        if state.alert.last_sent_at.is_some_and(|t| !in_cooldown(t, now, ALERT_COOLDOWN.as_secs())) {
            state.alert.last_sent_at = None;
        }

        let report = if let Some((reason, previous_pid)) = previous {
            state.alert.recent_restarts.push(now);
            if state.alert.recent_restarts.len() > MAX_RECENT_TIMESTAMPS {
                let extra = state.alert.recent_restarts.len() - MAX_RECENT_TIMESTAMPS;
                state.alert.recent_restarts.drain(..extra);
            }
            let recent = state.alert.recent_restarts.len() as u32;

            let (notify, suppressed_by_cooldown) = if !notify_configured {
                (None, None)
            } else if state.alert.last_sent_at.is_some_and(|t| in_cooldown(t, now, ALERT_COOLDOWN.as_secs())) {
                state.alert.suppressed = state.alert.suppressed.saturating_add(1);
                state.alert.suppressed_last_at = Some(now);
                (None, Some(state.alert.suppressed))
            } else {
                let carried = state.alert.suppressed;
                state.alert.last_sent_at = Some(now);
                state.alert.suppressed = 0;
                state.alert.suppressed_last_at = None;
                (Some(Event::DaemonRestarted { reason, recent, suppressed: carried }), None)
            };

            Some(AbnormalRestart { reason, previous_pid, recent, suppressed_by_cooldown, notify })
        } else {
            None
        };

        state.running = Some(RunningInstance { pid, started_at: now });
        state.watchdog = None;
        save_state(&self.path, &state);
        report
    }

    /// Clears the live running marker after [`crate::Daemon::shutdown`] finishes so the next startup knows this exit
    /// was intentional, while preserving persisted anti-storm alert state.
    pub(crate) fn mark_clean_shutdown(&self) {
        let _guard = self.io.lock().unwrap_or_else(|p| p.into_inner());
        self.clean_shutdown.store(true, Ordering::SeqCst);
        let mut state = load_state(&self.path);
        state.running = None;
        state.watchdog = None;
        save_state(&self.path, &state);
    }

    /// Persists a watchdog exit marker right before `std::process::exit(1)`. Returns `false` if a clean shutdown has
    /// already completed, telling the watchdog thread to stand down instead of killing a cleanly exiting process.
    fn mark_watchdog_exit(&self, silent_secs: u64) -> bool {
        self.mark_watchdog_exit_at(std::process::id(), silent_secs, now_secs())
    }

    pub(crate) fn mark_watchdog_exit_at(&self, pid: u32, silent_secs: u64, now: u64) -> bool {
        let _guard = self.io.lock().unwrap_or_else(|p| p.into_inner());
        if self.clean_shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let mut state = load_state(&self.path);
        state.watchdog = Some(WatchdogExit { pid, silent_secs, at: now });
        save_state(&self.path, &state);
        true
    }
}

fn within_window(ts: u64, now: u64, window_secs: u64) -> bool {
    ts <= now.saturating_add(CLOCK_SKEW_TOLERANCE) && now.saturating_sub(ts) <= window_secs
}

fn in_cooldown(last_sent_at: u64, now: u64, cooldown_secs: u64) -> bool {
    if last_sent_at > now {
        // Small backward NTP skew still respects cooldown; a wildly future timestamp does not wedge alerts forever.
        last_sent_at - now <= CLOCK_SKEW_TOLERANCE
    } else {
        now - last_sent_at < cooldown_secs
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn load_state(path: &Path) -> PersistedRunState {
    let Ok(bytes) = std::fs::read(path) else { return PersistedRunState::default() };
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        tracing::warn!(
            "{}",
            ictl_l10n::log_trf!(
                "run-state.json 解析失败，按空运行状态继续：{e}",
                "Parsing run-state.json failed, continuing with an empty run state: {e}"
            )
        );
        PersistedRunState::default()
    })
}

fn save_state(path: &Path, state: &PersistedRunState) {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    let r = serde_json::to_vec_pretty(state).map_err(std::io::Error::other).and_then(|data| {
        std::fs::File::create(&tmp)
            .and_then(|mut f| {
                f.write_all(&data)?;
                f.sync_all()
            })
            .and_then(|_| std::fs::rename(&tmp, path))
    });
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("{}", ictl_l10n::log_trf!("保存 run-state.json 失败：{e}", "Saving run-state.json failed: {e}"));
    }
}

pub(crate) fn spawn(heartbeat: Heartbeat, log: EventLog, run_state: Arc<RunState>) {
    let watch = move || {
        loop {
            std::thread::sleep(CHECK_EVERY);
            if run_state.is_clean_shutdown() {
                return;
            }
            let silent = heartbeat.silent_for();
            if silent >= STUCK_AFTER {
                if !run_state.mark_watchdog_exit(silent.as_secs()) {
                    return;
                }
                log.log(&ictl_l10n::log_trf!(
                    "守护进程已 {} 秒没有进展，判定卡死，退出后由服务管理器重启",
                    "No progress for {} s, the daemon is stuck; exiting so the service manager restarts it",
                    silent.as_secs()
                ));
                std::process::exit(1);
            }
        }
    };
    if let Err(e) = std::thread::Builder::new().name("watchdog".into()).spawn(watch) {
        tracing::warn!("{}", ictl_l10n::log_trf!("启动卡死检测失败：{e}", "Starting the watchdog failed: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ictl-runstate-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("run-state.json");
        (dir, file)
    }

    #[test]
    fn a_beat_ends_the_silence() {
        let Some(start) = Instant::now().checked_sub(Duration::from_secs(700)) else { return };
        let hb = Heartbeat { start, last: Arc::new(AtomicU64::new(0)) };
        assert!(hb.silent_for() >= STUCK_AFTER);
        hb.clone().beat();
        assert!(hb.silent_for() < Duration::from_secs(2));
    }

    #[test]
    fn first_start_and_clean_restart_never_alert() {
        let (dir, file) = temp_path("clean");
        let rs = RunState::new(file.clone());
        assert!(rs.on_startup_at(10_000, 101, true).is_none(), "first start is clean");
        rs.mark_clean_shutdown();

        let rs2 = RunState::new(file);
        assert!(rs2.on_startup_at(10_005, 102, true).is_none(), "clean shutdown produces no alert");
        rs2.mark_clean_shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unexpected_exit_is_detected_and_reported_once_per_cooldown() {
        let (dir, file) = temp_path("unexpected");
        let base = 100_000u64;
        // Start daemon 1, then vanish without clean shutdown (simulated SIGKILL / crash).
        let rs1 = RunState::new(file.clone());
        assert!(rs1.on_startup_at(base, 1001, true).is_none());

        // Daemon 2 starts: detects unexpected exit of pid 1001 and dispatches alert.
        let rs2 = RunState::new(file.clone());
        let r = rs2.on_startup_at(base + 10, 1002, true).expect("abnormal restart detected");
        assert_eq!(r.reason, RestartReason::Unexpected);
        assert_eq!(r.previous_pid, 1001);
        assert_eq!(r.recent, 1);
        assert_eq!(r.suppressed_by_cooldown, None);
        assert_eq!(
            r.notify,
            Some(Event::DaemonRestarted { reason: RestartReason::Unexpected, recent: 1, suppressed: 0 })
        );

        // Crash loop: 9 more crashes within cooldown must all be suppressed (anti-storm).
        for i in 1..=9u64 {
            let rs = RunState::new(file.clone());
            let r = rs.on_startup_at(base + 10 + i * 5, 1002 + i as u32, true).expect("crash detected");
            assert_eq!(r.reason, RestartReason::Unexpected);
            assert_eq!(r.recent, 1 + i as u32);
            assert_eq!(r.suppressed_by_cooldown, Some(i as u32));
            assert_eq!(r.notify, None, "must suppress crash #{i} during cooldown");
        }

        // After 30-minute cooldown expires, the next crash sends one summary alert with suppressed=9.
        let after_cd = base + 10 + ALERT_COOLDOWN.as_secs();
        let rs_after = RunState::new(file.clone());
        let r = rs_after.on_startup_at(after_cd, 2000, true).expect("crash after cooldown");
        assert_eq!(r.suppressed_by_cooldown, None);
        assert_eq!(r.recent, 11);
        assert_eq!(
            r.notify,
            Some(Event::DaemonRestarted { reason: RestartReason::Unexpected, recent: 11, suppressed: 9 })
        );

        // Immediate clean restart after that alert neither alerts nor wipes the fresh cooldown.
        rs_after.mark_clean_shutdown();
        let rs_clean = RunState::new(file.clone());
        assert!(rs_clean.on_startup_at(after_cd + 5, 2001, true).is_none());
        // Crashing 1 minute later is still inside the new cooldown window.
        let rs_again = RunState::new(file);
        let r = rs_again.on_startup_at(after_cd + 65, 2002, true).expect("crash after clean restart");
        assert_eq!(r.notify, None);
        assert_eq!(r.suppressed_by_cooldown, Some(1));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn watchdog_exit_is_distinguished_from_unexpected_exit_and_cleared_after_consumption() {
        let (dir, file) = temp_path("watchdog");
        let base = 200_000u64;
        let rs1 = RunState::new(file.clone());
        assert!(rs1.on_startup_at(base, 3001, true).is_none());
        assert!(rs1.mark_watchdog_exit_at(3001, 615, base + 615));

        let rs2 = RunState::new(file.clone());
        let r = rs2.on_startup_at(base + 617, 3002, true).expect("watchdog restart detected");
        assert_eq!(r.reason, RestartReason::Watchdog { silent_secs: 615 });
        assert_eq!(r.previous_pid, 3001);
        assert_eq!(
            r.notify,
            Some(Event::DaemonRestarted {
                reason: RestartReason::Watchdog { silent_secs: 615 },
                recent: 1,
                suppressed: 0,
            })
        );

        // Next clean restart does not re-report the consumed watchdog marker.
        rs2.mark_clean_shutdown();
        let rs3 = RunState::new(file);
        assert!(rs3.on_startup_at(base + 625, 3003, true).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_suppressed_count_resets_after_long_stable_period() {
        let (dir, file) = temp_path("stale-wave");
        let base = 300_000u64;
        let rs = RunState::new(file.clone());
        rs.on_startup_at(base, 4001, true);
        // Crash 1 alerts, crashes 2..4 suppressed.
        assert!(RunState::new(file.clone()).on_startup_at(base + 10, 4002, true).unwrap().notify.is_some());
        for i in 1..=3u64 {
            assert!(
                RunState::new(file.clone())
                    .on_startup_at(base + 10 + i * 5, 4002 + i as u32, true)
                    .unwrap()
                    .notify
                    .is_none()
            );
        }
        // Stable for 3 days, then one crash: old suppressed count must not bleed into the new incident.
        let three_days_later = base + 3 * 86_400;
        let r = RunState::new(file).on_startup_at(three_days_later, 5000, true).unwrap();
        assert_eq!(
            r.notify,
            Some(Event::DaemonRestarted { reason: RestartReason::Unexpected, recent: 1, suppressed: 0 })
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unconfigured_notify_still_reports_for_logs_without_consuming_cooldown() {
        let (dir, file) = temp_path("no-notify");
        let base = 400_000u64;
        RunState::new(file.clone()).on_startup_at(base, 6001, false);
        let r1 = RunState::new(file.clone()).on_startup_at(base + 10, 6002, false).unwrap();
        assert_eq!(r1.notify, None);
        assert_eq!(r1.suppressed_by_cooldown, None);
        assert_eq!(r1.recent, 1);

        // Once the user configures a notification channel, the next abnormal restart alerts right away.
        let r2 = RunState::new(file).on_startup_at(base + 20, 6003, true).unwrap();
        assert_eq!(
            r2.notify,
            Some(Event::DaemonRestarted { reason: RestartReason::Unexpected, recent: 2, suppressed: 0 })
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn backward_clock_skew_respects_small_drift_but_does_not_wedge_on_future_timestamp() {
        let cd = ALERT_COOLDOWN.as_secs();
        assert!(in_cooldown(1000, 1000, cd));
        assert!(in_cooldown(1000, 990, cd), "10s NTP skew stays in cooldown");
        assert!(!in_cooldown(1000, 900, cd), "100s future timestamp resets instead of wedging");
        assert!(!in_cooldown(1000, 1000 + cd, cd));
    }

    #[test]
    fn clean_shutdown_prevents_late_watchdog_mark_and_corrupt_file_recovers() {
        let (dir, file) = temp_path("race-corrupt");
        let rs = RunState::new(file.clone());
        rs.on_startup_at(500_000, 7001, true);
        rs.mark_clean_shutdown();
        assert!(!rs.mark_watchdog_exit_at(7001, 600, 500_600), "clean shutdown blocks late watchdog mark");
        assert!(RunState::new(file.clone()).on_startup_at(500_610, 7002, true).is_none());

        // Corrupt JSON on disk never crashes or falsely alerts on startup.
        std::fs::write(&file, b"{corrupt json").unwrap();
        assert!(RunState::new(file).on_startup_at(500_620, 7003, true).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
