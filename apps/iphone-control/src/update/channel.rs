//! Operator-initiated upgrades for verified package-manager installations. The Debian package is installed only
//! after its publisher-signed manifest (`<deb>.manifest.json` and `.sig`, see `archive.rs`) and the downloaded
//! bytes match: APT runs the package's scripts as root. After the package manager has replaced the program, a
//! background service that was running is restarted so it runs the new version, and checked to answer with it.
use std::fs::File;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use ictl_l10n::tr;
use tokio::process::Command;

use super::{Availability, InstallMethod, Report, archive, deb_asset, digest, install, publisher, release, tap};

/// Where the .deb puts its systemd user unit (`deploy/deb/iphone-control.service`)
const DEB_UNIT: &str = "/usr/lib/systemd/user/iphone-control.service";

pub(super) async fn apply(socket: &Path, report: &Report) -> Result<()> {
    let exe = report.executable.as_deref().context("missing installation path")?;
    let latest = report.available_version.as_deref().context("missing release version")?;
    ensure!(release::stable_tag(&format!("v{latest}")).is_some(), "invalid stable release version");
    match (report.install_method, report.availability) {
        (InstallMethod::Homebrew, Availability::BrewCheck) => {
            let cellar = exe.ancestors().nth(4).context("missing Homebrew Cellar")?;
            let tap = tap().context("missing Homebrew tap")?;
            let brew = install::brew_program(cellar).context("cannot locate the Homebrew executable")?;
            if homebrew(&brew, cellar, tap, latest).await? {
                let running = brew_service_running(&brew).await;
                restart(
                    socket,
                    latest,
                    running,
                    &[brew.as_os_str(), "services".as_ref(), "restart".as_ref(), "iphone-control".as_ref()],
                    "brew services restart iphone-control",
                )
                .await;
            }
            Ok(())
        }
        (InstallMethod::Debian, Availability::Available) => {
            let repo = publisher().context("missing release publisher")?;
            let target = super::target().context("unsupported Debian architecture")?;
            if debian(repo, target, latest, exe).await? {
                let running = deb_service_running().await;
                restart(
                    socket,
                    latest,
                    running,
                    &["systemctl".as_ref(), "--user".as_ref(), "restart".as_ref(), "iphone-control".as_ref()],
                    "systemctl --user restart iphone-control",
                )
                .await;
            }
            Ok(())
        }
        _ => bail!("{}", report.guidance),
    }
}

/// The .deb's systemd user service is running (only that unit: not a service the user wrote elsewhere)
async fn deb_service_running() -> bool {
    let Ok(out) = Command::new("systemctl")
        .args(["--user", "show", "iphone-control", "-p", "ActiveState", "-p", "FragmentPath"])
        .env("LC_ALL", "C")
        .stderr(Stdio::null())
        .output()
        .await
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    out.status.success()
        && text.lines().any(|l| l == "ActiveState=active")
        && text.lines().any(|l| l == format!("FragmentPath={DEB_UNIT}"))
}

/// `brew services` runs the formula's service. Older Homebrew without `services info --json` counts as not running
async fn brew_service_running(brew: &Path) -> bool {
    let Ok(out) = Command::new(brew)
        .args(["services", "info", "iphone-control", "--json"])
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .stderr(Stdio::null())
        .output()
        .await
    else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| v.get(0)?.get("running")?.as_bool())
        .unwrap_or(false)
}

/// Restarts the background service if it was `running` and waits until the daemon answers with `version`. Failing
/// that only prints what to run: the new program is installed either way
async fn restart(socket: &Path, version: &str, running: bool, cmd: &[&std::ffi::OsStr], by_hand: &str) {
    if !running {
        println!(
            "{}",
            ictl_l10n::tr!(
                "后台服务没有在运行，不用重启。Viewer 和 MCP 客户端下次启动时用新版本。",
                "The background service isn't running, so nothing to restart. Viewer and MCP clients use the new version when they next start."
            )
        );
        return;
    }
    println!("{}", ictl_l10n::tr!("正在重启后台服务…", "Restarting the background service…"));
    let restarted = match Command::new(cmd[0]).args(&cmd[1..]).status().await {
        Ok(s) if s.success() => wait_for_daemon(socket, version, Duration::from_secs(60)).await,
        _ => false,
    };
    if restarted {
        println!(
            "{}",
            ictl_l10n::trf!(
                "后台服务已在运行 {version}。重新打开 Viewer、重连 MCP 客户端后它们也会用新版本。",
                "The background service now runs {version}. Reopen Viewer and reconnect MCP clients to use it there too."
            )
        );
    } else {
        println!(
            "{}",
            ictl_l10n::trf!(
                "新版本已经装好，但后台服务没能确认重启成功。请运行：{by_hand}",
                "The new version is installed, but the background service couldn't be confirmed to restart. Run: {by_hand}"
            )
        );
    }
}

async fn wait_for_daemon(socket: &Path, version: &str, within: Duration) -> bool {
    let end = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < end {
        if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(2), crate::snapshot(socket)).await
            && s.version.as_deref() == Some(version)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

async fn run(cmd: &mut Command) -> Result<()> {
    let status =
        cmd.status().await.with_context(|| format!("starting {}", cmd.as_std().get_program().to_string_lossy()))?;
    ensure!(status.success(), "{}: {status}", cmd.as_std().get_program().to_string_lossy());
    Ok(())
}

async fn output(cmd: &mut Command) -> Result<String> {
    let result = cmd
        .stderr(Stdio::inherit())
        .output()
        .await
        .with_context(|| format!("starting {}", cmd.as_std().get_program().to_string_lossy()))?;
    ensure!(result.status.success(), "{}: {}", cmd.as_std().get_program().to_string_lossy(), result.status);
    Ok(String::from_utf8(result.stdout)?)
}

/// True when it upgraded (false: Homebrew already had `latest`)
async fn homebrew(brew: &Path, cellar: &Path, tap: &str, latest: &str) -> Result<bool> {
    let formula = format!("{tap}/iphone-control");
    let active = cellar.parent().context("invalid Homebrew prefix")?.join("opt/iphone-control/bin/iphone-control");
    let installed = || {
        active
            .canonicalize()
            .ok()
            .and_then(|exe| install::brew_keg(&exe, cellar, tap))
            .context("Homebrew's active keg does not match this installation and tap")
    };
    let before = semver::Version::parse(&installed()?)?;
    if before >= semver::Version::parse(latest)? {
        println!("{}", ictl_l10n::trf!("Homebrew 当前版本为 {before}。", "Homebrew already has {before}."));
        return Ok(false);
    }
    run(Command::new(brew).arg("update")).await?;
    // Use exactly the metadata refreshed above for the check and upgrade.
    let cmd = || {
        let mut c = Command::new(brew);
        c.env("HOMEBREW_NO_AUTO_UPDATE", "1");
        c
    };
    let info = output(cmd().args(["info", "--formula", "--json=v2", &formula])).await?;
    let info: serde_json::Value = serde_json::from_str(&info)?;
    let f = &info["formulae"][0];
    ensure!(
        f["full_name"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(&formula)),
        "Homebrew formula identity changed"
    );
    ensure!(f["pinned"] == false, "{}", tr!("Homebrew 已固定此版本。", "This formula is pinned in Homebrew."));
    ensure!(
        f["versions"]["stable"] == latest,
        "{}",
        ictl_l10n::trf!(
            "Homebrew tap 尚未提供目标版本 {latest}。",
            "The Homebrew tap does not offer version {latest} yet."
        )
    );
    run(cmd().args(["upgrade", "--formula", &formula])).await?;
    ensure!(installed()? == latest, "Homebrew did not activate {latest}");
    println!("{}", ictl_l10n::trf!("Homebrew 已升级至 {latest}。", "Homebrew upgraded to {latest}."));
    Ok(true)
}

fn check_deb_metadata<'a>(fields: &'a str, version: &str, arch: &str) -> Result<&'a str> {
    let mut lines = fields.lines();
    let (name, package_version, architecture) = (lines.next(), lines.next(), lines.next());
    let package_version = package_version.context("missing Debian package version")?;
    // cargo-deb adds its Debian revision to the upstream package version.
    ensure!(
        name == Some("iphone-control")
            && architecture == Some(arch)
            && lines.next().is_none()
            && (package_version == version || package_version.strip_suffix("-1") == Some(version)),
        "Debian package metadata does not match iphone-control {version} {arch}"
    );
    Ok(package_version)
}

async fn inspect_deb(path: &Path, latest: &str, arch: &str) -> Result<String> {
    let fields = output(
        Command::new("/usr/bin/dpkg-deb")
            .args(["--showformat=${Package}\\n${Version}\\n${Architecture}\\n", "--show"])
            .arg(path),
    )
    .await?;
    Ok(check_deb_metadata(&fields, latest, arch)?.to_owned())
}

async fn check_apt_candidate(apt_cache: &mut Command, path: &Path, deb_version: &str) -> Result<()> {
    // A direct .deb argument selects that version explicitly; also honor local
    // APT priorities as if the verified file were a package source.
    let policy =
        output(apt_cache.env("LC_ALL", "C").arg("--with-source").arg(path).args(["policy", "iphone-control"])).await?;
    let candidate = policy.lines().find_map(|line| line.trim().strip_prefix("Candidate: "));
    ensure!(
        candidate == Some(deb_version),
        "{}",
        ictl_l10n::trf!("APT 本机策略未选择 {deb_version}。", "Local APT policy did not select {deb_version}.")
    );
    Ok(())
}

/// True when it upgraded (false: APT already had `latest`)
async fn debian(repo: &str, target: &str, latest: &str, exe: &Path) -> Result<bool> {
    let asset = deb_asset(latest, target).context("unsupported Debian architecture")?;
    let arch = asset.rsplit_once('_').and_then(|(_, s)| s.strip_suffix(".deb")).context("invalid Debian asset name")?;
    let client = archive::client()?;
    let signed = archive::signed_manifest(&client, repo, target, latest, &asset).await?;
    let temp = tempfile::tempdir()?;
    let path = temp.path().canonicalize()?.join(&asset);
    let url = format!("https://github.com/{repo}/releases/download/v{latest}/{asset}");
    archive::download(&client, &url, signed.size, File::create_new(&path)?).await?;
    ensure!(
        path.metadata()?.len() == signed.size && digest(&path)? == signed.sha256,
        "Debian package does not match its signed manifest"
    );
    let deb_version = inspect_deb(&path, latest, arch).await?;
    let current = install::detect(Some(exe));
    ensure!(current.method == InstallMethod::Debian, "Debian package ownership changed");
    let current = semver::Version::parse(&current.version.context("missing installed Debian version")?)?;
    if current >= semver::Version::parse(latest)? {
        println!("{}", ictl_l10n::trf!("APT 当前版本为 {current}。", "APT already has {current}."));
        return Ok(false);
    }
    let held = output(Command::new("/usr/bin/apt-mark").args(["showhold", "iphone-control"])).await?;
    ensure!(held.trim().is_empty(), "{}", tr!("APT 已锁定 iphone-control。", "iphone-control is on hold in APT."));

    check_apt_candidate(&mut Command::new("/usr/bin/apt-cache"), &path, &deb_version).await?;
    // Make APT reject late downgrades, removals or hold changes while it owns
    // its locks, even if the package state changed during the sudo prompt.
    run(Command::new("/usr/bin/sudo")
        .args([
            "/usr/bin/apt-get",
            "install",
            "--only-upgrade",
            "--assume-yes",
            "--no-remove",
            "-o",
            "APT::Get::allow-downgrades=false",
            "-o",
            "APT::Get::allow-change-held-packages=false",
            "-o",
            "APT::Ignore-Hold=false",
            "-o",
            "APT::Get::force-yes=false",
        ])
        .arg(&path))
    .await?;
    let installed = install::detect(Some(exe));
    ensure!(
        installed.method == InstallMethod::Debian && installed.version.as_deref() == Some(latest),
        "APT did not install {latest}"
    );
    println!("{}", ictl_l10n::trf!("APT 已升级至 {latest}。", "APT upgraded to {latest}."));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn brew_upgrades_the_exact_active_formula_only_after_the_tap_is_ready() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().canonicalize().unwrap();
        let cellar = prefix.join("Cellar");
        let rack = cellar.join("iphone-control");
        let tap = "example/tools";
        for version in ["1.2.2", "1.2.3"] {
            let keg = rack.join(version);
            std::fs::create_dir_all(keg.join("bin")).unwrap();
            std::fs::write(keg.join("bin/iphone-control"), b"").unwrap();
            std::fs::write(keg.join("INSTALL_RECEIPT.json"), br#"{"source":{"tap":"example/tools"}}"#).unwrap();
        }
        std::fs::create_dir(prefix.join("opt")).unwrap();
        let active = prefix.join("opt/iphone-control");
        symlink(rack.join("1.2.2"), &active).unwrap();
        let brew = prefix.join("brew");
        std::fs::write(
            &brew,
            r#"#!/bin/sh
set -eu
cd "$(dirname "$0")"
case "$*" in
    update) ;;
    "info --formula --json=v2 example/tools/iphone-control") cat info ;;
    "upgrade --formula example/tools/iphone-control")
        touch attempted
        test -f allow || exit 17
        rm opt/iphone-control
        ln -s "$PWD/Cellar/iphone-control/1.2.3" opt/iphone-control ;;
    *) exit 12 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&brew, std::fs::Permissions::from_mode(0o755)).unwrap();
        let set_info = |formula, version, pinned| {
            let info =
                serde_json::json!({"formulae":[{"full_name":formula, "versions":{"stable":version}, "pinned":pinned}]});
            std::fs::write(prefix.join("info"), info.to_string()).unwrap();
        };
        for (name, version, pinned) in [
            ("other/tools/iphone-control", "1.2.3", false),
            ("example/tools/iphone-control", "1.2.2", false),
            ("example/tools/iphone-control", "1.2.3", true),
        ] {
            set_info(name, version, pinned);
            assert!(homebrew(&brew, &cellar, tap, "1.2.3").await.is_err());
            assert!(!prefix.join("attempted").exists());
        }
        set_info("example/tools/iphone-control", "1.2.3", false);
        let err = homebrew(&brew, &cellar, tap, "1.2.3").await.unwrap_err();
        assert!(err.to_string().contains("17"));
        std::fs::write(prefix.join("allow"), b"").unwrap();
        assert!(homebrew(&brew, &cellar, tap, "1.2.3").await.unwrap(), "upgraded");
        assert_eq!(active.canonicalize().unwrap(), rack.join("1.2.3"));
        assert!(!homebrew(&brew, &cellar, tap, "1.2.3").await.unwrap(), "already there: nothing to restart");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn inspect_a_real_deb_archive_before_install() {
        let dpkg = Path::new("/usr/bin/dpkg-deb");
        // Non-Debian Linux hosts need no Debian tools.
        if !dpkg.exists() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("pkg");
        let data = root.join("DEBIAN");
        std::fs::create_dir_all(&data).unwrap();
        let arch = output(Command::new("/usr/bin/dpkg").arg("--print-architecture")).await.unwrap();
        let arch = arch.trim();
        let fields = format!(
            "Package: iphone-control\nVersion: 1.2.3-1\nArchitecture: {arch}\nMaintainer: Test <test@example.org>\nDescription: Local update test\n"
        );
        std::fs::write(data.join("control"), fields).unwrap();
        let path = temp.path().join(format!("iphone-control_1.2.3_{arch}.deb"));
        run(Command::new(dpkg).arg("--build").arg(&root).arg(&path)).await.unwrap();
        assert_eq!(inspect_deb(&path, "1.2.3", arch).await.unwrap(), "1.2.3-1");
        assert!(inspect_deb(&path, "1.3.0", arch).await.is_err());
        assert!(inspect_deb(&path, "1.2.3", "wrong-arch").await.is_err());

        // Use APT's actual policy engine with an empty package inventory. A
        // negative version pin must win even though we supply a valid local file.
        let empty = temp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let prefs = temp.path().join("preferences");
        std::fs::write(&prefs, b"").unwrap();
        let policy = || {
            let mut cmd = Command::new("/usr/bin/apt-cache");
            for option in [
                "Dir::State::status=/dev/null".to_owned(),
                "Dir::Etc::sourcelist=/dev/null".to_owned(),
                format!("Dir::Etc::sourceparts={}", empty.display()),
                format!("Dir::Etc::preferences={}", prefs.display()),
                format!("Dir::Etc::preferencesparts={}", empty.display()),
            ] {
                cmd.arg("-o").arg(option);
            }
            cmd
        };
        check_apt_candidate(&mut policy(), &path, "1.2.3-1").await.unwrap();
        std::fs::write(&prefs, "Package: iphone-control\nPin: version 1.2.3-1\nPin-Priority: -1\n").unwrap();
        assert!(check_apt_candidate(&mut policy(), &path, "1.2.3-1").await.is_err());
    }

    #[test]
    fn deb_metadata_must_match_the_release_and_host() {
        let fields = "iphone-control\n1.2.3-1\narm64\n";
        check_deb_metadata(fields, "1.2.3", "arm64").unwrap();
        for bad in [
            fields.replace("iphone-control", "other"),
            fields.replace("1.2.3", "1.2.2"),
            fields.replace("arm64", "amd64"),
        ] {
            assert!(check_deb_metadata(&bad, "1.2.3", "arm64").is_err());
        }
    }
}
