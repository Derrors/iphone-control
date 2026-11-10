//! `iphone-control viewer`: a page on 127.0.0.1 to watch the phone live, act on it with the mouse, look at its
//! state, apps and elements, and hand tasks to a local AI agent (see [`crate::agent`]).
//!
//! - `GET /` the page, `GET /stream` WDA's MJPEG stream (port 9100 on the phone, over usbmuxd, read-only).
//! - `GET /api/{info,daemon,status,devices,apps,elements}`: the daemon's answers as JSON for the page.
//! - `POST /api/{tap,swipe,long_press,type,button,launch_app,terminate_app,open_url,tap_element,wait_for,select}`
//!   turn into ordinary daemon requests, so they queue with an agent's calls, get auto-unlock and follow the
//!   controlled device, exactly like an MCP tool call.
//! - `/api/agent/…`: available agents, starting a task, its events (server-sent events), and pause / resume /
//!   take over / hand back / stop / confirm.
//!
//! Protections, same model as `wda-bridge`: it binds to the loopback address only, every request needs a
//! loopback `Host` (against DNS rebinding), every action also needs an `Origin` of this very page and a JSON
//! content type (against other web pages posting to it), and a read whose `Sec-Fetch-Site` says another site
//! sent it is refused (reading elements unlocks the phone). Other local users can still reach the port, which the
//! command says when it starts.
//!
//! The viewer follows one device, the one controlled when it starts or the one picked on the page: the picture
//! always comes from it, and every action carries its UDID ([`ictl_daemon::protocol::EXPECT_UDID`]), so the daemon
//! refuses an action once it controls another device (after `select` elsewhere, or an unplug in automatic mode)
//! instead of tapping a phone the user isn't looking at. The device handle is looked up again for each stream, so a
//! replug (new usbmuxd id) doesn't break the picture.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::bail;
use ictl_daemon::machine::DeviceState;
use ictl_daemon::protocol::{self, Request, Response, TouchPhase};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::agent::{self, Agents};

const PAGE: &str = include_str!("viewer.html");
/// WDA's MJPEG server on the phone
const MJPEG_PORT: u16 = 9100;
const MAX_BODY: usize = 16 * 1024;
/// Most elements the Elements tab lists
const ELEMENT_LIMIT: usize = 300;
const SPRINGBOARD: &str = "com.apple.springboard";

/// What every connection shares
struct Ctx {
    /// The page, marked with the viewer's language (the page's default; it can switch)
    page: String,
    socket: PathBuf,
    port: u16,
    /// The device the page shows and acts on
    udid: Mutex<String>,
    agents: Arc<Agents>,
    /// Bundle id → display name, for the foreground app
    names: tokio::sync::Mutex<HashMap<String, String>>,
}

impl Ctx {
    fn udid(&self) -> String {
        self.udid.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

pub async fn run(socket: &Path, port: u16) -> anyhow::Result<()> {
    let udid = crate::target_device(socket, None).await?.udid;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    // next to the daemon's socket: a directory only this user can enter
    let gate = socket.parent().unwrap_or(Path::new("/tmp")).join(format!("viewer-{port}.sock"));
    let agents = Agents::new(socket, gate.clone())?;
    agent::GateServer::serve(gate, agents.clone()).await?;
    println!(
        "{}",
        ictl_l10n::trf!(
            "实时画面:http://127.0.0.1:{port}/ (手机 {},Ctrl-C 停止)",
            "Live viewer: http://127.0.0.1:{port}/ (phone {}, Ctrl-C to stop)",
            udid
        )
    );
    let found: Vec<&str> =
        agent::Backend::ALL.iter().filter(|b| b.find().is_some()).map(|b| b.id()).collect::<Vec<_>>();
    println!(
        "{}",
        if found.is_empty() {
            ictl_l10n::tr!(
                "AI 任务:PATH 里没有 claude 或 codex,网页上只能手动操作",
                "AI tasks: neither claude nor codex is on PATH, so the page can only be used by hand"
            )
            .to_owned()
        } else {
            ictl_l10n::trf!("AI 任务可用:{}", "AI tasks available with: {}", found.join(", "))
        }
    );
    println!(
        "{}",
        ictl_l10n::tr!(
            "注意:本机所有用户都能经这个端口看到屏幕、操作手机、让 AI 执行任务,用完就停掉。",
            "Note: every user on this machine can watch and control the phone and start AI tasks through this port; stop it when you are done."
        )
    );
    let ctx = Arc::new(Ctx {
        socket: socket.to_path_buf(),
        port,
        udid: Mutex::new(udid),
        agents,
        names: tokio::sync::Mutex::default(),
        page: page_in(ictl_l10n::lang()),
    });
    let slots = Arc::new(tokio::sync::Semaphore::new(crate::MAX_CONNECTIONS));
    loop {
        let (client, _) = listener.accept().await?;
        // too many open connections (for example many tabs): drop the new one
        let Ok(slot) = slots.clone().try_acquire_owned() else { continue };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(client, &ctx).await {
                tracing::debug!("viewer connection: {e}");
            }
            drop(slot);
        });
    }
}

/// The page with `data-lang` set for English, which the page reads as its default language
fn page_in(lang: ictl_l10n::Lang) -> String {
    match lang {
        ictl_l10n::Lang::En => PAGE.replacen("<html lang=\"zh-CN\">", "<html lang=\"en\" data-lang=\"en\">", 1),
        ictl_l10n::Lang::Zh => PAGE.to_owned(),
    }
}

/// One request per connection (`Connection: close`), which keeps the parser small. It is handled in the page's
/// language (`X-Ictl-Lang`), so the daemon's messages match the page
async fn serve(mut client: TcpStream, ctx: &Ctx) -> anyhow::Result<()> {
    let (head, body) = crate::read_head(&mut client).await?;
    let Some(req) = Head::parse(&head) else {
        return reply(&mut client, 400, "text/plain", b"bad request").await;
    };
    match req.lang {
        Some(l) => ictl_l10n::scope(l, handle(client, ctx, head, req, body)).await,
        None => handle(client, ctx, head, req, body).await,
    }
}

async fn handle(mut client: TcpStream, ctx: &Ctx, head: Vec<u8>, req: Head, mut body: Vec<u8>) -> anyhow::Result<()> {
    if !crate::host_is_loopback(&head) || (req.path != "/" && !fetch_site_ok(&req)) {
        return reply(&mut client, 403, "text/plain", b"forbidden").await;
    }
    let path = req.path.as_str();
    match (req.method.as_str(), path) {
        ("GET", "/") => reply(&mut client, 200, "text/html; charset=utf-8", ctx.page.as_bytes()).await,
        ("GET", "/stream") => {
            // looked up again each time: after a replug usbmuxd gives the device a new id
            let device = ictl_device::Device::find(Some(&ctx.udid())).await;
            let mut remote = match async { device?.connect_port(MJPEG_PORT).await }.await {
                Ok(r) => r,
                Err(e) => return reply(&mut client, 502, "text/plain", e.to_string().as_bytes()).await,
            };
            // WDA's MJPEG server answers any request with the multipart stream
            remote.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").await?;
            tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
            Ok(())
        }
        ("GET", p) if p.starts_with("/api/agent/tasks/") && p.ends_with("/events") => {
            let id = &p["/api/agent/tasks/".len()..p.len() - "/events".len()];
            match ctx.agents.task(id) {
                Some(t) => events(&mut client, &t).await,
                None => {
                    json_reply(&mut client, 404, &json!({"message": ictl_l10n::tr!("没有这个任务", "No such task")}))
                        .await
                }
            }
        }
        ("GET", p) if p.starts_with("/api/") => {
            let v = match &p[5..] {
                // always an answer: the page falls back to its offline demo when this one fails
                "info" => Ok(info(ctx).await.unwrap_or_else(|m| json!({"width": null, "height": null, "warning": m}))),
                "daemon" => daemon(ctx).await,
                "status" => status(ctx).await,
                "devices" => devices(ctx).await,
                "apps" => apps(ctx, req.query("system").is_some_and(|v| v == "1")).await,
                "elements" => elements(ctx, &req.query("filter").unwrap_or_default()).await,
                "agent/backends" => Ok(json!({"backends": agent::backends()})),
                _ => return reply(&mut client, 404, "text/plain", b"not found").await,
            };
            match v {
                Ok(v) => json_reply(&mut client, 200, &v).await,
                Err(m) => json_reply(&mut client, 502, &json!({"message": m})).await,
            }
        }
        ("POST", p) if p.starts_with("/api/") => {
            if !action_allowed(&req, ctx.port) {
                return reply(&mut client, 403, "text/plain", b"forbidden").await;
            }
            let len = req.content_length.unwrap_or(0);
            if len > MAX_BODY {
                return reply(&mut client, 413, "text/plain", b"too large").await;
            }
            while body.len() < len {
                let mut buf = [0u8; 4096];
                let n = tokio::time::timeout(crate::READ_TIMEOUT, client.read(&mut buf)).await??;
                if n == 0 {
                    bail!("body cut off");
                }
                body.extend_from_slice(&buf[..n]);
            }
            body.truncate(len);
            let Ok(b) = serde_json::from_slice::<Value>(&body) else {
                return json_reply(&mut client, 400, &json!({"ok": false, "message": "bad request"})).await;
            };
            let (code, v) = post(ctx, &p[5..], &b).await;
            json_reply(&mut client, code, &v).await
        }
        _ => reply(&mut client, 404, "text/plain", b"not found").await,
    }
}

/// A POST under `/api/`: status code and JSON answer
async fn post(ctx: &Ctx, name: &str, b: &Value) -> (u16, Value) {
    let conflict = |m: String| (409, json!({"ok": false, "message": m}));
    if name == "agent/tasks" {
        let prompt = b["prompt"].as_str().unwrap_or_default();
        let backend = b["agent"].as_str().unwrap_or("claude");
        return match ctx.agents.start(prompt, backend) {
            Ok(id) => (200, json!({"ok": true, "id": id})),
            Err(m) => conflict(m),
        };
    }
    if let Some(rest) = name.strip_prefix("agent/tasks/")
        && let Some((id, action)) = rest.split_once('/')
    {
        return match ctx.agents.control(id, action, b) {
            Ok(()) => (200, json!({"ok": true})),
            Err(m) => conflict(m),
        };
    }
    if name == "select" {
        if ctx.agents.busy() {
            return conflict(
                ictl_l10n::tr!("AI 任务执行中，不能切换设备", "An AI task is running, so the device can't be switched")
                    .into(),
            );
        }
        let Some(device) = b["device"].as_str() else { return (400, json!({"ok": false, "message": "bad action"})) };
        let v = run_action(&ctx.socket, &Request::SelectDevice { device: device.to_owned() }, None).await;
        // the page follows the device it picked
        if v["ok"] == true
            && let Ok(s) = crate::snapshot(&ctx.socket).await
            && let Some(d) = s.device
        {
            *ctx.udid.lock().unwrap_or_else(|p| p.into_inner()) = d.udid;
        }
        return (200, v);
    }
    let Some(req) = action(name, b) else { return (400, json!({"ok": false, "message": "bad action"})) };
    // reading is fine during a task, and so is lifting a finger that was down when it started; acting would compete
    // with the agent
    let harmless = matches!(req, Request::WaitFor { .. } | Request::Touch { phase: TouchPhase::Up, .. });
    if !harmless && !ctx.agents.manual_allowed() {
        return conflict(
            ictl_l10n::tr!(
                "AI 正在操作这台设备。要自己操作，先点「接管」。",
                "The AI is operating this device. To act yourself, click “Take over” first."
            )
            .into(),
        );
    }
    (200, run_action(&ctx.socket, &req, Some(&ctx.udid())).await)
}

/// The task's events so far, then new ones as they come, as server-sent events, until it is done
async fn events(client: &mut TcpStream, task: &agent::Task) -> anyhow::Result<()> {
    client
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n\
              X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        )
        .await?;
    let (log, mut rx, done) = task.subscribe();
    for e in &log {
        client.write_all(sse(e).as_bytes()).await?;
    }
    if done {
        return Ok(());
    }
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(e) => {
                    client.write_all(sse(&e).as_bytes()).await?;
                    if e.name == "done" {
                        return Ok(());
                    }
                }
                // fell behind (many screenshots at once): the page reconnects and gets everything again
                Err(_) => return Ok(()),
            },
            // a comment now and then finds a page that went away
            _ = tokio::time::sleep(Duration::from_secs(15)) => client.write_all(b": ping\n\n").await?,
        }
    }
}

fn sse(e: &agent::Event) -> String {
    format!("event: {}\ndata: {}\n\n", e.name, e.data)
}

async fn json_reply(client: &mut TcpStream, status: u16, v: &Value) -> anyhow::Result<()> {
    reply(client, status, "application/json", v.to_string().as_bytes()).await
}

async fn reply(client: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        _ => "Bad Gateway",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nConnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes()).await?;
    client.write_all(body).await?;
    Ok(())
}

struct Head {
    method: String,
    path: String,
    query: String,
    origin: Option<String>,
    content_type: Option<String>,
    content_length: Option<usize>,
    fetch_site: Option<String>,
    /// `X-Ictl-Lang`: the page's language
    lang: Option<ictl_l10n::Lang>,
}

impl Head {
    fn parse(head: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(head).ok()?;
        let mut lines = text.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let method = first.next()?.to_owned();
        let target = first.next()?;
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let (path, query) = (path.to_owned(), query.to_owned());
        let mut h = Self {
            method,
            path,
            query,
            origin: None,
            content_type: None,
            content_length: None,
            fetch_site: None,
            lang: None,
        };
        for l in lines {
            let Some((k, v)) = l.split_once(':') else { continue };
            let v = v.trim();
            match k.trim().to_ascii_lowercase().as_str() {
                "origin" => h.origin = Some(v.to_owned()),
                "content-type" => h.content_type = Some(v.to_ascii_lowercase()),
                "content-length" => h.content_length = Some(v.parse().ok()?),
                "sec-fetch-site" => h.fetch_site = Some(v.to_ascii_lowercase()),
                "x-ictl-lang" => h.lang = ictl_l10n::parse(v),
                _ => {}
            }
        }
        Some(h)
    }

    /// A query parameter, percent-decoded
    fn query(&self, key: &str) -> Option<String> {
        self.query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k == key).then(|| percent_decode(v))
        })
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An action must come from this page: `Origin` is the viewer's own address, and the body is JSON, which a
/// cross-origin page can't send without a preflight that this server never answers
fn action_allowed(req: &Head, port: u16) -> bool {
    let json = req.content_type.as_deref().is_some_and(|c| c.split(';').next() == Some("application/json"));
    let origin = req.origin.as_deref().is_some_and(|o| {
        ["127.0.0.1", "localhost", "[::1]"].iter().any(|h| o.eq_ignore_ascii_case(&format!("http://{h}:{port}")))
    });
    json && origin
}

/// A read from another site (an `<img>` or `<script>` on some page) is refused: browsers say so in
/// `Sec-Fetch-Site`. Requests without it (curl, older browsers) come from this machine anyway
fn fetch_site_ok(req: &Head) -> bool {
    req.fetch_site.as_deref().is_none_or(|s| matches!(s, "same-origin" | "none"))
}

/// Maps a page action to the daemon request it stands for
fn action(name: &str, b: &Value) -> Option<Request> {
    let num = |k: &str| b.get(k).and_then(Value::as_f64).filter(|v| v.is_finite());
    let text = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(match name {
        "tap" => Request::Tap { x: num("x")?, y: num("y")? },
        "long_press" => Request::LongPress { x: num("x")?, y: num("y")?, seconds: num("seconds")?.clamp(0.3, 5.0) },
        "swipe" => Request::Swipe {
            from: (num("x1")?, num("y1")?),
            to: (num("x2")?, num("y2")?),
            seconds: num("seconds")?.clamp(0.05, 3.0),
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
        "tap_element" => Request::TapElement {
            text: text("text")?,
            index: b.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
            max_age_ms: 0,
        },
        "wait_for" => Request::WaitFor {
            text: text("text")?,
            timeout_ms: (num("timeout").unwrap_or(5.0).clamp(0.5, 30.0) * 1000.0) as u64,
        },
        _ => return None,
    })
}

/// Sends one action to the daemon; `udid` limits it to the viewer's device (see the module docs)
async fn run_action(socket: &Path, req: &Request, udid: Option<&str>) -> Value {
    match protocol::call_expecting(socket, req, Some(Duration::from_secs(120)), udid).await {
        // tapping by text that found nothing is an ordinary answer for the agent, a failure for the page
        Ok(Response::Text { text, outcome }) => {
            let missed = matches!(req, Request::TapElement { .. }) && protocol::tapped_nothing(&text, outcome);
            json!({"ok": !missed, "message": text})
        }
        Ok(Response::Image { caption, .. }) => json!({"ok": true, "message": caption}),
        Ok(Response::Error { message }) => json!({"ok": false, "message": message}),
        Err(e) => json!({"ok": false, "message": e.to_string()}),
    }
}

/// One daemon request whose text the page needs as data. `en`: ask in English, whose wording the parsers expect
async fn text_of(socket: &Path, req: Request, en: bool) -> Result<String, String> {
    let call = protocol::call(socket, &req, Some(Duration::from_secs(60)));
    let r = if en { ictl_l10n::scope(ictl_l10n::Lang::En, call).await } else { call.await };
    match r {
        Ok(Response::Text { text, .. }) => Ok(text),
        Ok(Response::Error { message }) => Err(message),
        Ok(Response::Image { caption, .. }) => Ok(caption),
        Err(e) => Err(e.to_string()),
    }
}

/// Screen size in points (portrait) for turning a click on the picture into a tap, and whether the daemon
/// still controls the viewer's device
async fn info(ctx: &Ctx) -> Result<Value, String> {
    let udid = ctx.udid();
    let s = crate::snapshot(&ctx.socket).await.map_err(|e| e.to_string())?;
    let screen = screen_of(&s.persisted, &udid);
    let controlled = s.device.as_ref().is_some_and(|d| d.udid.eq_ignore_ascii_case(&udid));
    Ok(json!({
        "width": screen.map(|s| s.width),
        "height": screen.map(|s| s.height),
        "udid": udid,
        // the picture's pointer drives a finger over CoreDevice HID (iOS 27 and later); older phones get whole gestures
        "live_touch": s.device.as_ref().is_some_and(|d| d.major_version() >= 27),
        "device": s.device.map(|d| d.name),
        "warning": (!controlled).then(|| ictl_l10n::trf!(
            "守护进程现在控制的不是这台({udid}),操作会被拒绝;在页面上方重新选择设备",
            "The daemon no longer controls this device ({udid}), so actions are refused; pick the device again at the top of the page"
        )),
    }))
}

fn state_of(s: &DeviceState) -> (&'static str, Option<String>) {
    match s {
        DeviceState::Absent => ("Absent", None),
        DeviceState::NeedsTrust => ("NeedsTrust", None),
        DeviceState::DeveloperModeOff => ("DeveloperModeOff", None),
        DeviceState::NeedsUnlock => ("NeedsUnlock", None),
        DeviceState::NeedsSigning => ("NeedsSigning", None),
        DeviceState::WdaStarting { last_error } => ("WdaStarting", last_error.clone()),
        DeviceState::NeedsAutomationPin => ("NeedsAutomationPin", None),
        DeviceState::Ready => ("Ready", None),
    }
}

fn device_json(d: &ictl_device::DeviceInfo) -> Value {
    json!({"udid": d.udid, "name": d.name, "model": d.product_type, "ios": d.product_version})
}

/// `{ state, last_error?, device: { name, model, ios, udid }, auto_unlock }`
async fn daemon(ctx: &Ctx) -> Result<Value, String> {
    let s = crate::snapshot(&ctx.socket).await.map_err(|e| e.to_string())?;
    let (state, last_error) = state_of(&s.state);
    Ok(json!({
        "state": state,
        "state_text": s.state.describe(),
        "last_error": last_error,
        "device": s.device.as_ref().map(device_json),
        "auto_unlock": s.auto_unlock && !s.unlock_disabled,
    }))
}

/// `{ devices: [{ udid, name, model, ios, controlled }], fixed }`
async fn devices(ctx: &Ctx) -> Result<Value, String> {
    let s = crate::snapshot(&ctx.socket).await.map_err(|e| e.to_string())?;
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
    Ok(json!({"devices": list, "fixed": s.pinned_by_env, "auto": s.pinned.is_none()}))
}

/// `{ wda_ready, locked, foreground, foreground_id, width, height, route }`, from the English status text
async fn status(ctx: &Ctx) -> Result<Value, String> {
    let text = text_of(&ctx.socket, Request::Status, true).await?;
    let mut v = parse_status(&text);
    if let Some(id) = v["foreground_id"].as_str().map(str::to_owned) {
        v["foreground"] = app_name(ctx, &id).await.into();
    }
    Ok(v)
}

fn parse_status(text: &str) -> Value {
    let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)).map(str::trim);
    let ready = field("WDA ready:") == Some("True");
    let lock = field("Locked:");
    let locked = lock.map(|l| !l.starts_with("unlocked"));
    let fg = field("Foreground app:").filter(|s| !s.is_empty());
    let (w, h) = field("Screen:")
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<f64>().ok()?, h.parse::<f64>().ok()?)))
        .unzip();
    json!({
        "wda_ready": ready,
        "locked": locked,
        "possibly_locked": lock.is_some_and(|l| l.starts_with("possibly")),
        "foreground_id": fg,
        "foreground": fg,
        "width": w,
        "height": h,
        // without WDA, screenshots, taps and keys go over CoreDevice HID
        "route": if ready { "wda" } else { "hid" },
    })
}

/// The display name of an installed app (the home screen for SpringBoard); the bundle id when unknown
async fn app_name(ctx: &Ctx, bundle: &str) -> String {
    if bundle == SPRINGBOARD {
        return ictl_l10n::tr!("主屏幕", "Home Screen").into();
    }
    let mut names = ctx.names.lock().await;
    if !names.contains_key(bundle) {
        for system in [false, true] {
            if let Ok(list) = list_apps(ctx, system).await {
                for (n, b) in list {
                    names.insert(b, n);
                }
            }
        }
    }
    names.get(bundle).cloned().unwrap_or_else(|| bundle.to_owned())
}

async fn list_apps(ctx: &Ctx, system: bool) -> Result<Vec<(String, String)>, String> {
    let text = text_of(&ctx.socket, Request::ListApps { include_system: system }, false).await?;
    Ok(text
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(n, b)| (n.trim().to_owned(), b.trim().to_owned()))
        .collect())
}

/// `{ apps: [{ name, bundle_id }] }`: the user's apps, and with `system` the system's too
async fn apps(ctx: &Ctx, system: bool) -> Result<Value, String> {
    let mut all = list_apps(ctx, false).await?;
    if system {
        all.extend(list_apps(ctx, true).await?);
    }
    let mut seen = std::collections::HashSet::new();
    all.retain(|(_, b)| seen.insert(b.clone()));
    Ok(json!({"apps": all.iter().map(|(n, b)| json!({"name": n, "bundle_id": b})).collect::<Vec<_>>()}))
}

/// `{ elements: [{ type, label, x, y, width, height }] }`, `x`/`y` the top-left corner in points
async fn elements(ctx: &Ctx, filter: &str) -> Result<Value, String> {
    let req = Request::ListElements { filter: filter.to_owned(), limit: ELEMENT_LIMIT, max_age_ms: 0 };
    let text = text_of(&ctx.socket, req, true).await?;
    Ok(json!({"elements": text.lines().filter_map(parse_element).collect::<Vec<_>>()}))
}

/// One line of `iphone_list_elements`: `Type "label" [id="…"] [value="…"] @(cx,cy) WxH`. Other lines (a note
/// that more were left out) are skipped
fn parse_element(line: &str) -> Option<Value> {
    let (kind, rest) = line.split_once(" \"")?;
    if kind.is_empty() || kind.contains(' ') {
        return None;
    }
    let at = rest.rfind(" @(")?;
    let (desc, geom) = (&rest[..at], &rest[at + 3..]);
    let label_end =
        [desc.find("\" id=\""), desc.find("\" value=\"")].into_iter().flatten().min().unwrap_or_else(|| {
            // no id or value: the label runs to the last quote
            desc.rfind('"').unwrap_or(desc.len())
        });
    let label = &desc[..label_end];
    let (center, size) = geom.split_once(") ")?;
    let (cx, cy) = center.split_once(',')?;
    let (w, h) = size.trim().split_once('x')?;
    let n = |s: &str| s.trim().parse::<f64>().ok();
    let (cx, cy, w, h) = (n(cx)?, n(cy)?, n(w)?, n(h)?);
    Some(json!({"type": kind, "label": label, "x": cx - w / 2.0, "y": cy - h / 2.0, "width": w, "height": h}))
}

/// The recorded screen size, only if it belongs to `udid` (a size recorded by an older version, without
/// a device, is used as is, like the daemon does)
fn screen_of(p: &ictl_daemon::core::Persisted, udid: &str) -> Option<ictl_device::ScreenSize> {
    let same = p.screen_udid.as_deref().is_none_or(|u| u.eq_ignore_ascii_case(udid));
    p.screen.filter(|_| same).map(|s| s.portrait())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(s: &str) -> Head {
        Head::parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn actions_need_own_origin_and_json() {
        let ok = "POST /api/tap HTTP/1.1\r\nHost: 127.0.0.1:8200\r\nOrigin: http://127.0.0.1:8200\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n";
        assert!(action_allowed(&head(ok), 8200));
        assert!(action_allowed(&head(&ok.replace("http://127.0.0.1:8200", "http://localhost:8200")), 8200));
        // another page, another port, no origin, a form post
        assert!(!action_allowed(&head(&ok.replace("http://127.0.0.1:8200", "http://evil.example")), 8200));
        assert!(!action_allowed(&head(&ok.replace("http://127.0.0.1:8200", "http://127.0.0.1:3000")), 8200));
        assert!(!action_allowed(&head(&ok.replace("Origin: http://127.0.0.1:8200\r\n", "")), 8200));
        assert!(!action_allowed(&head(&ok.replace("application/json", "text/plain")), 8200));
        assert!(!action_allowed(&head(&ok.replace("Origin: http://127.0.0.1:8200", "Origin: null")), 8200));
    }

    #[test]
    fn reads_from_other_sites_are_refused() {
        let get = |site: &str| head(&format!("GET /api/elements HTTP/1.1\r\nHost: 127.0.0.1:8200\r\n{site}\r\n"));
        assert!(fetch_site_ok(&get("Sec-Fetch-Site: same-origin\r\n")));
        assert!(fetch_site_ok(&get("Sec-Fetch-Site: none\r\n")), "typed into the address bar");
        assert!(fetch_site_ok(&get("")), "curl");
        assert!(!fetch_site_ok(&get("Sec-Fetch-Site: cross-site\r\n")));
        assert!(!fetch_site_ok(&get("Sec-Fetch-Site: same-site\r\n")), "another port on localhost");
    }

    #[test]
    fn page_actions_map_to_daemon_requests() {
        assert_eq!(action("tap", &json!({"x": 10.0, "y": 20.5})), Some(Request::Tap { x: 10.0, y: 20.5 }));
        assert_eq!(
            action("swipe", &json!({"x1": 1, "y1": 2, "x2": 3, "y2": 4, "seconds": 9})),
            Some(Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 3.0 })
        );
        assert_eq!(
            action("type", &json!({"text": "hi"})),
            Some(Request::TypeText { text: "hi".into(), submit: false })
        );
        assert_eq!(action("button", &json!({"button": "home"})), Some(Request::PressButton { button: "home".into() }));
        assert_eq!(
            action("tap_element", &json!({"text": "通用", "index": 1})),
            Some(Request::TapElement { text: "通用".into(), index: 1, max_age_ms: 0 })
        );
        assert_eq!(
            action("wait_for", &json!({"text": "x", "timeout": 99})),
            Some(Request::WaitFor { text: "x".into(), timeout_ms: 30_000 })
        );
        assert_eq!(action("launch_app", &json!({"app": " 设置 "})), Some(Request::LaunchApp { app: "设置".into() }));
        assert_eq!(
            action("touch", &json!({"phase": "move", "x": 3, "y": 4.5})),
            Some(Request::Touch { phase: TouchPhase::Move, x: 3.0, y: 4.5 })
        );
        assert_eq!(
            action("touch", &json!({"phase": "prepare"})),
            Some(Request::Touch { phase: TouchPhase::Prepare, x: 0.0, y: 0.0 })
        );
        assert_eq!(action("touch", &json!({"phase": "hover", "x": 1, "y": 1})), None);
        assert_eq!(action("tap", &json!({"x": 1})), None);
        assert_eq!(action("uninstall_app", &json!({"app": "x"})), None);
    }

    #[test]
    fn screen_size_only_for_the_viewers_device() {
        let mut p = ictl_daemon::core::Persisted {
            screen: Some(ictl_device::ScreenSize { width: 932.0, height: 430.0 }),
            screen_udid: Some("AAA".into()),
            ..Default::default()
        };
        assert_eq!(screen_of(&p, "aaa").map(|s| (s.width, s.height)), Some((430.0, 932.0)), "portrait");
        assert!(screen_of(&p, "BBB").is_none(), "another device's size would put taps in the wrong place");
        p.screen_udid = None;
        assert!(screen_of(&p, "BBB").is_some(), "older state without a device");
    }

    #[test]
    fn parses_request_head_and_query() {
        let h = head("GET /stream?t=1 HTTP/1.1\r\nHost: localhost:8200\r\n\r\n");
        assert_eq!((h.method.as_str(), h.path.as_str()), ("GET", "/stream"));
        let h = head("GET /api/elements?filter=%E8%AE%BE+x&system=1 HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(
            (h.path.as_str(), h.query("filter").as_deref(), h.query("system").as_deref()),
            ("/api/elements", Some("设 x"), Some("1"))
        );
        assert_eq!(head("GET /api/apps HTTP/1.1\r\n\r\n").query("system"), None);
        assert_eq!(percent_decode("100%"), "100%");
        let h = head("GET /api/status HTTP/1.1\r\nHost: localhost\r\nX-Ictl-Lang: en\r\n\r\n");
        assert_eq!(h.lang, Some(ictl_l10n::Lang::En));
        assert!(Head::parse(b"POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n").is_none());
    }

    #[test]
    fn the_page_says_its_default_language() {
        assert!(page_in(ictl_l10n::Lang::En).contains("<html lang=\"en\" data-lang=\"en\">"));
        assert!(page_in(ictl_l10n::Lang::Zh).contains("<html lang=\"zh-CN\">"));
    }

    #[test]
    fn status_text_becomes_fields() {
        let v = parse_status(
            "WDA ready: True\nLocked: locked (actions unlock automatically)\nForeground app: com.apple.Preferences\nScreen: 440x956 points\nDaemon: Ready",
        );
        assert_eq!(v["wda_ready"], true);
        assert_eq!(v["locked"], true);
        assert_eq!(v["foreground_id"], "com.apple.Preferences");
        assert_eq!((v["width"].as_f64(), v["height"].as_f64()), (Some(440.0), Some(956.0)));
        assert_eq!(v["route"], "wda");
        assert_eq!(parse_status("WDA ready: True\nLocked: unlocked\n")["locked"], false);
        let down = parse_status("WDA is starting\nreading elements … wait until WDA is back\nDaemon: Starting WDA");
        assert_eq!((down["wda_ready"].as_bool(), down["route"].as_str()), (Some(false), Some("hid")));
        assert!(down["locked"].is_null());
    }

    #[test]
    fn element_lines_become_boxes() {
        let e = parse_element("Button \"通用\" id=\"com.apple.settings.general\" @(220,776) 400x52").unwrap();
        assert_eq!(
            e,
            json!({"type": "Button", "label": "通用", "x": 20.0, "y": 750.0, "width": 400.0, "height": 52.0})
        );
        let v = parse_element("Button \"秒表\" value=\"1\" @(267,904) 108x54").unwrap();
        assert_eq!((v["label"].as_str(), v["x"].as_f64()), (Some("秒表"), Some(213.0)));
        let plain = parse_element("StaticText \"世界时钟\" @(83,140) 126x41").unwrap();
        assert_eq!(plain["label"], "世界时钟");
        let quoted = parse_element("StaticText \"say \"hi\"\" @(10,10) 4x4").unwrap();
        assert_eq!(quoted["label"], "say \"hi\"");
        assert!(parse_element("……另有 4 个未列出,可用 filter 缩小范围").is_none());
        assert!(parse_element("4 more not listed; narrow it down with filter").is_none());
    }
}
