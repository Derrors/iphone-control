//! The record of the WDA launched in the background on a device, shared by every instance (docs/dev/multi-instance-
//! plan.md 4.6): `<data root>/wda-adopt/<UDID>.json`.
//!
//! A background WDA outlives the daemon that launched it, so the next one to control the device (the same instance
//! restarted or updated, or another instance taking the device over) can attach to it instead of launching it again
//! ([`crate::detached`]). The record says which process it is and whose it is:
//! - `pid`, `bundle`, `team`, and the process's `executable` as the phone lists it: the proof it is still the
//!   process that was launched (a pid alone may be another process's after the phone restarted). `loopback`: launched
//!   listening on the phone's loopback only, as this version does; any other is never attached to;
//! - `owner_id`: the id of the instance that wrote it (an instance name can be used again, an id isn't);
//! - `generation`: moved on with every write;
//! - `cleanup_pending`: an explicit stop couldn't end it: the next holder ends it, it isn't attached to.
//!
//! It is only written by the holder of the device's lease. A process without the lease (a `service stop` while
//! another instance has the device) only adds an intent (`<UDID>.intent-<id>-<n>.json`) naming the record's
//! generation and process as it saw them; the next holder ends the process only if the record still says the same,
//! and drops the intent either way ([`take_intents`]).

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub pid: u64,
    pub bundle: String,
    #[serde(default)]
    pub team: Option<String>,
    /// The process's executable as the phone listed it when it was launched or attached to
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub loopback: bool,
    pub owner_id: String,
    pub generation: u64,
    #[serde(default)]
    pub cleanup_pending: bool,
}

impl Record {
    /// May be attached to: launched as this version does, and not to be ended
    pub fn adoptable(&self) -> bool {
        self.loopback && !self.cleanup_pending
    }
}

/// An instance that couldn't touch the record asks the holder to end the process
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub owner_id: String,
    /// The record's generation it saw
    pub generation: u64,
    pub pid: u64,
    pub bundle: String,
}

fn valid(udid: &str) -> io::Result<String> {
    if !udid.is_empty() && udid.len() <= 64 && udid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        Ok(udid.to_ascii_uppercase())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a device UDID: {udid:?}")))
    }
}

fn record_path(dir: &Path, udid: &str) -> io::Result<PathBuf> {
    Ok(dir.join(format!("{}.json", valid(udid)?)))
}

/// `udid`'s record; `None` when there is none. One that can't be read counts as none to attach to, but is said so
pub fn read(dir: &Path, udid: &str) -> io::Result<Option<Record>> {
    match std::fs::read(record_path(dir, udid)?) {
        Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Writes `udid`'s record whole, with the generation after the one on disk. Only with the device's lease held
pub fn write(dir: &Path, udid: &str, mut r: Record) -> io::Result<Record> {
    let name = format!("{}.json", valid(udid)?);
    crate::lease::ensure_dir(dir)?;
    r.generation = read(dir, udid).ok().flatten().map_or(0, |o| o.generation) + 1;
    let data = serde_json::to_vec_pretty(&r).map_err(io::Error::other)?;
    ictl_unlock::durable::write_atomic(dir, &name, &data, 0o600)?;
    Ok(r)
}

/// Removes `udid`'s record. Only with the device's lease held
pub fn remove(dir: &Path, udid: &str) -> io::Result<()> {
    match std::fs::remove_file(record_path(dir, udid)?) {
        Ok(()) => std::fs::File::open(dir)?.sync_all(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Every record in `dir`, by UDID (upper case); ones that can't be read are left out
pub fn all(dir: &Path) -> Vec<(String, Record)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, Record)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.strip_suffix(".json")?.to_owned();
            valid(&name).ok().filter(|u| *u == name)?;
            let r = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
            Some((name, r))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Adds an intent to end `udid`'s process, for whoever holds the lease next
pub fn add_intent(dir: &Path, udid: &str, i: &Intent) -> io::Result<()> {
    let u = valid(udid)?;
    crate::lease::ensure_dir(dir)?;
    let id: String = i.owner_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
    let name = format!("{u}.intent-{id}-{}.json", uuid::Uuid::new_v4().simple());
    let data = serde_json::to_vec_pretty(i).map_err(io::Error::other)?;
    ictl_unlock::durable::write_atomic(dir, &name, &data, 0o600)
}

/// The intents for `udid`, removed: whether one of them still matches `record` (its generation and process), so the
/// process is to be ended. One that doesn't match was overtaken (another instance wrote the record since) and is only
/// dropped. Only with the device's lease held
pub fn take_intents(dir: &Path, udid: &str, record: Option<&Record>) -> bool {
    let Ok(u) = valid(udid) else { return false };
    let prefix = format!("{u}.intent-");
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let mut end = false;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(".json") {
            continue;
        }
        let intent: Option<Intent> = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
        if let (Some(i), Some(r)) = (intent, record) {
            end |= i.generation == r.generation && i.pid == r.pid && i.bundle == r.bundle;
        }
        let _ = std::fs::remove_file(e.path());
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pid: u64) -> Record {
        Record {
            pid,
            bundle: "com.x.WebDriverAgentRunner.xctrunner".into(),
            team: Some("T".into()),
            executable: Some("/private/var/containers/Bundle/Application/A/WebDriverAgentRunner-Runner.app/R".into()),
            loopback: true,
            owner_id: "default".into(),
            generation: 0,
            cleanup_pending: false,
        }
    }

    /// Each write moves the generation on; one to be cleaned up, or launched otherwise than this version does, isn't
    /// attached to
    #[test]
    fn a_record_moves_on_and_says_whether_to_attach() {
        let dir = std::env::temp_dir().join(format!("ictl-adopt-{}", uuid::Uuid::new_v4()));
        assert_eq!(read(&dir, "aa").unwrap(), None);
        let a = write(&dir, "aa", rec(10)).unwrap();
        assert_eq!(a.generation, 1);
        let b = write(&dir, "AA", rec(11)).unwrap();
        assert_eq!(b.generation, 2, "the same device in either case");
        assert_eq!(read(&dir, "aa").unwrap(), Some(b.clone()));
        assert!(!Record { cleanup_pending: true, ..b.clone() }.adoptable());
        assert!(!Record { loopback: false, ..b }.adoptable());
        remove(&dir, "aa").unwrap();
        assert_eq!(read(&dir, "aa").unwrap(), None);
        assert!(read(&dir, "../x").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An intent is acted on only while the record is the one it saw; overtaken (written again since), it is dropped
    /// without ending anything; either way it is gone afterwards
    #[test]
    fn an_intent_overtaken_by_a_newer_record_is_dropped() {
        let dir = std::env::temp_dir().join(format!("ictl-adopt-{}", uuid::Uuid::new_v4()));
        let seen = write(&dir, "aa", rec(10)).unwrap();
        let intent =
            Intent { owner_id: "default".into(), generation: seen.generation, pid: 10, bundle: seen.bundle.clone() };
        add_intent(&dir, "aa", &intent).unwrap();
        assert!(take_intents(&dir, "AA", Some(&seen)), "still the same: ended");
        assert!(!take_intents(&dir, "aa", Some(&seen)), "taken once");
        add_intent(&dir, "aa", &intent).unwrap();
        let newer = write(&dir, "aa", rec(12)).unwrap();
        assert!(!take_intents(&dir, "aa", Some(&newer)), "another instance took over meanwhile");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "only the record is left");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
