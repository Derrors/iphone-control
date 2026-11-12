//! Exercise the real executable before its config/runtime/maintenance initialization.

use std::ffi::CString;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn run(root: &Path, args: &[&str], language: &str) -> Output {
    let stdout = root.join("stdout");
    let stderr = root.join("stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_iphone-control"))
        .args(args)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("ICTL_DATA_DIR", root.join("data"))
        .env("ICTL_SOCKET", root.join("ctl.sock"))
        .env("IPHONE_CONTROL_LANG", language)
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("completion command blocked (possibly reading the settings FIFO): {args:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Output { status, stdout: fs::read(stdout).unwrap(), stderr: fs::read(stderr).unwrap() }
}

fn check_fish_candidates(script: &Path) {
    for (line, expected, present) in [
        ("iphone-control completions ", "bash", true),
        ("iphone-control --data-dir policy completions ", "zsh", true),
        ("iphone-control --socket=policy update policy ", "manual", true),
        ("iphone-control update policy ", "auto-patch", true),
        ("iphone-control update policy manual ", "auto-patch", false),
        ("iphone-control --data-dir policy update check ", "manual", false),
        ("iphone-control --data-dir completions status ", "bash", false),
        ("iphone-control select policy ", "auto-patch", false),
    ] {
        let output = Command::new("fish")
            .args(["--no-config", "-c", "source $argv[1]; complete -C $argv[2]", "--"])
            .arg(script)
            .arg(line)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let candidates = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            candidates.lines().any(|value| value.split('\t').next() == Some(expected)),
            present,
            "{line:?}: {candidates}"
        );
    }
}

#[test]
fn scripts_help_and_errors_do_not_read_settings_create_state_or_contact_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("config/iphone-control")).unwrap();
    // With no writer, reading this FIFO hangs. The bounded subprocess check catches config
    // access reliably, even when tests run as root and mode 000 would still be readable.
    let fifo = CString::new(root.join("config/iphone-control/env").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let listener = UnixListener::bind(root.join("ctl.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    for shell in ["bash", "zsh", "fish"] {
        let output = run(root, &["completions", shell], "en");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stderr.is_empty());
        let script = String::from_utf8(output.stdout).unwrap();
        assert!(script.contains("iphone-control"));
        assert!(script.contains("auto-patch"));
        let path = root.join(format!("completion.{shell}"));
        fs::write(&path, script).unwrap();
        // Syntax-check each available shell; CI need not install Fish just to generate it.
        match Command::new(shell).arg("--version").output() {
            Ok(_) => {
                let check = Command::new(shell).arg("-n").arg(&path).output().unwrap();
                assert!(check.status.success(), "{shell}: {}", String::from_utf8_lossy(&check.stderr));
                if shell == "fish" {
                    check_fish_candidates(&path);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot run {shell}: {error}"),
        }
    }
    let output = run(root, &["--data-dir", "completions", "--socket=completions", "completions", "fish"], "en");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    for (language, description) in [("en", "Generate Bash"), ("zh", "生成 Bash")] {
        for args in [["completions", "--help"], ["help", "completions"]] {
            let output = run(root, &args, language);
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            assert!(String::from_utf8(output.stdout).unwrap().contains(description));
        }
    }
    for args in [vec!["completions"], vec!["completions", "powershell"], vec!["completions", "bash", "extra"]] {
        let output = run(root, &args, "en");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    for name in ["home", "data", "runtime"] {
        assert!(!root.join(name).exists(), "completion created {name}");
    }
}
