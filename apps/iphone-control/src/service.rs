//! The daemon as a per-user service: a launchd agent on macOS, a systemd user unit on Linux.
//!
//! The same service `install.sh` sets up (launchd label [`LABEL`], unit `iphone-control.service` in
//! `~/.config/systemd/user`), and also the ones a package brings: the `.deb`'s unit in `/usr/lib/systemd/user` and
//! the agent or unit `brew services` writes. `install` only writes our own file when none of these exists; the other
//! commands act on whichever is there. The service reads `~/.config/iphone-control/env`, starts at login and is
//! restarted after a crash.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, bail};

use crate::envfile::home;

pub const LABEL: &str = "io.github.derrors.iphone-control";
const UNIT: &str = "iphone-control";

#[derive(Debug, Clone, PartialEq)]
pub enum Service {
    /// launchd agent: label and plist
    Launchd { label: String, plist: PathBuf, ours: bool },
    /// systemd user unit (name without `.service`)
    Systemd { unit: String, ours: bool },
}

impl Service {
    pub fn describe(&self) -> String {
        match self {
            Self::Launchd { label, plist, ours: true } => format!("launchd {label} ({})", plist.display()),
            Self::Launchd { label, .. } => format!("launchd {label} (brew services)"),
            Self::Systemd { unit, .. } => format!("systemd --user {unit}.service"),
        }
    }
}

/// The program path to put in a service or an MCP registration: one that survives upgrades. Homebrew's real path
/// has the version in it (`…/Cellar/iphone-control/0.1.5/bin`); its `opt` link doesn't
pub fn stable_exe() -> anyhow::Result<PathBuf> {
    let exe =
        std::env::current_exe().context(ictl_l10n::tr!("找不到本程序的路径", "can't find the path of this program"))?;
    Ok(stable_path(&exe))
}

fn stable_path(exe: &Path) -> PathBuf {
    let s = exe.to_string_lossy();
    if let Some(i) = s.find("/Cellar/iphone-control/") {
        let rest = &s[i + "/Cellar/iphone-control/".len()..];
        if let Some(j) = rest.find('/') {
            return PathBuf::from(format!("{}/opt/iphone-control{}", &s[..i], &rest[j..]));
        }
    }
    exe.to_owned()
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn launch_agents() -> PathBuf {
    home().join("Library/LaunchAgents")
}

fn our_plist() -> PathBuf {
    launch_agents().join(format!("{LABEL}.plist"))
}

fn our_unit() -> PathBuf {
    crate::envfile::config_dir().parent().unwrap_or(Path::new("/")).join("systemd/user/iphone-control.service")
}

fn run(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn systemctl(args: &[&str]) -> bool {
    run(Command::new("systemctl").arg("--user").args(args))
}

fn has_systemd_user() -> bool {
    systemctl(&["show-environment"])
}

/// The service set up for this user, if any (ours first, then a package's)
pub fn find() -> Option<Service> {
    if cfg!(target_os = "macos") {
        if our_plist().is_file() {
            return Some(Service::Launchd { label: LABEL.into(), plist: our_plist(), ours: true });
        }
        // brew services: homebrew.mxcl.iphone-control.plist, or sh.brew.iphone-control.plist in newer versions
        let found = std::fs::read_dir(launch_agents()).ok()?.flatten().map(|e| e.path()).find(|p| {
            p.extension().is_some_and(|x| x == "plist")
                && p.file_stem().is_some_and(|s| s.to_string_lossy().ends_with(".iphone-control"))
        })?;
        let label = found.file_stem()?.to_string_lossy().into_owned();
        return Some(Service::Launchd { label, plist: found, ours: false });
    }
    if !has_systemd_user() {
        return None;
    }
    for unit in [UNIT, "homebrew.iphone-control"] {
        if systemctl(&["cat", unit]) {
            return Some(Service::Systemd { unit: unit.into(), ours: unit == UNIT && our_unit().is_file() });
        }
    }
    None
}

fn launchd_loaded(label: &str) -> bool {
    run(Command::new("launchctl").arg("print").arg(format!("gui/{}/{label}", uid())))
}

pub fn start(s: &Service) -> anyhow::Result<()> {
    let ok = match s {
        Service::Launchd { label, plist, .. } => {
            if launchd_loaded(label) {
                run(Command::new("launchctl").arg("kickstart").arg(format!("gui/{}/{label}", uid())))
            } else {
                run(Command::new("launchctl").arg("bootstrap").arg(format!("gui/{}", uid())).arg(plist))
            }
        }
        Service::Systemd { unit, .. } => systemctl(&["enable", "--now", unit]),
    };
    if !ok {
        bail!("{}", ictl_l10n::trf!("启动服务失败：{}", "Starting the service failed: {}", s.describe()));
    }
    Ok(())
}

pub fn stop(s: &Service) -> anyhow::Result<()> {
    let ok = match s {
        Service::Launchd { label, .. } => {
            !launchd_loaded(label)
                || run(Command::new("launchctl").arg("bootout").arg(format!("gui/{}/{label}", uid())))
        }
        Service::Systemd { unit, .. } => systemctl(&["stop", unit]),
    };
    if !ok {
        bail!("{}", ictl_l10n::trf!("停止服务失败：{}", "Stopping the service failed: {}", s.describe()));
    }
    Ok(())
}

/// Restarts it so it reads the settings again (or starts it if it isn't running)
pub fn restart(s: &Service) -> anyhow::Result<()> {
    let ok = match s {
        Service::Launchd { label, .. } if launchd_loaded(label) => {
            run(Command::new("launchctl").arg("kickstart").arg("-k").arg(format!("gui/{}/{label}", uid())))
        }
        Service::Launchd { .. } => return start(s),
        Service::Systemd { unit, .. } => systemctl(&["restart", unit]),
    };
    if !ok {
        bail!("{}", ictl_l10n::trf!("重启服务失败：{}", "Restarting the service failed: {}", s.describe()));
    }
    Ok(())
}

/// Sets up our own service for this program, unless one already exists, and starts it
pub fn install() -> anyhow::Result<Service> {
    if let Some(s) = find() {
        start(&s)?;
        return Ok(s);
    }
    let exe = stable_exe()?;
    let exe = exe.to_string_lossy();
    if exe.contains(['"', '\'', '\\', '&', '<', '>']) {
        bail!(
            "{}",
            ictl_l10n::trf!(
                "程序路径里有特殊字符，装不了服务：{exe}",
                "The program path has special characters, can't set up a service: {exe}"
            )
        );
    }
    if cfg!(target_os = "macos") {
        let logs = home().join("Library/Logs/iphone-control");
        std::fs::create_dir_all(&logs)?;
        std::fs::create_dir_all(launch_agents())?;
        let log = logs.join("daemon.log");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>-c</string>
    <string>set -a; f="$HOME/.config/iphone-control/env"; [ -f "$f" ] &amp;&amp; . "$f"; set +a; exec "{exe}" daemon</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ExitTimeOut</key><integer>30</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            log = log.display()
        );
        std::fs::write(our_plist(), plist)?;
        let s = Service::Launchd { label: LABEL.into(), plist: our_plist(), ours: true };
        start(&s)?;
        return Ok(s);
    }
    if !has_systemd_user() {
        bail!(
            "{}",
            ictl_l10n::tr!(
                "没有可用的 systemd 用户会话，装不了服务；在另一个终端运行 iphone-control daemon",
                "No systemd user session, so no service can be set up; run iphone-control daemon in another terminal"
            )
        );
    }
    let unit = our_unit();
    std::fs::create_dir_all(unit.parent().unwrap_or(Path::new("/")))?;
    std::fs::write(
        &unit,
        format!(
            "# Written by iphone-control. Settings: ~/.config/iphone-control/env\n[Unit]\n\
             Description=iphone-control daemon (iPhone over USB, MCP)\nAfter=network-online.target\n\n[Service]\n\
             ExecStart={exe} daemon\nEnvironmentFile=-%h/.config/iphone-control/env\nEnvironment=MALLOC_ARENA_MAX=2\n\
             Restart=on-failure\nRestartSec=10\nTimeoutStopSec=30\n\n[Install]\nWantedBy=default.target\n"
        ),
    )?;
    systemctl(&["daemon-reload"]);
    let s = Service::Systemd { unit: UNIT.into(), ours: true };
    start(&s)?;
    Ok(s)
}

/// Stops and removes our own service. A package's service stays; says how to remove it
pub fn uninstall() -> anyhow::Result<()> {
    let Some(s) = find() else {
        println!("{}", ictl_l10n::tr!("没有装后台服务", "No service is set up"));
        return Ok(());
    };
    match &s {
        Service::Launchd { plist, ours: true, .. } => {
            stop(&s)?;
            std::fs::remove_file(plist)?;
        }
        Service::Systemd { unit, ours: true } => {
            systemctl(&["disable", "--now", unit]);
            std::fs::remove_file(our_unit())?;
            systemctl(&["daemon-reload"]);
        }
        Service::Launchd { .. } => bail!(
            "{}",
            ictl_l10n::tr!(
                "这是 brew services 装的服务：brew services stop iphone-control",
                "This service belongs to brew services: brew services stop iphone-control"
            )
        ),
        Service::Systemd { unit, .. } => {
            systemctl(&["disable", "--now", unit]);
            println!(
                "{}",
                ictl_l10n::trf!(
                    "已停用 {unit}（服务文件属于安装包，留着）",
                    "Disabled {unit} (its file belongs to the package and stays)"
                )
            );
            return Ok(());
        }
    }
    println!("{}", ictl_l10n::trf!("已移除后台服务 {}", "Removed the service {}", s.describe()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homebrew_paths_use_the_opt_link() {
        assert_eq!(
            stable_path(Path::new("/opt/homebrew/Cellar/iphone-control/0.1.5/bin/iphone-control")),
            PathBuf::from("/opt/homebrew/opt/iphone-control/bin/iphone-control")
        );
        assert_eq!(
            stable_path(Path::new("/home/linuxbrew/.linuxbrew/Cellar/iphone-control/0.2.0/bin/iphone-control")),
            PathBuf::from("/home/linuxbrew/.linuxbrew/opt/iphone-control/bin/iphone-control")
        );
        assert_eq!(stable_path(Path::new("/usr/bin/iphone-control")), PathBuf::from("/usr/bin/iphone-control"));
    }
}
