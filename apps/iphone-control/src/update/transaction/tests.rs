use super::*;
use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};

// The installed fixture really runs as a separate process, answers Daemon snapshots on a
// private socket and knows its own version. No host service or attached phone is involved.
#[test]
fn fixture_daemon() {
    let Some(socket) = std::env::var_os("ICTL_TEST_TX_SOCKET") else { return };
    let version = std::env::var("ICTL_TEST_TX_VERSION").unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(90));
        std::process::exit(0);
    });
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
            continue;
        }
        if line.trim_end() == "fixture-stop" {
            return;
        }
        let snapshot = serde_json::json!({
            "version": version, "pid":std::process::id(), "state":{"state":"absent"}, "device":null,
            "persisted":{}, "auto_unlock":false, "unlock_disabled":false, "notify_configured":false, "log_tail":[]
        });
        let response = ictl_daemon::protocol::Response::text(snapshot.to_string());
        let _ = writeln!(stream, "{}", serde_json::to_string(&response).unwrap());
    }
}

fn binary(path: &Path, version: &str, reported: &str) {
    let text = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'iphone-control {version}'; exit; fi\nexport ICTL_TEST_TX_VERSION='{reported}'\nexec \"$ICTL_TEST_TX_HARNESS\" --exact update::transaction::tests::fixture_daemon\n"
    );
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn receipt(bin: &Path, version: &str, sha256: String) -> Receipt {
    Receipt {
        schema: 1,
        publisher: "example/repo".into(),
        target: "fixture".into(),
        version: version.into(),
        binary_path: bin.into(),
        service: "fixture".into(),
        pinned: false,
        sha256,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    host: TestService,
    old: Receipt,
    new: Receipt,
    candidate: PathBuf,
}

struct TestService {
    socket: PathBuf,
    binary: PathBuf,
    child: RefCell<Option<Child>>,
    busy: Cell<bool>,
    /// The named instances' daemons don't come up with this version
    members_fail_on: RefCell<Option<String>>,
    /// What the named instances' daemons were asked to do, in order
    members: RefCell<Vec<String>>,
}

impl TestService {
    fn terminate(&self) {
        if let Some(mut child) = self.child.borrow_mut().take() {
            let _ = child.kill();
            child.wait().unwrap();
        } else if let Ok(mut orphan) = UnixStream::connect(&self.socket) {
            // A replacement launched by the killed worker is still running under its supervisor.
            orphan.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            writeln!(orphan, "fixture-stop").unwrap();
            assert_eq!(BufReader::new(orphan).read_line(&mut String::new()).unwrap(), 0);
        }
        let _ = fs::remove_file(&self.socket);
    }
}

impl Drop for TestService {
    fn drop(&mut self) {
        self.terminate();
    }
}

impl Services for TestService {
    fn exclusive(&self) -> Result<File> {
        ensure!(!self.busy.get(), "busy: fixture device control");
        Ok(File::open(&self.binary)?)
    }

    async fn idle(&self, _: &Receipt) -> Result<bool> {
        Ok(self.child.borrow().is_some())
    }
    async fn stop(&self) -> Result<()> {
        self.terminate();
        Ok(())
    }
    async fn start_healthy(&self, version: &str) -> Result<()> {
        let child = spawn_fixture(|| {
            Command::new(&self.binary)
                .arg("daemon")
                .env("ICTL_TEST_TX_HARNESS", std::env::current_exe()?)
                .env("ICTL_TEST_TX_SOCKET", &self.socket)
                .stdout(Stdio::null())
                .spawn()
        })?;
        let pid = child.id();
        self.child.replace(Some(child));
        for _ in 0..2 {
            let snap = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(s) = crate::client::snapshot(&self.socket).await {
                        break s;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await?;
            ensure!(snap.version.as_deref() == Some(version) && snap.pid == Some(pid), "bad fixture health");
        }
        Ok(())
    }
    async fn stop_members(&self) -> Result<()> {
        self.members.borrow_mut().push("stop".into());
        Ok(())
    }
    async fn start_members(&self, version: &str) -> Result<()> {
        self.members.borrow_mut().push(format!("start {version}"));
        ensure!(self.members_fail_on.borrow().as_deref() != Some(version), "a named daemon didn't come up");
        Ok(())
    }
}

/// Starts a fixture binary that was just written. Another test that spawns a process meanwhile can leave a copy of the
/// writing descriptor in its child until that execs, and executing the file then fails with "text file busy"
/// (ETXTBSY): tried again shortly, as `archive.rs` does for the real candidate
fn spawn_fixture(spawn: impl Fn() -> io::Result<Child>) -> io::Result<Child> {
    for _ in 0..50 {
        match spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => std::thread::sleep(Duration::from_millis(20)),
            r => return r,
        }
    }
    spawn()
}

/// [`Store::open`] once the updater lock is free: a test that spawns a process meanwhile briefly holds a copy of a
/// dropped lock's descriptor in the child until it execs, and the lock with it (as in `maintenance::tests`)
fn open_once_free(binary: &Path, receipt: PathBuf) -> Store {
    for _ in 0..100 {
        match Store::open(binary, receipt.clone()) {
            Err(e) if e.downcast_ref::<io::Error>().is_some_and(|e| e.kind() == io::ErrorKind::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            r => return r.unwrap(),
        }
    }
    Store::open(binary, receipt).unwrap()
}

impl Fixture {
    fn new(reported: &str) -> Self {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let base = dir.path().canonicalize().unwrap();
        let bin = base.join("iphone-control");
        let candidate = base.join("patch");
        binary(&bin, "0.2.0", "0.2.0");
        binary(&candidate, "0.2.1", reported);
        let store = Store::open(&bin, base.join("receipt.json")).unwrap();
        let old = receipt(&bin, "0.2.0", digest(&bin).unwrap());
        let new = receipt(&bin, "0.2.1", digest(&candidate).unwrap());
        write_json(&store.receipt, &old).unwrap();
        let host = TestService {
            socket: base.join("daemon.sock"),
            binary: bin,
            child: RefCell::default(),
            busy: Cell::new(false),
            members_fail_on: RefCell::default(),
            members: RefCell::default(),
        };
        Self { _dir: dir, host, store, old, new, candidate }
    }

    async fn apply(&self) -> Result<()> {
        replace(&self.store, &self.host, self.old.clone(), self.new.clone(), &self.candidate, false).await
    }

    fn assert_installed(&self, expected: &Receipt) {
        assert_eq!(read_json::<Receipt>(&self.store.receipt).unwrap().unwrap().sha256, expected.sha256);
        assert_eq!(digest(&self.store.binary).unwrap(), expected.sha256);
        assert!(!self.store.journal().exists());
    }
}

#[tokio::test]
async fn busy_then_replace_while_daemon_is_running() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new("0.2.1");
    let state = f.store.dir.parent().unwrap().join("state.json");
    fs::write(&state, "untouched device state").unwrap();
    f.host.start_healthy(&f.old.version).await.unwrap();
    let pid = f.host.child.borrow().as_ref().unwrap().id();
    f.host.busy.set(true);
    assert!(f.apply().await.is_err());
    f.assert_installed(&f.old);
    assert_eq!(outcome(&f.store.binary).unwrap().unwrap().status, "busy");
    assert_eq!(f.host.child.borrow().as_ref().unwrap().id(), pid);
    f.host.busy.set(false);
    f.apply().await.unwrap();
    f.assert_installed(&f.new);
    assert_eq!(crate::client::snapshot(&f.host.socket).await.unwrap().version.unwrap(), f.new.version);
    assert_eq!(outcome(&f.store.binary).unwrap().unwrap().status, "installed");
    assert_eq!(fs::read_to_string(&state).unwrap(), "untouched device state");
}

#[tokio::test]
async fn unhealthy_daemon_restores_last_verified_executable_and_service() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new("incorrect-version");
    f.host.start_healthy(&f.old.version).await.unwrap();
    assert!(f.apply().await.is_err());
    f.assert_installed(&f.old);
    assert_eq!(crate::client::snapshot(&f.host.socket).await.unwrap().version.unwrap(), f.old.version);
    let result = outcome(&f.store.binary).unwrap().unwrap();
    assert_eq!(result.status, "restored");
    assert_eq!(result.suppressed_version.unwrap(), f.new.version);
}

/// The named instances' daemons are part of the transaction: one that doesn't come up with the new program puts the
/// old one back for every instance, started again on it, and the update fails (docs/dev/multi-instance-plan.md 5.3)
#[tokio::test]
async fn a_named_daemon_that_does_not_come_up_puts_the_old_program_back() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new("0.2.1");
    f.host.start_healthy(&f.old.version).await.unwrap();
    f.host.members_fail_on.replace(Some(f.new.version.clone()));
    assert!(f.apply().await.is_err());
    f.assert_installed(&f.old);
    assert_eq!(crate::client::snapshot(&f.host.socket).await.unwrap().version.unwrap(), f.old.version);
    assert_eq!(
        *f.host.members.borrow(),
        [format!("start {}", f.new.version), "stop".into(), format!("start {}", f.old.version)]
    );
    assert_eq!(outcome(&f.store.binary).unwrap().unwrap().status, "restored");
}

#[tokio::test]
async fn operator_update_preserves_stopped_state() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new("0.2.1");
    f.apply().await.unwrap();
    f.assert_installed(&f.new);
    assert!(f.host.child.borrow().is_none());
}

struct Interrupted {
    stage: String,
    host: TestService,
}
impl Interrupted {
    fn die(&self, phase: &str) {
        if self.stage == phase {
            unsafe {
                libc::kill(libc::getpid(), libc::SIGKILL);
            }
        }
    }
}
impl Services for Interrupted {
    fn exclusive(&self) -> Result<File> {
        Ok(File::open("/dev/null")?)
    }
    async fn idle(&self, _: &Receipt) -> Result<bool> {
        Ok(true)
    }
    async fn stop(&self) -> Result<()> {
        self.die("prepared");
        Ok(())
    }
    async fn start_healthy(&self, version: &str) -> Result<()> {
        self.die("replaced");
        self.host.start_healthy(version).await?;
        self.die("starting");
        Ok(())
    }
}

#[tokio::test]
async fn interrupted_worker() {
    let Some(dir) = std::env::var_os("ICTL_TEST_INTERRUPTED") else { return };
    let dir = PathBuf::from(dir);
    let b = dir.join("iphone-control");
    let c = dir.join("patch");
    let store = open_once_free(&b, dir.join("receipt.json"));
    let old = receipt(&b, "0.2.0", digest(&b).unwrap());
    let new = receipt(&b, "0.2.1", digest(&c).unwrap());
    let host = Interrupted {
        stage: std::env::var("ICTL_TEST_PHASE").unwrap(),
        host: TestService {
            socket: dir.join("daemon.sock"),
            binary: b,
            child: RefCell::default(),
            busy: Cell::new(false),
            members_fail_on: RefCell::default(),
            members: RefCell::default(),
        },
    };
    replace(&store, &host, old, new, &c, false).await.unwrap();
}

#[tokio::test]
async fn recovers_after_process_death_using_only_durable_records() {
    use std::os::unix::process::ExitStatusExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    for phase in ["prepared", "replaced", "starting"] {
        let f = Fixture::new("0.2.1");
        let binary = f.store.binary.clone();
        let receipt_file = f.store.receipt.clone();
        drop(f.store);
        // what the worker printed, shown if it wasn't killed. A file, not a pipe: the fixture daemon it may have
        // started holds the descriptor on, and reading a pipe to its end would wait for that daemon
        let log = binary.parent().unwrap().join("worker.log");
        let out = File::create(&log).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "update::transaction::tests::interrupted_worker", "--nocapture"])
            .env("ICTL_TEST_INTERRUPTED", binary.parent().unwrap())
            .env("ICTL_TEST_PHASE", phase)
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .status()
            .unwrap();
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "{phase}: the worker ended otherwise ({status}):\n{}",
            fs::read_to_string(&log).unwrap_or_default()
        );
        let store = open_once_free(&binary, receipt_file);
        let mut tx: Transaction = read_json(&store.journal()).unwrap().unwrap();
        assert_eq!(
            digest(&store.binary).unwrap(),
            if phase == "prepared" { &f.old.sha256 } else { &f.new.sha256 }.as_str()
        );
        // A kill in the interval after the rename but before its phase checkpoint leaves a
        // prepared journal with the new digest on disk.
        if phase == "replaced" {
            tx.phase = Phase::Prepared;
            write_json(&store.journal(), &tx).unwrap();
        }
        if phase == "starting" {
            assert_eq!(crate::client::snapshot(&f.host.socket).await.unwrap().version.unwrap(), f.new.version);
        }
        resume(&store, &f.host, &tx).await.unwrap();
        assert_eq!(digest(&store.binary).unwrap(), f.old.sha256);
        assert!(!store.journal().exists());
        assert_eq!(crate::client::snapshot(&f.host.socket).await.unwrap().version.unwrap(), f.old.version);
    }
}

#[test]
fn automatic_policy_rejects_minor_releases_replays_and_prereleases() {
    for new in ["0.2.0", "0.1.9", "0.2.1-beta.1", "0.3.0", "1.2.1"] {
        assert!(eligible("0.2.0", new, true).is_err());
    }
    assert!(eligible("0.2.0", "0.2.1", true).is_ok());
    assert!(eligible("0.2.0", "0.3.0", false).is_ok());
}

#[tokio::test]
async fn confirmed_checkpoint_finishes_metadata_and_unknown_disk_is_preserved() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new("0.2.1");
    f.store.link(&f.store.binary, &f.store.dir.join("recovery")).unwrap();
    f.store.link(&f.candidate, &f.store.binary).unwrap();
    let mut tx = Transaction { old: f.old.clone(), new: f.new.clone(), was_running: false, phase: Phase::Confirmed };
    // what 0.2.2 left for `update rollback` goes with the next confirmed installation
    for name in LEGACY_PREVIOUS {
        fs::write(f.store.dir.join(name), "left by 0.2.2").unwrap();
    }
    write_json(&f.store.journal(), &tx).unwrap();
    resume(&f.store, &f.host, &tx).await.unwrap();
    f.assert_installed(&f.new);
    assert!(LEGACY_PREVIOUS.iter().all(|n| !f.store.dir.join(n).exists()));

    tx.phase = Phase::Replaced;
    write_json(&f.store.journal(), &tx).unwrap();
    fs::write(&f.store.binary, "changed outside updater").unwrap();
    assert!(resume(&f.store, &f.host, &tx).await.is_err());
    assert!(f.store.journal().is_file());
    assert_eq!(fs::read_to_string(&f.store.binary).unwrap(), "changed outside updater");
    assert_eq!(outcome(&f.store.binary).unwrap().unwrap().status, "recovery_failed");
    assert_eq!(digest(&f.store.dir.join("recovery")).unwrap(), f.old.sha256);
}

/// Staging failures and what is recorded: a release only not marked unattended is left to `update apply`,
/// distinct from a broken download
#[test]
fn a_release_without_unattended_mark_is_not_a_failure() {
    let e = anyhow::Error::from(archive::NeedsOperator("0.2.2".into()));
    assert_eq!(failure_status(&e), "needs_operator");
    assert!(e.to_string().ends_with("iphone-control update"), "{e}");
    assert_eq!(failure_status(&anyhow::anyhow!("archive digest mismatch")), "failed");
}

/// Idle MCP servers of this version don't keep an update waiting; anything else named iphone-control does
#[test]
fn only_mcp_servers_idle_without_the_lease() {
    for args in [
        "iphone-control mcp",
        "/home/u/.local/bin/iphone-control mcp",
        "iphone-control --socket /run/x.sock mcp",
        "iphone-control --data-dir=/d --socket=/s mcp --url https://h:8200",
    ] {
        assert!(idles_without_lease(args), "{args}");
    }
    for args in [
        "iphone-control",
        "iphone-control viewer",
        "iphone-control mcp-config",
        "iphone-control setup",
        "iphone-control --socket mcp viewer",
        "iphone-control task run claude mcp",
    ] {
        assert!(!idles_without_lease(args), "{args}");
    }
}
