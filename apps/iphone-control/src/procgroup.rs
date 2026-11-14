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
        // the group is gone too: no process with that group id is left
        let r = unsafe { libc::kill(-(leader.id() as libc::pid_t), 0) };
        assert!(r != 0, "a process of the group is still there");
    }

    use std::time::Duration;
}
