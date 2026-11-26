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

mod api;
pub mod auth;
mod io;
mod lan;
mod mcp;
mod peer;
mod pipe;
mod routes;
mod server;
mod stream;
pub mod tls;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ictl_daemon::machine::DeviceState;
use ictl_daemon::protocol::{self, Request, Response, TouchPhase};
use serde_json::{Value, json};
use tls::Conn;

use crate::agent::{self, Agents};

/// The control page, put together from `web/` by build.rs
const PAGE: &str = include_str!(concat!(env!("OUT_DIR"), "/viewer.html"));
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
    /// The page's `Content-Security-Policy` ([`csp_for`])
    page_csp: String,
    /// The page gzipped, for browsers that take it: about 90 KB instead of 290 ([`gzip`])
    page_gz: Vec<u8>,
    socket: PathBuf,
    port: u16,
    /// The device the page shows and acts on
    udid: Mutex<String>,
    feeds: Mutex<HashSet<String>>,
    agents: Arc<Agents>,
    checking: Arc<tokio::sync::Mutex<()>>,
    check_error: Arc<Mutex<Option<String>>>,
    /// Bundle id → display name, for the foreground app
    names: tokio::sync::Mutex<api::AppNames>,
    device_video_allowed: bool,
    /// H.264 for the picture; `None`: MJPEG only
    video: Option<Arc<crate::video::Encoders>>,
    /// How much of each H.264 stream its page has read
    acks: Arc<crate::video::Acks>,
    /// Places for pages getting the phone's HEVC transcoded ([`crate::video::transcode::MAX_TRANSCODERS`])
    transcoders: crate::video::transcode::Slots,
    /// The phones' MJPEG streams, one per phone for all its pages
    mjpeg: crate::video::hub::MjpegHub,
    /// HTTPS on the same port (`--lan`); plain HTTP from the network is then sent there
    tls: Option<Arc<tls::Tls>>,
    /// MCP over HTTP at `/mcp` (`--mcp`, on by default)
    mcp: Option<ictl_mcp::HttpService>,
    /// The login every HTTP request needs ([`auth`]); `None` only in tests
    auth: Option<auth::Auth>,
}

/// A page's picture (`?feed=`) registered while its stream runs: the page asks whether it is still open. Dropped with
/// the stream, at any point (it can be cancelled while opening its source too)
struct FeedGuard {
    ctx: Arc<Ctx>,
    id: String,
}

impl FeedGuard {
    fn register(ctx: &Arc<Ctx>, id: String) -> Option<Self> {
        if !ctx.feeds.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone()) {
            return None;
        }
        Some(Self { ctx: ctx.clone(), id })
    }
}

impl Drop for FeedGuard {
    fn drop(&mut self) {
        self.ctx.feeds.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
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
    let udid = match crate::client::target_device(socket, None).await {
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
    let page = page_in(ictl_l10n::lang());
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
        page_csp: csp_for(&page),
        page_gz: gzip(page.as_bytes()),
        page,
        device_video_allowed,
        video,
        acks: Arc::default(),
        transcoders: Default::default(),
        mjpeg: Default::default(),
        tls,
        mcp: mcp.then(|| ictl_mcp::http_service(socket.to_path_buf())),
        auth: Some(auth),
    });
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let busy = Arc::new(tokio::sync::Semaphore::new(BUSY_ANSWERS));
    let mut last_full: Option<Instant> = None;
    let arriving = Arriving::default();
    let mut stop = std::pin::pin!(shutdown_signal());
    loop {
        let accepted = tokio::select! {
            r = listener.accept() => r,
            () = &mut stop => break,
        };
        let (client, peer) = match accepted {
            Ok(c) => c,
            // for example out of file descriptors: the viewer goes on (and retrying at once would only spin)
            Err(e) => {
                tracing::warn!("viewer: accepting a connection failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // open to the network: only from the local network (a machine reachable from the internet isn't operated
        // from there)
        if !lan::peer_allowed(peer.ip()) {
            tracing::warn!("viewer: refused a connection from {}", peer.ip());
            continue;
        }
        // one machine on the network can't take every place with connections that never send a request
        let Some(early) = arriving.enter(peer.ip()) else { continue };
        // too many open connections (for example many tabs): the new one is told so (503), a few at a time, and
        // the event goes to the log now and then
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            if last_full.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                last_full = Some(Instant::now());
                tracing::warn!(
                    "viewer: all {MAX_CONNECTIONS} connections are in use (pictures, task events, /mcp, open pages); new ones get 503"
                );
            }
            if let Ok(permit) = busy.clone().try_acquire_owned() {
                let tls = ctx.tls.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(3), server::answer_busy(client, tls)).await;
                    drop(permit);
                });
            }
            continue;
        };
        let ctx = ctx.clone();
        let room = slots.clone();
        tokio::spawn(async move {
            let client = match &ctx.tls {
                // the handshake in the connection's own task: a slow client doesn't hold up the others
                Some(t) => match tokio::time::timeout(crate::HEAD_TIMEOUT, t.accept(client)).await {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => {
                        tracing::debug!("viewer TLS: {e}");
                        drop(slot);
                        return;
                    }
                    Err(_) => {
                        tracing::debug!("viewer TLS: the handshake took too long");
                        drop(slot);
                        return;
                    }
                },
                None => Conn::Plain(client),
            };
            if let Err(e) = server::serve_conn(client, ctx, Some(early), Some(room)).await {
                tracing::debug!("viewer connection: {e}");
            }
            drop(slot);
        });
    }
    // SIGTERM (the service stopping, an update) or Ctrl-C: no new connections; the agents of the tasks started
    // here are stopped (they run in process groups of their own, which nothing else would end); open pictures and
    // event streams end with the process, and the pages reconnect to the next viewer
    tracing::info!("viewer: stopping");
    // the port is let go first: nothing waits in the backlog while the agents are stopped
    drop(listener);
    ctx.agents.stop_all().await;
    Ok(())
}

/// Open connections at most: a page takes 3 to 5 (picture, task events, the API kept open, `/mcp` for an agent)
const MAX_CONNECTIONS: usize = 64;
/// At most this many connections over [`MAX_CONNECTIONS`] are answered 503 at a time; more are just closed
const BUSY_ANSWERS: usize = 4;

/// Resolves on SIGTERM or SIGINT (never, if they can't be watched)
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// Connections from a machine on the network that haven't sent their request yet (handshake and head), at most
/// [`ARRIVING_PER_PEER`] each. This machine isn't limited: the page's own requests come from it, and so does
/// everything a reverse proxy forwards
#[derive(Default, Clone)]
struct Arriving(Arc<Mutex<std::collections::HashMap<std::net::IpAddr, usize>>>);

const ARRIVING_PER_PEER: usize = 8;

/// One connection counted in [`Arriving`] until it has sent its request
struct Early(Option<(Arriving, std::net::IpAddr)>);

impl Arriving {
    fn enter(&self, ip: std::net::IpAddr) -> Option<Early> {
        let ip = ip.to_canonical();
        if ip.is_loopback() {
            return Some(Early(None));
        }
        let mut m = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let n = m.entry(ip).or_default();
        if *n >= ARRIVING_PER_PEER {
            tracing::debug!("viewer: too many connections from {ip} still sending their request");
            return None;
        }
        *n += 1;
        Some(Early(Some((self.clone(), ip))))
    }
}

impl Drop for Early {
    fn drop(&mut self) {
        if let Some((a, ip)) = self.0.take() {
            let mut m = a.0.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(n) = m.get_mut(&ip) {
                *n -= 1;
                if *n == 0 {
                    m.remove(&ip);
                }
            }
        }
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

/// A page's `Content-Security-Policy`: its own inline scripts only (by their SHA-256, so nothing injected into the page
/// runs), inline styles (the page sets `style` attributes), and nothing from other sites: requests, pictures and the
/// picture stream go to this server; no frames, plugins or `<base>`
fn csp_for(html: &str) -> String {
    use sha2::{Digest, Sha256};
    let scripts: Vec<String> = html
        .split("<script>")
        .skip(1)
        .filter_map(|s| s.split_once("</script>").map(|(body, _)| body))
        .map(|body| format!("'sha256-{}'", base64(&Sha256::digest(body.as_bytes()))))
        .collect();
    format!(
        "default-src 'none'; script-src {}; style-src 'unsafe-inline'; img-src 'self' data: blob:; \
         connect-src 'self'; media-src 'self' blob:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        if scripts.is_empty() { "'none'".to_owned() } else { scripts.join(" ") }
    )
}

/// Standard base64 with padding, for [`csp_for`]'s hashes
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// How long a connection kept open waits for the page's next request (short in tests, which wait it out)
const KEEP_IDLE: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(15) };

/// Free places ([`MAX_CONNECTIONS`]) below which a connection isn't kept open: idle ones would leave none
/// for a new tab or another machine, whose connections are dropped when they are all taken
const KEEP_ROOM: usize = 8;

/// A request whose answer is short, and leaves nothing unread: the page, and the page's API but for task events and
/// opening a task (streams)
fn keepable(req: &Head) -> bool {
    let short = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => true,
        ("GET" | "POST", p) => p.starts_with("/api/") && p != "/api/agent/tasks/open" && task_events_id(p).is_none(),
        _ => false,
    };
    short && !req.close && !req.chunked && req.content_length.unwrap_or(0) <= MAX_BODY
}

/// How often an open connection's token is looked for ([`revoked`])
const TOKEN_RECHECK: Duration = Duration::from_secs(5);

/// Returns once `who`'s token is no longer there
async fn revoked(a: &auth::Auth, who: &auth::Who) {
    loop {
        tokio::time::sleep(TOKEN_RECHECK).await;
        if !a.knows(who) {
            return;
        }
    }
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

async fn post(ctx: &Ctx, name: &str, b: &Value, device: Option<&str>, caller: routes::Caller) -> (u16, Value) {
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

fn sse(e: &protocol::TaskEvent) -> String {
    format!("event: {}\ndata: {}\n\n", e.event, e.data)
}

/// `/api/agent/tasks/<id>/events`: the task's id
fn task_events_id(path: &str) -> Option<&str> {
    path.strip_prefix("/api/agent/tasks/")?.strip_suffix("/events").filter(|id| !id.is_empty() && !id.contains('/'))
}

/// Writing one answer, or one event of a stream, at most this long: a client that reads nothing would hold its
/// connection's place for good (there are [`MAX_CONNECTIONS`])
const WRITE_TIMEOUT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(30) };

/// `data` gzipped (empty if that fails: the page then goes out as it is)
fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(data).and_then(|()| e.finish()).unwrap_or_default()
}

/// Whether an `Accept-Encoding` list takes gzip (RFC 9110 §12.5.3): `gzip` named, or else `*`; either with a `q` that
/// isn't a number above 0 (`q=0`, `q=NaN`) refuses it. `*` stands only for the codings not named, so `gzip;q=0, *`
/// refuses gzip
fn accepts_gzip(v: &str) -> bool {
    let (mut named, mut star) = (None::<bool>, None::<bool>);
    for c in v.split(',') {
        let mut parts = c.split(';').map(str::trim);
        let name = parts.next().unwrap_or("");
        let taken = parts.filter_map(|p| p.split_once('=')).filter(|(k, _)| k.trim().eq_ignore_ascii_case("q")).all(
            |(_, q)| q.trim().parse::<f32>().is_ok_and(|q| q > 0.0), // NaN isn't above 0 either
        );
        // named twice: refused if either refuses
        let slot = if name.eq_ignore_ascii_case("gzip") {
            &mut named
        } else if name == "*" {
            &mut star
        } else {
            continue;
        };
        *slot = Some(slot.unwrap_or(true) && taken);
    }
    named.or(star).unwrap_or(false)
}

/// Who a request is, once [`login_gate`] let it in
#[derive(Default)]
struct Pass {
    /// Logged in with this token; `None` when the viewer has no login (tests), or came in by a task's token
    who: Option<auth::Who>,
    /// Came in by the token of an active task, without a login: an agent, which may only work for its task
    /// ([`TASK_PATHS`])
    agent: bool,
}

/// What an agent may use with its task's token alone: the tools, and ending the task with a summary
const TASK_PATHS: [&str; 2] = ["/mcp", "/api/agent/tasks/finish"];

impl Pass {
    /// A program (`Authorization: Bearer`, or a task's token), which needs no `Origin`: a web page can't make a
    /// browser send either
    fn program(&self) -> bool {
        self.agent || self.who.as_ref().is_some_and(|w| w.bearer)
    }

    /// A read-only token may look (any `GET` but `/mcp`, and report how much of a video it read); an agent only works
    /// for its task
    /// A read-only token: may look, but nothing is done on the phone for it (not even unlocking it)
    fn watches(&self) -> bool {
        self.who.as_ref().is_some_and(|w| w.role == auth::Role::View)
    }

    fn may(&self, req: &Head) -> bool {
        match &self.who {
            Some(w) if w.role == auth::Role::View => {
                (req.method == "GET" && req.path != "/mcp") || (req.method == "POST" && req.path == "/api/video/ack")
            }
            None if self.agent => TASK_PATHS.contains(&req.path.as_str()),
            _ => true,
        }
    }
}

/// Where a plain HTTP request from the network goes: the same page over HTTPS, at the name it asked for (one of
/// this machine's: [`lan::Access::host_allowed`] already checked it)
fn https_location(head: &[u8], req: &Head, port: u16) -> Option<String> {
    let host = lan::host_header(head).filter(|h| !h.is_empty())?;
    let query = if req.query.is_empty() { String::new() } else { format!("?{}", req.query) };
    Some(format!("https://{host}:{port}{}{query}", req.path))
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
    /// [`protocol::TASK_TOKEN_HEADER`]: an agent working for a task
    task_token: Option<String>,
    /// HTTP/1.0, or `Connection: close`: the client closes after the answer
    close: bool,
    /// `Transfer-Encoding`: a body without a length, which this server reads only for `/mcp` ([`read_chunked`])
    chunked: bool,
    /// … and it is just `chunked`
    transfer_chunked_only: bool,
    /// `Accept-Encoding` takes gzip ([`accepts_gzip`])
    gzip: bool,
}

impl Head {
    /// `None` (answered 400) for what isn't a plain HTTP/1.x request: a target that isn't a path (it is put into the
    /// HTTPS redirect's `Location`, [`https_location`]), control characters or a lone CR or LF anywhere (they would
    /// end a header line we write), a second `Host` or `Content-Length`, or a body framed both ways (a request read
    /// two ways is how requests are smuggled past proxies)
    fn parse(head: &[u8]) -> Option<Self> {
        if !clean_head(head) {
            return None;
        }
        let text = std::str::from_utf8(head).ok()?;
        // the blank line that ends the head; any other empty line is a malformed one
        let mut lines = text.trim_end_matches("\r\n").split("\r\n");
        let mut first = lines.next()?.split(' ');
        let method = first.next()?.to_owned();
        let target = first.next()?;
        let close = match first.next()? {
            "HTTP/1.1" => false,
            "HTTP/1.0" => true,
            _ => return None,
        };
        if first.next().is_some()
            || method.is_empty()
            || !method.bytes().all(|b| b.is_ascii_uppercase())
            || !target.starts_with('/')
            || target.starts_with("//")
            // visible ASCII only: a tab or a byte past ASCII went into the redirect's `Location` as it was
            || !target.bytes().all(|b| (0x21..=0x7e).contains(&b))
        {
            return None;
        }
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
            task_token: None,
            close,
            chunked: false,
            transfer_chunked_only: false,
            gzip: false,
        };
        let mut hosts = 0;
        // every `Accept-Encoding` line together: they make one list (RFC 9110 §5.3)
        let mut accept = String::new();
        for l in lines {
            // a line without a colon (a folded continuation, garbage) or a name with blanks (`Content-Length : 1`,
            // which some proxies read and others don't): 400, as RFC 9112 §5.1 asks
            let (k, v) = l.split_once(':')?;
            if k.is_empty() || !k.bytes().all(is_tchar) {
                return None;
            }
            let v = v.trim();
            match k.to_ascii_lowercase().as_str() {
                "host" => hosts += 1,
                "origin" => h.origin = Some(v.to_owned()),
                "content-type" => h.content_type = Some(v.to_ascii_lowercase()),
                // a second length is refused even when it is the same: proxies differ in which one they take
                "content-length" if h.content_length.is_some() => return None,
                "content-length" if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) => return None,
                "content-length" => h.content_length = Some(v.parse().ok()?),
                "sec-fetch-site" => h.fetch_site = Some(v.to_ascii_lowercase()),
                "x-ictl-lang" => h.lang = ictl_l10n::parse(v),
                "accept-encoding" => {
                    accept.push_str(v);
                    accept.push(',');
                }
                "x-ictl-device" if !v.is_empty() => h.device = Some(v.to_owned()),
                "connection" => h.close |= v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")),
                "transfer-encoding" => {
                    // a second Transfer-Encoding line adds codings to the first
                    h.transfer_chunked_only = !h.chunked && v.eq_ignore_ascii_case("chunked");
                    h.chunked = true;
                }
                k if k == protocol::TASK_TOKEN_HEADER && !v.is_empty() => h.task_token = Some(v.to_owned()),
                _ => {}
            }
        }
        if hosts > 1 || (h.chunked && h.content_length.is_some()) {
            return None;
        }
        h.gzip = accepts_gzip(&accept);
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

/// A character of a header name (RFC 9110 §5.6.2 `tchar`)
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// A request head with nothing but printable text, tabs and CRLF line ends: no other control character, no CR or LF
/// on its own
fn clean_head(head: &[u8]) -> bool {
    let mut i = 0;
    while i < head.len() {
        match head[i] {
            b'\r' if head.get(i + 1) == Some(&b'\n') => i += 2,
            b'\t' => i += 1,
            b if b < 0x20 || b == 0x7f => return false,
            _ => i += 1,
        }
    }
    true
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
fn action_allowed(req: &Head, access: &lan::Access, port: u16, tls: bool, bearer: bool) -> bool {
    let json = req.content_type.as_deref().is_some_and(|c| c.split(';').next() == Some("application/json"));
    json && match req.origin.as_deref() {
        Some(o) => access.origin_allowed(o, port, tls),
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
        "tap_element" => Request::TapElement {
            text: text("text")?,
            index: b.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
            max_age_ms: 0,
            scroll: None,
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

/// An action from the page: the person sees the screen ([`protocol::WATCHED`])
async fn run_action(socket: &Path, req: &Request, udid: Option<&str>, timeout: Duration) -> Value {
    action_result(req, protocol::call_watched(socket, req, Some(timeout), udid).await)
}

fn action_result(req: &Request, result: Result<Response, impl std::fmt::Display>) -> Value {
    match result {
        // tapping by text that found nothing is an ordinary answer for the agent, a failure for the page
        Ok(Response::Text { text, outcome, .. }) => {
            let missed = matches!(req, Request::TapElement { .. }) && protocol::tapped_nothing(&text, outcome);
            json!({"ok": !missed, "message": text})
        }
        Ok(Response::Image { caption, .. }) => json!({"ok": true, "message": caption}),
        Ok(Response::Error { message, retry_as_gesture, .. }) => json!({"ok": false, "message": message,
            "retry_as_gesture": retry_as_gesture && matches!(req, Request::Touch { phase: TouchPhase::Down, .. })}),
        Err(e) => json!({"ok": false, "message": e.to_string()}),
    }
}

#[cfg(test)]
mod tests {
    use super::api::*;
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Requests on one connection, as the viewer serves them ([`server::serve_conn`])
    async fn serve(
        conn: Conn,
        ctx: &Arc<Ctx>,
        early: Option<Early>,
        room: Option<Arc<tokio::sync::Semaphore>>,
    ) -> anyhow::Result<()> {
        server::serve_conn(conn, ctx.clone(), early, room).await
    }

    fn test_ctx(dir: &Path, socket: &Path) -> Ctx {
        Ctx {
            access: lan::Access::local(),
            agents: Agents::new(socket).unwrap(),
            socket: socket.to_owned(),
            data_dir: dir.into(),
            port: 0,
            page: String::new(),
            page_csp: String::new(),
            page_gz: Vec::new(),
            checking: Arc::default(),
            check_error: Arc::default(),
            names: Default::default(),
            udid: Default::default(),
            device_video_allowed: true,
            video: None,
            acks: Arc::default(),
            transcoders: Default::default(),
            mjpeg: Default::default(),
            feeds: Mutex::default(),
            tls: None,
            mcp: None,
            auth: None,
        }
    }

    /// One request through [`serve`] on a real connection; the whole answer. It asks to close after the answer, unless
    /// it says otherwise (connections kept open: [`requests_share_a_connection`])
    async fn exchange(ctx: &Arc<Ctx>, raw: String) -> String {
        let raw = if raw.to_ascii_lowercase().contains("\r\nconnection:") {
            raw
        } else {
            raw.replacen("\r\n", "\r\nConnection: close\r\n", 1)
        };
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
        serve(Conn::Plain(tcp), ctx, None, None).await.unwrap();
        client.await.unwrap()
    }

    /// One answer off a connection kept open: its head and body (by `Content-Length`)
    async fn answer(s: &mut tokio::net::TcpStream) -> (String, String) {
        let mut data = Vec::new();
        let mut b = [0u8; 1];
        while !data.ends_with(b"\r\n\r\n") {
            assert_eq!(s.read(&mut b).await.unwrap(), 1, "closed: {}", String::from_utf8_lossy(&data));
            data.push(b[0]);
        }
        let head = String::from_utf8(data).unwrap();
        let len: usize = head
            .lines()
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
            .unwrap();
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).await.unwrap();
        (head, String::from_utf8_lossy(&body).into_owned())
    }

    /// Short requests share one connection, a body and all: no new connection and TLS handshake for each (over Wi-Fi
    /// two round trips more, ten times a second for the picture's reports). `Connection: close`, HTTP/1.0, streams
    /// and a body too large to read close it as before
    #[tokio::test]
    async fn requests_share_a_connection() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ctx = Arc::new(ctx);
        let server = async {
            let (tcp, _) = listener.accept().await.unwrap();
            serve(Conn::Plain(tcp), &ctx, None, None).await.unwrap();
        };
        let client = async {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            let ack = r#"{"stream":"x","bytes":5}"#;
            // a body, then the next request in the same write: each read where it belongs
            let two = format!(
                "POST /api/video/ack HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n{ack}GET /api/agent/backends HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                ack.len()
            );
            s.write_all(two.as_bytes()).await.unwrap();
            let (h1, _) = answer(&mut s).await;
            assert!(h1.contains("Connection: keep-alive"), "{h1}");
            let (h2, b2) = answer(&mut s).await;
            assert!(h2.starts_with("HTTP/1.1 200") && h2.contains("keep-alive") && b2.contains("backends"), "{h2}{b2}");
            // a stream's path (task events) closes, like a request that asks to
            s.write_all(b"GET /api/agent/tasks/abc/events HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
            let (h3, _) = answer(&mut s).await;
            assert!(h3.contains("Connection: close"), "{h3}");
            let mut rest = Vec::new();
            s.read_to_end(&mut rest).await.unwrap();
        };
        tokio::join!(server, client);
        for (req, why) in [
            ("GET /api/agent/backends HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n", "asked to close"),
            ("GET /api/agent/backends HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n", "HTTP/1.0"),
        ] {
            let out = exchange(&ctx, req.to_owned()).await;
            assert!(out.contains("Connection: close") && out.contains("backends"), "{why}: {out}");
        }
    }

    /// `/mcp` takes a chunked body (an MCP client or a proxy that streams the request) and answers a stream chunked;
    /// a body both chunked and with a length is refused
    #[tokio::test]
    async fn mcp_over_http_takes_a_chunked_body() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let mut ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
        let entry = auth::Entry { name: "owner".into(), role: auth::Role::Operate, token: "t".repeat(64), created: 0 };
        ctx.auth = Some(auth::Auth::fixed(vec![entry], 8202));
        ctx.mcp = Some(ictl_mcp::http_service(temp.path().join("none.sock")));
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
        let (a, b) = init.split_at(40);
        let head = |framing: &str| {
            format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\n\
                 Accept: application/json, text/event-stream\r\n{framing}\r\n",
                "t".repeat(64)
            )
        };
        let chunked = format!(
            "{}{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
            head("Transfer-Encoding: chunked\r\n"),
            a.len(),
            b.len()
        );
        let ctx = Arc::new(ctx);
        let r = exchange(&ctx, chunked).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("Transfer-Encoding: chunked\r\n") && r.ends_with("\r\n0\r\n\r\n"), "{r}");
        assert!(r.contains("iphone"), "{r}");
        let both =
            format!("{}{init}", head(&format!("Transfer-Encoding: chunked\r\nContent-Length: {}\r\n", init.len())));
        assert!(exchange(&ctx, both).await.starts_with("HTTP/1.1 400"));
        let gzip = format!("{}0\r\n\r\n", head("Transfer-Encoding: gzip, chunked\r\n"));
        assert!(exchange(&ctx, gzip).await.starts_with("HTTP/1.1 400"));
        // a broken chunk is told so, not just cut off
        let broken = format!("{}zz\r\n{{}}\r\n0\r\n\r\n", head("Transfer-Encoding: chunked\r\n"));
        let r = exchange(&ctx, broken).await;
        assert!(r.starts_with("HTTP/1.1 400"), "{r}");
        // framing that never ends (trailer lines, each short) stops at the limit instead of growing without bound
        let trailers = "X-A: 1\r\n".repeat((mcp::MAX_BODY + 64 * 1024) / 8 + 1);
        let endless = format!("{}0\r\n{trailers}", head("Transfer-Encoding: chunked\r\n"));
        let r = exchange(&ctx, endless).await;
        assert!(r.starts_with("HTTP/1.1 413") || r.starts_with("HTTP/1.1 400"), "refused: {r}");
    }

    /// Before the login nothing is kept open: without a token a machine on the network can't hold the places
    #[tokio::test]
    async fn only_a_logged_in_request_keeps_its_connection() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let mut ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
        let entry = auth::Entry { name: "owner".into(), role: auth::Role::Operate, token: "t".repeat(64), created: 0 };
        ctx.auth = Some(auth::Auth::fixed(vec![entry], 8202));
        let get = |extra: &str| {
            format!("GET /api/agent/backends HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nConnection: keep-alive\r\n{extra}\r\n")
        };
        let ctx = Arc::new(ctx);
        let r = exchange(&ctx, get("")).await;
        assert!(r.starts_with("HTTP/1.1 401") && r.contains("Connection: close"), "{r}");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = async {
            let (tcp, _) = listener.accept().await.unwrap();
            serve(Conn::Plain(tcp), &ctx, None, None).await.unwrap();
        };
        let client = async {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(get(&format!("Authorization: Bearer {}\r\n", "t".repeat(64))).as_bytes()).await.unwrap();
            let (h, _) = answer(&mut s).await;
            assert!(h.starts_with("HTTP/1.1 200") && h.contains("Connection: keep-alive"), "{h}");
        };
        tokio::join!(server, client);
    }

    /// A connection kept open that sends nothing more is closed after [`KEEP_IDLE`]: an idle one holds one of the
    /// [`MAX_CONNECTIONS`] places
    #[tokio::test]
    async fn an_idle_kept_connection_closes() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ctx = Arc::new(ctx);
        let server = async {
            let (tcp, _) = listener.accept().await.unwrap();
            let t0 = tokio::time::Instant::now();
            serve(Conn::Plain(tcp), &ctx, None, None).await.unwrap();
            t0.elapsed()
        };
        let client = async {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(b"GET /api/agent/backends HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
            answer(&mut s).await;
            let mut rest = Vec::new();
            s.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty());
        };
        let (open, ()) = tokio::join!(server, client);
        assert!(open >= KEEP_IDLE && open < KEEP_IDLE + Duration::from_secs(2), "{open:?}");
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

        let ctx = Arc::new(ctx);
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

        // refused before its body was read: the client still gets the answer, not a reset
        let big = "x".repeat(256 * 1024);
        let r = exchange(
            &ctx,
            format!("POST /api/button HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nContent-Length: {}\r\n\r\n{big}", big.len()),
        )
        .await;
        assert_eq!(status(&r), "401", "{}", &r[..r.len().min(200)]);

        // MCP clients are told to send a bearer token
        let r = exchange(&ctx, "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nContent-Length: 0\r\n\r\n".into()).await;
        assert!(status(&r) == "401" && r.to_ascii_lowercase().contains("www-authenticate: bearer"), "{r}");

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

    /// An agent on another machine has its task's token and nothing else: the tools and ending its task, no more
    #[tokio::test]
    async fn an_agent_works_for_its_task_by_its_token_alone() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("daemon.sock");
        let daemon = tokio::net::UnixListener::bind(&socket).unwrap();
        // a daemon that knows one active task, with the token `good`
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut peer, _)) = daemon.accept().await {
                let mut line = String::new();
                BufReader::new(&mut peer).read_line(&mut line).await.unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                let reply = if req["task"] == "good" { Response::text("t1") } else { Response::error("no such task") };
                log.lock().unwrap().push(req);
                let _ = peer.write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes()).await;
            }
        });
        let mut ctx = test_ctx(temp.path(), &socket);
        let entry = auth::Entry { name: "owner".into(), role: auth::Role::Operate, token: "o".repeat(64), created: 0 };
        ctx.auth = Some(auth::Auth::fixed(vec![entry], 8202));
        ctx.mcp = Some(ictl_mcp::http_service(socket.clone()));
        let status = |r: &str| r.split(' ').nth(1).unwrap_or_default().to_owned();
        let post = |path: &str, token: &str, body: &str| {
            format!(
                "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nContent-Type: application/json\r\n\
                 Accept: application/json, text/event-stream\r\nIphone-Control-Task-Token: {token}\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
        let ctx = Arc::new(ctx);
        let r = exchange(&ctx, post("/mcp", "good", init)).await;
        assert_eq!(status(&r), "200", "the tools: {r}");
        let r = exchange(&ctx, post("/mcp", "ended", init)).await;
        assert!(status(&r) == "401" && r.to_ascii_lowercase().contains("www-authenticate: bearer"), "{r}");
        let r = exchange(&ctx, post("/api/agent/tasks/finish", "good", r#"{"ok":true,"summary":"done"}"#)).await;
        assert_eq!(status(&r), "200", "{r}");
        let finish = seen.lock().unwrap().iter().find(|q| q["op"] == "task_finish").cloned().unwrap();
        assert_eq!((finish["task"].as_str(), finish["summary"].as_str()), (Some("good"), Some("done")));
        // nothing else: not the page's API, not controlling its task, not opening another
        for p in ["/api/agent/tasks/t1/confirm", "/api/agent/tasks/t1/resume", "/api/agent/tasks/open", "/api/button"] {
            let r = exchange(&ctx, post(p, "good", "{}")).await;
            assert!(matches!(status(&r).as_str(), "401" | "403"), "{p}: {r}");
        }
        let r = exchange(
            &ctx,
            "GET /api/info HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nIphone-Control-Task-Token: good\r\n\r\n".into(),
        )
        .await;
        assert_eq!(status(&r), "401", "{r}");
        assert!(
            !seen.lock().unwrap().iter().any(|q| q["op"] == "task_control"),
            "nothing reached the daemon's control"
        );
    }

    /// `iphone-control mcp --url`: stdio on one side, this viewer's /mcp on the other; with the login, and with a
    /// task's token alone (which lists the confirm tool)
    #[tokio::test]
    async fn a_stdio_client_reaches_mcp_over_http_through_the_bridge() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("daemon.sock");
        let daemon = tokio::net::UnixListener::bind(&socket).unwrap();
        // a daemon that knows one active task, with the token `good`
        tokio::spawn(async move {
            while let Ok((mut peer, _)) = daemon.accept().await {
                let mut line = String::new();
                BufReader::new(&mut peer).read_line(&mut line).await.unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                let reply = if req["task"] == "good" { Response::text("t1") } else { Response::error("no such task") };
                let _ = peer.write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes()).await;
            }
        });
        let mut ctx = test_ctx(temp.path(), &socket);
        let login = "o".repeat(64);
        let entry = auth::Entry { name: "owner".into(), role: auth::Role::Operate, token: login.clone(), created: 0 };
        ctx.auth = Some(auth::Auth::fixed(vec![entry], 8202));
        ctx.mcp = Some(ictl_mcp::http_service(socket.clone()));
        let ctx = Arc::new(ctx);
        let ctx = Arc::new(ctx);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = ctx.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let ctx = served.clone();
                tokio::spawn(async move { serve(Conn::Plain(tcp), &ctx, None, None).await });
            }
        });

        for (login, task, confirm) in [(Some(login), None, false), (None, Some("good".to_owned()), true)] {
            let remote = crate::remote::Remote::new(&url, None, login).unwrap();
            let (mut to_bridge, from_client) = tokio::io::duplex(1 << 16);
            let (to_client, from_bridge) = tokio::io::duplex(1 << 16);
            let bridge = tokio::spawn(crate::mcp_bridge::bridge(remote, task, BufReader::new(from_client), to_client));
            let mut answers = BufReader::new(from_bridge).lines();
            let mut say = async |line: &str| to_bridge.write_all(format!("{line}\n").as_bytes()).await.unwrap();
            say(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#).await;
            let a: Value = serde_json::from_str(&answers.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(a["id"], 1, "{a}");
            assert!(a["result"]["serverInfo"].is_object(), "{a}");
            say(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
            say(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).await;
            let a: Value = serde_json::from_str(&answers.next_line().await.unwrap().unwrap()).unwrap();
            let names: Vec<&str> =
                a["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
            assert!(names.contains(&"iphone_tap"), "{names:?}");
            assert_eq!(names.contains(&"iphone_confirm"), confirm, "the confirm tool only in a task");
            // a broken line gets an error instead of silence
            say("not json").await;
            let a: Value = serde_json::from_str(&answers.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(a["error"]["code"], -32700);
            drop(to_bridge);
            bridge.await.unwrap().unwrap();
        }

        // a wrong login: the request gets an error, not a hang
        let remote = crate::remote::Remote::new(&url, None, Some("nope".into())).unwrap();
        let (mut to_bridge, from_client) = tokio::io::duplex(1 << 16);
        let (to_client, from_bridge) = tokio::io::duplex(1 << 16);
        let bridge = tokio::spawn(crate::mcp_bridge::bridge(remote, None, BufReader::new(from_client), to_client));
        to_bridge.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"initialize\",\"params\":{}}\n").await.unwrap();
        let line = BufReader::new(from_bridge).lines().next_line().await.unwrap().unwrap();
        let a: Value = serde_json::from_str(&line).unwrap();
        assert!(
            a["id"] == 7 && a["error"]["message"].as_str().is_some_and(|m| m.contains("登录") || m.contains("login")),
            "{a}"
        );
        drop(to_bridge);
        bridge.await.unwrap().unwrap();
    }

    #[test]
    fn the_task_events_route_takes_an_id() {
        assert_eq!(task_events_id("/api/agent/tasks/0a1b2c/events"), Some("0a1b2c"));
        // starts with the prefix and ends with the suffix, overlapping: no id (this used to panic)
        assert_eq!(task_events_id("/api/agent/tasks/events"), None);
        assert_eq!(task_events_id("/api/agent/tasks//events"), None);
        assert_eq!(task_events_id("/api/agent/tasks/a/b/events"), None);
        assert_eq!(task_events_id("/api/agent/tasks/abc"), None);
    }

    #[test]
    fn a_machine_on_the_network_has_a_few_connections_arriving_at_a_time() {
        let a = Arriving::default();
        let lan: std::net::IpAddr = "192.168.1.9".parse().unwrap();
        let other: std::net::IpAddr = "192.168.1.10".parse().unwrap();
        let held: Vec<_> = (0..ARRIVING_PER_PEER).map(|_| a.enter(lan).unwrap()).collect();
        assert!(a.enter(lan).is_none());
        assert!(a.enter(other).is_some(), "another machine isn't held up");
        let local: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let many: Vec<_> = (0..100).map(|_| a.enter(local)).collect();
        assert!(many.iter().all(Option::is_some), "this machine isn't limited");
        drop(held);
        assert!(a.enter(lan).is_some(), "once they sent their requests");
        assert!(a.0.lock().unwrap().get(&lan).is_none_or(|n| *n == 0));
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

    /// An answer to a client that reads nothing gives up after [`WRITE_TIMEOUT`] instead of holding the connection
    #[tokio::test]
    async fn a_client_that_reads_nothing_is_let_go() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let mut ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
        ctx.page = "x".repeat(32 << 20);
        let ctx = Arc::new(ctx);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let (client, accepted) =
            tokio::join!(tokio::net::TcpStream::connect(listener.local_addr().unwrap()), listener.accept());
        let mut never_reads = client.unwrap();
        never_reads.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
        let t0 = std::time::Instant::now();
        let r =
            tokio::time::timeout(Duration::from_secs(10), serve(Conn::Plain(accepted.unwrap().0), &ctx, None, None))
                .await;
        assert!(r.is_ok(), "let go");
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    }

    /// The page goes out gzipped to a browser that takes it (about a third of the size), as it is otherwise
    #[test]
    fn the_page_is_gzipped_for_browsers_that_take_it() {
        for (v, want) in [
            ("gzip, deflate, br, zstd", true),
            ("GZIP", true),
            ("br;q=1.0, gzip;q=0.8", true),
            ("*", true),
            ("gzip;q=0", false),
            ("gzip; q=0.0, br", false),
            ("deflate, br", false),
            ("", false),
            ("x-gzip", false),
            ("gzip;q=0, *", false),
            ("*;q=0, gzip", true),
            ("gzip;q=NaN", false),
            ("gzip;q=abc", false),
            ("gzip, gzip;q=0", false),
        ] {
            assert_eq!(accepts_gzip(v), want, "{v}");
        }
        let page = page_in(ictl_l10n::Lang::En);
        let gz = gzip(page.as_bytes());
        assert!(gz.len() * 2 < page.len(), "{} of {}", gz.len(), page.len());
        let mut back = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&gz[..]), &mut back).unwrap();
        assert_eq!(back, page);
        let head = |accept: &str| {
            Head::parse(format!("GET / HTTP/1.1\r\nHost: a\r\nAccept-Encoding: {accept}\r\n").as_bytes()).unwrap()
        };
        assert!(head("gzip").gzip && !head("identity").gzip);
        // two lines make one list: the explicit refusal on the second wins over the first line's `*`
        let two =
            Head::parse(b"GET / HTTP/1.1\r\nHost: a\r\nAccept-Encoding: *\r\nAccept-Encoding: gzip;q=0\r\n").unwrap();
        assert!(!two.gzip);
    }

    /// The pages run their own inline scripts only: the policy names each by its hash, and a script changed or added
    /// by someone else matches none
    #[test]
    fn pages_allow_only_their_own_scripts() {
        use sha2::{Digest, Sha256};
        for (input, want) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(input.as_bytes()), want);
        }
        let page = page_in(ictl_l10n::Lang::En);
        let csp = csp_for(&page);
        let script = page.split("<script>").nth(1).unwrap().split("</script>").next().unwrap();
        let hash = format!("'sha256-{}'", base64(&Sha256::digest(script.as_bytes())));
        assert!(csp.contains(&format!("script-src {hash};")), "{csp}");
        assert!(csp.starts_with("default-src 'none';") && csp.contains("frame-ancestors 'none'"), "{csp}");
        let login = auth::login_page(None);
        assert_eq!(csp_for(&login).matches("'sha256-").count(), 2, "the login page's two scripts");
        assert!(!csp_for(&page.replace("'use strict';", "'use strict';alert(1);")).contains(&hash));
        assert!(csp_for("<p>no script</p>").contains("script-src 'none'"));
    }

    /// The redirect's `Location` is built from the request target: only a path is a target, so it can't send a
    /// browser to another host (`@evil.example/` would make `https://host:port@evil.example/`) nor add a header line
    #[test]
    fn only_a_plain_request_head_is_taken() {
        let ok = |s: &str| Head::parse(s.as_bytes()).is_some();
        assert!(ok("GET /stream?x=1 HTTP/1.1\r\nHost: a\r\n"));
        assert!(ok("POST /api/tap HTTP/1.0\r\nHost: a\r\nContent-Length: 2\r\nX-Pad:\tx\r\n"));
        for bad in [
            "GET @evil.example/ HTTP/1.1\r\nHost: a\r\n",
            "GET //evil.example/ HTTP/1.1\r\nHost: a\r\n",
            "GET http://evil.example/ HTTP/1.1\r\nHost: a\r\n",
            "GET /a\nLocation: x HTTP/1.1\r\nHost: a\r\n",
            "GET /a HTTP/1.1\r\nHost: a\nSet-Cookie: x\r\n",
            "GET /a HTTP/1.1\r\nHost: a\rX: y\r\n",
            "GET /a HTTP/1.1\r\nHost: a\x00\r\n",
            "GET /a HTTP/2\r\nHost: a\r\n",
            "GET /a HTTP/1.1 extra\r\nHost: a\r\n",
            "get /a HTTP/1.1\r\nHost: a\r\n",
            "GET /a HTTP/1.1\r\nHost: a\r\nHost: b\r\n",
            "POST /a HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 2\r\n",
            "POST /a HTTP/1.1\r\nContent-Length: +2\r\n",
            "POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 2\r\n",
            // a tab or a byte past ASCII in the target (it goes into the redirect's `Location`)
            "GET /\tfoo HTTP/1.1\r\nHost: a\r\n",
            "GET /a\tHTTP/1.1 HTTP/1.1\r\nHost: a\r\n",
            "GET /caf\u{e9} HTTP/1.1\r\nHost: a\r\n",
            // blanks in a header's name, which proxies read differently
            "POST /a HTTP/1.1\r\nContent-Length : 1\r\n",
            "POST /mcp HTTP/1.1\r\nTransfer-Encoding : chunked\r\n",
            "GET /a HTTP/1.1\r\n Host: a\r\n",
            // a line without a colon: a folded continuation, or garbage
            "GET /a HTTP/1.1\r\nHost: a\r\nX-A: 1\r\n folded\r\n",
            "GET /a HTTP/1.1\r\nHost: a\r\nnocolon\r\n",
            "GET /a HTTP/1.1\r\n\r\nHost: a\r\n",
            "GET /a HTTP/1.1\r\n: empty name\r\n",
        ] {
            assert!(!ok(bad), "{bad:?}");
        }
        assert!(ok("GET /a HTTP/1.1\r\nHost: a\r\nX-Ok: v\r\n\r\n"), "the blank line that ends the head");
    }

    /// A connection counts against its machine's limit until the login passes, not only until its head is in; a
    /// chunked body outside `/mcp` is refused (411) rather than taken for an empty one
    #[tokio::test]
    async fn the_place_is_kept_until_the_login_passes() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let mut ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
        let token = "t".repeat(64);
        let entry = auth::Entry { name: "owner".into(), role: auth::Role::Operate, token: token.clone(), created: 0 };
        ctx.auth = Some(auth::Auth::fixed(vec![entry], 8202));
        let ctx = Arc::new(ctx);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let arriving = Arriving::default();
        let lan: std::net::IpAddr = "192.168.1.9".parse().unwrap();
        let counted = || arriving.0.lock().unwrap().get(&lan).copied().unwrap_or(0);
        // a request on a connection of its own, counted as arriving from `lan`; the answer's head, and whether the
        // connection was still counted when it came (it stays open: closing lingers, a kept one waits)
        let run = async |raw: &str| {
            let (tab, accepted) =
                tokio::join!(tokio::net::TcpStream::connect(listener.local_addr().unwrap()), listener.accept());
            let mut tab = tab.unwrap();
            let early = arriving.enter(lan);
            let server = tokio::spawn(server::serve_conn(Conn::Plain(accepted.unwrap().0), ctx.clone(), early, None));
            tab.write_all(raw.as_bytes()).await.unwrap();
            let head = answer(&mut tab).await.0;
            let still = counted();
            drop(tab);
            let _ = server.await;
            (head, still)
        };
        let (h, still) = run("GET /api/daemon HTTP/1.1\r\nHost: 127.0.0.1:8202\r\n\r\n").await;
        assert!(h.starts_with("HTTP/1.1 401"), "{h}");
        assert_eq!(still, 1, "not logged in: still counted");
        let (h, still) =
            run("POST /api/tap HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n").await;
        assert!(h.starts_with("HTTP/1.1 411"), "{h}");
        assert_eq!(still, 1);
        let logged = format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nAuthorization: Bearer {token}\r\n\r\n");
        let (h, still) = run(&logged).await;
        assert!(h.starts_with("HTTP/1.1 200"), "{h}");
        assert_eq!(still, 0, "logged in: the place is let go");
    }

    /// Without a login nothing of a body is waited for: a request that may keep its connection open used to read
    /// its whole body before the login, so a body that never came held one of the places for the read timeout
    #[tokio::test]
    async fn a_body_is_not_waited_for_before_the_login() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let mut ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
        let entry = auth::Entry { name: "owner".into(), role: auth::Role::Operate, token: "t".repeat(64), created: 0 };
        ctx.auth = Some(auth::Auth::fixed(vec![entry], 8202));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ctx = Arc::new(ctx);
        let server = async {
            let (tcp, _) = listener.accept().await.unwrap();
            let _ = serve(Conn::Plain(tcp), &ctx, None, None).await;
        };
        let client = async {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            let req = "POST /api/tap HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nConnection: keep-alive\r\n\
                       Content-Type: application/json\r\nContent-Length: 4000\r\n\r\n{";
            s.write_all(req.as_bytes()).await.unwrap();
            let (h, _) = tokio::time::timeout(Duration::from_secs(2), answer(&mut s)).await.expect("answered at once");
            assert!(h.starts_with("HTTP/1.1 401"), "{h}");
        };
        tokio::join!(server, client);
    }

    /// A page that goes away while its picture is still being opened lets go of its feed: a new page may take it
    #[tokio::test]
    async fn cancelled_picture_unregisters_its_feed() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("daemon.sock");
        let daemon = tokio::net::UnixListener::bind(&socket).unwrap();
        let ctx = Arc::new(test_ctx(temp.path(), &socket));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let (tab, accepted) =
            tokio::join!(tokio::net::TcpStream::connect(listener.local_addr().unwrap()), listener.accept());
        let mut tab = tab.unwrap();
        tab.write_all(b"GET /video?src=device&udid=TEST&feed=tab HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
        let server = tokio::spawn(server::serve_conn(Conn::Plain(accepted.unwrap().0), ctx.clone(), None, None));
        // The simulated daemon has accepted the request but has not supplied a stream.
        let _peer = tokio::time::timeout(Duration::from_secs(5), daemon.accept()).await.unwrap().unwrap();
        assert!(FeedGuard::register(&ctx, "tab".into()).is_none(), "the first request still owns this ID");
        assert!(ctx.feeds.lock().unwrap().contains("tab"));
        drop(tab);
        tokio::time::timeout(Duration::from_secs(5), async {
            while ctx.feeds.lock().unwrap().contains("tab") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the feed let go once the page went away");
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    }

    #[tokio::test]
    async fn update_poll_keeps_polling_if_refresh_finishes_after_cache_read() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = temp.path().join("daemon.sock");
        let daemon = tokio::net::UnixListener::bind(&socket).unwrap();
        let ctx = test_ctx(temp.path(), &socket);
        let ctx = Arc::new(ctx);
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
        let ctx = Arc::new(ctx);
        *ctx.udid.lock().unwrap() = "different-device".into();
        assert_eq!(post(&ctx, "tap_element", &body, None, routes::Caller::default()).await.0, 409);
        assert_eq!(post(&ctx, "tap", &json!({"x": 1, "y": 2}), Some("phone"), routes::Caller::default()).await.0, 409);
        let (result, req) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(
                post(&ctx, "tap_element", &body, Some("phone"), routes::Caller::default()),
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
        let ctx = Arc::new(ctx);
        for (phase, daemon_retry, allow) in [("down", false, false), ("down", true, true), ("move", true, false)] {
            let body = json!({"phase":phase, "x":1, "y":2, "view":"portrait"});
            let reply =
                Response::Error { message: "touch failed".into(), retry_as_gesture: daemon_retry, failure: None };
            // The result comes directly from the action, with no follow-up status request.
            let ((_, result), req) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(
                    post(&ctx, "touch", &body, Some("phone"), routes::Caller::default()),
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

    /// build.rs put the page together whole: every include expanded, one stylesheet and one script, in order
    #[test]
    fn the_embedded_page_is_assembled() {
        assert!(!PAGE.lines().any(|l| l.starts_with("@include ")));
        assert!(PAGE.starts_with("<!doctype html>") && PAGE.trim_end().ends_with("</html>"));
        assert_eq!((PAGE.matches("<style>").count(), PAGE.matches("<script>").count()), (1, 1));
        // the first script's first statement, so the whole page script stays strict
        assert!(PAGE.split("<script>").nth(1).is_some_and(|s| s.trim_start().starts_with("'use strict';")));
    }

    #[tokio::test]
    async fn mjpeg_relays_a_frame_and_ends_on_disconnect() {
        let frames = b"HTTP/1.1 200 OK\r\n\r\n--wda\r\nContent-Length: 4\r\n\r\n\xff\xd8\xff\xd9\r\n";
        let (client, mut browser) = tokio::io::duplex(512);
        let mut received = Vec::new();
        let hub = crate::video::hub::MjpegHub::with_idle(Duration::from_secs(2));
        let src = hub.subscribe("A", || async { anyhow::Ok(&frames[..]) }).await.unwrap();
        let (_, count) = tokio::join!(
            crate::video::serve_mjpeg(src, client, Duration::from_secs(2)),
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
            let hub = crate::video::hub::MjpegHub::with_idle(idle);
            let remote = hub.subscribe("A", || async { anyhow::Ok(remote) }).await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), crate::video::serve_mjpeg(remote, client, idle))
                .await
                .unwrap();
        }
    }

    #[test]
    fn actions_need_own_origin_and_json() {
        let local = lan::Access::local();
        let ok = "POST /api/tap HTTP/1.1\r\nHost: 127.0.0.1:8200\r\nOrigin: http://127.0.0.1:8200\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n";
        assert!(action_allowed(&head(ok), &local, 8200, false, false));
        assert!(action_allowed(
            &head(&ok.replace("http://127.0.0.1:8200", "http://localhost:8200")),
            &local,
            8200,
            false,
            false
        ));
        // another page, another port, no origin, a form post
        assert!(!action_allowed(
            &head(&ok.replace("http://127.0.0.1:8200", "http://evil.example")),
            &local,
            8200,
            false,
            false
        ));
        assert!(!action_allowed(
            &head(&ok.replace("http://127.0.0.1:8200", "http://127.0.0.1:3000")),
            &local,
            8200,
            false,
            false
        ));
        assert!(!action_allowed(
            &head(&ok.replace("Origin: http://127.0.0.1:8200\r\n", "")),
            &local,
            8200,
            false,
            false
        ));
        // a program logged in with Authorization: Bearer sends no Origin; a cookie (a browser) needs it
        assert!(action_allowed(&head(&ok.replace("Origin: http://127.0.0.1:8200\r\n", "")), &local, 8200, false, true));
        assert!(!action_allowed(
            &head(&ok.replace("http://127.0.0.1:8200", "http://evil.example")),
            &local,
            8200,
            false,
            true
        ));
        assert!(!action_allowed(&head(&ok.replace("application/json", "text/plain")), &local, 8200, false, false));
        assert!(!action_allowed(
            &head(&ok.replace("Origin: http://127.0.0.1:8200", "Origin: null")),
            &local,
            8200,
            false,
            false
        ));
        // only open to the network with --lan
        let on_lan = ok.replace("http://127.0.0.1:8200", "http://192.168.10.100:8200");
        assert!(!action_allowed(&head(&on_lan), &local, 8200, false, false));
        assert!(action_allowed(
            &head(&on_lan),
            &lan::Access::with_hosts(["192.168.10.100".into()]),
            8200,
            false,
            false
        ));
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
        assert_eq!(action("tap", &json!({"x": 10.0, "y": 20.5})), Some(Request::Tap { x: 10.0, y: 20.5, count: 1 }));
        assert_eq!(
            action("swipe", &json!({"x1": 1, "y1": 2, "x2": 3, "y2": 4, "seconds": 9})),
            Some(Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 3.0, hold: 0.0 })
        );
        assert_eq!(
            action("type", &json!({"text": "hi"})),
            Some(Request::TypeText { text: "hi".into(), submit: false })
        );
        assert_eq!(action("button", &json!({"button": "home"})), Some(Request::PressButton { button: "home".into() }));
        assert_eq!(
            action("tap_element", &json!({"text": "通用", "index": 1})),
            Some(Request::TapElement { text: "通用".into(), index: 1, max_age_ms: 0, scroll: None })
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

    /// Switching to another device and back: the first one's size is still known (the HID fallback needs it while
    /// its WDA is down)
    #[test]
    fn every_device_keeps_its_screen_size() {
        let (phone, pad) = (
            ictl_device::ScreenSize { width: 430.0, height: 932.0 },
            ictl_device::ScreenSize { width: 768.0, height: 1024.0 },
        );
        let mut p = ictl_daemon::core::Persisted::default();
        assert!(p.record_screen("PHONE", phone));
        assert!(!p.record_screen("PHONE", phone), "nothing new: not saved again");
        assert!(p.record_screen("PAD", pad));
        assert_eq!(p.screen_for(Some("phone")), Some(phone), "after the switch, the phone's own");
        assert_eq!(p.screen_for(Some("PAD")), Some(pad));
        assert_eq!(p.screen_udid.as_deref(), Some("PAD"), "the last one stays where older versions look");
        assert_eq!(p.screen_for(Some("OTHER")), None);
        assert_eq!(p.screen_for(None), None);
        // state.json from 0.2.3 (no `screens`) still reads, and an older version reads ours
        let old: ictl_daemon::core::Persisted =
            serde_json::from_str(r#"{"screen":{"width":430.0,"height":932.0},"screen_udid":"PHONE"}"#).unwrap();
        assert_eq!(old.screen_for(Some("PHONE")), Some(phone));
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["screen_udid"], "PAD");
        assert_eq!(json["screens"]["PHONE"]["width"], 430.0);
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
    fn the_daemon_s_status_becomes_the_page_s() {
        let s = protocol::StatusData {
            wda_ready: true,
            locked: Some(true),
            possibly_locked: false,
            foreground: Some("com.apple.Preferences".into()),
            width: Some(440.0),
            height: Some(956.0),
            native: false,
        };
        let v = status_json(&s, true);
        assert_eq!(v["wda_ready"], true);
        assert_eq!(v["locked"], true);
        assert_eq!(v["foreground_id"], "com.apple.Preferences");
        assert_eq!((v["width"].as_f64(), v["height"].as_f64()), (Some(440.0), Some(956.0)));
        assert_eq!(v["route"], "wda");
        let down = protocol::StatusData::default();
        assert_eq!(status_json(&down, true)["route"], "hid", "iOS 27 and later");
        assert_eq!(status_json(&down, false)["route"], "none", "older: no HID fallback");
        let native = protocol::StatusData { native: true, ..Default::default() };
        assert_eq!(status_json(&native, true)["route"], "native", "iOS 27 driven without WDA");
        assert!(status_json(&down, true)["locked"].is_null());
    }

    /// The location prompt that the phone's own video didn't show (iPhone 15 Pro Max, iOS 27.0.1)
    #[test]
    fn a_system_alert_with_its_own_buttons() {
        // (type, label, center, size), as the daemon reads them
        let tree = [
            ("Button", "今天", (61.0, 880.0), (56.0, 38.0)),
            ("Alert", "允许“日历”使用你的位置？", (215.0, 466.0), (270.0, 320.0)),
            ("StaticText", "允许“日历”使用你的位置？", (215.0, 350.0), (230.0, 44.0)),
            ("Button", "允许一次", (215.0, 520.0), (270.0, 44.0)),
            ("Button", "使用 App 时允许", (215.0, 564.0), (270.0, 44.0)),
            ("Button", "不允许", (215.0, 608.0), (270.0, 44.0)),
        ];
        let elements = tree
            .iter()
            .map(|&(kind, label, (cx, cy), (w, h))| protocol::ElementData {
                kind: kind.into(),
                label: label.into(),
                x: cx - w / 2.0,
                y: cy - h / 2.0,
                width: w,
                height: h,
            })
            .collect();
        let els =
            element_list(Some(protocol::Data::Elements { elements, matched_count: None, truncated: None })).elements;
        assert_eq!(
            els[0],
            json!({"type": "Button", "label": "今天", "x": 33.0, "y": 861.0, "width": 56.0, "height": 38.0})
        );
        assert!(element_list(Some(protocol::Data::Alert { text: None })).elements.is_empty());
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
