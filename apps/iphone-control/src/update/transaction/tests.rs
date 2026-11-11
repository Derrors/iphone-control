use super::*;
use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader};
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
        let child = Command::new(&self.binary)
            .arg("daemon")
            .env("ICTL_TEST_TX_HARNESS", std::env::current_exe()?)
            .env("ICTL_TEST_TX_SOCKET", &self.socket)
            .stdout(Stdio::null())
            .spawn()?;
        let pid = child.id();
        self.child.replace(Some(child));
        for _ in 0..2 {
            let snap = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(s) = crate::snapshot(&self.socket).await {
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
        };
        Self { _dir: dir, host, store, old, new, candidate }
    }

    async fn apply(&self) -> Result<()> {
        replace(&self.store, &self.host, self.old.clone(), self.new.clone(), &self.candidate, false, false).await
    }

    fn assert_installed(&self, expected: &Receipt) {
        assert_eq!(read_json::<Receipt>(&self.store.receipt).unwrap().unwrap().sha256, expected.sha256);
        assert_eq!(digest(&self.store.binary).unwrap(), expected.sha256);
        assert!(!self.store.journal().exists());
    }
}

#[tokio::test]
async fn busy_then_replace_and_explicit_rollback_while_daemon_is_running() {
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
    assert_eq!(crate::snapshot(&f.host.socket).await.unwrap().version.unwrap(), f.new.version);
    let p: Previous = read_json(&f.store.dir.join("previous.json")).unwrap().unwrap();
    let previous = f.store.dir.join("previous");
    assert_eq!(p.before_digest, f.new.sha256);
    assert_eq!(digest(&previous).unwrap(), p.receipt.sha256);
    replace(&f.store, &f.host, f.new.clone(), p.receipt, &previous, true, false).await.unwrap();
    f.assert_installed(&f.old);
    assert_eq!(crate::snapshot(&f.host.socket).await.unwrap().version.unwrap(), f.old.version);
    assert_eq!(outcome(&f.store.binary).unwrap().unwrap().suppressed_version.unwrap(), f.new.version);
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
    assert_eq!(crate::snapshot(&f.host.socket).await.unwrap().version.unwrap(), f.old.version);
    let result = outcome(&f.store.binary).unwrap().unwrap();
    assert_eq!(result.status, "restored");
    assert_eq!(result.suppressed_version.unwrap(), f.new.version);
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
    let store = Store::open(&b, dir.join("receipt.json")).unwrap();
    let old = receipt(&b, "0.2.0", digest(&b).unwrap());
    let new = receipt(&b, "0.2.1", digest(&c).unwrap());
    let host = Interrupted {
        stage: std::env::var("ICTL_TEST_PHASE").unwrap(),
        host: TestService {
            socket: dir.join("daemon.sock"),
            binary: b,
            child: RefCell::default(),
            busy: Cell::new(false),
        },
    };
    replace(&store, &host, old, new, &c, false, false).await.unwrap();
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
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "update::transaction::tests::interrupted_worker"])
            .env("ICTL_TEST_INTERRUPTED", binary.parent().unwrap())
            .env("ICTL_TEST_PHASE", phase)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        let store = Store::open(&binary, receipt_file).unwrap();
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
            assert_eq!(crate::snapshot(&f.host.socket).await.unwrap().version.unwrap(), f.new.version);
        }
        resume(&store, &f.host, &tx).await.unwrap();
        assert_eq!(digest(&store.binary).unwrap(), f.old.sha256);
        assert!(!store.journal().exists());
        assert_eq!(crate::snapshot(&f.host.socket).await.unwrap().version.unwrap(), f.old.version);
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
    let mut tx = Transaction {
        old: f.old.clone(),
        new: f.new.clone(),
        was_running: false,
        rollback: false,
        phase: Phase::Confirmed,
    };
    write_json(&f.store.journal(), &tx).unwrap();
    resume(&f.store, &f.host, &tx).await.unwrap();
    f.assert_installed(&f.new);
    assert_eq!(digest(&f.store.dir.join("previous")).unwrap(), f.old.sha256);

    tx.phase = Phase::Replaced;
    write_json(&f.store.journal(), &tx).unwrap();
    fs::write(&f.store.binary, "changed outside updater").unwrap();
    assert!(resume(&f.store, &f.host, &tx).await.is_err());
    assert!(f.store.journal().is_file());
    assert_eq!(fs::read_to_string(&f.store.binary).unwrap(), "changed outside updater");
    assert_eq!(outcome(&f.store.binary).unwrap().unwrap().status, "recovery_failed");
    assert_eq!(digest(&f.store.dir.join("recovery")).unwrap(), f.old.sha256);
}
