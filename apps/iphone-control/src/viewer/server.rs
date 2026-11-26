//! The viewer's HTTP/1.1 under hyper: a connection, and what every request passes before its route.
//!
//! hyper reads requests (heads, bodies, chunked or not), writes answers and keeps connections open. What it doesn't
//! do is here: the checks on the head ([`Head::parse`], stricter than HTTP asks, see there), the `Host`, HTTPS for
//! the network, the login before anything (no body read before it), which connections stay open, the language of
//! each request, and ending a stream whose token goes. The connection itself has a time limit on writing and lingers
//! on close ([`super::io`]).

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
        .header("X-Frame-Options", "DENY")
        .header("Referrer-Policy", "no-referrer");
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
    let state = Arc::new(ConnState { peer, local, tls: conn.is_tls(), early: Mutex::new(early), room });
    let io = hyper_util::rt::TokioIo::new(super::io::Timed::new(conn, super::WRITE_TIMEOUT));
    let service = hyper::service::service_fn(move |req| {
        let (ctx, state) = (ctx.clone(), state.clone());
        async move { Ok::<_, std::convert::Infallible>(request(ctx, state, req).await) }
    });
    hyper::server::conn::http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        // a head (the first, or the next on a connection kept open) in this time, or the connection closes: a
        // connection that sends nothing doesn't keep its place
        .header_read_timeout(super::KEEP_IDLE.min(crate::HEAD_TIMEOUT))
        .max_buf_size(crate::MAX_HEAD.max(8192))
        // the same names as before (clients and tests read `Connection: close`)
        .title_case_headers(true)
        .serve_connection(io, service)
        .await
        // a connection that sent nothing more in time is closed: that is how idle ones end
        .or_else(|e| if e.is_timeout() { Ok(()) } else { Err(e) })?;
    Ok(())
}

/// One request: the checks, the login, its route; in the page's language (`X-Ictl-Lang`), so the daemon's messages
/// match the page
async fn request(ctx: Arc<Ctx>, conn: Arc<ConnState>, req: hyper::Request<Incoming>) -> Reply {
    let (parts, body) = req.into_parts();
    let raw = raw_head(&parts);
    let Some(head) = Head::parse(&raw) else { return closing(reply(400, "text/plain", "bad request")) };
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
    body: Incoming,
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
    let r = super::routes::route(ctx, conn, head, parts, body, pass).await;
    if keep { keeping(r) } else { closing(r) }
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

/// A head as it came, for the checks that read one ([`Head::parse`], [`auth::Auth::check`],
/// [`super::lan::Access::host_allowed`]): what hyper parsed, written out again
fn raw_head(parts: &http::request::Parts) -> Vec<u8> {
    let version = match parts.version {
        http::Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    };
    let mut out = format!("{} {} {version}\r\n", parts.method, parts.uri).into_bytes();
    for (k, v) in &parts.headers {
        out.extend_from_slice(k.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Why a body wasn't read
pub(super) enum BodyError {
    TooLarge,
    /// Cut off, or too slow
    Broken,
}

/// The whole body, at most `max` bytes, within `limit` however slowly it trickles in
pub(super) async fn read_body(body: Incoming, max: usize, limit: Duration) -> Result<Bytes, BodyError> {
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
    body: &mut Option<Incoming>,
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
        if head.origin.as_deref().is_some_and(|o| !ctx.access.origin_allowed(o, ctx.port, ctx.tls.is_some())) {
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

/// The login page, saying why the last try failed
fn login_page(v: auth::Verdict) -> Reply {
    let (status, why) = match v {
        auth::Verdict::Wrong => (401, Some(auth::LoginError::Wrong)),
        auth::Verdict::Limited => (429, Some(auth::LoginError::Limited)),
        _ => (401, None),
    };
    let page = auth::login_page(why);
    let csp = super::csp_for(&page);
    reply_with(status, "text/html; charset=utf-8", page, &[("Content-Security-Policy", &csp)])
}

/// A stream's handler, in the request's language, ended when the token it logged in with goes (removed or replaced)
pub(super) async fn guarded(
    ctx: Arc<Ctx>,
    who: Option<auth::Who>,
    lang: Option<ictl_l10n::Lang>,
    handler: impl std::future::Future<Output = ()>,
) {
    let run = async {
        match (&ctx.auth, &who) {
            (Some(a), Some(w)) => tokio::select! {
                () = handler => {}
                () = revoked(a, w) => {
                    tracing::info!("viewer: the token “{}” is gone, so its connection was closed", w.name);
                }
            },
            _ => handler.await,
        }
    };
    match lang {
        Some(l) => ictl_l10n::scope(l, run).await,
        None => run.await,
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
