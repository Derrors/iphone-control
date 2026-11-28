//! The WDA the daemon launches in the background (it holds no XCTest session), once it isn't used. It doesn't end
//! with the daemon (that is the point: a restart or an update attaches to it again), so it is ended where it is no
//! longer wanted: preparing a phone for native control, and `iphone-control service stop` / `uninstall`
//! ([`end_recorded`], with the daemon stopped). Each phone keeps its own record, shared by every instance
//! (`crate::adopt`; the default instance also keeps it in `Persisted::detached_wdas`, where 0.3.2 looks): switching
//! to another device and back, or another instance taking the device over, attaches to it again.

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

/// What a stop must find to end process `pid` ([`stop`])
#[derive(Debug, Clone, Copy)]
pub(crate) enum Expect<'a> {
    /// A shared record's process (`crate::adopt`): its creation identity, the executable and the start the phone
    /// reports, must be the one recorded. A record without them proves nothing: the process is left alone (the next
    /// launch ends it)
    Created { executable: Option<&'a str>, started: Option<&'a str> },
    /// Only in an older version's `state.json`, for a device without any shared record: a WDA with that pid
    Legacy,
}

/// The executable process `pid` runs, as the phone lists it
pub(crate) fn executable_of(procs: &[(u64, String)], pid: u64) -> Option<String> {
    procs.iter().find(|(p, _)| *p == pid).map(|(_, path)| path.clone())
}

/// Ends the WDA launched in the background earlier (`pid`, as `expect` says it must be) if it still runs: SIGKILL
/// through AppService (DVT's `kill_app` leaves WDA running), then waits up to 2 s for it to leave the process list.
/// Another process with that pid, even of the same app, is left alone
pub(crate) async fn stop(t: &mut Tunnel, pid: u64, expect: Expect<'_>) -> StoppedWda {
    let procs = match t.processes().await {
        Ok(p) => p,
        Err(e) => return StoppedWda::Failed(pid, e.to_string()),
    };
    if !is_wda(&procs, pid) {
        return StoppedWda::NotRunning(pid);
    }
    if let Expect::Created { executable, started } = expect {
        let (Some(executable), Some(started)) = (executable, started) else {
            let why = ictl_l10n::tr!(
                "记录里没有这个进程的创建身份，不结束它（下次启动 WDA 时会替换）",
                "the record has no creation identity for this process, so it isn't ended (the next launch of WDA replaces it)"
            );
            return StoppedWda::Failed(pid, why.to_string());
        };
        let now = match t.process_starts().await {
            Ok(s) => s.get(&pid).cloned(),
            Err(e) => return StoppedWda::Failed(pid, e.to_string()),
        };
        if executable_of(&procs, pid).as_deref() != Some(executable) || now.as_deref() != Some(started) {
            // that pid is another process now (the phone restarted, the app launched again)
            return StoppedWda::NotRunning(pid);
        }
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

/// A background WDA [`end_recorded`] is to end
struct Recorded {
    udid: String,
    pid: u64,
    exe: Option<String>,
    started: Option<String>,
    /// The shared record's generation and bundle; `None`: only in state.json, by an older version
    shared: Option<(u64, String)>,
}

/// For `service stop` and `uninstall`, with the daemon stopped: ends the background WDAs this instance recorded
/// (docs/dev/multi-instance-plan.md 4.5, 4.6): its own shared records (by its id, never by a name another instance
/// may have had) and the default instance's older ones in state.json. Only on the devices whose lease it can take:
/// - stopped or gone: the record goes;
/// - still running: the record says `cleanup_pending`, for whoever controls the device next;
/// - another instance holds the device: its record isn't touched, an intent asks that instance to end it.
///
/// A phone that isn't connected stays recorded; the daemon deals with it the next time it prepares that phone. By UDID
pub async fn end_recorded(cfg: &Config) -> Vec<(String, StoppedWda)> {
    use crate::adopt::{self, Intent};
    use crate::lease::{Claim, Holder};
    let path = cfg.state_file();
    let dir = cfg.adopt_dir();
    let mut recorded: Vec<Recorded> = adopt::all(&dir)
        .into_iter()
        .filter(|(_, r)| r.owner_id == cfg.instance_id)
        .map(|(udid, r)| Recorded {
            udid,
            pid: r.pid,
            exe: r.executable,
            started: r.started,
            shared: Some((r.generation, r.bundle)),
        })
        .collect();
    // an older version's record (state.json) only for a device with no shared record at all, readable or not: one
    // there, of any instance, says what runs on the device now (another instance may have taken its WDA over)
    let any = adopt::devices_recorded(&dir);
    for (udid, pid) in Persisted::load(&path).detached_all() {
        if !any.iter().any(|u| u.eq_ignore_ascii_case(&udid)) {
            recorded.push(Recorded { udid, pid, exe: None, started: None, shared: None });
        }
    }
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
            return recorded.into_iter().map(|r| (r.udid, StoppedWda::Failed(r.pid, why.clone()))).collect();
        }
    };
    let mut out = Vec::new();
    for Recorded { udid, pid, exe, started, shared } in recorded {
        let Some(usb) = connected.iter().find(|d| d.udid.eq_ignore_ascii_case(&udid)) else {
            out.push((udid, StoppedWda::Failed(pid, ictl_l10n::tr!("没接着", "not connected").to_string())));
            continue;
        };
        let _device = match crate::lease::try_claim(&cfg.devices_dir(), &udid, Holder::instance(cfg.instance_name())) {
            Ok(Claim::Got(l)) => l,
            Ok(Claim::Held(h)) => {
                let by = h.and_then(|h| h.instance_name().map(str::to_owned)).unwrap_or_else(|| "?".into());
                if let Some((generation, bundle)) = &shared {
                    let i = Intent {
                        owner_id: cfg.instance_id.clone(),
                        generation: *generation,
                        pid,
                        bundle: bundle.clone(),
                        record_owner: cfg.instance_id.clone(),
                        started: started.clone(),
                    };
                    if let Err(e) = adopt::add_intent(&dir, &udid, &i) {
                        tracing::warn!("asking for the background WDA of {udid} to be ended failed: {e}");
                    }
                }
                let why = ictl_l10n::trf!(
                    "设备由实例 {by} 控制：交给它结束",
                    "the device is controlled by the instance {by}: left to it to end"
                );
                out.push((udid, StoppedWda::Failed(pid, why)));
                continue;
            }
            Err(e) => {
                out.push((udid, StoppedWda::Failed(pid, e.to_string())));
                continue;
            }
        };
        // the lease held: the shared record read again, the process still ours to end only if nothing changed since
        // it was listed (no other instance took it over, wrote it again, or recorded one at all)
        // one that can't be read is left alone: not taken for none (which would let the older pid end something)
        let now = adopt::read(&dir, &udid);
        let still_ours = match (&shared, &now) {
            (Some((generation, _)), Ok(Some(r))) => {
                r.owner_id == cfg.instance_id && r.generation == *generation && r.pid == pid && r.started == started
            }
            (None, Ok(None)) => !adopt::devices_recorded(&dir).iter().any(|u| u.eq_ignore_ascii_case(&udid)),
            _ => false,
        };
        if !still_ours {
            let why = ictl_l10n::tr!(
                "这台设备上的后台 WDA 记录已经换了（别的实例接手了）：不动它",
                "the background WDA record of this device changed (another instance took over): left alone"
            );
            out.push((udid, StoppedWda::Failed(pid, why.to_string())));
            continue;
        }
        let s = match ictl_device::Device::from_usb(usb).open_tunnel().await {
            Ok(mut t) => {
                let expect = match &shared {
                    Some(_) => Expect::Created { executable: exe.as_deref(), started: started.as_deref() },
                    None => Expect::Legacy,
                };
                let s = stop(&mut t, pid, expect).await;
                t.close().await;
                s
            }
            Err(e) => StoppedWda::Failed(pid, e.to_string()),
        };
        // the lease is held: the record is this process's to change
        if shared.is_some() {
            let r = match &s {
                StoppedWda::Ended(_) | StoppedWda::NotRunning(_) => adopt::remove(&dir, &udid),
                StoppedWda::Failed(..) => match adopt::read(&dir, &udid) {
                    Ok(Some(r)) if r.pid == pid => {
                        adopt::write(&dir, &udid, adopt::Record { cleanup_pending: true, ..r }).map(drop)
                    }
                    other => other.map(drop),
                },
            };
            if let Err(e) = r {
                tracing::warn!("updating the background WDA record of {udid} failed: {e}");
            }
        }
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
