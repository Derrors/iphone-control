//! On-disk JSONL persistence and restart recovery for task event logs.

use std::path::Path;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

use crate::protocol::TaskEvent;

use super::{End, Status, Task, TaskState, now_ms};

/// Ordinary events kept per task log before capping (well above burst tests; `task` and `done` are always kept).
pub(super) const MAX_LOG_EVENTS: usize = 16_384;
/// Maximum serialized byte size of a single task's `.jsonl` log before ordinary events stop being persisted (`task`
/// and `done` are always allowed through so restart recovery never misclassifies a finished task as `interrupted`).
pub(super) const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

#[derive(serde::Deserialize)]
struct DiskLine {
    seq: Option<u64>,
    event: String,
    data: Value,
}

pub(super) fn encode_line(seq: u64, e: &TaskEvent) -> String {
    format!("{}\n", json!({"t": now_ms(), "seq": seq, "event": e.event, "data": e.data}))
}

/// Appends one serialized JSONL line and returns the resulting file length on success.
pub(super) fn append_line(file: &Path, line: &str) -> Option<u64> {
    use std::io::{Seek, Write};
    let r = std::fs::OpenOptions::new().create(true).append(true).open(file).and_then(|mut f| {
        let end = f.seek(std::io::SeekFrom::End(0))?;
        // all of the line or none of it: a write cut short (a full disk) is taken back, or the file would end in half
        // a line, read as no event or as this one (which is then also kept in memory) and glued to the next
        f.write_all(line.as_bytes()).inspect_err(|_| {
            let _ = f.set_len(end);
        })?;
        Ok(end + line.len() as u64)
    });
    if let Err(err) = &r {
        tracing::warn!("{}", ictl_l10n::log_trf!("写任务日志失败：{err}", "Writing the task log failed: {err}"));
    }
    r.ok()
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

/// Reads at most the first `limit` bytes of `file` and parses each complete JSONL event together with its monotonic
/// sequence number. Legacy lines written without a `seq` field receive sequential numbers starting at 1.
pub(super) fn read_events_prefix(file: &Path, limit: u64) -> Vec<(u64, TaskEvent)> {
    use std::io::{BufRead, Read};
    if limit == 0 {
        return Vec::new();
    }
    let f = match std::fs::File::open(file) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!("读任务日志失败（{}）：{e}", "Reading a task log failed ({}): {e}", file.display())
            );
            return Vec::new();
        }
    };
    let mut reader = std::io::BufReader::new(f.take(limit));
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut fallback_seq: u64 = 0;
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(line) = serde_json::from_slice::<DiskLine>(&buf) {
                    let seq = match line.seq {
                        Some(s) if s > 0 => {
                            fallback_seq = fallback_seq.max(s);
                            s
                        }
                        _ => {
                            fallback_seq += 1;
                            fallback_seq
                        }
                    };
                    out.push((seq, TaskEvent { event: line.event, data: line.data }));
                }
            }
            Err(e) => {
                tracing::warn!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "读任务日志失败（{}）：{e}",
                        "Reading a task log failed ({}): {e}",
                        file.display()
                    )
                );
                break;
            }
        }
    }
    out
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
    let file_len = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
    let log = read_events_prefix(file, file_len);
    let first = log.first().filter(|(_, e)| e.event == "task")?.1.data.clone();
    let s = |k: &str| first[k].as_str().unwrap_or_default().to_owned();
    let steps = log.iter().filter_map(|(_, e)| (e.event == "step").then(|| e.data["id"].as_u64()).flatten()).max();
    let done = log.iter().rev().find(|(_, e)| e.event == "done").map(|(_, e)| e.clone());
    let last_seq = log.last().map_or(0, |(seq, _)| *seq);
    let stored_events = log.len();
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
            next_seq: last_seq,
            stored_events,
            stored_bytes: file_len,
            capped: stored_events >= MAX_LOG_EVENTS || file_len >= MAX_LOG_BYTES,
            max_events: MAX_LOG_EVENTS,
            max_bytes: MAX_LOG_BYTES,
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
