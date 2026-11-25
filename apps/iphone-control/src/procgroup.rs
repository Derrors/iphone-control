//! A command run in a process group of its own (`Command::process_group(0)`): an AI task's agent from the control
//! page ([`crate::agent`]) or the command of `task run` ([`crate::task`]). Stopping the task stops the whole group,
//! so the agent's MCP server and anything else it started go too.

use std::time::Duration;

/// How long a group has after SIGTERM before SIGKILL
pub const GRACE: Duration = Duration::from_secs(3);

/// How often a stopped group is looked for while it has [`GRACE`]
const POLL: Duration = Duration::from_millis(100);

/// Whether any process of the group is left (zombies count until reaped)
fn alive(pgid: libc::pid_t) -> bool {
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// SIGTERM to the group led by `pid`, SIGKILL [`GRACE`] later if it is still there. Looked for every [`POLL`] and
/// left alone once gone: the system doesn't give a group's number to another process while the group exists, but
/// may once it is gone, and a SIGKILL sent blindly 3 s later could then hit someone else's group
pub fn stop(pid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else { return };
    if pgid <= 1 {
        // -0 and -1 would signal this process's own group, or every process
        return;
    }
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + GRACE;
        while tokio::time::Instant::now() < deadline {
            if !alive(pgid) {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
        if alive(pgid) {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    });
}

/// Whether the group led by `pid` still has a process in it
pub fn any_left(pid: u32) -> bool {
    libc::pid_t::try_from(pid).is_ok_and(|g| g > 1 && alive(g))
}

/// [`stop`] for several groups, waiting for them here instead of in a task (the program is about to exit, which a
/// task wouldn't outlive): SIGTERM to each, then SIGKILL to those still there after [`GRACE`]
pub async fn stop_and_wait(leaders: &[u32]) {
    let groups: Vec<libc::pid_t> =
        leaders.iter().filter_map(|&p| libc::pid_t::try_from(p).ok()).filter(|&g| g > 1).collect();
    for &g in &groups {
        unsafe { libc::kill(-g, libc::SIGTERM) };
    }
    let deadline = tokio::time::Instant::now() + GRACE;
    while groups.iter().any(|&g| alive(g)) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for &g in groups.iter().filter(|&&g| alive(g)) {
        unsafe { libc::kill(-g, libc::SIGKILL) };
    }
}

#[cfg(test)]
mod tests {
    /// A group is seen while anything of it runs, and not once it is gone (then `stop` sends no SIGKILL)
    #[tokio::test]
    async fn a_group_is_seen_until_it_is_gone() {
        use std::os::unix::process::CommandExt;
        let mut leader = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        assert!(super::any_left(leader.id()));
        super::stop(leader.id());
        leader.wait().unwrap();
        assert!(!super::any_left(leader.id()), "reaped and gone");
        assert!(!super::any_left(0) && !super::any_left(1), "never this process's own group, or every process");
    }

    #[tokio::test]
    async fn stopping_the_group_stops_what_it_started() {
        use std::os::unix::process::CommandExt;
        // a shell (the group's leader) and a child of its own
        let mut leader =
            std::process::Command::new("sh").args(["-c", "sleep 30 & wait"]).process_group(0).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        super::stop(leader.id());
        let started = std::time::Instant::now();
        while leader.try_wait().unwrap().is_none() {
            assert!(started.elapsed() < Duration::from_secs(5), "still running");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // the group is gone too: no live process with that group id is left. The child, orphaned when its shell
        // went, may stay a zombie until whatever adopted it reaps it (in a CI container that can take a while), so
        // zombies don't count
        let pgid = leader.id() as libc::pid_t;
        let started = std::time::Instant::now();
        while group_alive(pgid) {
            assert!(started.elapsed() < Duration::from_secs(5), "a process of the group is still running");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn stopping_and_waiting_ends_every_group() {
        use std::os::unix::process::CommandExt;
        let mut leaders: Vec<_> = (0..2)
            .map(|_| std::process::Command::new("sh").args(["-c", "sleep 30 & wait"]).process_group(0).spawn().unwrap())
            .collect();
        std::thread::sleep(Duration::from_millis(200));
        let ids: Vec<u32> = leaders.iter().map(std::process::Child::id).collect();
        tokio::time::timeout(Duration::from_secs(5), super::stop_and_wait(&ids)).await.unwrap();
        for l in &mut leaders {
            let started = std::time::Instant::now();
            while l.try_wait().unwrap().is_none() {
                assert!(started.elapsed() < Duration::from_secs(5), "still running");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// A process of group `pgid` still runs (on Linux, a zombie isn't counted; elsewhere any process is)
    fn group_alive(pgid: libc::pid_t) -> bool {
        let Ok(dir) = std::fs::read_dir("/proc") else { return unsafe { libc::kill(-pgid, 0) } == 0 };
        dir.flatten().filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok()).any(|stat| {
            // `pid (comm) state ppid pgrp …`: comm may hold spaces, so read after its closing parenthesis
            let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
            let mut f = rest.split_whitespace();
            let state = f.next();
            let pgrp = f.nth(1).and_then(|p| p.parse::<libc::pid_t>().ok());
            pgrp == Some(pgid) && state != Some("Z")
        })
    }

    use std::time::Duration;
}
