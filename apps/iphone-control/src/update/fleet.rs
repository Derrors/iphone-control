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
    /// The default instance's socket, where it is checked to answer with the program installed
    #[serde(default)]
    pub default_socket: Option<PathBuf>,
    /// A Homebrew installation's prefix: its locks say whether `brew` is still at work
    #[serde(default)]
    pub brew_prefix: Option<PathBuf>,
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
    /// The default instance's daemon and viewer services, if set up: `Ok(None)` only when not set up for sure,
    /// `Err` when that can't be told
    fn default_daemon(&self) -> std::result::Result<Option<Service>, String>;
    fn default_viewer(&self) -> std::result::Result<Option<Service>, String>;
    /// Whether something listens on `socket`: `Some(false)` only when nothing does for sure (no socket file, the
    /// connection refused); `None` when that can't be told
    async fn listening(&self, socket: &Path) -> Option<bool>;
    fn default_socket(&self) -> PathBuf;
    fn services(&self, name: &str) -> (Service, Service);
    /// Whether `s` runs: `None` when its manager can't tell (never taken for stopped)
    fn state(&self, s: &Service) -> Option<bool>;
    fn start(&self, s: &Service) -> Result<()>;
    fn stop(&self, s: &Service) -> Result<()>;
    fn restart(&self, s: &Service) -> Result<()>;
    async fn snapshot(&self, socket: &Path) -> Result<ictl_daemon::Snapshot>;
    async fn tasks(&self, socket: &Path) -> std::result::Result<Vec<ictl_daemon::protocol::TaskInfo>, String>;
    async fn viewer_identity(&self, port: u16) -> Option<(String, String)>;
    /// A package manager may still be at work (`brew_prefix`: the Homebrew installation's, whose locks are looked at)
    fn package_busy(&self, brew_prefix: Option<&Path>) -> bool;
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
                // not known whether it runs: the update doesn't go on (it would neither check its process nor know
                // what to start again)
                let state = |s: &Service| {
                    service::state(s).with_context(|| {
                        ictl_l10n::trf!(
                            "查不了实例 {} 的服务状态，没有升级",
                            "The instance {}'s service state can't be read; not updated",
                            r.name
                        )
                    })
                };
                let (dr, vr) = (state(&d)?, state(&v)?);
                let runs_this = |s: &Service, running: bool| {
                    !running || service::main_pid(s).is_some_and(|pid| same_program(pid, &exe))
                };
                ensure!(
                    service::instance_runs(&r.name, &exe) && runs_this(&d, dr) && runs_this(&v, vr),
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
                    daemon_running: dr,
                    viewer_running: vr,
                    port: r.managed.port,
                    id: r.managed.id,
                    udid: r.managed.udid,
                })
            })
            .collect()
    }

    fn default_daemon(&self) -> std::result::Result<Option<Service>, String> {
        service::find_checked()
    }

    fn default_viewer(&self) -> std::result::Result<Option<Service>, String> {
        service::viewer_find_checked()
    }

    async fn listening(&self, socket: &Path) -> Option<bool> {
        match tokio::time::timeout(Duration::from_secs(2), tokio::net::UnixStream::connect(socket)).await {
            Ok(Ok(_)) => Some(true),
            Ok(Err(e)) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
                Some(false)
            }
            _ => None,
        }
    }

    fn default_socket(&self) -> PathBuf {
        ictl_daemon::protocol::default_socket()
    }

    fn services(&self, name: &str) -> (Service, Service) {
        service::instance_services(name)
    }

    fn state(&self, s: &Service) -> Option<bool> {
        service::state(s)
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

    fn package_busy(&self, brew_prefix: Option<&Path>) -> bool {
        package_manager_busy(brew_prefix)
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
        if m.state(s) == Some(true)
            && m.viewer_identity(port).await.is_some_and(|(n, i)| n == name && (id.is_empty() || i == id))
        {
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
    // tests: another test spawning a process holds a copy of a lock just let go of until that child execs (as in
    // `transaction::tests::open_once_free`): tried again for a moment there; once otherwise
    for _ in 0..if cfg!(test) { 100 } else { 1 } {
        // SAFETY: a valid descriptor, owned by `f` for as long as the lock is held
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(f));
        }
        if cfg!(test) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(None)
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

/// The default instance's daemon as the update sees it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Default {
    /// No service set up, for sure (a daemon may still run by hand: see its socket)
    Absent,
    Stopped,
    Running,
    /// Its service, or whether there is one, can't be told
    Unknown,
}

fn default_daemon_state(m: &impl Machine) -> Default {
    match m.default_daemon() {
        Err(_) => Default::Unknown,
        Ok(None) => Default::Absent,
        Ok(Some(s)) => match m.state(&s) {
            Some(true) => Default::Running,
            Some(false) => Default::Stopped,
            None => Default::Unknown,
        },
    }
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
        Self::begin_with(Real, default_socket, target, default_viewer, false).await
    }

    /// [`Self::begin`] for a package manager's run: a record also without named instances, of the default instance's
    /// services as they were (the package manager has no transaction of its own to start them again from)
    pub async fn begin_package(default_socket: &Path, target: &str, default_viewer: bool) -> Result<Self> {
        Self::begin_with(Real, default_socket, target, default_viewer, true)
            .await?
            .context("a record is always made for a package manager's run")
    }
}

impl<M: Machine> Fleet<M> {
    pub(crate) async fn begin_with(
        m: M,
        default_socket: &Path,
        target: &str,
        default_viewer: bool,
        always: bool,
    ) -> Result<Option<Self>> {
        let registry = m.lock_registry()?;
        let members = m.inventory()?;
        if members.is_empty() && !always {
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
            // its state not known: what to start again wouldn't be either
            default_daemon: match default_daemon_state(&m) {
                Default::Running => true,
                Default::Stopped | Default::Absent => false,
                Default::Unknown => bail!(
                    "{}",
                    ictl_l10n::tr!(
                        "查不了默认实例的服务状态，没有升级",
                        "The default instance's service state can't be read; not updated"
                    )
                ),
            },
            default_socket: Some(default_socket.to_owned()),
            brew_prefix: None,
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
                    // not answering: the default one may be stopped, or never set up as a service (a Homebrew install
                    // without `brew services`) with nothing listening on its socket; anything else can't be told
                    _ if name == ictl_daemon::instance::DEFAULT
                        && match default_daemon_state(m) {
                            // stopped or never set up: and nothing listening on its socket for sure (a daemon started
                            // by hand there, whose answer only timed out, isn't taken for none)
                            Default::Stopped | Default::Absent => m.listening(socket).await == Some(false),
                            Default::Running | Default::Unknown => false,
                        } =>
                    {
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

    /// The package manager is about to run (a Homebrew one with this prefix)
    pub fn package(&mut self, brew_prefix: Option<PathBuf>) -> Result<()> {
        self.record.brew_prefix = brew_prefix;
        self.at(Phase::Package)
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
            if m.state(&v) != Some(false) {
                m.stop(&v)?;
            }
        }
        for _ in 0..STOP_TRIES {
            if self.record.members.iter().all(|member| m.state(&m.services(&member.name).1) == Some(false)) {
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
        if m.state(&d) != Some(false) {
            m.stop(&d)?;
        }
    }
    for _ in 0..STOP_TRIES {
        if members.iter().all(|member| m.state(&m.services(&member.name).0) == Some(false)) {
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
    // was running, so must be started again: one that can't be found now (its definition gone, the manager not
    // answering) isn't taken for started
    let default_viewer = match m.default_viewer() {
        Ok(Some(v)) => Some(v),
        _ if !record.default_viewer => None,
        other => {
            eprintln!(
                "{}",
                ictl_l10n::trf!(
                    "默认实例的控制台原来在运行，现在找不到它的服务（{}）：没有恢复",
                    "The default instance's viewer was running, and its service can't be found now ({}): not started again",
                    other.err().unwrap_or_else(|| ictl_l10n::tr!("没有装", "not set up").into())
                )
            );
            return false;
        }
    };
    if record.default_viewer
        && let Some(v) = default_viewer
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
    let record = read_record(&root)?;
    let brew = record.as_ref().and_then(|r| r.brew_prefix.clone());
    // a package manager's run the updater didn't see end: leases stay refused until it is done
    if root.join(PACKAGE).exists() {
        ensure!(!m.package_busy(brew.as_deref()), busy());
        remove(&root, PACKAGE)?;
    }
    let Some(record) = record else {
        // an intent without a record: written by nothing this version does; it would block every new task
        return remove(&root, INTENT);
    };
    if record.phase == Phase::Package {
        ensure!(!m.package_busy(brew.as_deref()), busy());
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
    if record.phase == Phase::Package && record.default_daemon {
        // it was running: one that can't be found now isn't taken for started again
        let s = match m.default_daemon() {
            Ok(Some(s)) => s,
            other => bail!(
                "{}",
                ictl_l10n::trf!(
                    "默认实例的守护进程原来在运行，现在找不到它的服务（{}），记录留着：查看后再运行 update recover",
                    "The default instance's daemon was running, and its service can't be found now ({}), so the record stays: look into it, then run update recover again",
                    other.err().unwrap_or_else(|| ictl_l10n::tr!("没有装", "not set up").into())
                )
            ),
        };
        m.restart(&s)?;
        ensure!(
            default_answers_with(m, record.default_socket.as_deref(), installed).await,
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
async fn default_answers_with(m: &impl Machine, socket: Option<&Path>, version: &str) -> bool {
    let socket = socket.map(Path::to_owned).unwrap_or_else(|| m.default_socket());
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

/// A package manager may still be at work, or left its work unfinished: an `apt`, `dpkg` or `brew` process runs, dpkg
/// was interrupted (`/var/lib/dpkg/updates` isn't empty) or its lock is held, a Homebrew lock is held. Anything that
/// can't be read (the process list, a lock, a directory, other than one that isn't there) counts as busy: the update
/// stays to be recovered
fn package_manager_busy(brew_prefix: Option<&Path>) -> bool {
    let out = match std::process::Command::new("ps").args(["-A", "-o", "comm="]).env("LC_ALL", "C").output() {
        Ok(o) if o.status.success() => o,
        _ => return true,
    };
    if busy_in(&String::from_utf8_lossy(&out.stdout)) {
        return true;
    }
    let dpkg = Path::new("/var/lib/dpkg");
    let brew_locks = brew_prefix.map(|p| p.join("var/homebrew/locks"));
    let checks = [
        dir_not_empty(&dpkg.join("updates")),
        dpkg_locked(&[dpkg.join("lock-frontend"), dpkg.join("lock")], Path::new("/proc/locks")),
        brew_locks.as_deref().map_or(Some(false), brew_locked),
    ];
    checks.iter().any(|c| c.unwrap_or(true))
}

/// Whether a directory has anything in it; `Some(false)` when it isn't there, `None` when it can't be read
fn dir_not_empty(dir: &Path) -> Option<bool> {
    match std::fs::read_dir(dir) {
        Ok(mut d) => Some(d.next().is_some()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

/// Whether one of dpkg's lock `files` is held, by `locks` (`/proc/locks`: dpkg's lock files are root's alone, but
/// their locks are listed there with the file's device and inode). `Some(false)` where there is no dpkg; `None`: not
/// known (a file or the list that can't be read)
fn dpkg_locked(files: &[PathBuf], locks: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let mut ids = Vec::new();
    for f in files {
        match std::fs::metadata(f) {
            Ok(m) => ids.push(dev_parts(m.dev(), m.ino())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    if ids.is_empty() {
        return Some(false);
    }
    let list = std::fs::read_to_string(locks).ok()?;
    Some(locks_hold(&list, &ids))
}

/// A file's (major, minor, inode) as `/proc/locks` names it, from its device number as Linux encodes it (`/proc/locks`
/// is Linux's; elsewhere the lock files aren't there)
fn dev_parts(dev: u64, ino: u64) -> (u64, u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major, minor, ino)
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

/// Whether one of the Homebrew installation's locks (`<prefix>/var/homebrew/locks/*.lock`, flocked while `brew`
/// works) is held. `Some(false)` with no such directory; `None` when it, or a lock in it, can't be read or tried
fn brew_locked(dir: &Path) -> Option<bool> {
    use std::os::fd::AsRawFd;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    for e in entries {
        let f = std::fs::File::open(e.ok()?.path()).ok()?;
        // SAFETY: a valid descriptor, owned by `f`; a shared lock taken and let go of with it
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            return if err.kind() == std::io::ErrorKind::WouldBlock { Some(true) } else { None };
        }
    }
    Some(false)
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
        /// Services whose state their manager can't tell
        unknown: HashSet<String>,
        /// The default daemon's or viewer's service not set up (absent), or not known whether it is
        default_absent: bool,
        default_lookup_fails: bool,
        viewer_absent: bool,
        /// Sockets of which it can't be told whether anything listens on them
        listening_unknown: HashSet<PathBuf>,
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
        fn default_daemon(&self) -> std::result::Result<Option<Service>, String> {
            if self.default_lookup_fails {
                return Err("can't tell".into());
            }
            Ok((!self.default_absent).then(|| svc("iphone-control")))
        }
        fn default_viewer(&self) -> std::result::Result<Option<Service>, String> {
            if self.default_lookup_fails {
                return Err("can't tell".into());
            }
            Ok((!self.viewer_absent).then(|| svc("iphone-control-viewer")))
        }
        async fn listening(&self, socket: &Path) -> Option<bool> {
            if self.listening_unknown.contains(socket) {
                return None;
            }
            Some(self.answers.borrow().contains_key(socket))
        }
        fn default_socket(&self) -> PathBuf {
            "/run/default.sock".into()
        }
        fn services(&self, n: &str) -> (Service, Service) {
            (svc(&format!("iphone-control@{n}")), svc(&format!("iphone-control-viewer@{n}")))
        }
        fn state(&self, s: &Service) -> Option<bool> {
            if self.unknown.contains(&name(s)) {
                return None;
            }
            Some(self.running.borrow().contains(&name(s)))
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
        fn package_busy(&self, _: Option<&Path>) -> bool {
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
        let e = Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, false).await.err().unwrap();
        assert!(e.to_string().contains("default"), "{e}");
        assert!(!tmp.path().join(INTENT).exists() && !tmp.path().join(RECORD).exists());
    }

    /// The default instance's tasks can't be listed, or it doesn't answer while its service runs: refused (fails
    /// closed); stopped, it doesn't keep the update from going on
    #[tokio::test]
    async fn the_default_instance_s_tasks_not_known_refuse_the_update() {
        let (mut f, tmp) = two();
        f.tasks_fail.insert("/run/default.sock".into());
        assert!(Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, false).await.is_err());
        assert!(!tmp.path().join(INTENT).exists());
        let (f, _tmp) = two();
        f.answers.borrow_mut().remove(Path::new("/run/default.sock"));
        assert!(
            Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, false).await.is_err(),
            "running, silent"
        );
        let (f, _tmp) = two();
        f.answers.borrow_mut().remove(Path::new("/run/default.sock"));
        f.running.borrow_mut().remove("iphone-control");
        let fleet = Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, false).await.unwrap().unwrap();
        assert!(fleet.root.join(INTENT).exists(), "begun: the intent written");
    }

    /// A package manager's run done but the updater killed before it started the default daemon again: `update
    /// recover` waits for the package manager, then starts it again and checks its version, then the instances'
    #[tokio::test]
    async fn after_a_package_run_the_default_daemon_is_started_again() {
        let (mut f, tmp) = two();
        f.viewers.insert(8203, ("ipad".into(), "id-ipad".into()));
        f.viewers.insert(8204, ("phone".into(), "id-phone".into()));
        let mut fleet =
            Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, false).await.unwrap().unwrap();
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
        let mut fleet =
            Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, false).await.unwrap().unwrap();
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

    /// The default instance's service state not known (its manager can't be asked) while its socket doesn't answer:
    /// not taken for stopped, the update refused
    #[tokio::test]
    async fn a_service_state_not_known_isn_t_taken_for_stopped() {
        let (mut f, tmp) = two();
        f.answers.borrow_mut().remove(Path::new("/run/default.sock"));
        f.unknown.insert("iphone-control".into());
        assert!(Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, false).await.is_err());
        assert!(!tmp.path().join(INTENT).exists());
        // a named viewer whose state can't be told isn't taken for stopped either
        let (mut f, _tmp) = two();
        f.unknown.insert("iphone-control-viewer@ipad".into());
        let mut fleet =
            Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, false).await.unwrap().unwrap();
        assert!(fleet.stop_viewers().await.is_err());
    }

    /// No named instance, only the default one's services: a package manager's run still records them, and an
    /// updater killed after the package was installed and before it started them again leaves `update recover` to
    /// start the default daemon (checked on its version) and its viewer (checked as itself)
    #[tokio::test]
    async fn a_package_run_without_named_instances_is_recovered_too() {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = Fake { root: tmp.path().to_owned(), ..Fake::default() };
        f.answers
            .borrow_mut()
            .insert("/run/default.sock".into(), ("1.0.0".into(), "default".into(), "default".into(), None));
        f.running.borrow_mut().insert("iphone-control".into());
        f.running.borrow_mut().insert("iphone-control-viewer".into());
        f.viewers.insert(default_port(), ("default".into(), "default".into()));
        f.default_restarts_with = Some("1.1.0".into());
        assert!(Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", true, false).await.unwrap().is_none());
        let (mut f2, tmp) = {
            let mut f = Fake { root: tmp.path().to_owned(), ..Fake::default() };
            f.answers
                .borrow_mut()
                .insert("/run/default.sock".into(), ("1.0.0".into(), "default".into(), "default".into(), None));
            f.running.borrow_mut().insert("iphone-control".into());
            f.running.borrow_mut().insert("iphone-control-viewer".into());
            f.viewers.insert(default_port(), ("default".into(), "default".into()));
            (f, tmp)
        };
        f2.default_restarts_with = Some("1.1.0".into());
        let mut fleet =
            Fleet::begin_with(f2, Path::new("/run/default.sock"), "1.1.0", true, true).await.unwrap().unwrap();
        assert!(fleet.record.default_daemon && fleet.record.default_viewer);
        fleet.package(Some("/opt/homebrew".into())).unwrap();
        // the package installed; the updater killed with the default daemon stopped and still the old program, its
        // viewer stopped
        let Fleet { m, _lock: lock, .. } = fleet;
        drop(lock);
        m.running.borrow_mut().clear();
        recover_with(&m, "1.1.0").await.unwrap();
        let log = m.log.borrow();
        assert!(log.contains(&"restart iphone-control".to_string()), "{log:?}");
        assert!(log.contains(&"start iphone-control-viewer".to_string()), "{log:?}");
        assert!(!tmp.path().join(RECORD).exists());
    }

    /// A Homebrew installation without `brew services` (no default service at all) and nothing on its socket: the
    /// update goes on. Not known whether there is a service, or whether something listens: refused
    #[tokio::test]
    async fn a_default_instance_never_set_up_as_a_service_doesn_t_block_the_update() {
        let silent = |f: &Fake| {
            f.answers.borrow_mut().remove(Path::new("/run/default.sock"));
            f.running.borrow_mut().remove("iphone-control");
        };
        let (mut f, _tmp) = two();
        silent(&f);
        f.default_absent = true;
        let fleet = Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, true).await.unwrap().unwrap();
        assert!(!fleet.record.default_daemon);
        let (mut f, _tmp) = two();
        silent(&f);
        f.default_absent = true;
        f.listening_unknown.insert("/run/default.sock".into());
        assert!(
            Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, true).await.is_err(),
            "a socket not known"
        );
        let (mut f, _tmp) = two();
        silent(&f);
        f.default_lookup_fails = true;
        assert!(
            Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, true).await.is_err(),
            "the manager can't tell"
        );
        // its service stopped, but something on its socket that doesn't answer in time (a daemon started by hand)
        let (mut f, _tmp) = two();
        silent(&f);
        f.listening_unknown.insert("/run/default.sock".into());
        assert!(
            Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", false, true).await.is_err(),
            "stopped, socket not known"
        );
    }

    /// Recorded as running, the default daemon's or viewer's service can't be found when recovering (its definition
    /// gone, the manager not answering): not taken for started again, the record kept
    #[tokio::test]
    async fn a_default_service_gone_when_recovering_keeps_the_record() {
        let record_for = |tmp: &tempfile::TempDir| {
            let mut f = Fake { root: tmp.path().to_owned(), ..Fake::default() };
            f.answers
                .borrow_mut()
                .insert("/run/default.sock".into(), ("1.0.0".into(), "default".into(), "default".into(), None));
            f.running.borrow_mut().insert("iphone-control".into());
            f.running.borrow_mut().insert("iphone-control-viewer".into());
            f.viewers.insert(default_port(), ("default".into(), "default".into()));
            f.default_restarts_with = Some("1.1.0".into());
            f
        };
        for (daemon_gone, viewer_gone) in [(true, false), (false, true)] {
            let tmp = tempfile::tempdir().unwrap();
            let f = record_for(&tmp);
            let mut fleet =
                Fleet::begin_with(f, Path::new("/run/default.sock"), "1.1.0", true, true).await.unwrap().unwrap();
            fleet.package(None).unwrap();
            let Fleet { mut m, _lock: lock, .. } = fleet;
            drop(lock);
            m.running.borrow_mut().clear();
            (m.default_absent, m.viewer_absent) = (daemon_gone, viewer_gone);
            assert!(recover_with(&m, "1.1.0").await.is_err(), "daemon gone {daemon_gone}, viewer gone {viewer_gone}");
            assert!(tmp.path().join(RECORD).exists(), "kept");
            (m.default_absent, m.viewer_absent) = (false, false);
            recover_with(&m, "1.1.0").await.unwrap();
            assert!(!tmp.path().join(RECORD).exists());
        }
    }

    /// A lock or a directory that can't be read isn't taken for none: only one that isn't there is
    #[test]
    fn package_locks_that_can_t_be_read_count_as_held() {
        use std::os::fd::AsRawFd;
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, b"").unwrap();
        // under a file: not a directory, an error other than "not there"
        let under = file.join("x");
        assert_eq!(dir_not_empty(&tmp.path().join("none")), Some(false));
        assert_eq!(dir_not_empty(&under), None);
        assert_eq!(dpkg_locked(&[tmp.path().join("none")], Path::new("/nonexistent")), Some(false), "no dpkg");
        assert_eq!(
            dpkg_locked(std::slice::from_ref(&under), Path::new("/proc/locks")),
            None,
            "a lock file that can't be read"
        );
        assert_eq!(
            dpkg_locked(std::slice::from_ref(&file), &tmp.path().join("no-locks")),
            None,
            "the lock list can't be read"
        );
        let locks = tmp.path().join("locks");
        assert_eq!(brew_locked(&locks), Some(false), "no Homebrew locks");
        assert_eq!(brew_locked(&under), None);
        std::fs::create_dir(&locks).unwrap();
        let held = std::fs::File::create(locks.join("update.lock")).unwrap();
        assert_eq!(brew_locked(&locks), Some(false));
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
        assert_eq!(brew_locked(&locks), Some(true), "brew at work");
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
        // Linux's device numbers: 8:2 is 0x802, 259:3 (an NVMe partition) is 0x10303
        assert_eq!(dev_parts(0x802, 131), (8, 2, 131));
        assert_eq!(dev_parts(0x10303, 5), (259, 3, 5));
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
