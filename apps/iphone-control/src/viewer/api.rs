//! The page's JSON (`GET /api/…`) from the daemon's answers: its snapshot and the facts it reports as data
//! ([`protocol::Data`]), never its text, which is worded for people and agents

use super::*;

/// One daemon request whose answer the page needs as data ([`protocol::Data`], not the text: that is worded for
/// people and agents and may change)
pub(super) async fn data_of(socket: &Path, req: Request) -> Result<Option<protocol::Data>, String> {
    answer_data(protocol::call(socket, &req, Some(Duration::from_secs(60))).await)
}

/// [`data_of`] for a check the page repeats anyway: [`protocol::Data::Busy`] when an action wanted the phone
/// ([`protocol::BACKGROUND`])
pub(super) async fn polled(socket: &Path, req: Request) -> Result<Option<protocol::Data>, String> {
    answer_data(protocol::call_background(socket, &req, Some(Duration::from_secs(60))).await)
}

fn answer_data(r: std::io::Result<Response>) -> Result<Option<protocol::Data>, String> {
    match r {
        Ok(Response::Text { data, .. }) => Ok(data),
        Ok(Response::Error { message, .. }) => Err(message),
        Ok(Response::Image { caption, .. }) => Err(caption),
        Err(e) => Err(e.to_string()),
    }
}

/// Screen size in points (portrait) for turning a click on the picture into a tap, and the device the viewer follows.
/// It follows the device the daemon controls: switched on this page, on another page, with `iphone-control select`,
/// or automatically after an unplug. The page notices `udid` changing and moves its picture and panels over; until
/// then its actions carry the old UDID and are refused, never sent to a phone the user isn't looking at
pub(super) async fn info(ctx: &Ctx) -> Result<Value, String> {
    let s = crate::client::snapshot(&ctx.socket).await.map_err(|e| e.to_string())?;
    let udid = s.device.as_ref().map(|d| d.udid.as_str()).unwrap_or_default();
    *ctx.udid.lock().unwrap_or_else(|p| p.into_inner()) = udid.to_owned();
    let screen = screen_of(&s.persisted, udid);
    let live_touch = s.device.as_ref().is_some_and(|d| d.major_version() >= 27);
    Ok(json!({
        "display": s.display,
        "width": screen.map(|s| s.width),
        "height": screen.map(|s| s.height),
        "udid": udid,
        // the picture's pointer drives a finger over CoreDevice HID (iOS 27 and later); older phones get whole gestures
        "live_touch": live_touch,
        "device": s.device.as_ref().map(|d| &d.name),
        "model": s.device.as_ref().map(|d| &d.product_type),
        "model_name": s.device.as_ref().and_then(|d| d.model_name()),
        "family": s.device.as_ref().map_or("unknown", |d| d.family()),
        "state": state_of(&s.state).0,
        // the picture as H.264 (`/video`) and with which encoder; null: MJPEG only
        "video": ctx.video.as_ref().and_then(|e| e.label()),
        // the phone's own screen video can be tried (`/video?src=device`): iOS 27 and later, not turned off
        "device_video": ctx.device_video_allowed && live_touch,
    }))
}

pub(super) fn state_of(s: &DeviceState) -> (&'static str, Option<String>) {
    match s {
        DeviceState::Absent => ("Absent", None),
        DeviceState::NeedsTrust => ("NeedsTrust", None),
        DeviceState::DeveloperModeOff => ("DeveloperModeOff", None),
        DeviceState::NeedsUnlock => ("NeedsUnlock", None),
        DeviceState::NeedsSigning => ("NeedsSigning", None),
        DeviceState::WdaStarting { last_error } => ("WdaStarting", last_error.clone()),
        DeviceState::NeedsAutomationPin => ("NeedsAutomationPin", None),
        DeviceState::Preparing { last_error } => ("Preparing", last_error.clone()),
        DeviceState::Ready => ("Ready", None),
    }
}

pub(super) fn device_json(d: &ictl_device::DeviceInfo) -> Value {
    json!({
        "udid": d.udid, "name": d.name, "model": d.product_type, "ios": d.product_version,
        "model_name": d.model_name(), "family": d.family(),
    })
}

/// `{ state, last_error?, device: { name, model, model_name, family, ios, udid }, auto_unlock, devices, fixed, auto }`:
/// with [`devices`]'s list too (the same snapshot), so the device panel asks one request less each round
pub(super) async fn daemon(ctx: &Ctx) -> Result<Value, String> {
    let s = crate::client::snapshot(&ctx.socket).await.map_err(|e| e.to_string())?;
    let (state, last_error) = state_of(&s.state);
    let mut v = json!({
        "state": state,
        "state_text": s.state.describe(),
        "last_error": last_error,
        "device": s.device.as_ref().map(device_json),
        "auto_unlock": s.auto_unlock && !s.unlock_disabled,
        // the page offers to sign and install WDA (NeedsSigning) only with an Apple ID to sign with
        "signed_in": crate::client::account_dir(&ctx.data_dir).selected_email().await.is_ok(),
    });
    let list = devices_of(&s);
    for k in ["devices", "fixed", "auto"] {
        v[k] = list[k].clone();
    }
    Ok(v)
}

/// `{ devices: [{ udid, name, model, model_name, family, ios, controlled }], fixed }`
pub(super) async fn devices(ctx: &Ctx) -> Result<Value, String> {
    let s = crate::client::snapshot(&ctx.socket).await.map_err(|e| e.to_string())?;
    Ok(devices_of(&s))
}

fn devices_of(s: &ictl_daemon::Snapshot) -> Value {
    let cur = s.device.as_ref().map(|d| d.udid.to_ascii_lowercase());
    let list: Vec<Value> = s
        .devices
        .iter()
        .map(|d| {
            let mut v = device_json(d);
            v["controlled"] = (cur.as_deref() == Some(d.udid.to_ascii_lowercase().as_str())).into();
            v
        })
        .collect();
    json!({"devices": list, "fixed": s.pinned_by_env, "auto": s.pinned.is_none()})
}

/// `{ wda_ready, locked, foreground, foreground_id, width, height, route }`
pub(super) async fn status(ctx: &Ctx) -> Result<Value, String> {
    let status = match polled(&ctx.socket, Request::Status).await? {
        Some(protocol::Data::Status(s)) => s,
        // the phone was busy with an action: the page keeps what it showed and asks again
        Some(protocol::Data::Busy) => return Ok(json!({"busy": true})),
        _ => protocol::StatusData::default(),
    };
    // the HID fallback needs iOS 27; an older device without WDA can't be operated at all
    let hid =
        crate::client::snapshot(&ctx.socket).await.ok().and_then(|s| s.device).is_some_and(|d| d.major_version() >= 27);
    let mut v = status_json(&status, hid);
    if let Some(id) = v["foreground_id"].as_str().map(str::to_owned) {
        v["foreground"] = app_name(ctx, &id).await.into();
    }
    Ok(v)
}

pub(super) fn status_json(s: &protocol::StatusData, hid: bool) -> Value {
    json!({
        "wda_ready": s.wda_ready,
        "locked": s.locked,
        "possibly_locked": s.possibly_locked,
        "foreground_id": s.foreground,
        "foreground": s.foreground,
        "width": s.width,
        "height": s.height,
        // without WDA, screenshots, taps and keys go over CoreDevice HID (iOS 27 and later); otherwise nothing works
        // driven natively (iOS 27 and later): no WDA at all, by design
        "route": if s.native { "native" } else if s.wda_ready { "wda" } else if hid { "hid" } else { "none" },
    })
}

/// The display name of an installed app (the home screen for SpringBoard); the bundle id when unknown
/// App names by bundle ID, and the IDs the app lists didn't have when last read ([`app_name`])
#[derive(Default)]
pub(super) struct AppNames {
    known: HashMap<String, String>,
    missed: HashMap<String, std::time::Instant>,
}

/// An ID not in the app lists is looked for again only this much later: a process that isn't an installed app (some
/// of the system's) is in front for a while, and each status poll would read both lists again meanwhile
const MISSED_FOR: std::time::Duration = std::time::Duration::from_secs(60);

impl AppNames {
    /// Whether the lists should be read for `bundle`
    fn wants_lists(&self, bundle: &str, now: std::time::Instant) -> bool {
        !self.known.contains_key(bundle)
            && self.missed.get(bundle).is_none_or(|at| now.saturating_duration_since(*at) >= MISSED_FOR)
    }
}

pub(super) async fn app_name(ctx: &Ctx, bundle: &str) -> String {
    if bundle == SPRINGBOARD {
        return ictl_l10n::tr!("主屏幕", "Home Screen").into();
    }
    let mut names = ctx.names.lock().await;
    let now = std::time::Instant::now();
    if names.wants_lists(bundle, now) {
        let mut read = false;
        for system in [false, true] {
            if let Ok(list) = list_apps(ctx, system).await {
                read = true;
                names.known.extend(list.into_iter().map(|(n, b)| (b, n)));
            }
        }
        // only when the lists were read: a daemon that didn't answer is asked again next time
        if read && !names.known.contains_key(bundle) {
            if names.missed.len() > 200 {
                names.missed.clear();
            }
            names.missed.insert(bundle.to_owned(), now);
        }
    }
    names.known.get(bundle).cloned().unwrap_or_else(|| bundle.to_owned())
}

pub(super) async fn list_apps(ctx: &Ctx, system: bool) -> Result<Vec<(String, String)>, String> {
    Ok(match data_of(&ctx.socket, Request::ListApps { include_system: system }).await? {
        Some(protocol::Data::Apps { apps }) => apps.into_iter().map(|a| (a.name, a.bundle_id)).collect(),
        _ => Vec::new(),
    })
}

/// `{ apps: [{ name, bundle_id }] }`: the user's apps, and with `system` the system's too
pub(super) async fn apps(ctx: &Ctx, system: bool) -> Result<Value, String> {
    let mut all = list_apps(ctx, false).await?;
    if system {
        all.extend(list_apps(ctx, true).await?);
    }
    let mut seen = std::collections::HashSet::new();
    all.retain(|(_, b)| seen.insert(b.clone()));
    Ok(json!({"apps": all.iter().map(|(n, b)| json!({"name": n, "bundle_id": b})).collect::<Vec<_>>()}))
}

/// `{ elements: [{ type, label, x, y, width, height }] }`, `x`/`y` the top-left corner in points
/// `keep_locked`: a read-only visitor, for whom a locked phone isn't unlocked
pub(super) async fn elements(ctx: &Ctx, filter: &str, keep_locked: bool) -> Result<Value, String> {
    let req = Request::ListElements { filter: filter.to_owned(), limit: ELEMENT_LIMIT, max_age_ms: 0, keep_locked };
    Ok(serde_json::to_value(element_list(data_of(&ctx.socket, req).await?)).expect("element list is JSON"))
}

/// A system alert on the phone (a permission prompt and the like): `{ alert: null }` or
/// `{ alert: { text, frame?: { x, y, width, height }, buttons: [{ label, x, y, width }] } }` in points: the alert's
/// box (top left), each button's center and width. The page draws it where the real one is. The phone's own video
/// (iOS 27, D16) can leave such an alert out: a location prompt showed there only as the dimmed screen behind it,
/// while WDA's screenshot had it. The page asks while it shows that video and puts the alert next to the picture.
/// `AlertCheck` is two quick WDA calls (the foreground app; the alert's text only when SpringBoard is in front); the
/// element tree is only read when there is an alert
pub(super) async fn alert(ctx: &Ctx, keep_locked: bool) -> Result<Value, String> {
    let text = match polled(&ctx.socket, Request::AlertCheck).await? {
        Some(protocol::Data::Alert { text: Some(text) }) => text,
        Some(protocol::Data::Busy) => return Ok(json!({"busy": true})),
        _ => return Ok(json!({"alert": null})),
    };
    let req = Request::ListElements { filter: String::new(), limit: ELEMENT_LIMIT, max_age_ms: 0, keep_locked };
    // without its buttons the page still says there is an alert
    let els = data_of(&ctx.socket, req).await.map(element_list).unwrap_or_default();
    let (frame, buttons) = alert_parts(&els.elements);
    Ok(json!({"alert": {"text": text, "frame": frame, "buttons": buttons}}))
}

/// The elements in a [`protocol::Data::Elements`] as the page's JSON: `{ type, label, x, y, width, height }`
#[derive(Default, serde::Serialize)]
pub(super) struct ElementList {
    pub(super) elements: Vec<Value>,
    matched_count: Option<usize>,
    truncated: Option<bool>,
}

pub(super) fn element_list(data: Option<protocol::Data>) -> ElementList {
    match data {
        Some(protocol::Data::Elements { elements, matched_count, truncated }) => {
            ElementList { elements: elements.into_iter().map(|e| json!(e)).collect(), matched_count, truncated }
        }
        _ => ElementList::default(),
    }
}

/// The tree's alert box, and the buttons on it (whose center lies inside it) as `{ label, x, y, width }`
pub(super) fn alert_parts(els: &[Value]) -> (Value, Vec<Value>) {
    let f = |e: &Value, k: &str| e[k].as_f64().unwrap_or(0.0);
    let Some(a) = els.iter().find(|e| e["type"] == "Alert") else { return (Value::Null, Vec::new()) };
    let frame = json!({"x": f(a, "x"), "y": f(a, "y"), "width": f(a, "width"), "height": f(a, "height")});
    let inside = |x: f64, y: f64| {
        x >= f(a, "x") && x <= f(a, "x") + f(a, "width") && y >= f(a, "y") && y <= f(a, "y") + f(a, "height")
    };
    let buttons = els
        .iter()
        .filter(|e| e["type"] == "Button")
        .map(|e| (e, f(e, "x") + f(e, "width") / 2.0, f(e, "y") + f(e, "height") / 2.0))
        .filter(|&(_, x, y)| inside(x, y))
        .map(|(e, x, y)| json!({"label": e["label"], "x": x, "y": y, "width": f(e, "width")}))
        .collect();
    (frame, buttons)
}

/// The recorded screen size, only if it belongs to `udid` (a size recorded by an older version, without
/// a device, is used as is, like the daemon does)
pub(super) fn screen_of(p: &ictl_daemon::core::Persisted, udid: &str) -> Option<ictl_device::ScreenSize> {
    p.screen_for(Some(udid)).map(|s| s.portrait())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ID the app lists didn't have is looked for again only after a minute; a known one never
    #[test]
    fn an_unknown_app_is_looked_for_again_only_after_a_while() {
        let t0 = std::time::Instant::now();
        let mut names = AppNames::default();
        assert!(names.wants_lists("com.example.gone", t0));
        names.missed.insert("com.example.gone".into(), t0);
        assert!(!names.wants_lists("com.example.gone", t0 + std::time::Duration::from_secs(30)));
        assert!(names.wants_lists("com.example.gone", t0 + MISSED_FOR));
        names.known.insert("com.apple.Preferences".into(), "设置".into());
        assert!(!names.wants_lists("com.apple.Preferences", t0));
    }
}
