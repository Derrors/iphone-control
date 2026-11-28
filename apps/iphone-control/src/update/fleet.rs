//! An update with named instances (docs/dev/multi-instance-plan.md 5.3): every instance's daemon and control page go
//! through it together, not the default instance's alone.
//!
//! - **Frozen inventory**: the registered instances, which of their daemons and viewers run now. Only those are
//!   started again afterwards, the stopped ones stay stopped.
//! - **The record** (`<data root>/update-fleet.json`) is written first, with the viewers to start again, before the
//!   **intent** (`<data root>/update-intent`): whenever the intent exists, a record says what it belongs to. With the
//!   intent, the daemons refuse to open a task ("an update is being prepared"); the updater waits for the tasks being
//!   opened at that moment (`Request::Daemon`'s count) and refuses while any instance has an active task.
//! - It is kept apart from the transaction's journal: while that exists, `maintenance.pending` refuses every shared
//!   lease, the viewers' too, so the viewers to start again after the commit must be known without it.
//! - `<data root>/updater.lock` is held for the whole update: `update recover` that gets it knows no updater is
//!   alive, and picks up from the record ([`recover`]).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::service;

const RECORD: &str = "update-fleet.json";
const INTENT: &str = "update-intent";
const LOCK: &str = "updater.lock";
/// How long tasks being opened as the intent is written are waited for
const OPENING_WAIT: Duration = Duration::from_secs(10);
/// How long a named instance's daemon gets to answer again
const START_WAIT: Duration = Duration::from_secs(60);

/// A named instance as the update found it
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Member {
    pub name: String,
    pub socket: PathBuf,
    pub daemon_running: bool,
    pub viewer_running: bool,
}

/// Where the update is ([`Record::phase`])
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    /// The intent is (about to be) written; nothing stopped
    Intent,
    /// Services are being stopped or are stopped, the program not yet replaced (or being put back)
    Stopping,
    /// A package manager runs, replacing the program (APT, Homebrew)
    Package,
    /// The program is the new one, confirmed; the services to start again are left
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub phase: Phase,
    pub target: String,
    pub members: Vec<Member>,
    /// The default instance's viewer was running
    pub default_viewer: bool,
}

/// The update's hold on the host: the updater's lock, the record and the intent. Dropped without [`Self::finish`],
/// the record and the intent stay for `update recover`
pub(crate) struct Fleet {
    root: PathBuf,
    pub record: Record,
    _lock: File,
}

fn root() -> PathBuf {
    ictl_daemon::Config::from_env(&ictl_daemon::protocol::default_data_dir()).root
}

/// The updater's lock; `None` when another updater holds it
fn take_lock(root: &Path) -> Result<Option<File>> {
    std::fs::create_dir_all(root)?;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join(LOCK))?;
    // SAFETY: a valid descriptor, owned by `f` for as long as the lock is held
    Ok((unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f))
}

fn read_record(root: &Path) -> Result<Option<Record>> {
    match std::fs::read(root.join(RECORD)) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write(root: &Path, name: &str, data: &[u8]) -> Result<()> {
    ictl_daemon::instance::write_file(root, name, data).with_context(|| root.join(name).display().to_string())
}

fn remove(root: &Path, name: &str) -> Result<()> {
    match std::fs::remove_file(root.join(name)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(std::fs::File::open(root)?.sync_all()?),
    }
}

/// The registered instances and what of theirs runs now
fn inventory() -> Result<Vec<Member>> {
    let scan = ictl_daemon::instance::scan(&ictl_daemon::instance::registry_dir()?)?;
    Ok(scan
        .registered
        .into_iter()
        .map(|r| {
            let (d, v) = service::instance_services(&r.name);
            Member {
                name: r.name,
                socket: r.managed.socket,
                daemon_running: service::running(&d),
                viewer_running: service::running(&v),
            }
        })
        .collect())
}

impl Fleet {
    /// The update begins: lock, inventory, record, intent; then the tasks of every instance checked (`default_socket`
    /// is the default instance's). `None`: no named instance, nothing to do beyond the single service of before
    pub async fn begin(default_socket: &Path, target: &str, default_viewer: bool) -> Result<Option<Self>> {
        let members = inventory()?;
        if members.is_empty() {
            return Ok(None);
        }
        let root = root();
        let lock = take_lock(&root)?.context(ictl_l10n::tr!("另一个升级正在进行", "another update is under way"))?;
        if let Some(r) = read_record(&root)? {
            bail!(
                "{}",
                ictl_l10n::trf!(
                    "上一次升级（{}）没收尾：先运行 iphone-control update recover",
                    "The last update ({}) wasn't finished: run iphone-control update recover first",
                    r.target
                )
            );
        }
        let record = Record { phase: Phase::Intent, target: target.into(), members, default_viewer };
        write(&root, RECORD, &serde_json::to_vec_pretty(&record)?)?;
        write(&root, INTENT, target.as_bytes())?;
        let fleet = Self { root, record, _lock: lock };
        if let Err(e) = fleet.check_tasks(default_socket).await {
            fleet.finish()?;
            return Err(e);
        }
        Ok(Some(fleet))
    }

    /// No instance opening a task, none with one active: the intent keeps it so
    async fn check_tasks(&self, default_socket: &Path) -> Result<()> {
        let sockets: Vec<(String, PathBuf)> =
            std::iter::once((ictl_daemon::instance::DEFAULT.to_owned(), default_socket.to_owned()))
                .chain(self.record.members.iter().map(|m| (m.name.clone(), m.socket.clone())))
                .collect();
        for (name, socket) in &sockets {
            let running = self.record.members.iter().find(|m| &m.name == name).is_none_or(|m| m.daemon_running);
            let deadline = tokio::time::Instant::now() + OPENING_WAIT;
            loop {
                match tokio::time::timeout(Duration::from_secs(2), crate::client::snapshot(socket)).await {
                    Ok(Ok(s)) if s.task_opening == 0 => break,
                    Ok(Ok(_)) if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(200)).await
                    }
                    Ok(Ok(_)) => bail!(
                        "{}",
                        ictl_l10n::trf!(
                            "实例 {name} 一直在开任务，没有升级",
                            "The instance {name} keeps opening a task; not updated"
                        )
                    ),
                    // the default instance may well not run; a named one recorded as running must answer
                    _ if !running || name == ictl_daemon::instance::DEFAULT => break,
                    _ => bail!(
                        "{}",
                        ictl_l10n::trf!(
                            "实例 {name} 的服务在运行，控制接口却连不上，没有升级",
                            "The instance {name}'s service runs but its socket doesn't answer; not updated"
                        )
                    ),
                }
            }
            if let Ok(list) = ictl_daemon::protocol::task_list(socket).await
                && list.iter().any(|t| t.active())
            {
                bail!(
                    "{}",
                    ictl_l10n::trf!(
                        "实例 {name} 有任务在进行，没有升级（--yes 也不跳过）",
                        "The instance {name} has a task under way; not updated (--yes doesn't skip this)"
                    )
                );
            }
        }
        Ok(())
    }

    /// Moves the record on
    pub fn at(&mut self, phase: Phase) -> Result<()> {
        self.record.phase = phase;
        write(&self.root, RECORD, &serde_json::to_vec_pretty(&self.record)?)
    }

    /// The named instances' viewers and daemons stopped (the viewers first: they hold the maintenance lease), each
    /// confirmed stopped
    pub fn stop_all(&mut self) -> Result<()> {
        self.at(Phase::Stopping)?;
        for m in &self.record.members {
            let (d, v) = service::instance_services(&m.name);
            for s in [&v, &d] {
                if service::running(s) {
                    service::stop(s)?;
                }
            }
        }
        for _ in 0..150 {
            if self.record.members.iter().all(|m| {
                let (d, v) = service::instance_services(&m.name);
                !service::running(&d) && !service::running(&v)
            }) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        bail!("{}", ictl_l10n::tr!("命名实例的服务没有停下", "the named instances' services didn't stop"))
    }

    /// The named daemons that were running started again, each checked to answer as itself with `version`
    pub async fn start_daemons(&self, version: &str) -> Result<()> {
        start_daemons(&self.record.members, version).await
    }

    /// The viewers that were running started again; one that doesn't is reported, not undone
    pub fn start_viewers(&self) {
        start_viewers(&self.record);
    }

    /// Done: the intent and the record go, new tasks can be opened again
    pub fn finish(&self) -> Result<()> {
        remove(&self.root, INTENT)?;
        remove(&self.root, RECORD)
    }
}

async fn start_daemons(members: &[Member], version: &str) -> Result<()> {
    for m in members.iter().filter(|m| m.daemon_running) {
        let (d, _) = service::instance_services(&m.name);
        service::start_managed(&d)?;
    }
    for m in members.iter().filter(|m| m.daemon_running) {
        let deadline = tokio::time::Instant::now() + START_WAIT;
        loop {
            let ok = matches!(
                tokio::time::timeout(Duration::from_secs(2), crate::client::snapshot(&m.socket)).await,
                Ok(Ok(s)) if s.version.as_deref() == Some(version) && s.instance.as_ref().is_some_and(|i| i.name == m.name)
            );
            if ok {
                break;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "{}",
                ictl_l10n::trf!(
                    "实例 {} 的守护进程没有以 {version} 起来",
                    "The instance {}'s daemon didn't come up with {version}",
                    m.name
                )
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    Ok(())
}

fn start_viewers(record: &Record) {
    for m in record.members.iter().filter(|m| m.viewer_running) {
        let (_, v) = service::instance_services(&m.name);
        if let Err(e) = service::start_managed(&v) {
            eprintln!(
                "{}",
                ictl_l10n::trf!(
                    "程序已升级，实例 {} 的控制台没有恢复：{e:#}（update recover 可以再试）",
                    "The program was updated, but the instance {}'s viewer wasn't started again: {e:#} (update recover tries again)",
                    m.name
                )
            );
        }
    }
    if record.default_viewer
        && let Some(v) = service::viewer_find()
        && let Err(e) = service::start_managed(&v)
    {
        eprintln!("{e:#}");
    }
}

/// `update recover`'s part for the instances, once the transaction of the program itself is settled (put back or
/// committed) and no updater holds the lock: what the record says is started again, then the intent and the record
/// go. A package manager that may still run (an interrupted `apt`, `brew`) leaves it pending
pub(crate) async fn recover(installed: &str) -> Result<()> {
    let root = root();
    let Some(_lock) = take_lock(&root)? else {
        bail!("{}", ictl_l10n::tr!("升级程序还在运行，没有动手", "An updater is still running; nothing done"))
    };
    let Some(record) = read_record(&root)? else {
        // an intent without a record: written by nothing this version does; it would block every new task
        return remove(&root, INTENT);
    };
    if record.phase == Phase::Package {
        ensure!(
            !package_manager_running(),
            "{}",
            ictl_l10n::tr!(
                "包管理器还在运行：等它结束再 update recover",
                "The package manager is still running: let it finish, then run update recover"
            )
        );
        if installed != record.target {
            eprintln!(
                "{}",
                ictl_l10n::trf!(
                    "装上的是 {installed}，不是要升级到的 {}：按原来的状态恢复服务；要升级就再运行 iphone-control update",
                    "{installed} is installed, not {} the update was for: the services are started as they were; run iphone-control update to update",
                    record.target
                )
            );
        }
    }
    // the intent first: until it goes, no instance opens a task, and a viewer restored would find none to run
    remove(&root, INTENT)?;
    if record.phase != Phase::Intent {
        start_daemons(&record.members, installed).await?;
    }
    start_viewers(&record);
    remove(&root, RECORD)
}

/// An `apt`, `dpkg` or `brew` process of this machine is running
fn package_manager_running() -> bool {
    let Ok(out) = std::process::Command::new("ps").args(["-A", "-o", "comm="]).env("LC_ALL", "C").output() else {
        return true;
    };
    String::from_utf8_lossy(&out.stdout).lines().any(|c| {
        let c = Path::new(c.trim()).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        matches!(c.as_str(), "apt" | "apt-get" | "dpkg" | "brew" | "unattended-upgr")
    })
}
