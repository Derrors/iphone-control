//! The last resort when the daemon wedges: a device call that never returns while holding the device lock would
//! leave every later request queuing until it goes stale, and the process looks healthy from outside. Timeouts around
//! the device calls are the first line of defence; this catches what they miss.
//!
//! Everything that finishes work under the device lock (a request, a full supervision round) goes through
//! [`crate::Daemon`]'s `publish`, which beats the heartbeat. A round that finds the lock busy skips without beating,
//! so a stuck lock holder shows up as silence. The check runs on its own thread, so a blocked runtime is caught too.
//! The service manager (systemd `Restart=on-failure`, Docker `restart: unless-stopped`) starts a fresh daemon.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::eventlog::EventLog;

/// Longer than any legitimate hold of the device lock (starting WDA, the slowest thing, is a few minutes)
const STUCK_AFTER: Duration = Duration::from_secs(600);
const CHECK_EVERY: Duration = Duration::from_secs(30);

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

pub(crate) fn spawn(heartbeat: Heartbeat, log: EventLog) {
    let watch = move || {
        loop {
            std::thread::sleep(CHECK_EVERY);
            let silent = heartbeat.silent_for();
            if silent >= STUCK_AFTER {
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

    #[test]
    fn a_beat_ends_the_silence() {
        let Some(start) = Instant::now().checked_sub(Duration::from_secs(700)) else { return };
        let hb = Heartbeat { start, last: Arc::new(AtomicU64::new(0)) };
        assert!(hb.silent_for() >= STUCK_AFTER);
        hb.clone().beat();
        assert!(hb.silent_for() < Duration::from_secs(2));
    }
}
