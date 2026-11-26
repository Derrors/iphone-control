//! Single-task lifecycle: event emission, steps, thoughts, user confirmations, and control actions.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::protocol::{Request, Response, TaskEvent, TaskInfo};

use super::store::{append_line, encode_line, read_events_prefix};
use super::{Asked, End, Status, Subscription, Task, TaskState, cancelled, ended_message, now_ms, refuse};

/// A thought sent this soon after a step started belongs to that step (it comes from the agent's output, which
/// reaches its opener slightly after the tool call reached the daemon)
const LATE_THOUGHT: Duration = Duration::from_secs(2);
/// Lines of a step's result kept for the page
const RESULT_LINES: usize = 6;

impl Task {
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, TaskState> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn info(&self) -> TaskInfo {
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

    pub(super) fn emit(&self, st: &mut TaskState, name: &str, data: Value) {
        // `done` is the last event: readers stop at it (the page, `task events`), and a log replayed later ends there
        if !st.status.active() && name != "done" {
            return;
        }
        st.next_seq += 1;
        let seq = st.next_seq;
        let e = TaskEvent { event: name.into(), data };
        // `task` (header) and `done` (terminal outcome) are always persisted even after hitting the cap so restart
        // recovery never misclassifies a finished task as `interrupted` and lagged followers always receive `done`.
        let essential = matches!(name, "task" | "done");
        if !essential && st.capped {
            let _ = self.tx.send((seq, e));
            return;
        }
        let line = encode_line(seq, &e);
        let line_bytes = line.len() as u64;
        let over_limit = !essential
            && (st.stored_events >= st.max_events || st.stored_bytes.saturating_add(line_bytes) > st.max_bytes);
        if over_limit {
            st.capped = true;
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!(
                    "任务 {} 日志达到上限（{} 条 / {} 字节），后续普通事件不再落盘（保留终态 done）",
                    "Task {} log reached its limit ({} events / {} bytes); skipping further ordinary events on disk (done is still kept)",
                    self.id,
                    st.max_events,
                    st.max_bytes
                )
            );
            let _ = self.tx.send((seq, e));
            return;
        }
        if st.on_disk {
            match self.file.as_deref().and_then(|f| append_line(f, &line)) {
                Some(new_len) => {
                    st.stored_bytes = new_len;
                    st.stored_events += 1;
                }
                None => {
                    // what the file has so far, here from now on
                    st.log = self.file.as_deref().map(|f| read_events_prefix(f, st.stored_bytes)).unwrap_or_default();
                    st.on_disk = false;
                    st.log.push((seq, e.clone()));
                    st.stored_events += 1;
                    st.stored_bytes = st.stored_bytes.saturating_add(line_bytes);
                }
            }
        } else {
            st.log.push((seq, e.clone()));
            st.stored_events += 1;
            st.stored_bytes = st.stored_bytes.saturating_add(line_bytes);
        }
        let _ = self.tx.send((seq, e));
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

    /// Takes a consistent snapshot of the task's stored history and its live broadcast receiver without holding the
    /// task mutex during disk reads: the lock only subscribes to `tx` and records the exact file byte length (or clones
    /// the in-memory fallback), and the file prefix is then read on a blocking thread.
    pub async fn subscribe(&self) -> Subscription {
        enum Source {
            File { path: PathBuf, len: u64 },
            Memory(Vec<(u64, TaskEvent)>),
        }
        let (source, snap_seq, rx, ended) = {
            let st = self.lock();
            let rx = self.tx.subscribe();
            let snap_seq = st.next_seq;
            let ended = !st.status.active();
            let source = match self.file.as_deref() {
                Some(f) if st.on_disk => Source::File { path: f.to_owned(), len: st.stored_bytes },
                _ => Source::Memory(st.log.clone()),
            };
            (source, snap_seq, rx, ended)
        };
        let events = match source {
            Source::Memory(events) => events,
            Source::File { len: 0, .. } => Vec::new(),
            Source::File { path, len } => {
                tokio::task::spawn_blocking(move || read_events_prefix(&path, len)).await.unwrap_or_default()
            }
        };
        Subscription { events, snap_seq, rx, ended }
    }

    #[cfg(test)]
    pub(crate) fn set_log_limits(&self, max_events: usize, max_bytes: u64) {
        let mut st = self.lock();
        st.max_events = max_events;
        st.max_bytes = max_bytes;
    }

    /// Ends the task unless it already ended. Returns whether it did
    pub fn end(&self, end: End, summary: &str) -> bool {
        let mut st = self.lock();
        if !st.status.active() {
            return false;
        }
        // the steps under way end with the task, said before `done` (nothing is written after it)
        st.last = None;
        for id in std::mem::take(&mut st.in_flight) {
            self.emit(&mut st, "step", json!({"id": id, "status": "stopped"}));
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

    /// The agent saw the screen (an observation that succeeded): it may act again (`TaskState::recheck`)
    pub fn observed(&self) {
        self.lock().recheck = false;
    }

    /// Holds until the task runs; `Err` when it ended or the caller went away
    pub(crate) async fn wait_running(&self, gone: &mut Option<oneshot::Receiver<()>>) -> Result<bool, Response> {
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

    pub(super) fn thought(&self, text: &str) {
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

    pub(super) fn begin_step(
        &self,
        st: &mut TaskState,
        tool: &str,
        args: &Value,
        gesture: Option<Value>,
        status: &str,
    ) -> u64 {
        let id = st.next_step;
        st.next_step += 1;
        let thought = st.thought.take();
        st.last = Some((id, Instant::now(), true, thought.is_some()));
        st.in_flight.push(id);
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
    pub(super) async fn step(&self, tool: &str, args: &Value, gone: &mut Option<oneshot::Receiver<()>>) -> Response {
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
    pub(super) fn step_done(&self, id: u64, ok: bool, ms: u64, text: &str, gesture_now: Option<Value>) {
        let mut st = self.lock();
        if let Some((last, at, _, th)) = st.last
            && last == id
        {
            st.last = Some((last, at, false, th));
        }
        st.in_flight.retain(|&s| s != id);
        // the user said no to going outside the scope: that's a choice, not something that went wrong
        let declined = st.declined.iter().position(|&s| s == id).map(|i| st.declined.swap_remove(i)).is_some();
        let status = match (ok, declined) {
            (true, _) => "ok",
            (false, true) => "rejected",
            (false, false) => "error",
        };
        let mut d = json!({"id": id, "status": status, "ms": ms, "result": first_lines(text)});
        // where iphone_tap_element tapped is only known now, from its result
        let by_text = st.taps_by_text.iter().position(|&s| s == id).map(|i| st.taps_by_text.swap_remove(i));
        let g = gesture_now
            .or_else(|| (ok && by_text.is_some()).then(|| gesture("iphone_tap_element", &Value::Null, text)).flatten());
        if let Some(g) = g {
            d["gesture"] = g;
        }
        self.emit(&mut st, "step", d);
    }

    /// Shows a confirmation on the page and holds until the user answers, the task ends or the caller goes away.
    /// Asks only while the task runs, like a step: a pause or takeover holds the question (it doesn't replace the
    /// user's pause), and so does another question (one at a time, each answered on its own)
    pub(super) async fn ask_user(&self, action: &str, detail: &str, gone: &mut Option<oneshot::Receiver<()>>) -> Asked {
        let (mut rx, id) = loop {
            if self.wait_running(gone).await.is_err() {
                return if self.lock().status.active() { Asked::Gone } else { Asked::Ended };
            }
            let mut st = self.lock();
            match st.status {
                Status::Running => {}
                Status::Ended(_) => return Asked::Ended,
                // paused, taken over or asked again between the wake-up and the lock: wait again
                _ => continue,
            }
            let args = json!({"action": action, "detail": detail});
            let id = self.begin_step(&mut st, "iphone_confirm", &args, None, "waiting");
            self.emit(&mut st, "confirm", json!({"step": id, "action": action, "detail": detail}));
            let (tx, rx) = oneshot::channel();
            st.confirm = Some((id, tx));
            self.set_status(&mut st, Status::Waiting);
            break (rx, id);
        };
        // a sender dropped without an answer isn't the user saying no: nothing to carry out either way
        let answer = match gone {
            Some(g) => tokio::select! {
                a = &mut rx => a.ok(),
                _ = g => None,
            },
            None => rx.await.ok(),
        };
        let mut st = self.lock();
        st.last = None;
        st.in_flight.retain(|&s| s != id);
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
        if !ended && st.status == Status::Waiting {
            self.set_status(&mut st, Status::Running);
        }
        asked
    }

    /// The agent asks before a sensitive action ([`crate::protocol::Request::TaskConfirm`])
    pub(super) async fn confirm(
        &self,
        action: &str,
        detail: &str,
        gone: &mut Option<oneshot::Receiver<()>>,
    ) -> Response {
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

    pub(super) fn finish(&self, ok: bool, summary: &str) {
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

    /// The user's pause, resume and the like, for the daemon's own tests
    #[cfg(test)]
    pub(crate) fn control_as_user(&self, action: &str) -> Result<(), String> {
        self.control(action, None, false, None)
    }

    /// `by`: who asked, for the log (a `control` event after the change)
    pub(super) fn control(
        &self,
        action: &str,
        step: Option<u64>,
        approve: bool,
        by: Option<&str>,
    ) -> Result<(), String> {
        let mut st = self.lock();
        if !st.status.active() {
            return Err(ictl_l10n::tr!("任务已经结束", "The task has ended").into());
        }
        let mut what = json!({"action": action});
        if let Some(by) = by.map(|b| b.trim().chars().take(80).collect::<String>()).filter(|b| !b.is_empty()) {
            what["by"] = json!(by);
        }
        match (action, st.status) {
            ("pause", Status::Running) => self.set_status(&mut st, Status::Paused),
            ("resume", Status::Paused) => self.set_status(&mut st, Status::Running),
            // also while a step waits for the user: they may want to look or change something before answering; the
            // question stays, to be answered under takeover or after handing back
            ("takeover", Status::Running | Status::Paused | Status::Waiting) => {
                self.set_status(&mut st, Status::Takeover)
            }
            ("handback", Status::Takeover) => {
                st.recheck = true;
                let back = if st.confirm.is_some() { Status::Waiting } else { Status::Running };
                self.set_status(&mut st, back);
            }
            ("confirm", Status::Waiting | Status::Takeover) => match st.confirm.take() {
                Some((waiting, tx)) if step.is_none_or(|s| s == waiting) => {
                    what["step"] = json!(waiting);
                    what["approve"] = json!(approve);
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
                self.emit(&mut st, "control", what);
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
        self.emit(&mut st, "control", what);
        Ok(())
    }
}

/// Where a step touches the screen, for the mark on the page: `{ kind, x, y, x2?, y2? }`.
/// For `iphone_tap_element` it comes from the result text (`… @(x,y) WxH`)
pub(super) fn gesture(tool: &str, args: &Value, result: &str) -> Option<Value> {
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
pub(super) fn request_gesture(req: &Request) -> Option<Value> {
    match req {
        Request::Tap { x, y, .. } => Some(json!({"kind": "tap", "x": x, "y": y})),
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
