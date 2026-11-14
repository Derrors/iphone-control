//! Exercise the shipped CLI and embedded Viewer with no device, daemon or release credentials.
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[tokio::test]
async fn viewer_and_cli_share_the_unbound_report() -> Result<()> {
    if option_env!("ICTL_UPDATE_PUBLISHER").is_some_and(|s| !s.is_empty()) || unsafe { libc::geteuid() } == 0 {
        return Ok(());
    }
    let temp = tempfile::tempdir_in("/tmp")?;
    let dir = temp.path();
    let cli = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_iphone-control"));
        cmd.env_clear()
            .env("HOME", dir)
            .env("PATH", "/usr/bin:/bin")
            .env("ICTL_SOCKET", dir.join("daemon.sock"))
            .env("USBMUXD_SOCKET_ADDRESS", dir.join("absent-usbmuxd.sock"))
            .kill_on_drop(true);
        cmd
    };
    let result = cli().args(["update", "check", "--json"]).output().await?;
    assert!(result.status.success());
    let report: Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(report["availability"], "development");
    assert!(report["publisher"].is_null() && report["daemon_version"].is_null());

    // every HTTP request needs the viewer's login: its token, from `viewer token`
    let shown = cli().args(["viewer", "token"]).output().await?;
    assert!(shown.status.success());
    let shown = String::from_utf8_lossy(&shown.stdout);
    let token = shown
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|w| w.len() == 64)
        .context("a token in `viewer token`")?
        .to_owned();
    let mut viewer = cli().args(["viewer", "--port", "0"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let mut messages = BufReader::new(viewer.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(10), messages.next_line()).await??.context("viewer exited")?;
    let from = line.find("http://127.0.0.1:").context("viewer URL")?;
    let origin = line[from..].split_once("/ ").context("viewer port")?.0;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let unauthenticated = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build()?;
    assert_eq!(unauthenticated.get(format!("{origin}/api/updates")).send().await?.status(), 401);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, format!("Bearer {token}").parse()?);
    let client =
        reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).default_headers(headers).build()?;
    let response = client.get(format!("{origin}/api/updates")).send().await?.error_for_status()?;
    let shown: Value = serde_json::from_slice(&response.bytes().await?)?;
    for (field, value) in report.as_object().unwrap() {
        assert_eq!(&shown[field], value, "{field}");
    }
    let check = |site, body| {
        client
            .post(format!("{origin}/api/updates/check"))
            .header("Origin", site)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
    };
    assert_eq!(check(origin, "{}").await?.status(), 202);
    assert_eq!(check("http://other.invalid", "{}").await?.status(), 403);
    assert_eq!(check(origin, r#"{"publisher":"other/fork"}"#).await?.status(), 400);
    assert!(!dir.join(".cache/iphone-control/updates").exists());
    viewer.kill().await?;
    Ok(())
}

/// `iphone-control status | head -1`: the reader is gone before the command writes. It ends quietly on SIGPIPE
/// like other command-line tools instead of panicking (exit 101 and a panic message on stderr)
#[test]
fn a_closed_stdout_ends_printing_commands_quietly() -> Result<()> {
    use std::os::unix::process::ExitStatusExt;
    let temp = tempfile::tempdir_in("/tmp")?;
    let dir = temp.path();
    for args in [&["mcp-config"][..], &["update", "check"]] {
        // the read end is closed before the command starts, so its first write already fails
        let (reader, writer) = std::io::pipe()?;
        drop(reader);
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_iphone-control"))
            .args(args)
            .env_clear()
            .env("HOME", dir)
            .env("PATH", "/usr/bin:/bin")
            .env("ICTL_SOCKET", dir.join("daemon.sock"))
            .stdout(writer)
            .stderr(Stdio::piped())
            .output()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.signal(), Some(libc::SIGPIPE), "{args:?}: {:?} {stderr}", out.status);
        assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
    }
    Ok(())
}
