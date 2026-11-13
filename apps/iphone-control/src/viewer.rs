//! `iphone-control viewer`: a page on 127.0.0.1 to watch the phone live, act on it with the mouse, look at its
//! state, apps and elements, and hand tasks to a local AI agent (see [`crate::agent`]).
//!
//! - `GET /` the page, `GET /stream` WDA's MJPEG stream (port 9100 on the phone, over usbmuxd, read-only; repeated
//!   frames dropped), `GET /video` the same frames as H.264 when ffmpeg can encode them ([`crate::video`]), about
//!   1/15 of the traffic.
//! - `GET /api/{info,daemon,status,devices,apps,elements}`: the daemon's answers as JSON for the page; `GET /api/alert`
//!   a system alert on the phone with its buttons (the phone's own video can leave it out, see [`alert`]).
//! - `POST /api/{tap,swipe,long_press,type,button,launch_app,terminate_app,open_url,tap_element,wait_for,select}`
//!   turn into ordinary daemon requests, so they queue with an agent's calls, get auto-unlock and follow the
//!   controlled device, exactly like an MCP tool call.
//! - `/api/agent/…`: available agents, starting a task (optionally limited to some apps), its events (server-sent events), and pause / resume /
//!   take over / hand back / stop / confirm.
//!
//! Protections, same model as `wda-bridge`: it binds to the loopback address only, every request needs a
//! loopback `Host` (against DNS rebinding), every action also needs an `Origin` of this very page and a JSON
//! content type (against other web pages posting to it), and a read whose `Sec-Fetch-Site` says another site
//! sent it is refused (reading elements unlocks the phone). Every request needs the login ([`auth`]), from this
//! machine too. With `--lan` the same port is open to the local network (see [`lan`]): only local-network addresses
//! may connect, and `Host` and `Origin` may also be this machine's own network names.
//!
//! The viewer follows one device, the one controlled when it starts or the one picked on the page: the picture
//! always comes from it, and every action carries its UDID ([`ictl_daemon::protocol::EXPECT_UDID`]), so the daemon
//! refuses an action once it controls another device (after `select` elsewhere, or an unplug in automatic mode)
//! instead of tapping a phone the user isn't looking at. The device handle is looked up again for each stream, so a
//! replug (new usbmuxd id) doesn't break the picture.

pub mod auth;
mod lan;
mod mcp;
mod peer;
pub mod tls;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::bail;
use ictl_daemon::machine::DeviceState;
use ictl_daemon::protocol::{self, Request, Response, TouchPhase};
use serde_json::{Value, json};
use tls::Conn;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    /// Who may reach the page: `127.0.0.1` only, or the local network too (`--lan`)
    access: lan::Access,
    /// The data directory: whether an Apple ID is signed in (signing WDA from the page needs one)
    data_dir: PathBuf,
    /// The page, marked with the viewer's language (the page's default; it can switch)
    page: String,
    socket: PathBuf,
    port: u16,
    /// The device the page shows and acts on
    udid: Mutex<String>,
    feeds: Mutex<HashSet<String>>,
    agents: Arc<Agents>,
    checking: Arc<tokio::sync::Mutex<()>>,
    check_error: Arc<Mutex<Option<String>>>,
    /// Bundle id → display name, for the foreground app
    names: tokio::sync::Mutex<HashMap<String, String>>,
    device_video_allowed: bool,
    /// H.264 for the picture; `None`: MJPEG only
    video: Option<Arc<crate::video::Encoders>>,
    /// How much of each H.264 stream its page has read
    acks: Arc<crate::video::Acks>,
    /// HTTPS on the same port (`--lan`); plain HTTP from the network is then sent there
    tls: Option<Arc<tls::Tls>>,
    /// MCP over HTTP at `/mcp` (`--mcp`, on by default)
    mcp: Option<ictl_mcp::HttpService>,
    /// The login every HTTP request needs ([`auth`]); `None` only in tests
    auth: Option<auth::Auth>,
}

/// A picture handler can be cancelled at any await, including while opening its source.
struct FeedGuard<'a> {
    feeds: &'a Mutex<HashSet<String>>,
    id: String,
}

impl<'a> FeedGuard<'a> {
    fn register(feeds: &'a Mutex<HashSet<String>>, id: String) -> Option<Self> {
        if !feeds.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone()) {
            return None;
        }
        Some(Self { feeds, id })
    }
}

impl Drop for FeedGuard<'_> {
    fn drop(&mut self) {
        self.feeds.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
    }
}

impl Ctx {
    fn udid(&self) -> String {
        self.udid.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// `lan`: also open the page to the local network (`--lan`); `lan_hosts`: more names it answers to there; `tls`:
/// whether and with which certificate it speaks HTTPS; `mcp`: MCP over HTTP at `/mcp`
pub async fn run(
    socket: &Path,
    data_dir: &Path,
    port: u16,
    lan: bool,
    lan_hosts: &[String],
    tls: &tls::Setting,
    mcp: bool,
) -> anyhow::Result<()> {
    // only a first guess: the page follows whichever device the daemon controls (see `info`). Not knowing yet is
    // fine: the daemon may still be starting (started together with this at login), and with several devices
    // connected it is the one that picks
    let udid = match crate::target_device(socket, None).await {
        Ok(d) => d.udid,
        Err(e) => {
            eprintln!(
                "{}",
                ictl_l10n::trf!(
                    "还不知道控制哪台设备（{e}），等守护进程选好后页面自动跟上",
                    "Not sure which device yet ({e}); the page follows once the daemon has picked one"
                )
            );
            String::new()
        }
    };
    let access = if lan { lan::Access::lan(lan_hosts) } else { lan::Access::local() };
    let listener = tokio::net::TcpListener::bind((access.bind_ip(), port)).await?;
    let port = listener.local_addr()?.port();
    let tls = match tls.enabled(access.is_lan()) {
        true => Some(Arc::new(tls.load(data_dir, &access.names())?)),
        false => None,
    };
    let agents = Agents::new(socket)?;
    let auth = auth::Auth::load(data_dir, port)?;
    println!(
        "{}",
        ictl_l10n::trf!(
            "实时画面:http://127.0.0.1:{port}/ (手机 {},Ctrl-C 停止)",
            "Live viewer: http://127.0.0.1:{port}/ (phone {}, Ctrl-C to stop)",
            if udid.is_empty() { ictl_l10n::tr!("跟随守护进程", "whichever the daemon controls") } else { &udid }
        )
    );
    let device_video_allowed = crate::video::device_allowed();
    if device_video_allowed {
        println!(
            "{}",
            ictl_l10n::tr!(
                "画面:iOS 27 及以上直接用手机自己编的视频(HEVC,约 60 帧/秒,本机不编码);浏览器解不了 HEVC 或更早的系统用下面这种",
                "Picture: on iOS 27 and later the phone's own video (HEVC, about 60 fps, nothing encoded here); browsers that can't decode HEVC and older systems get the one below"
            )
        );
    }
    let video = match crate::video::Encoders::detect().await {
        Ok(e) => {
            println!("{}", ictl_l10n::trf!("画面编码:H.264,{}", "Picture: H.264, {}", e.describe()));
            println!(
                "{}",
                ictl_l10n::tr!(
                    "  浏览器不支持 WebCodecs 时自动改用 MJPEG;改设置见 IPHONE_CONTROL_VIEWER_CODEC、IPHONE_CONTROL_VIEWER_QP",
                    "  Browsers without WebCodecs get MJPEG; settings: IPHONE_CONTROL_VIEWER_CODEC, IPHONE_CONTROL_VIEWER_QP"
                )
            );
            Some(e)
        }
        Err(why) => {
            println!("{}", ictl_l10n::trf!("画面编码:MJPEG({why})", "Picture: MJPEG ({why})"));
            None
        }
    };
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
    let scheme = if tls.is_some() { "https" } else { "http" };
    println!(
        "{}",
        ictl_l10n::tr!(
            "登录:浏览器和 /mcp 都要先登录(本机也一样)。iphone-control viewer token 显示令牌和点开就能登录的链接",
            "Login: browsers and /mcp need the token (on this machine too). iphone-control viewer token shows it, with a link that logs in"
        )
    );
    if let Some(t) = &tls {
        if t.made {
            println!(
                "{}",
                ictl_l10n::trf!(
                    "HTTPS:证书是本机生成的({}),SHA-256 指纹 {}。浏览器第一次打开会提示证书不受信任:核对指纹一致后继续访问。想免掉这一步,用 --tls-cert/--tls-key 换成自己的证书",
                    "HTTPS: a certificate made on this machine ({}), SHA-256 fingerprint {}. The first time, the browser warns that it isn't trusted: check that the fingerprint matches, then go on. To skip that, use a certificate of your own (--tls-cert/--tls-key)",
                    t.cert.display(),
                    t.fingerprint
                )
            );
        } else {
            println!(
                "{}",
                ictl_l10n::trf!(
                    "HTTPS:证书 {}(SHA-256 {})",
                    "HTTPS: certificate {} (SHA-256 {})",
                    t.cert.display(),
                    t.fingerprint
                )
            );
        }
    }
    if mcp {
        println!(
            "{}",
            ictl_l10n::trf!(
                "MCP(HTTP):http://127.0.0.1:{port}/mcp;任务里的 agent 在请求头 Iphone-Control-Task-Token 里带上任务口令",
                "MCP (HTTP): http://127.0.0.1:{port}/mcp; a task's agent sends the task's token in the Iphone-Control-Task-Token header"
            )
        );
    }
    if access.is_lan() {
        for a in access.addresses() {
            println!("{}", ictl_l10n::trf!("局域网:{scheme}://{a}:{port}/", "On the network: {scheme}://{a}:{port}/"));
            if mcp {
                println!("{}", ictl_l10n::trf!("  MCP:{scheme}://{a}:{port}/mcp", "  MCP: {scheme}://{a}:{port}/mcp"));
            }
        }
        println!(
            "{}",
            if tls.is_some() {
                ictl_l10n::tr!(
                    "注意:已对局域网开放,传输经 HTTPS 加密。有令牌的人都能看到屏幕、操作手机、用本机的 Claude Code / Codex 执行任务:令牌别外传。",
                    "Note: open to the local network, encrypted with HTTPS. Whoever has the token can watch and control the phone and run tasks with this machine's Claude Code / Codex: keep it to yourself."
                )
            } else {
                ictl_l10n::tr!(
                    "注意:已对局域网开放,但没有 HTTPS(--tls off):令牌和画面都以明文经过局域网。从局域网地址打开时浏览器不提供 WebCodecs,画面是 MJPEG。",
                    "Note: open to the local network without HTTPS (--tls off): the token and the picture cross the network in the clear. Opened at a network address, browsers don't offer WebCodecs, so the picture is MJPEG."
                )
            }
        );
    } else {
        println!(
            "{}",
            ictl_l10n::tr!(
                "注意:只在本机。经反向代理或隧道(frp、cloudflared 等)转发出去的访问在这里也像本机,同样要令牌:令牌别外传。",
                "Note: this machine only. Visits forwarded by a reverse proxy or tunnel (frp, cloudflared…) also look local here and need the token as well: keep it to yourself."
            )
        );
    }
    let ctx = Arc::new(Ctx {
        access,
        socket: socket.to_path_buf(),
        data_dir: data_dir.to_path_buf(),
        port,
        udid: Mutex::new(udid),
        feeds: Mutex::default(),
        agents,
        checking: Arc::default(),
        check_error: Arc::default(),
        names: tokio::sync::Mutex::default(),
        page: page_in(ictl_l10n::lang()),
        device_video_allowed,
        video,
        acks: Arc::default(),
        tls,
        mcp: mcp.then(|| ictl_mcp::http_service(socket.to_path_buf())),
        auth: Some(auth),
    });
    let slots = Arc::new(tokio::sync::Semaphore::new(crate::MAX_CONNECTIONS));
    loop {
        let (client, peer) = listener.accept().await?;
        // open to the network: only from the local network (a machine reachable from the internet isn't operated
        // from there)
        if !lan::peer_allowed(peer.ip()) {
            tracing::warn!("viewer: refused a connection from {}", peer.ip());
            continue;
        }
        // too many open connections (for example many tabs): drop the new one
        let Ok(slot) = slots.clone().try_acquire_owned() else { continue };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let client = match &ctx.tls {
                // the handshake in the connection's own task: a slow client doesn't hold up the others
                Some(t) => match t.accept(client).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::debug!("viewer TLS: {e}");
                        drop(slot);
                        return;
                    }
                },
                None => Conn::Plain(client),
            };
            if let Err(e) = serve(client, &ctx).await {
                tracing::debug!("viewer connection: {e}");
            }
            drop(slot);
        });
    }
}

/// This machine's network addresses `--lan` serves on (for `viewer token`)
pub fn lan_addresses(lan_hosts: &[String]) -> Vec<std::net::Ipv4Addr> {
    lan::Access::lan(lan_hosts).addresses().to_vec()
}

/// The page with `data-lang` set for English, which the page reads as its default language
fn page_in(lang: ictl_l10n::Lang) -> String {
    let html = match lang {
        ictl_l10n::Lang::En => "<html lang=\"en\" data-lang=\"en\" data-viewer>",
        ictl_l10n::Lang::Zh => "<html lang=\"zh-CN\" data-viewer>",
    };
    PAGE.replacen("<html lang=\"zh-CN\">", html, 1)
}

/// One request per connection (`Connection: close`), which keeps the parser small. It is handled in the page's
/// language (`X-Ictl-Lang`), so the daemon's messages match the page
async fn serve(mut client: Conn, ctx: &Ctx) -> anyhow::Result<()> {
    let (head, body) = crate::read_head(&mut client).await?;
    let Some(req) = Head::parse(&head) else {
        return reply(&mut client, 400, "text/plain", b"bad request").await;
    };
    match req.lang {
        Some(l) => ictl_l10n::scope(l, handle(client, ctx, head, req, body)).await,
        None => handle(client, ctx, head, req, body).await,
    }
}

async fn handle(mut client: Conn, ctx: &Ctx, head: Vec<u8>, req: Head, mut body: Vec<u8>) -> anyhow::Result<()> {
    if !ctx.access.host_allowed(&head) || (req.path != "/" && !fetch_site_ok(&req)) {
        return reply(&mut client, 403, "text/plain", b"forbidden").await;
    }
    // HTTPS is on: from the network only HTTPS (plain HTTP stays for this machine)
    let local = client.tcp().peer_addr().is_ok_and(|a| a.ip().to_canonical().is_loopback());
    if ctx.tls.is_some() && !client.is_tls() && !local {
        let Some(to) = https_location(&head, &req, ctx.port) else {
            return reply(&mut client, 400, "text/plain", b"bad request").await;
        };
        return redirect(&mut client, &to).await;
    }
    let Some(pass) = login_gate(&mut client, ctx, &head, &req, &mut body).await? else {
        return Ok(());
    };
    if !pass.may(&req) {
        let m = ictl_l10n::tr!(
            "这是只读令牌：只能看画面、设备状态和任务，不能操作",
            "This token is read-only: it can watch the picture, the device's state and tasks, but not act"
        );
        return json_reply(&mut client, 403, &json!({"ok": false, "message": m})).await;
    }
    let path = req.path.as_str();
    match (req.method.as_str(), path) {
        ("GET", "/") => reply(&mut client, 200, "text/html; charset=utf-8", ctx.page.as_bytes()).await,
        ("GET", "/stream" | "/video") => {
            let _feed = if let Some(id) = req.query("feed").filter(|id| !id.is_empty() && id.len() <= 80) {
                let Some(guard) = FeedGuard::register(&ctx.feeds, id) else {
                    return reply(&mut client, 409, "text/plain", b"duplicate stream").await;
                };
                Some(guard)
            } else {
                None
            };
            picture(client, ctx, &req).await
        }
        ("GET", p) if p.starts_with("/api/agent/tasks/") && p.ends_with("/events") => {
            let id = &p["/api/agent/tasks/".len()..p.len() - "/events".len()];
            match protocol::task_events(&ctx.socket, id).await {
                Ok(stream) => events(&mut client, stream).await,
                Err(m) => json_reply(&mut client, 404, &json!({"message": m})).await,
            }
        }
        ("GET", p) if p.starts_with("/api/") => {
            let v = match &p[5..] {
                // always an answer: the page falls back to its offline demo when this one fails
                "info" => {
                    let mut v =
                        info(ctx).await.unwrap_or_else(|m| json!({"width": null, "height": null, "warning": m}));
                    if let Some(id) = req.query("feed") {
                        v["feed_open"] = json!(ctx.feeds.lock().unwrap_or_else(|p| p.into_inner()).contains(&id));
                    }
                    // who the page is logged in as: a read-only token hides what it can't do
                    if let Some(w) = &pass.who {
                        v["login"] = json!({"name": w.name, "role": w.role.name()});
                    }
                    Ok(v)
                }
                "updates" => updates(ctx).await,
                "daemon" => daemon(ctx).await,
                "status" => status(ctx).await,
                "devices" => devices(ctx).await,
                "apps" => apps(ctx, req.query("system").is_some_and(|v| v == "1")).await,
                "elements" => elements(ctx, &req.query("filter").unwrap_or_default()).await,
                "alert" => alert(ctx).await,
                "agent/backends" => Ok(json!({"backends": agent::backends()})),
                // every task the daemon knows, also those started elsewhere (`iphone-control task run`)
                "agent/tasks" => protocol::task_list(&ctx.socket).await.map(|t| json!({"tasks": t})),
                _ => return reply(&mut client, 404, "text/plain", b"not found").await,
            };
            match v {
                Ok(v) => json_reply(&mut client, 200, &v).await,
                Err(m) => json_reply(&mut client, 502, &json!({"message": m})).await,
            }
        }
        ("GET" | "POST" | "DELETE", "/mcp") if ctx.mcp.is_some() => {
            // a browser says where it comes from: only the page itself. Other clients send no Origin
            if req.origin.as_deref().is_some_and(|o| !ctx.access.origin_allowed(o, ctx.port)) {
                return reply(&mut client, 403, "text/plain", b"forbidden").await;
            }
            if req.method == "POST" && req.content_length.is_none() {
                return reply(&mut client, 411, "text/plain", b"length required").await;
            }
            let len = req.content_length.unwrap_or(0);
            if len > mcp::MAX_BODY {
                return reply(&mut client, 413, "text/plain", b"too large").await;
            }
            read_body(&mut client, &mut body, len).await?;
            let svc = ctx.mcp.as_ref().expect("checked above");
            mcp::serve(svc, &mut client, &head, body).await
        }
        ("POST", p) if p.starts_with("/api/") => {
            if !action_allowed(&req, &ctx.access, ctx.port, pass.bearer()) {
                return reply(&mut client, 403, "text/plain", b"forbidden").await;
            }
            let len = req.content_length.unwrap_or(0);
            if len > MAX_BODY {
                return reply(&mut client, 413, "text/plain", b"too large").await;
            }
            read_body(&mut client, &mut body, len).await?;
            let Ok(b) = serde_json::from_slice::<Value>(&body) else {
                return json_reply(&mut client, 400, &json!({"ok": false, "message": "bad request"})).await;
            };
            // controlling a task: who, for its log, and which local process (the daemon refuses the task's agent)
            let caller = if p.starts_with("/api/agent/tasks/") {
                Caller::of(&client, pass.who.as_ref()).await
            } else {
                Caller::default()
            };
            let (code, v) = post(ctx, &p[5..], &b, req.device.as_deref(), caller).await;
            json_reply(&mut client, code, &v).await
        }
        _ => reply(&mut client, 404, "text/plain", b"not found").await,
    }
}

async fn video_head(client: &mut Conn) -> anyhow::Result<()> {
    let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nCache-Control: no-store\r\n\
                 X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n";
    tokio::time::timeout(crate::video::STREAM_IDLE, client.write_all(head)).await??;
    tokio::time::timeout(crate::video::STREAM_IDLE, client.flush()).await??;
    Ok(())
}

async fn picture(mut client: Conn, ctx: &Ctx, req: &Head) -> anyhow::Result<()> {
    // the device the page shows (`?udid=`), so a page that hasn't noticed a switch keeps its own picture
    // instead of silently showing another phone. Looked up again each time: after a replug usbmuxd gives the
    // device a new id
    let udid = req.query("udid").filter(|u| !u.is_empty()).unwrap_or_else(|| ctx.udid());
    if udid.is_empty() {
        return reply(&mut client, 503, "text/plain", b"no device yet").await;
    }
    if req.path == "/video" && req.query("src").as_deref() == Some("device") {
        if !ctx.device_video_allowed {
            return reply(&mut client, 404, "text/plain", b"turned off by IPHONE_CONTROL_VIEWER_CODEC").await;
        }
        return match crate::video::device::open(&ctx.socket, &udid).await {
            // the page takes 404 as "not this way" and falls back to H.264 or MJPEG
            Err(e) => reply(&mut client, 404, "text/plain; charset=utf-8", e.as_bytes()).await,
            Ok((src, w)) => {
                video_head(&mut client).await?;
                crate::video::device::serve(src, w, ctx.acks.clone(), client).await;
                Ok(())
            }
        };
    }
    let video = if req.path == "/video" {
        match ctx.video.clone().filter(|e| e.label().is_some()) {
            Some(e) => Some(e),
            None => return reply(&mut client, 404, "text/plain", b"no H.264 encoder").await,
        }
    } else {
        None
    };
    let connection = async { ictl_device::Device::find(Some(&udid)).await?.connect_port(MJPEG_PORT).await };
    let mut remote = match tokio::time::timeout(crate::video::STREAM_IDLE, connection).await {
        Ok(Ok(r)) => r,
        _ => return reply(&mut client, 502, "text/plain", b"device feed unavailable").await,
    };
    tokio::time::timeout(crate::video::STREAM_IDLE, remote.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n"))
        .await??;
    match video {
        Some(enc) => {
            video_head(&mut client).await?;
            crate::video::serve(enc, ctx.acks.clone(), remote, client).await;
        }
        None => crate::video::serve_mjpeg(remote, client, crate::video::STREAM_IDLE).await,
    }
    Ok(())
}

/// The release cache belongs to the host user; process versions belong to this Viewer and socket.
async fn updates(ctx: &Ctx) -> Result<Value, String> {
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

/// A POST under `/api/`: status code and JSON answer
/// `device`: the one the page shows (`X-Ictl-Device`); actions only go to it
/// Who called the page's API to control a task
#[derive(Default)]
struct Caller {
    by: String,
    pid: Option<u32>,
}

impl Caller {
    /// `who`: the token it logged in with, by name in the task's log
    async fn of(client: &Conn, who: Option<&auth::Who>) -> Self {
        let (Ok(peer), Ok(local)) = (client.tcp().peer_addr(), client.tcp().local_addr()) else {
            return Self::default();
        };
        // reads every process's open files: not on the runtime's threads
        let pid = tokio::task::spawn_blocking(move || peer::local_client(peer, local)).await.ok().flatten();
        let mut by = who.map_or_else(|| "viewer".to_owned(), |w| format!("viewer:{}", w.name));
        if !peer.ip().to_canonical().is_loopback() {
            by = format!("{by} {}", peer.ip());
        }
        Self { by, pid }
    }
}

async fn post(ctx: &Ctx, name: &str, b: &Value, device: Option<&str>, caller: Caller) -> (u16, Value) {
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
            && let Ok(s) = crate::snapshot(&ctx.socket).await
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

/// The task's events so far, then new ones as they come (from the daemon), as server-sent events, until it is done
async fn events(client: &mut Conn, mut stream: protocol::TaskStream) -> anyhow::Result<()> {
    client
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n\
              X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        )
        .await?;
    client.flush().await?;
    loop {
        tokio::select! {
            e = stream.next() => match e {
                Some(e) => {
                    client.write_all(sse(&e).as_bytes()).await?;
                    client.flush().await?;
                    if e.event == "done" {
                        return Ok(());
                    }
                }
                // the daemon went away (restarting): the page reconnects
                None => return Ok(()),
            },
            // a comment now and then finds a page that went away
            _ = tokio::time::sleep(Duration::from_secs(15)) => {
                client.write_all(b": ping\n\n").await?;
                client.flush().await?;
            }
        }
    }
}

fn sse(e: &protocol::TaskEvent) -> String {
    format!("event: {}\ndata: {}\n\n", e.event, e.data)
}

/// The rest of a request body: `body` holds what came with the head; `len` bytes in all
async fn read_body(client: &mut Conn, body: &mut Vec<u8>, len: usize) -> anyhow::Result<()> {
    while body.len() < len {
        let mut buf = [0u8; 4096];
        let n = tokio::time::timeout(crate::READ_TIMEOUT, client.read(&mut buf)).await??;
        if n == 0 {
            bail!("body cut off");
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(len);
    Ok(())
}

async fn json_reply(client: &mut Conn, status: u16, v: &Value) -> anyhow::Result<()> {
    reply(client, status, "application/json", v.to_string().as_bytes()).await
}

async fn reply(client: &mut Conn, status: u16, kind: &str, body: &[u8]) -> anyhow::Result<()> {
    reply_with(client, status, kind, body, "").await
}

/// [`reply`] with more header lines (`extra`: each ending with `\r\n`)
async fn reply_with(client: &mut Conn, status: u16, kind: &str, body: &[u8], extra: &str) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        303 => "See Other",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        _ => "Bad Gateway",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes()).await?;
    client.write_all(body).await?;
    // over TLS the last record may still be buffered
    client.flush().await?;
    Ok(())
}

/// Who a request is, once [`login_gate`] let it in
#[derive(Default)]
struct Pass {
    /// Logged in with this token; `None` when the viewer has no login (tests)
    who: Option<auth::Who>,
}

impl Pass {
    /// With `Authorization: Bearer`: a program, which needs no `Origin`
    fn bearer(&self) -> bool {
        self.who.as_ref().is_some_and(|w| w.bearer)
    }

    /// A read-only token may look (any `GET` but `/mcp`, and report how much of a video it read), nothing else
    fn may(&self, req: &Head) -> bool {
        match &self.who {
            Some(w) if w.role == auth::Role::View => {
                (req.method == "GET" && req.path != "/mcp") || (req.method == "POST" && req.path == "/api/video/ack")
            }
            _ => true,
        }
    }
}

/// The login ([`auth`]) before anything else: who the request is when it may go on, otherwise `None` and it was
/// answered (the login page, a login, or 401 / 429)
async fn login_gate(
    client: &mut Conn,
    ctx: &Ctx,
    head: &[u8],
    req: &Head,
    body: &mut Vec<u8>,
) -> anyhow::Result<Option<Pass>> {
    use auth::Verdict;
    let Some(a) = &ctx.auth else { return Ok(Some(Pass::default())) };
    let secure = client.is_tls();
    // the link `iphone-control viewer token` shows
    if req.method == "GET"
        && req.path == "/"
        && let Some(t) = req.query("token")
    {
        match a.try_token(&t) {
            Verdict::Ok(w) => logged_in(client, a, &w.name, secure).await?,
            v => login_page(client, v).await?,
        }
        return Ok(None);
    }
    // the login page's form
    if req.method == "POST" && req.path == "/login" {
        if req.origin.as_deref().is_some_and(|o| !ctx.access.origin_allowed(o, ctx.port)) {
            reply(client, 403, "text/plain", b"forbidden").await?;
            return Ok(None);
        }
        let len = req.content_length.unwrap_or(0);
        if len > 4096 {
            reply(client, 413, "text/plain", b"too large").await?;
            return Ok(None);
        }
        read_body(client, body, len).await?;
        match auth::form_token(body).filter(|t| !t.trim().is_empty()).map(|t| a.try_token(&t)) {
            Some(Verdict::Ok(w)) => logged_in(client, a, &w.name, secure).await?,
            Some(v) => login_page(client, v).await?,
            None => login_page(client, Verdict::Missing).await?,
        }
        return Ok(None);
    }
    let v = match a.check(head) {
        Verdict::Ok(w) => return Ok(Some(Pass { who: Some(w) })),
        v => v,
    };
    let status = if v == Verdict::Limited { 429 } else { 401 };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => login_page(client, v).await?,
        // MCP clients: send the token as a bearer token (RFC 6750)
        (_, "/mcp") => {
            let extra = "WWW-Authenticate: Bearer realm=\"iphone-control\"\r\n";
            reply_with(client, status, "text/plain", b"log in: Authorization: Bearer <token>", extra).await?
        }
        _ => reply(client, status, "text/plain", b"log in first").await?,
    }
    Ok(None)
}

/// Logged in: a session cookie, and the page without the token in its address (and history)
async fn logged_in(client: &mut Conn, a: &auth::Auth, name: &str, secure: bool) -> anyhow::Result<()> {
    let extra = format!("Location: /\r\nSet-Cookie: {}\r\n", a.set_cookie(name, secure));
    reply_with(client, 303, "text/plain", b"", &extra).await
}

/// The login page, saying why the last try failed
async fn login_page(client: &mut Conn, v: auth::Verdict) -> anyhow::Result<()> {
    let (status, why) = match v {
        auth::Verdict::Wrong => (401, Some(ictl_l10n::tr!("令牌不对", "That isn't the token"))),
        auth::Verdict::Limited => {
            (429, Some(ictl_l10n::tr!("错误次数太多，一分钟后再试", "Too many wrong tokens; try again in a minute")))
        }
        _ => (401, None),
    };
    reply(client, status, "text/html; charset=utf-8", auth::login_page(why).as_bytes()).await
}

/// Where a plain HTTP request from the network goes: the same page over HTTPS, at the name it asked for (one of
/// this machine's: [`lan::Access::host_allowed`] already checked it)
fn https_location(head: &[u8], req: &Head, port: u16) -> Option<String> {
    let host = lan::host_header(head).filter(|h| !h.is_empty())?;
    let query = if req.query.is_empty() { String::new() } else { format!("?{}", req.query) };
    Some(format!("https://{host}:{port}{}{query}", req.path))
}

/// Temporary (307): with `--tls off` later, browsers mustn't remember it
async fn redirect(client: &mut Conn, to: &str) -> anyhow::Result<()> {
    let head = format!(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: {to}\r\nContent-Length: 0\r\nCache-Control: no-store\r\n\
         Connection: close\r\n\r\n"
    );
    client.write_all(head.as_bytes()).await?;
    client.flush().await?;
    Ok(())
}

struct Head {
    method: String,
    path: String,
    query: String,
    origin: Option<String>,
    content_type: Option<String>,
    content_length: Option<usize>,
    /// `X-Ictl-Device`: the device the page shows; its actions are refused once the daemon controls another
    device: Option<String>,
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
            device: None,
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
                "x-ictl-device" if !v.is_empty() => h.device = Some(v.to_owned()),
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

/// An action must come from this page: `Origin` is the viewer's own address (one of this machine's names with
/// `--lan`), and the body is JSON, which a cross-origin page can't send without a preflight that this server never
/// answers
/// `bearer`: logged in with `Authorization: Bearer` (a program). A browser always sends `Origin` with a POST, and a
/// web page can't make it send that header, so without `Origin` only a program's request is taken
fn action_allowed(req: &Head, access: &lan::Access, port: u16, bearer: bool) -> bool {
    let json = req.content_type.as_deref().is_some_and(|c| c.split(';').next() == Some("application/json"));
    json && match req.origin.as_deref() {
        Some(o) => access.origin_allowed(o, port),
        None => bearer,
    }
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
/// How long a page action may take; signing and installing WDA has its own, longer limit in the daemon
const SHORT: Duration = Duration::from_secs(120);
const SIGN: Duration = Duration::from_secs(16 * 60);

async fn run_action(socket: &Path, req: &Request, udid: Option<&str>, timeout: Duration) -> Value {
    action_result(req, protocol::call_expecting(socket, req, Some(timeout), udid).await)
}

fn action_result(req: &Request, result: Result<Response, impl std::fmt::Display>) -> Value {
    match result {
        // tapping by text that found nothing is an ordinary answer for the agent, a failure for the page
        Ok(Response::Text { text, outcome }) => {
            let missed = matches!(req, Request::TapElement { .. }) && protocol::tapped_nothing(&text, outcome);
            json!({"ok": !missed, "message": text})
        }
        Ok(Response::Image { caption, .. }) => json!({"ok": true, "message": caption}),
        Ok(Response::Error { message, retry_as_gesture }) => json!({"ok": false, "message": message,
            "retry_as_gesture": retry_as_gesture && matches!(req, Request::Touch { phase: TouchPhase::Down, .. })}),
        Err(e) => json!({"ok": false, "message": e.to_string()}),
    }
}

/// One daemon request whose text the page needs as data. `en`: ask in English, whose wording the parsers expect
async fn text_of(socket: &Path, req: Request, en: bool) -> Result<String, String> {
    let call = protocol::call(socket, &req, Some(Duration::from_secs(60)));
    let r = if en { ictl_l10n::scope(ictl_l10n::Lang::En, call).await } else { call.await };
    match r {
        Ok(Response::Text { text, .. }) => Ok(text),
        Ok(Response::Error { message, .. }) => Err(message),
        Ok(Response::Image { caption, .. }) => Ok(caption),
        Err(e) => Err(e.to_string()),
    }
}

/// Screen size in points (portrait) for turning a click on the picture into a tap, and the device the viewer follows.
/// It follows the device the daemon controls: switched on this page, on another page, with `iphone-control select`,
/// or automatically after an unplug. The page notices `udid` changing and moves its picture and panels over; until
/// then its actions carry the old UDID and are refused, never sent to a phone the user isn't looking at
async fn info(ctx: &Ctx) -> Result<Value, String> {
    let s = crate::snapshot(&ctx.socket).await.map_err(|e| e.to_string())?;
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
    json!({
        "udid": d.udid, "name": d.name, "model": d.product_type, "ios": d.product_version,
        "model_name": d.model_name(), "family": d.family(),
    })
}

/// `{ state, last_error?, device: { name, model, model_name, family, ios, udid }, auto_unlock }`
async fn daemon(ctx: &Ctx) -> Result<Value, String> {
    let s = crate::snapshot(&ctx.socket).await.map_err(|e| e.to_string())?;
    let (state, last_error) = state_of(&s.state);
    Ok(json!({
        "state": state,
        "state_text": s.state.describe(),
        "last_error": last_error,
        "device": s.device.as_ref().map(device_json),
        "auto_unlock": s.auto_unlock && !s.unlock_disabled,
        // the page offers to sign and install WDA (NeedsSigning) only with an Apple ID to sign with
        "signed_in": crate::account_dir(&ctx.data_dir).selected_email().await.is_ok(),
    }))
}

/// `{ devices: [{ udid, name, model, model_name, family, ios, controlled }], fixed }`
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
    // the HID fallback needs iOS 27; an older device without WDA can't be operated at all
    let hid = crate::snapshot(&ctx.socket).await.ok().and_then(|s| s.device).is_some_and(|d| d.major_version() >= 27);
    let mut v = parse_status(&text, hid);
    if let Some(id) = v["foreground_id"].as_str().map(str::to_owned) {
        v["foreground"] = app_name(ctx, &id).await.into();
    }
    Ok(v)
}

fn parse_status(text: &str, hid: bool) -> Value {
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
        // without WDA, screenshots, taps and keys go over CoreDevice HID (iOS 27 and later); otherwise nothing works
        "route": if ready { "wda" } else if hid { "hid" } else { "none" },
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

/// A system alert on the phone (a permission prompt and the like): `{ alert: null }` or
/// `{ alert: { text, frame?: { x, y, width, height }, buttons: [{ label, x, y, width }] } }` in points: the alert's
/// box (top left), each button's center and width. The page draws it where the real one is. The phone's own video
/// (iOS 27, D16) can leave such an alert out: a location prompt showed there only as the dimmed screen behind it,
/// while WDA's screenshot had it. The page asks while it shows that video and puts the alert next to the picture.
/// `AlertCheck` is two quick WDA calls (the foreground app; the alert's text only when SpringBoard is in front); the
/// element tree is only read when there is an alert
async fn alert(ctx: &Ctx) -> Result<Value, String> {
    let check = text_of(&ctx.socket, Request::AlertCheck, true).await?;
    let Some(text) = alert_text(&check) else { return Ok(json!({"alert": null})) };
    let req = Request::ListElements { filter: String::new(), limit: ELEMENT_LIMIT, max_age_ms: 0 };
    // without its buttons the page still says there is an alert
    let els: Vec<Value> = text_of(&ctx.socket, req, true)
        .await
        .map(|t| t.lines().filter_map(parse_element).collect())
        .unwrap_or_default();
    let (frame, buttons) = alert_parts(&els);
    Ok(json!({"alert": {"text": text, "frame": frame, "buttons": buttons}}))
}

/// The alert's own text from `AlertCheck`'s English answer (`…a system alert is on screen: “text”, taps…`); `None`
/// when it reports none
fn alert_text(check: &str) -> Option<String> {
    let start = check.find('“')? + '“'.len_utf8();
    let end = check.rfind('”')?;
    (end > start).then(|| check[start..end].trim().to_owned()).filter(|t| !t.is_empty())
}

/// The tree's alert box, and the buttons on it (whose center lies inside it) as `{ label, x, y, width }`
fn alert_parts(els: &[Value]) -> (Value, Vec<Value>) {
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

    fn test_ctx(dir: &Path, socket: &Path) -> Ctx {
        Ctx {
            access: lan::Access::local(),
            agents: Agents::new(socket).unwrap(),
            socket: socket.to_owned(),
            data_dir: dir.into(),
            port: 0,
            page: String::new(),
            checking: Arc::default(),
            check_error: Arc::default(),
            names: Default::default(),
            udid: Default::default(),
            device_video_allowed: true,
            video: None,
            acks: Arc::default(),
            feeds: Mutex::default(),
            tls: None,
            mcp: None,
            auth: None,
        }
    }

    /// One request through [`serve`] on a real connection; the whole answer
    async fn exchange(ctx: &Ctx, raw: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(raw.as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        });
        let (tcp, _) = listener.accept().await.unwrap();
        serve(Conn::Plain(tcp), ctx).await.unwrap();
        client.await.unwrap()
    }

    /// Every request needs the login, from 127.0.0.1 too (a reverse proxy here looks just like it)
    #[tokio::test]
    async fn every_request_needs_the_login() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let mut ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
        let token = "t".repeat(64);
        let entry = |name: &str, role, c: char| auth::Entry {
            name: name.into(),
            role,
            token: c.to_string().repeat(64),
            created: 0,
        };
        ctx.auth = Some(auth::Auth::fixed(
            vec![entry("owner", auth::Role::Operate, 't'), entry("tv", auth::Role::View, 'v')],
            8202,
        ));
        let get = |path: &str, extra: &str| format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:8202\r\n{extra}\r\n");
        let status = |r: &str| r.split(' ').nth(1).unwrap_or_default().to_owned();

        let r = exchange(&ctx, get("/api/agent/backends", "")).await;
        assert_eq!(status(&r), "401", "{r}");
        let r = exchange(&ctx, get("/", "")).await;
        assert!(status(&r) == "401" && r.contains("action=\"/login\""), "the login page: {r}");
        let r = exchange(&ctx, get("/?token=nope", "")).await;
        assert!(status(&r) == "401" && r.contains("class=\"err\""), "{r}");

        // the link: a cookie, and the page without the token
        let r = exchange(&ctx, get(&format!("/?token={token}"), "")).await;
        assert_eq!(status(&r), "303", "{r}");
        assert!(r.contains("\r\nLocation: /\r\n"));
        let cookie = r.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap().split(';').next().unwrap();
        assert!(cookie.starts_with("ictl_viewer_8202=") && !cookie.contains(&token));
        let r = exchange(&ctx, get("/api/agent/backends", &format!("Cookie: {cookie}\r\n"))).await;
        assert_eq!(status(&r), "200", "{r}");
        let r = exchange(&ctx, get("/api/agent/backends", &format!("Authorization: Bearer {token}\r\n"))).await;
        assert_eq!(status(&r), "200", "a program: {r}");

        // the login page's form; not from another site
        let form = |origin: &str| {
            let body = format!("token={token}");
            format!(
                "POST /login HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nContent-Type: application/x-www-form-urlencoded\r\n\
                 Origin: {origin}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let r = exchange(&ctx, form("http://127.0.0.1:0")).await;
        assert!(status(&r) == "303" && r.contains("Set-Cookie: ictl_viewer_8202="), "{r}");
        let r = exchange(&ctx, form("https://evil.example")).await;
        assert_eq!(status(&r), "403", "{r}");

        // MCP clients are told to send a bearer token
        let r = exchange(&ctx, "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nContent-Length: 0\r\n\r\n".into()).await;
        assert!(status(&r) == "401" && r.contains("WWW-Authenticate: Bearer"), "{r}");

        // a read-only token looks but doesn't act
        let tv = format!("Authorization: Bearer {}\r\n", "v".repeat(64));
        let r = exchange(&ctx, get("/api/agent/backends", &tv)).await;
        assert_eq!(status(&r), "200", "{r}");
        let post = |path: &str, auth: &str| {
            format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nContent-Type: application/json\r\n{auth}Content-Length: 2\r\n\r\n{{}}"
            )
        };
        for p in ["/api/button", "/api/agent/tasks", "/api/agent/tasks/abc/pause", "/api/select"] {
            let r = exchange(&ctx, post(p, &tv)).await;
            assert!(status(&r) == "403" && (r.contains("只读") || r.contains("read-only")), "{p}: {r}");
        }
        let r = exchange(&ctx, post("/mcp", &tv)).await;
        assert_eq!(status(&r), "403", "no MCP: its tools act: {r}");
        let r = exchange(&ctx, post("/api/video/ack", &tv)).await;
        assert_ne!(status(&r), "403", "it may say how much of the picture it read: {r}");
        // a program with the operating token needs no Origin (no daemon here: past the checks it can't reach it)
        let r = exchange(&ctx, post("/api/button", &format!("Authorization: Bearer {token}\r\n"))).await;
        assert_ne!(status(&r), "403", "{r}");
        // the same request from a browser's session cookie without Origin is refused
        let r = exchange(&ctx, post("/api/button", &format!("Cookie: {cookie}\r\n"))).await;
        assert_eq!(status(&r), "403", "{r}");
    }

    #[test]
    fn plain_http_from_the_network_goes_to_the_same_page_over_https() {
        let at = |raw: &[u8]| https_location(raw, &Head::parse(raw).unwrap(), 8202);
        assert_eq!(at(b"GET / HTTP/1.1\r\nHost: 192.168.10.100:8202\r\n\r\n").unwrap(), "https://192.168.10.100:8202/");
        assert_eq!(
            at(b"GET /stream?feed=a&udid=b HTTP/1.1\r\nHost: NAS.local:8202\r\n\r\n").unwrap(),
            "https://nas.local:8202/stream?feed=a&udid=b"
        );
        assert_eq!(at(b"GET / HTTP/1.1\r\nHost: [fd00::5]:8202\r\n\r\n").unwrap(), "https://[fd00::5]:8202/");
        assert_eq!(at(b"GET / HTTP/1.1\r\n\r\n"), None);
    }

    #[tokio::test]
    async fn cancelled_picture_unregisters_its_feed() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("daemon.sock");
        let daemon = tokio::net::UnixListener::bind(&socket).unwrap();
        let ctx = test_ctx(temp.path(), &socket);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let (tab, accepted) =
            tokio::join!(tokio::net::TcpStream::connect(listener.local_addr().unwrap()), listener.accept());
        let _tab = tab.unwrap();
        let raw = b"GET /video?src=device&udid=TEST&feed=tab HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (client, _) = accepted.unwrap();
        let mut request = Box::pin(handle(Conn::Plain(client), &ctx, raw.to_vec(), Head::parse(raw).unwrap(), vec![]));
        // The simulated daemon has accepted the request but has not supplied a stream.
        tokio::select! {
            result = &mut request => panic!("picture ended before cancellation: {result:?}"),
            peer = daemon.accept() => { let _peer = peer.unwrap(); }
        }
        assert!(FeedGuard::register(&ctx.feeds, "tab".into()).is_none(), "the first request still owns this ID");
        assert!(ctx.feeds.lock().unwrap().contains("tab"));
        drop(request);
        assert!(!ctx.feeds.lock().unwrap().contains("tab"));
    }

    #[tokio::test]
    async fn update_poll_keeps_polling_if_refresh_finishes_after_cache_read() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("daemon.sock");
        let daemon = tokio::net::UnixListener::bind(&socket).unwrap();
        let ctx = test_ctx(temp.path(), &socket);
        let guard = ctx.checking.lock().await;
        let finish_check = async {
            // The GET has consumed the release cache when its daemon snapshot begins.
            let (peer, _) = daemon.accept().await.unwrap();
            drop(guard);
            drop(peer);
        };
        let (report, ()) =
            tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(updates(&ctx), finish_check) })
                .await
                .unwrap();
        assert_eq!(report.unwrap()["checking"], true);
    }

    fn head(s: &str) -> Head {
        Head::parse(s.as_bytes()).unwrap()
    }

    async fn reply_to_action(daemon: &tokio::net::UnixListener, reply: Response) -> Value {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let (mut peer, _) = daemon.accept().await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut peer).read_line(&mut line).await.unwrap();
        peer.write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes()).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn element_tap_requires_only_an_explicit_device() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("daemon.sock");
        let daemon = tokio::net::UnixListener::bind(&socket).unwrap();
        let ctx = test_ctx(temp.path(), &socket);
        let body = json!({"text": "Settings", "index": 1});
        *ctx.udid.lock().unwrap() = "different-device".into();
        assert_eq!(post(&ctx, "tap_element", &body, None, Caller::default()).await.0, 409);
        assert_eq!(post(&ctx, "tap", &json!({"x": 1, "y": 2}), Some("phone"), Caller::default()).await.0, 409);
        let (result, req) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(
                post(&ctx, "tap_element", &body, Some("phone"), Caller::default()),
                reply_to_action(&daemon, Response::text("ok"))
            )
        })
        .await
        .unwrap();
        assert_eq!(result.1["ok"], true);
        assert_eq!(req["op"], "tap_element");
        assert_eq!(req["expect_udid"], "phone");
        assert_eq!(req["index"], 1);
        assert!(req.get("expect_view").is_none());
    }

    #[tokio::test]
    async fn only_daemon_confirmed_pre_contact_failure_allows_gesture_retry() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("daemon.sock");
        let daemon = tokio::net::UnixListener::bind(&socket).unwrap();
        let ctx = test_ctx(temp.path(), &socket);
        for (phase, daemon_retry, allow) in [("down", false, false), ("down", true, true), ("move", true, false)] {
            let body = json!({"phase":phase, "x":1, "y":2, "view":"portrait"});
            let reply = Response::Error { message: "touch failed".into(), retry_as_gesture: daemon_retry };
            // The result comes directly from the action, with no follow-up status request.
            let ((_, result), req) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(
                    post(&ctx, "touch", &body, Some("phone"), Caller::default()),
                    reply_to_action(&daemon, reply)
                )
            })
            .await
            .unwrap();
            assert_eq!(req["expect_view"], "portrait");
            assert_eq!(req["expect_udid"], "phone");
            assert_eq!(result["retry_as_gesture"], allow);
        }
    }

    #[tokio::test]
    async fn mjpeg_relays_a_frame_and_ends_on_disconnect() {
        let frames = b"HTTP/1.1 200 OK\r\n\r\n--wda\r\nContent-Length: 4\r\n\r\n\xff\xd8\xff\xd9\r\n";
        let (client, mut browser) = tokio::io::duplex(512);
        let mut received = Vec::new();
        let (_, count) = tokio::join!(
            crate::video::serve_mjpeg(&frames[..], client, Duration::from_secs(2)),
            browser.read_to_end(&mut received),
        );
        assert!(count.unwrap() > frames.len());
        assert!(received.windows(4).any(|frame| frame == b"\xff\xd8\xff\xd9"));
    }

    #[tokio::test]
    async fn mjpeg_expires_on_stalled_phone_or_browser() {
        let idle = Duration::from_millis(25);
        for capacity in [1, 512] {
            let (_phone, remote) = tokio::io::duplex(8);
            let (client, _browser) = tokio::io::duplex(capacity);
            tokio::time::timeout(Duration::from_secs(2), crate::video::serve_mjpeg(remote, client, idle))
                .await
                .unwrap();
        }
    }

    #[test]
    fn actions_need_own_origin_and_json() {
        let local = lan::Access::local();
        let ok = "POST /api/tap HTTP/1.1\r\nHost: 127.0.0.1:8200\r\nOrigin: http://127.0.0.1:8200\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n";
        assert!(action_allowed(&head(ok), &local, 8200, false));
        assert!(action_allowed(
            &head(&ok.replace("http://127.0.0.1:8200", "http://localhost:8200")),
            &local,
            8200,
            false
        ));
        // another page, another port, no origin, a form post
        assert!(!action_allowed(
            &head(&ok.replace("http://127.0.0.1:8200", "http://evil.example")),
            &local,
            8200,
            false
        ));
        assert!(!action_allowed(
            &head(&ok.replace("http://127.0.0.1:8200", "http://127.0.0.1:3000")),
            &local,
            8200,
            false
        ));
        assert!(!action_allowed(&head(&ok.replace("Origin: http://127.0.0.1:8200\r\n", "")), &local, 8200, false));
        // a program logged in with Authorization: Bearer sends no Origin; a cookie (a browser) needs it
        assert!(action_allowed(&head(&ok.replace("Origin: http://127.0.0.1:8200\r\n", "")), &local, 8200, true));
        assert!(!action_allowed(
            &head(&ok.replace("http://127.0.0.1:8200", "http://evil.example")),
            &local,
            8200,
            true
        ));
        assert!(!action_allowed(&head(&ok.replace("application/json", "text/plain")), &local, 8200, false));
        assert!(!action_allowed(
            &head(&ok.replace("Origin: http://127.0.0.1:8200", "Origin: null")),
            &local,
            8200,
            false
        ));
        // only open to the network with --lan
        let on_lan = ok.replace("http://127.0.0.1:8200", "http://192.168.10.100:8200");
        assert!(!action_allowed(&head(&on_lan), &local, 8200, false));
        assert!(action_allowed(&head(&on_lan), &lan::Access::with_hosts(["192.168.10.100".into()]), 8200, false));
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
        // the device the page shows: its actions go only there, its picture comes from there
        let h = head("POST /api/tap HTTP/1.1\r\nX-Ictl-Device: 00008150-001A\r\n\r\n");
        assert_eq!(h.device.as_deref(), Some("00008150-001A"));
        assert_eq!(head("POST /api/tap HTTP/1.1\r\nX-Ictl-Device: \r\n\r\n").device, None, "empty means none");
        let h = head("GET /stream?udid=00008150-001A&t=1 HTTP/1.1\r\n\r\n");
        assert_eq!(h.query("udid").as_deref(), Some("00008150-001A"));
        assert!(Head::parse(b"POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n").is_none());
    }

    #[test]
    fn the_page_says_its_default_language() {
        assert!(page_in(ictl_l10n::Lang::En).contains("<html lang=\"en\" data-lang=\"en\" data-viewer>"));
        assert!(page_in(ictl_l10n::Lang::Zh).contains("<html lang=\"zh-CN\" data-viewer>"));
    }

    #[test]
    fn status_text_becomes_fields() {
        let v = parse_status(
            "WDA ready: True\nLocked: locked (actions unlock automatically)\nForeground app: com.apple.Preferences\nScreen: 440x956 points\nDaemon: Ready",
            true,
        );
        assert_eq!(v["wda_ready"], true);
        assert_eq!(v["locked"], true);
        assert_eq!(v["foreground_id"], "com.apple.Preferences");
        assert_eq!((v["width"].as_f64(), v["height"].as_f64()), (Some(440.0), Some(956.0)));
        assert_eq!(v["route"], "wda");
        assert_eq!(parse_status("WDA ready: True\nLocked: unlocked\n", false)["locked"], false);
        let down = "WDA is starting\nreading elements … wait until WDA is back\nDaemon: Starting WDA";
        assert_eq!(parse_status(down, true)["route"], "hid", "iOS 27 and later");
        assert_eq!(parse_status(down, false)["route"], "none", "older: no HID fallback");
        let down = parse_status(down, true);
        assert_eq!(down["wda_ready"], false);
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

    /// The location prompt that the phone's own video didn't show (iPhone 15 Pro Max, iOS 27.0.1)
    #[test]
    fn a_system_alert_with_its_own_buttons() {
        let check = "\nNote: a system alert is on screen: “Allow “Calendar” to use your location?\nYour location is used for travel time alerts.”, taps and swipes land on the alert; first tap a button on the alert with iphone_tap_element";
        assert_eq!(
            alert_text(check).as_deref(),
            Some("Allow “Calendar” to use your location?\nYour location is used for travel time alerts.")
        );
        assert_eq!(alert_text(""), None, "no alert");
        assert_eq!(alert_text("Launched x"), None);
        let tree = [
            "Button \"今天\" id=\"today-button\" @(61,880) 56x38",
            "Alert \"允许“日历”使用你的位置？\" @(215,466) 270x320",
            "StaticText \"允许“日历”使用你的位置？\" @(215,350) 230x44",
            "Button \"允许一次\" @(215,520) 270x44",
            "Button \"使用 App 时允许\" @(215,564) 270x44",
            "Button \"不允许\" @(215,608) 270x44",
        ];
        let els: Vec<Value> = tree.iter().filter_map(|l| parse_element(l)).collect();
        let (frame, b) = alert_parts(&els);
        assert_eq!(frame, json!({"x": 80.0, "y": 306.0, "width": 270.0, "height": 320.0}));
        let labels: Vec<&str> = b.iter().filter_map(|v| v["label"].as_str()).collect();
        assert_eq!(labels, ["允许一次", "使用 App 时允许", "不允许"], "not the app's own button behind it");
        assert_eq!(
            (b[2]["x"].as_f64(), b[2]["y"].as_f64(), b[2]["width"].as_f64()),
            (Some(215.0), Some(608.0), Some(270.0))
        );
        assert_eq!(alert_parts(&els[..1]), (Value::Null, Vec::new()), "no alert in the tree: no box, no buttons");
    }
}
