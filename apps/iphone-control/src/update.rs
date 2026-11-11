//! Host program updates. Only release builds explicitly bound to a GitHub publisher
//! contact the release API. CLI and Viewer use the same local cache and channel evidence.
mod archive;
mod install;
mod release;
pub mod transaction;

use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use ictl_l10n::{tr, trf};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use install::Installation;
pub use install::{Receipt, digest, read_receipt, receipt_path, validate_receipt};
use release::{Cache, Release};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn repository(s: &str) -> bool {
    // Same slug grammar as the signed installer and release workflow.
    let part = |v: &str, extra: &[u8]| {
        v.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && v.bytes().all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
    };
    s.split_once('/').is_some_and(|(owner, repo)| part(owner, b"-") && part(repo, b"-_."))
}

pub fn publisher() -> Option<&'static str> {
    option_env!("ICTL_UPDATE_PUBLISHER").filter(|s| repository(s))
}

fn tap() -> Option<&'static str> {
    option_env!("ICTL_UPDATE_TAP").filter(|s| repository(s))
}

pub fn target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("macos-arm64"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Some("linux-aarch64"),
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Some("linux-x86_64"),
        _ => None,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
    Development,
    Managed,
    Script,
    Homebrew,
    Debian,
    Unknown,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    Manual,
    Notify,
    AutoPatch,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Development,
    Unchecked,
    Current,
    Ahead,
    Available,
    PlatformPending,
    Unsupported,
    BrewCheck,
    ManualOnly,
}

#[derive(Serialize)]
pub struct Report {
    pub running_version: &'static str,
    pub daemon_version: Option<String>,
    pub installed_version: Option<String>,
    pub publisher: Option<&'static str>,
    pub target: Option<&'static str>,
    pub executable: Option<PathBuf>,
    pub install_method: InstallMethod,
    pub policy: Policy,
    pub pinned: Option<bool>,
    pub available_version: Option<String>,
    pub availability: Availability,
    pub last_success_at: Option<u64>,
    pub last_attempt_at: Option<u64>,
    pub retry_after: Option<u64>,
    pub error_code: Option<String>,
    pub published_at: Option<String>,
    pub release_url: Option<String>,
    pub guidance: String,
    pub upgrade_command: Option<String>,
    pub last_update: Option<transaction::Outcome>,
}

impl Report {
    pub fn automatic_patch(&self) -> bool {
        self.policy == Policy::AutoPatch
            && self.error_code.is_none()
            && self.availability == Availability::Available
            && self.available_version.as_deref().is_some_and(|v| {
                transaction::eligible(self.installed_version.as_deref().unwrap_or(self.running_version), v, true)
                    .is_ok()
            })
    }
}

fn state_dir(repo: &str) -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::envfile::home().join(".cache"))
        .join("iphone-control/updates")
        .join(repo)
        .join(target().unwrap_or("unsupported"))
}

fn policy_file(repo: &str, exe: &Path, method: InstallMethod) -> PathBuf {
    // A verified keg is Cellar/formula/version/bin/iphone-control. Its formula
    // directory survives upgrades and still distinguishes separate installations.
    let installation = if method == InstallMethod::Homebrew { exe.ancestors().nth(3).unwrap_or(exe) } else { exe };
    let id = Sha256::digest(installation.as_os_str().as_encoded_bytes());
    crate::envfile::config_dir().join("updates").join(repo).join(format!("{id:x}.json"))
}

fn policy(repo: Option<&str>, exe: Option<&Path>, installation: &Installation) -> Result<Policy> {
    let (Some(repo), Some(exe)) = (repo, exe) else { return Ok(Policy::Manual) };
    read_policy(&policy_file(repo, exe, installation.method), installation)
}

fn read_policy(file: &Path, installation: &Installation) -> Result<Policy> {
    let saved: Policy = match fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(if installation.method == InstallMethod::Managed { Policy::Notify } else { Policy::Manual });
        }
        Err(e) => return Err(e.into()),
    };
    if saved == Policy::AutoPatch
        && (installation.method != InstallMethod::Managed || installation.pinned != Some(false))
    {
        return Ok(Policy::Manual);
    }
    Ok(saved)
}

pub fn set_policy(value: Policy) -> Result<()> {
    let Some(repo) = publisher() else {
        ensure!(
            value == Policy::Manual,
            "{}",
            tr!("开发构建没有绑定发布者", "Development build has no bound publisher")
        );
        return Ok(());
    };
    let exe = std::env::current_exe()?.canonicalize()?;
    let installation = install::detect(Some(&exe));
    if value == Policy::AutoPatch {
        ensure!(
            installation.method == InstallMethod::Managed && installation.pinned == Some(false),
            "{}",
            tr!("自动补丁更新需要未固定版本的受管用户服务", "Auto-patch requires an unpinned, managed user service")
        );
    }
    release::write_json(&policy_file(repo, &exe, installation.method), &value)
}

fn deb_asset(version: &str, platform: &str) -> Option<String> {
    let arch = match platform {
        "linux-x86_64" => "amd64",
        "linux-aarch64" => "arm64",
        _ => return None,
    };
    Some(format!("iphone-control_{version}_{arch}.deb"))
}

fn availability(
    local_version: &str,
    method: InstallMethod,
    platform: Option<&str>,
    release: Option<&Release>,
) -> Availability {
    use Availability::*;
    if method == InstallMethod::Development {
        return Development;
    }
    let Some(latest) = release else { return Unchecked };
    let Some(version) = release::stable_tag(&latest.tag) else { return Unchecked };
    let installed = semver::Version::parse(local_version).expect("validated installation or Cargo version");
    match version.cmp_precedence(&installed) {
        Ordering::Less => return Ahead,
        Ordering::Equal => return Current,
        Ordering::Greater => {}
    }
    let Some(platform) = platform else { return Unsupported };
    if method == InstallMethod::Homebrew {
        return BrewCheck;
    }
    if method == InstallMethod::Debian {
        return match deb_asset(&version.to_string(), platform) {
            Some(asset) if latest.assets.contains(&asset) => Available,
            Some(_) => PlatformPending,
            None => Unsupported,
        };
    }
    let base = format!("iphone-control-{}-{platform}", latest.tag);
    if !latest.assets.contains(&format!("{base}.tar.gz")) {
        return PlatformPending;
    }
    // Presence allows the CLI to attempt verified installation. It verifies the exact
    // manifest bytes, signature and archive before handing anything to the installer.
    if method == InstallMethod::Managed
        && (!installed.pre.is_empty()
            || option_env!("ICTL_UPDATE_PUBLIC_KEYS").unwrap_or("").is_empty()
            || !latest.assets.contains(&format!("{base}.manifest.json"))
            || !latest.assets.contains(&format!("{base}.manifest.json.sig")))
    {
        return ManualOnly;
    }
    Available
}

fn upgrade_command(
    status: Availability,
    installation: &Installation,
    exe: Option<&Path>,
    release: Option<&Release>,
    platform: Option<&str>,
    tap: Option<&str>,
) -> Option<String> {
    match (status, installation.method) {
        (Availability::Available, InstallMethod::Managed) => {
            let exe = crate::shell_quote(exe?.to_str()?);
            Some(if installation.pinned? {
                format!("{exe} update unpin && {exe} update apply")
            } else {
                format!("{exe} update apply")
            })
        }
        (Availability::BrewCheck, InstallMethod::Homebrew) => {
            let tap = tap.filter(|tap| repository(tap))?;
            let brew = install::brew_program(exe?.ancestors().nth(4)?)?;
            let brew = crate::shell_quote(brew.to_str()?);
            Some(format!("{brew} update && {brew} upgrade {tap}/iphone-control"))
        }
        (Availability::Available, InstallMethod::Debian) => {
            let version = release::stable_tag(&release?.tag)?;
            let asset = deb_asset(&version.to_string(), platform?)?;
            Some(format!("sudo apt install ./{asset}"))
        }
        _ => None,
    }
}

fn guidance(status: Availability, method: InstallMethod, version: Option<&str>, pinned: Option<bool>) -> String {
    match status {
        Availability::Development => {
            tr!("开发构建。在对应源码仓库检出目标版本，按原来的构建或安装方式更新本机程序。", "Development build. Check out the desired revision in its source repository and update this copy using its original build or install method.").into()
        }
        Availability::Unsupported => tr!(
            "此平台没有预编译包。请查看发行说明并从源码构建。",
            "No prebuilt package for this platform. See the release notes and build from source."
        )
        .into(),
        Availability::PlatformPending => tr!(
            "发行版已发布。当前平台的安装包尚未就绪。",
            "The release is published. The package for this platform is pending."
        )
        .into(),
        Availability::ManualOnly => tr!(
            "当前安装无法应用此版本的受管升级。请按发行说明手动升级。",
            "A managed upgrade is unavailable for this installation and release. Follow the manual release instructions."
        )
        .into(),
        Availability::Current => tr!("已安装最新稳定版。", "The latest stable release is installed.").into(),
        Availability::Ahead => {
            tr!("本机版本领先于最新公开稳定版。", "The local version is ahead of the latest public stable release.").into()
        }
        Availability::Unchecked => tr!("尚无成功检查记录。", "No successful release check yet.").into(),
        Availability::BrewCheck => {
            tr!(
                "在主机终端运行 Homebrew 命令。可安装版本取决于 tap 的发布进度及本机是否固定了版本。",
                "Run the Homebrew command in a terminal on the host. The local tap and Homebrew pin determine the upgrade."
            ).into()
        }
        Availability::Available => match method {
            InstallMethod::Managed if pinned == Some(true) => tr!(
                "安装已固定到指定版本。此命令会解除固定并升级。先退出 Viewer 和 MCP 客户端，等待任务完成，再在主机终端运行。安装前会验证签名并检查回退兼容性。",
                "This command unpins and upgrades the installation. Close Viewer and MCP clients and let tasks finish before running it in a host terminal. It verifies the signature and checks rollback compatibility before installing."
            )
            .into(),
            InstallMethod::Managed => tr!(
                "先退出 Viewer 和 MCP 客户端，等待任务完成，再在主机终端运行升级命令。安装前会验证签名并检查回退兼容性。",
                "Close Viewer and MCP clients and let tasks finish before running the upgrade command in a host terminal. It verifies the signature and checks rollback compatibility before installing."
            )
            .into(),
            InstallMethod::Debian => {
                let asset = deb_asset(version.unwrap_or_default(), target().unwrap_or_default()).unwrap_or_default();
                trf!(
                    "从发行页下载并校验 {asset}，在下载目录中运行安装命令。",
                    "Download and verify {asset} from the release, then run the install command in the download directory."
                )
            }
            _ => tr!(
                "关闭正在运行的客户端和服务，再按发行说明手动更新。",
                "Close running clients and services, then follow the manual release instructions."
            )
            .into(),
        },
    }
}

fn manual_migration(
    installation: &Installation,
    outcome: Option<&transaction::Outcome>,
    latest: Option<&str>,
    repo: Option<&str>,
    platform: Option<&str>,
) -> bool {
    let (Some(outcome), Some(from), Some(repo), Some(platform)) =
        (outcome, installation.version.as_deref(), repo, platform)
    else {
        return false;
    };
    installation.method == InstallMethod::Managed
        && outcome.status == "manual_migration"
        && Some(outcome.target_version.as_str()) == latest
        && outcome.migration_source.as_ref().is_some_and(|source| source.matches(repo, platform, from))
}

struct Local {
    source_command: Option<String>,
    exe: Option<PathBuf>,
    install: Installation,
    policy: Policy,
}

impl Local {
    fn load() -> Result<Self> {
        let exe = std::env::current_exe().ok().as_deref().and_then(install::installed_executable);
        let mut install = install::detect(exe.as_deref());
        let source_command = if install.method == InstallMethod::Development && unsafe { libc::geteuid() } != 0 {
            exe.as_deref().and_then(|exe| install::cargo_source(exe, Path::new(env!("CARGO_MANIFEST_DIR")))).map(
                |(version, command)| {
                    install.version = Some(version);
                    command
                },
            )
        } else {
            None
        };
        let policy = policy(publisher(), exe.as_deref(), &install)?;
        Ok(Self { exe, install, policy, source_command })
    }

    fn report(self, cache: Cache) -> Result<Report> {
        let repo = publisher();
        let release = cache.release.as_ref().filter(|r| release::stable_tag(&r.tag).is_some());
        let version = release.and_then(|r| r.tag.strip_prefix('v')).map(str::to_owned);
        let last_update = self.exe.as_deref().map(transaction::outcome).transpose()?.flatten();
        let local_version = self.install.version.as_deref().unwrap_or(VERSION);
        let status = availability(local_version, self.install.method, target(), release);
        let (status, mut guidance) = if status == Availability::Available
            && manual_migration(&self.install, last_update.as_ref(), version.as_deref(), repo, target())
        {
            (Availability::ManualOnly, tr!(
                "已验证的发行清单要求从当前安装版本手动迁移。请按发行说明中的步骤升级。",
                "The verified release manifest requires a manual migration from the installed version. Follow the release notes to upgrade."
            ).into())
        } else {
            (status, guidance(status, self.install.method, version.as_deref(), self.install.pinned))
        };
        let command = if self.source_command.is_some() {
            guidance = tr!(
                "Cargo 源码安装。先在命令所指的本机源码仓库检出目标版本，再运行命令重新安装。完成后重新打开 Viewer，如有后台服务也请重启。",
                "Installed with Cargo from source. First check out the desired revision in the local repository shown in the command, then run the command to reinstall. Reopen Viewer and restart the service if you use it."
            ).into();
            self.source_command
        } else if cache.error_code.is_some() {
            guidance =
                tr!("检查更新失败。请稍后重试，再决定是否升级。", "The release check failed. Retry before upgrading.")
                    .into();
            None
        } else {
            upgrade_command(status, &self.install, self.exe.as_deref(), release, target(), tap())
        };
        if status == Availability::Current && self.install.version.as_deref().is_some_and(|v| v != VERSION) {
            guidance.push_str(tr!(
                " 重新打开正在运行的旧版客户端，并按需重启后台服务。",
                " Reopen clients still running the old version and restart the service if needed."
            ));
        }
        if self.policy == Policy::AutoPatch
            && status == Availability::Available
            && version.as_deref().is_some_and(|v| transaction::eligible(local_version, v, true).is_err())
        {
            guidance.push_str(tr!(
                " 此版本跨越补丁范围，不会自动安装。",
                " This is not a patch in the current series; it will not be installed automatically."
            ));
        }
        Ok(Report {
            running_version: VERSION,
            daemon_version: None,
            installed_version: self.install.version,
            publisher: repo,
            target: target(),
            executable: self.exe,
            install_method: self.install.method,
            policy: self.policy,
            pinned: self.install.pinned,
            available_version: version,
            availability: status,
            last_success_at: cache.last_success_at,
            last_attempt_at: cache.last_attempt_at,
            retry_after: cache.retry_after,
            error_code: cache.error_code,
            published_at: release.and_then(|r| r.published_at.clone()),
            release_url: repo.zip(release).map(|(repo, r)| format!("https://github.com/{repo}/releases/tag/{}", r.tag)),
            guidance,
            upgrade_command: command,
            last_update,
        })
    }
}

pub fn cached() -> Result<Report> {
    let cache = publisher().map(|repo| release::read(&state_dir(repo))).transpose()?.unwrap_or_default();
    Local::load()?.report(cache)
}

pub async fn check(background: bool) -> Result<Report> {
    let local = tokio::task::spawn_blocking(Local::load).await??;
    let Some(repo) = publisher() else { return local.report(Cache::default()) };
    let dir = state_dir(repo);
    let cache = if background && local.policy == Policy::Manual {
        release::read(&dir)?
    } else {
        let url = format!("https://api.github.com/repos/{repo}/releases/latest");
        release::check(&dir, &release::client()?, &url, background).await?
    };
    local.report(cache)
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unknown = tr!("未知", "unknown");
        writeln!(
            f,
            "{}  {}    daemon  {}    {}  {}",
            tr!("运行版本", "Running"),
            self.running_version,
            self.daemon_version.as_deref().unwrap_or(unknown),
            tr!("磁盘版本", "Installed"),
            self.installed_version.as_deref().unwrap_or(unknown)
        )?;
        let method = match self.install_method {
            InstallMethod::Managed => tr!("受管用户服务", "managed user service"),
            InstallMethod::Script => tr!("脚本安装，手动启动", "installer, manual startup"),
            InstallMethod::Homebrew => "Homebrew",
            InstallMethod::Debian => "Debian",
            InstallMethod::Development => tr!("开发构建", "development build"),
            InstallMethod::Unknown => unknown,
        };
        let policy = match self.policy {
            Policy::Manual => "manual",
            Policy::Notify => "notify",
            Policy::AutoPatch => "auto-patch",
        };
        writeln!(
            f,
            "{}  {}    {}  {method}    {}  {policy}",
            tr!("发布者", "Publisher"),
            self.publisher.unwrap_or(unknown),
            tr!("安装方式", "Installation"),
            tr!("策略", "Policy")
        )?;
        if let Some(path) = &self.executable {
            writeln!(f, "{}", path.display())?;
        }
        if let Some(version) = &self.available_version {
            writeln!(
                f,
                "{}  {}    {}",
                tr!("公开稳定版", "Latest stable release"),
                version,
                self.published_at.as_deref().unwrap_or("")
            )?;
        }
        if let Some(url) = &self.release_url {
            writeln!(f, "{url}")?;
        }
        if let Some(ts) = self.last_success_at.and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0)) {
            writeln!(f, "{}  {ts}", tr!("上次成功检查", "Last successful check"))?;
        }
        if let Some(code) = &self.error_code {
            writeln!(f, "{}  {code}", tr!("本次检查失败", "Release check failed"))?;
        }
        if let Some(outcome) = &self.last_update {
            writeln!(
                f,
                "{}  {}  {}  {}",
                tr!("最近更新事务", "Last installation"),
                outcome.target_version,
                outcome.status,
                outcome.detail
            )?;
        }
        write!(f, "{}", self.guidance)?;
        if let Some(command) = &self.upgrade_command {
            write!(f, "\n{command}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_migration_decision_only_applies_to_the_same_upgrade() {
        let mut install =
            Installation { method: InstallMethod::Managed, version: Some("0.2.0".into()), pinned: Some(false) };
        let outcome: transaction::Outcome = serde_json::from_value(serde_json::json!({
            "status": "manual_migration", "target_version": "0.2.2", "detail": "", "suppressed_version": "0.2.2",
            "migration_source": { "publisher": "acme/tools", "target": "macos-arm64", "version": "0.2.0" }
        }))
        .unwrap();
        let matches = |install: &Installation, latest, repo, platform| {
            manual_migration(install, Some(&outcome), Some(latest), Some(repo), Some(platform))
        };
        assert!(matches(&install, "0.2.2", "acme/tools", "macos-arm64"));
        assert!(!matches(&install, "0.2.3", "acme/tools", "macos-arm64"));
        assert!(!matches(&install, "0.2.2", "other/tools", "macos-arm64"));
        assert!(!matches(&install, "0.2.2", "acme/tools", "linux-aarch64"));
        install.version = Some("0.2.1".into());
        assert!(!matches(&install, "0.2.2", "acme/tools", "macos-arm64"));
        install.version = Some("0.2.0".into());
        install.method = InstallMethod::Unknown;
        assert!(!matches(&install, "0.2.2", "acme/tools", "macos-arm64"));
    }

    #[test]
    fn managed_guidance_explains_verification_before_installation() {
        ictl_l10n::with_lang(ictl_l10n::Lang::En, || {
            for pinned in [false, true] {
                let guide = guidance(Availability::Available, InstallMethod::Managed, Some("0.2.1"), Some(pinned));
                assert!(guide.contains("signature") && guide.contains("rollback compatibility"), "{guide}");
            }
        });
    }

    #[test]
    fn homebrew_policy_survives_a_keg_upgrade() {
        let old = Path::new("/opt/homebrew/Cellar/iphone-control/0.2.0/bin/iphone-control");
        let new = Path::new("/opt/homebrew/Cellar/iphone-control/0.2.1/bin/iphone-control");
        let policy = |exe, method| policy_file("owner/repo", exe, method);
        assert_eq!(policy(old, InstallMethod::Homebrew), policy(new, InstallMethod::Homebrew));
        assert_ne!(policy(old, InstallMethod::Unknown), policy(new, InstallMethod::Unknown));
        let other = Path::new("/usr/local/Cellar/iphone-control/0.2.1/bin/iphone-control");
        assert_ne!(policy(new, InstallMethod::Homebrew), policy(other, InstallMethod::Homebrew));
    }

    #[test]
    fn policy_defaults_to_notify_only_for_managed_and_disables_auto_when_ownership_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("policy.json");
        let mut install =
            Installation { method: InstallMethod::Managed, version: Some("0.2.0".into()), pinned: Some(false) };
        assert_eq!(read_policy(&file, &install).unwrap(), Policy::Notify);
        release::write_json(&file, &Policy::AutoPatch).unwrap();
        assert_eq!(read_policy(&file, &install).unwrap(), Policy::AutoPatch);
        install.pinned = Some(true);
        assert_eq!(read_policy(&file, &install).unwrap(), Policy::Manual);
        install.method = InstallMethod::Unknown;
        install.pinned = None;
        assert_eq!(read_policy(&file, &install).unwrap(), Policy::Manual);
        fs::remove_file(file.as_path()).unwrap();
        assert_eq!(read_policy(&file, &install).unwrap(), Policy::Manual);
    }

    #[test]
    fn only_relative_repository_names_can_be_publishers() {
        for invalid in
            ["other", "http://example.org", "owner/repo?key=1", "../repo", "owner/..", "/repo", "owner/repo/extra"]
        {
            assert!(!repository(invalid), "{invalid}");
        }
        assert!(repository("Example-Org/iphone-control"));
    }

    #[test]
    fn an_old_viewer_does_not_offer_to_upgrade_the_current_installation() {
        let local = Local {
            source_command: None,
            exe: None,
            install: Installation { method: InstallMethod::Debian, version: Some("9.4.2".into()), pinned: None },
            policy: Policy::Manual,
        };
        let mut cache = Cache::default();
        cache.release = Some(Release { tag: "v9.4.2".into(), published_at: None, assets: vec![] });
        let report = local.report(cache).unwrap();
        assert_eq!(report.availability, Availability::Current);
        assert!(report.upgrade_command.is_none());
    }

    #[test]
    fn upgrade_commands_follow_the_verified_channel_and_release_state() {
        let exe = Path::new("/Users/it's me/bin/iphone-control");
        let release = Release { tag: "v0.2.1".into(), published_at: None, assets: vec![] };
        let brew = Path::new("/opt/homebrew/bin/brew");
        let keg = Path::new("/opt/homebrew/Cellar/iphone-control/0.2.0/bin/iphone-control");
        let command = |method, pinned, status, platform, tap| {
            let exe = if method == InstallMethod::Homebrew { keg } else { exe };
            let installation = Installation { method, version: Some("0.2.0".into()), pinned };
            upgrade_command(status, &installation, Some(exe), Some(&release), Some(platform), tap)
        };
        use Availability::*;
        use InstallMethod::{Debian, Homebrew, Managed, Script, Unknown};
        let quoted = crate::shell_quote(exe.to_str().unwrap());
        for (pinned, expected) in [
            (false, format!("{quoted} update apply")),
            (true, format!("{quoted} update unpin && {quoted} update apply")),
        ] {
            assert_eq!(command(Managed, Some(pinned), Available, "macos-arm64", None), Some(expected));
        }
        assert_eq!(
            command(Homebrew, None, BrewCheck, "macos-arm64", Some("Example/tap")),
            Some(format!(
                "{brew} update && {brew} upgrade Example/tap/iphone-control",
                brew = crate::shell_quote(brew.to_str().unwrap())
            ))
        );
        assert!(command(Homebrew, None, BrewCheck, "macos-arm64", Some("unverified tap")).is_none());
        for (target, arch) in [("linux-aarch64", "arm64"), ("linux-x86_64", "amd64")] {
            assert_eq!(
                command(Debian, None, Available, target, None),
                Some(format!("sudo apt install ./iphone-control_0.2.1_{arch}.deb"))
            );
        }
        for method in [Unknown, Script] {
            assert!(command(method, None, Available, "macos-arm64", None).is_none());
        }
        for status in [Development, Unchecked, Current, Ahead, PlatformPending, Unsupported, ManualOnly] {
            assert!(command(Managed, Some(false), status, "macos-arm64", None).is_none());
        }
    }

    #[test]
    fn availability_follows_local_version_and_install_channel() {
        let release = Release {
            tag: "v0.2.1".into(),
            published_at: None,
            assets: vec!["iphone-control-v0.2.1-linux-x86_64.tar.gz".into(), "iphone-control_0.2.1_amd64.deb".into()],
        };
        let for_install = |running, method, platform| availability(running, method, platform, Some(&release));
        assert_eq!(for_install("0.2.1", InstallMethod::Unknown, None), Availability::Current);
        assert_eq!(for_install("0.3.0-rc.1", InstallMethod::Unknown, None), Availability::Ahead);
        assert_eq!(for_install("0.2.0", InstallMethod::Managed, Some("macos-arm64")), Availability::PlatformPending);
        assert_eq!(for_install("0.2.0", InstallMethod::Managed, Some("linux-x86_64")), Availability::ManualOnly);
        assert_eq!(for_install("0.2.0", InstallMethod::Homebrew, Some("macos-arm64")), Availability::BrewCheck);
        assert_eq!(for_install("0.2.0", InstallMethod::Debian, Some("linux-x86_64")), Availability::Available);
        assert_eq!(for_install("0.2.0", InstallMethod::Debian, Some("linux-aarch64")), Availability::PlatformPending);
        assert_eq!(for_install("0.2.0", InstallMethod::Unknown, None), Availability::Unsupported);
    }
}
