//! A replacement is recoverable using only the journal, the installed file and the pre-swap
//! executable. Commit metadata after two healthy snapshots; an unconfirmed swap restores the
//! original file. Each rename is synced before the next step.
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{Receipt, archive, digest, read_receipt, receipt_path, validate_receipt, write_json_atomic as write_json};
use crate::service::{self, Service};

#[derive(Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub status: String,
    pub target_version: String,
    pub detail: String,
    pub suppressed_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_source: Option<MigrationSource>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MigrationSource {
    pub publisher: String,
    pub target: String,
    pub version: String,
}

impl MigrationSource {
    pub(super) fn matches(&self, publisher: &str, target: &str, version: &str) -> bool {
        self.publisher == publisher && self.target == target && self.version == version
    }

    fn from_receipt(receipt: &Receipt) -> Self {
        Self { publisher: receipt.publisher.clone(), target: receipt.target.clone(), version: receipt.version.clone() }
    }
}

impl Outcome {
    fn suppression_for(self, receipt: &Receipt) -> Option<String> {
        if let Some(src) = self.migration_source
            && !src.matches(&receipt.publisher, &receipt.target, &receipt.version)
        {
            return None;
        }
        self.suppressed_version
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Replaced,
    Confirmed,
}

#[derive(Serialize, Deserialize)]
struct Transaction {
    old: Receipt,
    new: Receipt,
    was_running: bool,
    rollback: bool,
    phase: Phase,
}

#[derive(Serialize, Deserialize)]
struct Previous {
    before_digest: String,
    receipt: Receipt,
}

fn state_dir(exe: &Path) -> Result<PathBuf> {
    Ok(exe.parent().context("invalid installation path")?.join(".iphone-control-update"))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let file = match File::options().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(file.metadata()?.is_file() && file.metadata()?.len() <= 64 * 1024, "invalid updater state file");
    Ok(Some(serde_json::from_reader(file)?))
}

pub fn outcome(exe: &Path) -> Result<Option<Outcome>> {
    read_json(&state_dir(exe)?.join("outcome.json"))
}

struct Store {
    binary: PathBuf,
    dir: PathBuf,
    receipt: PathBuf,
    _lock: File,
}

impl Store {
    fn open(binary: &Path, receipt: PathBuf) -> Result<Self> {
        let parent = binary.parent().context("invalid installation path")?;
        let uid = unsafe { libc::geteuid() };
        let meta = parent.metadata()?;
        ensure!(
            uid != 0 && binary.canonicalize()? == binary && meta.uid() == uid && meta.mode() & 0o022 == 0,
            "unsafe installation directory"
        );
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(parent.join(".iphone-control-update.lock"))?;
        let meta = lock.metadata()?;
        ensure!(meta.is_file() && meta.uid() == uid && meta.mode() & 0o077 == 0, "unsafe updater lock");
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error()).context("another installation transaction is running");
        }
        let dir = state_dir(binary)?;
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => File::open(parent)?.sync_all()?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let meta = dir.symlink_metadata()?;
        ensure!(meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0, "unsafe updater directory");
        Ok(Self { binary: binary.into(), receipt, dir, _lock: lock })
    }

    fn journal(&self) -> PathBuf {
        self.dir.join("transaction.json")
    }

    fn record(
        &self,
        status: &str,
        version: &str,
        detail: impl ToString,
        suppressed: Option<String>,
        migration_source: Option<MigrationSource>,
    ) -> Result<()> {
        write_json(
            &self.dir.join("outcome.json"),
            &Outcome {
                status: status.into(),
                target_version: version.into(),
                detail: detail.to_string(),
                suppressed_version: suppressed,
                migration_source,
            },
        )
    }

    fn stage(&self) -> Result<PathBuf> {
        let stage = self.dir.join("stage");
        if stage.exists() {
            fs::remove_dir_all(&stage)?;
        }
        fs::DirBuilder::new().mode(0o700).create(&stage)?;
        Ok(stage)
    }

    // Keep the source (including the independent recovery entry) callable even after rename.
    fn link(&self, source: &Path, dest: &Path) -> Result<()> {
        let tmp = self.dir.join("swap-link");
        if tmp.exists() {
            fs::remove_file(&tmp)?;
        }
        fs::hard_link(source, &tmp)?;
        fs::rename(&tmp, dest)?;
        File::open(dest.parent().context("missing destination directory")?)?.sync_all()?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    fn finish(&self, tx: &Transaction) -> Result<()> {
        write_json(&self.receipt, &tx.new)?;
        if !tx.rollback {
            self.link(&self.dir.join("recovery"), &self.dir.join("previous"))?;
            write_json(
                &self.dir.join("previous.json"),
                &Previous { before_digest: tx.new.sha256.clone(), receipt: tx.old.clone() },
            )?;
        }
        let (status, suppressed) =
            if tx.rollback { ("rolled_back", Some(tx.old.version.clone())) } else { ("installed", None) };
        self.record(status, &tx.new.version, "installation confirmed", suppressed, None)?;
        self.clear()
    }

    fn clear(&self) -> Result<()> {
        fs::remove_file(self.journal())?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

trait Services {
    fn mark_pending(&self, _journal: &Path) -> Result<()> {
        Ok(())
    }
    fn exclusive(&self) -> Result<File>;
    async fn idle(&self, current: &Receipt) -> Result<bool>;
    async fn stop(&self) -> Result<()>;
    async fn start_healthy(&self, version: &str) -> Result<()>;
}

struct Host<'a> {
    service: Service,
    socket: &'a Path,
    binary: &'a Path,
}

#[cfg(target_os = "macos")]
fn process_path(pid: u32) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    ensure!(n > 0, "cannot identify daemon process");
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..n as usize])))
}

#[cfg(not(target_os = "macos"))]
fn process_path(pid: u32) -> Result<PathBuf> {
    Ok(fs::read_link(format!("/proc/{pid}/exe"))?)
}

fn check_clients(daemon: Option<u32>) -> Result<()> {
    // Older binaries don't take a shared lease. Refuse an existing client (also an installation
    // using a different socket) before restarting the service.
    let output = std::process::Command::new("ps").args(["-A", "-o", "uid=,pid=,comm="]).env("LC_ALL", "C").output()?;
    ensure!(output.status.success(), "cannot enumerate running clients");
    let uid = unsafe { libc::geteuid() };
    for line in std::str::from_utf8(&output.stdout)?.lines() {
        let mut parts = line.split_whitespace();
        if parts.next().and_then(|s| s.parse::<u32>().ok()) != Some(uid) {
            continue;
        }
        let pid = parts.next().and_then(|s| s.parse::<u32>().ok()).context("invalid process list")?;
        if daemon == Some(pid) || pid == std::process::id() {
            continue;
        }
        let command = parts.collect::<Vec<_>>().join(" ");
        ensure!(
            !Path::new(&command).file_name().is_some_and(|s| s == "iphone-control"),
            "busy: another iphone-control process ({pid}) is running"
        );
    }
    Ok(())
}

impl Host<'_> {
    fn owns_pid(&self, pid: u32) -> bool {
        process_path(pid)
            .and_then(|path| {
                let p = path.metadata()?;
                let b = self.binary.metadata()?;
                Ok(p.dev() == b.dev() && p.ino() == b.ino())
            })
            .unwrap_or(false)
    }

    async fn socket_owner(&self) -> Result<Option<u32>> {
        match tokio::net::UnixStream::connect(self.socket).await {
            Ok(s) => {
                let cred = s.peer_cred()?;
                ensure!(cred.uid() == unsafe { libc::geteuid() }, "unexpected daemon socket owner");
                Ok(Some(cred.pid().context("unknown daemon socket PID")?.try_into()?))
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn healthy(&self, pid: u32, version: &str) -> bool {
        if !self.owns_pid(pid) {
            return false;
        }
        matches!(tokio::time::timeout(Duration::from_secs(2), crate::snapshot(self.socket)).await, Ok(Ok(s)) if s.maintenance == Some(true) && s.pid == Some(pid) && s.version.as_deref() == Some(version))
    }
}

impl Services for Host<'_> {
    fn mark_pending(&self, journal: &Path) -> Result<()> {
        Ok(ictl_daemon::maintenance::mark_pending(journal)?)
    }
    fn exclusive(&self) -> Result<File> {
        ictl_daemon::maintenance::exclusive()
            .context("busy; retry in a minute (the automatic job tries again next hour)")
    }

    async fn idle(&self, current: &Receipt) -> Result<bool> {
        service::managed(self.binary, &current.service)?;
        let pid = service::managed_pid(&self.service, false)?;
        check_clients(pid)?;
        if let Some(pid) = pid {
            ensure!(
                self.socket_owner().await? == Some(pid) && self.healthy(pid, &current.version).await,
                "busy: service and socket identity do not match"
            );
        } else {
            ensure!(self.socket_owner().await?.is_none(), "busy: a foreground daemon is running");
        }
        Ok(pid.is_some())
    }

    async fn stop(&self) -> Result<()> {
        let pid = service::managed_pid(&self.service, true)?;
        ensure!(pid.is_none_or(|p| self.owns_pid(p)), "service PID belongs to another executable");
        if let Some(peer) = self.socket_owner().await? {
            ensure!(Some(peer) == pid, "busy: socket is served by a different process");
        }
        service::stop(&self.service)?;
        let end = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            let gone = pid.is_none_or(|p| unsafe { libc::kill(p as i32, 0) } < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
            if gone
                && service::managed_pid(&self.service, false).is_ok_and(|p| p.is_none())
                && self.socket_owner().await?.is_none()
            {
                return Ok(());
            }
            ensure!(tokio::time::Instant::now() < end, "timed out waiting for daemon exit");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn start_healthy(&self, version: &str) -> Result<()> {
        service::start_managed(&self.service)?;
        let mut first = None;
        let poll = async {
            loop {
                let mut seen = None;
                if let Ok(Some(pid)) = service::managed_pid(&self.service, false)
                    && self.healthy(pid, version).await
                {
                    if first == Some(pid) {
                        return;
                    }
                    seen = Some(pid);
                }
                first = seen;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(60), poll).await.context("daemon health check timed out")?;
        Ok(())
    }
}

async fn restore(store: &Store, host: &impl Services, tx: &Transaction, why: &str) -> Result<()> {
    let current = digest(&store.binary)?;
    ensure!(current == tx.old.sha256 || current == tx.new.sha256, "installed digest unknown; recovery stopped");
    let backup = store.dir.join("recovery");
    ensure!(digest(&backup)? == tx.old.sha256, "recovery executable digest mismatch");
    host.stop().await?;
    if current != tx.old.sha256 {
        store.link(&backup, &store.binary)?;
    }
    if tx.was_running {
        host.start_healthy(&tx.old.version).await?;
    }
    write_json(&store.receipt, &tx.old)?;
    store.record("restored", &tx.new.version, why, Some(tx.new.version.clone()), None)?;
    store.clear()
}

async fn replace(
    store: &Store,
    host: &impl Services,
    old: Receipt,
    new: Receipt,
    candidate: &Path,
    rollback: bool,
    auto: bool,
) -> Result<()> {
    let activity = async {
        let lease = host.exclusive()?;
        let running = host.idle(&old).await?;
        ensure!(running || !auto, "busy: managed service is stopped");
        Ok::<_, anyhow::Error>((lease, running))
    }
    .await;
    let (_lease, running) = match activity {
        Ok(activity) => activity,
        Err(e) => {
            let suppressed = outcome(&store.binary)?.and_then(|o| o.suppression_for(&old));
            store.record("busy", &new.version, format!("{e:#}"), suppressed, None)?;
            return Err(e);
        }
    };
    if auto {
        ensure!(matches!(super::cached()?.policy, super::Policy::AutoPatch), "automatic installation was disabled");
    }
    validate_receipt(&old, &store.binary)?;
    ensure!(digest(candidate)? == new.sha256, "staged executable changed");
    eprintln!(
        "{} → {} ({}; {})",
        old.version,
        new.version,
        old.service,
        if running { "idle, restarting daemon" } else { "stopped, verifying then restoring stopped state" }
    );
    host.mark_pending(&store.journal())?;
    store.link(&store.binary, &store.dir.join("recovery"))?;
    let mut tx = Transaction { old, new, was_running: running, rollback, phase: Phase::Prepared };
    write_json(&store.journal(), &tx)?;

    let swap = async {
        if running {
            host.stop().await?;
        }
        store.link(candidate, &store.binary)?;
        tx.phase = Phase::Replaced;
        write_json(&store.journal(), &tx)?;
        host.start_healthy(&tx.new.version).await?;
        if !running {
            host.stop().await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(e) = swap {
        restore_or_report(store, host, &tx, &format!("{e:#}")).await?;
        bail!("update failed; restored {}: {e:#}", tx.old.version);
    }
    tx.phase = Phase::Confirmed;
    write_json(&store.journal(), &tx)?;
    store.finish(&tx)
}

async fn restore_or_report(store: &Store, host: &impl Services, tx: &Transaction, why: &str) -> Result<()> {
    if let Err(e) = restore(store, host, tx, why).await {
        let detail = format!(
            "{why}; recovery failed: {e:#}. Last working version {} at {}. Run update recover after resolving the error.",
            tx.old.version,
            store.dir.join("recovery").display()
        );
        store.record("recovery_failed", &tx.new.version, &detail, Some(tx.new.version.clone()), None)?;
        bail!(detail);
    }
    Ok(())
}

pub(super) fn eligible(old: &str, new: &str, automatic: bool) -> Result<()> {
    let old = Version::parse(old)?;
    let new = Version::parse(new)?;
    ensure!(old.pre.is_empty() && new.pre.is_empty() && new.cmp_precedence(&old).is_gt(), "no newer stable release");
    ensure!(
        !automatic || (old.major == new.major && old.minor == new.minor && old.patch < new.patch),
        "only patch releases can be installed automatically"
    );
    Ok(())
}

fn managed(receipt: &Receipt) -> Result<(Store, Service)> {
    validate_receipt(receipt, &std::env::current_exe()?)?;
    ensure!(receipt.version == env!("CARGO_PKG_VERSION"), "running updater version differs from installed version");
    let store = Store::open(&receipt.binary_path, receipt_path())?;
    let reread = read_receipt()?;
    ensure!(
        same_install(receipt, &reread)
            && reread.sha256 == receipt.sha256
            && reread.version == receipt.version
            && reread.pinned == receipt.pinned,
        "installation changed; retry"
    );
    validate_receipt(receipt, &store.binary)?;
    ensure!(!store.journal().try_exists()?, "interrupted update; run iphone-control update recover");
    let svc = service::managed(&store.binary, &receipt.service)?;
    Ok((store, svc))
}

pub async fn apply(socket: &Path, background: bool, report: super::Report) -> Result<()> {
    let receipt = read_receipt()?;
    let (store, service) = managed(&receipt)?;
    ensure!(!receipt.pinned, "this installation is pinned; unpin it locally first");
    ensure!(report.error_code.is_none(), "release check failed: {:?}", report.error_code);
    ensure!(matches!(report.availability, super::Availability::Available), "{}", report.guidance);
    let version = report.available_version.context("no stable release is available")?;
    eligible(&receipt.version, &version, background)?;
    let host = Host { service, socket, binary: &store.binary };
    if background {
        ensure!(matches!(report.policy, super::Policy::AutoPatch), "automatic installation is disabled");
        let suppressed = outcome(&store.binary)?.and_then(|o| o.suppression_for(&receipt));
        ensure!(
            suppressed.as_ref() != Some(&version),
            "automatic retry suppressed for {version}; run iphone-control update to retry"
        );
        // An always-open client may stay busy for days. The lease here is deliberately
        // short-lived; replacement reacquires it and rechecks identity after download.
        if let Err(e) = host.exclusive() {
            store.record("busy", &version, format!("{e:#}"), suppressed, None)?;
            return Err(e);
        }
    }
    let stage = store.stage()?;
    let work = async {
        let candidate = match archive::stage(&stage, &receipt, &version, background).await {
            Ok(candidate) => candidate,
            Err(e) => {
                let suppressed = if e.downcast_ref::<reqwest::Error>().is_some() {
                    outcome(&store.binary)?.and_then(|o| o.suppression_for(&receipt))
                } else {
                    Some(version.clone())
                };
                let status = failure_status(&e);
                let source = e.is::<archive::ManualMigration>().then(|| MigrationSource::from_receipt(&receipt));
                store.record(status, &version, format!("{e:#}"), suppressed, source)?;
                return Err(e);
            }
        };
        let next = Receipt { version, sha256: digest(&candidate)?, ..receipt.clone() };
        replace(&store, &host, receipt, next, &candidate, false, background).await
    }
    .await;
    let _ = fs::remove_dir_all(&stage);
    work
}

/// The outcome recorded when staging a release fails. A release that only isn't marked for unattended
/// installation is no failure: the automatic job leaves it to `update apply` (and doesn't download it again)
fn failure_status(e: &anyhow::Error) -> &'static str {
    if e.is::<archive::ManualMigration>() {
        "manual_migration"
    } else if e.is::<archive::NeedsOperator>() {
        "needs_operator"
    } else {
        "failed"
    }
}

pub fn unpin() -> Result<()> {
    let mut receipt = read_receipt()?;
    let (store, _) = managed(&receipt)?;
    receipt.pinned = false;
    write_json(&store.receipt, &receipt)
}

pub async fn rollback(socket: &Path) -> Result<()> {
    let receipt = read_receipt()?;
    let (store, service) = managed(&receipt)?;
    let previous: Previous =
        read_json(&store.dir.join("previous.json"))?.context("no compatible previous installation")?;
    ensure!(
        receipt.sha256 == previous.before_digest && same_install(&receipt, &previous.receipt),
        "rollback belongs to a different installation"
    );
    let candidate = store.dir.join("previous");
    ensure!(digest(&candidate)? == previous.receipt.sha256, "previous executable digest mismatch");
    archive::preflight(&candidate, &previous.receipt.version).await?;
    let host = Host { service, socket, binary: &store.binary };
    replace(&store, &host, receipt, previous.receipt, &candidate, true, false).await
}

fn same_install(a: &Receipt, b: &Receipt) -> bool {
    a.schema == b.schema
        && a.binary_path == b.binary_path
        && a.publisher == b.publisher
        && a.target == b.target
        && a.service == b.service
}

async fn resume(store: &Store, host: &impl Services, tx: &Transaction) -> Result<()> {
    let _maintenance = host.exclusive()?;
    host.mark_pending(&store.journal())?;
    let current = digest(&store.binary)?;
    if matches!(tx.phase, Phase::Confirmed) {
        ensure!(current == tx.new.sha256, "confirmed executable changed; preserving transaction");
        store.finish(tx)
    } else {
        restore_or_report(store, host, tx, "interrupted installation").await
    }
}

pub async fn recover(socket: &Path, scheduled: bool) -> Result<()> {
    let receipt = read_receipt()?;
    let store = Store::open(&receipt.binary_path, receipt_path())?;
    let Some(tx) = read_json::<Transaction>(&store.journal())? else { return Ok(()) };
    if scheduled
        && outcome(&store.binary)?.is_some_and(|o| o.status == "recovery_failed" && o.target_version == tx.new.version)
    {
        bail!("automatic recovery paused; resolve the recorded error and run update recover");
    }
    ensure!(
        same_install(&receipt, &tx.old)
            && same_install(&receipt, &tx.new)
            && (receipt.sha256 == tx.old.sha256 || receipt.sha256 == tx.new.sha256),
        "journal and receipt do not match"
    );
    let caller = std::env::current_exe()?.canonicalize()?;
    let backup = store.dir.join("recovery");
    ensure!(
        caller == store.binary || (caller == backup && digest(&backup)? == tx.old.sha256),
        "invalid recovery entry point"
    );
    let service = service::managed(&store.binary, &receipt.service)?;
    check_clients(service::managed_pid(&service, true)?)?;
    let host = Host { service, socket, binary: &store.binary };
    resume(&store, &host, &tx).await
}

#[cfg(test)]
mod tests;
