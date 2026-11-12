//! Identify the executable the caller is running, independently of other installs on PATH.
use std::fs::{self, File, Metadata};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{InstallMethod, publisher, target};
use crate::envfile;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub schema: u32,
    pub publisher: String,
    pub target: String,
    pub binary_path: PathBuf,
    pub service: String,
    pub version: String,
    pub sha256: String,
    pub pinned: bool,
}

pub fn receipt_path() -> PathBuf {
    envfile::config_dir().join("install-receipt.json")
}

pub fn read_receipt() -> Result<Receipt> {
    let file = File::options().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(receipt_path()).context("No readable script installation receipt; run iphone-control update check for the upgrade instructions for this installation")?;
    let meta = file.metadata()?;
    ensure!(user_owned(&meta) && meta.is_file() && meta.len() < 16 * 1024, "invalid installation receipt file");
    let r: Receipt = serde_json::from_reader(file)?;
    ensure!(
        r.schema == 1 && Some(r.publisher.as_str()) == publisher() && Some(r.target.as_str()) == target(),
        "receipt does not match this build"
    );
    ensure!(matches!(r.service.as_str(), "none" | "launchd" | "systemd"), "invalid service in receipt");
    ensure!(
        r.binary_path.is_absolute() && r.binary_path.file_name().is_some_and(|n| n == "iphone-control"),
        "invalid binary path"
    );
    ensure!(Version::parse(&r.version).is_ok(), "invalid receipt version");
    ensure!(
        r.sha256.len() == 64 && r.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid binary digest"
    );
    Ok(r)
}

fn user_owned(meta: &Metadata) -> bool {
    let uid = unsafe { libc::geteuid() };
    uid != 0 && meta.uid() == uid && meta.mode() & 0o022 == 0
}

fn receipt_file(r: &Receipt, exe: &Path) -> Result<File> {
    ensure!(
        exe.canonicalize()? == r.binary_path && r.binary_path.canonicalize()? == r.binary_path,
        "receipt belongs to another executable"
    );
    let parent = r.binary_path.parent().context("missing install directory")?;
    ensure!(user_owned(&fs::metadata(parent)?), "unsafe install directory ownership or permissions");
    let file = File::options().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&r.binary_path)?;
    let meta = file.metadata()?;
    ensure!(meta.is_file() && user_owned(&meta) && meta.mode() & 0o100 != 0, "unsafe installed executable");
    Ok(file)
}

fn hash(mut file: File) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn digest(path: &Path) -> Result<String> {
    hash(File::options().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?)
}

pub fn validate_receipt(receipt: &Receipt, expected_exe: &Path) -> Result<()> {
    ensure!(
        hash(receipt_file(receipt, expected_exe)?)? == receipt.sha256,
        "installed binary digest differs from receipt"
    );
    Ok(())
}

// The settings card polls frequently. Reuse a successful hash of the same unchanged inode
// for display only. Installation transactions always rehash the open file with validate_receipt.
fn matches_for_display(receipt: &Receipt, exe: &Path) -> Result<bool> {
    type FileStamp = (u64, u64, u64, i64, i64);
    static LAST: Mutex<Option<(FileStamp, String)>> = Mutex::new(None);
    let file = receipt_file(receipt, exe)?;
    let meta = file.metadata()?;
    let stamp = (meta.dev(), meta.ino(), meta.len(), meta.ctime(), meta.ctime_nsec());
    let mut value = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, digest)) = value.as_ref().filter(|(s, _)| *s == stamp) {
        return Ok(*digest == receipt.sha256);
    }
    let digest = hash(file)?;
    let matched = digest == receipt.sha256;
    *value = Some((stamp, digest));
    Ok(matched)
}

pub(super) struct Installation {
    pub method: InstallMethod,
    pub version: Option<String>,
    pub pinned: Option<bool>,
}

/// Cargo records the source directory, binary names and build options beside its bin directory.
/// A development executable copied elsewhere has no corresponding installation record.
pub(super) fn cargo_source(exe: &Path, manifest_dir: &Path) -> Option<(String, String)> {
    if exe.file_name()? != "iphone-control" || exe.parent()?.file_name()? != "bin" {
        return None;
    }
    let root = exe.parent()?.parent()?;
    let source = manifest_dir.canonicalize().ok()?;
    if !source.join("Cargo.toml").is_file() {
        return None;
    }
    let data: serde_json::Value = serde_json::from_reader(File::open(root.join(".crates2.json")).ok()?).ok()?;
    let mut owners = data["installs"].as_object()?.iter().filter(|(_, entry)| {
        entry["bins"].as_array().is_some_and(|bins| bins.iter().any(|bin| bin.as_str() == Some("iphone-control")))
    });
    let (id, entry) = owners.next()?;
    if owners.next().is_some() {
        return None;
    }
    let (version, url) = id.strip_prefix("iphone-control ")?.strip_suffix(')')?.split_once(" (path+")?;
    let recorded_source = reqwest::Url::parse(url).ok()?.to_file_path().ok()?;
    if recorded_source.canonicalize().ok()? != source {
        return None;
    }
    let version = Version::parse(version).ok()?.to_string();
    let mut command = format!(
        "cd {} && cargo install --locked --path . --root {}",
        crate::shell_quote(source.to_str()?),
        crate::shell_quote(root.to_str()?),
    );
    match entry["profile"].as_str()? {
        "release" => {}
        "debug" | "dev" => command.push_str(" --debug"),
        profile => command.push_str(&format!(" --profile {}", crate::shell_quote(profile))),
    }
    if entry["all_features"].as_bool()? {
        command.push_str(" --all-features");
    }
    if entry["no_default_features"].as_bool()? {
        command.push_str(" --no-default-features");
    }
    let features = entry["features"].as_array()?.iter().map(|f| f.as_str()).collect::<Option<Vec<_>>>()?;
    if !features.is_empty() {
        command.push_str(&format!(" --features {}", crate::shell_quote(&features.join(","))));
    }
    let target = entry["target"].as_str()?;
    let host = entry["rustc"].as_str()?.lines().find_map(|line| line.strip_prefix("host: "))?;
    if target != host {
        command.push_str(&format!(" --target {}", crate::shell_quote(target)));
    }
    Some((version, command))
}

pub(super) fn brew_keg(exe: &Path, cellar: &Path, tap: &str) -> Option<String> {
    let version = exe.strip_prefix(cellar.join("iphone-control")).ok()?.components().next()?.as_os_str();
    let keg = cellar.join("iphone-control").join(version);
    if exe != keg.join("bin/iphone-control") {
        return None;
    }
    let receipt: serde_json::Value =
        serde_json::from_reader(File::open(keg.join("INSTALL_RECEIPT.json")).ok()?).ok()?;
    if !receipt.pointer("/source/tap")?.as_str()?.eq_ignore_ascii_case(tap) {
        return None;
    }
    // Homebrew may append a formula revision (_1) to the keg name.
    Some(Version::parse(version.to_str()?.split('_').next()?).ok()?.to_string())
}

pub(super) fn brew_program(cellar: &Path) -> Option<PathBuf> {
    // GUI processes need not inherit Homebrew's PATH. Only use a neighboring
    // executable for known Homebrew prefixes; custom installations use the host PATH.
    match cellar.to_str()? {
        "/opt/homebrew/Cellar" | "/usr/local/Cellar" | "/home/linuxbrew/.linuxbrew/Cellar" => {
            return Some(cellar.parent()?.join("bin/brew"));
        }
        _ => {}
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).filter(|p| p.is_absolute()).map(|p| p.join("brew")).find(|p| p.is_file())
}

fn brew_installation(exe: &Path, tap: &str) -> Option<String> {
    // First match the exact keg and its tap receipt. Never start a package manager
    // for a source checkout, a plain copy, or an installation in another tap.
    let cellar = exe.ancestors().nth(4)?;
    let version = brew_keg(exe, cellar, tap)?;
    static CELLAR: OnceLock<Option<PathBuf>> = OnceLock::new();
    let actual = CELLAR.get_or_init(|| {
        let brew = brew_program(cellar)?;
        let output = Command::new(brew).arg("--cellar").env("HOMEBREW_NO_AUTO_UPDATE", "1").output().ok()?;
        if !output.status.success() {
            return None;
        }
        Path::new(std::str::from_utf8(&output.stdout).ok()?.trim()).canonicalize().ok()
    });
    if actual.as_deref() != Some(cellar) {
        return None;
    }
    // The old Viewer may still run from a retained keg after Homebrew relinks opt.
    let active = cellar.parent()?.join("opt/iphone-control/bin/iphone-control");
    Some(active.canonicalize().ok().and_then(|p| brew_keg(&p, cellar, tap)).unwrap_or(version))
}

fn deb_package(exe: &Path) -> Option<String> {
    if !cfg!(target_os = "linux") || exe != Path::new("/usr/bin/iphone-control") {
        return None;
    }
    let dpkg = |args: &[&str]| {
        let output = Command::new("/usr/bin/dpkg-query").args(args).output().ok()?;
        output.status.success().then(|| String::from_utf8(output.stdout).ok())?
    };
    let ownership = dpkg(&["-S", "/usr/bin/iphone-control"])?;
    let (owner, file) = ownership.trim().split_once(": ")?;
    if owner.split(':').next()? != "iphone-control" || file != "/usr/bin/iphone-control" {
        return None;
    }
    let installed = dpkg(&["-W", "-f=${db:Status-Abbrev} ${Version}", owner])?;
    let version = installed.strip_prefix("ii  ").or_else(|| installed.strip_prefix("hi  "))?.trim();
    let upstream = version.rsplit_once('-').map_or(version, |(upstream, _)| upstream);
    Some(Version::parse(upstream).ok()?.to_string())
}

/// Linux marks /proc/self/exe as deleted when Cargo, dpkg or an installer atomically
/// replaces the binary under a running Viewer. Inspect the replacement at its original path.
pub(super) fn installed_executable(running: &Path) -> Option<PathBuf> {
    running.canonicalize().ok().or_else(|| {
        if cfg!(target_os = "linux") && running.file_name()? == "iphone-control (deleted)" {
            running.with_file_name("iphone-control").canonicalize().ok()
        } else {
            None
        }
    })
}

pub(super) fn detect(exe: Option<&Path>) -> Installation {
    let unknown = |method| Installation { method, version: None, pinned: None };
    if publisher().is_none() {
        return unknown(InstallMethod::Development);
    }
    let Some(exe) = exe else { return unknown(InstallMethod::Unknown) };
    if unsafe { libc::geteuid() } == 0 {
        return unknown(InstallMethod::Unknown);
    }
    if let Ok(r) = read_receipt() {
        if matches_for_display(&r, exe).unwrap_or(false) {
            let method = match r.service.as_str() {
                "none" => InstallMethod::Script,
                _ if crate::service::managed(&r.binary_path, &r.service).is_ok() => InstallMethod::Managed,
                _ => InstallMethod::Unknown,
            };
            return Installation { method, version: Some(r.version), pinned: Some(r.pinned) };
        }
        // A receipt for this path whose digest changed is not evidence of ownership.
        if r.binary_path == exe {
            return unknown(InstallMethod::Unknown);
        }
    }
    if let Some(version) = super::tap().and_then(|tap| brew_installation(exe, tap)) {
        return Installation { method: InstallMethod::Homebrew, version: Some(version), pinned: None };
    }
    if let Some(version) = deb_package(exe) {
        return Installation { method: InstallMethod::Debian, version: Some(version), pinned: None };
    }
    unknown(InstallMethod::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn cargo_commands_require_the_recorded_source_and_preserve_install_options() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let root = dir.join("install root");
        let source = dir.join("my fork's $source");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("Cargo.toml"), b"").unwrap();
        let exe = root.join("bin/iphone-control");
        let package = format!("iphone-control 0.2.1 (path+{})", reqwest::Url::from_file_path(&source).unwrap());
        let data = serde_json::json!({ "installs": { package: {
            "bins": ["iphone-control"], "features": ["debug-cmds"], "profile": "debug", "all_features": false,
            "no_default_features": true, "target": "aarch64-apple-darwin", "rustc": "host: aarch64-apple-darwin"
        } } });
        fs::write(root.join(".crates2.json"), data.to_string()).unwrap();
        let (version, command) = cargo_source(&exe, &source).unwrap();
        assert_eq!(version, "0.2.1");
        assert_eq!(
            command,
            format!(
                "cd {} && cargo install --locked --path . --root {} --debug --no-default-features --features debug-cmds",
                crate::shell_quote(source.to_str().unwrap()),
                crate::shell_quote(root.to_str().unwrap())
            )
        );
        assert!(cargo_source(&dir.join("other/bin/iphone-control"), &source).is_none());
        assert!(cargo_source(&exe, &root).is_none());
        fs::write(root.join(".crates2.json"), "{}").unwrap();
        assert!(cargo_source(&exe, &source).is_none());
    }

    #[test]
    fn only_identifies_the_receipted_file() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().canonicalize().unwrap().join("iphone-control");
        fs::write(&bin, b"original").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let r = Receipt {
            schema: 1,
            publisher: "example/test".into(),
            target: "linux-x86_64".into(),
            binary_path: bin.clone(),
            service: "systemd".into(),
            version: "1.0.0".into(),
            sha256: digest(&bin).unwrap(),
            pinned: false,
        };
        validate_receipt(&r, &bin).unwrap();
        let other = dir.path().join("copy");
        fs::copy(&bin, &other).unwrap();
        assert!(validate_receipt(&r, &other).is_err());
        fs::write(&bin, b"modified").unwrap();
        assert!(validate_receipt(&r, &bin).is_err());
    }

    #[test]
    fn brew_requires_exact_cellar_binary_and_tap_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let keg = dir.path().join("iphone-control/1.3.2_1");
        fs::create_dir_all(keg.join("bin")).unwrap();
        fs::write(keg.join("INSTALL_RECEIPT.json"), br#"{"source":{"tap":"acme/tools"}}"#).unwrap();
        let exe = keg.join("bin/iphone-control");
        assert_eq!(brew_keg(&exe, dir.path(), "Acme/tools").as_deref(), Some("1.3.2"));
        assert!(brew_keg(&exe, dir.path(), "other/tap").is_none());
        assert!(brew_keg(&keg.join("extra/bin/iphone-control"), dir.path(), "acme/tools").is_none());
    }
}
