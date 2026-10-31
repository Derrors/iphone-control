//! 发往 WDA 的极简 HTTP/1.1。
//!
//! WDA 直接经 usbmuxd 连手机上的端口，本机不监听任何端口（docs/decisions.md D3）。
//! idevice 自带的 `WdaClient` 不支持 `excluded_attributes`、appium settings、W3C actions 等，
//! 所以这里自己发请求：每个请求一条新连接、`Connection: close`，读到 EOF 为止。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use idevice::provider::IdeviceProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 一次 HTTP 请求的传输。抽象出来便于单测里换成假的
pub trait Transport: Send + Sync {
    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> impl Future<Output = std::io::Result<(u16, Vec<u8>)>> + Send;
}

/// 经 usbmuxd 直连手机上 WDA 端口
#[derive(Clone)]
pub struct DeviceTransport {
    provider: Arc<dyn IdeviceProvider>,
    port: u16,
}

impl DeviceTransport {
    pub fn new(provider: Arc<dyn IdeviceProvider>, port: u16) -> Self {
        Self { provider, port }
    }
}

impl Transport for DeviceTransport {
    async fn send(&self, method: &str, path: &str, body: Option<Vec<u8>>, timeout: Duration) -> std::io::Result<(u16, Vec<u8>)> {
        let req = build_request(method, path, body.as_deref());
        let fut = async {
            let idevice = self.provider.connect(self.port).await.map_err(std::io::Error::other)?;
            let mut sock = idevice.get_socket().ok_or_else(|| std::io::Error::other("连接没有套接字"))?;
            sock.write_all(&req).await?;
            sock.flush().await?;
            let mut buf = Vec::new();
            sock.read_to_end(&mut buf).await?;
            parse_response(&buf)
        };
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, format!("请求超时（{} 秒）", timeout.as_secs())))?
    }
}

pub fn build_request(method: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
    let body = body.unwrap_or_default();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    if method != "GET" || !body.is_empty() {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    let mut out = req.into_bytes();
    out.extend_from_slice(body);
    out
}

/// 解析完整的 HTTP 响应：状态码和正文。支持 Content-Length、chunked 和读到 EOF 三种正文
pub fn parse_response(raw: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_owned());
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| bad("HTTP 响应没有头部结束标记"))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad("HTTP 头不是 UTF-8"))?;
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("HTTP 状态行无效"))?;
    let mut length = None;
    let mut chunked = false;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "content-length" {
            length = v.parse::<usize>().ok();
        } else if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
    }
    if chunked {
        return Ok((status, dechunk(body).ok_or_else(|| bad("chunked 正文不完整"))?));
    }
    match length {
        Some(n) if body.len() < n => Err(bad("正文比 Content-Length 短，连接提前断了")),
        Some(n) => Ok((status, body[..n].to_vec())),
        None => Ok((status, body.to_vec())),
    }
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_has_length_only_with_body() {
        let get = String::from_utf8(build_request("GET", "/status", None)).unwrap();
        assert!(get.starts_with("GET /status HTTP/1.1\r\n"));
        assert!(!get.contains("Content-Length"));
        let post = String::from_utf8(build_request("POST", "/session", Some(b"{}"))).unwrap();
        assert!(post.contains("Content-Length: 2\r\n") && post.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn parses_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nServer: x\r\n\r\nhelloEXTRA";
        assert_eq!(parse_response(raw).unwrap(), (200, b"hello".to_vec()));
    }

    #[test]
    fn parses_chunked_body() {
        let raw = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), (404, b"abcde".to_vec()));
    }

    #[test]
    fn body_until_eof_and_truncated_body() {
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\n\r\n{}").unwrap(), (200, b"{}".to_vec()));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").is_err());
        assert!(parse_response(b"garbage").is_err());
    }
}
