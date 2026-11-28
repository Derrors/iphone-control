//! File-system steps that survive a power cut, for what auto-unlock keeps on disk: the passcodes and the records of
//! auto-unlock disabled after a failure ([`crate::DisableStore`]).
//!
//! Syncing a file keeps its data, not its name: the name is an entry in its directory, kept only once the directory is
//! synced too. The same goes for a directory made just now, whose name is an entry in its parent. So every rename and
//! removal is followed by a sync of the directory it changed, and every directory made by [`create_dir_all`] by a sync
//! of its parent.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// `dir` synced: the entries made, renamed or removed in it are on disk when it returns
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(test)]
    tests::SYNCED.with(|s| s.borrow_mut().push(dir.to_owned()));
    File::open(dir)?.sync_all()
}

/// `dir` and the directories above it that are missing, each made and its parent synced, so none of them is lost
/// with a power cut. The parent of `dir` itself is synced even when `dir` was there already: whoever made it may have
/// stopped before syncing
pub fn create_dir_all(dir: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut p = dir;
    loop {
        match std::fs::metadata(p) {
            Ok(m) if m.is_dir() => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is not a directory", p.display()),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(p);
                match p.parent() {
                    Some(q) if !q.as_os_str().is_empty() => p = q,
                    _ => break,
                }
            }
            Err(e) => return Err(e),
        }
    }
    for d in missing.iter().rev() {
        match std::fs::create_dir(d) {
            Ok(()) => {}
            // made meanwhile by another process: synced below all the same
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        sync_dir(parent(d))?;
    }
    if missing.is_empty() {
        sync_dir(parent(dir))?;
    }
    Ok(())
}

/// `name` in `dir` replaced whole by `data` with permissions `mode`, on disk (data and name) when it returns. The
/// temporary file it is written to first has `mode` from the start and a name no other call uses
pub fn write_atomic(dir: &Path, name: &str, data: &[u8], mode: u32) -> io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    create_dir_all(dir)?;
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.tmp-{}-{n}", std::process::id()));
    let r = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(data)?;
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
pub fn remove(dir: &Path, name: &str) -> io::Result<()> {
    match std::fs::remove_file(dir.join(name)) {
        Ok(()) => sync_dir(dir),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn parent(p: &Path) -> &Path {
    match p.parent() {
        Some(q) if !q.as_os_str().is_empty() => q,
        _ => Path::new("."),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        /// The directories synced on this thread, in order: a power cut keeps what was synced and may lose the rest
        pub(crate) static SYNCED: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn synced() -> Vec<PathBuf> {
        SYNCED.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    /// Every directory made is followed by a sync of its parent, top down, before anything is written inside it
    #[test]
    fn a_new_directory_is_kept_by_syncing_its_parent() {
        let root = std::env::temp_dir().join(format!("ictl-durable-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        synced();
        write_atomic(&root.join("secrets/unlock-disabled"), "A", b"x", 0o600).unwrap();
        assert_eq!(
            synced(),
            [root.clone(), root.join("secrets"), root.join("secrets/unlock-disabled")],
            "secrets/ in the root, unlock-disabled/ in secrets/, then the file's name"
        );
        // there already: its parent once more (its maker may have stopped before syncing), then the file
        write_atomic(&root.join("secrets/unlock-disabled"), "B", b"y", 0o600).unwrap();
        assert_eq!(synced(), [root.join("secrets"), root.join("secrets/unlock-disabled")]);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.join("secrets/unlock-disabled/A")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let left: Vec<_> = std::fs::read_dir(root.join("secrets/unlock-disabled")).unwrap().flatten().collect();
        assert_eq!(left.len(), 2, "no temporary file left");
        std::fs::remove_dir_all(root).unwrap();
    }
}
