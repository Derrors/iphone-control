//! On-disk JSONL persistence and restart recovery for task event logs.

use std::path::Path;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

use crate::protocol::TaskEvent;

use super::{End, Status, Task, TaskState, now_ms};

/// Whether the event was written
pub(super) fn append(file: &Path, e: &TaskEvent) -> bool {
    use std::io::{Seek, Write};
    let line = format!("{}\n", json!({"t": now_ms(), "event": e.event, "data": e.data}));
    let r = std::fs::OpenOptions::new().create(true).append(true).open(file).and_then(|mut f| {
        let end = f.seek(std::io::SeekFrom::End(0))?;
        // all of the line or none of it: a write cut short (a full disk) is taken back, or the file would end in half
        // a line, read as no event or as this one (which is then also kept in memory) and glued to the next
        f.write_all(line.as_bytes()).inspect_err(|_| {
            let _ = f.set_len(end);
        })
    });
    if let Err(err) = &r {
        tracing::warn!("{}", ictl_l10n::log_trf!("写任务日志失败：{err}", "Writing the task log failed: {err}"));
    }
    r.is_ok()
}

pub(super) fn drop_unreadable(file: &Path) {
    tracing::warn!(
        "{}",
        ictl_l10n::log_trf!(
            "任务日志读不出来，删掉：{}",
            "A task log can't be read, so it was removed: {}",
            file.display()
        )
    );
    let _ = std::fs::remove_file(file);
}

pub(super) fn read_events(file: &Path) -> Vec<TaskEvent> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!("读任务日志失败（{}）：{e}", "Reading a task log failed ({}): {e}", file.display())
            );
            return Vec::new();
        }
    };
    text.lines().filter_map(|l| serde_json::from_str::<TaskEvent>(l).ok()).collect()
}

/// A log whose last line was cut short (a power cut while it was written) ends at its last whole line again: what is
/// written next (`done`, as the task is interrupted) would otherwise be glued to the half line and lost with it
fn drop_partial_line(file: &Path) {
    let Ok(bytes) = std::fs::read(file) else { return };
    if bytes.last().is_none_or(|&b| b == b'\n') {
        return;
    }
    let keep = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let r = std::fs::OpenOptions::new().write(true).open(file).and_then(|f| f.set_len(keep as u64));
    if let Err(e) = r {
        tracing::warn!(
            "{}",
            ictl_l10n::log_trf!("截不掉任务日志的半行：{e}", "Couldn't cut a task log's half line: {e}")
        );
    }
}

pub(super) fn first_started(file: &Path) -> Option<u64> {
    use std::io::BufRead;
    let f = std::fs::File::open(file).ok()?;
    let line = std::io::BufReader::new(f).lines().next()?.ok()?;
    serde_json::from_str::<Value>(&line).ok()?["data"]["started"].as_u64()
}

/// A task from its log; one that hadn't ended is interrupted now
pub(super) fn restore(file: &Path) -> Option<Task> {
    drop_partial_line(file);
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
        opener: None,
        file: Some(file.to_owned()),
        st: Mutex::new(TaskState {
            status: Status::Running,
            // read again from the file when wanted
            log: Vec::new(),
            on_disk: true,
            next_step: steps.unwrap_or(0) + 1,
            thought: None,
            last: None,
            in_flight: Vec::new(),
            taps_by_text: Vec::new(),
            recheck: false,
            confirm: None,
            rejected: false,
            ended: None,
            declined: Vec::new(),
            agent_below_opener: false,
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
