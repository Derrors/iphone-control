//! `task run --url`: a task on another machine's phone, through its viewer (docs/task-control.md 7.6).
//!
//! The viewer's `POST /api/agent/tasks/open` opens the task and keeps answering with its events (server-sent
//! events); that response is the lease, as the socket's connection is for `task run` on the same machine. This side
//! logs in with `IPHONE_CONTROL_VIEWER_TOKEN` (a token that may operate) and keeps it: the command only gets the
//! task's token, with the viewer's address, so the agent can use `/mcp` for its task but can't control it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use ictl_daemon::protocol::{TaskEvent, TaskOpened};
use serde_json::{Value, json};

/// The viewer's login token for `task run --url`
pub const LOGIN_ENV: &str = "IPHONE_CONTROL_VIEWER_TOKEN";
/// Given to the command: the viewer it works through
pub const URL_ENV: &str = "IPHONE_CONTROL_URL";
/// Given to the command: the viewer's MCP endpoint, for an agent that connects by URL
pub const MCP_URL_ENV: &str = "IPHONE_CONTROL_MCP_URL";
/// Given to the command when `--ca` was: the certificate to trust
pub const CA_ENV: &str = "IPHONE_CONTROL_CA";

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
        if !base.starts_with("http://") && !base.starts_with("https://") {
            bail!(ictl_l10n::trf!(
                "--url 要是控制台的地址，比如 https://192.168.1.5:8200：{url}",
                "--url takes the viewer's address, such as https://192.168.1.5:8200: {url}"
            ));
        }
        // reqwest is built without a crypto provider of its own (main installs ring; so does this, for tests)
        let _ = rustls::crypto::ring::default_provider().install_default();
        // the viewer is on this network: never through a proxy from the environment
        let mut b = reqwest::Client::builder().no_proxy().connect_timeout(Duration::from_secs(10));
        if let Some(ca) = ca {
            let pem = std::fs::read(ca).with_context(|| ca.display().to_string())?;
            let cert = reqwest::Certificate::from_pem(&pem).with_context(|| ca.display().to_string())?;
            b = b.tls_certs_merge([cert]);
        }
        Ok(Self { client: b.build()?, base, ca: ca.map(Path::to_owned), login })
    }

    /// From inside a command `task run --url` started (its environment), with the task's token only
    pub fn from_env() -> Option<anyhow::Result<Self>> {
        let url = std::env::var(URL_ENV).ok().filter(|u| !u.is_empty())?;
        let ca = std::env::var_os(CA_ENV).filter(|c| !c.is_empty()).map(PathBuf::from);
        Some(Self::new(&url, ca.as_deref(), None))
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

    fn post(&self, path: &str, body: &Value) -> reqwest::RequestBuilder {
        let r = self.client.post(format!("{}{path}", self.base)).json(body);
        match &self.login {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    /// Opens a task: its id and token, and its events, whose response is the lease
    pub async fn open(&self, title: &str, owner: &str, apps: &[String]) -> anyhow::Result<(TaskOpened, Events)> {
        let body = json!({"title": title, "owner": owner, "apps": apps});
        let resp = self.post("/api/agent/tasks/open", &body).send().await.with_context(|| self.base.clone())?;
        if !resp.status().is_success() {
            bail!("{}", failure(resp).await);
        }
        let mut events = Events { resp, buf: Vec::new() };
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

/// What the viewer said when it refused
async fn failure(r: reqwest::Response) -> String {
    let status = r.status();
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
    /// The next event; `None` when the response ended (the task ended, or the viewer went away)
    pub async fn next(&mut self) -> Option<TaskEvent> {
        loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\n\n") {
                let block: Vec<u8> = self.buf.drain(..i + 2).collect();
                if let Some(e) = parse_event(&String::from_utf8_lossy(&block)) {
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

/// One server-sent event (`event:` and `data:` lines); comments (`: ping`) and malformed ones are `None`
fn parse_event(block: &str) -> Option<TaskEvent> {
    let (mut event, mut data) = (None, Vec::new());
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    Some(TaskEvent { event: event?, data: serde_json::from_str(&data.join("\n")).ok()? })
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
    }

    #[test]
    fn the_address_is_the_viewer_s() {
        let r = Remote::new("https://nas.local:8202/mcp/", None, None).unwrap();
        assert_eq!(r.base, "https://nas.local:8202");
        let env = r.env();
        assert!(env.contains(&(MCP_URL_ENV, "https://nas.local:8202/mcp".into())));
        assert!(Remote::new("nas.local:8202", None, None).is_err());
    }
}
