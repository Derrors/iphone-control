//! Device leases (docs/dev/multi-instance-plan.md 4.1): one process at a time works with a device.
//!
//! A lease is `flock(LOCK_EX)` on `<data root>/devices/<UDID in upper case>.lock`. The file is only ever created,
//! never removed or replaced, so every process locks the same inode; the kernel lets go of the lock when the holder
//! ends, however it ends. Who holds a lease is what `flock` says, nothing else.
//!
//! The holder writes a short description of itself into the file ([`Holder`]): which instance (or a `probe:`, a
//! short lease to read a device's details for display), its process, and the device's name, model and iOS version.
//! It is a hint for others to show (`devices` lists a device another instance controls by it, without reading the
//! device): it may be half written or out of date.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The hint's size in the file: written whole each time, so a shorter one leaves nothing of a longer one behind
const HINT_LEN: usize = 512;
const INSTANCE: &str = "instance:";
const PROBE: &str = "probe:";

/// Who holds a lease, as the holder describes itself (a hint, see the module)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// `instance:<name>` or `probe:<name>`
    pub by: String,
    pub pid: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub ios: String,
}

impl Holder {
    /// The instance `name` controlling the device
    pub fn instance(name: &str) -> Self {
        Self { by: format!("{INSTANCE}{name}"), pid: std::process::id(), ..Self::default() }
    }

    /// The instance `name` reading the device's details for a moment
    pub fn probe(name: &str) -> Self {
        Self { by: format!("{PROBE}{name}"), pid: std::process::id(), ..Self::default() }
    }

    /// The instance controlling the device; `None` for a probe or a hint that can't be read
    pub fn instance_name(&self) -> Option<&str> {
        self.by.strip_prefix(INSTANCE)
    }

    /// Written by this process as the instance `name`: a lease it let go of, still held by its own work
    pub fn is_own(&self, name: &str) -> bool {
        self.pid == std::process::id() && self.instance_name() == Some(name)
    }

    pub fn is_probe(&self) -> bool {
        self.by.starts_with(PROBE)
    }

    /// The same holder, describing `info`'s device
    pub fn with_device(mut self, info: Option<&ictl_device::DeviceInfo>) -> Self {
        if let Some(i) = info {
            self.name = cap(&i.name);
            self.model = cap(&i.product_type);
            self.ios = cap(&i.product_version);
        }
        self
    }
}

/// Room for every field in [`HINT_LEN`]
fn cap(s: &str) -> String {
    s.chars().take(96).collect()
}

/// A held lease; let go of when dropped, and every [`Hold`] of it with it
#[derive(Debug)]
pub struct Lease {
    file: std::sync::Arc<File>,
    udid: String,
    described: Holder,
}

/// What keeps a lease held for work still going on after the instance let go of it (docs/dev/multi-instance-plan.md
/// 10.5): the kernel lets go of the lock once the last descriptor of it closes, so a re-signing's thread still
/// installing on the device keeps it from another instance until it ends
#[derive(Debug, Clone)]
pub struct Hold(#[allow(dead_code)] std::sync::Arc<File>);

impl Lease {
    /// Keeps the lease held for as long as the `Hold` is
    pub fn hold(&self) -> Hold {
        Hold(self.file.clone())
    }

    /// The device, as it was named when the lease was taken
    pub fn udid(&self) -> &str {
        &self.udid
    }

    /// Whether this is the lease of `udid` (in either case)
    pub fn is_for(&self, udid: &str) -> bool {
        self.udid.eq_ignore_ascii_case(udid)
    }

    /// Writes the hint again when it changed
    pub fn describe(&mut self, holder: Holder) -> io::Result<()> {
        if holder == self.described {
            return Ok(());
        }
        write_hint(&self.file, &holder)?;
        self.described = holder;
        Ok(())
    }
}

/// What [`try_claim`] found
#[derive(Debug)]
pub enum Claim {
    Got(Lease),
    /// Someone else holds it; what it says of itself, if that can be read
    Held(Option<Holder>),
}

/// A UDID that can name a file: usbmuxd's are letters, digits and dashes
fn valid_udid(udid: &str) -> bool {
    !udid.is_empty() && udid.len() <= 64 && udid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn path(dir: &Path, udid: &str) -> io::Result<PathBuf> {
    if !valid_udid(udid) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a device UDID: {udid:?}")));
    }
    Ok(dir.join(format!("{}.lock", udid.to_ascii_uppercase())))
}

/// A shared directory of the data root, made this user's alone when it is made
pub(crate) fn ensure_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Takes the lease of `udid` if no one holds it, describing the holder as `holder`. Never waits
pub fn try_claim(dir: &Path, udid: &str, holder: Holder) -> io::Result<Claim> {
    let p = path(dir, udid)?;
    ensure_dir(dir)?;
    // std opens with O_CLOEXEC: a child the daemon starts doesn't keep the lease after it ends
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&p)?;
    // a few tries a few milliseconds apart: [`holder`] takes a shared lock for an instant on a lease no one holds
    for tries_left in (0..3).rev() {
        // SAFETY: a valid descriptor, owned by `file` for as long as the lease is held
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::WouldBlock {
            return Err(e);
        }
        if tries_left == 0 {
            return Ok(Claim::Held(read_hint(&file)));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    write_hint(&file, &holder)?;
    Ok(Claim::Got(Lease { file: std::sync::Arc::new(file), udid: udid.to_owned(), described: holder }))
}

/// What the holder of `udid`'s lease says of itself, without taking it; `None` when no one holds it or the hint can't
/// be read
pub fn holder(dir: &Path, udid: &str) -> Option<Holder> {
    let file = File::open(path(dir, udid).ok()?).ok()?;
    // held: a shared lock can't be taken while an exclusive one is
    // SAFETY: a valid descriptor, owned by `file`
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return None;
    }
    read_hint(&file)
}

fn write_hint(file: &File, holder: &Holder) -> io::Result<()> {
    let mut buf = serde_json::to_vec(holder).map_err(io::Error::other)?;
    buf.truncate(HINT_LEN);
    buf.resize(HINT_LEN, b' ');
    file.write_all_at(&buf, 0)
}

fn read_hint(file: &File) -> Option<Holder> {
    let mut buf = vec![0; HINT_LEN];
    let n = file.read_at(&mut buf, 0).ok()?;
    serde_json::from_slice(buf[..n].trim_ascii()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        std::env::temp_dir().join(format!("ictl-lease-{}", uuid::Uuid::new_v4()))
    }

    /// One holder at a time, across separate opens (as in two processes), whatever the case of the UDID; the others
    /// see who holds it; dropped, it is free again
    #[test]
    fn a_lease_has_one_holder_whatever_the_case() {
        let d = dir();
        let Claim::Got(mut a) = try_claim(&d, "00008020-000925043499002e", Holder::instance("ipad")).unwrap() else {
            panic!("free")
        };
        assert!(a.is_for("00008020-000925043499002E"));
        match try_claim(&d, "00008020-000925043499002E", Holder::instance("default")).unwrap() {
            Claim::Held(Some(h)) => assert_eq!(h.instance_name(), Some("ipad")),
            other => panic!("{other:?}"),
        }
        let info = ictl_device::DeviceInfo {
            udid: "x".into(),
            name: "iPad".into(),
            product_type: "iPad11,1".into(),
            product_version: "26.1".into(),
        };
        a.describe(Holder::instance("ipad").with_device(Some(&info))).unwrap();
        let h = holder(&d, "00008020-000925043499002E").unwrap();
        assert_eq!((h.name.as_str(), h.ios.as_str(), h.pid), ("iPad", "26.1", std::process::id()));
        drop(a);
        assert_eq!(holder(&d, "00008020-000925043499002E"), None, "no one holds it");
        // work going on under it keeps it held after the lease itself went
        let Claim::Got(again) = try_claim(&d, "00008020-000925043499002E", Holder::instance("ipad")).unwrap() else {
            panic!("free")
        };
        let hold = again.hold();
        drop(again);
        assert!(matches!(try_claim(&d, "00008020-000925043499002E", Holder::probe("x")).unwrap(), Claim::Held(_)));
        drop(hold);
        assert!(matches!(try_claim(&d, "00008020-000925043499002E", Holder::probe("default")).unwrap(), Claim::Got(_)));
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "one file per device");
        std::fs::remove_dir_all(d).unwrap();
    }

    /// A probe says so; a long hint is cut to fit and still leaves nothing of an earlier one; a UDID that isn't one
    /// names no file
    #[test]
    fn hints_and_udids() {
        let d = dir();
        let Claim::Got(mut l) = try_claim(&d, "AAAA", Holder::instance("phone")).unwrap() else { panic!() };
        let long = ictl_device::DeviceInfo { name: "n".repeat(400), ..Default::default() };
        l.describe(Holder::instance("phone").with_device(Some(&long))).unwrap();
        assert_eq!(holder(&d, "AAAA").unwrap().name.len(), 96);
        drop(l);
        let Claim::Got(_p) = try_claim(&d, "aaaa", Holder::probe("phone")).unwrap() else { panic!() };
        assert!(holder(&d, "AAAA").unwrap().is_probe());
        for bad in ["", "../x", "a/b", "a b"] {
            assert!(try_claim(&d, bad, Holder::probe("x")).is_err(), "{bad:?}");
        }
        std::fs::remove_dir_all(d).unwrap();
    }
}
