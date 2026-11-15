//! The WDA launched in the background (`ICTL_WDA_DETACHED`), once it isn't used. It doesn't end with the daemon (that
//! is the point: a restart or an update attaches to it again), so it is ended where it is no longer wanted: preparing
//! a phone for native control, starting WDA with the switch turned off, and `iphone-control service stop` /
//! `uninstall` ([`end_recorded`], with the daemon stopped). Each phone keeps its own (`Persisted::detached_wdas`):
//! switching to another device and back attaches to it again.

use std::time::{Duration, Instant};

use ictl_device::Tunnel;

use crate::Config;
use crate::core::Persisted;

/// What became of a WDA launched in the background earlier (by its pid)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoppedWda {
    /// It was running and is gone now
    Ended(u64),
    /// It wasn't running (anymore)
    NotRunning(u64),
    /// It may still be running: why
    Failed(u64, String),
}

impl StoppedWda {
    /// Whether it is gone, so its record can be forgotten
    pub fn gone(&self) -> bool {
        !matches!(self, Self::Failed(..))
    }
}

/// Whether process `pid` in the phone's process list is WDA
pub(crate) fn is_wda(procs: &[(u64, String)], pid: u64) -> bool {
    procs.iter().any(|(p, path)| *p == pid && path.contains("WebDriverAgentRunner"))
}

/// Ends the WDA launched in the background earlier (`pid`) if it still runs: SIGKILL through AppService (DVT's
/// `kill_app` leaves WDA running), then waits up to 2 s for it to leave the process list
pub(crate) async fn stop(t: &mut Tunnel, pid: u64) -> StoppedWda {
    match t.processes().await {
        Ok(p) if !is_wda(&p, pid) => return StoppedWda::NotRunning(pid),
        Ok(_) => {}
        Err(e) => return StoppedWda::Failed(pid, e.to_string()),
    }
    if let Err(e) = t.signal(pid, 9).await {
        return StoppedWda::Failed(pid, e.to_string());
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if t.processes().await.is_ok_and(|p| !is_wda(&p, pid)) {
            return StoppedWda::Ended(pid);
        }
        if Instant::now() >= deadline {
            return StoppedWda::Failed(
                pid,
                ictl_l10n::tr!("发了 SIGKILL，2 秒后还在运行", "still running 2 s after SIGKILL").to_string(),
            );
        }
    }
}

/// For `service stop` and `uninstall`, with the daemon stopped: ends every recorded background WDA on the phones that
/// are connected and forgets the ones that are gone. A phone that isn't connected, or one that couldn't be stopped,
/// stays recorded; the daemon deals with it the next time it prepares that phone. By UDID
pub async fn end_recorded(cfg: &Config) -> Vec<(String, StoppedWda)> {
    let path = cfg.state_file();
    let recorded = Persisted::load(&path).detached_all();
    if recorded.is_empty() {
        return Vec::new();
    }
    let connected = ictl_device::list_devices().await.unwrap_or_default();
    // an update or a re-sign holding the phones: leave them to the daemon
    let _lease = match crate::maintenance::shared() {
        Ok(l) => l,
        Err(e) => {
            let why =
                ictl_l10n::trf!("别的维护操作正占着手机：{e}", "another maintenance operation holds the phones: {e}");
            return recorded.into_iter().map(|(u, pid)| (u, StoppedWda::Failed(pid, why.clone()))).collect();
        }
    };
    let mut out = Vec::new();
    for (udid, pid) in recorded {
        let Some(usb) = connected.iter().find(|d| d.udid.eq_ignore_ascii_case(&udid)) else {
            out.push((udid, StoppedWda::Failed(pid, ictl_l10n::tr!("没接着", "not connected").to_string())));
            continue;
        };
        let s = match ictl_device::Device::from_usb(usb).open_tunnel().await {
            Ok(mut t) => {
                let s = stop(&mut t, pid).await;
                t.close().await;
                s
            }
            Err(e) => StoppedWda::Failed(pid, e.to_string()),
        };
        out.push((udid, s));
    }
    // read again: only what was dealt with here changes
    let mut p = Persisted::load(&path);
    let mut changed = false;
    for (udid, s) in &out {
        if let StoppedWda::Ended(pid) | StoppedWda::NotRunning(pid) = s {
            changed |= p.forget_detached(udid, Some(*pid));
        }
    }
    if changed {
        p.save(&path);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_wda_runner_with_that_pid_is_wda() {
        let procs = vec![
            (10, "/private/var/containers/Bundle/Application/X/WebDriverAgentRunner-Runner.app/WebDriverAgentRunner-Runner".into()),
            (11, "/Applications/MobileSafari.app/MobileSafari".into()),
        ];
        assert!(is_wda(&procs, 10));
        assert!(!is_wda(&procs, 11), "another app");
        assert!(!is_wda(&procs, 12), "gone");
    }
}
