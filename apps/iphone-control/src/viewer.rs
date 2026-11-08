//! `iphone-control viewer`: a small page on 127.0.0.1 that shows the phone's screen live and acts on it.
//!
//! - `GET /` the page, `GET /stream` WDA's MJPEG stream (port 9100 on the phone, over usbmuxd, read-only),
//!   `GET /api/info` the screen size in points.
//! - `POST /api/{tap,swipe,long_press,type,button}` turn into ordinary daemon requests, so they queue with
//!   the agent's calls, get auto-unlock and follow the controlled device, exactly like an MCP tool call.
//!
//! Protections, same model as `wda-bridge`: it binds to the loopback address only, every request needs a
//! loopback `Host` (against DNS rebinding), and every action also needs an `Origin` of this very page and a
//! JSON content type (against other web pages posting to it). Other local users can still reach the port,
//! which the command says when it starts.

use std::path::Path;
use std::time::Duration;

use anyhow::bail;
use ictl_daemon::protocol::{Request, Response};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PAGE: &str = include_str!("viewer.html");
/// WDA's MJPEG server on the phone
const MJPEG_PORT: u16 = 9100;
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 16 * 1024;

pub async fn run(socket: &Path, port: u16) -> anyhow::Result<()> {
    let target = crate::target_device(socket, None).await?;
    let device = ictl_device::Device::find(Some(&target.udid)).await?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    println!(
        "{}",
        ictl_l10n::trf!(
            "实时画面:http://127.0.0.1:{port}/ (手机 {},Ctrl-C 停止)",
            "Live viewer: http://127.0.0.1:{port}/ (phone {}, Ctrl-C to stop)",
            device.udid
        )
    );
    println!(
        "{}",
        ictl_l10n::tr!(
            "注意:本机所有用户都能经这个端口看到屏幕、操作手机,用完就停掉。",
            "Note: every user on this machine can watch and control the phone through this port; stop it when you are done."
        )
    );
    loop {
        let (client, _) = listener.accept().await?;
        let device = device.clone();
        let socket = socket.to_path_buf();
        tokio::spawn(async move {
            if let Err(e) = serve(client, &device, &socket, port).await {
                tracing::debug!("viewer connection: {e}");
            }
        });
    }
}

/// One request per connection (`Connection: close`), which keeps the parser small
async fn serve(mut client: TcpStream, device: &ictl_device::Device, socket: &Path, port: u16) -> anyhow::Result<()> {
    let (head, mut body) = read_head(&mut client).await?;
    let Some(req) = Head::parse(&head) else {
        return reply(&mut client, 400, "text/plain", b"bad request").await;
    };
    if !crate::host_is_loopback(&head) {
        return reply(&mut client, 403, "text/plain", b"forbidden").await;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => reply(&mut client, 200, "text/html; charset=utf-8", PAGE.as_bytes()).await,
        ("GET", "/stream") => {
            let mut remote = match device.connect_port(MJPEG_PORT).await {
                Ok(r) => r,
                Err(e) => return reply(&mut client, 502, "text/plain", e.to_string().as_bytes()).await,
            };
            // WDA's MJPEG server answers any request with the multipart stream
            remote.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").await?;
            tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
            Ok(())
        }
        ("GET", "/api/info") => {
            let v = info(socket).await;
            reply(&mut client, 200, "application/json", v.to_string().as_bytes()).await
        }
        ("POST", path) if path.starts_with("/api/") => {
            if !action_allowed(&req, port) {
                return reply(&mut client, 403, "text/plain", b"forbidden").await;
            }
            let len = req.content_length.unwrap_or(0);
            if len > MAX_BODY {
                return reply(&mut client, 413, "text/plain", b"too large").await;
            }
            while body.len() < len {
                let mut buf = [0u8; 4096];
                let n = client.read(&mut buf).await?;
                if n == 0 {
                    bail!("body cut off");
                }
                body.extend_from_slice(&buf[..n]);
            }
            body.truncate(len);
            let v = match serde_json::from_slice::<Value>(&body).ok().and_then(|b| action(&path[5..], &b)) {
                Some(r) => run_action(socket, r).await,
                None => json!({"ok": false, "message": "bad action"}),
            };
            reply(&mut client, 200, "application/json", v.to_string().as_bytes()).await
        }
        _ => reply(&mut client, 404, "text/plain", b"not found").await,
    }
}

/// Reads up to the end of the request head; returns the head and whatever of the body came with it
async fn read_head(client: &mut TcpStream) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let body = data.split_off(i + 4);
            return Ok((data, body));
        }
        if data.len() > MAX_HEAD {
            bail!("request head too long");
        }
        let n = tokio::time::timeout(Duration::from_secs(10), client.read(&mut buf)).await??;
        if n == 0 {
            bail!("connection closed");
        }
        data.extend_from_slice(&buf[..n]);
    }
}

async fn reply(client: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
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
    origin: Option<String>,
    content_type: Option<String>,
    content_length: Option<usize>,
}

impl Head {
    fn parse(head: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(head).ok()?;
        let mut lines = text.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let method = first.next()?.to_owned();
        let path = first.next()?.split('?').next()?.to_owned();
        let mut h = Self { method, path, origin: None, content_type: None, content_length: None };
        for l in lines {
            let Some((k, v)) = l.split_once(':') else { continue };
            let v = v.trim();
            match k.trim().to_ascii_lowercase().as_str() {
                "origin" => h.origin = Some(v.to_owned()),
                "content-type" => h.content_type = Some(v.to_ascii_lowercase()),
                "content-length" => h.content_length = Some(v.parse().ok()?),
                _ => {}
            }
        }
        Some(h)
    }
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

/// Maps a page action to the daemon request it stands for
fn action(name: &str, b: &Value) -> Option<Request> {
    let num = |k: &str| b.get(k).and_then(Value::as_f64).filter(|v| v.is_finite());
    Some(match name {
        "tap" => Request::Tap { x: num("x")?, y: num("y")? },
        "long_press" => Request::LongPress { x: num("x")?, y: num("y")?, seconds: num("seconds")?.clamp(0.3, 5.0) },
        "swipe" => Request::Swipe {
            from: (num("x1")?, num("y1")?),
            to: (num("x2")?, num("y2")?),
            seconds: num("seconds")?.clamp(0.05, 3.0),
        },
        "type" => Request::TypeText {
            text: b.get("text")?.as_str()?.to_owned(),
            submit: b.get("submit").and_then(Value::as_bool).unwrap_or(false),
        },
        "button" => Request::PressButton { button: b.get("button")?.as_str()?.to_owned() },
        _ => return None,
    })
}

async fn run_action(socket: &Path, req: Request) -> Value {
    match ictl_daemon::protocol::call(socket, &req, Some(Duration::from_secs(120))).await {
        Ok(Response::Text { text, .. }) => json!({"ok": true, "message": text}),
        Ok(Response::Image { caption, .. }) => json!({"ok": true, "message": caption}),
        Ok(Response::Error { message }) => json!({"ok": false, "message": message}),
        Err(e) => json!({"ok": false, "message": e.to_string()}),
    }
}

/// Screen size in points (portrait) for turning a click on the picture into a tap
async fn info(socket: &Path) -> Value {
    match crate::snapshot(socket).await {
        Ok(s) => {
            let screen = s.persisted.screen.map(|s| s.portrait());
            json!({
                "width": screen.map(|s| s.width),
                "height": screen.map(|s| s.height),
                "state": s.state,
                "device": s.device.map(|d| d.name),
            })
        }
        Err(e) => json!({"error": e.to_string()}),
    }
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
        assert_eq!(action("tap", &json!({"x": 1})), None);
        assert_eq!(action("launch", &json!({})), None);
    }

    #[test]
    fn parses_request_head() {
        let h = head("GET /stream?t=1 HTTP/1.1\r\nHost: localhost:8200\r\n\r\n");
        assert_eq!((h.method.as_str(), h.path.as_str()), ("GET", "/stream"));
        assert!(Head::parse(b"POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n").is_none());
    }
}
