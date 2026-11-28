//! `iphone-control viewer`: a page on 127.0.0.1 to watch the phone live, act on it with the mouse, look at its
//! state, apps and elements, and hand tasks to a local AI agent (see [`crate::agent`]).
//!
//! - `GET /` the page, `GET /stream` WDA's MJPEG stream (port 9100 on the phone, over usbmuxd, read-only; repeated
//!   frames dropped), `GET /video` the same frames as H.264 when ffmpeg can encode them ([`crate::video`]), about
//!   1/15 of the traffic.
//! - `GET /api/{info,daemon,status,devices,apps,elements}`: the daemon's answers as JSON for the page; `GET /api/alert`
//!   a system alert on the phone with its buttons (the phone's own video can leave it out, see [`api::alert`]).
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

mod actions;
mod api;
pub mod auth;
mod events;
mod head;
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

use actions::{SHORT, post, updates};
use head::{
    Head, Pass, TASK_PATHS, action_allowed, fetch_site_ok, https_location, keepable, percent_decode, task_events_id,
};
use ictl_daemon::machine::DeviceState;
use ictl_daemon::protocol::{self, Request, Response};
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
    /// Shared on-demand SSE state broadcaster for `GET /api/events`
    events: events::StateHub,
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
    // connected it is the one that picks. Ask the daemon's snapshot only, never falling back to enumerating USB
    // directly from the viewer
    let udid = match crate::client::snapshot(socket).await {
        Ok(s) => s.device.map(|d| d.udid).or(s.pinned).unwrap_or_default(),
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
        events: events::StateHub::default(),
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

/// How often an open connection's token is looked for ([`revoked`])
const TOKEN_RECHECK: Duration = if cfg!(test) { Duration::from_millis(200) } else { Duration::from_secs(5) };

/// Returns once `who`'s token is no longer there
async fn revoked(a: &auth::Auth, who: &auth::Who) {
    loop {
        tokio::time::sleep(TOKEN_RECHECK).await;
        if !a.knows(who) {
            return;
        }
    }
}

fn sse(e: &protocol::TaskEvent) -> String {
    format!("event: {}\ndata: {}\n\n", e.event, e.data)
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

#[cfg(test)]
mod tests;
