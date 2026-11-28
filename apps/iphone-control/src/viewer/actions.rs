//! Control-page actions (`POST /api/…`) and update status (`GET /api/updates`), turned into daemon requests.

use std::path::Path;
use std::time::Duration;

use ictl_daemon::protocol::{self, Request, Response, TouchPhase};
use serde_json::{Value, json};

use super::{Ctx, routes};

/// The release cache belongs to the host user; process versions belong to this Viewer and socket.
pub(super) async fn updates(ctx: &Ctx) -> Result<Value, String> {
    // Sample before reading cached metadata: a check that finishes while this GET
    // awaits the daemon still needs one final poll to display its committed result.
    let checking = ctx.checking.try_lock().is_err();
    let lang = ictl_l10n::lang();
    let mut report = tokio::task::spawn_blocking(move || ictl_l10n::with_lang(lang, crate::update::cached))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    crate::report_daemon(&mut report, &ctx.socket).await;
    let mut v = json!(report);
    v["checking"] = json!(checking);
    v["check_error"] = json!(*ctx.check_error.lock().unwrap_or_else(|p| p.into_inner()));
    Ok(v)
}

pub(super) async fn post(
    ctx: &Ctx,
    name: &str,
    b: &Value,
    device: Option<&str>,
    caller: routes::Caller,
) -> (u16, Value) {
    let conflict = |m: String| (409, json!({"ok": false, "message": m}));
    if name == "updates/check" {
        if !b.as_object().is_some_and(serde_json::Map::is_empty) {
            return (400, json!({"message": "Expected {}"}));
        }
        if let Ok(guard) = ctx.checking.clone().try_lock_owned() {
            let error = ctx.check_error.clone();
            tokio::spawn(async move {
                let result = crate::update::check(false).await;
                *error.lock().unwrap_or_else(|p| p.into_inner()) = result.err().map(|e| e.to_string());
                drop(guard);
            });
        }
        return (202, json!({"checking": true}));
    }
    // how much of an H.264 stream the page has read (see `crate::video`)
    if name == "video/ack" {
        let ok = match (b["stream"].as_str(), b["bytes"].as_u64()) {
            (Some(id), Some(n)) => ctx.acks.report(id, n),
            _ => false,
        };
        return (if ok { 200 } else { 404 }, json!({"ok": ok}));
    }
    if name == "agent/tasks" {
        let prompt = b["prompt"].as_str().unwrap_or_default();
        let backend = b["agent"].as_str().unwrap_or("claude");
        // the apps the task is limited to (bundle IDs from the page's picker); absent or empty: no limit
        let apps: Vec<String> = match &b["apps"] {
            Value::Null => Vec::new(),
            Value::Array(a) if a.iter().all(Value::is_string) => {
                a.iter().filter_map(Value::as_str).map(str::to_owned).collect()
            }
            _ => return (400, json!({"ok": false, "message": "apps must be a list of strings"})),
        };
        return match ctx.agents.start(prompt, backend, &apps).await {
            Ok(id) => (200, json!({"ok": true, "id": id})),
            Err(m) => conflict(m),
        };
    }
    if let Some(rest) = name.strip_prefix("agent/tasks/")
        && let Some((id, action)) = rest.split_once('/')
    {
        return match ctx.agents.control(id, action, b, caller.by, caller.pid).await {
            Ok(()) => (200, json!({"ok": true})),
            Err(m) => conflict(m),
        };
    }
    // while a task runs the daemon refuses switching devices, signing and acting (unless the user took over), and
    // says why; the page shows that message
    if name == "select" {
        let Some(device) = b["device"].as_str() else { return (400, json!({"ok": false, "message": "bad action"})) };
        let v = run_action(&ctx.socket, &Request::SelectDevice { device: device.to_owned() }, None, SHORT).await;
        // the page follows the device it picked
        if v["ok"] == true
            && let Ok(s) = crate::client::snapshot(&ctx.socket).await
            && let Some(d) = s.device
        {
            *ctx.udid.lock().unwrap_or_else(|p| p.into_inner()) = d.udid;
        }
        return (200, v);
    }
    let udid = device.map(str::to_owned).unwrap_or_else(|| ctx.udid());
    // not known yet (the daemon wasn't up when this started): no device check rather than one that always fails
    let udid = Some(udid.as_str()).filter(|u| !u.is_empty());
    if name == "sign" {
        // signing for the page's device: refused if the daemon has moved on to another one
        return (200, run_action(&ctx.socket, &Request::Resign { force: true }, udid, SIGN).await);
    }
    let Some(req) = action(name, b) else { return (400, json!({"ok": false, "message": "bad action"})) };
    if matches!(req, Request::TapElement { .. }) {
        // The daemon resolves the name in a fresh accessibility tree on this device.
        let Some(target) = device.filter(|id| !id.is_empty()) else {
            return conflict(ictl_l10n::tr!("请先选择设备", "Select a device first.").into());
        };
        return (200, run_action(&ctx.socket, &req, Some(target), SHORT).await);
    }
    let positional = matches!(
        req,
        Request::Tap { .. }
            | Request::LongPress { .. }
            | Request::Swipe { .. }
            | Request::Touch { phase: TouchPhase::Down | TouchPhase::Move | TouchPhase::Up, .. }
    );
    if positional {
        let (Some(target), Some(view)) = (device, b["view"].as_str().filter(|id| !id.is_empty())) else {
            return conflict(
                ictl_l10n::tr!("画面已变化，请刷新后重试", "Display changed. Refresh the viewer and try again.").into(),
            );
        };
        let reply = protocol::call_in_view(&ctx.socket, &req, Some(SHORT), target, view).await;
        return (200, action_result(&req, reply));
    }
    (200, run_action(&ctx.socket, &req, udid, SHORT).await)
}

/// Maps a page action to the daemon request it stands for
pub(super) fn action(name: &str, b: &Value) -> Option<Request> {
    let num = |k: &str| b.get(k).and_then(Value::as_f64).filter(|v| v.is_finite());
    let text = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(match name {
        "tap" => Request::Tap { x: num("x")?, y: num("y")?, count: 1 },
        "long_press" => Request::LongPress { x: num("x")?, y: num("y")?, seconds: num("seconds")?.clamp(0.3, 5.0) },
        "swipe" => Request::Swipe {
            from: (num("x1")?, num("y1")?),
            to: (num("x2")?, num("y2")?),
            seconds: num("seconds")?.clamp(0.05, 3.0),
            hold: 0.0,
        },
        "type" => {
            Request::TypeText { text: text("text")?, submit: b.get("submit").and_then(Value::as_bool).unwrap_or(false) }
        }
        "button" => Request::PressButton { button: text("button")? },
        "touch" => Request::Touch {
            phase: match b.get("phase")?.as_str()? {
                "prepare" => TouchPhase::Prepare,
                "down" => TouchPhase::Down,
                "move" => TouchPhase::Move,
                "up" => TouchPhase::Up,
                _ => return None,
            },
            x: num("x").unwrap_or(0.0),
            y: num("y").unwrap_or(0.0),
        },
        "launch_app" => Request::LaunchApp { app: text("app")?.trim().to_owned() },
        "terminate_app" => Request::TerminateApp { app: text("app")?.trim().to_owned() },
        "open_url" => Request::OpenUrl { url: text("url")?.trim().to_owned() },
        "tap_element" => {
            let selector = selector_of(b);
            let t = match text("text") {
                Some(t) => t,
                None if selector.has_structure() => String::new(),
                None => return None,
            };
            Request::TapElement {
                text: t,
                index: b.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
                max_age_ms: 0,
                scroll: None,
                selector,
            }
        }
        "wait_for" => {
            let selector = selector_of(b);
            let t = match text("text") {
                Some(t) => t,
                None if selector.has_structure() => String::new(),
                None => return None,
            };
            Request::WaitFor {
                text: t,
                timeout_ms: (num("timeout").unwrap_or(5.0).clamp(0.5, 30.0) * 1000.0) as u64,
                selector,
            }
        }
        _ => return None,
    })
}

fn selector_of(b: &Value) -> protocol::ElementSelector {
    let kind = b.get("kind").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let id = b.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let mode = match b.get("match").and_then(Value::as_str) {
        Some("exact") => protocol::MatchMode::Exact,
        Some("contains") => protocol::MatchMode::Contains,
        _ => protocol::MatchMode::Auto,
    };
    protocol::ElementSelector { kind, id, mode }
}

/// Sends one action to the daemon; `udid` limits it to the viewer's device (see the module docs)
/// How long a page action may take; signing and installing WDA has its own, longer limit in the daemon
pub(super) const SHORT: Duration = Duration::from_secs(120);
const SIGN: Duration = Duration::from_secs(16 * 60);

/// An action from the page: the person sees the screen ([`protocol::WATCHED`])
async fn run_action(socket: &Path, req: &Request, udid: Option<&str>, timeout: Duration) -> Value {
    action_result(req, protocol::call_watched(socket, req, Some(timeout), udid).await)
}

pub(super) fn action_result(req: &Request, result: Result<Response, impl std::fmt::Display>) -> Value {
    match result {
        // tapping by text that found nothing is an ordinary answer for the agent, a failure for the page
        Ok(Response::Text { text, outcome, .. }) => {
            let missed = matches!(req, Request::TapElement { .. }) && outcome.is_some_and(|o| o.tapped_nothing);
            json!({"ok": !missed, "message": text})
        }
        Ok(Response::Image { caption, .. }) => json!({"ok": true, "message": caption}),
        Ok(Response::Error { message, retry_as_gesture, .. }) => json!({"ok": false, "message": message,
            "retry_as_gesture": retry_as_gesture && matches!(req, Request::Touch { phase: TouchPhase::Down, .. })}),
        Err(e) => json!({"ok": false, "message": e.to_string()}),
    }
}
