//! Host program updates. Only release builds explicitly bound to a GitHub publisher
//! contact the release API. CLI and Viewer use the same local cache and channel evidence.
mod archive;
mod channel;
mod install;
mod release;
pub mod transaction;

use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail, ensure};
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
    pub release_notes: Option<String>,
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

/// Writes `bytes` to `path` atomically and durably: a temporary file in the same directory, synced, renamed over
/// `path`, then the directory synced. The directory must exist: callers create it with the permissions it needs
/// (the updater's store is private and checked, see `transaction::Store::open`)
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| anyhow::anyhow!("no parent directory: {}", path.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// [`write_atomic`] of `value` as JSON
pub(crate) fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    write_atomic(path, &serde_json::to_vec(value)?)
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
    let keys = !option_env!("ICTL_UPDATE_PUBLIC_KEYS").unwrap_or("").is_empty();
    availability_with(local_version, method, platform, release, keys)
}

/// [`availability`] with whether this build carries release signing keys (`ICTL_UPDATE_PUBLIC_KEYS`)
fn availability_with(
    local_version: &str,
    method: InstallMethod,
    platform: Option<&str>,
    release: Option<&Release>,
    keys: bool,
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
        // APT runs the package's scripts as root: only a package the publisher signed (`<deb>.manifest.json` and
        // its signature) is installed by `iphone-control update`
        let Some(asset) = deb_asset(&version.to_string(), platform) else { return Unsupported };
        if !keys {
            return ManualOnly;
        }
        let signed = [asset.clone(), format!("{asset}.manifest.json"), format!("{asset}.manifest.json.sig")];
        return if signed.iter().all(|a| latest.assets.contains(a)) { Available } else { PlatformPending };
    }
    let base = format!("iphone-control-{}-{platform}", latest.tag);
    if !latest.assets.contains(&format!("{base}.tar.gz")) {
        return PlatformPending;
    }
    if method == InstallMethod::Managed {
        // this installation can never take a managed upgrade: a prerelease, or a build without signing keys
        if !installed.pre.is_empty() || !keys {
            return ManualOnly;
        }
        // The platform's signing job publishes the manifest and its signature after the archive (and may wait
        // for approval): until both are there the upgrade is pending, not manual. Presence allows the CLI to
        // attempt verified installation; it verifies the exact manifest bytes, signature and archive before
        // handing anything to the installer.
        if !latest.assets.contains(&format!("{base}.manifest.json"))
            || !latest.assets.contains(&format!("{base}.manifest.json.sig"))
        {
            return PlatformPending;
        }
    }
    Available
}

fn upgrade_command(status: Availability, installation: &Installation, exe: Option<&Path>) -> Option<String> {
    let exe = exe?;
    let exe =
        if installation.method == InstallMethod::Homebrew { crate::service::stable_path(exe) } else { exe.to_owned() };
    let exe = crate::shell_quote(exe.to_str()?);
    match (status, installation.method) {
        (Availability::Available, InstallMethod::Managed) if installation.pinned == Some(true) => {
            Some(format!("{exe} update unpin && {exe} update"))
        }
        (Availability::Available, InstallMethod::Debian) if installation.pinned == Some(true) => {
            Some(format!("sudo apt-mark unhold iphone-control && {exe} update"))
        }
        (Availability::Available, InstallMethod::Managed | InstallMethod::Debian)
        | (Availability::BrewCheck, InstallMethod::Homebrew) => Some(format!("{exe} update")),
        _ => None,
    }
}

fn guidance(status: Availability, method: InstallMethod, pinned: Option<bool>) -> String {
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
                "通过 Homebrew 升级。可安装版本取决于 tap 的发布进度及本机是否固定了版本。",
                "Update through Homebrew. The local tap and Homebrew pin determine the upgrade."
            ).into()
        }
        Availability::Available => match method {
            InstallMethod::Managed if pinned == Some(true) => tr!(
                "安装已固定到指定版本。此命令会解除固定并升级。先退出 Viewer 和 MCP 客户端，等待任务完成，再在主机终端运行。安装前会验证发布者签名；新版本起不来时自动恢复旧版本。",
                "This command unpins and upgrades the installation. Close Viewer and MCP clients and let tasks finish before running it in a host terminal. It verifies the publisher's signature before installing and restores the old version if the new one doesn't start."
            )
            .into(),
            InstallMethod::Managed => tr!(
                "先退出 Viewer 和 MCP 客户端，等待任务完成，再在主机终端运行升级命令。安装前会验证发布者签名；新版本起不来时自动恢复旧版本。",
                "Close Viewer and MCP clients and let tasks finish before running the upgrade command in a host terminal. It verifies the publisher's signature before installing and restores the old version if the new one doesn't start."
            )
            .into(),
            InstallMethod::Debian if pinned == Some(true) => tr!(
                "iphone-control 在 APT 里被锁定（apt-mark hold），不会升级。要升级，先解除锁定，命令会接着下载经发布者签名的 .deb 并通过 APT 安装。",
                "iphone-control is on hold in APT (apt-mark hold) and won't be upgraded. To upgrade, release the hold; the command then downloads the publisher-signed .deb and installs it with APT."
            ).into(),
            InstallMethod::Debian => tr!(
                "升级命令会下载匹配本机架构、经发布者签名的 .deb，验证后通过 APT 安装（需要 sudo）。",
                "The update command downloads the publisher-signed .deb for this architecture, verifies it and installs it with APT (needs sudo)."
            ).into(),
            _ => tr!(
                "关闭正在运行的客户端和服务，再按发行说明手动更新。",
                "Close running clients and services, then follow the manual release instructions."
            )
            .into(),
        },
    }
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
        let mut guidance = guidance(status, self.install.method, self.install.pinned);
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
            upgrade_command(status, &self.install, self.exe.as_deref())
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
            release_notes: release.and_then(|r| r.notes.as_ref()?.for_lang(ictl_l10n::lang())),
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

/// The user explicitly requested an update (`iphone-control update`). Use this check's release and installation
/// channel; say what will happen and ask before installing, unless `yes`
pub async fn upgrade(socket: &Path, report: Report, yes: bool) -> Result<()> {
    if let Some(code) = &report.error_code {
        bail!("{}", trf!("检查更新失败：{code}", "Update check failed: {code}"));
    }
    let pinned = report.pinned == Some(true);
    match (report.availability, report.install_method) {
        (Availability::Current | Availability::Ahead, _) => {
            println!("{} ({})", report.guidance, report.installed_version.as_deref().unwrap_or(VERSION));
        }
        (Availability::Available, InstallMethod::Managed) if pinned => bail!(
            "{}",
            tr!(
                "已固定安装版本。若要升级，请先运行 iphone-control update unpin。",
                "This installation is pinned. To upgrade, run iphone-control update unpin first."
            )
        ),
        (Availability::Available, InstallMethod::Debian) if pinned => bail!(
            "{}",
            tr!(
                "iphone-control 在 APT 里被锁定。若要升级，请先运行 sudo apt-mark unhold iphone-control。",
                "iphone-control is on hold in APT. To upgrade, run sudo apt-mark unhold iphone-control first."
            )
        ),
        (Availability::Available, InstallMethod::Managed) => {
            if !confirm(&report, yes)? {
                return Ok(());
            }
            let version = report.available_version.clone().ok_or_else(|| anyhow::anyhow!("missing release version"))?;
            transaction::apply(socket, false, report).await?;
            println!("{}", trf!("已升级到 {version}。", "Updated to {version}."));
        }
        (Availability::Available, InstallMethod::Debian) | (Availability::BrewCheck, InstallMethod::Homebrew) => {
            if !confirm(&report, yes)? {
                return Ok(());
            }
            channel::apply(socket, &report).await?;
        }
        _ => {
            let next = report.upgrade_command.as_deref().or(report.release_url.as_deref());
            if let Some(next) = next {
                bail!("{}\n{next}", report.guidance);
            }
            bail!("{}", report.guidance);
        }
    }
    Ok(())
}

/// What installing `report`'s release will do, the same for every caller
fn plan(report: &Report) -> String {
    let from = report.installed_version.as_deref().unwrap_or(VERSION);
    let to = report.available_version.as_deref().unwrap_or("?");
    let how = match report.install_method {
        InstallMethod::Managed => trf!(
            "验证发布者签名后安装，然后重启后台服务：正在执行的 AI 任务和控制台画面会中断。新版本起不来时自动换回 {from}。",
            "It verifies the publisher's signature, installs, then restarts the background service: running AI tasks and the control page's picture are interrupted. If the new version doesn't start, {from} is put back."
        ),
        InstallMethod::Debian => tr!(
            "下载并验证经发布者签名的 .deb，用 sudo 通过 APT 安装（会询问密码）。后台服务在运行的话会重启，正在执行的任务会中断。",
            "It downloads and verifies the publisher-signed .deb and installs it with APT through sudo (asks for your password). A running background service is restarted, interrupting running tasks."
        )
        .into(),
        _ => tr!(
            "运行 brew update 和 brew upgrade。Homebrew 的后台服务在运行的话会重启，正在执行的任务会中断。",
            "It runs brew update and brew upgrade. A running Homebrew service is restarted, interrupting running tasks."
        )
        .into(),
    };
    trf!("将从 {from} 升级到 {to}。{how}", "Upgrading from {from} to {to}. {how}")
}

/// Shows [`plan`] and asks. False: the user said no. Without a terminal it needs `yes`
fn confirm(report: &Report, yes: bool) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    println!("{}", plan(report));
    if yes {
        return Ok(true);
    }
    ensure!(
        std::io::stdin().is_terminal(),
        "{}",
        tr!(
            "没有终端，无法确认。在终端里运行，或加 --yes 直接升级。",
            "No terminal to confirm in. Run it in a terminal, or add --yes to upgrade without asking."
        )
    );
    print!("{}", tr!("继续吗？[y/N] ", "Continue? [y/N] "));
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes" | "是") {
        return Ok(true);
    }
    println!("{}", tr!("已取消，没有升级。", "Cancelled; nothing was upgraded."));
    Ok(false)
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
    fn managed_guidance_explains_verification_before_installation() {
        ictl_l10n::with_lang(ictl_l10n::Lang::En, || {
            for pinned in [false, true] {
                let guide = guidance(Availability::Available, InstallMethod::Managed, Some(pinned));
                assert!(guide.contains("signature") && guide.contains("restores the old version"), "{guide}");
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
        cache.release = Some(Release { tag: "v9.4.2".into(), published_at: None, assets: vec![], notes: None });
        let report = local.report(cache).unwrap();
        assert_eq!(report.availability, Availability::Current);
        assert!(report.upgrade_command.is_none());
    }

    #[tokio::test]
    async fn user_update_checks_the_release_before_choosing_an_installer() {
        let report = |installed: &str, error: Option<&str>, pinned| {
            let local = Local {
                source_command: None,
                exe: None,
                install: Installation {
                    method: InstallMethod::Managed,
                    version: Some(installed.into()),
                    pinned: Some(pinned),
                },
                policy: Policy::Manual,
            };
            let mut cache = Cache::default();
            cache.release = Some(Release { tag: "v1.2.3".into(), published_at: None, assets: vec![], notes: None });
            cache.error_code = error.map(str::to_owned);
            local.report(cache).unwrap()
        };
        let socket = Path::new("/unused");
        // Current or newer local versions succeed even when pinned and without an installer or daemon.
        for version in ["1.2.3", "1.2.4"] {
            upgrade(socket, report(version, None, true), false).await.unwrap();
        }
        // A cached version from an unsuccessful refresh cannot authorize any installation.
        let mut stale = report("1.2.2", Some("rate_limited"), false);
        stale.availability = Availability::Available;
        let err = upgrade(socket, stale, true).await.unwrap_err();
        assert!(err.to_string().contains("rate_limited"), "{err}");
        let mut pinned = report("1.2.2", None, true);
        pinned.availability = Availability::Available;
        let err = upgrade(socket, pinned, true).await.unwrap_err();
        assert!(err.to_string().contains("update unpin"), "{err}");
    }

    #[test]
    fn cached_report_uses_viewer_request_language() {
        let cache: Cache = serde_json::from_value(serde_json::json!({
            "release": {"tag": "v1.2.3", "assets": [], "notes": {"en": "New video", "zh": "新画面"}}
        }))
        .unwrap();
        for (lang, expected) in [(ictl_l10n::Lang::En, "New video"), (ictl_l10n::Lang::Zh, "新画面")] {
            let local = Local {
                source_command: None,
                exe: None,
                install: Installation { method: InstallMethod::Unknown, version: None, pinned: None },
                policy: Policy::Manual,
            };
            let report = ictl_l10n::with_lang(lang, || local.report(cache.clone())).unwrap();
            assert_eq!(report.release_notes.as_deref(), Some(expected));
        }
    }

    #[test]
    fn upgrade_commands_follow_the_verified_channel_and_release_state() {
        use Availability::*;
        use InstallMethod::{Debian, Homebrew, Managed, Script, Unknown};

        let exe = Path::new("/Users/it's me/bin/iphone-control");
        let command = |method, pinned, status| {
            let installation = Installation { method, version: Some("0.2.0".into()), pinned };
            upgrade_command(status, &installation, Some(exe))
        };
        let quoted = crate::shell_quote(exe.to_str().unwrap());
        let brew = Installation { method: Homebrew, version: Some("0.2.0".into()), pinned: None };
        let keg = Path::new("/opt/homebrew/Cellar/iphone-control/0.2.0/bin/iphone-control");
        assert_eq!(
            upgrade_command(BrewCheck, &brew, Some(keg)),
            Some(format!("{} update", crate::shell_quote("/opt/homebrew/opt/iphone-control/bin/iphone-control")))
        );
        for (method, status) in [(Managed, Available), (Debian, Available)] {
            assert_eq!(command(method, Some(false), status), Some(format!("{quoted} update")));
        }
        assert_eq!(command(Managed, Some(true), Available), Some(format!("{quoted} update unpin && {quoted} update")));
        for method in [Unknown, Script] {
            assert!(command(method, None, Available).is_none());
        }
        for status in [Development, Unchecked, Current, Ahead, PlatformPending, Unsupported, ManualOnly] {
            assert!(command(Managed, Some(false), status).is_none());
        }
    }

    #[test]
    fn availability_follows_local_version_and_install_channel() {
        let release = Release {
            tag: "v0.2.1".into(),
            published_at: None,
            assets: vec![
                "iphone-control-v0.2.1-linux-x86_64.tar.gz".into(),
                "iphone-control_0.2.1_amd64.deb".into(),
                "iphone-control_0.2.1_amd64.deb.manifest.json".into(),
                "iphone-control_0.2.1_amd64.deb.manifest.json.sig".into(),
            ],
            notes: None,
        };
        let for_install =
            |running, method, platform| availability_with(running, method, platform, Some(&release), true);
        assert_eq!(for_install("0.2.1", InstallMethod::Unknown, None), Availability::Current);
        assert_eq!(for_install("0.3.0-rc.1", InstallMethod::Unknown, None), Availability::Ahead);
        assert_eq!(for_install("0.2.0", InstallMethod::Managed, Some("macos-arm64")), Availability::PlatformPending);
        assert_eq!(for_install("0.2.0", InstallMethod::Managed, Some("linux-x86_64")), Availability::PlatformPending);
        assert_eq!(for_install("0.2.0", InstallMethod::Homebrew, Some("macos-arm64")), Availability::BrewCheck);
        assert_eq!(for_install("0.2.0", InstallMethod::Debian, Some("linux-x86_64")), Availability::Available);
        assert_eq!(for_install("0.2.0", InstallMethod::Debian, Some("linux-aarch64")), Availability::PlatformPending);
        assert_eq!(for_install("0.2.0", InstallMethod::Unknown, None), Availability::Unsupported);
    }

    /// APT installs as root: the .deb is offered only once the publisher has signed it, its own manifest (not
    /// the other architecture's, not a bare checksum file) with the signature; a build without keys can't verify
    #[test]
    fn deb_upgrade_waits_for_its_signed_manifest() {
        let asset = "iphone-control_0.2.1_amd64.deb";
        let mut release = Release {
            tag: "v0.2.1".into(),
            assets: vec![asset.into(), format!("{asset}.sha256")],
            published_at: None,
            notes: None,
        };
        let status =
            |r: &Release, keys| availability_with("0.2.0", InstallMethod::Debian, Some("linux-x86_64"), Some(r), keys);
        assert_eq!(status(&release, true), Availability::PlatformPending, "a checksum file is no signature");
        release.assets.push("iphone-control_0.2.1_arm64.deb.manifest.json".into());
        release.assets.push("iphone-control_0.2.1_arm64.deb.manifest.json.sig".into());
        release.assets.push(format!("{asset}.manifest.json"));
        assert_eq!(status(&release, true), Availability::PlatformPending, "signature still missing");
        release.assets.push(format!("{asset}.manifest.json.sig"));
        assert_eq!(status(&release, true), Availability::Available);
        assert_eq!(status(&release, false), Availability::ManualOnly, "no signing keys in this build");
    }

    #[test]
    fn a_held_deb_is_reported_like_a_pinned_installation() {
        let exe = Path::new("/usr/bin/iphone-control");
        let held = Installation { method: InstallMethod::Debian, version: Some("0.2.0".into()), pinned: Some(true) };
        assert_eq!(
            upgrade_command(Availability::Available, &held, Some(exe)).as_deref(),
            Some("sudo apt-mark unhold iphone-control && /usr/bin/iphone-control update")
        );
        ictl_l10n::with_lang(ictl_l10n::Lang::En, || {
            assert!(guidance(Availability::Available, InstallMethod::Debian, Some(true)).contains("on hold"));
            assert!(guidance(Availability::Available, InstallMethod::Debian, Some(false)).contains("signed"));
        });
    }

    /// A managed install with signing keys: the archive alone (the signing job hasn't published the manifest and
    /// its signature yet, or waits for approval) is pending, not a manual upgrade; with both it is available.
    /// Without keys, or from a prerelease, it stays manual
    #[test]
    fn managed_upgrade_waits_for_the_signed_manifest() {
        let base = "iphone-control-v0.2.1-linux-x86_64";
        let mut release =
            Release { tag: "v0.2.1".into(), published_at: None, assets: vec![format!("{base}.tar.gz")], notes: None };
        let managed = |r: &Release, running, keys| {
            availability_with(running, InstallMethod::Managed, Some("linux-x86_64"), Some(r), keys)
        };
        assert_eq!(managed(&release, "0.2.0", true), Availability::PlatformPending);
        release.assets.push(format!("{base}.manifest.json"));
        assert_eq!(managed(&release, "0.2.0", true), Availability::PlatformPending, "signature still missing");
        release.assets.push(format!("{base}.manifest.json.sig"));
        assert_eq!(managed(&release, "0.2.0", true), Availability::Available);
        assert_eq!(managed(&release, "0.2.0", false), Availability::ManualOnly, "no signing keys in this build");
        assert_eq!(managed(&release, "0.2.1-rc.1", true), Availability::ManualOnly, "prerelease install");
    }
}
