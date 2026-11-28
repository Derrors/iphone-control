//! Where auto-unlock is recorded as disabled after a failure: one record per device, so a wrong passcode on one
//! device leaves the others alone (docs/dev/multi-instance-plan.md 4.3).
//!
//! Every change goes through [`DisableStore`] under one file lock (`.lock` in [`DisableStore::dir`]), and is made so
//! that a crash or a power cut at any point leaves auto-unlock disabled for more devices, never fewer: an interrupted
//! write or removal must not let a wrong passcode be entered a second time (#25).
//!
//! The files, all in [`DisableStore::dir`] except the shadow:
//! - `<UDID>`: this device is disabled. The text says when and why.
//! - `LEGACY`: a record from a version that kept one flag for all devices. Nobody knows which device failed, so every
//!   device counts as disabled while it exists, except one released from it by `unlock-reset`
//!   (`<UDID>.legacy-released`).
//! - The shadow, `logs/UNLOCK-DISABLED`: the single flag older versions read. It exists whenever any record above
//!   does, so a downgraded daemon keeps every device disabled rather than entering a passcode again. This version
//!   writes `ictl-shadow v2 <UDID>` on its first line; a shadow without that line was written by an older version.
//!
//! Each new file is written to a temporary name, synced and renamed, and the directory is synced after every rename
//! and removal: syncing the file alone doesn't keep its name across a power cut.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

const LEGACY: &str = "LEGACY";
const RELEASED: &str = ".legacy-released";
const LOCK: &str = ".lock";
const SHADOW_MARK: &str = "ictl-shadow v2";

/// What [`DisableStore::migrate`] found and did at start-up
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Migration {
    /// Nothing to do
    None,
    /// An older version's flag: every device is disabled until reset one by one
    Legacy,
    /// A shadow left alone by a cut-short write or removal: the device it names stays disabled
    Orphan(Option<String>),
    /// Records without their shadow (removed by hand, or by an older version's `unlock-reset`): written again
    Shadow,
}

/// What [`DisableStore::reset`] cleared
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reset {
    /// The device wasn't disabled
    WasEnabled,
    /// Its own record was removed and, if it was disabled by an older version's flag, it was released from that
    Cleared,
}

/// The records of one installation. Cheap to clone
#[derive(Debug, Clone)]
pub struct DisableStore {
    dir: PathBuf,
    shadow: PathBuf,
}

/// One device's record in a [`DisableStore`]
#[derive(Debug, Clone)]
pub struct DeviceFlag {
    store: DisableStore,
    name: String,
}

impl DisableStore {
    /// `dir` holds the records, `shadow` is the older versions' single flag
    pub fn new(dir: impl Into<PathBuf>, shadow: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), shadow: shadow.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The record of the device `udid`
    pub fn device(&self, udid: &str) -> DeviceFlag {
        DeviceFlag { store: self.clone(), name: file_name(udid) }
    }

    /// Whether an older version's flag still disables devices that weren't released from it
    pub fn legacy(&self) -> bool {
        present(&self.dir.join(LEGACY))
    }

    /// Brings what an older version or an interrupted change left into line. Run once at start-up, before any
    /// device is unlocked
    pub fn migrate(&self) -> io::Result<Migration> {
        let _lock = self.lock()?;
        let records = self.records()?;
        let legacy = self.dir.join(LEGACY).try_exists()?;
        let shadow = match std::fs::read_to_string(&self.shadow) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            // unreadable but there: an older version's or not, it disables; treat it as theirs
            Err(_) => Some(String::new()),
        };
        let Some(text) = shadow else {
            if records.is_empty() && !legacy {
                return Ok(Migration::None);
            }
            self.write_shadow(records.first().map(String::as_str), "restored: records without the shadow")?;
            return Ok(Migration::Shadow);
        };
        let first = text.lines().next().unwrap_or_default();
        let Some(rest) = first.strip_prefix(SHADOW_MARK) else {
            // an older version's flag (never migrated, or written again after a downgrade): which device failed is
            // unknown, so all of them are disabled again, and earlier releases from a previous LEGACY don't count
            write_atomic(&self.dir, LEGACY, &format!("{} {}\n", now(), text.trim()))?;
            for r in self.released()? {
                remove(&self.dir, &r)?;
            }
            self.write_shadow(None, "an older version's flag, now LEGACY")?;
            return Ok(Migration::Legacy);
        };
        if !records.is_empty() || legacy {
            return Ok(Migration::None);
        }
        // a shadow with nothing behind it: cut short between its two steps. Which one isn't known, so the device it
        // names stays disabled (or all of them, if it names none)
        let udid = rest.trim();
        if udid.is_empty() {
            write_atomic(&self.dir, LEGACY, &format!("{} orphaned shadow without a device\n", now()))?;
            Ok(Migration::Orphan(None))
        } else {
            let name = file_name(udid);
            write_atomic(&self.dir, &name, &format!("{} orphaned shadow: an interrupted change\n", now()))?;
            Ok(Migration::Orphan(Some(name)))
        }
    }

    /// Re-enables auto-unlock for `udid`. The caller makes sure nothing is entering its passcode now
    pub fn reset(&self, udid: &str) -> io::Result<Reset> {
        let flag = self.device(udid);
        let was = flag.is_disabled();
        let _lock = self.lock()?;
        remove(&self.dir, &flag.name)?;
        if self.dir.join(LEGACY).try_exists()? {
            write_atomic(&self.dir, &format!("{}{RELEASED}", flag.name), &format!("{}\n", now()))?;
        }
        self.drop_shadow_if_unused()?;
        Ok(if was { Reset::Cleared } else { Reset::WasEnabled })
    }

    fn lock(&self) -> io::Result<File> {
        std::fs::create_dir_all(&self.dir)?;
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(self.dir.join(LOCK))?;
        // short sections only (a few small writes), so waiting for it is fine
        // SAFETY: a valid descriptor, owned by `f` for as long as the lock is used
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(f)
    }

    /// The devices with a record of their own (file names)
    fn records(&self) -> io::Result<Vec<String>> {
        Ok(self.names()?.into_iter().filter(|n| is_device_name(n)).collect())
    }

    fn released(&self) -> io::Result<Vec<String>> {
        Ok(self.names()?.into_iter().filter(|n| n.ends_with(RELEASED)).collect())
    }

    fn names(&self) -> io::Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut v: Vec<String> = entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        v.sort();
        Ok(v)
    }

    fn write_shadow(&self, udid: Option<&str>, why: &str) -> io::Result<()> {
        let dir = self.shadow.parent().unwrap_or(Path::new("."));
        let name = self.shadow.file_name().and_then(|n| n.to_str()).unwrap_or("UNLOCK-DISABLED");
        let head = match udid {
            Some(u) => format!("{SHADOW_MARK} {u}"),
            None => SHADOW_MARK.to_owned(),
        };
        write_atomic(dir, name, &format!("{head}\n{} {why}\n", now()))
    }

    /// The shadow goes only once no record is left: until then an older version must still see it
    fn drop_shadow_if_unused(&self) -> io::Result<()> {
        if self.records()?.is_empty() && !self.dir.join(LEGACY).try_exists()? {
            let dir = self.shadow.parent().unwrap_or(Path::new("."));
            let name = self.shadow.file_name().and_then(|n| n.to_str()).unwrap_or("UNLOCK-DISABLED");
            remove(dir, name)?;
        }
        Ok(())
    }
}

impl DeviceFlag {
    /// Whether auto-unlock is disabled for this device. When the records can't be read it counts as disabled
    pub fn is_disabled(&self) -> bool {
        let dir = &self.store.dir;
        if present(&dir.join(&self.name)) {
            return true;
        }
        present(&dir.join(LEGACY)) && !matches!(dir.join(format!("{}{RELEASED}", self.name)).try_exists(), Ok(true))
    }

    /// Records this device as disabled: the shadow first, then its own record, both on disk before this returns.
    /// Only after that may a digit that could complete the passcode be entered
    pub(crate) fn disable(&self, why: &str) -> io::Result<()> {
        let _lock = self.store.lock()?;
        self.store.write_shadow(Some(&self.name), why)?;
        write_atomic(&self.store.dir, &self.name, &format!("{} {why}\n", now()))
    }

    /// Removes this device's record once the passcode was confirmed to have unlocked the phone, or confirmed not to
    /// have been submitted. Its own record goes first, the shadow last
    pub(crate) fn clear(&self) -> io::Result<()> {
        let _lock = self.store.lock()?;
        remove(&self.store.dir, &self.name)?;
        self.store.drop_shadow_if_unused()
    }
}

/// Exists, or can't be told: either way it disables
fn present(p: &Path) -> bool {
    !matches!(p.try_exists(), Ok(false))
}

/// A UDID as a record's name: letters, digits and dashes only, upper case (usbmuxd reports them either way)
fn file_name(udid: &str) -> String {
    let name: String = udid
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_uppercase() } else { '_' })
        .collect();
    if name.is_empty() || name == LEGACY { format!("_{name}") } else { name }
}

fn is_device_name(n: &str) -> bool {
    n != LEGACY && !n.starts_with('.') && !n.contains('.')
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// `name` in `dir` replaced by `text`, on disk (file and name) when it returns
fn write_atomic(dir: &Path, name: &str, text: &str) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let r = (|| {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, dir.join(name))?;
        sync_dir(dir)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// `name` in `dir` removed, on disk when it returns; not being there is fine
fn remove(dir: &Path, name: &str) -> io::Result<()> {
    match std::fs::remove_file(dir.join(name)) {
        Ok(()) => sync_dir(dir),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (DisableStore, PathBuf) {
        let root = std::env::temp_dir().join(format!("ictl-disable-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("logs")).unwrap();
        (DisableStore::new(root.join("secrets/unlock-disabled"), root.join("logs/UNLOCK-DISABLED")), root)
    }

    const A: &str = "00008130-0019295E012B803A";
    const B: &str = "00008020-000925043499002E";

    /// A failure on one device disables that one only; the shadow stays until the last record goes
    #[test]
    fn one_device_failing_leaves_the_others_alone() {
        let (s, root) = store();
        let (a, b) = (s.device(A), s.device(B));
        a.disable("wrong").unwrap();
        assert!(a.is_disabled() && !b.is_disabled());
        assert!(s.shadow.is_file(), "an older version still sees it");
        b.disable("wrong").unwrap();
        assert_eq!(s.reset(A).unwrap(), Reset::Cleared);
        assert!(!a.is_disabled() && b.is_disabled());
        assert!(s.shadow.is_file(), "B is still disabled");
        b.clear().unwrap();
        assert!(!s.shadow.exists(), "nothing left");
        assert_eq!(s.reset(A).unwrap(), Reset::WasEnabled);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The same device in either case is one record
    #[test]
    fn a_udid_is_one_record_in_any_case() {
        let (s, root) = store();
        s.device(&A.to_lowercase()).disable("wrong").unwrap();
        assert!(s.device(A).is_disabled());
        s.reset(A).unwrap();
        assert!(!s.device(&A.to_lowercase()).is_disabled());
        assert_eq!(file_name("../LEGACY"), "___LEGACY", "never a path or the legacy record");
        assert_eq!(file_name("LEGACY"), "_LEGACY");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// An older version's single flag disables every device until each is reset; written again after a downgrade,
    /// it disables them all again
    #[test]
    fn an_older_versions_flag_disables_every_device_until_reset_one_by_one() {
        let (s, root) = store();
        std::fs::write(&s.shadow, "1700000000 still locked after entering the whole passcode\n").unwrap();
        assert_eq!(s.migrate().unwrap(), Migration::Legacy);
        assert!(s.legacy() && s.device(A).is_disabled() && s.device(B).is_disabled());
        assert_eq!(s.migrate().unwrap(), Migration::None, "once");
        assert_eq!(s.reset(A).unwrap(), Reset::Cleared);
        assert!(!s.device(A).is_disabled() && s.device(B).is_disabled());
        assert!(s.shadow.is_file(), "B was never released");
        // downgraded: the older version writes its flag again over the shadow
        std::fs::write(&s.shadow, "1700000100 entering the passcode\n").unwrap();
        assert_eq!(s.migrate().unwrap(), Migration::Legacy);
        assert!(s.device(A).is_disabled(), "the earlier release doesn't count any more");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Cut short between the shadow and the record (or after the record went, before the shadow did): the device
    /// the shadow names stays disabled
    #[test]
    fn a_shadow_left_alone_keeps_its_device_disabled() {
        let (s, root) = store();
        s.write_shadow(Some(&file_name(A)), "entering").unwrap();
        assert_eq!(s.migrate().unwrap(), Migration::Orphan(Some(file_name(A))));
        assert!(s.device(A).is_disabled() && !s.device(B).is_disabled());
        assert_eq!(s.migrate().unwrap(), Migration::None);
        std::fs::remove_dir_all(&root).unwrap();

        let (s, root) = store();
        s.write_shadow(None, "?").unwrap();
        assert_eq!(s.migrate().unwrap(), Migration::Orphan(None));
        assert!(s.device(A).is_disabled() && s.device(B).is_disabled(), "no device named: all of them");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Records whose shadow is gone get it back, so a downgrade still sees them
    #[test]
    fn records_without_their_shadow_get_it_back() {
        let (s, root) = store();
        s.device(A).disable("wrong").unwrap();
        std::fs::remove_file(&s.shadow).unwrap();
        assert_eq!(s.migrate().unwrap(), Migration::Shadow);
        assert!(s.shadow.is_file() && s.device(A).is_disabled());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Every state a crash can leave between the steps of a change disables at least what was disabled before it
    /// and, for a write, the device being written: never fewer
    #[test]
    fn a_change_cut_short_anywhere_disables_more_never_fewer() {
        // disable(A): shadow, then A's record. Cut after the shadow: the next start keeps A disabled
        let (s, root) = store();
        s.write_shadow(Some(&file_name(A)), "entering").unwrap();
        s.migrate().unwrap();
        assert!(s.device(A).is_disabled());
        std::fs::remove_dir_all(&root).unwrap();

        // clear(A) with B disabled too: A's record goes, then the shadow would; cut in between
        let (s, root) = store();
        s.device(A).disable("x").unwrap();
        s.device(B).disable("x").unwrap();
        remove(&s.dir, &file_name(A)).unwrap();
        assert_eq!(s.migrate().unwrap(), Migration::None);
        assert!(s.device(B).is_disabled() && s.shadow.is_file());
        std::fs::remove_dir_all(&root).unwrap();

        // the last record removed, the shadow not yet: an orphan, so A stays disabled (more, not fewer)
        let (s, root) = store();
        s.device(A).disable("x").unwrap();
        remove(&s.dir, &file_name(A)).unwrap();
        assert!(matches!(s.migrate().unwrap(), Migration::Orphan(_)));
        assert!(s.device(A).is_disabled());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Records that can't be read disable
    #[test]
    fn unreadable_records_disable() {
        use std::os::unix::fs::PermissionsExt;
        let (s, root) = store();
        s.device(B).disable("x").unwrap();
        std::fs::set_permissions(&s.dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        // root can read anything: the check only means something for a regular user
        let unreadable = std::fs::read_dir(&s.dir).is_err();
        let disabled = s.device(A).is_disabled();
        std::fs::set_permissions(&s.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if unreadable {
            assert!(disabled);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
