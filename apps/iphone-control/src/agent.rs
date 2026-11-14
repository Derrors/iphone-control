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
/// Most apps a task from the page may be limited to (docs/task-control.md 5.5); a name or bundle ID is at most
/// [`MAX_APP_LEN`] characters
pub const MAX_SCOPE_APPS: usize = 30;
const MAX_APP_LEN: usize = 200;

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

/// The agent's working directory under `base`: this user's own (0700, not a link). The agent reads what is there
/// (`CLAUDE.md`, `AGENTS.md`, `.claude/`), so in a shared directory such as `/tmp` a name made by someone else first
/// isn't used: then one made just now, which nobody else can have
fn private_dir(base: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let uid = unsafe { libc::getuid() };
    let make = |dir: &Path| std::fs::DirBuilder::new().mode(0o700).create(dir);
    let dir = base.join(format!("iphone-control-agent-{uid}"));
    match make(&dir) {
        Ok(()) => return Ok(dir),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let m = std::fs::symlink_metadata(&dir)?;
    if m.file_type().is_dir() && m.uid() == uid {
        // made by an earlier version, with the default permissions
        if m.mode() & 0o077 != 0 {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        return Ok(dir);
    }
    tracing::warn!("{} isn't this user's own directory: the agent works in a new one", dir.display());
    for i in 0.. {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let fresh = base.join(format!("iphone-control-agent-{uid}-{}-{nanos}-{i}", std::process::id()));
        match make(&fresh) {
            Ok(()) => return Ok(fresh),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && i < 100 => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!("returns from the loop")
}

impl Agents {
    pub fn new(socket: &Path) -> anyhow::Result<Arc<Self>> {
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute());
        let workdir = private_dir(&base.unwrap_or_else(std::env::temp_dir))?;
        Ok(Arc::new(Self { exe: std::env::current_exe()?, socket: socket.to_path_buf(), workdir }))
    }

    /// Opens a task in the daemon and starts the agent on it; `Err` is a message for the page. `apps` limits the
    /// task to those apps (names or bundle IDs, resolved by the daemon); empty for no limit
    pub async fn start(self: &Arc<Self>, prompt: &str, backend: &str, apps: &[String]) -> Result<String, String> {
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
        let apps = scope_apps(apps)?;
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
        // the daemon resolves names and refuses an app the phone doesn't have
        let (opened, lease) = protocol::open_task(&self.socket, prompt, backend.id(), &apps).await?;
        let id = opened.id.clone();
        let me = self.clone();
        let prompt = prompt.to_owned();
        let lang = ictl_l10n::lang();
        tokio::spawn(ictl_l10n::scope(lang, async move {
            let mut lease = lease;
            if let Err(e) = me.run(&opened.token, &mut lease, backend, &program, &prompt, &apps).await {
                let summary =
                    ictl_l10n::trf!("{} 没能启动或中途出错：{e}", "{} didn't start or failed: {e}", backend.name());
                me.finish(&opened.token, false, summary).await;
            }
            // only now: letting go of the lease first would end the task as abandoned instead
            drop(lease);
        }));
        Ok(id)
    }

    /// pause / resume / takeover / handback / stop / confirm from the page. `by`: who, for the task's log;
    /// `from`: the local process that called the page's API, when known (the daemon refuses the task's agent)
    pub async fn control(
        &self,
        id: &str,
        action: &str,
        body: &Value,
        by: String,
        from: Option<u32>,
    ) -> Result<(), String> {
        let req = Request::TaskControl {
            id: id.to_owned(),
            action: action.to_owned(),
            step: body["step"].as_u64(),
            approve: body["approve"].as_bool().unwrap_or(false),
            by: Some(by),
            for_pid: from,
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
        apps: &[String],
    ) -> anyhow::Result<()> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(self.agent_args(backend, token, prompt, apps))
            .current_dir(&self.workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // the agent's tool calls can wait for the user (a pause, a confirmation): no MCP timeout in practice
            .env("MCP_TOOL_TIMEOUT", "86400000")
            .process_group(0)
            .kill_on_drop(true);
        // the agent and what it starts may not control its task (answer its own confirmations, resume itself);
        // said before it starts, so nothing it does comes first
        self.as_task(token, Request::TaskAgent).await;
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
                            crate::procgroup::stop(pid);
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

    fn agent_args(&self, backend: Backend, token: &str, prompt: &str, apps: &[String]) -> Vec<String> {
        let guide = guidance(apps);
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
                    guide.clone(),
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
                    format!("{guide}\n\n{}{prompt}", ictl_l10n::tr!("任务：", "Task: ")),
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

/// The apps a task from the page is limited to: trimmed, each once, at most [`MAX_SCOPE_APPS`]. `Err` is a message
fn scope_apps(apps: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for a in apps.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
        if a.chars().count() > MAX_APP_LEN {
            return Err(ictl_l10n::tr!("App 名太长", "An app name is too long").into());
        }
        if !out.iter().any(|o| o.eq_ignore_ascii_case(a)) {
            out.push(a.to_owned());
        }
    }
    if out.len() > MAX_SCOPE_APPS {
        return Err(ictl_l10n::trf!(
            "最多限定 {MAX_SCOPE_APPS} 个 App",
            "A task can be limited to at most {MAX_SCOPE_APPS} apps"
        ));
    }
    Ok(out)
}

/// [`instructions`], and the task's app scope when it has one: the agent plans within it from the start (the daemon
/// holds anything outside it for the user either way)
fn guidance(apps: &[String]) -> String {
    if apps.is_empty() {
        return instructions().to_owned();
    }
    let list = apps.join(ictl_l10n::tr!("、", ", "));
    format!(
        "{}\n{}",
        instructions(),
        ictl_l10n::trf!(
            "- 这个任务限定在这些 App 里：{list}（主屏幕总是可以）。在别的 App 里操作、打开别的 App 或网页以外的链接时，那一步会先等用户确认；用户不同意，就回到这些 App 里继续，做不到就结束并说明原因。",
            "- This task is limited to these apps: {list} (the home screen is always fine). Acting in another app, opening one, or opening a link that isn't a web page waits for the user to confirm first; if the user says no, go back to these apps and carry on, or if you can't, finish and say why."
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_from_the_page_is_checked_and_told_to_the_agent() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            scope_apps(&v(&[" 设置 ", "", "com.apple.mobiletimer", "设置"])).unwrap(),
            v(&["设置", "com.apple.mobiletimer"])
        );
        assert_eq!(scope_apps(&v(&["com.apple.Preferences", "COM.APPLE.PREFERENCES"])).unwrap().len(), 1);
        assert!(scope_apps(&["x".repeat(MAX_APP_LEN + 1)]).is_err());
        assert!(scope_apps(&(0..=MAX_SCOPE_APPS).map(|i| format!("app{i}")).collect::<Vec<_>>()).is_err());

        let agents = Agents::new(Path::new("/nonexistent/ctl.sock")).unwrap();
        let scoped = v(&["com.apple.Preferences", "com.apple.mobiletimer"]);
        for backend in Backend::ALL {
            let plain = agents.agent_args(backend, "t", "看看设置", &[]).join("\n");
            let limited = agents.agent_args(backend, "t", "看看设置", &scoped).join("\n");
            assert!(!plain.contains("com.apple.Preferences"), "{backend:?}: no scope, nothing about one");
            assert!(limited.contains("com.apple.Preferences、com.apple.mobiletimer"), "{backend:?}");
        }
    }

    #[test]
    fn the_agent_works_in_a_directory_of_its_own() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let base = std::env::temp_dir().join(format!("ictl-agent-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let mine = private_dir(&base).unwrap();
        assert_eq!(std::fs::metadata(&mine).unwrap().mode() & 0o777, 0o700);
        // an earlier version's, with the default permissions
        std::fs::set_permissions(&mine, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(private_dir(&base).unwrap(), mine);
        assert_eq!(std::fs::metadata(&mine).unwrap().mode() & 0o777, 0o700);
        // the name taken by something that isn't a directory of ours (here: a link to one)
        std::fs::remove_dir(&mine).unwrap();
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &mine).unwrap();
        let fresh = private_dir(&base).unwrap();
        assert!(fresh != mine && fresh.starts_with(&base) && !fresh.starts_with(&elsewhere));
        assert!(std::fs::symlink_metadata(&fresh).unwrap().is_dir());
        std::fs::remove_dir_all(base).unwrap();
    }

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
