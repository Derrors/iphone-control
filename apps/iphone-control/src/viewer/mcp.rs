//! MCP over HTTP at `/mcp`, on the viewer's port (docs/task-control.md 7.4).
//!
//! The same tools as `iphone-control mcp` on stdio, for clients that connect by URL: an agent on another machine
//! (with `--lan`, over HTTPS), or a client that only speaks HTTP. rmcp's Streamable HTTP server does the protocol
//! (sessions, SSE); hyper reads the request (a body with a length or chunked: some clients and proxies send that)
//! and writes the answer (one without a length, an SSE stream, goes out chunked, so its end is explicit and a proxy
//! can tell it from a cut-off). One request per connection.
//!
//! Who may call it is who may open the page: the viewer checks the peer address, `Host` and `Sec-Fetch-Site` first,
//! and here a request that carries an `Origin` (a browser) must come from the page itself. Clients that aren't
//! browsers send none. The login is the page's (`super::auth`): clients send `Authorization: Bearer <token>`.
//!
//! A task's agent sends the task's token in `Iphone-Control-Task-Token` ([`ictl_daemon::protocol::TASK_TOKEN_HEADER`]),
//! as `IPHONE_CONTROL_TASK_TOKEN` does on stdio.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};

use super::server::Reply;

/// A request body bigger than this is refused (a tool call is a few KB)
pub const MAX_BODY: usize = 1024 * 1024;

/// Hands the request (its head and whole `body`) to `svc`; its answer streams (SSE) until it ends or the client goes
/// away
pub async fn serve(svc: &ictl_mcp::HttpService, parts: http::request::Parts, body: Bytes) -> Reply {
    let (mut parts, body) = svc.handle(http::Request::from_parts(parts, Full::new(body))).await.into_parts();
    // how it goes over the connection is hyper's
    parts.headers.remove(http::header::CONNECTION);
    parts.headers.remove(http::header::TRANSFER_ENCODING);
    // as every answer of the viewer's
    parts.headers.insert(http::header::REFERRER_POLICY, http::HeaderValue::from_static("no-referrer"));
    hyper::Response::from_parts(parts, body.map_err(|never| match never {}).boxed_unsync())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One request through [`serve`]: (status, headers, body)
    async fn exchange(
        svc: &ictl_mcp::HttpService,
        body: &str,
        extra: &[(&str, &str)],
    ) -> (u16, http::HeaderMap, String) {
        let mut b = http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:8202")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        let (parts, ()) = b.body(()).unwrap().into_parts();
        let r = serve(svc, parts, Bytes::from(body.to_owned())).await;
        let (parts, body) = r.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        (parts.status.as_u16(), parts.headers, String::from_utf8_lossy(&body).into_owned())
    }

    /// A session: initialize, then list the tools, with and without a task's token. The tool list doesn't need the
    /// daemon, so none runs here
    #[tokio::test]
    async fn an_mcp_session_over_http() {
        let svc = ictl_mcp::http_service("/nonexistent/ctl.sock".into());
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
        let (status, headers, body) = exchange(&svc, init, &[]).await;
        assert_eq!(status, 200);
        assert!(body.contains("iphone"), "{body}");
        let session = headers.get("mcp-session-id").expect("a session").to_str().unwrap().to_owned();
        let sid = [("Mcp-Session-Id", session.as_str()), ("Mcp-Protocol-Version", "2025-06-18")];
        let done = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let (status, headers, _) = exchange(&svc, done, &sid).await;
        assert_eq!(status, 202);
        assert_eq!(headers.get("referrer-policy").map(|v| v.to_str().unwrap()), Some("no-referrer"));

        let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let (_, _, body) = exchange(&svc, list, &sid).await;
        assert!(body.contains("iphone_tap") && !body.contains("iphone_confirm"), "a plain client: no confirm tool");
        let with_task = [sid[0], sid[1], ("Iphone-Control-Task-Token", "abc")];
        let (_, _, body) = exchange(&svc, list, &with_task).await;
        assert!(body.contains("iphone_confirm"), "a task's agent gets the confirm tool: {body}");
    }
}
