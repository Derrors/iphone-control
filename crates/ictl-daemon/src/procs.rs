//! Which process is on the other end, and whether it was started by an agent: a task's agent may not answer the
//! questions it asks the user, nor resume itself (docs/task-control.md §2.5).
//!
//! Not a security boundary: the agent runs as the same user, and a process it double-forks into a daemon is
//! adopted by init (or a subreaper) and no longer descends from it. It does stop an agent that runs
//! `iphone-control task approve`, or calls the page's API, from its own shell.

/// The process that started `pid`; `None` when it is gone or can't be read
pub fn parent(pid: u32) -> Option<u32> {
    parent_of(pid).filter(|&p| p != pid)
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (name) state ppid …`; the name may hold spaces and parentheses
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a plain C struct of exactly `size` bytes, which proc_pidinfo fills
    let n = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (n == size).then_some(info.pbi_ppid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn parent_of(_pid: u32) -> Option<u32> {
    None
}

/// Whether `pid` is `ancestor` or was started by it, directly or further down
pub fn descends_from(pid: u32, ancestor: u32) -> bool {
    if ancestor <= 1 {
        return false;
    }
    let mut cur = pid;
    // a chain of processes is short; the bound only guards against a loop read while pids are reused
    for _ in 0..128 {
        if cur == ancestor {
            return true;
        }
        match parent(cur) {
            Some(p) if p > 1 => cur = p,
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_child_descends_from_its_parent_and_not_the_other_way() {
        let me = std::process::id();
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let kid = child.id();
        assert_eq!(parent(kid), Some(me));
        assert!(descends_from(kid, me));
        assert!(descends_from(me, me), "a process counts as its own");
        assert!(!descends_from(me, kid));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn init_or_nothing_is_nobody_s_ancestor() {
        assert!(!descends_from(std::process::id(), 1));
        assert!(!descends_from(std::process::id(), 0));
    }
}
