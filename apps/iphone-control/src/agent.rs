//! AI tasks from the web viewer: a prompt typed on the page goes to a local agent (Claude Code or Codex), which works
//! the phone through `iphone-control mcp`, and the page follows every step.
//!
//! The task itself lives in the daemon (docs/task-control.md, D17): this opens it ([`protocol::open_task`]), keeps
//! its lease while the agent runs, and runs the agent with the task's token, so the agent's MCP server takes part in
//! the task. The daemon holds the agent while the user pauses or takes over, refuses anyone else's actions meanwhile,
//! records the steps and waits for confirmations; the page follows the task's events and controls it through here.
//! What this adds is the agent: its process (in its own process group, with only this program's MCP server and none
//! of its built-in tools that touch the machine), what it says it is doing (sent as notes, shown with the next step)
//! and its summary when it exits. When the task ends some other way (stopped on the page or with
//! `iphone-control task stop`), the agent is stopped too.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ictl_daemon::protocol::{self, Request, Response};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Longest prompt accepted (the page limits the text box to the same)
pub const MAX_PROMPT: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Claude,
    Codex,
}

impl Backend {
    pub const ALL: [Backend; 2] = [Backend::Claude, Backend::Codex];

    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.id() == s)
    }

    /// The CLI on `PATH`
    pub fn find(self) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).map(|d| d.join(self.id())).find(|p| is_executable(p))
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `[{ id, name, available }]` for the page's agent picker
pub fn backends() -> Value {
    Value::Array(
        Backend::ALL.iter().map(|b| json!({"id": b.id(), "name": b.name(), "available": b.find().is_some()})).collect(),
    )
}

pub struct Agents {
    /// This program, for the agent's MCP server
    exe: PathBuf,
    socket: PathBuf,
    workdir: PathBuf,
}

impl Agents {
    pub fn new(socket: &Path) -> anyhow::Result<Arc<Self>> {
        let workdir = std::env::temp_dir().join(format!("iphone-control-agent-{}", unsafe { libc::getuid() }));
        std::fs::create_dir_all(&workdir)?;
        Ok(Arc::new(Self { exe: std::env::current_exe()?, socket: socket.to_path_buf(), workdir }))
    }

    /// Opens a task in the daemon and starts the agent on it; `Err` is a message for the page
    pub async fn start(self: &Arc<Self>, prompt: &str, backend: &str) -> Result<String, String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(ictl_l10n::tr!("先写下要 AI 做的事", "Write what the AI should do first").into());
        }
        if prompt.chars().count() > MAX_PROMPT {
            return Err(ictl_l10n::trf!(
                "任务描述太长，最多 {MAX_PROMPT} 个字",
                "The task is too long, at most {MAX_PROMPT} characters"
            ));
        }
        let Some(backend) = Backend::parse(backend) else {
            return Err(ictl_l10n::trf!("不认识的 agent「{backend}」", "Unknown agent “{backend}”"));
        };
        let Some(program) = backend.find() else {
            return Err(ictl_l10n::trf!(
                "这台机器上没有找到 {}（PATH 里没有 {}）",
                "{} isn't installed on this machine ({} isn't on PATH)",
                backend.name(),
                backend.id()
            ));
        };
        let (opened, lease) = protocol::open_task(&self.socket, prompt, backend.id()).await?;
        let id = opened.id.clone();
        let me = self.clone();
        let prompt = prompt.to_owned();
        let lang = ictl_l10n::lang();
        tokio::spawn(ictl_l10n::scope(lang, async move {
            let mut lease = lease;
            if let Err(e) = me.run(&opened.token, &mut lease, backend, &program, &prompt).await {
                let summary =
                    ictl_l10n::trf!("{} 没能启动或中途出错：{e}", "{} didn't start or failed: {e}", backend.name());
                me.finish(&opened.token, false, summary).await;
            }
            // only now: letting go of the lease first would end the task as abandoned instead
            drop(lease);
        }));
        Ok(id)
    }

    /// pause / resume / takeover / handback / stop / confirm from the page
    pub async fn control(&self, id: &str, action: &str, body: &Value) -> Result<(), String> {
        let req = Request::TaskControl {
            id: id.to_owned(),
            action: action.to_owned(),
            step: body["step"].as_u64(),
            approve: body["approve"].as_bool().unwrap_or(false),
        };
        match protocol::call(&self.socket, &req, Some(Duration::from_secs(10))).await {
            Ok(Response::Text { .. }) => Ok(()),
            Ok(Response::Error { message, .. }) => Err(message),
            Ok(Response::Image { caption, .. }) => Err(caption),
            Err(e) => Err(e.to_string()),
        }
    }

    /// A request on behalf of the task (its token): a note or the end
    async fn as_task(&self, token: &str, req: Request) {
        let call = protocol::call(&self.socket, &req, Some(Duration::from_secs(10)));
        if let Err(e) = protocol::as_task(Some(token.to_owned()), None, call).await {
            tracing::warn!("task: {e}");
        }
    }

    async fn finish(&self, token: &str, ok: bool, summary: String) {
        self.as_task(token, Request::TaskFinish { ok, summary }).await;
    }

    /// Runs the agent until it exits, holding the task's lease
    async fn run(
        &self,
        token: &str,
        lease: &mut protocol::TaskStream,
        backend: Backend,
        program: &Path,
        prompt: &str,
    ) -> anyhow::Result<()> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(self.agent_args(backend, token, prompt))
            .current_dir(&self.workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // the agent's tool calls can wait for the user (a pause, a confirmation): no MCP timeout in practice
            .env("MCP_TOOL_TIMEOUT", "86400000")
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow::anyhow!("no stderr"))?;
        let errs = tokio::spawn(async move {
            let mut tail: Vec<String> = Vec::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                tail.push(l);
                if tail.len() > 20 {
                    tail.remove(0);
                }
            }
            tail
        });
        let (mut summary, mut failure, mut last_text) = (None, None, None);
        let mut ended = false;
        let mut lines = BufReader::new(stdout).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Some(line) = line? else { break };
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    match parse_line(backend, &v) {
                        Some(Out::Thought(t)) => {
                            last_text = Some(t.clone());
                            self.as_task(token, Request::TaskNote { text: t }).await;
                        }
                        Some(Out::Summary(t)) => summary = Some(t),
                        Some(Out::Failure(t)) => failure = Some(t),
                        None => {}
                    }
                }
                // the task ended elsewhere (stopped, interrupted) or the daemon went away: stop the agent too
                e = lease.next(), if !ended => {
                    if e.is_none_or(|e| e.event == "done") {
                        ended = true;
                        if let Some(pid) = pid {
                            kill_group(pid);
                        }
                    }
                }
            }
        }
        let status = child.wait().await?;
        let tail = errs.await.unwrap_or_default();
        if ended {
            return Ok(());
        }
        let ok = status.success() && failure.is_none();
        let summary = summary.or(last_text);
        let summary = match (ok, summary, failure) {
            (true, s, _) => s.unwrap_or_default(),
            (false, s, f) => {
                let why = f
                    .or_else(|| tail.iter().rev().find(|l| !l.trim().is_empty()).cloned())
                    .unwrap_or_else(|| ictl_l10n::trf!("{} 退出（{status}）", "{} exited ({status})", backend.name()));
                match s {
                    Some(s) => format!("{s}\n（{why}）"),
                    None => why,
                }
            }
        };
        self.finish(token, ok, summary).await;
        Ok(())
    }

    fn agent_args(&self, backend: Backend, token: &str, prompt: &str) -> Vec<String> {
        let exe = self.exe.to_string_lossy().into_owned();
        let socket = self.socket.to_string_lossy().into_owned();
        let mut env = Map::new();
        env.insert(protocol::TASK_TOKEN_ENV.into(), token.into());
        // the agent's tools answer in the task's language (this runs in its scope, see `start`)
        env.insert("IPHONE_CONTROL_LANG".into(), ictl_l10n::lang().code().into());
        let s = |v: &str| v.to_owned();
        match backend {
            Backend::Claude => {
                let mcp = json!({"mcpServers": {"iphone": {"command": exe, "args": ["--socket", socket, "mcp"], "env": env}}});
                vec![
                    s("-p"),
                    s(prompt),
                    s("--output-format"),
                    s("stream-json"),
                    s("--verbose"),
                    s("--strict-mcp-config"),
                    s("--mcp-config"),
                    mcp.to_string(),
                    // none of the built-in tools (shell, files, web): only the phone
                    s("--tools"),
                    s(""),
                    s("--allowedTools"),
                    s("mcp__iphone"),
                    s("--permission-mode"),
                    s("dontAsk"),
                    s("--no-session-persistence"),
                    s("--append-system-prompt"),
                    s(instructions()),
                ]
            }
            Backend::Codex => {
                // TOML values; a JSON string is a valid TOML basic string
                let q = |v: &str| Value::from(v).to_string();
                let env_toml = env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",");
                vec![
                    s("exec"),
                    s("--json"),
                    s("--skip-git-repo-check"),
                    s("--ephemeral"),
                    // the agent's own shell can read but not change anything; it is told to use only the phone tools
                    s("--sandbox"),
                    s("read-only"),
                    s("-C"),
                    self.workdir.to_string_lossy().into_owned(),
                    s("-c"),
                    format!("mcp_servers.iphone.command={}", q(&exe)),
                    s("-c"),
                    format!("mcp_servers.iphone.args=[{},{},{}]", q("--socket"), q(&socket), q("mcp")),
                    s("-c"),
                    format!("mcp_servers.iphone.env={{{env_toml}}}"),
                    s("-c"),
                    s("mcp_servers.iphone.tool_timeout_sec=86400"),
                    // the task gate asks the user where it matters (iphone_confirm); Codex's own approval for
                    // tools marked destructive would refuse every tap in a non-interactive run
                    s("-c"),
                    s("mcp_servers.iphone.default_tools_approval_mode=\"approve\""),
                    s("-c"),
                    s("suppress_unstable_features_warning=true"),
                    format!("{}\n\n{}{prompt}", instructions(), ictl_l10n::tr!("任务：", "Task: ")),
                ]
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Out {
    Thought(String),
    Summary(String),
    Failure(String),
}

/// One line of the agent's JSON output
fn parse_line(backend: Backend, v: &Value) -> Option<Out> {
    match backend {
        Backend::Claude => match v["type"].as_str()? {
            "assistant" => {
                let text: Vec<&str> = v["message"]["content"]
                    .as_array()?
                    .iter()
                    .filter(|c| c["type"] == "text")
                    .filter_map(|c| c["text"].as_str())
                    .collect();
                (!text.is_empty()).then(|| Out::Thought(text.join("\n")))
            }
            "result" if v["is_error"].as_bool() == Some(true) => Some(Out::Failure(
                v["result"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(v["subtype"].as_str().unwrap_or("error"))
                    .to_owned(),
            )),
            "result" => v["result"].as_str().map(|s| Out::Summary(s.trim().to_owned())),
            _ => None,
        },
        Backend::Codex => match v["type"].as_str()? {
            "item.completed" if v["item"]["type"] == "agent_message" => {
                v["item"]["text"].as_str().map(|s| Out::Thought(s.trim().to_owned()))
            }
            "turn.failed" => Some(Out::Failure(
                v["error"]["message"]
                    .as_str()
                    .unwrap_or(ictl_l10n::tr!("Codex 没能完成这一轮", "Codex couldn't finish the turn"))
                    .to_owned(),
            )),
            "error" => v["message"].as_str().map(|s| Out::Failure(s.to_owned())),
            _ => None,
        },
    }
}

/// SIGTERM to the agent and everything it started (its MCP server), SIGKILL a little later if still there
fn kill_group(pid: u32) {
    let pgid = pid as libc::pid_t;
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    });
}

/// What the agent is told, besides the task, in the task's language
fn instructions() -> &'static str {
    ictl_l10n::tr!(
        "你在通过 iphone_* 工具操作用户的一台真实 iPhone 或 iPad，完成用户在网页上交给你的任务。用户在网页上实时看着你的每一步。\n\
- 只用 iphone_* 工具。先看清屏幕（iphone_screenshot 或 iphone_list_elements）再操作，每步之后确认结果。\n\
- 每次调用工具之前，用一句简短的中文说明这一步要做什么。\n\
- 付款、购买、下单、转账；删除或清空数据；发送消息、邮件、评论、帖子；修改账号、密码、隐私或安全设置；其他不可撤销的操作：在真正执行那一步之前，先调用 iphone_confirm 等用户同意。用户拒绝就停下，不要换别的方式去做。\n\
- 工具说用户接管过手机时，先截图重新看界面；工具说任务已停止时，立即结束，不要再调用任何工具。\n\
- 做完后用一两句中文总结结果；没能完成就说明卡在哪一步、为什么。",
        "You are operating the user's real iPhone or iPad through the iphone_* tools, to carry out a task the user gave you on a web page. The user watches every step live on that page.\n\
- Use only the iphone_* tools. Look at the screen first (iphone_screenshot or iphone_list_elements), then act, and check the result after each step.\n\
- Before each tool call, say in one short sentence what the step does.\n\
- Payments, purchases, orders, transfers; deleting or clearing data; sending messages, emails, comments, posts; changing account, password, privacy or security settings; anything else that can't be undone: call iphone_confirm right before that step and wait for the user to agree. If the user rejects it, stop; don't do it another way.\n\
- When a tool says the user took over the phone, take a screenshot and look again first; when a tool says the task was stopped, finish at once without calling any more tools.\n\
- When done, sum up the result in a sentence or two; if you couldn't finish, say at which step and why."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_lines() {
        let a = json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "先看屏幕"}, {"type": "tool_use", "name": "x"}]}});
        assert_eq!(parse_line(Backend::Claude, &a), Some(Out::Thought("先看屏幕".into())));
        let only_tool = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "x"}]}});
        assert_eq!(parse_line(Backend::Claude, &only_tool), None);
        let r = json!({"type": "result", "subtype": "success", "is_error": false, "result": " 完成了 "});
        assert_eq!(parse_line(Backend::Claude, &r), Some(Out::Summary("完成了".into())));
        let e = json!({"type": "result", "subtype": "error_max_turns", "is_error": true});
        assert_eq!(parse_line(Backend::Claude, &e), Some(Out::Failure("error_max_turns".into())));
    }

    #[test]
    fn codex_lines() {
        let m = json!({"type": "item.completed", "item": {"type": "agent_message", "text": "打开设置\n"}});
        assert_eq!(parse_line(Backend::Codex, &m), Some(Out::Thought("打开设置".into())));
        // the unstable-features warning comes as an error item: not a failure
        let w = json!({"type": "item.completed", "item": {"type": "error", "message": "Under-development features"}});
        assert_eq!(parse_line(Backend::Codex, &w), None);
        let f = json!({"type": "turn.failed", "error": {"message": "quota"}});
        assert_eq!(parse_line(Backend::Codex, &f), Some(Out::Failure("quota".into())));
    }
}
