use super::actions::*;
use super::api::*;
use super::head::*;
use super::*;
use ictl_daemon::protocol::TouchPhase;
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
    let mut ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
    ctx.port = 8202;
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
             Origin: http://127.0.0.1:8202\r\nContent-Length: {}\r\n\r\n{ack}\
             GET /api/agent/backends HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            ack.len()
        );
        s.write_all(two.as_bytes()).await.unwrap();
        let (h1, _) = answer(&mut s).await;
        // no such stream: its body read, then refused
        assert!(h1.starts_with("HTTP/1.1 404") && h1.contains("Connection: keep-alive"), "{h1}");
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

/// On a connection kept open each head is checked as it came, not only the first: a later request with its length
/// twice is refused. And an answer given before a body was read closes the connection
#[tokio::test]
async fn each_head_on_a_kept_connection_is_checked_as_it_came() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let mut ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
    ctx.port = 8202;
    let ctx = Arc::new(ctx);
    let ack = r#"{"stream":"x","bytes":5}"#;
    let post = |origin: &str, extra: &str| {
        format!(
            "POST /api/video/ack HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Origin: {origin}\r\n{extra}Content-Length: {}\r\n\r\n{ack}",
            ack.len()
        )
    };
    let ours = "http://127.0.0.1:8202";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = async {
        let (tcp, _) = listener.accept().await.unwrap();
        serve(Conn::Plain(tcp), &ctx, None, None).await.unwrap();
    };
    let client = async {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        // the second request in the same write, its length twice
        let two = format!("{}{}", post(ours, ""), post(ours, &format!("Content-Length: {}\r\n", ack.len())));
        s.write_all(two.as_bytes()).await.unwrap();
        let (h1, _) = answer(&mut s).await;
        assert!(h1.starts_with("HTTP/1.1 404") && h1.contains("keep-alive"), "{h1}");
        let (h2, _) = answer(&mut s).await;
        assert!(h2.starts_with("HTTP/1.1 400") && h2.contains("Connection: close"), "{h2}");
    };
    tokio::join!(server, client);
    // refused before its body was read: not kept open
    let r = exchange(&ctx, post("http://elsewhere.example", "Connection: keep-alive\r\n")).await;
    assert!(r.starts_with("HTTP/1.1 403") && r.contains("Connection: close"), "{r}");
}

/// `/mcp` ends when the token it logged in with goes (replaced here), its stream too, as every stream
#[tokio::test]
async fn mcp_ends_when_its_token_goes() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let mut ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
    ctx.auth = Some(auth::Auth::load(temp.path(), 8202).unwrap());
    ctx.mcp = Some(ictl_mcp::http_service(temp.path().join("none.sock")));
    let ctx = Arc::new(ctx);
    let token = auth::get(temp.path(), "owner").unwrap().token;
    let mcp = |body: &str, extra: &str| {
        format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{extra}\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
    let r = exchange(&ctx, mcp(init, "")).await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    let session = r
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("mcp-session-id:").map(|v| v.trim().to_owned()))
        .expect("a session");
    let sid = format!("Mcp-Session-Id: {session}\r\nMcp-Protocol-Version: 2025-06-18\r\n");
    let r = exchange(&ctx, mcp(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &sid)).await;
    assert!(r.starts_with("HTTP/1.1 202"), "{r}");
    // the server's stream of the session: open until the token goes
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let ctx = ctx.clone();
        async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let _ = serve(Conn::Plain(tcp), &ctx, None, None).await;
        }
    });
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let get = format!(
        "GET /mcp HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nAuthorization: Bearer {token}\r\n\
         Accept: text/event-stream\r\n{sid}\r\n"
    );
    s.write_all(get.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut b = [0u8; 1024];
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut b).await.unwrap();
        assert!(n > 0, "closed: {}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&b[..n]);
    }
    assert!(got.starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&got));
    // still open a while later
    assert!(tokio::time::timeout(Duration::from_millis(500), s.read(&mut b)).await.is_err());
    tokio::time::sleep(Duration::from_millis(20)).await;
    auth::renew(temp.path(), "owner").unwrap();
    let end = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match s.read(&mut b).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(end.is_ok(), "the stream outlived its token");
    server.await.unwrap();
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
    let chunked =
        format!("{}{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n", head("Transfer-Encoding: chunked\r\n"), a.len(), b.len());
    let ctx = Arc::new(ctx);
    let r = exchange(&ctx, chunked).await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    assert!(r.contains("Transfer-Encoding: chunked\r\n") && r.ends_with("\r\n0\r\n\r\n"), "{r}");
    assert!(r.contains("iphone"), "{r}");
    // chunked and with a length, in either order (hyper drops the length: the head as it came is checked), and the
    // body really chunked; a length twice, even the same
    let body = format!("{:x}\r\n{init}\r\n0\r\n\r\n", init.len());
    for framing in [
        format!("Transfer-Encoding: chunked\r\nContent-Length: {}\r\n", body.len()),
        format!("Content-Length: {}\r\nTransfer-Encoding: chunked\r\n", body.len()),
    ] {
        let r = exchange(&ctx, format!("{}{body}", head(&framing))).await;
        assert!(r.starts_with("HTTP/1.1 400"), "{framing}: {r}");
    }
    let twice = format!("Content-Length: {0}\r\nContent-Length: {0}\r\n", init.len());
    let r = exchange(&ctx, format!("{}{init}", head(&twice))).await;
    assert!(r.starts_with("HTTP/1.1 400"), "{r}");
    let gzip = format!("{}0\r\n\r\n", head("Transfer-Encoding: gzip, chunked\r\n"));
    assert!(exchange(&ctx, gzip).await.starts_with("HTTP/1.1 400"));
    // a broken chunk is told so, not just cut off
    let broken = format!("{}zz\r\n{{}}\r\n0\r\n\r\n", head("Transfer-Encoding: chunked\r\n"));
    let r = exchange(&ctx, broken).await;
    assert!(r.starts_with("HTTP/1.1 400"), "{r}");
    // framing that never ends (trailer lines, each short) stops at the limit instead of growing without bound:
    // hyper's, 16 KB of trailers. Four times that is sent, less than closing reads off (`io::LINGER_BYTES`): with
    // more left unread the close is a reset, which on macOS loses the answer that came before it
    let trailers = "X-A: 1\r\n".repeat(64 * 1024 / 8 + 1);
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
    assert!(!seen.lock().unwrap().iter().any(|q| q["op"] == "task_control"), "nothing reached the daemon's control");
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
        tokio::time::timeout(Duration::from_secs(10), serve(Conn::Plain(accepted.unwrap().0), &ctx, None, None)).await;
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
    let two = Head::parse(b"GET / HTTP/1.1\r\nHost: a\r\nAccept-Encoding: *\r\nAccept-Encoding: gzip;q=0\r\n").unwrap();
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

/// No ffmpeg to turn the phone's HEVC into H.264 (a Mac encodes WDA's JPEG without it, not HEVC): `/video` is
/// refused with 404 before the phone's video is opened, so the page gives the route up. It used to open the
/// phone's video and answer 200 with nothing in it, and the page kept retrying
#[tokio::test]
async fn no_h264_of_the_phone_s_video_without_ffmpeg_is_refused() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    // no daemon: no WDA picture, and opening the phone's video would fail (502) if it were tried
    let mut ctx = test_ctx(temp.path(), &temp.path().join("daemon.sock"));
    ctx.video = Some(crate::video::Encoders::native_only());
    let ctx = Arc::new(ctx);
    let r = exchange(&ctx, "GET /video?udid=TEST HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".into()).await;
    assert!(r.starts_with("HTTP/1.1 404"), "{r}");
    assert!(r.contains("ffmpeg"), "{r}");
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
        let reply = Response::Error { message: "touch failed".into(), retry_as_gesture: daemon_retry, failure: None };
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
        tokio::time::timeout(Duration::from_secs(2), crate::video::serve_mjpeg(remote, client, idle)).await.unwrap();
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
    assert!(!action_allowed(&head(&ok.replace("Origin: http://127.0.0.1:8200\r\n", "")), &local, 8200, false, false));
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
    assert!(action_allowed(&head(&on_lan), &lan::Access::with_hosts(["192.168.10.100".into()]), 8200, false, false));
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
    assert_eq!(action("type", &json!({"text": "hi"})), Some(Request::TypeText { text: "hi".into(), submit: false }));
    assert_eq!(action("button", &json!({"button": "home"})), Some(Request::PressButton { button: "home".into() }));
    assert_eq!(
        action("tap_element", &json!({"text": "通用", "index": 1})),
        Some(Request::TapElement {
            text: "通用".into(),
            index: 1,
            max_age_ms: 0,
            scroll: None,
            selector: protocol::ElementSelector::default(),
        })
    );
    assert_eq!(
        action("wait_for", &json!({"text": "x", "timeout": 99})),
        Some(Request::WaitFor { text: "x".into(), timeout_ms: 30_000, selector: protocol::ElementSelector::default() })
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
    let els = element_list(Some(protocol::Data::Elements {
        elements,
        matched_count: None,
        truncated: None,
        incomplete: None,
        incomplete_reason: None,
    }))
    .elements;
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

/// Real browsers send `Origin: null` on a same-origin `<form method="post">` when the login page carries
/// `Referrer-Policy: no-referrer`. The login page therefore sends `Referrer-Policy: same-origin` (still never
/// leaking a `Referer` cross-origin) while every other response keeps `no-referrer`; and as defense-in-depth
/// (e.g. behind a reverse proxy forcing `no-referrer`), `POST /login` accepts `Origin: null` only together with
/// `Sec-Fetch-Site: same-origin`.
#[tokio::test]
async fn login_form_referrer_policy_and_null_origin_checks() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let mut ctx = test_ctx(temp.path(), &temp.path().join("none.sock"));
    ctx.port = 8202;
    let token = "t".repeat(64);
    ctx.auth = Some(auth::Auth::fixed(
        vec![auth::Entry { name: "owner".into(), role: auth::Role::Operate, token: token.clone(), created: 0 }],
        8202,
    ));
    let ctx = Arc::new(ctx);
    let status = |r: &str| r.split(' ').nth(1).unwrap_or_default().to_owned();
    let head_lines = |r: &str| -> Vec<String> {
        r.split("\r\n\r\n").next().unwrap_or_default().lines().map(|l| l.to_ascii_lowercase()).collect()
    };

    // Login page uses a single `Referrer-Policy: same-origin` header (not duplicated with `no-referrer`).
    let r = exchange(&ctx, "GET / HTTP/1.1\r\nHost: 127.0.0.1:8202\r\n\r\n".into()).await;
    assert_eq!(status(&r), "401", "{r}");
    let rp: Vec<_> = head_lines(&r).into_iter().filter(|l| l.starts_with("referrer-policy:")).collect();
    assert_eq!(rp, ["referrer-policy: same-origin"], "{r}");

    // 303 redirect after login and the logged-in page itself both keep `Referrer-Policy: no-referrer`.
    let r = exchange(&ctx, format!("GET /?token={token} HTTP/1.1\r\nHost: 127.0.0.1:8202\r\n\r\n")).await;
    assert_eq!(status(&r), "303", "{r}");
    let rp: Vec<_> = head_lines(&r).into_iter().filter(|l| l.starts_with("referrer-policy:")).collect();
    assert_eq!(rp, ["referrer-policy: no-referrer"], "{r}");
    let cookie = r.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap().split(';').next().unwrap().to_owned();
    let r = exchange(&ctx, format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nCookie: {cookie}\r\n\r\n")).await;
    assert_eq!(status(&r), "200", "{r}");
    let rp: Vec<_> = head_lines(&r).into_iter().filter(|l| l.starts_with("referrer-policy:")).collect();
    assert_eq!(rp, ["referrer-policy: no-referrer"], "{r}");

    // `POST /login` with `Origin: null` succeeds ONLY when `Sec-Fetch-Site: same-origin` is present.
    let form = |extra: &str| {
        let body = format!("token={token}");
        format!(
            "POST /login HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nContent-Type: application/x-www-form-urlencoded\r\n\
             Origin: null\r\n{extra}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let r = exchange(&ctx, form("Sec-Fetch-Site: same-origin\r\n")).await;
    assert_eq!(status(&r), "303", "same-origin form POST with Origin: null must succeed: {r}");
    let r = exchange(&ctx, form("")).await;
    assert_eq!(status(&r), "403", "Origin: null without Sec-Fetch-Site must be refused: {r}");
    let r = exchange(&ctx, form("Sec-Fetch-Site: cross-site\r\n")).await;
    assert_eq!(status(&r), "403", "cross-site Origin: null must be refused: {r}");
    let r = exchange(&ctx, form("Sec-Fetch-Site: same-site\r\n")).await;
    assert_eq!(status(&r), "403", "same-site Origin: null must be refused: {r}");
}
