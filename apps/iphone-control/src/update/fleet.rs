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

use crate::service::{self, Service};

const RECORD: &str = "update-fleet.json";
const INTENT: &str = "update-intent";
const LOCK: &str = "updater.lock";
/// While a package manager replaces the program: what `maintenance.pending` points at, so nothing works with a phone
/// until `update recover` has seen the package manager done, even when the updater died and its child goes on
const PACKAGE: &str = "update-package";
/// How long tasks being opened as the intent is written are waited for
const OPENING_WAIT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(10) };
/// How long a named instance's daemon gets to answer again
const START_WAIT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(60) };
/// Tries 200 ms apart for services to stop
const STOP_TRIES: u32 = if cfg!(test) { 3 } else { 150 };

/// A named instance as the update found it
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Member {
    pub name: String,
    pub socket: PathBuf,
    pub daemon_running: bool,
    pub viewer_running: bool,
    /// Its control page's port: a viewer counts as started again once it answers there ([`viewer_identity`])
    #[serde(default)]
    pub port: u16,
    /// Its id and device, from the registration: the daemon started again must answer as that instance, with that
    /// device (a name can be used again, an id can't)
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub udid: String,
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
    /// The default instance's daemon service was running: started again by `update recover` after a package
    /// manager's run, which starts it again only once it is done
    #[serde(default)]
    pub default_daemon: bool,
}

fn default_port() -> u16 {
    std::env::var(ictl_daemon::protocol::env::VIEWER_PORT).ok().and_then(|p| p.parse().ok()).unwrap_or(8200)
}

/// How long a viewer started again gets to answer on its port
const VIEWER_WAIT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(20) };

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
#[cfg(test)]
pub(crate) async fn viewer_answers(port: u16, name: &str) -> bool {
    viewer_identity(port).await.is_some_and(|(n, _)| n == name)
}

/// What the update does with this machine: its services, daemons and viewers, and where it keeps its record. The
/// real one ([`Real`]); tests give a fake one, to go through the update's failures without services or phones
#[allow(async_fn_in_trait)]
pub(crate) trait Machine {
    /// The data root (the record, the intent, the updater's lock)
    fn root(&self) -> PathBuf;
    fn lock_registry(&self) -> Result<File>;
    /// The registered instances this update goes through, and what of theirs runs now
    fn inventory(&self) -> Result<Vec<Member>>;
    /// The default instance's daemon and viewer services, if set up
    fn default_daemon(&self) -> Option<Service>;
    fn default_viewer(&self) -> Option<Service>;
    fn default_socket(&self) -> PathBuf;
    fn services(&self, name: &str) -> (Service, Service);
    fn running(&self, s: &Service) -> bool;
    fn start(&self, s: &Service) -> Result<()>;
    fn stop(&self, s: &Service) -> Result<()>;
    fn restart(&self, s: &Service) -> Result<()>;
    async fn snapshot(&self, socket: &Path) -> Result<ictl_daemon::Snapshot>;
    async fn tasks(&self, socket: &Path) -> std::result::Result<Vec<ictl_daemon::protocol::TaskInfo>, String>;
    async fn viewer_identity(&self, port: u16) -> Option<(String, String)>;
    fn package_busy(&self) -> bool;
    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await
    }
}

/// This machine
pub(crate) struct Real;

impl Machine for Real {
    fn root(&self) -> PathBuf {
        ictl_daemon::Config::from_env(&ictl_daemon::protocol::default_data_dir()).root
    }

    fn lock_registry(&self) -> Result<File> {
        ictl_daemon::instance::lock_registry(&ictl_daemon::instance::registry_dir()?, Duration::from_secs(10)).context(
            ictl_l10n::tr!(
                "拿不到实例登记的锁（另一个 instance 命令在运行？）",
                "can't lock the instance registry (another instance command running?)"
            ),
        )
    }

    fn inventory(&self) -> Result<Vec<Member>> {
        let scan = ictl_daemon::instance::scan(&ictl_daemon::instance::registry_dir()?)?;
        let exe = service::stable_exe()?;
        scan.registered
            .into_iter()
            .map(|r| {
                // only the services of this installation: their unit names this program, and the processes running
                // now are this program (a unit changed since, not reloaded, could run another)
                let (d, v) = service::instance_services(&r.name);
                let runs_this = |s: &Service| {
                    !service::running(s) || service::main_pid(s).is_some_and(|pid| same_program(pid, &exe))
                };
                ensure!(
                    service::instance_runs(&r.name, &exe) && runs_this(&d) && runs_this(&v),
                    "{}",
                    ictl_l10n::trf!(
                        "实例 {} 的服务运行的不是 {}：没有升级（先 instance remove 掉它，或用它自己的程序升级）",
                        "The instance {}'s services don't run {}: not updated (instance remove it first, or update it with its own program)",
                        r.name,
                        exe.display()
                    )
                );
                Ok(Member {
                    name: r.name,
                    socket: r.managed.socket,
                    daemon_running: service::running(&d),
                    viewer_running: service::running(&v),
                    port: r.managed.port,
                    id: r.managed.id,
                    udid: r.managed.udid,
                })
            })
            .collect()
    }

    fn default_daemon(&self) -> Option<Service> {
        service::find()
    }

    fn default_viewer(&self) -> Option<Service> {
        service::viewer_find()
    }

    fn default_socket(&self) -> PathBuf {
        ictl_daemon::protocol::default_socket()
    }

    fn services(&self, name: &str) -> (Service, Service) {
        service::instance_services(name)
    }

    fn running(&self, s: &Service) -> bool {
        service::running(s)
    }

    fn start(&self, s: &Service) -> Result<()> {
        service::start_managed(s)
    }

    fn stop(&self, s: &Service) -> Result<()> {
        service::stop(s)
    }

    fn restart(&self, s: &Service) -> Result<()> {
        service::restart(s)
    }

    async fn snapshot(&self, socket: &Path) -> Result<ictl_daemon::Snapshot> {
        tokio::time::timeout(Duration::from_secs(2), crate::client::snapshot(socket))
            .await
            .map_err(|_| anyhow::anyhow!("no answer"))?
    }

    async fn tasks(&self, socket: &Path) -> std::result::Result<Vec<ictl_daemon::protocol::TaskInfo>, String> {
        ictl_daemon::protocol::task_list(socket).await
    }

    async fn viewer_identity(&self, port: u16) -> Option<(String, String)> {
        viewer_identity(port).await
    }

    fn package_busy(&self) -> bool {
        package_manager_busy()
    }
}

/// Process `pid` runs the program file `exe` (the same file: device and inode)
fn same_program(pid: u32, exe: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(p), Ok(e)) = (super::transaction::process_path(pid).and_then(|p| Ok(p.metadata()?)), exe.metadata()) else {
        return false;
    };
    p.dev() == e.dev() && p.ino() == e.ino()
}

/// `s` started again and answering on `port` as the viewer of `name` (with `id`) within [`VIEWER_WAIT`]
async fn viewer_up(m: &impl Machine, s: &Service, port: u16, name: &str, id: &str) -> bool {
    let deadline = tokio::time::Instant::now() + VIEWER_WAIT;
    while tokio::time::Instant::now() < deadline {
        if m.running(s) && m.viewer_identity(port).await.is_some_and(|(n, i)| n == name && (id.is_empty() || i == id)) {
            return true;
        }
        m.sleep(Duration::from_millis(50)).await;
    }
    false
}

/// The update's hold on the host: the updater's lock, the record and the intent. Dropped without [`Self::finish`],
/// the record and the intent stay for `update recover`
pub(crate) struct Fleet<M: Machine = Real> {
    m: M,
    root: PathBuf,
    pub record: Record,
    _lock: File,
    _registry: File,
}

/// The named instances an interrupted update recorded, for `update recover` to stop and start with the program
pub(crate) fn recorded_members() -> Vec<Member> {
    read_record(&Real.root()).ok().flatten().map(|r| r.members).unwrap_or_default()
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

/// The default instance's daemon service runs now; `None`: not known (no service found, a foreground daemon may run)
fn default_daemon_running(m: &impl Machine) -> Option<bool> {
    m.default_daemon().map(|s| m.running(&s))
}

/// A package manager is about to replace the program (with the maintenance lock's exclusive lease held): from now on
/// every shared lease is refused (`maintenance.pending`), until [`package_done`], or `update recover` once it sees
/// the package manager done. Holds if the updater dies and the package manager goes on
pub(crate) fn package_started() -> Result<()> {
    let root = Real.root();
    write(&root, PACKAGE, b"")?;
    Ok(ictl_daemon::maintenance::mark_pending(&root.join(PACKAGE))?)
}

/// The package manager is done (its command returned): leases may be had again
pub(crate) fn package_done() -> Result<()> {
    remove(&Real.root(), PACKAGE)
}

impl Fleet<Real> {
    /// The update begins: lock, inventory, record, intent; then the tasks of every instance checked (`default_socket`
    /// is the default instance's). `None`: no named instance, nothing to do beyond the single service of before
    pub async fn begin(default_socket: &Path, target: &str, default_viewer: bool) -> Result<Option<Self>> {
        Self::begin_with(Real, default_socket, target, default_viewer).await
    }
}

impl<M: Machine> Fleet<M> {
    pub(crate) async fn begin_with(
        m: M,
        default_socket: &Path,
        target: &str,
        default_viewer: bool,
    ) -> Result<Option<Self>> {
        let registry = m.lock_registry()?;
        let members = m.inventory()?;
        if members.is_empty() {
            return Ok(None);
        }
        // the socket given is the default instance's: one another instance answers on would leave the default
        // instance's tasks unchecked
        if let Ok(s) = m.snapshot(default_socket).await
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
        let root = m.root();
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
            default_daemon: default_daemon_running(&m).unwrap_or(false),
        };
        write(&root, RECORD, &serde_json::to_vec_pretty(&record)?)?;
        write(&root, INTENT, target.as_bytes())?;
        let fleet = Self { m, root, record, _lock: lock, _registry: registry };
        if let Err(e) = fleet.check_tasks(default_socket).await {
            fleet.finish(true)?;
            return Err(e);
        }
        Ok(Some(fleet))
    }

    /// No instance opening a task, none with one active: the intent keeps it so
    async fn check_tasks(&self, default_socket: &Path) -> Result<()> {
        let m = &self.m;
        let sockets: Vec<(String, PathBuf)> =
            std::iter::once((ictl_daemon::instance::DEFAULT.to_owned(), default_socket.to_owned()))
                .chain(self.record.members.iter().map(|m| (m.name.clone(), m.socket.clone())))
                .collect();
        for (name, socket) in &sockets {
            let running = self.record.members.iter().find(|m| &m.name == name).is_none_or(|m| m.daemon_running);
            let deadline = tokio::time::Instant::now() + OPENING_WAIT;
            // whether it answers: then its tasks must be known, a list that can't be read isn't "none"
            let answers = loop {
                match m.snapshot(socket).await {
                    Ok(s) if s.task_opening == 0 => break true,
                    Ok(_) if tokio::time::Instant::now() < deadline => m.sleep(Duration::from_millis(50)).await,
                    Ok(_) => bail!(
                        "{}",
                        ictl_l10n::trf!(
                            "实例 {name} 一直在开任务，没有升级",
                            "The instance {name} keeps opening a task; not updated"
                        )
                    ),
                    // not answering: fine only for one known not to run (a named one stopped, the default one with its
                    // service stopped); one running, or the default one whose state isn't known, fails closed
                    _ if name == ictl_daemon::instance::DEFAULT && default_daemon_running(m) == Some(false) => {
                        break false;
                    }
                    _ if name != ictl_daemon::instance::DEFAULT && !running => break false,
                    _ if name == ictl_daemon::instance::DEFAULT => bail!(
                        "{}",
                        ictl_l10n::tr!(
                            "默认实例的控制接口连不上，查不了它的任务，没有升级",
                            "The default instance's socket doesn't answer, so its tasks can't be checked; not updated"
                        )
                    ),
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
            let list = m.tasks(socket).await.map_err(|e| {
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
    pub async fn stop_viewers(&mut self) -> Result<()> {
        self.at(Phase::Stopping)?;
        let m = &self.m;
        for member in &self.record.members {
            let v = m.services(&member.name).1;
            if m.running(&v) {
                m.stop(&v)?;
            }
        }
        for _ in 0..STOP_TRIES {
            if self.record.members.iter().all(|member| !m.running(&m.services(&member.name).1)) {
                return Ok(());
            }
            m.sleep(Duration::from_millis(200)).await;
        }
        bail!("{}", ictl_l10n::tr!("命名实例的控制台没有停下", "the named instances' viewers didn't stop"))
    }

    /// The named daemons that were running started again, each checked to answer as itself with `version`
    pub async fn start_daemons(&self, version: &str) -> Result<()> {
        start_daemons_with(&self.m, &self.record.members, version).await
    }

    /// The instances' members, for the program's transaction to stop and start with it
    pub fn members(&self) -> Vec<Member> {
        self.record.members.clone()
    }

    /// The viewers that were running started again; whether they all answer (one that doesn't is reported, not
    /// undone)
    pub async fn start_viewers(&self) -> bool {
        start_viewers(&self.m, &self.record).await
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
    stop_daemons_with(&Real, members).await
}

async fn stop_daemons_with(m: &impl Machine, members: &[Member]) -> Result<()> {
    for member in members {
        let d = m.services(&member.name).0;
        if m.running(&d) {
            m.stop(&d)?;
        }
    }
    for _ in 0..STOP_TRIES {
        if members.iter().all(|member| !m.running(&m.services(&member.name).0)) {
            return Ok(());
        }
        m.sleep(Duration::from_millis(200)).await;
    }
    bail!("{}", ictl_l10n::tr!("命名实例的守护进程没有停下", "the named instances' daemons didn't stop"))
}

pub(crate) async fn start_daemons(members: &[Member], version: &str) -> Result<()> {
    start_daemons_with(&Real, members, version).await
}

async fn start_daemons_with(m: &impl Machine, members: &[Member], version: &str) -> Result<()> {
    for member in members.iter().filter(|x| x.daemon_running) {
        m.start(&m.services(&member.name).0)?;
    }
    for member in members.iter().filter(|x| x.daemon_running) {
        let deadline = tokio::time::Instant::now() + START_WAIT;
        loop {
            // as itself: its version, its instance and id, the device it is pinned to
            let ok = matches!(
                m.snapshot(&member.socket).await,
                Ok(s) if s.version.as_deref() == Some(version)
                    && s.instance.as_ref().is_some_and(|i| i.name == member.name && (member.id.is_empty() || i.id == member.id))
                    && (member.udid.is_empty() || s.pinned.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&member.udid)))
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
                    member.name
                )
            );
            m.sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(())
}

async fn start_viewers(m: &impl Machine, record: &Record) -> bool {
    let mut up = true;
    for member in record.members.iter().filter(|x| x.viewer_running) {
        let v = m.services(&member.name).1;
        let started = match m.start(&v) {
            Ok(()) if viewer_up(m, &v, member.port, &member.name, &member.id).await => Ok(()),
            Ok(()) => Err(anyhow::anyhow!(
                "{}",
                ictl_l10n::trf!(
                    "{} 秒内没在端口 {} 上应答",
                    "it didn't answer on the port {} within {} s",
                    VIEWER_WAIT.as_secs(),
                    member.port
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
                    member.name
                )
            );
        }
    }
    if record.default_viewer
        && let Some(v) = m.default_viewer()
    {
        let default = (ictl_daemon::instance::DEFAULT, ictl_daemon::instance::DEFAULT_ID);
        match m.start(&v) {
            Ok(()) if viewer_up(m, &v, record.default_port, default.0, default.1).await => {}
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
    recover_with(&Real, installed).await
}

async fn recover_with(m: &impl Machine, installed: &str) -> Result<()> {
    let root = m.root();
    let _registry = m.lock_registry()?;
    let Some(_lock) = take_lock(&root)? else {
        bail!("{}", ictl_l10n::tr!("升级程序还在运行，没有动手", "An updater is still running; nothing done"))
    };
    let busy = || {
        anyhow::anyhow!(
            "{}",
            ictl_l10n::tr!(
                "包管理器还在运行（或者状态不明）：等它结束再 update recover",
                "The package manager is still running (or its state isn't known): let it finish, then run update recover"
            )
        )
    };
    // a package manager's run the updater didn't see end: leases stay refused until it is done
    if root.join(PACKAGE).exists() {
        ensure!(!m.package_busy(), busy());
        remove(&root, PACKAGE)?;
    }
    let Some(record) = read_record(&root)? else {
        // an intent without a record: written by nothing this version does; it would block every new task
        return remove(&root, INTENT);
    };
    if record.phase == Phase::Package {
        ensure!(!m.package_busy(), busy());
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
        start_daemons_with(m, &record.members, installed).await?;
    }
    // after a package manager's run the default daemon is started again only once it is done: killed in between,
    // the old program may still run as the default instance
    if record.phase == Phase::Package
        && record.default_daemon
        && let Some(s) = m.default_daemon()
    {
        m.restart(&s)?;
        ensure!(
            default_answers_with(m, installed).await,
            "{}",
            ictl_l10n::trf!(
                "默认实例的守护进程没有以 {installed} 起来，记录留着：查看后再运行 update recover",
                "The default instance's daemon didn't come up with {installed}, so the record stays: look into it, then run update recover again"
            )
        );
    }
    if start_viewers(m, &record).await {
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

/// The default instance's daemon answers with `version`, within [`START_WAIT`]
async fn default_answers_with(m: &impl Machine, version: &str) -> bool {
    let socket = m.default_socket();
    let deadline = tokio::time::Instant::now() + START_WAIT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(s) = m.snapshot(&socket).await
            && s.version.as_deref() == Some(version)
        {
            return true;
        }
        m.sleep(Duration::from_millis(50)).await;
    }
    false
}

/// A package manager may still be at work, or left its work unfinished: an `apt`, `dpkg` or `brew` process runs, the
/// process list can't be read, dpkg's lock is held or dpkg was interrupted (`/var/lib/dpkg/updates` isn't empty), or a
/// Homebrew lock is held. Not known counts as busy: the update stays to be recovered
fn package_manager_busy() -> bool {
    let out = match std::process::Command::new("ps").args(["-A", "-o", "comm="]).env("LC_ALL", "C").output() {
        Ok(o) if o.status.success() => o,
        _ => return true,
    };
    if busy_in(&String::from_utf8_lossy(&out.stdout)) {
        return true;
    }
    let interrupted = std::fs::read_dir("/var/lib/dpkg/updates").is_ok_and(|mut d| d.next().is_some());
    interrupted || dpkg_locked().unwrap_or(true) || brew_locked()
}

/// Whether dpkg's locks are held: by `/proc/locks` (dpkg's lock files are root's alone, but their locks are listed
/// there with the file's device and inode). `Some(false)` where there is no dpkg; `None`: not known
fn dpkg_locked() -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let files: Vec<_> = ["/var/lib/dpkg/lock-frontend", "/var/lib/dpkg/lock"]
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| (libc::major(m.dev()) as u64, libc::minor(m.dev()) as u64, m.ino()))
        .collect();
    if files.is_empty() {
        return Some(false);
    }
    let locks = std::fs::read_to_string("/proc/locks").ok()?;
    Some(locks_hold(&locks, &files))
}

/// Whether `/proc/locks` lists a lock on one of `files` (major, minor, inode)
fn locks_hold(locks: &str, files: &[(u64, u64, u64)]) -> bool {
    locks.lines().any(|l| {
        // `1: POSIX  ADVISORY  WRITE 1234 08:02:131 0 EOF`: the file is the field `MAJ:MIN:INODE` (major and minor
        // in hexadecimal); a blocked waiter's line starts with `->` and names it too
        l.split_whitespace()
            .find_map(|w| {
                let mut p = w.split(':');
                let (a, b, c) = (p.next()?, p.next()?, p.next()?);
                if p.next().is_some() {
                    return None;
                }
                Some((u64::from_str_radix(a, 16).ok()?, u64::from_str_radix(b, 16).ok()?, c.parse::<u64>().ok()?))
            })
            .is_some_and(|f| files.contains(&f))
    })
}

/// Whether one of Homebrew's locks (`var/homebrew/locks/*.lock`, flocked while it works) is held
fn brew_locked() -> bool {
    use std::os::fd::AsRawFd;
    [
        "/opt/homebrew/var/homebrew/locks",
        "/usr/local/var/homebrew/locks",
        "/home/linuxbrew/.linuxbrew/var/homebrew/locks",
    ]
    .iter()
    .filter_map(|d| std::fs::read_dir(d).ok())
    .flatten()
    .flatten()
    .filter_map(|e| std::fs::File::open(e.path()).ok())
    // SAFETY: a valid descriptor, owned by the file; a shared lock taken and let go of at once
    .any(|f| unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0)
}

/// Whether `ps -A -o comm=` lists a package manager
fn busy_in(ps: &str) -> bool {
    ps.lines().any(|c| {
        let c = Path::new(c.trim()).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        matches!(c.as_str(), "apt" | "apt-get" | "dpkg" | "brew" | "unattended-upgr" | "unattended-upgrade")
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// What a daemon answers: version, instance name, instance id, pinned device
    type Answer = (String, String, String, Option<String>);

    /// A machine with no services or phones: what runs, what answers and how, set by each test
    #[derive(Default)]
    struct Fake {
        root: PathBuf,
        members: Vec<Member>,
        /// Services running, by their name
        running: RefCell<HashSet<String>>,
        /// Services that don't start
        no_start: HashSet<String>,
        /// What answers on each socket: (version, instance name, instance id, pinned device); none: nothing answers
        answers: RefCell<HashMap<PathBuf, Answer>>,
        /// Sockets whose task list can't be read; sockets with an active task
        tasks_fail: HashSet<PathBuf>,
        tasks_active: HashSet<PathBuf>,
        /// Viewer identities by port, once its service runs
        viewers: HashMap<u16, (String, String)>,
        /// The version the default daemon comes up with when restarted
        default_restarts_with: Option<String>,
        package_busy: bool,
        /// What was done, in order
        log: RefCell<Vec<String>>,
    }

    fn name(s: &Service) -> String {
        match s {
            Service::Systemd { unit, .. } => unit.clone(),
            Service::Launchd { label, .. } => label.clone(),
        }
    }

    fn svc(n: &str) -> Service {
        Service::Systemd { unit: n.into(), ours: true }
    }

    impl Machine for Fake {
        fn root(&self) -> PathBuf {
            self.root.clone()
        }
        fn lock_registry(&self) -> Result<File> {
            Ok(File::open("/dev/null")?)
        }
        fn inventory(&self) -> Result<Vec<Member>> {
            Ok(self.members.clone())
        }
        fn default_daemon(&self) -> Option<Service> {
            Some(svc("iphone-control"))
        }
        fn default_viewer(&self) -> Option<Service> {
            Some(svc("iphone-control-viewer"))
        }
        fn default_socket(&self) -> PathBuf {
            "/run/default.sock".into()
        }
        fn services(&self, n: &str) -> (Service, Service) {
            (svc(&format!("iphone-control@{n}")), svc(&format!("iphone-control-viewer@{n}")))
        }
        fn running(&self, s: &Service) -> bool {
            self.running.borrow().contains(&name(s))
        }
        fn start(&self, s: &Service) -> Result<()> {
            self.log.borrow_mut().push(format!("start {}", name(s)));
            ensure!(!self.no_start.contains(&name(s)), "can't start");
            self.running.borrow_mut().insert(name(s));
            Ok(())
        }
        fn stop(&self, s: &Service) -> Result<()> {
            self.log.borrow_mut().push(format!("stop {}", name(s)));
            self.running.borrow_mut().remove(&name(s));
            Ok(())
        }
        fn restart(&self, s: &Service) -> Result<()> {
            self.log.borrow_mut().push(format!("restart {}", name(s)));
            self.running.borrow_mut().insert(name(s));
            if let Some(v) = &self.default_restarts_with {
                self.answers
                    .borrow_mut()
                    .insert(self.default_socket(), (v.clone(), "default".into(), "default".into(), None));
            }
            Ok(())
        }
        async fn snapshot(&self, socket: &Path) -> Result<ictl_daemon::Snapshot> {
            let Some((version, name, id, pinned)) = self.answers.borrow().get(socket).cloned() else {
                bail!("not answering")
            };
            Ok(serde_json::from_value(serde_json::json!({
                "version": version, "state": {"state": "ready"}, "device": null, "pinned": pinned, "persisted": {},
                "auto_unlock": false, "unlock_disabled": false, "notify_configured": false, "log_tail": [],
                "instance": {"name": name, "id": id, "data_dir": "/d", "root": "/r", "socket": socket,
                    "boot_id": "b", "selection_generation": 0},
            }))?)
        }
        async fn tasks(&self, socket: &Path) -> std::result::Result<Vec<ictl_daemon::protocol::TaskInfo>, String> {
            if self.tasks_fail.contains(socket) {
                return Err("broken".into());
            }
            let state = if self.tasks_active.contains(socket) { "running" } else { "completed" };
            Ok(vec![
                serde_json::from_value(serde_json::json!({
                    "id": "t", "title": "t", "owner": "o", "udid": "u", "state": state, "started": 0
                }))
                .unwrap(),
            ])
        }
        async fn viewer_identity(&self, port: u16) -> Option<(String, String)> {
            self.viewers.get(&port).cloned()
        }
        fn package_busy(&self) -> bool {
            self.package_busy
        }
        async fn sleep(&self, d: Duration) {
            tokio::time::sleep(d.min(Duration::from_millis(5))).await
        }
    }

    fn member(n: &str, port: u16) -> Member {
        Member {
            name: n.into(),
            socket: format!("/run/{n}.sock").into(),
            daemon_running: true,
            viewer_running: true,
            port,
            id: format!("id-{n}"),
            udid: format!("UDID-{n}"),
        }
    }

    /// Two named instances, both answering on the old version, and the default one
    fn two() -> (Fake, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let f = Fake {
            root: tmp.path().to_owned(),
            members: vec![member("ipad", 8203), member("phone", 8204)],
            ..Fake::default()
        };
        for m in &f.members {
            f.answers
                .borrow_mut()
                .insert(m.socket.clone(), ("1.0.0".into(), m.name.clone(), m.id.clone(), Some(m.udid.clone())));
            f.running.borrow_mut().insert(format!("iphone-control@{}", m.name));
            f.running.borrow_mut().insert(format!("iphone-control-viewer@{}", m.name));
        }
        f.answers
            .borrow_mut()
            .insert("/run/default.sock".into(), ("1.0.0".into(), "default".into(), "default".into(), None));
        f.running.borrow_mut().insert("iphone-control".into());
        (f, tmp)
    }

    /// Only the default instance has a task under way: refused, nothing left behind (no intent: tasks open again)
    #[tokio::test]
    async fn a_task_on_the_default_instance_alone_refuses_the_update() {
        let (mut f, tmp) = two();
        f.tasks_active.insert("/run/default.sock".into());
        let e = Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false).await.err().unwrap();
        assert!(e.to_string().contains("default"), "{e}");
        assert!(!tmp.path().join(INTENT).exists() && !tmp.path().join(RECORD).exists());
    }

    /// The default instance's tasks can't be listed, or it doesn't answer while its service runs: refused (fails
    /// closed); stopped, it doesn't keep the update from going on
    #[tokio::test]
    async fn the_default_instance_s_tasks_not_known_refuse_the_update() {
        let (mut f, tmp) = two();
        f.tasks_fail.insert("/run/default.sock".into());
        assert!(Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false).await.is_err());
        assert!(!tmp.path().join(INTENT).exists());
        let (f, _tmp) = two();
        f.answers.borrow_mut().remove(Path::new("/run/default.sock"));
        assert!(Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false).await.is_err(), "running, silent");
        let (f, _tmp) = two();
        f.answers.borrow_mut().remove(Path::new("/run/default.sock"));
        f.running.borrow_mut().remove("iphone-control");
        let fleet = Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false).await.unwrap().unwrap();
        assert!(fleet.root.join(INTENT).exists(), "begun: the intent written");
    }

    /// A package manager's run done but the updater killed before it started the default daemon again: `update
    /// recover` waits for the package manager, then starts it again and checks its version, then the instances'
    #[tokio::test]
    async fn after_a_package_run_the_default_daemon_is_started_again() {
        let (mut f, tmp) = two();
        f.viewers.insert(8203, ("ipad".into(), "id-ipad".into()));
        f.viewers.insert(8204, ("phone".into(), "id-phone".into()));
        let mut fleet = Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false).await.unwrap().unwrap();
        fleet.stop_viewers().await.unwrap();
        stop_daemons_with(&fleet.m, &fleet.members()).await.unwrap();
        fleet.at(Phase::Package).unwrap();
        // the updater gone: its lock let go of
        let Fleet { mut m, _lock: lock, .. } = fleet;
        drop(lock);
        // the package manager replaced the program, the updater died: the default daemon still the old one
        m.package_busy = true;
        for mem in &m.members {
            m.answers
                .borrow_mut()
                .insert(mem.socket.clone(), ("1.1.0".into(), mem.name.clone(), mem.id.clone(), Some(mem.udid.clone())));
        }
        // what keeps leases refused while it runs (`package_started`), left by the updater that died
        std::fs::write(tmp.path().join(PACKAGE), b"").unwrap();
        assert!(recover_with(&m, "1.1.0").await.is_err(), "the package manager may still be at work");
        assert!(tmp.path().join(RECORD).exists() && tmp.path().join(PACKAGE).exists(), "leases still refused");
        m.package_busy = false;
        m.default_restarts_with = Some("1.1.0".into());
        recover_with(&m, "1.1.0").await.unwrap();
        assert!(m.log.borrow().contains(&"restart iphone-control".to_string()));
        assert!(!tmp.path().join(RECORD).exists() && !tmp.path().join(INTENT).exists());
        assert!(!tmp.path().join(PACKAGE).exists(), "leases may be had again");
    }

    /// A second viewer that can't listen on its port (something else took it) keeps the record, for `update recover`
    /// to start it again; once it answers as itself, the record goes
    #[tokio::test]
    async fn a_viewer_that_can_t_listen_keeps_the_record() {
        let (mut f, tmp) = two();
        f.viewers.insert(8203, ("ipad".into(), "id-ipad".into()));
        // on 8204: another instance's viewer (an old one, say), not phone's
        f.viewers.insert(8204, ("phone".into(), "another-id".into()));
        let mut fleet = Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false).await.unwrap().unwrap();
        fleet.stop_viewers().await.unwrap();
        fleet.at(Phase::Committed).unwrap();
        let up = fleet.start_viewers().await;
        assert!(!up);
        fleet.finish(up).unwrap();
        assert!(
            tmp.path().join(RECORD).exists() && !tmp.path().join(INTENT).exists(),
            "the record kept, tasks open again"
        );
        // the updater gone: its lock let go of
        let Fleet { mut m, _lock: lock, .. } = fleet;
        drop(lock);
        for mem in &m.members {
            m.answers
                .borrow_mut()
                .insert(mem.socket.clone(), ("1.1.0".into(), mem.name.clone(), mem.id.clone(), Some(mem.udid.clone())));
        }
        assert!(recover_with(&m, "1.1.0").await.is_err(), "still not phone's viewer");
        m.viewers.insert(8204, ("phone".into(), "id-phone".into()));
        recover_with(&m, "1.1.0").await.unwrap();
        assert!(!tmp.path().join(RECORD).exists());
    }

    /// A named daemon started again must answer as itself: another instance id, or another pinned device, isn't it
    #[tokio::test]
    async fn a_daemon_started_again_answers_as_itself() {
        let (f, _tmp) = two();
        let members = f.members.clone();
        start_daemons_with(&f, &members, "1.0.0").await.unwrap();
        f.answers
            .borrow_mut()
            .insert("/run/ipad.sock".into(), ("1.0.0".into(), "ipad".into(), "other".into(), Some("UDID-ipad".into())));
        assert!(start_daemons_with(&f, &members, "1.0.0").await.is_err(), "another id");
        f.answers
            .borrow_mut()
            .insert("/run/ipad.sock".into(), ("1.0.0".into(), "ipad".into(), "id-ipad".into(), Some("UDID-x".into())));
        assert!(start_daemons_with(&f, &members, "1.0.0").await.is_err(), "another device");
    }

    /// dpkg's lock is seen in `/proc/locks` by its file's device and inode, as a holder or a waiter
    #[test]
    fn dpkg_s_lock_is_found_in_proc_locks() {
        let locks = "1: POSIX  ADVISORY  WRITE 1234 08:02:131 0 EOF\n2: FLOCK  ADVISORY  WRITE 99 fd:01:7 0 EOF\n2: -> FLOCK  ADVISORY  WRITE 100 fd:01:9 0 EOF\n";
        assert!(locks_hold(locks, &[(8, 2, 131)]));
        assert!(locks_hold(locks, &[(0xfd, 1, 9)]), "a waiter names it too");
        assert!(!locks_hold(locks, &[(8, 2, 132)]));
        assert!(!locks_hold("", &[(8, 2, 131)]));
    }

    /// The package managers a recovery waits for, as `ps` names them
    #[test]
    fn a_package_manager_still_at_work_is_seen() {
        assert!(super::busy_in("bash\n/usr/bin/dpkg\nsshd\n"));
        assert!(super::busy_in("  apt-get\n"));
        assert!(super::busy_in("brew\n"));
        assert!(!super::busy_in("bash\nsystemd\niphone-control\n"));
    }

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
