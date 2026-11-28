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
//!   alive, and picks up from the record ([`recover`]). Before it, `registry.lock` (the order every command that
//!   changes the registry or the services keeps): the inventory doesn't change under the update.
//! - The named daemons are part of the program's transaction (`transaction.rs`): the new program is committed only
//!   once they answer with it too, and put back, with them started again on the old one, when they don't.
//! - The record stays until every viewer it lists is running again: one that doesn't start is left for `update
//!   recover` to start, while the intent is gone already (tasks can be opened again).

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
    /// Its control page's port: a viewer counts as started again once it answers there ([`viewer_answers`])
    #[serde(default)]
    pub port: u16,
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
    #[serde(default = "default_port")]
    pub default_port: u16,
}

fn default_port() -> u16 {
    std::env::var(ictl_daemon::protocol::env::VIEWER_PORT).ok().and_then(|p| p.parse().ok()).unwrap_or(8200)
}

/// How long a viewer started again gets to answer on its port
const VIEWER_WAIT: Duration = Duration::from_secs(20);

/// Whose viewer answers on `port`, without logging in (the login page says, `X-Iphone-Control: viewer` and
/// `X-Iphone-Control-Instance: <name> <id>`): `(name, id)`; `None` when nothing does, or something else. A service
/// manager reporting the service started isn't enough: the process may still fail to bind its port (taken by
/// something else). No token is sent: what listens there isn't known yet
pub(crate) async fn viewer_identity(port: u16) -> Option<(String, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ask = async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        s.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await.ok()?;
        let mut buf = vec![0u8; 8192];
        let mut n = 0;
        let end = loop {
            if let Some(e) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
                break e;
            }
            if n == buf.len() {
                return None;
            }
            let got = s.read(&mut buf[n..]).await.ok()?;
            if got == 0 {
                return None;
            }
            n += got;
        };
        parse_identity(&String::from_utf8_lossy(&buf[..end]))
    };
    tokio::time::timeout(Duration::from_secs(2), ask).await.ok().flatten()
}

/// The identity in a response's head (the status line and headers, the body excluded)
fn parse_identity(head: &str) -> Option<(String, String)> {
    let mut lines = head.split("\r\n");
    if !lines.next()?.starts_with("HTTP/1.") {
        return None;
    }
    let (mut viewer, mut instance) = (false, None);
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("x-iphone-control") {
            viewer = v == "viewer";
        } else if k.eq_ignore_ascii_case("x-iphone-control-instance") {
            instance = v.split_once(' ').map(|(n, id)| (n.to_owned(), id.to_owned()));
        }
    }
    instance.filter(|_| viewer)
}

/// The viewer of the instance `name` answers on `port`
pub(crate) async fn viewer_answers(port: u16, name: &str) -> bool {
    viewer_identity(port).await.is_some_and(|(n, _)| n == name)
}

/// `s` started again and answering on `port` within [`VIEWER_WAIT`]
async fn viewer_up(s: &service::Service, port: u16, name: &str) -> bool {
    let deadline = tokio::time::Instant::now() + VIEWER_WAIT;
    while tokio::time::Instant::now() < deadline {
        if service::running(s) && viewer_answers(port, name).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// The update's hold on the host: the updater's lock, the record and the intent. Dropped without [`Self::finish`],
/// the record and the intent stay for `update recover`
pub(crate) struct Fleet {
    root: PathBuf,
    pub record: Record,
    _lock: File,
    _registry: File,
}

fn root() -> PathBuf {
    ictl_daemon::Config::from_env(&ictl_daemon::protocol::default_data_dir()).root
}

fn lock_registry() -> Result<File> {
    ictl_daemon::instance::lock_registry(&ictl_daemon::instance::registry_dir()?, Duration::from_secs(10)).context(
        ictl_l10n::tr!(
            "拿不到实例登记的锁（另一个 instance 命令在运行？）",
            "can't lock the instance registry (another instance command running?)"
        ),
    )
}

/// The named instances an interrupted update recorded, for `update recover` to stop and start with the program
pub(crate) fn recorded_members() -> Vec<Member> {
    read_record(&root()).ok().flatten().map(|r| r.members).unwrap_or_default()
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
                port: r.managed.port,
            }
        })
        .collect())
}

impl Fleet {
    /// The update begins: lock, inventory, record, intent; then the tasks of every instance checked (`default_socket`
    /// is the default instance's). `None`: no named instance, nothing to do beyond the single service of before
    pub async fn begin(default_socket: &Path, target: &str, default_viewer: bool) -> Result<Option<Self>> {
        let registry = lock_registry()?;
        let members = inventory()?;
        if members.is_empty() {
            return Ok(None);
        }
        // the socket given is the default instance's: one another instance answers on would leave the default
        // instance's tasks unchecked
        if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(2), crate::client::snapshot(default_socket)).await
            && let Some(i) = s.instance.filter(|i| i.name != ictl_daemon::instance::DEFAULT)
        {
            bail!(
                "{}",
                ictl_l10n::trf!(
                    "{} 上的是实例 {}：升级在默认实例上运行（不带 --instance、--socket）",
                    "{} is the instance {}: an update runs on the default instance (without --instance or --socket)",
                    default_socket.display(),
                    i.name
                )
            );
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
        let record = Record {
            phase: Phase::Intent,
            target: target.into(),
            members,
            default_viewer,
            default_port: default_port(),
        };
        write(&root, RECORD, &serde_json::to_vec_pretty(&record)?)?;
        write(&root, INTENT, target.as_bytes())?;
        let fleet = Self { root, record, _lock: lock, _registry: registry };
        if let Err(e) = fleet.check_tasks(default_socket).await {
            fleet.finish(true)?;
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
            // whether it answers: then its tasks must be known, a list that can't be read isn't "none"
            let answers = loop {
                match tokio::time::timeout(Duration::from_secs(2), crate::client::snapshot(socket)).await {
                    Ok(Ok(s)) if s.task_opening == 0 => break true,
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
                    _ if !running || name == ictl_daemon::instance::DEFAULT => break false,
                    _ => bail!(
                        "{}",
                        ictl_l10n::trf!(
                            "实例 {name} 的服务在运行，控制接口却连不上，没有升级",
                            "The instance {name}'s service runs but its socket doesn't answer; not updated"
                        )
                    ),
                }
            };
            if !answers {
                continue;
            }
            let list = ictl_daemon::protocol::task_list(socket).await.map_err(|e| {
                anyhow::anyhow!(
                    "{}",
                    ictl_l10n::trf!(
                        "查不了实例 {name} 的任务（{e}），没有升级",
                        "The instance {name}'s tasks can't be listed ({e}); not updated"
                    )
                )
            })?;
            if list.iter().any(|t| t.active()) {
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

    /// The named instances' viewers stopped and confirmed stopped: each holds the maintenance lease for as long as it
    /// runs. Their daemons are stopped only once the exclusive lease is held ([`stop_daemons`])
    pub fn stop_viewers(&mut self) -> Result<()> {
        self.at(Phase::Stopping)?;
        for m in &self.record.members {
            let v = service::instance_services(&m.name).1;
            if service::running(&v) {
                service::stop(&v)?;
            }
        }
        for _ in 0..150 {
            if self.record.members.iter().all(|m| !service::running(&service::instance_services(&m.name).1)) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        bail!("{}", ictl_l10n::tr!("命名实例的控制台没有停下", "the named instances' viewers didn't stop"))
    }

    /// The named daemons that were running started again, each checked to answer as itself with `version`
    pub async fn start_daemons(&self, version: &str) -> Result<()> {
        start_daemons(&self.record.members, version).await
    }

    /// The instances' members, for the program's transaction to stop and start with it
    pub fn members(&self) -> Vec<Member> {
        self.record.members.clone()
    }

    /// The viewers that were running started again; whether they all answer (one that doesn't is reported, not
    /// undone)
    pub async fn start_viewers(&self) -> bool {
        start_viewers(&self.record).await
    }

    /// Done: the intent goes, new tasks can be opened again; the record too, unless `viewers_up` is false (then
    /// `update recover` starts the viewers it lists)
    pub fn finish(&self, viewers_up: bool) -> Result<()> {
        remove(&self.root, INTENT)?;
        if viewers_up { remove(&self.root, RECORD) } else { Ok(()) }
    }
}

/// The named daemons stopped and confirmed stopped (the viewers are already)
pub(crate) async fn stop_daemons(members: &[Member]) -> Result<()> {
    for m in members {
        let (d, _) = service::instance_services(&m.name);
        if service::running(&d) {
            service::stop(&d)?;
        }
    }
    for _ in 0..150 {
        if members.iter().all(|m| !service::running(&service::instance_services(&m.name).0)) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("{}", ictl_l10n::tr!("命名实例的守护进程没有停下", "the named instances' daemons didn't stop"))
}

pub(crate) async fn start_daemons(members: &[Member], version: &str) -> Result<()> {
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

async fn start_viewers(record: &Record) -> bool {
    let mut up = true;
    for m in record.members.iter().filter(|m| m.viewer_running) {
        let (_, v) = service::instance_services(&m.name);
        let started = match service::start_managed(&v) {
            Ok(()) if viewer_up(&v, m.port, &m.name).await => Ok(()),
            Ok(()) => Err(anyhow::anyhow!(
                "{}",
                ictl_l10n::trf!(
                    "{} 秒内没在端口 {} 上应答",
                    "it didn't answer on the port {} within {} s",
                    VIEWER_WAIT.as_secs(),
                    m.port
                )
            )),
            Err(e) => Err(e),
        };
        if let Err(e) = started {
            up = false;
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
    {
        match service::start_managed(&v) {
            Ok(()) if viewer_up(&v, record.default_port, ictl_daemon::instance::DEFAULT).await => {}
            Ok(()) => {
                up = false;
                eprintln!(
                    "{}",
                    ictl_l10n::trf!(
                        "默认实例的控制台 {} 秒内没在端口 {} 上应答（update recover 可以再试）",
                        "The default instance's viewer didn't answer on the port {} within {} s (update recover tries again)",
                        VIEWER_WAIT.as_secs(),
                        record.default_port
                    )
                );
            }
            Err(e) => {
                up = false;
                eprintln!("{e:#}");
            }
        }
    }
    up
}

/// `update recover`'s part for the instances, once the transaction of the program itself is settled (put back or
/// committed) and no updater holds the lock: what the record says is started again, then the intent and the record
/// go. A package manager that may still run (an interrupted `apt`, `brew`) leaves it pending
pub(crate) async fn recover(installed: &str) -> Result<()> {
    let root = root();
    let _registry = lock_registry()?;
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
    if start_viewers(&record).await {
        remove(&root, RECORD)?;
    } else {
        bail!(
            "{}",
            ictl_l10n::tr!(
                "有控制台没起来，记录留着：处理后再运行 update recover",
                "A viewer didn't start, so the record stays: deal with it, then run update recover again"
            )
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Something else on the port (it took it first: the viewer couldn't bind) isn't a viewer started again; one
    /// whose login page says so is
    #[tokio::test]
    async fn a_viewer_counts_as_up_only_when_it_answers_as_one() {
        let serve = |reply: &'static [u8]| async move {
            let l = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = l.local_addr().unwrap().port();
            tokio::spawn(async move {
                while let Ok((mut c, _)) = l.accept().await {
                    let mut b = [0u8; 1024];
                    let _ = c.read(&mut b).await;
                    let _ = c.write_all(reply).await;
                }
            });
            port
        };
        let other = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
        assert!(!super::viewer_answers(other, "ipad").await, "another program on the port");
        // the header only in the body: not a viewer
        let body = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\nX-Iphone-Control: viewer\r\nX-Iphone-Control-Instance: ipad 1\r\n").await;
        assert!(!super::viewer_answers(body, "ipad").await, "a body isn't a head");
        let viewer = serve(
            b"HTTP/1.1 401 Unauthorized\r\nX-Iphone-Control: viewer\r\nX-Iphone-Control-Instance: ipad 3f2a\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(super::viewer_answers(viewer, "ipad").await);
        assert!(!super::viewer_answers(viewer, "default").await, "another instance's viewer");
        assert_eq!(super::viewer_identity(viewer).await, Some(("ipad".into(), "3f2a".into())));
        let free = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        assert!(!super::viewer_answers(free, "ipad").await, "nothing listening");
    }
}
