//! The phone's "viewed remotely" state left behind by a daemon that ended with a HID session open.
//!
//! The HID session holds the phone's screen media stream; closing it stops the stream, and the phone drops the
//! Dynamic Island's "viewed" icon and may auto-lock again. A daemon that ends without closing it (the watchdog's exit
//! while a request is stuck holding the device lock, a crash, SIGKILL) leaves the phone thinking it is still viewed:
//! the icon stays and the phone doesn't auto-lock, for minutes at least (iPhone16,2, iOS 27.2.1: still so 5 minutes
//! after a SIGKILL). Opening a session and closing it properly clears it.
//!
//! The watchdog can't close the session itself (the stuck request holds the lock it is behind), so the next daemon
//! does: a marker file per phone (`hid-open/<udid>` in the data directory) is written when a session opens and
//! removed when it closes properly. Markers found at start are cleared by opening and closing a session once the
//! phone is ready again ([`crate::Daemon`]'s supervision round).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Tries at clearing a phone's state before giving up on it
pub(crate) const TRIES: u32 = 3;

/// The marker for `udid` in `dir`, written now. `None` if it couldn't be (nothing else depends on it)
pub(crate) fn mark(dir: &Path, udid: &str) -> Option<PathBuf> {
    let name = sanitize(udid)?;
    std::fs::create_dir_all(dir).ok()?;
    let p = dir.join(name);
    std::fs::write(&p, b"").ok()?;
    Some(p)
}

/// The phones whose marker an earlier daemon left behind, each with no tries yet
pub(crate) fn left_over(dir: &Path) -> HashMap<String, u32> {
    let Ok(entries) = std::fs::read_dir(dir) else { return HashMap::new() };
    entries.flatten().filter_map(|e| e.file_name().to_str().and_then(sanitize)).map(|u| (u, 0)).collect()
}

/// Whether an earlier daemon (of this instance or another) left `udid`'s marker: read again whenever this instance
/// takes the device's lease, since another may have had it meanwhile (docs/dev/multi-instance-plan.md 4.6)
pub(crate) fn is_left(dir: &Path, udid: &str) -> bool {
    sanitize(udid).is_some_and(|n| dir.join(n).is_file())
}

/// The marker for `udid` in `dir`, if any, removed
pub(crate) fn clear(dir: &Path, udid: &str) {
    if let Some(name) = sanitize(udid) {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// A UDID as a file name: letters, digits and dashes only
fn sanitize(udid: &str) -> Option<String> {
    (!udid.is_empty() && udid.len() <= 64 && udid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .then(|| udid.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_outlive_the_process_until_cleared() {
        let dir = std::env::temp_dir().join(format!("ictl-hidmark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(left_over(&dir).is_empty(), "no directory yet");
        let p = mark(&dir, "00008130-0019295E012B803A").unwrap();
        assert!(p.is_file());
        assert!(mark(&dir, "../escape").is_none(), "not a UDID");
        // the next daemon finds it
        assert_eq!(left_over(&dir), HashMap::from([("00008130-0019295E012B803A".to_owned(), 0)]));
        clear(&dir, "00008130-0019295E012B803A");
        assert!(left_over(&dir).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
