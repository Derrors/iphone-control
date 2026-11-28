//! The viewer's HTTP/1.1 under hyper: a connection, and what every request passes before its route.
//!
//! hyper reads requests (heads, bodies, chunked or not), writes answers and keeps connections open. What it doesn't
//! do is here: the checks on the head ([`Head::parse`], stricter than HTTP asks, see there; on the head as it came,
//! [`super::io::HeadTap`]), the `Host`, HTTPS for the network, the login before anything (no body read before it),
//! which connections stay open, the language of each request, and ending an answer (a stream, `/mcp`) whose token
//! goes. The connection itself has a time limit on writing and lingers on close ([`super::io`]).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use serde_json::{Value, json};

use super::{Ctx, Early, Head, Pass, auth, fetch_site_ok, https_location, keepable, protocol, revoked};

/// An answer's body: whole, or written by a streaming handler ([`super::pipe`])
pub(super) type Body = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;
pub(super) type Reply = hyper::Response<Body>;

/// What a request knows of its connection
pub(super) struct ConnState {
    pub peer: SocketAddr,
    pub local: SocketAddr,
    pub tls: bool,
    /// Counted against its machine's connections arriving until a request on it passed the login
    /// ([`super::ARRIVING_PER_PEER`])
    early: Mutex<Option<Early>>,
    /// The viewer's connection places: one is kept open only while enough are free ([`super::KEEP_ROOM`])
    room: Option<Arc<tokio::sync::Semaphore>>,
    /// The heads as they came ([`super::io::HeadTap`])
    heads: Arc<Mutex<super::io::HeadTap>>,
    /// A request came: the first one's time limit is over
    came: std::sync::atomic::AtomicBool,
}

/// An answer with the headers every answer carries
pub(super) fn reply(status: u16, kind: &str, body: impl Into<Bytes>) -> Reply {
    reply_with(status, kind, body, &[])
}

/// [`reply`] with more headers
pub(super) fn reply_with(status: u16, kind: &str, body: impl Into<Bytes>, extra: &[(&str, &str)]) -> Reply {
    let body = Full::new(body.into()).map_err(|never| match never {}).boxed_unsync();
    with_headers(status, kind, body, extra)
}

pub(super) fn json_reply(status: u16, v: &Value) -> Reply {
    reply(status, "application/json", v.to_string())
}

/// A stream: its body written as it goes ([`super::pipe::streamed`]); the connection closes after it
pub(super) fn stream(kind: &str, body: super::pipe::Streamed) -> Reply {
    let mut r = with_headers(200, kind, body.boxed_unsync(), &[]);
    r.headers_mut().insert(hyper::header::CONNECTION, hyper::header::HeaderValue::from_static("close"));
    r
}

fn with_headers(status: u16, kind: &str, body: Body, extra: &[(&str, &str)]) -> Reply {
    let mut b = hyper::Response::builder()
        .status(status)
        .header("Content-Type", kind)
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("X-Frame-Options", "DENY");
    if !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("referrer-policy")) {
        b = b.header("Referrer-Policy", "no-referrer");
    }
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    b.body(body).unwrap_or_else(|_| {
        // only a header value of ours that isn't one (a control character): never with the values written here
        let mut r = hyper::Response::new(Full::new(Bytes::new()).map_err(|never| match never {}).boxed_unsync());
        *r.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
        r
    })
}

/// Requests on one connection (after the TLS handshake). `early`: the connection still arriving
/// ([`super::Arriving`]); `room`: the viewer's places, for keeping connections open
pub(super) async fn serve_conn(
    conn: super::Conn,
    ctx: Arc<Ctx>,
    early: Option<Early>,
    room: Option<Arc<tokio::sync::Semaphore>>,
) -> anyhow::Result<()> {
    let (Ok(peer), Ok(local)) = (conn.tcp().peer_addr(), conn.tcp().local_addr()) else { return Ok(()) };
    let heads = Arc::new(Mutex::new(super::io::HeadTap::default()));
    let state = Arc::new(ConnState {
        peer,
        local,
        tls: conn.is_tls(),
        early: Mutex::new(early),
        room,
        heads: heads.clone(),
        came: Default::default(),
    });
    let io = super::io::Timed::new(super::io::Tapped::new(conn, heads), super::WRITE_TIMEOUT);
    let service = {
        let state = state.clone();
        hyper::service::service_fn(move |req| {
            let (ctx, state) = (ctx.clone(), state.clone());
            async move { Ok::<_, std::convert::Infallible>(request(ctx, state, req).await) }
        })
    };
    let served = hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        // the next head on a connection kept open, waiting included, in this time, or the connection closes: an idle
        // connection doesn't keep its place. hyper starts it each time it reads a head, the first one too, which has
        // a shorter limit below
        .header_read_timeout(super::KEEP_IDLE)
        // about a head (hyper reads no further ahead): the most a connection that sends nothing but a head holds
        .max_buf_size(crate::MAX_HEAD.max(8192))
        // the same names as before (clients and tests read `Connection: close`)
        .title_case_headers(true)
        .serve_connection(hyper_util::rt::TokioIo::new(io), service);
    tokio::pin!(served);
    // the first head in [`crate::HEAD_TIMEOUT`] (as the TLS handshake): a connection that sends nothing, or a byte now
    // and then, doesn't keep its place
    let done = match tokio::time::timeout(crate::HEAD_TIMEOUT, &mut served).await {
        Ok(r) => r,
        Err(_) if !state.came.load(std::sync::atomic::Ordering::Relaxed) => return Ok(()),
        Err(_) => served.await,
    };
    // a connection that sent nothing more in time is closed: that is how idle ones end
    done.or_else(|e| if e.is_timeout() { Ok(()) } else { Err(e) })?;
    Ok(())
}

/// One request: the checks, the login, its route; in the page's language (`X-Ictl-Lang`), so the daemon's messages
/// match the page
async fn request(ctx: Arc<Ctx>, conn: Arc<ConnState>, req: hyper::Request<Incoming>) -> Reply {
    conn.came.store(true, std::sync::atomic::Ordering::Relaxed);
    let (parts, body) = req.into_parts();
    let body = RequestBody::new(body);
    // its head as it came, the same request as hyper read (see [`super::io::HeadTap`])
    let raw = conn.heads.lock().unwrap_or_else(|p| p.into_inner()).take(body_length(&parts));
    let Some((raw, head)) = raw.filter(|r| same_request(r, &parts)).and_then(|r| Head::parse(&r).map(|h| (r, h)))
    else {
        return closing(reply(400, "text/plain", "bad request"));
    };
    let lang = head.lang;
    let run = checked(ctx, conn, raw, head, parts, body);
    match lang {
        Some(l) => ictl_l10n::scope(l, run).await,
        None => run.await,
    }
}

/// [`request`] past the head: who may come in, and how, then the route. Short requests of someone logged in (the
/// page, its API) keep the connection open for the next one ([`keepable`]): over Wi-Fi a new connection and TLS
/// handshake are two more round trips, for each of the ten reports a second on the picture, each tap and each poll.
/// Streams end it, and so does any answer before the login: without a token nobody holds places for long
async fn checked(
    ctx: Arc<Ctx>,
    conn: Arc<ConnState>,
    raw: Vec<u8>,
    head: Head,
    parts: http::request::Parts,
    body: RequestBody,
) -> Reply {
    if !ctx.access.host_allowed(&raw) || (head.path != "/" && !fetch_site_ok(&head)) {
        return closing(reply(403, "text/plain", "forbidden"));
    }
    // a chunked body is taken for `/mcp` only; elsewhere it would be taken for an empty one
    if head.chunked && head.path != "/mcp" {
        return closing(reply(411, "text/plain", "length required"));
    }
    // HTTPS is on: from the network only HTTPS (plain HTTP stays for this machine)
    let local = conn.peer.ip().to_canonical().is_loopback();
    if ctx.tls.is_some() && !conn.tls && !local {
        return closing(match https_location(&raw, &head, ctx.port) {
            // temporary (307): with `--tls off` later, browsers mustn't remember it
            Some(to) => reply_with(307, "text/plain", "", &[("Location", &to)]),
            None => reply(400, "text/plain", "bad request"),
        });
    }
    let mut body = Some(body);
    let pass = match login_gate(&ctx, &conn, &raw, &head, &mut body).await {
        Ok(p) => p,
        Err(answered) => return closing(*answered),
    };
    drop(conn.early.lock().unwrap_or_else(|p| p.into_inner()).take());
    if !pass.may(&head) {
        let m = ictl_l10n::tr!(
            "这是只读令牌：只能看画面、设备状态和任务，不能操作",
            "This token is read-only: it can watch the picture, the device's state and tasks, but not act"
        );
        return closing(json_reply(403, &json!({"ok": false, "message": m})));
    }
    let keep = keepable(&head) && conn.room.as_ref().is_none_or(|r| r.available_permits() >= super::KEEP_ROOM);
    let body = body.take().expect("taken only by the login form, which answers");
    // an answer before the body was read (refused, too large) ends the connection: hyper wouldn't read on past it
    let read = body.read();
    let who = pass.who.clone().filter(|_| ctx.auth.is_some());
    let route = super::routes::route(ctx.clone(), conn, head, parts, body, pass);
    // the answer ends when the token it logged in with goes (removed or replaced): a stream, `/mcp` too, as it goes
    let r = match who {
        Some(w) => {
            let mut gone = token_gone(ctx, w);
            tokio::select! {
                r = route => until(r, gone),
                () = &mut gone => return closing(reply(401, "text/plain", "log in first")),
            }
        }
        None => route.await,
    };
    if keep && read.load(std::sync::atomic::Ordering::Relaxed) { keeping(r) } else { closing(r) }
}

/// Returns once `who`'s token is gone ([`revoked`]), and says so
fn token_gone(ctx: Arc<Ctx>, who: auth::Who) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        if let Some(a) = &ctx.auth {
            revoked(a, &who).await;
            tracing::info!("viewer: the token “{}” is gone, so its connection was closed", who.name);
        }
    })
}

/// `r`, its body cut off once `stop` returns: the connection ends there (a chunked body without its last chunk)
fn until(r: Reply, stop: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>) -> Reply {
    r.map(|inner| Until { inner, stop }.boxed_unsync())
}

struct Until {
    inner: Body,
    stop: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl http_body::Body for Until {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
        if self.stop.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(Some(Err(std::io::Error::other("the token is gone"))));
        }
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

fn closing(mut r: Reply) -> Reply {
    r.headers_mut().insert(hyper::header::CONNECTION, hyper::header::HeaderValue::from_static("close"));
    r
}

/// Said so the client knows (HTTP/1.1 keeps connections open anyway); a stream closes all the same
fn keeping(mut r: Reply) -> Reply {
    if !r.headers().contains_key(hyper::header::CONNECTION) {
        r.headers_mut().insert(hyper::header::CONNECTION, hyper::header::HeaderValue::from_static("keep-alive"));
    }
    r
}

/// How much body hyper reads after this head, for the next one's place ([`super::io::HeadTap::take`]); `None`:
/// chunked
fn body_length(parts: &http::request::Parts) -> Option<u64> {
    if parts.headers.contains_key(http::header::TRANSFER_ENCODING) {
        return None;
    }
    // hyper refused differing lengths already
    let n = parts.headers.get(http::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.trim().parse().ok());
    Some(n.unwrap_or(0))
}

/// The head that came is the request hyper read: its method and target
fn same_request(raw: &[u8], parts: &http::request::Parts) -> bool {
    let line = raw.split(|b| *b == b'\r').next().unwrap_or_default();
    let mut words = line.split(|b| *b == b' ');
    words.next() == Some(parts.method.as_str().as_bytes()) && words.next() == Some(parts.uri.to_string().as_bytes())
}

/// A request's body, which tells when it was read to its end ([`RequestBody::read`])
pub(super) struct RequestBody {
    inner: Incoming,
    read: Arc<std::sync::atomic::AtomicBool>,
}

impl RequestBody {
    fn new(inner: Incoming) -> Self {
        let read = Arc::new(std::sync::atomic::AtomicBool::new(http_body::Body::is_end_stream(&inner)));
        Self { inner, read }
    }

    /// Set once the body was read to its end (at once when there is none)
    fn read(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.read.clone()
    }
}

impl http_body::Body for RequestBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, hyper::Error>>> {
        let r = std::pin::Pin::new(&mut self.inner).poll_frame(cx);
        if matches!(r, std::task::Poll::Ready(None)) || self.inner.is_end_stream() {
            self.read.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        r
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Why a body wasn't read
pub(super) enum BodyError {
    TooLarge,
    /// Cut off, or too slow
    Broken,
}

/// The whole body, at most `max` bytes, within `limit` however slowly it trickles in
pub(super) async fn read_body(body: RequestBody, max: usize, limit: Duration) -> Result<Bytes, BodyError> {
    match tokio::time::timeout(limit, Limited::new(body, max).collect()).await {
        Ok(Ok(b)) => Ok(b.to_bytes()),
        Ok(Err(e)) if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() => Err(BodyError::TooLarge),
        _ => Err(BodyError::Broken),
    }
}

/// How long a whole body (at most `mcp::MAX_BODY`) may take
pub(super) const BODY_TIMEOUT: Duration = Duration::from_secs(60);

/// The login ([`auth`]) before anything else: who the request is when it may go on, otherwise the answer (the login
/// page, a login, or 401 / 429). Nothing of a body is read for it but the login form's: someone without a token
/// can't hold places with bodies that trickle in
async fn login_gate(
    ctx: &Ctx,
    conn: &ConnState,
    raw: &[u8],
    head: &Head,
    body: &mut Option<RequestBody>,
) -> Result<Pass, Box<Reply>> {
    use auth::Verdict;
    let Some(a) = &ctx.auth else { return Ok(Pass::default()) };
    let secure = conn.tls;
    // the link `iphone-control viewer token` shows
    if head.method == "GET"
        && head.path == "/"
        && let Some(t) = head.query("token")
    {
        return Err(Box::new(match a.try_token(&t) {
            Verdict::Ok(w) => logged_in(a, &w.name, secure),
            v => login_page(v),
        }));
    }
    // the login page's form
    if head.method == "POST" && head.path == "/login" {
        let origin_ok = match head.origin.as_deref() {
            None => true,
            // Under `Referrer-Policy: no-referrer` (e.g. forced by a reverse proxy), browsers send `Origin: null`
            // on same-origin form POSTs together with `Sec-Fetch-Site: same-origin`.
            Some("null") => head.fetch_site.as_deref() == Some("same-origin"),
            Some(o) => ctx.access.origin_allowed(o, ctx.port, ctx.tls.is_some()),
        };
        if !origin_ok {
            return Err(Box::new(reply(403, "text/plain", "forbidden")));
        }
        if head.content_length.unwrap_or(0) > 4096 {
            return Err(Box::new(reply(413, "text/plain", "too large")));
        }
        // a 4 KB form: no longer than a head may take ([`BODY_TIMEOUT`] is for large bodies)
        let form = match read_body(body.take().expect("not read yet"), 4096, crate::HEAD_TIMEOUT).await {
            Ok(b) => b,
            Err(BodyError::TooLarge) => return Err(Box::new(reply(413, "text/plain", "too large"))),
            Err(BodyError::Broken) => return Err(Box::new(reply(400, "text/plain", "bad request"))),
        };
        return Err(Box::new(
            match auth::form_token(&form).filter(|t| !t.trim().is_empty()).map(|t| a.try_token(&t)) {
                Some(Verdict::Ok(w)) => logged_in(a, &w.name, secure),
                Some(v) => login_page(v),
                None => login_page(Verdict::Missing),
            },
        ));
    }
    let mut v = match a.check(raw) {
        Verdict::Ok(w) => return Ok(Pass { who: Some(w), agent: false }),
        v => v,
    };
    // an agent on another machine: its task's token instead of a login, only for that task (docs/task-control.md 7.6)
    // (checked even after too many wrong tokens: a running task isn't cut off by someone sending wrong ones)
    if super::TASK_PATHS.contains(&head.path.as_str())
        && let Some(t) = &head.task_token
    {
        if protocol::task_check(&ctx.socket, t).await.is_ok() {
            return Ok(Pass { who: None, agent: true });
        }
        // a wrong task token counts like a wrong login (once, when no login token was sent with it)
        if v == Verdict::Missing {
            v = a.failed();
        }
    }
    let status = if v == Verdict::Limited { 429 } else { 401 };
    Err(Box::new(match (head.method.as_str(), head.path.as_str()) {
        ("GET", "/") => login_page(v),
        // MCP clients: send the token as a bearer token (RFC 6750)
        (_, "/mcp") => reply_with(
            status,
            "text/plain",
            "log in: Authorization: Bearer <token>",
            &[("WWW-Authenticate", "Bearer realm=\"iphone-control\"")],
        ),
        _ => reply(status, "text/plain", "log in first"),
    }))
}

/// Logged in: a session cookie, and the page without the token in its address (and history)
fn logged_in(a: &auth::Auth, name: &str, secure: bool) -> Reply {
    reply_with(303, "text/plain", "", &[("Location", "/"), ("Set-Cookie", &a.set_cookie(name, secure))])
}

/// The login page, saying why the last try failed.
/// Uses `Referrer-Policy: same-origin` (instead of `no-referrer`) so browsers send the real `Origin` header on
/// same-origin `<form method="post" action="/login">` submissions (including plain HTTP LAN `--tls off` where
/// `Sec-Fetch-Site` is omitted), while still never leaking a `Referer` to any other origin.
fn login_page(v: auth::Verdict) -> Reply {
    let (status, why) = match v {
        auth::Verdict::Wrong => (401, Some(auth::LoginError::Wrong)),
        auth::Verdict::Limited => (429, Some(auth::LoginError::Limited)),
        _ => (401, None),
    };
    let page = auth::login_page(why);
    let csp = super::csp_for(&page);
    reply_with(
        status,
        "text/html; charset=utf-8",
        page,
        // says what answers here without logging in: an update checks a viewer it started again is up (`fleet`)
        &[("Content-Security-Policy", &csp), ("Referrer-Policy", "same-origin"), ("X-Iphone-Control", "viewer")],
    )
}

/// A stream's handler, in the request's language (its token going ends it with the answer, [`checked`])
pub(super) async fn in_lang(lang: Option<ictl_l10n::Lang>, handler: impl std::future::Future<Output = ()>) {
    match lang {
        Some(l) => ictl_l10n::scope(l, handler).await,
        None => handler.await,
    }
}

/// `503 Service Unavailable` to a connection over [`super::MAX_CONNECTIONS`] (after the TLS handshake on HTTPS), so
/// the page says why instead of seeing a connection drop. Written out directly: it gets no place of its own
pub(super) async fn answer_busy(client: tokio::net::TcpStream, tls: Option<Arc<super::tls::Tls>>) {
    use tokio::io::AsyncWriteExt;
    let mut conn = match tls {
        Some(t) => match t.accept(client).await {
            Ok(c) => c,
            Err(_) => return,
        },
        None => super::Conn::Plain(client),
    };
    let body = b"too many connections, retry shortly";
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nRetry-After: 2\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = conn.write_all(&[head.as_bytes(), body].concat()).await;
    let _ = conn.shutdown().await;
}
