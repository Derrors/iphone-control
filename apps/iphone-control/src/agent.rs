//! AI tasks for the web viewer: a prompt typed on the page goes to a local agent (Claude Code or Codex), which works
//! the phone through `iphone-control mcp`, and the page follows every step.
//!
//! - The agent runs as a child process (`claude -p … --output-format stream-json` or `codex exec --json …`) in its
//!   own process group, with only this program's MCP server and none of its built-in tools that touch the machine.
//! - Its MCP server gets the task gate ([`ictl_mcp::gate`]): before each tool call it asks the viewer over a Unix
//!   socket next to the daemon's ([`GateServer`]), after the call it reports the result. That is where steps come
//!   from, where a pause or the user taking over holds the agent, and where [`ictl_mcp::gate::CONFIRM_TOOL`] waits
//!   for the user's answer. The agent's own output only adds what it says it is doing (attached to the next step)
//!   and its final summary.
//! - The page gets the task as server-sent events: `step`, `confirm`, `state`, `done`. Every event is kept, so a
//!   page that (re)connects gets the whole task replayed.
//!
//! One task at a time: there is one phone. While a task runs and the user hasn't taken over, the viewer refuses the
//! page's manual actions ([`Agents::manual_allowed`]), so the user and the agent don't tap at the same time.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, broadcast, oneshot};

/// Longest prompt accepted (the page limits the text box to the same)
pub const MAX_PROMPT: usize = 4000;
/// Finished tasks kept for replay; each can hold dozens of screenshots
const KEEP_FINISHED: usize = 4;
/// A thought printed by the agent this soon after a step started belongs to that step (it reached the viewer
/// through the agent's stdout, slightly after the tool call reached the gate)
const LATE_THOUGHT: Duration = Duration::from_secs(2);
/// Tools that look without acting: allowed right after the user hands control back
const LOOKING: [&str; 3] = ["iphone_screenshot", "iphone_list_elements", "iphone_status"];

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Paused,
    Takeover,
    Waiting,
    Done,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Takeover => "takeover",
            Self::Waiting => "waiting",
            Self::Done => "done",
        }
    }
}

/// One event for the page: SSE event name and its JSON data
#[derive(Debug, Clone)]
pub struct Event {
    pub name: &'static str,
    pub data: Value,
}

struct TaskState {
    status: Status,
    log: Vec<Event>,
    next_step: u64,
    /// What the agent said it is about to do, for the next step
    thought: Option<String>,
    /// The latest step: id, when it started, still running, has a thought
    last: Option<(u64, Instant, bool, bool)>,
    /// Steps by id whose tool taps by text: where it tapped comes with the result
    taps_by_text: Vec<u64>,
    /// The user handed control back: the agent looks at the screen again before acting
    recheck: bool,
    confirm: Option<(u64, oneshot::Sender<bool>)>,
    /// The agent's last words, the summary when it ends without a result line
    last_text: Option<String>,
    summary: Option<String>,
    failure: Option<String>,
    pid: Option<u32>,
    stopped: bool,
    /// The user turned down a step: the task ends as stopped, not as done
    rejected: bool,
}

pub struct Task {
    pub id: String,
    token: String,
    /// The page's language when the task started: its messages and the agent's tools use it
    lang: ictl_l10n::Lang,
    st: Mutex<TaskState>,
    tx: broadcast::Sender<Event>,
    /// Wakes calls held at the gate when the status changes
    changed: Notify,
}

impl Task {
    fn lock(&self) -> std::sync::MutexGuard<'_, TaskState> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn emit(&self, st: &mut TaskState, name: &'static str, data: Value) {
        let e = Event { name, data };
        st.log.push(e.clone());
        let _ = self.tx.send(e);
    }

    fn set_status(&self, st: &mut TaskState, s: Status) {
        if st.status != s && st.status != Status::Done {
            st.status = s;
            if s != Status::Done {
                self.emit(st, "state", json!({"state": s.name()}));
            }
            self.changed.notify_waiters();
        }
    }

    /// The whole task so far, and the events after it (taken together, so nothing falls between)
    pub fn subscribe(&self) -> (Vec<Event>, broadcast::Receiver<Event>, bool) {
        let st = self.lock();
        (st.log.clone(), self.tx.subscribe(), st.status == Status::Done)
    }

    fn thought(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let mut st = self.lock();
        st.last_text = Some(text.to_owned());
        match st.last {
            // arrived just after its step reached the gate
            Some((id, at, true, false)) if at.elapsed() < LATE_THOUGHT => {
                st.last = Some((id, at, true, true));
                self.emit(&mut st, "step", json!({"id": id, "thought": text}));
            }
            _ => st.thought = Some(text.to_owned()),
        }
    }

    fn finish(&self, ok: bool, stopped: bool, summary: String) {
        let mut st = self.lock();
        if st.status == Status::Done {
            return;
        }
        st.status = Status::Done;
        if let Some((_, tx)) = st.confirm.take() {
            let _ = tx.send(false);
        }
        self.emit(&mut st, "done", json!({"ok": ok, "stopped": stopped, "summary": summary}));
        self.changed.notify_waiters();
    }

    fn is_done(&self) -> bool {
        self.lock().status == Status::Done
    }
}

#[derive(Default)]
struct Registry {
    tasks: Vec<Arc<Task>>,
}

pub struct Agents {
    reg: Mutex<Registry>,
    /// This program, for the agent's MCP server
    exe: PathBuf,
    socket: PathBuf,
    gate: PathBuf,
    workdir: PathBuf,
}

impl Agents {
    pub fn new(socket: &Path, gate: PathBuf) -> anyhow::Result<Arc<Self>> {
        let workdir = std::env::temp_dir().join(format!("iphone-control-agent-{}", unsafe { libc::getuid() }));
        std::fs::create_dir_all(&workdir)?;
        Ok(Arc::new(Self {
            reg: Mutex::default(),
            exe: std::env::current_exe()?,
            socket: socket.to_path_buf(),
            gate,
            workdir,
        }))
    }

    fn reg(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.reg.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn task(&self, id: &str) -> Option<Arc<Task>> {
        self.reg().tasks.iter().find(|t| t.id == id).cloned()
    }

    fn active(&self) -> Option<Arc<Task>> {
        self.reg().tasks.iter().find(|t| !t.is_done()).cloned()
    }

    /// The page may act on the phone itself: no task, or the user has taken over
    pub fn manual_allowed(&self) -> bool {
        self.active().is_none_or(|t| t.lock().status == Status::Takeover)
    }

    pub fn busy(&self) -> bool {
        self.active().is_some()
    }

    /// Starts a task; `Err` is a message for the page
    pub fn start(self: &Arc<Self>, prompt: &str, backend: &str) -> Result<String, String> {
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
        let task = {
            let mut reg = self.reg();
            if reg.tasks.iter().any(|t| !t.is_done()) {
                return Err(ictl_l10n::tr!(
                    "已经有一个任务在执行，等它结束或先停止它",
                    "A task is already running; wait for it to end or stop it first"
                )
                .into());
            }
            // all finished by now: keep the newest few for pages that reconnect
            while reg.tasks.len() >= KEEP_FINISHED {
                reg.tasks.remove(0);
            }
            let task = Arc::new(Task {
                id: uuid::Uuid::new_v4().simple().to_string()[..12].to_owned(),
                token: uuid::Uuid::new_v4().simple().to_string(),
                lang: ictl_l10n::lang(),
                st: Mutex::new(TaskState {
                    status: Status::Running,
                    log: Vec::new(),
                    next_step: 1,
                    thought: None,
                    last: None,
                    taps_by_text: Vec::new(),
                    recheck: false,
                    confirm: None,
                    last_text: None,
                    summary: None,
                    failure: None,
                    pid: None,
                    stopped: false,
                    rejected: false,
                }),
                tx: broadcast::channel(1024).0,
                changed: Notify::new(),
            });
            reg.tasks.push(task.clone());
            task
        };
        let me = self.clone();
        let (t, prompt) = (task.clone(), prompt.to_owned());
        let lang = task.lang;
        tokio::spawn(ictl_l10n::scope(lang, async move {
            if let Err(e) = me.run(&t, backend, &program, &prompt).await {
                t.finish(
                    false,
                    false,
                    ictl_l10n::trf!("{} 没能启动或中途出错：{e}", "{} didn't start or failed: {e}", backend.name()),
                );
            }
        }));
        Ok(task.id.clone())
    }

    /// pause / resume / takeover / handback / stop / confirm from the page
    pub fn control(&self, id: &str, action: &str, body: &Value) -> Result<(), String> {
        let Some(task) = self.task(id) else { return Err(ictl_l10n::tr!("没有这个任务", "No such task").into()) };
        let mut st = task.lock();
        if st.status == Status::Done {
            return Err(ictl_l10n::tr!("任务已经结束", "The task has ended").into());
        }
        match (action, st.status) {
            ("pause", Status::Running) => task.set_status(&mut st, Status::Paused),
            ("resume", Status::Paused) => task.set_status(&mut st, Status::Running),
            ("takeover", Status::Running | Status::Paused) => task.set_status(&mut st, Status::Takeover),
            ("handback", Status::Takeover) => {
                st.recheck = true;
                task.set_status(&mut st, Status::Running);
            }
            ("confirm", Status::Waiting) => {
                let approve = body["approve"].as_bool().unwrap_or(false);
                let want = body["step"].as_u64();
                match st.confirm.take() {
                    Some((step, tx)) if want.is_none_or(|w| w == step) => {
                        let _ = tx.send(approve);
                    }
                    other => {
                        st.confirm = other;
                        return Err(ictl_l10n::tr!(
                            "这一步已经不在等确认了",
                            "That step is no longer waiting for confirmation"
                        )
                        .into());
                    }
                }
            }
            ("stop", _) => {
                st.stopped = true;
                let pid = st.pid;
                let n = st.next_step - 1;
                drop(st);
                if let Some(pid) = pid {
                    kill_group(pid);
                }
                task.finish(
                    false,
                    true,
                    ictl_l10n::trf!(
                        "在第 {n} 步停下了。已经做完的操作不会撤销。",
                        "Stopped at step {n}. What was already done isn't undone."
                    ),
                );
                return Ok(());
            }
            (a, s) => return Err(ictl_l10n::trf!("现在（{}）不能{a}", "Can't {a} now ({})", s.name())),
        }
        Ok(())
    }

    async fn run(&self, task: &Arc<Task>, backend: Backend, program: &Path, prompt: &str) -> anyhow::Result<()> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(self.agent_args(backend, &task.token, prompt))
            .current_dir(&self.workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // the agent's tool calls can wait for the user (a pause, a confirmation): no MCP timeout in practice
            .env("MCP_TOOL_TIMEOUT", "86400000")
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;
        task.lock().pid = child.id();
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
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            match parse_line(backend, &v) {
                Some(Out::Thought(t)) => task.thought(&t),
                Some(Out::Summary(t)) => task.lock().summary = Some(t),
                Some(Out::Failure(t)) => task.lock().failure = Some(t),
                None => {}
            }
        }
        let status = child.wait().await?;
        let tail = errs.await.unwrap_or_default();
        let (summary, failure, stopped, rejected) = {
            let st = task.lock();
            (st.summary.clone().or_else(|| st.last_text.clone()), st.failure.clone(), st.stopped, st.rejected)
        };
        if stopped {
            return Ok(());
        }
        if rejected && failure.is_none() {
            let s = summary.unwrap_or_else(|| {
                ictl_l10n::tr!(
                    "你拒绝了一步操作，任务在那里停下了。",
                    "You rejected a step, so the task stopped there."
                )
                .into()
            });
            task.finish(false, true, s);
            return Ok(());
        }
        let ok = status.success() && failure.is_none();
        let summary = match (ok, summary, failure) {
            (true, Some(s), _) => s,
            (true, None, _) => {
                ictl_l10n::tr!("任务结束了，agent 没有给出总结。", "The task ended without a summary from the agent.")
                    .into()
            }
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
        task.finish(ok, false, summary);
        Ok(())
    }

    fn agent_args(&self, backend: Backend, token: &str, prompt: &str) -> Vec<String> {
        let exe = self.exe.to_string_lossy().into_owned();
        let socket = self.socket.to_string_lossy().into_owned();
        let gate = self.gate.to_string_lossy().into_owned();
        let mut env = Map::new();
        env.insert(ictl_mcp::gate::GATE_ENV.into(), gate.clone().into());
        env.insert(ictl_mcp::gate::TOKEN_ENV.into(), token.into());
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
                let env_toml = env.iter().map(|(k, v)| format!("{k}={}", v)).collect::<Vec<_>>().join(",");
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

    /// One gate message from the agent's MCP server; the reply line
    async fn gate(&self, msg: Value) -> Value {
        let token = msg["token"].as_str().unwrap_or_default();
        let task = self.reg().tasks.iter().find(|t| t.token == token).cloned();
        let Some(task) = task else {
            return refuse(ictl_l10n::tr!(
                "这个任务已经不在了：停下来，不要再操作手机",
                "This task no longer exists: stop and don't act on the phone any more"
            ));
        };
        ictl_l10n::scope(task.lang, async {
            match msg["op"].as_str() {
                Some("before") => task.before(msg["tool"].as_str().unwrap_or_default(), &msg["args"]).await,
                Some("after") => {
                    task.after(&msg);
                    json!({})
                }
                Some("confirm") => task.confirm(&msg).await,
                _ => refuse("bad gate message"),
            }
        })
        .await
    }
}

fn refuse(message: &str) -> Value {
    json!({"go": false, "approve": false, "message": message})
}

impl Task {
    /// Holds the call while the task is paused, taken over or waiting; then records it as a step
    async fn before(&self, tool: &str, args: &Value) -> Value {
        loop {
            let notified = self.changed.notified();
            {
                let mut st = self.lock();
                if st.status == Status::Done || st.stopped {
                    return refuse(ictl_l10n::tr!(
                        "用户停止了这个任务：不要再调用任何工具，直接结束",
                        "The user stopped this task: don't call any more tools, just finish"
                    ));
                }
                if st.status == Status::Running {
                    if tool == "iphone_select_device" {
                        return refuse(ictl_l10n::tr!(
                            "网页上的任务不能切换设备，继续操作当前这台",
                            "A task from the web page can't switch devices; keep using the current one"
                        ));
                    }
                    if st.recheck && !LOOKING.contains(&tool) {
                        st.recheck = false;
                        return refuse(ictl_l10n::tr!(
                            "用户刚才接管并操作过手机，界面可能已经变了：先用 iphone_screenshot 重新看一遍，再决定下一步",
                            "The user took over and used the phone, so the screen may have changed: look again with iphone_screenshot before the next step"
                        ));
                    }
                    st.recheck = false;
                    let id = st.next_step;
                    st.next_step += 1;
                    let thought = st.thought.take();
                    st.last = Some((id, Instant::now(), true, thought.is_some()));
                    let mut d = json!({"id": id, "tool": tool, "args": args, "status": "running"});
                    if let Some(t) = thought {
                        d["thought"] = t.into();
                    }
                    if let Some(g) = gesture(tool, args, "") {
                        d["gesture"] = g;
                    }
                    if tool == "iphone_tap_element" {
                        st.taps_by_text.push(id);
                    }
                    self.emit(&mut st, "step", d);
                    return json!({"go": true, "step": id});
                }
            }
            notified.await;
        }
    }

    fn after(&self, msg: &Value) {
        let mut st = self.lock();
        let Some(id) = msg["step"].as_u64() else { return };
        if let Some((last, at, _, th)) = st.last
            && last == id
        {
            st.last = Some((last, at, false, th));
        }
        let ok = msg["ok"].as_bool().unwrap_or(true);
        let text = msg["text"].as_str().unwrap_or_default();
        let mut d = json!({"id": id, "status": if ok { "ok" } else { "error" }, "ms": msg["ms"], "result": first_lines(text, 6)});
        // where iphone_tap_element tapped is only known now, from its result
        let by_text = st.taps_by_text.iter().position(|&s| s == id).map(|i| st.taps_by_text.swap_remove(i));
        if ok
            && by_text.is_some()
            && let Some(g) = gesture("iphone_tap_element", &Value::Null, text)
        {
            d["gesture"] = g;
        }
        self.emit(&mut st, "step", d);
    }

    async fn confirm(&self, msg: &Value) -> Value {
        let (rx, id) = {
            let mut st = self.lock();
            if st.status == Status::Done || st.stopped {
                return refuse(ictl_l10n::tr!("用户停止了这个任务", "The user stopped this task"));
            }
            let id = st.next_step;
            st.next_step += 1;
            let thought = st.thought.take();
            let (action, detail) =
                (msg["action"].as_str().unwrap_or_default(), msg["detail"].as_str().unwrap_or_default());
            let args = json!({"action": action, "detail": detail});
            let mut d = json!({"id": id, "tool": "iphone_confirm", "args": args, "status": "waiting"});
            if let Some(t) = thought {
                d["thought"] = t.into();
            }
            st.last = Some((id, Instant::now(), true, d.get("thought").is_some()));
            self.emit(&mut st, "step", d);
            self.emit(&mut st, "confirm", json!({"step": id, "action": action, "detail": detail}));
            let (tx, rx) = oneshot::channel();
            st.confirm = Some((id, tx));
            self.set_status(&mut st, Status::Waiting);
            (rx, id)
        };
        let approve = rx.await.unwrap_or(false);
        let mut st = self.lock();
        st.last = None;
        let stopped = st.status == Status::Done || st.stopped;
        st.rejected |= !approve && !stopped;
        let (status, result) = match (approve, stopped) {
            (true, false) => ("ok", ictl_l10n::tr!("你同意了，继续执行。", "You approved; carrying on.")),
            (_, true) => ("stopped", ictl_l10n::tr!("任务已停止。", "The task was stopped.")),
            (false, false) => {
                ("rejected", ictl_l10n::tr!("你拒绝了这一步，没有执行。", "You rejected this step; it wasn't done."))
            }
        };
        self.emit(&mut st, "step", json!({"id": id, "status": status, "result": result}));
        if !stopped {
            self.set_status(&mut st, Status::Running);
        }
        if approve && !stopped {
            json!({"approve": true, "message": ictl_l10n::tr!("用户同意了，继续执行这一步。", "The user approved; go ahead with this step.")})
        } else if stopped {
            refuse(ictl_l10n::tr!(
                "用户停止了这个任务：不要再调用任何工具，直接结束",
                "The user stopped this task: don't call any more tools, just finish"
            ))
        } else {
            refuse(ictl_l10n::tr!(
                "用户拒绝了这一步：不要执行，也不要换别的方式去做。停下来，说明没做这一步",
                "The user rejected this step: don't do it, and don't do it another way. Stop and say it wasn't done"
            ))
        }
    }
}

/// Where a step touches the screen, for the mark on the page: `{ kind, x, y, x2?, y2? }`.
/// For `iphone_tap_element` it comes from the result text (`… @(x,y) WxH`)
fn gesture(tool: &str, args: &Value, result: &str) -> Option<Value> {
    let f = |k: &str| args.get(k).and_then(Value::as_f64);
    match tool {
        "iphone_tap" => Some(json!({"kind": "tap", "x": f("x")?, "y": f("y")?})),
        "iphone_long_press" => Some(json!({"kind": "long_press", "x": f("x")?, "y": f("y")?})),
        "iphone_swipe" => {
            Some(json!({"kind": "swipe", "x": f("from_x")?, "y": f("from_y")?, "x2": f("to_x")?, "y2": f("to_y")?}))
        }
        "iphone_tap_element" if !result.is_empty() => {
            let first = result.lines().next()?;
            let at = &first[first.find(" @(")? + 3..];
            let (x, rest) = at.split_once(',')?;
            let y = rest.split(')').next()?;
            Some(json!({"kind": "tap", "x": x.trim().parse::<f64>().ok()?, "y": y.trim().parse::<f64>().ok()?}))
        }
        _ => None,
    }
}

fn first_lines(text: &str, n: usize) -> String {
    let mut out: Vec<&str> = text.lines().take(n).collect();
    if text.lines().count() > n {
        out.push("…");
    }
    out.join("\n")
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

/// The Unix socket the agents' MCP servers ask (see the module docs). Only this user can connect (the file is
/// 0600 in the daemon's directory), and every message carries the task's token
pub struct GateServer;

impl GateServer {
    pub async fn serve(path: PathBuf, agents: Arc<Agents>) -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                let agents = agents.clone();
                tokio::spawn(async move {
                    let (r, mut w) = s.into_split();
                    let mut line = String::new();
                    let mut r = BufReader::new(r);
                    if r.read_line(&mut line).await.is_err() || line.len() > 4 * 1024 * 1024 {
                        return;
                    }
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else { return };
                    // the MCP server gives up on a call (the agent cancelled it): stop waiting for the user
                    let reply = tokio::select! {
                        v = agents.gate(msg) => v,
                        _ = closed(&mut r) => return,
                    };
                    let mut out = reply.to_string();
                    out.push('\n');
                    let _ = w.write_all(out.as_bytes()).await;
                });
            }
        });
        Ok(())
    }
}

/// Resolves when the other side closes the connection
async fn closed(r: &mut BufReader<tokio::net::unix::OwnedReadHalf>) {
    let mut b = [0u8; 1];
    loop {
        match tokio::io::AsyncReadExt::read(r, &mut b).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
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

    #[test]
    fn gestures_for_the_marks() {
        assert_eq!(
            gesture("iphone_tap", &json!({"x": 10, "y": 20.5}), ""),
            Some(json!({"kind": "tap", "x": 10.0, "y": 20.5}))
        );
        assert_eq!(
            gesture("iphone_swipe", &json!({"from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4}), ""),
            Some(json!({"kind": "swipe", "x": 1.0, "y": 2.0, "x2": 3.0, "y2": 4.0}))
        );
        let r = "已点击 Button \"通用\" id=\"com.apple.settings.general\" @(220,776) 400x52";
        assert_eq!(
            gesture("iphone_tap_element", &Value::Null, r),
            Some(json!({"kind": "tap", "x": 220.0, "y": 776.0}))
        );
        assert_eq!(gesture("iphone_tap_element", &Value::Null, "没找到包含 \"x\" 的元素"), None);
        assert_eq!(gesture("iphone_screenshot", &json!({}), ""), None);
    }

    fn task() -> Arc<Task> {
        Arc::new(Task {
            id: "t".into(),
            token: "k".into(),
            lang: ictl_l10n::Lang::Zh,
            st: Mutex::new(TaskState {
                status: Status::Running,
                log: Vec::new(),
                next_step: 1,
                thought: None,
                last: None,
                taps_by_text: Vec::new(),
                recheck: false,
                confirm: None,
                last_text: None,
                summary: None,
                failure: None,
                pid: None,
                stopped: false,
                rejected: false,
            }),
            tx: broadcast::channel(16).0,
            changed: Notify::new(),
        })
    }

    #[tokio::test]
    async fn a_paused_task_holds_the_call_until_resumed() {
        let t = task();
        t.thought("先看一下屏幕");
        t.set_status(&mut t.lock(), Status::Paused);
        let held = {
            let t = t.clone();
            tokio::spawn(async move { t.before("iphone_tap", &json!({"x": 1, "y": 2})).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!held.is_finished(), "paused: the call waits");
        t.set_status(&mut t.lock(), Status::Running);
        let v = held.await.unwrap();
        assert_eq!((v["go"].as_bool(), v["step"].as_u64()), (Some(true), Some(1)));
        let log = t.lock().log.clone();
        let step = log.iter().find(|e| e.name == "step").unwrap();
        assert_eq!(step.data["thought"], "先看一下屏幕");
        assert_eq!(step.data["gesture"]["kind"], "tap");
    }

    #[tokio::test]
    async fn only_a_tap_by_text_gets_a_mark_from_its_result() {
        let t = task();
        let listed = t.before("iphone_list_elements", &json!({})).await["step"].as_u64().unwrap();
        t.after(&json!({"step": listed, "ok": true, "text": "NavigationBar \"世界时钟\" @(220,115) 440x106"}));
        assert!(t.lock().log.last().unwrap().data.get("gesture").is_none());
        let tapped = t.before("iphone_tap_element", &json!({"text": "秒表"})).await["step"].as_u64().unwrap();
        t.after(&json!({"step": tapped, "ok": true, "text": "已点击 Button \"秒表\" @(267,904) 108x54"}));
        assert_eq!(t.lock().log.last().unwrap().data["gesture"], json!({"kind": "tap", "x": 267.0, "y": 904.0}));
    }

    #[tokio::test]
    async fn after_a_takeover_the_agent_looks_again_before_acting() {
        let t = task();
        t.lock().recheck = true;
        assert_eq!(t.before("iphone_tap", &json!({"x": 1, "y": 2})).await["go"], false);
        t.lock().recheck = true;
        assert_eq!(t.before("iphone_screenshot", &json!({})).await["go"], true);
        assert_eq!(t.before("iphone_tap", &json!({"x": 1, "y": 2})).await["go"], true, "only once");
    }

    #[tokio::test]
    async fn a_rejected_confirmation_tells_the_agent_not_to_do_it() {
        let t = task();
        let asked = {
            let t = t.clone();
            tokio::spawn(async move { t.confirm(&json!({"action": "发送消息", "detail": "给妈妈"})).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(t.lock().status, Status::Waiting);
        let (step, tx) = t.lock().confirm.take().unwrap();
        assert_eq!(step, 1);
        tx.send(false).unwrap();
        let v = asked.await.unwrap();
        assert_eq!(v["approve"], false);
        assert_eq!(t.lock().status, Status::Running);
        assert!(t.lock().rejected, "the task will end as stopped");
        assert!(t.lock().log.iter().any(|e| e.name == "confirm" && e.data["action"] == "发送消息"));
    }

    #[tokio::test]
    async fn a_stopped_task_refuses_everything() {
        let t = task();
        t.finish(false, true, "停了".into());
        assert_eq!(t.before("iphone_screenshot", &json!({})).await["go"], false);
        assert_eq!(t.confirm(&json!({"action": "a"})).await["approve"], false);
        assert_eq!(t.lock().log.last().unwrap().name, "done");
    }

    #[test]
    fn a_late_thought_joins_the_step_it_belongs_to() {
        let t = task();
        t.lock().last = Some((3, Instant::now(), true, false));
        t.thought("点「通用」");
        assert_eq!(t.lock().log.last().unwrap().data, json!({"id": 3, "thought": "点「通用」"}));
        // a finished step keeps it for the next one (the final summary doesn't land on the last step)
        t.lock().last = Some((4, Instant::now(), false, false));
        t.thought("都做完了");
        assert_eq!(t.lock().thought.as_deref(), Some("都做完了"));
    }
}
