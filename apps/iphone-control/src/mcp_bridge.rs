//! `iphone-control mcp --url`: the MCP server of another machine's viewer, for clients that only start a command
//! (docs/task-control.md 7.7).
//!
//! Stdio on this side, the viewer's `/mcp` (Streamable HTTP) on the other: each JSON-RPC message read from stdin is
//! posted there, and what comes back (a JSON body, or server-sent events) is written to stdout, one message a line.
//! Requests go out at once and side by side (a tool call may wait for the user a long time); notifications and the
//! answers to the server's own requests go in order, so `notifications/initialized` reaches the server before the
//! next request. The session id and protocol version from `initialize` go along with every message after it, and
//! the session is ended when stdin closes.
//!
//! The address comes from `--url` or `IPHONE_CONTROL_URL`, which `task run --url` gives its command: an agent set
//! up with plain `iphone-control mcp` then works for its task on the other machine without changing anything. With
//! a task's token (`IPHONE_CONTROL_TASK_TOKEN`) only that goes along, so the agent can't control its task;
//! otherwise the login from `IPHONE_CONTROL_VIEWER_TOKEN`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use ictl_daemon::protocol;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;

use crate::remote::{self, Remote};

/// `iphone-control mcp --url <viewer>` on stdin and stdout
pub async fn run(url: &str, ca: Option<&Path>) -> anyhow::Result<()> {
    let task = std::env::var(protocol::TASK_TOKEN_ENV).ok().filter(|t| !t.trim().is_empty());
    // working for a task: only its token, never the login (which could control the task)
    let login = match task {
        Some(_) => None,
        None => Some(Remote::login_from_env()?),
    };
    let remote = Remote::new(url, ca, login)?;
    bridge(remote, task, tokio::io::BufReader::new(tokio::io::stdin()), tokio::io::stdout()).await
}

/// What `initialize` set up, for the messages after it
#[derive(Default)]
struct Session {
    id: Option<String>,
    version: Option<String>,
}

/// One MCP client's messages from `input` to the viewer, and its answers to `output`, until `input` ends
pub async fn bridge<R, W>(remote: Remote, task: Option<String>, input: R, output: W) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // one writer: lines from requests answered side by side never mix
    let writer = tokio::spawn(async move {
        let mut out = output;
        while let Some(line) = rx.recv().await {
            out.write_all(line.as_bytes()).await?;
            out.write_all(b"\n").await?;
            out.flush().await?;
        }
        anyhow::Ok(())
    });
    let link = Arc::new(Link { remote, task, session: Mutex::default(), tx });
    let mut requests = tokio::task::JoinSet::new();
    let mut lines = input.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = link.tx.send(rpc_error(Value::Null, -32700, &format!("parse error: {e}")));
                continue;
            }
        };
        let request = msg.get("method").is_some() && msg.get("id").is_some_and(|i| !i.is_null());
        // `initialize` first and alone: what follows needs its session
        if request && msg["method"] != "initialize" {
            let link = link.clone();
            requests.spawn(async move { link.forward(msg).await });
        } else {
            link.forward(msg).await;
        }
        while requests.try_join_next().is_some() {}
    }
    // the client is done: what it still waited for goes with it
    requests.abort_all();
    link.end().await;
    drop(link);
    writer.await??;
    Ok(())
}

struct Link {
    remote: Remote,
    task: Option<String>,
    session: Mutex<Session>,
    tx: UnboundedSender<String>,
}

impl Link {
    fn session(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn with_headers(&self, mut r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(t) = &self.task {
            r = r.header(protocol::TASK_TOKEN_HEADER, t);
        }
        let s = self.session();
        if let Some(id) = &s.id {
            r = r.header("mcp-session-id", id);
        }
        if let Some(v) = &s.version {
            r = r.header("mcp-protocol-version", v);
        }
        r
    }

    /// Sends one message; a request that fails gets a JSON-RPC error, so the client doesn't wait for ever
    async fn forward(&self, msg: Value) {
        let id = msg.get("method").and(msg.get("id")).cloned().filter(|i| !i.is_null());
        if let Err(e) = self.send(&msg, id.as_ref()).await {
            match id {
                Some(id) => {
                    let reply = if msg["method"] == "tools/call"
                        && msg["params"]["name"].as_str().is_some_and(ictl_mcp::reports_input)
                    {
                        // The remote daemon may have acted before the tunnel lost the reply.
                        json!({"jsonrpc": "2.0", "id": id, "error": {
                            "code": -32603,
                            "message": format!("{e}. {}", ictl_l10n::tr!(
                                "未收到远端工具结果；先读屏确认，再决定是否重试",
                                "No remote tool result; inspect the screen before retrying"
                            )),
                            "data": {"delivery": "unknown", "stage": "transport"}
                        }})
                        .to_string()
                    } else {
                        rpc_error(id, -32603, &e)
                    };
                    let _ = self.tx.send(reply);
                }
                // a notification has nobody to tell but the log (stdout is the protocol)
                None => eprintln!("iphone-control mcp --url: {e}"),
            }
        }
    }

    async fn send(&self, msg: &Value, id: Option<&Value>) -> Result<(), String> {
        let r = self.remote.request(reqwest::Method::POST, "/mcp");
        let r = self.with_headers(r).header(reqwest::header::ACCEPT, "application/json, text/event-stream").json(msg);
        let resp = r.send().await.map_err(|e| format!("{}: {e}", self.remote.base()))?;
        if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            self.session().id = Some(sid.to_owned());
        }
        if resp.status() == reqwest::StatusCode::ACCEPTED {
            return missing_reply(id);
        }
        if !resp.status().is_success() {
            return Err(remote::failure(resp).await);
        }
        let initialize = msg["method"] == "initialize";
        let pass = |v: Value| {
            if initialize && let Some(version) = v["result"]["protocolVersion"].as_str() {
                self.session().version = Some(version.to_owned());
            }
            let answered = id.is_some_and(|id| v.get("id") == Some(id) && v.get("method").is_none());
            let _ = self.tx.send(v.to_string());
            answered
        };
        let sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.starts_with("text/event-stream"));
        if sse {
            let mut events = remote::Events::new(resp);
            while let Some((_, data)) = events.next_raw().await {
                // the answer to this request ends it; what came before (progress, the server's requests) went too
                if let Ok(v) = serde_json::from_str::<Value>(&data)
                    && pass(v)
                {
                    return Ok(());
                }
            }
            return missing_reply(id);
        }
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if text.trim().is_empty() {
            return missing_reply(id);
        }
        let reply = serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())?;
        let mut answered = false;
        match reply {
            Value::Array(messages) => {
                for message in messages {
                    answered |= pass(message);
                }
            }
            message => answered = pass(message),
        }
        if answered { Ok(()) } else { missing_reply(id) }
    }

    /// Ends the session on the viewer (best effort)
    async fn end(&self) {
        if self.session().id.is_none() {
            return;
        }
        let r = self.with_headers(self.remote.request(reqwest::Method::DELETE, "/mcp"));
        let _ = r.timeout(std::time::Duration::from_secs(3)).send().await;
    }
}

/// Notifications and client responses have no corresponding JSON-RPC response.
fn missing_reply(id: Option<&Value>) -> Result<(), String> {
    if id.is_some() { Err("viewer closed the response without a matching JSON-RPC reply".into()) } else { Ok(()) }
}

fn rpc_error(id: Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn interrupted_tool_reply_reports_unknown_instead_of_hanging_or_retrying() {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (mut socket, _) = server.accept().await.unwrap();
            let mut buf = [0; 4096];
            assert!(socket.read(&mut buf).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let link = Link { remote: Remote::new(&url, None, None).unwrap(), task: None, session: Mutex::default(), tx };
        link.forward(json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"iphone_tap"}})).await;
        let reply: Value = serde_json::from_str(&rx.try_recv().expect("client must receive a terminal reply")).unwrap();
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["error"]["data"]["delivery"], "unknown");
        peer.await.unwrap();
    }
}
