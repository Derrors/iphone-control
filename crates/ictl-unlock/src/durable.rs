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

/// [`create_dir_all`] for `dir` inside `base`, and every directory from `base` down to `dir` kept in its parent, made
/// now or not: one made earlier by something that didn't sync its parent (a plain `create_dir_all`, an older
/// version) would otherwise still be lost with a power cut, taking whatever is written inside it along
pub fn create_dir_all_from(base: &Path, dir: &Path) -> io::Result<()> {
    let rest = dir.strip_prefix(base).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not inside {}", dir.display(), base.display()))
    })?;
    create_dir_all(dir)?;
    let mut p = base.to_path_buf();
    sync_dir(parent(&p))?;
    for c in rest.components() {
        sync_dir(&p)?;
        p.push(c);
    }
    Ok(())
}

/// `base` with every directory above it that is missing, then each of `subdirs` inside it, every one of them kept in
/// its parent ([`create_dir_all`], [`create_dir_all_from`]). For the start of a data directory: run before anything
/// makes directories in it the plain way, which wouldn't keep a missing directory above it
pub fn create_tree(base: &Path, subdirs: &[&str]) -> io::Result<()> {
    create_dir_all(base)?;
    for d in subdirs {
        create_dir_all_from(base, &base.join(d))?;
    }
    Ok(())
}

/// `name` in `dir` replaced whole by `data` with permissions `mode`, on disk (data and name) when it returns. The
/// temporary file it is written to first is created with `mode` and set to exactly `mode` on its descriptor before
/// anything is written (the umask could leave it different), and named after `name`
/// (`.<name>.tmp-<pid>-<n>`), so a copy a crash left behind is found by [`remove_temps`]
pub fn write_atomic(dir: &Path, name: &str, data: &[u8], mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    create_dir_all(dir)?;
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{}{}-{n}", temp_prefix(name), std::process::id()));
    let r = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
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

/// The temporaries [`write_atomic`] left for `name` in `dir` (a crash between writing one and renaming it) removed,
/// on disk when it returns. The caller makes sure no write of `name` is under way
pub fn remove_temps(dir: &Path, name: &str) -> io::Result<()> {
    let prefix = temp_prefix(name);
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut removed = false;
    for e in entries {
        let e = e?;
        if e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix)) {
            match std::fs::remove_file(e.path()) {
                Ok(()) => removed = true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    if removed { sync_dir(dir) } else { Ok(()) }
}

fn temp_prefix(name: &str) -> String {
    format!(".{name}.tmp-")
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

    /// A data directory whose parent and grandparent are missing too: each directory made is kept in its parent, top
    /// down, before the subdirectories in it
    #[test]
    fn a_new_tree_is_kept_from_the_first_missing_directory_down() {
        let root = std::env::temp_dir().join(format!("ictl-durable-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let data = root.join("new/share/iphone-control");
        synced();
        create_tree(&data, &["secrets", "logs"]).unwrap();
        let got = synced();
        let at =
            |p: &Path| got.iter().position(|q| q == p).unwrap_or_else(|| panic!("{} not synced: {got:?}", p.display()));
        // new/ in the root, share/ in new/, iphone-control/ in share/, secrets/ and logs/ in iphone-control/
        let order = [root.clone(), root.join("new"), root.join("new/share"), data.clone()];
        for w in order.windows(2) {
            assert!(at(&w[0]) < at(&w[1]), "{} before {}: {got:?}", w[0].display(), w[1].display());
        }
        assert!(data.join("secrets").is_dir() && data.join("logs").is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Directories made earlier without syncing (a plain `create_dir_all`) are kept too, from the base down
    #[test]
    fn directories_made_earlier_are_kept_from_the_base_down() {
        let root = std::env::temp_dir().join(format!("ictl-durable-from-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("data/secrets")).unwrap();
        synced();
        create_dir_all_from(&root.join("data"), &root.join("data/secrets/passcodes")).unwrap();
        let got = synced();
        let at = |p: &Path| {
            got.iter().rposition(|q| q == p).unwrap_or_else(|| panic!("{} not synced: {got:?}", p.display()))
        };
        assert!(at(&root) < at(&root.join("data")), "data/ in its parent, then secrets/ in data/");
        assert!(at(&root.join("data")) < at(&root.join("data/secrets")), "then passcodes/ in secrets/");
        assert!(create_dir_all_from(&root.join("data"), &root.join("elsewhere")).is_err());
        std::fs::remove_dir_all(root).unwrap();
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
