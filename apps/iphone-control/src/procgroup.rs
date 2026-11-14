//! A command run in a process group of its own (`Command::process_group(0)`): an AI task's agent from the control
//! page ([`crate::agent`]) or the command of `task run` ([`crate::task`]). Stopping the task stops the whole group,
//! so the agent's MCP server and anything else it started go too.

use std::time::Duration;

/// How long a group has after SIGTERM before SIGKILL
const GRACE: Duration = Duration::from_secs(3);

/// SIGTERM to the group led by `pid`, SIGKILL [`GRACE`] later if it is still there
pub fn stop(pid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else { return };
    if pgid <= 1 {
        // -0 and -1 would signal this process's own group, or every process
        return;
    }
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    tokio::spawn(async move {
        tokio::time::sleep(GRACE).await;
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    });
}

#[cfg(test)]
mod tests {
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
