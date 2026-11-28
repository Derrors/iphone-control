// iphone-control: this file is not upstream; it is part of the MPL-2.0 omnisette here (THIRD_PARTY.md).
//
//! The remote provider's provisioning state on disk (`state.plist`), shared by every process that uses the same
//! configuration directory (several iphone-control daemons, the CLI's `login`).
//!
//! Upstream reads it once, provisions when it isn't, and writes it back in place. Two processes provisioning at once
//! wrote over each other, and a write cut short left a file that reads as unprovisioned. Here:
//! - provisioning happens under a file lock (`state.lock`), after the state on disk is read again: a process that
//!   waited uses what the other one provisioned instead of writing its own over it;
//! - the state is written to a temporary file, synced and renamed, and the directory synced;
//! - the address the state was provisioned with is kept in the same file (`provisioned_by`), so the state and where
//!   it came from never disagree. A state without it was provisioned by an older version: where it came from isn't
//!   known ([`Origin::Unknown`]).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::AnisetteError;
use crate::remote_anisette_v3::AnisetteState;

pub const STATE_FILE: &str = "state.plist";
const LOCK_FILE: &str = "state.lock";
/// How long to wait for another process provisioning (a provisioning takes seconds, and is bounded by its own
/// timeouts)
const LOCK_WAIT: Duration = Duration::from_secs(90);

/// Where the state in a configuration directory came from
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// No state yet: the next use provisions one with the configured address
    Missing,
    /// A state that isn't provisioned (an identifier only): the same
    Unprovisioned,
    /// Provisioned, but by a version that didn't record with which address
    Unknown,
    /// Provisioned with this address
    Url(String),
}

/// The same address written with or without a trailing slash, in either case
pub fn same_address(a: &str, b: &str) -> bool {
    a.trim_end_matches('/').eq_ignore_ascii_case(b.trim_end_matches('/'))
}

/// Where the state in `dir` came from. A state that can't be read is an error, never taken for missing
pub fn origin(dir: &Path) -> Result<Origin, AnisetteError> {
    origin_of(&read(dir)?)
}

/// The state file's bytes and where they came from, read once: the two never belong to different states. `None`: no
/// state yet
pub fn snapshot(dir: &Path) -> Result<Option<(Vec<u8>, Origin)>, AnisetteError> {
    let bytes = match std::fs::read(dir.join(STATE_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let state: AnisetteState = plist::from_bytes(&bytes)?;
    let origin = origin_of(&Some(state))?;
    Ok(Some((bytes, origin)))
}

fn origin_of(state: &Option<AnisetteState>) -> Result<Origin, AnisetteError> {
    Ok(match state {
        None => Origin::Missing,
        Some(s) if !s.is_provisioned() => Origin::Unprovisioned,
        Some(s) => s.provisioned_by.clone().map(Origin::Url).unwrap_or(Origin::Unknown),
    })
}

/// The state's lock, held while nothing may provision or replace it (a login compares the state it used with the one
/// there, then writes its credentials). Waited for as provisioning does
pub async fn hold(dir: &Path) -> Result<StateLock, AnisetteError> {
    Ok(lock(dir).await?)
}

/// Records that the provisioned state in `dir` came from `url` (the user said so). `Ok(false)`: there is no
/// provisioned state to record it for
pub async fn confirm_origin(dir: &Path, url: &str) -> Result<bool, AnisetteError> {
    let _lock = lock(dir).await?;
    let Some(mut s) = read(dir)? else { return Ok(false) };
    if !s.is_provisioned() {
        return Ok(false);
    }
    s.provisioned_by = Some(url.to_owned());
    write(dir, &s)?;
    Ok(true)
}

/// Removes the state in `dir`, so the next use provisions a new one with the configured address
pub async fn discard(dir: &Path) -> Result<(), AnisetteError> {
    let _lock = lock(dir).await?;
    match std::fs::remove_file(dir.join(STATE_FILE)) {
        Ok(()) => sync_dir(dir)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Held while the state is read, provisioned and written
pub struct StateLock(#[allow(dead_code)] File);

pub(crate) async fn lock(dir: &Path) -> io::Result<StateLock> {
    std::fs::create_dir_all(dir)?;
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(LOCK_FILE))?;
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        // SAFETY: a valid descriptor, owned by `f` for as long as the lock is used
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(StateLock(f));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::WouldBlock {
            return Err(e);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "another process has been provisioning anisette for too long",
            ));
        }
        // polled, not waited on: a daemon runs on one thread, and a blocking wait would stop it
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The state in `dir`; `None` when there is none. One that is there but can't be parsed is an error
pub(crate) fn read(dir: &Path) -> Result<Option<AnisetteState>, AnisetteError> {
    match plist::from_file::<_, AnisetteState>(dir.join(STATE_FILE)) {
        Ok(s) => Ok(Some(s)),
        Err(e) => match e.as_io() {
            Some(io) if io.kind() == io::ErrorKind::NotFound => Ok(None),
            _ => Err(e.into()),
        },
    }
}

/// `state` written in place of the one in `dir`, whole: a crash leaves the old one or the new one
pub(crate) fn write(dir: &Path, state: &AnisetteState) -> Result<(), AnisetteError> {
    use std::os::unix::fs::PermissionsExt;
    let mut data = Vec::new();
    plist::to_writer_xml(&mut data, state)?;
    let tmp: PathBuf = dir.join(format!(".{STATE_FILE}.tmp-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
    let r = (|| -> io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(&data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dir.join(STATE_FILE))?;
        sync_dir(dir)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(r?)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}
