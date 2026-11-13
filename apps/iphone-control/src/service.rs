//! The daemon as a per-user service: a launchd agent on macOS, a systemd user unit on Linux.
//!
//! The same service `install.sh` sets up (launchd label [`LABEL`], unit `iphone-control.service` in
//! `~/.config/systemd/user`), and also the ones a package brings: the `.deb`'s unit in `/usr/lib/systemd/user` and
//! the agent or unit `brew services` writes. `install` only writes our own file when none of these exists; the other
//! commands act on whichever is there. The service reads `~/.config/iphone-control/env`, starts at login and is
//! restarted after a crash.
//!
//! The control page can be a service of its own too (`iphone-control service viewer install`): launchd label
//! [`VIEWER_LABEL`], unit `iphone-control-viewer.service`, running `iphone-control viewer` with its settings from the
//! same file (`IPHONE_CONTROL_VIEWER_PORT`, `_LAN`, `_LAN_HOSTS`). It gets the `PATH` of the shell that installed
//! it, so it finds `claude` and `codex` (a user service's own `PATH` usually lacks `~/.local/bin`, npm's bin…).
//! It is an always-running client: an update stops it first and starts it again afterwards ([`pause_viewer`]).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, bail, ensure};

use crate::envfile::home;

pub const LABEL: &str = "io.github.derrors.iphone-control";
const UNIT: &str = "iphone-control";
pub const VIEWER_LABEL: &str = "io.github.derrors.iphone-control.viewer";
const VIEWER_UNIT: &str = "iphone-control-viewer";

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

pub(super) fn stable_path(exe: &Path) -> PathBuf {
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

fn viewer_plist() -> PathBuf {
    launch_agents().join(format!("{VIEWER_LABEL}.plist"))
}

fn viewer_unit_file() -> PathBuf {
    our_unit().with_file_name(format!("{VIEWER_UNIT}.service"))
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

/// Identify the user service recorded by the script installer. Package-managed services have
/// their own upgrade lifecycle, even when they use the same systemd unit name.
pub fn managed(exe: &Path, method: &str) -> anyhow::Result<Service> {
    let (file, service) = match method {
        "launchd" if cfg!(target_os = "macos") => {
            (our_plist(), Service::Launchd { label: LABEL.into(), plist: our_plist(), ours: true })
        }
        "systemd" if cfg!(target_os = "linux") => (our_unit(), Service::Systemd { unit: UNIT.into(), ours: true }),
        _ => bail!("a script-managed user service is required"),
    };
    let meta = file.symlink_metadata()?;
    ensure!(meta.is_file() && meta.uid() == uid() && meta.mode() & 0o022 == 0, "untrusted service file");
    let exe = exe.to_str().context("invalid executable path")?;
    match &service {
        Service::Launchd { label, .. } => {
            let plist = plist::Value::from_file(&file)?;
            let dict = plist.as_dictionary().context("invalid service plist")?;
            let args = dict.get("ProgramArguments").and_then(|v| v.as_array()).context("missing service arguments")?;
            let script = format!(
                r#"set -a; f="$HOME/.config/iphone-control/env"; [ -f "$f" ] && . "$f"; set +a; exec "{exe}" daemon"#
            );
            ensure!(
                dict.get("Label").and_then(|v| v.as_string()) == Some(label.as_str())
                    && !dict.contains_key("Program")
                    && (args.iter().map(|v| v.as_string()).eq([Some(exe), Some("daemon")])
                        || args.iter().map(|v| v.as_string()).eq([Some("/bin/sh"), Some("-c"), Some(script.as_str())])),
                "the launch agent does not start the recorded binary"
            );
        }
        Service::Systemd { .. } => {
            let unit = std::fs::read_to_string(&file)?;
            let mut commands = unit.lines().filter_map(|line| line.trim().strip_prefix("ExecStart="));
            let command = commands.next().context("missing ExecStart")?;
            ensure!(
                commands.next().is_none()
                    && (command == format!(r#""{exe}" daemon"#)
                        || (!exe.contains(char::is_whitespace) && command == format!("{exe} daemon"))),
                "the systemd unit does not start the recorded binary"
            );
        }
    }
    Ok(service)
}

/// The effective service owner matters during replacement. A foreground daemon can answer the
/// same socket. Reject transitional manager states and stale/overridden user service definitions.
pub fn managed_pid(service: &Service, stopping: bool) -> anyhow::Result<Option<u32>> {
    match service {
        Service::Launchd { label, plist, .. } => {
            let out = Command::new("launchctl")
                .args(["print", &format!("gui/{}/{label}", uid())])
                .env("LC_ALL", "C")
                .output()?;
            if !out.status.success() {
                ensure!(
                    String::from_utf8_lossy(&out.stderr).contains("Could not find service"),
                    "cannot query launchd"
                );
                return Ok(None);
            }
            launchd_pid(std::str::from_utf8(&out.stdout)?, plist, stopping)
        }
        Service::Systemd { unit, .. } => {
            let out = Command::new("systemctl")
                .args([
                    "--user",
                    "show",
                    unit,
                    "--all", // include an empty DropInPaths (absence is not proof of ownership)
                    "--property=ActiveState,LoadState,MainPID,FragmentPath,DropInPaths,NeedDaemonReload",
                ])
                .output()?;
            ensure!(out.status.success(), "cannot query systemd user service");
            let info = std::str::from_utf8(&out.stdout)?;
            let field = |key: &str| info.lines().find_map(|line| line.strip_prefix(key));
            ensure!(
                field("LoadState=") == Some("loaded")
                    && field("NeedDaemonReload=") == Some("no")
                    && field("FragmentPath=").map(Path::new) == Some(our_unit().as_path())
                    && field("DropInPaths=") == Some(""),
                "systemd unit is stale or overridden"
            );
            match (field("ActiveState="), field("MainPID=")) {
                (Some("inactive" | "failed"), Some("0")) => Ok(None),
                (Some("active"), Some(pid)) => {
                    Ok(Some(pid.parse::<u32>().ok().filter(|p| *p > 0).context("missing MainPID")?))
                }
                (_, Some(pid)) if stopping => Ok(pid.parse::<u32>().ok().filter(|p| *p > 0)),
                _ => bail!("systemd service is transitioning"),
            }
        }
    }
}

/// The daemon's PID from `launchctl print`: `None` when the job is loaded but not running. With
/// `KeepAlive={SuccessfulExit:false}` launchd leaves a daemon that exited cleanly (SIGTERM, `kill`) that way,
/// with no PID, for good: like an inactive systemd unit, it is stopped, not transitioning
fn launchd_pid(info: &str, plist: &Path, stopping: bool) -> anyhow::Result<Option<u32>> {
    let field = |key: &str| info.lines().find_map(|line| line.strip_prefix('\t')?.strip_prefix(key));
    ensure!(field("path = ").map(Path::new) == Some(plist), "launchd loaded a different plist");
    let pid = field("pid = ");
    if pid.is_none() && (stopping || field("state = ") == Some("not running")) {
        return Ok(None);
    }
    ensure!(stopping || field("state = ") == Some("running"), "launchd service is transitioning");
    Ok(Some(pid.context("no launchd PID")?.parse()?))
}

/// Restart exactly the service we stopped without changing the user's enable-at-login setting.
pub fn start_managed(s: &Service) -> anyhow::Result<()> {
    if let Service::Systemd { unit, .. } = s {
        ensure!(systemctl(&["start", unit]), "could not start systemd user service");
        Ok(())
    } else {
        start(s)
    }
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

/// The control page's service, if it is set up (no package brings one: only ours)
pub fn viewer_find() -> Option<Service> {
    if cfg!(target_os = "macos") {
        return viewer_plist().is_file().then(|| Service::Launchd {
            label: VIEWER_LABEL.into(),
            plist: viewer_plist(),
            ours: true,
        });
    }
    viewer_unit_file().is_file().then(|| Service::Systemd { unit: VIEWER_UNIT.into(), ours: true })
}

/// The service's process is running now
pub fn running(s: &Service) -> bool {
    match s {
        Service::Systemd { unit, .. } => systemctl(&["is-active", "--quiet", unit]),
        Service::Launchd { label, .. } => Command::new("launchctl")
            .arg("print")
            .arg(format!("gui/{}/{label}", uid()))
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("state = running")),
    }
}

/// `PATH` for the control page's service: the installing shell's, so the agents are found. Characters that can't
/// be written into a unit or a plist safely are refused
fn service_path(path: &str) -> anyhow::Result<&str> {
    ensure!(
        !path.is_empty() && !path.contains(['"', '\\', '\n', '\r', '\'', '`', '$']),
        "{}",
        ictl_l10n::tr!(
            "PATH 里有服务文件写不了的字符（引号、反斜杠、$、换行），装不了控制台服务",
            "PATH has characters a service file can't hold (quotes, backslashes, $, newlines), so the viewer service can't be set up"
        )
    );
    Ok(path)
}

/// The control page's systemd unit
fn viewer_unit_text(exe: &str, path: &str) -> anyhow::Result<String> {
    // systemd expands %-specifiers in Environment=
    let path = service_path(path)?.replace('%', "%%");
    Ok(format!(
        "# Written by iphone-control. Settings: ~/.config/iphone-control/env (IPHONE_CONTROL_VIEWER_*)\n[Unit]\n\
         Description=iphone-control viewer (control page)\nAfter=iphone-control.service\n\n[Service]\n\
         ExecStart={exe} viewer\nEnvironmentFile=-%h/.config/iphone-control/env\nEnvironment=\"PATH={path}\"\n\
         Restart=on-failure\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n"
    ))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The control page's launch agent
fn viewer_plist_text(exe: &str, path: &str, log: &Path) -> anyhow::Result<String> {
    let path = xml(service_path(path)?);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{VIEWER_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>-c</string>
    <string>set -a; f="$HOME/.config/iphone-control/env"; [ -f "$f" ] &amp;&amp; . "$f"; set +a; exec "{exe}" viewer</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>{path}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe = xml(exe),
        log = xml(&log.to_string_lossy())
    ))
}

/// Sets up the control page's service (or rewrites it, e.g. for a new `PATH`) and starts it
pub fn viewer_install() -> anyhow::Result<Service> {
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
    let path = std::env::var("PATH").unwrap_or_default();
    if let Some(s) = viewer_find() {
        // rewritten below with the current PATH and program; stop the old one first
        let _ = stop(&s);
    }
    if cfg!(target_os = "macos") {
        let logs = home().join("Library/Logs/iphone-control");
        std::fs::create_dir_all(&logs)?;
        std::fs::create_dir_all(launch_agents())?;
        std::fs::write(viewer_plist(), viewer_plist_text(&exe, &path, &logs.join("viewer.log"))?)?;
        let s = Service::Launchd { label: VIEWER_LABEL.into(), plist: viewer_plist(), ours: true };
        start(&s)?;
        return Ok(s);
    }
    if !has_systemd_user() {
        bail!(
            "{}",
            ictl_l10n::tr!(
                "没有可用的 systemd 用户会话，装不了服务；在另一个终端运行 iphone-control viewer",
                "No systemd user session, so no service can be set up; run iphone-control viewer in another terminal"
            )
        );
    }
    let unit = viewer_unit_file();
    std::fs::create_dir_all(unit.parent().unwrap_or(Path::new("/")))?;
    std::fs::write(&unit, viewer_unit_text(&exe, &path)?)?;
    systemctl(&["daemon-reload"]);
    let s = Service::Systemd { unit: VIEWER_UNIT.into(), ours: true };
    start(&s)?;
    Ok(s)
}

/// Stops and removes the control page's service
pub fn viewer_uninstall() -> anyhow::Result<()> {
    let Some(s) = viewer_find() else {
        println!("{}", ictl_l10n::tr!("没有装控制台服务", "No viewer service is set up"));
        return Ok(());
    };
    match &s {
        Service::Launchd { plist, .. } => {
            stop(&s)?;
            std::fs::remove_file(plist)?;
        }
        Service::Systemd { unit, .. } => {
            systemctl(&["disable", "--now", unit]);
            std::fs::remove_file(viewer_unit_file())?;
            systemctl(&["daemon-reload"]);
        }
    }
    println!("{}", ictl_l10n::trf!("已移除控制台服务 {}", "Removed the viewer service {}", s.describe()));
    Ok(())
}

/// For an update: stops the control page's service if it is running (it is an `iphone-control` process holding a
/// client lease, so the update would wait for it forever) and waits until it is gone. `Some` to start it again
/// with [`resume_viewer`]
pub fn pause_viewer() -> anyhow::Result<Option<Service>> {
    let Some(s) = viewer_find().filter(running) else { return Ok(None) };
    stop(&s)?;
    for _ in 0..50 {
        if !running(&s) {
            return Ok(Some(s));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let _ = start(&s);
    bail!("busy: the viewer service didn't stop")
}

/// Starts the control page's service again after [`pause_viewer`]; a failure is only reported
pub fn resume_viewer(paused: Option<Service>) {
    if let Some(s) = paused
        && let Err(e) = start(&s)
    {
        eprintln!("{e:#}");
    }
}

/// After a package manager replaced the program: a running control page service runs the new one
pub fn restart_viewer_if_running() {
    if let Some(s) = viewer_find().filter(running) {
        match restart(&s) {
            Ok(()) => println!("{}", ictl_l10n::tr!("控制台服务已重启。", "The viewer service was restarted.")),
            Err(e) => eprintln!("{e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `launchctl print` as macOS 27 shows it for a job with KeepAlive={SuccessfulExit:false}: running, and
    /// after the program exited with status 0 (loaded, not running, no PID)
    #[test]
    fn launchd_job_that_exited_cleanly_is_stopped() {
        let plist = Path::new("/Users/u/Library/LaunchAgents/io.github.derrors.iphone-control.plist");
        let print = |state: &str, pid: Option<u32>| {
            let pid = pid.map(|p| format!("\tpid = {p}\n")).unwrap_or_default();
            format!(
                "gui/501/io.github.derrors.iphone-control = {{\n\tpath = {}\n\tstate = {state}\n{pid}\tlast exit code = 0\n}}\n",
                plist.display()
            )
        };
        assert_eq!(launchd_pid(&print("running", Some(4242)), plist, false).unwrap(), Some(4242));
        assert_eq!(launchd_pid(&print("not running", None), plist, false).unwrap(), None, "exited cleanly");
        assert_eq!(launchd_pid(&print("not running", None), plist, true).unwrap(), None);
        assert!(launchd_pid(&print("spawn scheduled", None), plist, false).is_err(), "really transitioning");
        assert!(launchd_pid(&print("running", Some(1)), Path::new("/elsewhere.plist"), false).is_err());
    }

    #[test]
    fn the_viewer_service_runs_the_viewer_with_this_path() {
        let path = "/home/u/.local/bin:/home/u/.npm-global/bin:/usr/bin";
        let unit = viewer_unit_text("/home/u/.local/bin/iphone-control", path).unwrap();
        assert!(unit.contains("ExecStart=/home/u/.local/bin/iphone-control viewer\n"), "{unit}");
        assert!(unit.contains(&format!("Environment=\"PATH={path}\"\n")));
        assert!(unit.contains("EnvironmentFile=-%h/.config/iphone-control/env"));
        assert!(unit.contains("After=iphone-control.service"));
        assert!(
            viewer_unit_text("x", "/odd%dir:/usr/bin").unwrap().contains("PATH=/odd%%dir:/usr/bin"),
            "not a specifier"
        );
        for bad in ["/a\"b", "/a\\b", "/a$HOME", "/a\nb", "/a`b`", ""] {
            assert!(viewer_unit_text("x", bad).is_err(), "{bad:?}");
        }
        let plist = viewer_plist_text(
            "/opt/homebrew/opt/iphone-control/bin/iphone-control",
            "/a&b:/usr/bin",
            Path::new("/L/viewer.log"),
        )
        .unwrap();
        assert!(plist.contains(r#"exec "/opt/homebrew/opt/iphone-control/bin/iphone-control" viewer</string>"#));
        assert!(plist.contains("<key>PATH</key><string>/a&amp;b:/usr/bin</string>"));
        assert!(plist.contains(&format!("<string>{VIEWER_LABEL}</string>")));
        assert!(plist::Value::from_reader_xml(plist.as_bytes()).is_ok(), "a valid plist");
    }

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
