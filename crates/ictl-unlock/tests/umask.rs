//! A restrictive umask doesn't change what `durable::write_atomic` gives a file. In a test binary of its own: the
//! umask is the whole process's, and would change what the other tests' files get

use std::os::unix::fs::PermissionsExt;

#[test]
fn a_restrictive_umask_leaves_the_mode_asked_for() {
    let dir = std::env::temp_dir().join(format!("ictl-umask-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: no other thread of this binary makes files meanwhile
    let old = unsafe { libc::umask(0o277) };
    let r = ictl_unlock::durable::write_atomic(&dir, "A", b"123456", 0o600);
    unsafe { libc::umask(old) };
    r.unwrap();
    let mode = std::fs::metadata(dir.join("A")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");
    assert_eq!(std::fs::read(dir.join("A")).unwrap(), b"123456");
    std::fs::remove_dir_all(dir).unwrap();
}
