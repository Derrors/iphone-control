//! The task gate: how a task started from the web viewer (`iphone-control viewer`) follows and steers the agent.
//!
//! The viewer runs an agent (Claude Code or Codex) with this MCP server and two environment variables:
//! [`GATE_ENV`], the path of a Unix socket the viewer listens on, and [`TOKEN_ENV`], the task's secret. With them
//! set, every tool call asks the viewer first (`before`): the viewer records it as a step of the task, holds it while
//! the task is paused or the user has taken over, and can refuse it. After the call the result goes back (`after`)
//! so the page shows it, screenshot included. [`CONFIRM_TOOL`] is listed too: the agent calls it before payments,
//! deletions, sending messages and the like, and it waits until the user approves or rejects on the page.
//!
//! The steps come from here rather than from the agent's own output, so they look the same whichever agent runs.
//! Without the variables nothing changes: the tool list and every call are as before.
//!
//! One JSON line each way per connection. Calls can wait as long as the user takes (a pause, a confirmation), so
//! there is no timeout; when the viewer is gone the call is refused instead of acting unsupervised.

use std::path::PathBuf;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const GATE_ENV: &str = "ICTL_AGENT_GATE";
pub const TOKEN_ENV: &str = "ICTL_AGENT_TOKEN";
/// Listed only under a gate: asks the user on the page before a sensitive action
pub const CONFIRM_TOOL: &str = "iphone_confirm";
/// Text results longer than this are cut before they go to the page (the agent still gets all of it)
const MAX_TEXT: usize = 4000;

#[derive(Debug, Clone)]
pub struct Gate {
    path: PathBuf,
    token: String,
}

/// What the viewer says before a tool call
#[derive(Debug, PartialEq)]
pub enum Before {
    /// Go ahead; the call is step `step` of the task
    Go(u64),
    /// Don't run it; tell the agent why
    Refuse(String),
}

impl Gate {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os(GATE_ENV).filter(|p| !p.is_empty())?;
        let token = std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty())?;
        Some(Self { path: path.into(), token })
    }

    async fn ask(&self, mut msg: Value) -> std::io::Result<Value> {
        if let Some(o) = msg.as_object_mut() {
            o.insert("token".into(), self.token.clone().into());
        }
        let stream = tokio::net::UnixStream::connect(&self.path).await?;
        let (r, mut w) = stream.into_split();
        let mut line = msg.to_string();
        line.push('\n');
        w.write_all(line.as_bytes()).await?;
        let mut resp = String::new();
        BufReader::new(r).read_line(&mut resp).await?;
        serde_json::from_str(&resp).map_err(std::io::Error::other)
    }

    pub async fn before(&self, tool: &str, args: &Map<String, Value>) -> Before {
        match self.ask(json!({"op": "before", "tool": tool, "args": args})).await {
            Ok(v) if v["go"].as_bool() == Some(true) => Before::Go(v["step"].as_u64().unwrap_or(0)),
            Ok(v) => Before::Refuse(v["message"].as_str().unwrap_or_default().to_owned()),
            Err(e) => Before::Refuse(gone(&e)),
        }
    }

    /// Reports a finished call. `result` is the serialized tool result (`content`, `isError`)
    pub async fn after(&self, step: u64, result: &Value, ms: u64) {
        let text = summarize(result);
        let ok = result["isError"].as_bool() != Some(true);
        let msg = json!({"op": "after", "step": step, "ok": ok, "ms": ms, "text": text});
        if let Err(e) = self.ask(msg).await {
            // stdout is the MCP protocol: stderr only
            eprintln!("task gate: {e}");
        }
    }

    /// Waits for the user's answer. `Ok(message)` when approved, `Err(message)` when rejected or the task ended
    pub async fn confirm(&self, args: &Map<String, Value>) -> Result<String, String> {
        let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_owned();
        let (action, detail) = (s("action"), s("detail"));
        if action.is_empty() {
            return Err(ictl_l10n::tr!("action 不能为空", "action can't be empty").into());
        }
        match self.ask(json!({"op": "confirm", "action": action, "detail": detail})).await {
            Ok(v) if v["approve"].as_bool() == Some(true) => Ok(v["message"].as_str().unwrap_or_default().to_owned()),
            Ok(v) => Err(v["message"].as_str().unwrap_or_default().to_owned()),
            Err(e) => Err(gone(&e)),
        }
    }
}

fn gone(e: &std::io::Error) -> String {
    ictl_l10n::trf!(
        "网页上的任务已经结束或页面服务已停止({e}),没有执行;停下来,不要再操作手机",
        "The task on the web page has ended or its server stopped ({e}); nothing was done. Stop and don't act on the phone any more"
    )
}

/// The text blocks joined, cut to [`MAX_TEXT`]. Screenshots stay with the agent: the page shows a sentence per step
pub fn summarize(result: &Value) -> String {
    let mut text = String::new();
    for c in result["content"].as_array().into_iter().flatten() {
        if c["type"].as_str() == Some("text") {
            if !text.is_empty() {
                text.push('\n');
            }
            text += c["text"].as_str().unwrap_or_default();
        }
    }
    if let Some((i, _)) = text.char_indices().nth(MAX_TEXT) {
        text.truncate(i);
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_joins_text_and_leaves_images_out() {
        let r = json!({"content": [
            {"type": "text", "text": "已点击"},
            {"type": "image", "data": "AAA", "mimeType": "image/jpeg"},
            {"type": "text", "text": "440x956"},
            {"type": "image", "data": "BBB", "mimeType": "image/jpeg"}
        ]});
        assert_eq!(summarize(&r), "已点击\n440x956");
        let long = json!({"content": [{"type": "text", "text": "字".repeat(MAX_TEXT + 10)}]});
        assert_eq!(summarize(&long).chars().count(), MAX_TEXT + 1);
    }

    #[tokio::test]
    async fn a_gate_nobody_listens_on_refuses() {
        let g = Gate { path: "/nonexistent/ictl-gate.sock".into(), token: "t".into() };
        assert!(matches!(g.before("iphone_tap", &Map::new()).await, Before::Refuse(_)));
        let args = json!({"action": "发送消息"}).as_object().cloned().unwrap();
        assert!(g.confirm(&args).await.is_err());
    }

    #[tokio::test]
    async fn talks_one_json_line_with_the_token() {
        let dir = std::env::temp_dir().join(format!("ictl-gate-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("g.sock");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut line = String::new();
            BufReader::new(r).read_line(&mut line).await.unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            w.write_all(b"{\"go\":true,\"step\":7}\n").await.unwrap();
            v
        });
        let g = Gate { path: path.clone(), token: "secret".into() };
        assert_eq!(g.before("iphone_tap", &Map::new()).await, Before::Go(7));
        let seen = server.await.unwrap();
        assert_eq!((seen["op"].as_str(), seen["token"].as_str()), (Some("before"), Some("secret")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
