//! Another machine's viewer, for the phone connected there (docs/task-control.md 7.6, 7.7): `task … --url` and
//! `mcp --url`.
//!
//! `task run --url`: the viewer's `POST /api/agent/tasks/open` opens the task and keeps answering with its events
//! (server-sent events); that response is the lease, as the socket's connection is for `task run` on the same
//! machine. This side logs in with `IPHONE_CONTROL_VIEWER_TOKEN` (a token that may operate) and keeps it: the command
//! only gets the task's token, with the viewer's address, so the agent can use `/mcp` for its task but can't control
//! it. The other `task` subcommands list, follow and steer tasks through the same viewer with that login.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use ictl_daemon::protocol::{TaskEvent, TaskInfo, TaskOpened};
use serde_json::{Value, json};

/// The viewer's login token for `task run --url`
pub const LOGIN_ENV: &str = ictl_daemon::protocol::env::VIEWER_TOKEN;
/// Given to the command: the viewer it works through
pub const URL_ENV: &str = ictl_daemon::protocol::env::URL;
/// Given to the command: the viewer's MCP endpoint, for an agent that connects by URL
pub const MCP_URL_ENV: &str = ictl_daemon::protocol::env::MCP_URL;
/// Given to the command when `--ca` was: the certificate to trust
pub const CA_ENV: &str = ictl_daemon::protocol::env::CA;

/// A viewer on another machine
pub struct Remote {
    client: reqwest::Client,
    /// `https://host:port`, no trailing slash
    base: String,
    ca: Option<PathBuf>,
    /// `None`: only the task's token (`task finish` from inside the command)
    login: Option<String>,
}

impl Remote {
    /// `url`: the viewer's address; `ca`: a certificate to trust besides the system's (the viewer's self-signed one)
    pub fn new(url: &str, ca: Option<&Path>, login: Option<String>) -> anyhow::Result<Self> {
        let base = url.trim().trim_end_matches('/').trim_end_matches("/mcp").to_owned();
        let parsed = reqwest::Url::parse(&base).ok().filter(|u| matches!(u.scheme(), "http" | "https") && u.has_host());
        let Some(parsed) = parsed else {
            bail!(ictl_l10n::trf!(
                "--url 要是控制台的地址，比如 https://192.168.1.5:8200：{url}",
                "--url takes the viewer's address, such as https://192.168.1.5:8200: {url}"
            ));
        };
        // the login token and the task's token go with every request: in the clear only to this machine
        if parsed.scheme() == "http" && !loopback(&parsed) {
            bail!(ictl_l10n::trf!(
                "另一台机器的控制台要用 https://（令牌不能明文发出去）：{url}",
                "Another machine's viewer takes https:// (tokens aren't sent in the clear): {url}"
            ));
        }
        // reqwest is built without a crypto provider of its own (main installs ring; so does this, for tests)
        let _ = rustls::crypto::ring::default_provider().install_default();
        // the viewer is on this network: never through a proxy from the environment. Redirects aren't followed: the
        // tokens would go along to wherever it points
        let mut b = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10));
        if let Some(ca) = ca {
            let pem = std::fs::read(ca).with_context(|| ca.display().to_string())?;
            let cert = reqwest::Certificate::from_pem(&pem).with_context(|| ca.display().to_string())?;
            b = b.tls_certs_merge([cert]);
        }
        Ok(Self { client: b.build()?, base, ca: ca.map(Path::to_owned), login })
    }

    /// The login token for opening and controlling tasks
    pub fn login_from_env() -> anyhow::Result<String> {
        std::env::var(LOGIN_ENV).ok().filter(|t| !t.trim().is_empty()).map(|t| t.trim().to_owned()).with_context(|| {
            ictl_l10n::trf!(
                "--url 要用控制台的登录令牌：设 {LOGIN_ENV}（在接着手机的机器上运行 iphone-control viewer token 查看）",
                "--url needs the viewer's login token: set {LOGIN_ENV} (iphone-control viewer token shows it, on the machine with the phone)"
            )
        })
    }

    /// The variables the command gets, besides the task's token and id
    pub fn env(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![(URL_ENV, self.base.clone()), (MCP_URL_ENV, format!("{}/mcp", self.base))];
        if let Some(ca) = &self.ca {
            v.push((CA_ENV, ca.display().to_string()));
        }
        v
    }

    /// A request to `path` on the viewer, with the login if there is one
    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.client.request(method, format!("{}{path}", self.base));
        match &self.login {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    fn post(&self, path: &str, body: &Value) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, path).json(body)
    }

    /// The viewer's address, for messages
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Recent tasks, as `task_list` on the socket
    pub async fn list(&self) -> anyhow::Result<Vec<TaskInfo>> {
        let r = self.request(reqwest::Method::GET, "/api/agent/tasks").timeout(Duration::from_secs(15));
        let r = r.send().await.with_context(|| self.base.clone())?;
        if !r.status().is_success() {
            bail!("{}", failure(r).await);
        }
        let v: Value = r.json().await?;
        Ok(serde_json::from_value(v["tasks"].clone())?)
    }

    /// A task's events so far, then new ones until it ends
    pub async fn events(&self, id: &str) -> anyhow::Result<Events> {
        let r = self.request(reqwest::Method::GET, &format!("/api/agent/tasks/{id}/events"));
        let resp = r.send().await.with_context(|| self.base.clone())?;
        if !resp.status().is_success() {
            bail!("{}", failure(resp).await);
        }
        Ok(Events::new(resp))
    }

    /// `pause`, `resume`, `takeover`, `handback`, `stop`, `confirm` (`body`: `{step, approve}`)
    pub async fn control(&self, id: &str, action: &str, body: &Value) -> anyhow::Result<()> {
        let r = self.post(&format!("/api/agent/tasks/{id}/{action}"), body).timeout(Duration::from_secs(15));
        let r = r.send().await.with_context(|| self.base.clone())?;
        if !r.status().is_success() {
            bail!("{}", failure(r).await);
        }
        Ok(())
    }

    /// Opens a task: its id and token, and its events, whose response is the lease
    pub async fn open(&self, title: &str, owner: &str, apps: &[String]) -> anyhow::Result<(TaskOpened, Events)> {
        let body = json!({"title": title, "owner": owner, "apps": apps});
        let resp = self.post("/api/agent/tasks/open", &body).send().await.with_context(|| self.base.clone())?;
        if !resp.status().is_success() {
            bail!("{}", failure(resp).await);
        }
        let mut events = Events::new(resp);
        match events.next().await {
            Some(e) if e.event == "opened" => Ok((serde_json::from_value(e.data)?, events)),
            _ => bail!(ictl_l10n::tr!("控制台没有开出任务", "The viewer didn't open the task")),
        }
    }

    /// Stops the task (the platform's Ctrl-C); whether the viewer took it
    pub async fn stop(&self, id: &str) -> bool {
        let r = self.post(&format!("/api/agent/tasks/{id}/stop"), &json!({})).send().await;
        r.is_ok_and(|r| r.status().is_success())
    }

    /// Ends the task with the command's result, as the task (its token)
    pub async fn finish(&self, token: &str, ok: bool, summary: &str) -> anyhow::Result<()> {
        let r = self
            .post("/api/agent/tasks/finish", &json!({"ok": ok, "summary": summary}))
            .header(ictl_daemon::protocol::TASK_TOKEN_HEADER, token)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .with_context(|| self.base.clone())?;
        if !r.status().is_success() {
            bail!("{}", failure(r).await);
        }
        Ok(())
    }

    /// The task's state as the viewer lists it
    pub async fn state(&self, id: &str) -> Option<String> {
        let mut r = self.client.get(format!("{}/api/agent/tasks", self.base)).timeout(Duration::from_secs(15));
        if let Some(t) = &self.login {
            r = r.bearer_auth(t);
        }
        let v: Value = r.send().await.ok()?.json().await.ok()?;
        v["tasks"].as_array()?.iter().find(|t| t["id"] == id)?["state"].as_str().map(str::to_owned)
    }
}

/// `localhost`, `127.0.0.0/8` or `[::1]`
fn loopback(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback())
}

/// What the viewer said when it refused
pub async fn failure(r: reqwest::Response) -> String {
    let status = r.status();
    if status.is_redirection() {
        let to = r.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
        return ictl_l10n::trf!(
            "控制台要求换个地址（HTTP {status} → {to}）：把 --url 改成这个地址再试",
            "The viewer points elsewhere (HTTP {status} → {to}): use that address as --url and try again"
        );
    }
    let text = r.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["message"].as_str().map(str::to_owned));
    let why = message.unwrap_or_else(|| text.trim().chars().take(200).collect());
    match status.as_u16() {
        401 => ictl_l10n::trf!("控制台没认这个登录令牌（{why}）", "The viewer didn't take the login token ({why})"),
        403 => ictl_l10n::trf!("控制台拒绝了（{why}）", "The viewer refused ({why})"),
        _ => format!("HTTP {status}: {why}"),
    }
}

/// A task's events as server-sent events
pub struct Events {
    resp: reqwest::Response,
    buf: Vec<u8>,
}

impl Events {
    pub fn new(resp: reqwest::Response) -> Self {
        Self { resp, buf: Vec::new() }
    }

    /// The next event; `None` when the response ended (the task ended, or the viewer went away)
    pub async fn next(&mut self) -> Option<TaskEvent> {
        loop {
            let (event, data) = self.next_raw().await?;
            if let Some(e) = event.and_then(|event| Some(TaskEvent { event, data: serde_json::from_str(&data).ok()? }))
            {
                return Some(e);
            }
        }
    }

    /// The next event with data, as it came: its name if it had one, and its data
    pub async fn next_raw(&mut self) -> Option<(Option<String>, String)> {
        loop {
            // events end with a blank line (CRLF too). Found in the bytes: a chunk may end inside a character
            if let Some((end, next)) = event_end(&self.buf) {
                let block = String::from_utf8_lossy(&self.buf[..end]).replace("\r\n", "\n");
                self.buf.drain(..next);
                if let Some(e) = parse_block(&block) {
                    return Some(e);
                }
                continue;
            }
            match self.resp.chunk().await {
                Ok(Some(c)) => self.buf.extend_from_slice(&c),
                _ => return None,
            }
        }
    }
}

/// Where the first event in `buf` ends, and where the next one starts
fn event_end(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2));
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, i + 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// One server-sent event: its `event:` name and `data:` lines; `None` for one without data (comments such as
/// `: ping`, a `retry:` alone)
fn parse_block(block: &str) -> Option<(Option<String>, String)> {
    let (mut event, mut data) = (None, Vec::new());
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    let data = data.join("\n");
    (!data.trim().is_empty()).then_some((event, data))
}

/// One task event (named, with JSON data); `None` otherwise
#[cfg(test)]
fn parse_event(block: &str) -> Option<TaskEvent> {
    let (event, data) = parse_block(block)?;
    Some(TaskEvent { event: event?, data: serde_json::from_str(&data).ok()? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_server_sent_events() {
        assert!(parse_event(": ping\n\n").is_none());
        let e = parse_event("event: opened\ndata: {\"id\":\"a\",\"token\":\"b\"}\n\n").unwrap();
        assert_eq!((e.event.as_str(), e.data["token"].as_str()), ("opened", Some("b")));
        let e = parse_event("event: step\ndata:{\"id\":1}\n").unwrap();
        assert_eq!(e.data["id"], 1);
        assert!(parse_event("data: {}\n\n").is_none(), "no event name");
        // MCP over HTTP: events without a name, priming ones without data
        assert_eq!(parse_block("id: 1\nretry: 3000\ndata:\n"), None);
        assert_eq!(parse_block("data: {\"id\":1}"), Some((None, "{\"id\":1}".into())));
        assert_eq!(event_end(b"data: 1\r\n\r\ndata: 2\n\n"), Some((7, 11)));
        assert_eq!(event_end(b"data: 1\n\ndata: 2\r\n\r\n"), Some((7, 9)));
        assert_eq!(event_end("data: \"设".as_bytes()), None, "not yet");
    }

    #[test]
    fn the_address_is_the_viewer_s() {
        let r = Remote::new("https://nas.local:8202/mcp/", None, None).unwrap();
        assert_eq!(r.base, "https://nas.local:8202");
        let env = r.env();
        assert!(env.contains(&(MCP_URL_ENV, "https://nas.local:8202/mcp".into())));
        assert!(Remote::new("nas.local:8202", None, None).is_err());
        // tokens in the clear only to this machine
        assert!(Remote::new("http://192.168.1.5:8202", None, None).is_err());
        assert!(Remote::new("http://nas.local:8202", None, None).is_err());
        for local in ["http://127.0.0.1:8202", "http://localhost:8202", "http://[::1]:8202"] {
            assert!(Remote::new(local, None, None).is_ok(), "{local}");
        }
    }
}
