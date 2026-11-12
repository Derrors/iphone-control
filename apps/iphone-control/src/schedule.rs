//! Run checks outside the daemon so that stopping the daemon cannot kill its updater.
//! The launcher selects the *old* executable for crash recovery, even if the new one cannot start.
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::{envfile, service, shell_quote, update};

const LABEL: &str = "io.github.iphone-control.updates";
const TIMER: &str = "iphone-control-updates.timer";
const UNIT: &str = "iphone-control-updates.service";

pub fn set_policy(socket: &Path, policy: update::Policy) -> Result<()> {
    let _locks = configuration_locks()?;
    update::set_policy(policy)?;
    if let Err(e) = configure_job(socket, policy) {
        // A partial timer install must not later run with unintended permission to replace files.
        update::set_policy(update::Policy::Manual)?;
        bail!("Update policy is manual; could not configure the user job: {e:#}")
    }
    println!(
        "{}: {}",
        ictl_l10n::tr!("更新策略", "Update policy"),
        serde_json::to_value(policy)?.as_str().unwrap_or("manual")
    );
    Ok(())
}

// Keep concurrent policy writes and service-manager changes in the same order. The shared
// maintenance lease also prevents launchctl bootout from terminating an active replacement.
fn configuration_locks() -> Result<(File, File)> {
    let config = envfile::config_dir();
    fs::create_dir_all(&config)?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(config.join("update-schedule.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("could not lock the update schedule");
    }
    Ok((lock, ictl_daemon::maintenance::shared()?))
}

pub fn configure(socket: &Path) -> Result<()> {
    let _locks = configuration_locks()?;
    configure_job(socket, update::cached()?.policy)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().context("invalid job path")?)?;
    crate::update::write_atomic(path, bytes)
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program).args(args).output()?;
    ensure!(
        output.status.success(),
        "{program} {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn path_str(path: &Path) -> Result<&str> {
    let s = path.to_str().context("non UTF-8 update job path")?;
    ensure!(path.is_absolute() && !s.contains(['\r', '\n']), "expected an absolute update job path");
    Ok(s)
}

fn launcher(exe: &Path, socket: &Path, config: &Path) -> Result<String> {
    let exe = shell_quote(path_str(exe)?);
    let sock = shell_quote(path_str(socket)?);
    let xdg = shell_quote(path_str(config.parent().context("missing config home")?)?);
    // launchd and the interactive shell often have different XDG environments. The timer
    // must see the same release cache as Viewer, including a custom cache location.
    let mut xdg_env = String::new();
    for name in ["XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR"] {
        if let Some(value) = std::env::var_os(name).filter(|v| !v.is_empty()) {
            let value = shell_quote(path_str(Path::new(&value))?);
            xdg_env.push_str(&format!("export {name}={value}\n"));
        }
    }
    Ok(format!(
        "#!/bin/sh\n# iphone-control update job\nexe={exe}\nsocket={sock}\nexport XDG_CONFIG_HOME={xdg}\n{xdg_env}\
        state=\"${{exe%/*}}/.iphone-control-update\"\n\
        if [ -f \"$state/transaction.json\" ]; then exec \"$state/recovery\" --socket \"$socket\" update recover --scheduled; fi\n\
        exec \"$exe\" --socket \"$socket\" update run\n"
    ))
}

fn systemd_path(path: &Path) -> Result<String> {
    Ok(format!(
        "\"{}\"",
        path_str(path)?.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$")
    ))
}

fn configure_job(socket: &Path, policy: update::Policy) -> Result<()> {
    let config = envfile::config_dir();
    let launcher_file = config.join("update-job.sh");
    let mac = cfg!(target_os = "macos");
    let jobs = if mac {
        envfile::home().join("Library/LaunchAgents")
    } else {
        config.parent().context("missing config home")?.join("systemd/user")
    };
    let job = jobs.join(if mac { format!("{LABEL}.plist") } else { UNIT.into() });
    let timer = jobs.join(TIMER);
    let owner = format!("exe={}", shell_quote(path_str(&service::stable_exe()?)?));
    let enabled = policy != update::Policy::Manual;
    // A second checkout or a stale PATH entry must not remove or hijack another installation's job.
    if launcher_file.try_exists()? {
        ensure!(
            fs::read_to_string(&launcher_file)?.lines().nth(2) == Some(owner.as_str()),
            "the updater job belongs to a different executable"
        );
    } else {
        ensure!(!job.try_exists()? && (mac || !timer.try_exists()?), "an unrecognized update job already exists");
        if !enabled {
            return Ok(());
        }
    }
    ensure!(!ictl_daemon::protocol::running_as_root(), "only user update jobs are supported");
    if enabled {
        write_file(&launcher_file, launcher(&service::stable_exe()?, socket, &config)?.as_bytes())?;
    }
    if mac {
        let domain = format!("gui/{}", unsafe { libc::geteuid() });
        let id = format!("{domain}/{LABEL}");
        let loaded = Command::new("launchctl").args(["print", &id]).env("LC_ALL", "C").output()?;
        let loaded = if loaded.status.success() {
            let listing = std::str::from_utf8(&loaded.stdout)?;
            let path = listing.lines().find_map(|line| line.strip_prefix('\t')?.strip_prefix("path = "));
            ensure!(path.map(Path::new) == Some(&job), "launchd loaded a different updater plist");
            true
        } else {
            false
        };
        if enabled {
            let mut xml = Vec::new();
            plist::to_writer_xml(
                &mut xml,
                &serde_json::json!({
                    "Label": LABEL, "ProgramArguments": ["/bin/sh", path_str(&launcher_file)?],
                    "StartInterval": 3600, "RunAtLoad": true,
                }),
            )?;
            if loaded && job.try_exists()? && fs::read(&job)? == xml {
                return Ok(());
            }
            if loaded {
                run("launchctl", &["bootout", &id])?;
            }
            write_file(&job, &xml)?;
            run("launchctl", &["bootstrap", &domain, path_str(&job)?])?;
        } else if loaded {
            run("launchctl", &["bootout", &id])?;
        }
    } else if enabled {
        let unit = format!(
            "[Unit]\nDescription=Check iphone-control updates\n[Service]\nType=oneshot\nExecStart=/bin/sh {}\n",
            systemd_path(&launcher_file)?
        );
        write_file(&job, unit.as_bytes())?;
        write_file(&timer, b"[Unit]\nDescription=Check iphone-control updates when due\n[Timer]\nOnActiveSec=1min\nOnUnitActiveSec=1h\n[Install]\nWantedBy=timers.target\n")?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", TIMER])?;
    } else if timer.try_exists()? {
        // Do not stop an already-running replacement. Manual prevents the next one.
        run("systemctl", &["--user", "disable", "--now", TIMER])?;
    }
    if !enabled {
        let mut files = vec![job, launcher_file];
        if !mac {
            files.push(timer);
        }
        for f in files {
            if f.try_exists()? {
                fs::remove_file(f)?;
            }
        }
        if !mac {
            run("systemctl", &["--user", "daemon-reload"])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn configuration_worker() {
        let Some(test_home) = std::env::var_os("ICTL_TEST_SCHEDULE_HOME") else { return };
        let home = Path::new(&test_home);
        assert_eq!(envfile::home(), home);
        let socket = home.join("daemon.sock");
        let lease = ictl_daemon::maintenance::exclusive().unwrap();
        assert!(configure(&socket).is_err());
        assert!(set_policy(&socket, update::Policy::Manual).is_err());
        assert!(!envfile::config_dir().join("update-job.sh").exists());
        drop(lease);

        configure_job(&socket, update::Policy::Notify).unwrap();
        configure_job(&socket, update::Policy::Notify).unwrap();
        let calls = fs::read_to_string(home.join("manager.log")).unwrap();
        assert!(!calls.contains("bootout") && !calls.contains("disable"));
        if cfg!(target_os = "macos") {
            assert_eq!(calls.matches("bootstrap").count(), 1);
        }
    }

    #[test]
    fn configuring_jobs_respects_maintenance_and_leaves_an_active_job_running() {
        if ictl_daemon::protocol::running_as_root() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let (name, script) = if cfg!(target_os = "macos") {
            (
                "launchctl",
                r#"#!/bin/sh
echo "$1" >> "$HOME/manager.log"
case "$1" in
print) [ -f "$HOME/loaded" ] || exit 1
       printf '\tpath = %s\n' "$HOME/Library/LaunchAgents/io.github.iphone-control.updates.plist" ;;
bootstrap) : > "$HOME/loaded";;
*) exit 1;;
esac
"#,
            )
        } else {
            ("systemctl", "#!/bin/sh\necho \"$*\" >> \"$HOME/manager.log\"\n")
        };
        let manager = home.join(name);
        fs::write(&manager, script).unwrap();
        fs::set_permissions(&manager, fs::Permissions::from_mode(0o700)).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "schedule::tests::configuration_worker"])
            .env("HOME", home)
            .env("ICTL_TEST_SCHEDULE_HOME", home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("PATH", home)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn launcher_can_recover_without_starting_a_broken_installed_binary() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("test's prefix");
        let state = dir.join(".iphone-control-update");
        fs::create_dir_all(&state).unwrap();
        let socket = dir.join("other socket");
        let exe = dir.join("iphone-control");
        let config = dir.join("xdg/iphone-control");
        let script = launcher(&exe, &socket, &config).unwrap();
        let set_program = |p: &Path| {
            fs::write(p, "#!/bin/sh\nprintf '%s\\n' \"$@\" \"$XDG_CONFIG_HOME\"\n").unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        };
        set_program(&exe);
        let invoke = || {
            let result = Command::new("/bin/sh").args(["-c", &script]).output().unwrap();
            assert!(result.status.success());
            String::from_utf8(result.stdout).unwrap()
        };
        assert_eq!(
            invoke(),
            format!("--socket\n{}\nupdate\nrun\n{}\n", socket.display(), config.parent().unwrap().display())
        );
        fs::write(state.join("transaction.json"), "{}").unwrap();
        fs::remove_file(exe).unwrap();
        set_program(&state.join("recovery"));
        assert!(invoke().contains("\nupdate\nrecover\n--scheduled\n"));
    }
}
