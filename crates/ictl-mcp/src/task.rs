//! Taking part in a task (docs/task-control.md): with [`ictl_protocol::TASK_TOKEN_ENV`] set, this MCP server works
//! for that task. Every request it sends carries the token, so the daemon holds them while the task is paused or
//! taken over and refuses them once it ended; each tool call is announced as a step first (`task_step`) and its
//! result reported after (`task_step_done`), so the control page shows the tool and its arguments.
//! [`CONFIRM_TOOL`] is listed too: the agent calls it before payments, deletions, sending messages and the like,
//! and it waits until the user approves or rejects.
//!
//! The daemon enforces all of it; this side only makes the steps readable. Without the variable nothing changes:
//! the tool list and every call are as before.

use ictl_protocol::{self as protocol, Request, Response};
use serde_json::{Map, Value};

/// Listed only in a task: asks the user before a sensitive action
pub const CONFIRM_TOOL: &str = "iphone_confirm";
/// Text results longer than this are cut before they go to the page (the agent still gets all of it)
const MAX_TEXT: usize = 4000;

/// Joins the task named by the environment, if any. Returns whether it did
pub fn join_from_env() -> bool {
    match std::env::var(protocol::TASK_TOKEN_ENV).ok().filter(|t| !t.trim().is_empty()) {
        Some(t) => {
            protocol::join_task(t.trim());
            true
        }
        None => false,
    }
}

/// Whether this process works for a task
pub fn joined() -> bool {
    protocol::process_task().is_some()
}

/// What the daemon says before a tool call
#[derive(Debug, PartialEq)]
pub enum Before {
    /// Go ahead; the call is step `n` of the task
    Go(u64),
    /// Don't run it; tell the agent why
    Refuse(String),
}

/// Announces a tool call; waits while the task is paused or taken over
pub async fn before(socket: &std::path::Path, tool: &str, args: &Map<String, Value>) -> Before {
    let req = Request::TaskStep { tool: tool.to_owned(), args: Value::Object(args.clone()) };
    match protocol::call(socket, &req, None).await {
        Ok(Response::Text { text, .. }) => Before::Go(text.trim().parse().unwrap_or(0)),
        Ok(Response::Error { message, .. }) => Before::Refuse(message),
        Ok(Response::Image { caption, .. }) => Before::Refuse(caption),
        Err(e) => Before::Refuse(gone(&e)),
    }
}

/// Reports a finished call. `result` is the serialized tool result (`content`, `isError`)
pub async fn after(socket: &std::path::Path, step: u64, result: &Value, ms: u64) {
    let ok = result["isError"].as_bool() != Some(true);
    let req = Request::TaskStepDone { step, ok, ms, text: summarize(result) };
    if let Err(e) = protocol::call(socket, &req, Some(std::time::Duration::from_secs(10))).await {
        // stdout is the MCP protocol: stderr only
        eprintln!("task: {e}");
    }
}

/// Waits for the user's answer. `Ok(message)` when approved, `Err(message)` when rejected or the task ended
pub async fn confirm(socket: &std::path::Path, args: &Map<String, Value>) -> Result<String, String> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_owned();
    let req = Request::TaskConfirm { action: s("action"), detail: s("detail") };
    match protocol::call(socket, &req, None).await {
        Ok(Response::Text { text, .. }) => Ok(text),
        Ok(Response::Error { message, .. }) => Err(message),
        Ok(Response::Image { caption, .. }) => Err(caption),
        Err(e) => Err(gone(&e)),
    }
}

fn gone(e: &std::io::Error) -> String {
    ictl_l10n::trf!(
        "连不上守护进程({e}),没有执行;停下来,不要再操作手机",
        "Can't reach the daemon ({e}); nothing was done. Stop and don't act on the phone any more"
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
    use serde_json::json;

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

    /// No daemon: refused, never run unsupervised
    #[tokio::test]
    async fn without_the_daemon_a_task_call_is_refused() {
        let socket = std::path::Path::new("/nonexistent/ictl-task-test.sock");
        assert!(matches!(before(socket, "iphone_tap", &Map::new()).await, Before::Refuse(_)));
        let args = json!({"action": "发送消息"}).as_object().cloned().unwrap();
        assert!(confirm(socket, &args).await.is_err());
    }
}
