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
//! Controlling a task (pause, resume, take over, hand back, answer a confirmation) is the user's
//! ([`Tasks::handle`]): refused with a task's token, and from the processes the opener starts as its agent
//! ([`crate::procs`]). Stopping is always allowed: an agent may give up. Who controlled it goes into the log.
//!
//! Steps: a tool call announces itself (`Task::step`); the requests it makes carry the step number. An action
//! with the token but no step (a client that doesn't announce) becomes a step of its own, so nothing the task does
//! is missing from it. Every event goes to `tasks/<id>.jsonl` too; tasks that hadn't ended when the daemon stopped
//! are marked interrupted when it starts again.

mod scope;
mod store;
mod task;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::sync::{Notify, broadcast, oneshot};

use crate::protocol::{Request, Response, TaskEvent, TaskInfo, TouchPhase};

pub use scope::{Check, Target, scope_check};
use store::{MAX_LOG_BYTES, MAX_LOG_EVENTS, drop_unreadable, first_started, restore};
use task::request_gesture;

/// Task logs kept on disk, and the tasks listed (their events are read from the logs, see `TaskState::on_disk`)
const KEEP: usize = 50;

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
    /// The events so far (each paired with its monotonic sequence number), kept here only while they aren't all in the
    /// task's file ([`Self::on_disk`]). Both disk and in-memory logs are bounded by `max_events` / `max_bytes`, with
    /// `task` and `done` always allowed through.
    log: Vec<(u64, TaskEvent)>,
    /// Every stored event is in [`Task::file`]: read from there asynchronously without holding this mutex. Off without
    /// a file, and from the first write to it that fails (the events so far are read into [`Self::log`] then).
    on_disk: bool,
    /// Highest monotonic sequence number emitted for this task (1-based; 0 before the first event).
    next_seq: u64,
    /// Number of events persisted to [`Task::file`] (or held in [`Self::log`] when `!on_disk`).
    stored_events: usize,
    /// Exact byte length written to [`Task::file`] (or cumulative serialized bytes in [`Self::log`] when `!on_disk`).
    stored_bytes: u64,
    /// Whether this task has already logged a one-time warning after hitting the log cap.
    capped: bool,
    max_events: usize,
    max_bytes: u64,
    next_step: u64,
    /// What the agent said it is about to do, for the next step
    thought: Option<String>,
    /// The latest step: id, when it started, still running, has a thought
    last: Option<(u64, Instant, bool, bool)>,
    /// Steps started and not yet finished, oldest first: a tool call and the question it asks are both under way,
    /// and so are calls an agent makes at once. The task's end stops them all
    in_flight: Vec<u64>,
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
    /// Steps whose action the user kept out of the task's scope: shown as rejected, not as failed
    declined: Vec<u64>,
    /// The processes the opener starts are the agent ([`Request::TaskAgent`]): they may not control the task
    agent_below_opener: bool,
}

/// Consistent snapshot of a task's stored history paired with its live event receiver, captured without holding the
/// task mutex while reading the on-disk log.
pub struct Subscription {
    /// Stored events up to [`Self::snap_seq`], in order, each with its monotonic sequence number.
    pub events: Vec<(u64, TaskEvent)>,
    /// Highest sequence number emitted when this subscription was taken (including any ordinary events skipped after
    /// the log cap was hit); live items with `seq <= snap_seq` are already covered.
    pub snap_seq: u64,
    pub rx: broadcast::Receiver<(u64, TaskEvent)>,
    pub ended: bool,
}

pub struct Task {
    pub id: String,
    token: String,
    title: String,
    owner: String,
    udid: String,
    started: u64,
    /// The process that opened it (holds the lease); only it may name the agent
    opener: Option<u32>,
    file: Option<PathBuf>,
    st: Mutex<TaskState>,
    tx: broadcast::Sender<(u64, TaskEvent)>,
    /// Wakes requests held for the task when its status changes
    changed: Notify,
}

/// [`Tasks::still`]: whether an admitted request may go on once it holds its lock
pub enum Still {
    Admitted,
    /// The task's request waits for it to run again (then is checked again)
    Wait(Arc<Task>),
    Refused(Response),
}

fn look_first() -> Response {
    refuse(ictl_l10n::tr!(
        "用户刚才操作过手机，界面可能已经变了：先用 iphone_screenshot 或 iphone_list_elements 重新看一遍，再决定下一步",
        "The user acted on the phone, so the screen may have changed: look again with iphone_screenshot or iphone_list_elements before the next step"
    ))
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

pub(crate) fn refuse(msg: &str) -> Response {
    Response::error(msg)
}

fn ended_message() -> &'static str {
    ictl_l10n::tr!(
        "这个任务已经结束（被停止、中断或不存在）：不要再调用任何工具，直接结束",
        "This task has ended (stopped, interrupted or unknown): don't call any more tools, just finish"
    )
}

/// Requests that show the agent the screen: after one, it may act again ([`TaskState::recheck`])
pub(crate) fn observes(req: &Request) -> bool {
    matches!(
        req,
        Request::Screenshot { .. }
            | Request::ListElements { .. }
            | Request::WaitFor { .. }
            | Request::WaitForDisappearance { .. }
    )
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
    req.is_device_action()
        || matches!(
            req,
            Request::Resign { .. }
                | Request::UnlockReset
                | Request::UnlockResetDevice { .. }
                | Request::SetupStep { .. }
                | Request::WdaForward { .. }
                | Request::PinCurrent { .. }
        )
}

impl Tasks {
    /// Loads the task logs in `dir` (None: nothing on disk, for tests); tasks that hadn't ended are interrupted
    pub fn load(dir: Option<PathBuf>) -> Self {
        let mut list = Vec::new();
        if let Some(dir) = &dir {
            let _ = std::fs::create_dir_all(dir);
            let mut files: Vec<(u64, PathBuf)> = Vec::new();
            for p in std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()) {
                if !p.extension().is_some_and(|x| x == "jsonl") {
                    continue;
                }
                match first_started(&p) {
                    Some(t) => files.push((t, p)),
                    // its first line was cut short (a power cut while it was written): no task to show, and left
                    // alone it would never count towards the limit, so never go
                    None => drop_unreadable(&p),
                }
            }
            files.sort();
            let extra = files.len().saturating_sub(KEEP);
            for (_, p) in files.drain(..extra) {
                let _ = std::fs::remove_file(p);
            }
            for (_, p) in files {
                match restore(&p) {
                    Some(t) => list.push(Arc::new(t)),
                    None => drop_unreadable(&p),
                }
            }
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
    /// `opener`: the process opening it, which may then name its agent ([`Request::TaskAgent`])
    pub fn open(
        &self,
        title: &str,
        owner: &str,
        udid: Option<&str>,
        apps: Vec<String>,
        opener: Option<u32>,
    ) -> crate::error::Result<Arc<Task>> {
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
            )
            .into());
        }
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
        let file = self.dir.as_ref().map(|d| d.join(format!("{id}.jsonl")));
        let on_disk = file.is_some();
        let task = Arc::new(Task {
            file,
            id,
            token: uuid::Uuid::new_v4().simple().to_string(),
            title: title.chars().take(4000).collect(),
            owner: owner.trim().chars().take(80).collect(),
            udid: udid.to_owned(),
            opener,
            // after every task before it, even within the same millisecond: logs are ordered by it on the next start
            started: now_ms().max(list.iter().map(|t| t.started + 1).max().unwrap_or(0)),
            st: Mutex::new(TaskState {
                status: Status::Running,
                log: Vec::new(),
                on_disk,
                next_seq: 0,
                stored_events: 0,
                stored_bytes: 0,
                capped: false,
                max_events: MAX_LOG_EVENTS,
                max_bytes: MAX_LOG_BYTES,
                next_step: 1,
                thought: None,
                last: None,
                in_flight: Vec::new(),
                taps_by_text: Vec::new(),
                recheck: false,
                confirm: None,
                rejected: false,
                ended: None,
                scope: apps,
                declined: Vec::new(),
                agent_below_opener: false,
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
            if acts(req) && task.lock().recheck {
                return Err(look_first());
            }
            *expect_udid = Some(task.udid.clone());
            return Ok((Admission::Actor(task), waited));
        }
        self.admit_other(req).map(|a| (a, false))
    }

    /// [`Self::admit`] again once the request holds its lock (it may have queued for it a while): the task may have
    /// been paused or taken over, or handed back, meanwhile. A request of the task waits for it to run again
    pub fn still(&self, admission: &Admission, req: &Request) -> Still {
        match admission {
            Admission::Actor(task) => {
                let st = task.lock();
                match st.status {
                    Status::Running if acts(req) && st.recheck => Still::Refused(look_first()),
                    Status::Running => Still::Admitted,
                    Status::Ended(_) => Still::Refused(refuse(ended_message())),
                    _ => Still::Wait(task.clone()),
                }
            }
            Admission::Free | Admission::Supervisor(_) => match self.admit_other(req) {
                Ok(_) => Still::Admitted,
                Err(r) => Still::Refused(r),
            },
        }
    }

    /// Why the device can't be switched now: a task is active (#106)
    pub fn switch_refused(&self) -> Option<String> {
        self.active().map(|task| {
            ictl_l10n::trf!(
                "任务「{}」进行中，不能切换设备；先停止它",
                "Task “{}” is running, so the device can't be switched; stop it first",
                task.title
            )
        })
    }

    /// [`Self::admit`] for a request without a task's token
    fn admit_other(&self, req: &Request) -> Result<Admission, Response> {
        if matches!(req, Request::SelectDevice { .. })
            && let Some(why) = self.switch_refused()
        {
            return Err(refuse(&why));
        }
        let Some(task) = self.active() else { return Ok(Admission::Free) };
        if !acts(req) || matches!(req, Request::Touch { phase: TouchPhase::Up, .. }) {
            return Ok(Admission::Free);
        }
        let mut st = task.lock();
        if st.status == Status::Takeover {
            // the agent looks again before its next action
            st.recheck = true;
            drop(st);
            return Ok(Admission::Supervisor(task));
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
    /// `peer`: the process that sent it
    pub async fn handle(
        &self,
        req: &Request,
        token: Option<&str>,
        peer: Option<u32>,
        gone: &mut Option<oneshot::Receiver<()>>,
    ) -> Option<Response> {
        Some(match req {
            Request::TaskList => Response::text(serde_json::to_string(&self.infos()).unwrap_or_default()),
            Request::TaskControl { id, action, step, approve, by, for_pid } => match self.get(id) {
                Some(t) => {
                    if action != "stop" && from_agent(&t, token, [peer, *for_pid]) {
                        return Some(refuse(ictl_l10n::tr!(
                            "AI 任务不能自己暂停、继续、接管、交还或确认：这些要由人在控制台或命令行（另开一个终端）里操作。要放弃任务，可以停止它",
                            "An AI task can't pause, resume, take over, hand back or confirm itself: the user does that on the control page or from the command line (in a terminal of their own). To give up, stop the task"
                        )));
                    }
                    match t.control(action, *step, *approve, by.as_deref()) {
                        Ok(()) => Response::text("ok"),
                        Err(m) => Response::error(m),
                    }
                }
                None => Response::error(ictl_l10n::tr!("没有这个任务", "No such task")),
            },
            Request::TaskAgent => match self.actor(token) {
                Ok(t) if peer.is_some() && peer == t.opener => {
                    t.lock().agent_below_opener = true;
                    Response::text("ok")
                }
                Ok(_) => refuse(ictl_l10n::tr!(
                    "只有开任务的进程能说明 Agent 是谁",
                    "Only the process that opened the task can say which processes are its agent"
                )),
                Err(r) => r,
            },
            Request::TaskCheck => match self.actor(token) {
                Ok(t) => Response::text(t.id.clone()),
                Err(r) => r,
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

/// A control request that comes from inside the task: it carries a task's token, or one of `pids` (the
/// connection's process, the one the request was passed on for) was started by the opener after it said its children
/// are the agent ([`Request::TaskAgent`]). The opener itself is the user's side (the page, `task run`)
fn from_agent(task: &Task, token: Option<&str>, pids: [Option<u32>; 2]) -> bool {
    if token.is_some() {
        return true;
    }
    let Some(opener) = task.opener.filter(|_| task.lock().agent_below_opener) else { return false };
    pids.into_iter().flatten().any(|p| p != opener && crate::procs::descends_from(p, opener))
}
