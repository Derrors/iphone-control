//! A shared lease covers every host client and device operation. The updater alone takes the
//! exclusive lease, across daemon restarts. The kernel cancels either lease on process death.
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

fn directory() -> io::Result<PathBuf> {
    // Independent of the socket, XDG runtime directory and executable: a second Viewer, even
    // one talking to a different socket, or a direct WDA client must still count as active.
    if crate::protocol::running_as_root() {
        return Ok(crate::protocol::default_data_dir());
    }
    Ok(PathBuf::from(std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?)
        .join(".local/share/iphone-control"))
}

fn lock(dir: &Path, operation: libc::c_int) -> io::Result<File> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let owner = dir.metadata()?;
    if owner.uid() != unsafe { libc::geteuid() } || owner.mode() & 0o022 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe host maintenance directory"));
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(dir.join("maintenance.lock"))?;
    // Separate open file descriptions, including within the same process, conflict.
    if unsafe { libc::flock(f.as_raw_fd(), operation | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Busy: host update or device client is active; retry when idle",
            ));
        }
        return Err(e);
    }
    Ok(f)
}

/// Busy is not always a client: the daemon itself holds the shared lease while it works on the phone (every
/// status tick, starting WDA, re-signing, the Apple ID check), from milliseconds to a couple of minutes
pub fn exclusive() -> io::Result<File> {
    lock(&directory()?, libc::LOCK_EX).map_err(|e| {
        if e.kind() == io::ErrorKind::WouldBlock {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "the phone is in use by a client (control page, MCP) or by the daemon's own work \
                 (status check, starting WDA, re-signing)",
            )
        } else {
            e
        }
    })
}

fn pending(dir: &Path) -> io::Result<Option<PathBuf>> {
    let data = match std::fs::read(dir.join("maintenance.pending")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let path = PathBuf::from(OsString::from_vec(data));
    if !path.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid maintenance journal path"));
    }
    Ok(path.try_exists()?.then_some(path))
}

/// Called under the exclusive lease, before the durable transaction journal is created.
/// Removing the journal at commit makes this pointer inert, even if the updater then dies.
pub fn mark_pending(journal: &Path) -> io::Result<()> {
    remember(&directory()?, journal)
}

fn remember(dir: &Path, journal: &Path) -> io::Result<()> {
    if let Some(path) = pending(dir)? {
        if path == journal {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("recover the pending transaction at {}", path.display()),
        ));
    }
    let temp = dir.join("maintenance.pending.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    file.write_all(journal.as_os_str().as_bytes())?;
    file.sync_all()?;
    std::fs::rename(temp, dir.join("maintenance.pending"))?;
    File::open(dir)?.sync_all()
}

fn shared_in(dir: &Path) -> io::Result<File> {
    let lease = lock(dir, libc::LOCK_SH)?;
    if let Some(journal) = pending(dir)? {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("Host update recovery pending ({}); run iphone-control update recover", journal.display()),
        ));
    }
    Ok(lease)
}

pub fn shared() -> io::Result<File> {
    shared_in(&directory()?)
}

pub(crate) fn active() -> Option<bool> {
    match shared() {
        Ok(_) => Some(false),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Some(true),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_journal_blocks_other_installs_after_lock_owner_dies() {
        let dir = std::env::temp_dir().join(format!("ictl-pending-{}", std::process::id()));
        let install = dir.join("prefix").join("journal.json");
        let other = dir.join("another-prefix").join("journal.json");
        let lease = lock(&dir, libc::LOCK_EX).unwrap();
        remember(&dir, &install).unwrap();
        std::fs::create_dir_all(install.parent().unwrap()).unwrap();
        std::fs::write(&install, "prepared").unwrap();
        drop(lease);
        assert_eq!(shared_in(&dir).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(remember(&dir, &other).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        std::fs::remove_file(install).unwrap();
        shared_in(&dir).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// A lock that is free again: another test spawning a process meanwhile (procs.rs) briefly holds a copy of the
    /// dropped descriptor in the child until it execs, and the lock with it
    fn once_free(dir: &Path, operation: libc::c_int) -> File {
        for _ in 0..100 {
            match lock(dir, operation) {
                Ok(f) => return f,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                Err(e) => panic!("{e}"),
            }
        }
        panic!("still locked after 2 s");
    }

    #[test]
    fn readers_exclude_maintenance_and_resume_after_it() {
        let dir = std::env::temp_dir().join(format!("ictl-lease-{}", std::process::id()));
        let client = lock(&dir, libc::LOCK_SH).unwrap();
        let signing = lock(&dir, libc::LOCK_SH).unwrap();
        assert_eq!(lock(&dir, libc::LOCK_EX).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        drop(client);
        assert!(lock(&dir, libc::LOCK_EX).is_err());
        drop(signing);
        let update = once_free(&dir, libc::LOCK_EX);
        assert!(lock(&dir, libc::LOCK_SH).is_err());
        drop(update);
        once_free(&dir, libc::LOCK_SH);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
