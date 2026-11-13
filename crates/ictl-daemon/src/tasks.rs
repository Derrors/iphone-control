//! Tasks: who may act on the phone while an AI task runs, enforced where the actions are carried out
//! (docs/task-control.md, D17).
//!
//! A task is opened on a connection that stays open ([`crate::protocol::Request::TaskOpen`]); that connection is its
//! lease, and when it closes before the task ended the task is abandoned. The opener gets a token and hands it to
//! the agent (`IPHONE_CONTROL_TASK_TOKEN` for `iphone-control mcp`); every request with the token belongs to the task.
//!
//! While a task is active ([`Tasks::admit`]):
//! - its requests wait while it is paused, taken over or waiting for a confirmation, and only go to the device it was
//!   opened on; after the user handed control back, or someone else acted on the phone, an action is refused until
//!   the agent has looked at the screen again;
//! - requests without the token may look (screenshots, elements, status) but not act, unless the user has taken
//!   over; lifting a finger is always allowed, switching the device never is;
//! - a token of a task that ended, or that the daemon doesn't know, is refused for everything: the agent stops.
//!
//! Steps: a tool call announces itself ([`Tasks::step`]); the requests it makes carry the step number. An action
//! with the token but no step (a client that doesn't announce) becomes a step of its own, so nothing the task does
//! is missing from it. Every event goes to `tasks/<id>.jsonl` too; tasks that hadn't ended when the daemon stopped
//! are marked interrupted when it starts again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast, oneshot};

use crate::protocol::{Request, Response, TaskEvent, TaskInfo, TouchPhase};

/// Task logs kept on disk (and in memory: a task's events are text only, a few KB)
const KEEP: usize = 50;
/// A thought sent this soon after a step started belongs to that step (it comes from the agent's output, which
/// reaches its opener slightly after the tool call reached the daemon)
const LATE_THOUGHT: Duration = Duration::from_secs(2);
/// Lines of a step's result kept for the page
const RESULT_LINES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Completed,
    Failed,
    Cancelled,
    Rejected,
    Abandoned,
    Interrupted,
}

impl End {
    fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Rejected => "rejected",
            Self::Abandoned => "abandoned",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Paused,
    Takeover,
    Waiting,
    Ended(End),
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Takeover => "takeover",
            Self::Waiting => "waiting",
            Self::Ended(e) => e.name(),
        }
    }
    fn active(self) -> bool {
        !matches!(self, Self::Ended(_))
    }
}

struct TaskState {
    status: Status,
    log: Vec<TaskEvent>,
    next_step: u64,
    /// What the agent said it is about to do, for the next step
    thought: Option<String>,
    /// The latest step: id, when it started, still running, has a thought
    last: Option<(u64, Instant, bool, bool)>,
    /// Steps whose tool taps by text: where it tapped comes with the result
    taps_by_text: Vec<u64>,
    /// The screen may have changed under the agent (the user acted, or handed back): look before acting
    recheck: bool,
    confirm: Option<(u64, oneshot::Sender<bool>)>,
    /// The user turned down a step: the task ends as rejected
    rejected: bool,
    ended: Option<u64>,
    /// Bundle IDs the task may act in ([`Target`]); empty: anywhere. Grows when the user lets it go further
    scope: Vec<String>,
}

pub struct Task {
    pub id: String,
    token: String,
    title: String,
    owner: String,
    udid: String,
    started: u64,
    file: Option<PathBuf>,
    st: Mutex<TaskState>,
    tx: broadcast::Sender<TaskEvent>,
    /// Wakes requests held for the task when its status changes
    changed: Notify,
}

/// How [`Tasks::admit`] lets a request through
pub enum Admission {
    /// No task is involved
    Free,
    /// Part of this task
    Actor(Arc<Task>),
    /// Someone other than the task acts while the user has taken over
    Supervisor(Arc<Task>),
}

pub struct Tasks {
    dir: Option<PathBuf>,
    list: Mutex<Vec<Arc<Task>>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn refuse(msg: &str) -> Response {
    Response::error(msg)
}

fn ended_message() -> &'static str {
    ictl_l10n::tr!(
        "这个任务已经结束（被停止、中断或不存在）：不要再调用任何工具，直接结束",
        "This task has ended (stopped, interrupted or unknown): don't call any more tools, just finish"
    )
}

/// Requests that show the agent the screen: after one, it may act again ([`TaskState::recheck`])
fn observes(req: &Request) -> bool {
    matches!(req, Request::Screenshot { .. } | Request::ListElements { .. } | Request::WaitFor { .. })
}

/// The home screen, the lock screen and notifications: always in a task's scope (getting back from elsewhere)
const SPRINGBOARD: &str = "com.apple.springboard";
/// Where http(s) links open
const SAFARI: &str = "com.apple.mobilesafari";

/// What a request of a scoped task needs looked up before it may run ([`scope_check`])
#[derive(Debug, PartialEq)]
pub enum Check {
    /// Launches this app (a name or a bundle ID, to resolve)
    Launch(String),
    /// Opens this URL
    Url(String),
    /// Acts on whatever app is in front
    Foreground,
    /// Always asks, whatever the scope
    Always { action: String, detail: String },
}

/// What a scoped task's action touches, once looked up
#[derive(Debug, PartialEq)]
pub enum Target {
    Launch(String),
    Url(String),
    /// The foreground app's bundle ID; empty when WDA didn't say
    Foreground(String),
    Always {
        action: String,
        detail: String,
    },
}

/// What a request does to the app scope. Pressing buttons and quitting apps aren't checked: they are how an agent
/// gets back after the user said no. Every variant is listed (no `_`), so a new request type has to be decided here
pub fn scope_check(req: &Request) -> Option<Check> {
    match req {
        Request::LaunchApp { app } => Some(Check::Launch(app.clone())),
        Request::OpenUrl { url } => Some(Check::Url(url.clone())),
        Request::Tap { .. }
        | Request::TapElement { .. }
        | Request::LongPress { .. }
        | Request::Swipe { .. }
        | Request::TypeText { .. }
        | Request::WebEval { .. }
        | Request::Touch { phase: TouchPhase::Down | TouchPhase::Move, .. } => Some(Check::Foreground),
        Request::InstallApp { path } => {
            Some(Check::Always { action: ictl_l10n::tr!("安装 App", "Install an app").into(), detail: path.clone() })
        }
        Request::UninstallApp { app } => Some(Check::Always {
            action: ictl_l10n::tr!("卸载 App（连同它的数据）", "Uninstall an app (with its data)").into(),
            detail: app.clone(),
        }),
        Request::Touch { phase: TouchPhase::Prepare | TouchPhase::Up, .. }
        | Request::PressButton { .. }
        | Request::TerminateApp { .. }
        | Request::SelectDevice { .. }
        | Request::Status
        | Request::ScreenStream
        | Request::Screenshot { .. }
        | Request::ListElements { .. }
        | Request::ListApps { .. }
        | Request::Settle { .. }
        | Request::AlertCheck
        | Request::WaitFor { .. }
        | Request::Daemon
        | Request::Doctor
        | Request::Resign { .. }
        | Request::UnlockReset
        | Request::ListDevices
        | Request::AppLogs { .. }
        | Request::CrashReports { .. }
        | Request::WebTargets
        | Request::TaskOpen { .. }
        | Request::TaskEvents { .. }
        | Request::TaskList
        | Request::TaskControl { .. }
        | Request::TaskStep { .. }
        | Request::TaskStepDone { .. }
        | Request::TaskConfirm { .. }
        | Request::TaskNote { .. }
        | Request::TaskFinish { .. } => None,
    }
}

fn in_scope(scope: &[String], bundle: &str) -> bool {
    bundle.eq_ignore_ascii_case(SPRINGBOARD) || scope.iter().any(|s| s.eq_ignore_ascii_case(bundle))
}

/// `None` when `target` is within `scope` (empty: no limit); otherwise what to ask the user (action, detail) and
/// the app that approving lets the task into for the rest of it
fn out_of_scope(scope: &[String], target: &Target) -> Option<(String, String, Option<String>)> {
    if scope.is_empty() {
        return None;
    }
    let within = || ictl_l10n::trf!("任务范围：{}", "Task scope: {}", scope.join(", "));
    match target {
        Target::Launch(b) if !in_scope(scope, b) => Some((
            ictl_l10n::trf!("打开任务范围以外的 App：{b}", "Open an app outside the task's scope: {b}"),
            within(),
            Some(b.clone()),
        )),
        Target::Foreground(b) if b.is_empty() => Some((
            ictl_l10n::tr!("在不知道是哪个 App 的界面上操作", "Act on a screen whose app is unknown").into(),
            within(),
            None,
        )),
        Target::Foreground(b) if !in_scope(scope, b) => Some((
            ictl_l10n::trf!("在任务范围以外的 App 里操作：{b}", "Act in an app outside the task's scope: {b}"),
            within(),
            Some(b.clone()),
        )),
        Target::Url(u) => {
            let web = u
                .split_once(':')
                .is_some_and(|(s, _)| s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https"));
            match (web, in_scope(scope, SAFARI)) {
                (true, true) => None,
                (true, false) => Some((
                    ictl_l10n::trf!("在 Safari 里打开链接：{u}", "Open a link in Safari: {u}"),
                    within(),
                    Some(SAFARI.into()),
                )),
                // tel:, sms:, an app's own scheme…: which app it opens, and what it starts there, isn't known
                (false, _) => Some((
                    ictl_l10n::trf!("打开链接：{u}", "Open the link {u}"),
                    ictl_l10n::trf!(
                        "不知道它会打开哪个 App、在那里做什么。{}",
                        "Which app it opens, and what it does there, isn't known. {}",
                        within()
                    ),
                    None,
                )),
            }
        }
        Target::Always { action, detail } => Some((action.clone(), detail.clone(), None)),
        Target::Launch(_) | Target::Foreground(_) => None,
    }
}

/// How the user answered [`Task::ask_user`]
#[derive(Debug, PartialEq)]
enum Asked {
    Approved,
    Rejected,
    /// The task ended meanwhile
    Ended,
    /// The caller went away before the answer
    Gone,
}

fn cancelled() -> Response {
    Response::error(ictl_l10n::tr!(
        "调用方已取消,请求没有执行",
        "The caller cancelled; the request was not carried out"
    ))
}

/// Requests that change something on or about the phone, beyond [`Request::is_device_action`]
fn acts(req: &Request) -> bool {
    req.is_device_action() || matches!(req, Request::Resign { .. } | Request::UnlockReset)
}

impl Task {
    fn lock(&self) -> std::sync::MutexGuard<'_, TaskState> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn info(&self) -> TaskInfo {
        let st = self.lock();
        TaskInfo {
            id: self.id.clone(),
            title: self.title.clone(),
            owner: self.owner.clone(),
            udid: self.udid.clone(),
            state: st.status.name().into(),
            started: self.started,
            ended: st.ended,
            steps: st.next_step - 1,
            apps: st.scope.clone(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.lock().status.active()
    }

    fn emit(&self, st: &mut TaskState, name: &str, data: Value) {
        let e = TaskEvent { event: name.into(), data };
        if let Some(f) = &self.file {
            append(f, &e);
        }
        st.log.push(e.clone());
        let _ = self.tx.send(e);
    }

    fn set_status(&self, st: &mut TaskState, s: Status) {
        if st.status != s && st.status.active() {
            st.status = s;
            if s.active() {
                self.emit(st, "state", json!({"state": s.name()}));
            }
            self.changed.notify_waiters();
        }
    }

    /// The whole task so far and the events after it (taken together, so nothing falls between), and whether it ended
    pub fn subscribe(&self) -> (Vec<TaskEvent>, broadcast::Receiver<TaskEvent>, bool) {
        let st = self.lock();
        (st.log.clone(), self.tx.subscribe(), !st.status.active())
    }

    /// Ends the task unless it already ended. Returns whether it did
    pub fn end(&self, end: End, summary: &str) -> bool {
        let mut st = self.lock();
        if !st.status.active() {
            return false;
        }
        st.status = Status::Ended(end);
        st.ended = Some(now_ms());
        if let Some((_, tx)) = st.confirm.take() {
            let _ = tx.send(false);
        }
        let ok = end == End::Completed;
        let stopped = matches!(end, End::Cancelled | End::Rejected);
        self.emit(&mut st, "done", json!({"ok": ok, "stopped": stopped, "outcome": end.name(), "summary": summary}));
        self.changed.notify_waiters();
        true
    }

    /// The opener's lease went away
    pub fn abandon(&self) {
        self.end(
            End::Abandoned,
            ictl_l10n::tr!(
                "开这个任务的程序断开了，任务中断。已经做完的操作不会撤销。",
                "The program that opened this task went away, so the task was interrupted. What was already done isn't undone."
            ),
        );
    }

    /// Holds until the task runs; `Err` when it ended or the caller went away
    async fn wait_running(&self, gone: &mut Option<oneshot::Receiver<()>>) -> Result<bool, Response> {
        let mut waited = false;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.lock().status {
                Status::Running => return Ok(waited),
                Status::Ended(_) => return Err(refuse(ended_message())),
                _ => {}
            }
            waited = true;
            match gone {
                Some(g) => tokio::select! {
                    () = &mut notified => {}
                    _ = g => return Err(cancelled()),
                },
                None => notified.await,
            }
        }
    }

    fn thought(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let mut st = self.lock();
        match st.last {
            // arrived just after its step reached the daemon
            Some((id, at, true, false)) if at.elapsed() < LATE_THOUGHT => {
                st.last = Some((id, at, true, true));
                self.emit(&mut st, "step", json!({"id": id, "thought": text}));
            }
            _ => st.thought = Some(text.to_owned()),
        }
    }

    fn begin_step(&self, st: &mut TaskState, tool: &str, args: &Value, gesture: Option<Value>, status: &str) -> u64 {
        let id = st.next_step;
        st.next_step += 1;
        let thought = st.thought.take();
        st.last = Some((id, Instant::now(), true, thought.is_some()));
        let mut d = json!({"id": id, "tool": tool, "args": args, "status": status});
        if let Some(t) = thought {
            d["thought"] = t.into();
        }
        if let Some(g) = gesture {
            d["gesture"] = g;
        }
        self.emit(st, "step", d);
        id
    }

    /// A tool call starts: waits while the task isn't running, then records it
    async fn step(&self, tool: &str, args: &Value, gone: &mut Option<oneshot::Receiver<()>>) -> Response {
        if let Err(r) = self.wait_running(gone).await {
            return r;
        }
        let mut st = self.lock();
        if !st.status.active() {
            return refuse(ended_message());
        }
        let id = self.begin_step(&mut st, tool, args, gesture(tool, args, ""), "running");
        if tool == "iphone_tap_element" {
            st.taps_by_text.push(id);
        }
        Response::text(id.to_string())
    }

    /// A step's result
    fn step_done(&self, id: u64, ok: bool, ms: u64, text: &str, gesture_now: Option<Value>) {
        let mut st = self.lock();
        if let Some((last, at, _, th)) = st.last
            && last == id
        {
            st.last = Some((last, at, false, th));
        }
        let mut d = json!({"id": id, "status": if ok { "ok" } else { "error" }, "ms": ms, "result": first_lines(text)});
        // where iphone_tap_element tapped is only known now, from its result
        let by_text = st.taps_by_text.iter().position(|&s| s == id).map(|i| st.taps_by_text.swap_remove(i));
        let g = gesture_now
            .or_else(|| (ok && by_text.is_some()).then(|| gesture("iphone_tap_element", &Value::Null, text)).flatten());
        if let Some(g) = g {
            d["gesture"] = g;
        }
        self.emit(&mut st, "step", d);
    }

    /// Shows a confirmation on the page and holds until the user answers, the task ends or the caller goes away
    async fn ask_user(&self, action: &str, detail: &str, gone: &mut Option<oneshot::Receiver<()>>) -> Asked {
        let (mut rx, id) = {
            let mut st = self.lock();
            if !st.status.active() {
                return Asked::Ended;
            }
            let args = json!({"action": action, "detail": detail});
            let id = self.begin_step(&mut st, "iphone_confirm", &args, None, "waiting");
            self.emit(&mut st, "confirm", json!({"step": id, "action": action, "detail": detail}));
            let (tx, rx) = oneshot::channel();
            st.confirm = Some((id, tx));
            self.set_status(&mut st, Status::Waiting);
            (rx, id)
        };
        let answer = match gone {
            Some(g) => tokio::select! {
                a = &mut rx => Some(a.unwrap_or(false)),
                _ = g => None,
            },
            None => Some(rx.await.unwrap_or(false)),
        };
        let mut st = self.lock();
        st.last = None;
        let ended = !st.status.active();
        let (asked, status, result) = match (answer, ended) {
            (_, true) => (Asked::Ended, "stopped", ictl_l10n::tr!("任务已停止。", "The task was stopped.")),
            (None, false) => {
                // nobody waits for the answer any more: take the question off the page
                if st.confirm.as_ref().is_some_and(|(waiting, _)| *waiting == id) {
                    st.confirm = None;
                }
                (Asked::Gone, "stopped", ictl_l10n::tr!("调用方已取消，没有执行。", "The caller cancelled; not done."))
            }
            (Some(true), false) => {
                (Asked::Approved, "ok", ictl_l10n::tr!("你同意了，继续执行。", "You approved; carrying on."))
            }
            (Some(false), false) => (
                Asked::Rejected,
                "rejected",
                ictl_l10n::tr!("你拒绝了这一步，没有执行。", "You rejected this step; it wasn't done."),
            ),
        };
        self.emit(&mut st, "step", json!({"id": id, "status": status, "result": result}));
        if !ended {
            self.set_status(&mut st, Status::Running);
        }
        asked
    }

    /// The agent asks before a sensitive action ([`crate::protocol::Request::TaskConfirm`])
    async fn confirm(&self, action: &str, detail: &str, gone: &mut Option<oneshot::Receiver<()>>) -> Response {
        let action = action.trim();
        if action.is_empty() {
            return refuse(ictl_l10n::tr!("action 不能为空", "action can't be empty"));
        }
        match self.ask_user(action, detail, gone).await {
            Asked::Approved => Response::text(ictl_l10n::tr!(
                "用户同意了，继续执行这一步。",
                "The user approved; go ahead with this step."
            )),
            Asked::Ended => refuse(ended_message()),
            Asked::Gone => cancelled(),
            Asked::Rejected => {
                self.lock().rejected = true;
                refuse(ictl_l10n::tr!(
                    "用户拒绝了这一步：不要执行，也不要换别的方式去做。停下来，说明没做这一步",
                    "The user rejected this step: don't do it, and don't do it another way. Stop and say it wasn't done"
                ))
            }
        }
    }

    /// Whether the task is limited to some apps
    pub fn scoped(&self) -> bool {
        !self.lock().scope.is_empty()
    }

    /// Before an action of a scoped task: within its scope it goes on; outside it waits for the user. Approving lets
    /// the task into that app for the rest of it; rejecting refuses this action only, so the agent can go back
    /// (unlike a rejected [`Self::confirm`], the task doesn't end as rejected). Returns whether it waited
    pub async fn check_scope(
        &self,
        target: &Target,
        gone: &mut Option<oneshot::Receiver<()>>,
    ) -> Result<bool, Response> {
        let ask = {
            let st = self.lock();
            out_of_scope(&st.scope, target).map(|a| (a, st.scope.join(", ")))
        };
        let Some(((action, detail, adds), scope)) = ask else { return Ok(false) };
        match self.ask_user(&action, &detail, gone).await {
            Asked::Approved => {
                if let Some(a) = adds {
                    let mut st = self.lock();
                    if !in_scope(&st.scope, &a) {
                        st.scope.push(a);
                    }
                }
                Ok(true)
            }
            Asked::Rejected => Err(refuse(&ictl_l10n::trf!(
                "用户不同意超出任务范围（{action}），这一步没有执行。回到任务范围内的 App（{scope}）再继续，做不到就结束任务并说明原因",
                "The user didn't allow going outside the task's scope ({action}); this step wasn't done. Go back to an app within the scope ({scope}) and carry on there, or if you can't, finish and say why"
            ))),
            Asked::Ended => Err(refuse(ended_message())),
            Asked::Gone => Err(cancelled()),
        }
    }

    fn finish(&self, ok: bool, summary: &str) {
        let rejected = self.lock().rejected;
        let summary = summary.trim();
        let end = match (rejected, ok) {
            (true, _) => End::Rejected,
            (false, true) => End::Completed,
            (false, false) => End::Failed,
        };
        let fallback = match end {
            End::Rejected => {
                ictl_l10n::tr!(
                    "你拒绝了一步操作，任务在那里停下了。",
                    "You rejected a step, so the task stopped there."
                )
            }
            End::Completed => {
                ictl_l10n::tr!("任务结束了，agent 没有给出总结。", "The task ended without a summary from the agent.")
            }
            _ => ictl_l10n::tr!("任务没有完成。", "The task didn't finish."),
        };
        self.end(end, if summary.is_empty() { fallback } else { summary });
    }

    fn control(&self, action: &str, step: Option<u64>, approve: bool) -> Result<(), String> {
        let mut st = self.lock();
        if !st.status.active() {
            return Err(ictl_l10n::tr!("任务已经结束", "The task has ended").into());
        }
        match (action, st.status) {
            ("pause", Status::Running) => self.set_status(&mut st, Status::Paused),
            ("resume", Status::Paused) => self.set_status(&mut st, Status::Running),
            ("takeover", Status::Running | Status::Paused) => self.set_status(&mut st, Status::Takeover),
            ("handback", Status::Takeover) => {
                st.recheck = true;
                self.set_status(&mut st, Status::Running);
            }
            ("confirm", Status::Waiting) => match st.confirm.take() {
                Some((waiting, tx)) if step.is_none_or(|s| s == waiting) => {
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
            },
            ("stop", _) => {
                let n = st.next_step - 1;
                drop(st);
                self.end(
                    End::Cancelled,
                    &ictl_l10n::trf!(
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
}

impl Tasks {
    /// Loads the task logs in `dir` (None: nothing on disk, for tests); tasks that hadn't ended are interrupted
    pub fn load(dir: Option<PathBuf>) -> Self {
        let mut list = Vec::new();
        if let Some(dir) = &dir {
            let _ = std::fs::create_dir_all(dir);
            let mut files: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                .filter_map(|p| Some((first_started(&p)?, p)))
                .collect();
            files.sort();
            let extra = files.len().saturating_sub(KEEP);
            for (_, p) in files.drain(..extra) {
                let _ = std::fs::remove_file(p);
            }
            list.extend(files.into_iter().filter_map(|(_, p)| restore(&p)).map(Arc::new));
        }
        Self { dir, list: Mutex::new(list) }
    }

    fn list(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Task>>> {
        self.list.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn active(&self) -> Option<Arc<Task>> {
        self.list().iter().find(|t| t.is_active()).cloned()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.list().iter().find(|t| t.id == id).cloned()
    }

    fn by_token(&self, token: &str) -> Option<Arc<Task>> {
        self.list().iter().find(|t| !t.token.is_empty() && t.token == token).cloned()
    }

    pub fn infos(&self) -> Vec<TaskInfo> {
        self.list().iter().map(|t| t.info()).collect()
    }

    /// Opens a task on `udid` (the controlled device). `Err` is a message
    /// `apps`: bundle IDs it may act in, empty for no limit (see [`Task::check_scope`])
    pub fn open(&self, title: &str, owner: &str, udid: Option<&str>, apps: Vec<String>) -> Result<Arc<Task>, String> {
        let title = title.trim();
        if title.is_empty() {
            return Err(ictl_l10n::tr!("任务要有标题", "A task needs a title").into());
        }
        let Some(udid) = udid.filter(|u| !u.is_empty()) else {
            return Err(ictl_l10n::tr!(
                "现在没有在控制的设备，开不了任务",
                "No device is being controlled, so a task can't be opened"
            )
            .into());
        };
        let mut list = self.list();
        if let Some(t) = list.iter().find(|t| t.is_active()) {
            return Err(ictl_l10n::trf!(
                "已经有一个任务在执行（{}，{}）：等它结束或先停止它",
                "A task is already running ({}, {}): wait for it to end or stop it first",
                t.id,
                t.title
            ));
        }
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
        let task = Arc::new(Task {
            file: self.dir.as_ref().map(|d| d.join(format!("{id}.jsonl"))),
            id,
            token: uuid::Uuid::new_v4().simple().to_string(),
            title: title.chars().take(4000).collect(),
            owner: owner.trim().chars().take(80).collect(),
            udid: udid.to_owned(),
            // after every task before it, even within the same millisecond: logs are ordered by it on the next start
            started: now_ms().max(list.iter().map(|t| t.started + 1).max().unwrap_or(0)),
            st: Mutex::new(TaskState {
                status: Status::Running,
                log: Vec::new(),
                next_step: 1,
                thought: None,
                last: None,
                taps_by_text: Vec::new(),
                recheck: false,
                confirm: None,
                rejected: false,
                ended: None,
                scope: apps,
            }),
            tx: broadcast::channel(4096).0,
            changed: Notify::new(),
        });
        {
            let mut st = task.lock();
            let info = json!({"id": task.id, "title": task.title, "owner": task.owner, "udid": task.udid, "started": task.started, "apps": st.scope});
            task.emit(&mut st, "task", info);
        }
        if list.len() >= KEEP {
            let old = list.remove(0);
            if let Some(f) = &old.file {
                let _ = std::fs::remove_file(f);
            }
        }
        list.push(task.clone());
        Ok(task)
    }

    pub fn token_of(task: &Task) -> &str {
        &task.token
    }

    /// The task with this token, active: for the task's own requests (steps, notes, confirmations)
    fn actor(&self, token: Option<&str>) -> Result<Arc<Task>, Response> {
        let Some(token) = token else {
            return Err(refuse(ictl_l10n::tr!("这个请求要带任务口令", "This request needs a task token")));
        };
        match self.by_token(token) {
            Some(t) if t.is_active() => Ok(t),
            _ => Err(refuse(ended_message())),
        }
    }

    /// Decides whether a request may go on while tasks exist; holds a task's own requests while it isn't running.
    /// `expect_udid` is set to the task's device for its requests. Returns whether it waited (the request's time in
    /// the queue starts again)
    pub async fn admit(
        &self,
        req: &Request,
        token: Option<&str>,
        expect_udid: &mut Option<String>,
        gone: &mut Option<oneshot::Receiver<()>>,
    ) -> Result<(Admission, bool), Response> {
        if let Some(token) = token {
            let task = self.by_token(token).filter(|t| t.is_active()).ok_or_else(|| refuse(ended_message()))?;
            if matches!(req, Request::SelectDevice { .. }) {
                return Err(refuse(ictl_l10n::tr!(
                    "任务进行中不能切换设备，继续操作当前这台",
                    "A task can't switch devices; keep using the current one"
                )));
            }
            let waited = task.wait_running(gone).await?;
            {
                let mut st = task.lock();
                if acts(req) && st.recheck {
                    return Err(refuse(ictl_l10n::tr!(
                        "用户刚才操作过手机，界面可能已经变了：先用 iphone_screenshot 或 iphone_list_elements 重新看一遍，再决定下一步",
                        "The user acted on the phone, so the screen may have changed: look again with iphone_screenshot or iphone_list_elements before the next step"
                    )));
                }
                if observes(req) {
                    st.recheck = false;
                }
            }
            *expect_udid = Some(task.udid.clone());
            return Ok((Admission::Actor(task), waited));
        }
        let Some(task) = self.active() else { return Ok((Admission::Free, false)) };
        if matches!(req, Request::SelectDevice { .. }) {
            return Err(refuse(&ictl_l10n::trf!(
                "任务「{}」进行中，不能切换设备；先停止它",
                "Task “{}” is running, so the device can't be switched; stop it first",
                task.title
            )));
        }
        if !acts(req) || matches!(req, Request::Touch { phase: TouchPhase::Up, .. }) {
            return Ok((Admission::Free, false));
        }
        let mut st = task.lock();
        if st.status == Status::Takeover {
            // the agent looks again before its next action
            st.recheck = true;
            drop(st);
            return Ok((Admission::Supervisor(task), false));
        }
        Err(refuse(&ictl_l10n::trf!(
            "AI 任务「{}」正在操作这台设备（{}）。要自己操作，先接管或停止这个任务",
            "AI task “{}” is operating this device ({}). To act yourself, take it over or stop it first",
            task.title,
            task.id
        )))
    }

    /// An action of the task that no announced step covers: it becomes a step of its own. Returns the step
    pub fn auto_step(&self, task: &Task, req: &Request) -> u64 {
        let mut st = task.lock();
        let mut args = serde_json::to_value(req).unwrap_or_default();
        let op = args.as_object_mut().and_then(|o| o.remove("op")).and_then(|v| v.as_str().map(str::to_owned));
        task.begin_step(&mut st, &op.unwrap_or_default(), &args, request_gesture(req), "running")
    }

    pub fn auto_step_done(&self, task: &Task, step: u64, resp: &Response, ms: u64) {
        let (ok, text) = match resp {
            Response::Text { text, .. } => (true, text.as_str()),
            Response::Image { caption, .. } => (true, caption.as_str()),
            Response::Error { message, .. } => (false, message.as_str()),
        };
        task.step_done(step, ok, ms, text, None);
    }

    /// The task requests that aren't about the device; `None` for any other request
    pub async fn handle(
        &self,
        req: &Request,
        token: Option<&str>,
        gone: &mut Option<oneshot::Receiver<()>>,
    ) -> Option<Response> {
        Some(match req {
            Request::TaskList => Response::text(serde_json::to_string(&self.infos()).unwrap_or_default()),
            Request::TaskControl { id, action, step, approve } => match self.get(id) {
                Some(t) => match t.control(action, *step, *approve) {
                    Ok(()) => Response::text("ok"),
                    Err(m) => Response::error(m),
                },
                None => Response::error(ictl_l10n::tr!("没有这个任务", "No such task")),
            },
            Request::TaskStep { tool, args } => match self.actor(token) {
                Ok(t) => t.step(tool, args, gone).await,
                Err(r) => r,
            },
            Request::TaskStepDone { step, ok, ms, text } => match self.actor(token) {
                Ok(t) => {
                    t.step_done(*step, *ok, *ms, text, None);
                    Response::text("ok")
                }
                Err(r) => r,
            },
            Request::TaskConfirm { action, detail } => match self.actor(token) {
                Ok(t) => t.confirm(action, detail, gone).await,
                Err(r) => r,
            },
            Request::TaskNote { text } => match self.actor(token) {
                Ok(t) => {
                    t.thought(text);
                    Response::text("ok")
                }
                Err(r) => r,
            },
            Request::TaskFinish { ok, summary } => match self.actor(token) {
                Ok(t) => {
                    t.finish(*ok, summary);
                    Response::text("ok")
                }
                Err(r) => r,
            },
            _ => return None,
        })
    }

    /// The daemon now controls `udid`: a task on another device can't go on
    pub fn device_is(&self, udid: Option<&str>) {
        if let (Some(t), Some(u)) = (self.active(), udid)
            && !t.udid.eq_ignore_ascii_case(u)
        {
            t.end(
                End::Interrupted,
                &ictl_l10n::log_trf!(
                    "正在控制的设备变成了 {u}，任务中断（它是在 {} 上开的）",
                    "The controlled device changed to {u}, so the task was interrupted (it was opened on {})",
                    t.udid
                ),
            );
        }
    }
}

fn append(file: &Path, e: &TaskEvent) {
    use std::io::Write;
    let line = json!({"t": now_ms(), "event": e.event, "data": e.data});
    let r = std::fs::OpenOptions::new().create(true).append(true).open(file).and_then(|mut f| writeln!(f, "{line}"));
    if let Err(err) = r {
        tracing::warn!("{}", ictl_l10n::log_trf!("写任务日志失败：{err}", "Writing the task log failed: {err}"));
    }
}

fn read_events(file: &Path) -> Vec<TaskEvent> {
    let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
    text.lines().filter_map(|l| serde_json::from_str::<TaskEvent>(l).ok()).collect()
}

fn first_started(file: &Path) -> Option<u64> {
    use std::io::BufRead;
    let f = std::fs::File::open(file).ok()?;
    let line = std::io::BufReader::new(f).lines().next()?.ok()?;
    serde_json::from_str::<Value>(&line).ok()?["data"]["started"].as_u64()
}

/// A task from its log; one that hadn't ended is interrupted now
fn restore(file: &Path) -> Option<Task> {
    let log = read_events(file);
    let first = log.first().filter(|e| e.event == "task")?.data.clone();
    let s = |k: &str| first[k].as_str().unwrap_or_default().to_owned();
    let steps = log.iter().filter_map(|e| (e.event == "step").then(|| e.data["id"].as_u64()).flatten()).max();
    let done = log.iter().rev().find(|e| e.event == "done").cloned();
    let status = match done.as_ref().and_then(|d| d.data["outcome"].as_str()) {
        Some("completed") => End::Completed,
        Some("cancelled") => End::Cancelled,
        Some("rejected") => End::Rejected,
        Some("abandoned") => End::Abandoned,
        Some("interrupted") => End::Interrupted,
        _ if done.is_some() => End::Failed,
        _ => End::Interrupted,
    };
    let task = Task {
        id: s("id"),
        // nobody holds a token for a task from before this daemon started
        token: String::new(),
        title: s("title"),
        owner: s("owner"),
        udid: s("udid"),
        started: first["started"].as_u64().unwrap_or(0),
        file: Some(file.to_owned()),
        st: Mutex::new(TaskState {
            status: Status::Running,
            log,
            next_step: steps.unwrap_or(0) + 1,
            thought: None,
            last: None,
            taps_by_text: Vec::new(),
            recheck: false,
            confirm: None,
            rejected: false,
            ended: None,
            scope: first["apps"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                .unwrap_or_default(),
        }),
        tx: broadcast::channel(16).0,
        changed: Notify::new(),
    };
    if task.id.is_empty() {
        return None;
    }
    if done.is_none() {
        task.end(
            End::Interrupted,
            ictl_l10n::log_tr!(
                "守护进程重启了，任务中断。已经做完的操作不会撤销。",
                "The daemon restarted, so the task was interrupted. What was already done isn't undone."
            ),
        );
    } else {
        let mut st = task.lock();
        st.status = Status::Ended(status);
        st.ended = file
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as u64));
    }
    Some(task)
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

/// [`gesture`] for a request that no tool call announced
fn request_gesture(req: &Request) -> Option<Value> {
    match req {
        Request::Tap { x, y } => Some(json!({"kind": "tap", "x": x, "y": y})),
        Request::LongPress { x, y, .. } => Some(json!({"kind": "long_press", "x": x, "y": y})),
        Request::Swipe { from, to, .. } => {
            Some(json!({"kind": "swipe", "x": from.0, "y": from.1, "x2": to.0, "y2": to.1}))
        }
        _ => None,
    }
}

fn first_lines(text: &str) -> String {
    let mut out: Vec<&str> = text.lines().take(RESULT_LINES).collect();
    if text.lines().count() > RESULT_LINES {
        out.push("…");
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHONE: &str = "00008150-000A";

    fn tasks() -> Tasks {
        Tasks::load(None)
    }

    fn open(ts: &Tasks) -> Arc<Task> {
        ts.open("看看设置", "test", Some(PHONE), Vec::new()).unwrap()
    }

    fn tap() -> Request {
        Request::Tap { x: 1.0, y: 2.0 }
    }

    async fn admit(ts: &Tasks, req: &Request, token: Option<&str>) -> Result<(Admission, bool), Response> {
        ts.admit(req, token, &mut None, &mut None).await
    }

    fn message(r: Result<(Admission, bool), Response>) -> String {
        match r {
            Err(Response::Error { message, .. }) => message,
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("admitted"),
        }
    }

    #[tokio::test]
    async fn without_a_task_everything_goes_as_before() {
        let ts = tasks();
        assert!(matches!(admit(&ts, &tap(), None).await, Ok((Admission::Free, false))));
        let select = Request::SelectDevice { device: "auto".into() };
        assert!(matches!(admit(&ts, &select, None).await, Ok((Admission::Free, _))));
    }

    #[tokio::test]
    async fn only_one_task_and_only_on_a_device() {
        let ts = tasks();
        assert!(ts.open("x", "", None, Vec::new()).is_err());
        assert!(ts.open(" ", "", Some(PHONE), Vec::new()).is_err());
        let t = open(&ts);
        assert!(ts.open("another", "", Some(PHONE), Vec::new()).err().unwrap().contains(&t.id));
        t.end(End::Completed, "");
        assert!(ts.open("another", "", Some(PHONE), Vec::new()).is_ok());
    }

    /// The point of D17: while a task runs, a caller without its token can look but not act
    #[tokio::test]
    async fn others_may_look_but_not_act_until_the_user_takes_over() {
        let ts = tasks();
        let t = open(&ts);
        assert!(message(admit(&ts, &tap(), None).await).contains(&t.id));
        assert!(admit(&ts, &Request::Resign { force: true }, None).await.is_err());
        let shot = Request::Screenshot { region: None };
        assert!(matches!(admit(&ts, &shot, None).await, Ok((Admission::Free, _))));
        let up = Request::Touch { phase: TouchPhase::Up, x: 0.0, y: 0.0 };
        assert!(admit(&ts, &up, None).await.is_ok(), "a finger that was down can always be lifted");
        let select = Request::SelectDevice { device: "auto".into() };
        assert!(admit(&ts, &select, None).await.is_err());

        t.control("takeover", None, false).unwrap();
        assert!(matches!(admit(&ts, &tap(), None).await, Ok((Admission::Supervisor(_), _))));
        assert!(admit(&ts, &select, None).await.is_err(), "not even while taken over");
    }

    #[tokio::test]
    async fn the_task_goes_to_its_own_device() {
        let ts = tasks();
        let t = open(&ts);
        let mut udid = Some("someone-else".to_owned());
        let r = ts.admit(&tap(), Some(&t.token), &mut udid, &mut None).await;
        assert!(matches!(r, Ok((Admission::Actor(_), false))));
        assert_eq!(udid.as_deref(), Some(PHONE));
        let select = Request::SelectDevice { device: "auto".into() };
        assert!(admit(&ts, &select, Some(&t.token)).await.is_err());
    }

    #[tokio::test]
    async fn a_paused_task_holds_its_requests_until_resumed() {
        let ts = Arc::new(tasks());
        let t = open(&ts);
        t.control("pause", None, false).unwrap();
        let (ts2, token) = (ts.clone(), t.token.clone());
        let held = tokio::spawn(async move { admit(&ts2, &tap(), Some(&token)).await.map(|(_, w)| w) });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!held.is_finished());
        t.control("resume", None, false).unwrap();
        assert!(matches!(held.await.unwrap(), Ok(true)), "let through, and it says it waited");
    }

    #[tokio::test]
    async fn a_held_request_whose_caller_left_is_dropped() {
        let ts = tasks();
        let t = open(&ts);
        t.control("pause", None, false).unwrap();
        let (tx, rx) = oneshot::channel();
        tx.send(()).unwrap();
        let r = ts.admit(&tap(), Some(&t.token), &mut None, &mut Some(rx)).await;
        assert!(message(r).contains("取消"));
    }

    #[tokio::test]
    async fn after_someone_else_acted_the_agent_looks_before_acting() {
        let ts = tasks();
        let t = open(&ts);
        t.control("takeover", None, false).unwrap();
        assert!(admit(&ts, &tap(), None).await.is_ok());
        t.control("handback", None, false).unwrap();
        assert!(message(admit(&ts, &tap(), Some(&t.token)).await).contains("重新看"));
        assert!(admit(&ts, &tap(), Some(&t.token)).await.is_err(), "until it has looked, not just once");
        assert!(admit(&ts, &Request::Status, Some(&t.token)).await.is_ok(), "status isn't acting");
        let look = Request::ListElements { filter: String::new(), limit: 10, max_age_ms: 0 };
        assert!(admit(&ts, &look, Some(&t.token)).await.is_ok());
        assert!(admit(&ts, &tap(), Some(&t.token)).await.is_ok());
    }

    #[tokio::test]
    async fn an_ended_or_unknown_token_is_refused_for_everything() {
        let ts = Arc::new(tasks());
        let t = open(&ts);
        t.control("pause", None, false).unwrap();
        let (ts2, token) = (ts.clone(), t.token.clone());
        let held = tokio::spawn(async move { admit(&ts2, &Request::Status, Some(&token)).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(20)).await;
        t.control("stop", None, false).unwrap();
        assert!(held.await.unwrap().is_err(), "a held request is released with a refusal");
        assert!(message(admit(&ts, &Request::Status, Some(&t.token)).await).contains("结束"));
        assert!(message(admit(&ts, &Request::Status, Some("nobody")).await).contains("结束"));
        assert!(
            ts.handle(&Request::TaskNote { text: "x".into() }, Some(&t.token), &mut None)
                .await
                .is_some_and(|r| matches!(r, Response::Error { .. }))
        );
        assert!(matches!(admit(&ts, &tap(), None).await, Ok((Admission::Free, _))), "the phone is free again");
    }

    #[tokio::test]
    async fn a_rejected_confirmation_ends_the_task_as_rejected() {
        let ts = Arc::new(tasks());
        let t = open(&ts);
        let (ts2, token) = (ts.clone(), t.token.clone());
        let waiting = tokio::spawn(async move {
            let req = Request::TaskConfirm { action: "发送消息".into(), detail: "给妈妈".into() };
            ts2.handle(&req, Some(&token), &mut None).await.unwrap()
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(t.info().state, "waiting");
        assert!(t.control("confirm", Some(99), false).is_err(), "another step");
        t.control("confirm", Some(1), false).unwrap();
        assert!(matches!(waiting.await.unwrap(), Response::Error { message, .. } if message.contains("拒绝")));
        assert_eq!(t.info().state, "running");
        t.finish(true, "done");
        assert_eq!(t.info().state, "rejected");
    }

    #[tokio::test]
    async fn steps_get_thoughts_results_and_marks() {
        let ts = tasks();
        let t = open(&ts);
        let req = |tool: &str, args: Value| Request::TaskStep { tool: tool.into(), args };
        t.thought("先点「通用」");
        let r = ts.handle(&req("iphone_tap_element", json!({"text": "通用"})), Some(&t.token), &mut None).await;
        assert_eq!(r, Some(Response::text("1")));
        t.step_done(1, true, 120, "已点击「通用」 @(200,300) 100x40", None);
        // a thought arriving just after its step started joins it
        let r = ts.handle(&req("iphone_tap", json!({"x": 5, "y": 6})), Some(&t.token), &mut None).await;
        assert_eq!(r, Some(Response::text("2")));
        t.thought("点一下");
        let n = ts.auto_step(&t, &Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.3 });
        assert_eq!(n, 3);
        let (log, _, _) = t.subscribe();
        let steps: Vec<&Value> = log.iter().filter(|e| e.event == "step").map(|e| &e.data).collect();
        assert_eq!(steps[0]["thought"], "先点「通用」");
        assert_eq!(steps[1]["gesture"], json!({"kind": "tap", "x": 200.0, "y": 300.0}));
        assert_eq!(steps[2]["gesture"]["kind"], "tap");
        assert_eq!(steps[3]["thought"], "点一下");
        assert_eq!(steps[4]["tool"], "swipe");
        assert_eq!(steps[4]["gesture"]["x2"], 3.0);
    }

    #[tokio::test]
    async fn another_device_interrupts_the_task() {
        let ts = tasks();
        let t = open(&ts);
        ts.device_is(None);
        ts.device_is(Some(&PHONE.to_lowercase()));
        assert!(t.is_active(), "unplugged, or the same phone: the task waits");
        ts.device_is(Some("00008150-000B"));
        assert_eq!(t.info().state, "interrupted");
    }

    #[test]
    fn logs_survive_a_restart_and_unfinished_tasks_are_interrupted() {
        let dir = std::env::temp_dir().join(format!("ictl-tasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ts = Tasks::load(Some(dir.clone()));
        let done = open(&ts);
        done.thought("看一下");
        done.step_done(1, true, 5, "ok", None);
        done.end(End::Completed, "好了");
        let running = open(&ts);
        drop(ts);

        let ts = Tasks::load(Some(dir.clone()));
        let infos = ts.infos();
        assert_eq!(infos.len(), 2);
        assert_eq!((infos[0].id.as_str(), infos[0].state.as_str()), (done.id.as_str(), "completed"));
        assert_eq!((infos[1].id.as_str(), infos[1].state.as_str()), (running.id.as_str(), "interrupted"));
        assert!(ts.active().is_none());
        assert!(ts.by_token(&running.token).is_none(), "the old token means nothing now");
        let (log, _, ended) = ts.get(&running.id).unwrap().subscribe();
        assert!(ended && log.last().unwrap().data["outcome"] == "interrupted");
        // and it stays that way on the next start
        let ts = Tasks::load(Some(dir.clone()));
        let (log, _, _) = ts.get(&running.id).unwrap().subscribe();
        assert_eq!(log.iter().filter(|e| e.event == "done").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Tasks opened within the same millisecond (a fast machine, a test) still come back in the order they were opened
    #[test]
    fn tasks_keep_their_order_across_a_restart_even_within_a_millisecond() {
        let dir = std::env::temp_dir().join(format!("ictl-tasks-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ts = Tasks::load(Some(dir.clone()));
        let ids: Vec<String> = (0..8)
            .map(|_| {
                let t = open(&ts);
                t.end(End::Completed, "");
                t.id.clone()
            })
            .collect();
        let started: Vec<u64> = ts.infos().iter().map(|t| t.started).collect();
        assert!(started.windows(2).all(|w| w[0] < w[1]), "{started:?}");
        drop(ts);
        let again: Vec<String> = Tasks::load(Some(dir.clone())).infos().into_iter().map(|t| t.id).collect();
        assert_eq!(again, ids);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn scope(apps: &[&str]) -> Vec<String> {
        apps.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn what_each_request_checks_against_the_scope() {
        assert_eq!(scope_check(&tap()), Some(Check::Foreground));
        assert_eq!(scope_check(&Request::LaunchApp { app: "设置".into() }), Some(Check::Launch("设置".into())));
        assert_eq!(scope_check(&Request::OpenUrl { url: "tel:10086".into() }), Some(Check::Url("tel:10086".into())));
        assert!(matches!(scope_check(&Request::UninstallApp { app: "x".into() }), Some(Check::Always { .. })));
        // the ways back after a refusal, and looking, are never held
        assert_eq!(scope_check(&Request::PressButton { button: "home".into() }), None);
        assert_eq!(scope_check(&Request::TerminateApp { app: "x".into() }), None);
        assert_eq!(scope_check(&Request::Screenshot { region: None }), None);
        assert_eq!(scope_check(&Request::Touch { phase: TouchPhase::Up, x: 0.0, y: 0.0 }), None);
    }

    #[test]
    fn what_is_outside_the_scope() {
        let prefs = scope(&["com.apple.Preferences"]);
        let fg = |b: &str| Target::Foreground(b.into());
        assert_eq!(out_of_scope(&[], &fg("com.tencent.xin")), None, "no scope: no limit");
        assert_eq!(out_of_scope(&prefs, &fg("com.apple.preferences")), None, "bundle IDs ignore case");
        assert_eq!(out_of_scope(&prefs, &fg(SPRINGBOARD)), None, "the home screen is always fine");
        let (_, _, adds) = out_of_scope(&prefs, &fg("com.tencent.xin")).unwrap();
        assert_eq!(adds.as_deref(), Some("com.tencent.xin"));
        let (_, _, adds) = out_of_scope(&prefs, &fg("")).unwrap();
        assert_eq!(adds, None, "an unknown app is asked about, never let in");
        assert_eq!(out_of_scope(&prefs, &Target::Launch("com.apple.Preferences".into())), None);
        assert!(out_of_scope(&prefs, &Target::Launch("com.apple.MobileSMS".into())).is_some());
        // links: http(s) is Safari; other schemes open who knows what
        let web = Target::Url("https://example.com".into());
        assert_eq!(out_of_scope(&prefs, &web).unwrap().2.as_deref(), Some(SAFARI));
        assert_eq!(out_of_scope(&scope(&["com.apple.Preferences", SAFARI]), &web), None);
        let tel = Target::Url("tel:10086".into());
        assert_eq!(out_of_scope(&scope(&[SAFARI]), &tel).unwrap().2, None);
        let always = Target::Always { action: "卸载".into(), detail: "x".into() };
        assert!(out_of_scope(&prefs, &always).is_some());
    }

    async fn answer_soon(t: &Arc<Task>, approve: bool) {
        let t = t.clone();
        tokio::spawn(async move {
            while t.info().state != "waiting" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            t.control("confirm", None, approve).unwrap();
        });
    }

    #[tokio::test]
    async fn an_approved_app_joins_the_scope_for_the_rest_of_the_task() {
        let ts = tasks();
        let t = ts.open("改设置", "test", Some(PHONE), scope(&["com.apple.Preferences"])).unwrap();
        assert!(t.scoped());
        let wechat = Target::Foreground("com.tencent.xin".into());
        answer_soon(&t, true).await;
        assert_eq!(t.check_scope(&wechat, &mut None).await.ok(), Some(true), "asked, then let through");
        assert_eq!(t.check_scope(&wechat, &mut None).await.ok(), Some(false), "not asked again");
        assert_eq!(t.info().apps, scope(&["com.apple.Preferences", "com.tencent.xin"]));
        assert_eq!(t.info().state, "running");
    }

    #[tokio::test]
    async fn a_rejected_app_refuses_that_step_but_the_task_can_go_back() {
        let ts = tasks();
        let t = ts.open("改设置", "test", Some(PHONE), scope(&["com.apple.Preferences"])).unwrap();
        answer_soon(&t, false).await;
        let r = t.check_scope(&Target::Launch("com.apple.MobileSMS".into()), &mut None).await;
        assert!(matches!(r, Err(Response::Error { message, .. }) if message.contains("com.apple.Preferences")));
        assert_eq!(t.info().state, "running");
        assert_eq!(t.info().apps, scope(&["com.apple.Preferences"]));
        t.finish(true, "改好了");
        assert_eq!(t.info().state, "completed", "a scope refusal isn't a rejected task");
    }

    #[tokio::test]
    async fn a_question_nobody_waits_for_leaves_the_page() {
        let ts = tasks();
        let t = ts.open("改设置", "test", Some(PHONE), scope(&["com.apple.Preferences"])).unwrap();
        let (tx, rx) = oneshot::channel();
        let t2 = t.clone();
        let asking =
            tokio::spawn(
                async move { t2.check_scope(&Target::Foreground("com.tencent.xin".into()), &mut Some(rx)).await },
            );
        while t.info().state != "waiting" {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tx.send(()).unwrap();
        assert!(matches!(asking.await.unwrap(), Err(Response::Error { message, .. }) if message.contains("取消")));
        assert_eq!(t.info().state, "running");
        assert!(t.control("confirm", None, true).is_err(), "nothing waits for an answer any more");
    }

    #[test]
    fn the_scope_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("ictl-tasks-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ts = Tasks::load(Some(dir.clone()));
        let t = ts.open("改设置", "test", Some(PHONE), scope(&["com.apple.Preferences"])).unwrap();
        t.end(End::Completed, "");
        let ts = Tasks::load(Some(dir.clone()));
        assert_eq!(ts.get(&t.id).unwrap().info().apps, scope(&["com.apple.Preferences"]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_the_newest_logs_are_kept() {
        let dir = std::env::temp_dir().join(format!("ictl-tasks-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ts = Tasks::load(Some(dir.clone()));
        for _ in 0..KEEP + 3 {
            open(&ts).end(End::Completed, "");
        }
        assert_eq!(ts.infos().len(), KEEP);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), KEEP);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
