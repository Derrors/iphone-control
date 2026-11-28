//! On-demand, diff-deduplicated device state publisher for `GET /api/events` (SSE).
//!
//! Inspired by `vphoned`'s `GuestEventPublisher`: when no control page is connected, no background sampling runs at
//! all. While one or more pages are subscribed, a single shared sampler polls the daemon and broadcasts an SSE event
//! only when the canonical JSON payload for that channel actually changed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::{Ctx, api, protocol};

/// How often the shared sampler checks daemon snapshots (`info`, `daemon`, `tasks`) while subscribers exist.
const SNAPSHOT_EVERY: Duration = Duration::from_secs(2);
/// Sample `/api/status` (which touches the phone via the background lane) every N snapshot rounds unless state changed.
const STATUS_EVERY_ROUNDS: u32 = 5;
const CHANNEL_CAP: usize = 32;

/// One SSE message emitted by [`StateHub`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StateEvent {
    pub(super) event: &'static str,
    pub(super) data: String,
}

impl StateEvent {
    /// Formats as one SSE frame (`event: …\ndata: …\n\n`).
    pub(super) fn sse_frame(&self) -> String {
        format!("event: {}\ndata: {}\n\n", self.event, self.data)
    }
}

/// Last emitted canonical JSON per channel, so unchanged polls broadcast nothing.
#[derive(Default)]
pub(super) struct StateCache {
    info: Option<String>,
    daemon: Option<String>,
    status: Option<String>,
    tasks: Option<String>,
}

impl StateCache {
    /// Records `value` for `event` and returns a [`StateEvent`] only if its serialized JSON changed.
    pub(super) fn update(&mut self, event: &'static str, value: &Value) -> Option<StateEvent> {
        let data = value.to_string();
        let slot = match event {
            "info" => &mut self.info,
            "daemon" => &mut self.daemon,
            "status" => &mut self.status,
            "tasks" => &mut self.tasks,
            _ => return None,
        };
        if slot.as_deref() == Some(data.as_str()) {
            return None;
        }
        *slot = Some(data.clone());
        Some(StateEvent { event, data })
    }

    /// Current cached events in display order, sent immediately to newly connected subscribers.
    pub(super) fn snapshot(&self) -> Vec<StateEvent> {
        let mut out = Vec::with_capacity(4);
        for (event, slot) in
            [("info", &self.info), ("daemon", &self.daemon), ("status", &self.status), ("tasks", &self.tasks)]
        {
            if let Some(data) = slot {
                out.push(StateEvent { event, data: data.clone() });
            }
        }
        out
    }

    fn clear(&mut self) {
        self.info = None;
        self.daemon = None;
        self.status = None;
        self.tasks = None;
    }
}

/// Shared on-demand state broadcaster owned by [`Ctx`].
pub(super) struct StateHub {
    tx: broadcast::Sender<StateEvent>,
    cache: Mutex<StateCache>,
    running: AtomicBool,
}

impl Default for StateHub {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAP);
        Self { tx, cache: Mutex::default(), running: AtomicBool::new(false) }
    }
}

impl StateHub {
    /// Subscribes one SSE client, returning any cached snapshot events plus the live stream, and starts the single
    /// shared sampler if it is not already running.
    pub(super) fn subscribe(&self, ctx: &Arc<Ctx>) -> (Vec<StateEvent>, broadcast::Receiver<StateEvent>) {
        let rx = self.tx.subscribe();
        let initial = self.snapshot();
        self.ensure_running(ctx);
        (initial, rx)
    }

    pub(super) fn snapshot(&self) -> Vec<StateEvent> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner()).snapshot()
    }

    #[cfg(test)]
    pub(super) fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn publish(&self, event: &'static str, value: &Value) {
        if let Some(changed) = self.cache.lock().unwrap_or_else(|p| p.into_inner()).update(event, value) {
            let _ = self.tx.send(changed);
        }
    }

    fn ensure_running(&self, ctx: &Arc<Ctx>) {
        if self.running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return;
        }
        let weak = Arc::downgrade(ctx);
        tokio::spawn(run_sampler(weak));
    }
}

async fn run_sampler(weak: Weak<Ctx>) {
    let mut round: u32 = 0;
    let mut prev_device_state: Option<(String, String)> = None;
    loop {
        let Some(ctx) = weak.upgrade() else {
            return;
        };
        if ctx.events.tx.receiver_count() == 0 {
            ctx.events.cache.lock().unwrap_or_else(|p| p.into_inner()).clear();
            ctx.events.running.store(false, Ordering::SeqCst);
            // A subscriber may have arrived right before `running` was cleared: re-claim if so.
            if ctx.events.tx.receiver_count() > 0
                && ctx.events.running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()
            {
                round = 0;
                prev_device_state = None;
                continue;
            }
            return;
        }

        let mut changed_identity = false;
        if let Ok(info) = api::info(&ctx).await {
            let key = (
                info["udid"].as_str().unwrap_or_default().to_owned(),
                info["state"].as_str().unwrap_or_default().to_owned(),
            );
            if prev_device_state.as_ref() != Some(&key) {
                changed_identity = true;
                prev_device_state = Some(key);
            }
            ctx.events.publish("info", &info);
        }
        if let Ok(daemon) = api::daemon(&ctx).await {
            ctx.events.publish("daemon", &daemon);
        }
        if let Ok(tasks) = protocol::task_list(&ctx.socket).await {
            ctx.events.publish("tasks", &json!(tasks));
        }
        if (round.is_multiple_of(STATUS_EVERY_ROUNDS) || changed_identity)
            && let Ok(status) = api::status(&ctx).await
            && status.get("busy").and_then(Value::as_bool) != Some(true)
        {
            ctx.events.publish("status", &status);
        }
        round = round.wrapping_add(1);
        drop(ctx);
        tokio::time::sleep(SNAPSHOT_EVERY).await;
    }
}

/// Injects per-connection login identity into an `"info"` SSE event without polluting the shared cache.
pub(super) fn personalize_event(ev: &StateEvent, login: Option<(&str, &str)>) -> StateEvent {
    if ev.event != "info" {
        return ev.clone();
    }
    let Some((name, role)) = login else {
        return ev.clone();
    };
    let Ok(mut v) = serde_json::from_str::<Value>(&ev.data) else {
        return ev.clone();
    };
    v["login"] = json!({"name": name, "role": role});
    StateEvent { event: ev.event, data: v.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_cache_deduplicates_identical_updates_and_preserves_order() {
        let mut c = StateCache::default();
        assert!(c.snapshot().is_empty());
        let first = c.update("info", &json!({"b": 2, "a": 1}));
        assert!(first.is_some());
        // Same keys in a different order serialize identically in serde_json's BTreeMap and emit nothing.
        assert!(c.update("info", &json!({"a": 1, "b": 2})).is_none());
        assert!(c.update("daemon", &json!({"state": "Ready"})).is_some());
        assert!(c.update("status", &json!({"locked": false})).is_some());
        assert!(c.update("tasks", &json!([])).is_some());
        let snap = c.snapshot();
        let names: Vec<&str> = snap.iter().map(|e| e.event).collect();
        assert_eq!(names, ["info", "daemon", "status", "tasks"]);
        // A changed value emits again.
        let changed = c.update("status", &json!({"locked": true})).unwrap();
        assert_eq!(changed.event, "status");
        assert!(changed.sse_frame().starts_with("event: status\ndata: "));
        assert!(changed.sse_frame().ends_with("\n\n"));
    }

    #[test]
    fn personalize_event_adds_login_only_to_info() {
        let info = StateEvent { event: "info", data: json!({"udid": "u1"}).to_string() };
        let with_login = personalize_event(&info, Some(("alice", "view")));
        let parsed: Value = serde_json::from_str(&with_login.data).unwrap();
        assert_eq!(parsed["udid"], "u1");
        assert_eq!(parsed["login"], json!({"name": "alice", "role": "view"}));

        let status = StateEvent { event: "status", data: json!({"locked": false}).to_string() };
        assert_eq!(personalize_event(&status, Some(("alice", "view"))), status);
    }
}
