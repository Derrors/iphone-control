//! MCP over HTTP at `/mcp`, on the viewer's port (docs/task-control.md 7.4).
//!
//! The same tools as `iphone-control mcp` on stdio, for clients that connect by URL: an agent on another machine
//! (with `--lan`, over HTTPS), or a client that only speaks HTTP. rmcp's Streamable HTTP server does the protocol
//! (sessions, SSE); this passes requests the viewer has already read to it and writes its answer back, one request
//! per connection like the rest of the viewer.
//!
//! Who may call it is who may open the page: the viewer checks the peer address, `Host` and `Sec-Fetch-Site` first,
//! and here a request that carries an `Origin` (a browser) must come from the page itself. Clients that aren't
//! browsers send none. No login yet, as for the page.
//!
//! A task's agent sends the task's token in `Iphone-Control-Task-Token` ([`ictl_daemon::protocol::TASK_TOKEN_HEADER`]),
//! as `IPHONE_CONTROL_TASK_TOKEN` does on stdio.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use tokio::io::AsyncWriteExt;

use super::tls::Conn;

/// A request body bigger than this is refused (a tool call is a few KB)
pub const MAX_BODY: usize = 1024 * 1024;

/// Hands the request (`head` and its whole `body`) to `svc` and writes the answer, streaming it (SSE) until it ends
/// or the client goes away
pub async fn serve(svc: &ictl_mcp::HttpService, client: &mut Conn, head: &[u8], body: Vec<u8>) -> anyhow::Result<()> {
    let Some(req) = request(head, body) else {
        client.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        client.flush().await?;
        return Ok(());
    };
    let (parts, mut body) = svc.handle(req).await.into_parts();
    client.write_all(response_head(&parts).as_bytes()).await?;
    client.flush().await?;
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(f) => f,
            Err(never) => match never {},
        };
        if let Some(data) = frame.data_ref() {
            client.write_all(data).await?;
            // each event goes out now (TLS holds the last record back otherwise)
            client.flush().await?;
        }
    }
    Ok(())
}

/// The request as rmcp takes it
fn request(head: &[u8], body: Vec<u8>) -> Option<http::Request<Full<Bytes>>> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target) = (first.next()?, first.next()?);
    let mut b = http::Request::builder().method(method).uri(target);
    for line in lines.filter(|l| !l.is_empty()) {
        let (k, v) = line.split_once(':')?;
        b = b.header(k.trim(), v.trim());
    }
    b.body(Full::new(Bytes::from(body))).ok()
}

/// Status line and headers; the body ends with the connection (`Connection: close`) unless it has a length
fn response_head(parts: &http::response::Parts) -> String {
    let mut out =
        format!("HTTP/1.1 {} {}\r\n", parts.status.as_u16(), parts.status.canonical_reason().unwrap_or("Unknown"));
    for (k, v) in &parts.headers {
        if k == http::header::CONNECTION || k == http::header::TRANSFER_ENCODING {
            continue;
        }
        out.push_str(&format!("{}: {}\r\n", k.as_str(), String::from_utf8_lossy(v.as_bytes())));
    }
    out.push_str("Connection: close\r\n\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// One HTTP request through [`serve`], as a client sees it: (head, body)
    async fn exchange(svc: &ictl_mcp::HttpService, raw: String) -> (String, String) {
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
        let mut conn = Conn::Plain(tcp);
        let (head, body) = crate::read_head(&mut conn).await.unwrap();
        let len = Head::content_length(&head);
        let mut body = body;
        while body.len() < len {
            let mut buf = [0u8; 4096];
            let n = conn.read(&mut buf).await.unwrap();
            body.extend_from_slice(&buf[..n]);
        }
        serve(svc, &mut conn, &head, body).await.unwrap();
        drop(conn);
        let out = client.await.unwrap();
        let (h, b) = out.split_once("\r\n\r\n").unwrap();
        (h.to_owned(), b.to_owned())
    }

    struct Head;
    impl Head {
        fn content_length(head: &[u8]) -> usize {
            String::from_utf8_lossy(head)
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                .unwrap_or(0)
        }
    }

    fn post(body: &str, extra: &str) -> String {
        format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:8202\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n{extra}\r\n{body}",
            body.len()
        )
    }

    fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    /// A session: initialize, then list the tools, with and without a task's token. The tool list doesn't need the
    /// daemon, so none runs here
    #[tokio::test]
    async fn an_mcp_session_over_http() {
        let svc = ictl_mcp::http_service("/nonexistent/ctl.sock".into());
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
        let (head, body) = exchange(&svc, post(init, "")).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(body.contains("iphone"), "{body}");
        let session = header(&head, "mcp-session-id").expect("a session").to_owned();
        let sid = format!("Mcp-Session-Id: {session}\r\nMcp-Protocol-Version: 2025-06-18\r\n");
        let done = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let (head, _) = exchange(&svc, post(done, &sid)).await;
        assert!(head.starts_with("HTTP/1.1 202"), "{head}");

        let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let (_, body) = exchange(&svc, post(list, &sid)).await;
        assert!(body.contains("iphone_tap") && !body.contains("iphone_confirm"), "a plain client: no confirm tool");
        let (_, body) = exchange(&svc, post(list, &format!("{sid}Iphone-Control-Task-Token: abc\r\n"))).await;
        assert!(body.contains("iphone_confirm"), "a task's agent gets the confirm tool: {body}");
    }

    #[test]
    fn a_broken_request_head_isn_t_passed_on() {
        assert!(request(b"GET /mcp HTTP/1.1\r\nno colon here", Vec::new()).is_none());
        assert!(request(b"GET /mcp HTTP/1.1\r\nHost: x", Vec::new()).is_some());
    }
}
