//! 发往 WDA 的极简 HTTP/1.1。
//!
//! WDA 直接经 usbmuxd 连手机上的端口，本机不监听任何端口（docs/decisions.md D3）。
//! idevice 自带的 `WdaClient` 不支持 `excluded_attributes`、appium settings、W3C actions 等，
//! 所以这里自己发请求：每个请求一条新连接，读够一个完整响应（Content-Length / chunked 结束块）就关掉。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use idevice::provider::IdeviceProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 一次 HTTP 请求的传输。抽象出来便于单测里换成假的。
///
/// 错误的 kind 区分请求有没有送到 WDA（决定能不能退到 HID 重做）：
/// [`ErrorKind::TimedOut`](std::io::ErrorKind::TimedOut) 是发出后没在时限内应答，
/// [`AFTER_SENT`] 是读应答出错，[注入请求写入失败](WRITE_FAILED) 也可能已经送达；其他 kind 都表示请求没送到
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

/// 建立到 WDA 端口的连接的时限。连不上（或连接超时）算 WDA 不可用，调用方可以退到 HID；
/// 连上之后的超时是「请求已发出、手机可能还在执行」，不能退到 HID 重做一遍
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Writing or flushing failed after opening the device connection.
pub const WRITE_FAILED: std::io::ErrorKind = std::io::ErrorKind::BrokenPipe;

/// Failure while reading the reply to a written request.
pub const AFTER_SENT: std::io::ErrorKind = std::io::ErrorKind::ConnectionAborted;

fn after_sent(e: std::io::Error) -> std::io::Error {
    std::io::Error::new(AFTER_SENT, e.to_string())
}

impl Transport for DeviceTransport {
    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        let t0 = std::time::Instant::now();
        let r = self.send_request(method, path, body.as_deref(), timeout).await;
        match &r {
            Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | AFTER_SENT) => {
                tracing::warn!("{}", request_log(method, path, t0.elapsed(), &r));
            }
            _ => tracing::debug!("{}", request_log(method, path, t0.elapsed(), &r)),
        }
        r
    }
}

/// 诊断只记录方法、短路径、耗时与状态，不记录请求正文或查询参数。
fn request_log(method: &str, path: &str, took: Duration, result: &std::io::Result<(u16, Vec<u8>)>) -> String {
    let short = path.split('?').next().unwrap_or(path);
    let secs = took.as_secs_f64();
    match result {
        Ok((status, _)) => ictl_l10n::log_trf!(
            "wda {method} {short} → {status}（{secs:.2}s）",
            "wda {method} {short} → {status} ({secs:.2}s)"
        ),
        Err(e) => ictl_l10n::log_trf!(
            "wda {method} {short} 失败（{secs:.2}s）：{e}",
            "wda {method} {short} failed ({secs:.2}s): {e}"
        ),
    }
}

impl DeviceTransport {
    /// 将所有提前返回的连接错误和请求超时交回外层，保证这些失败也能留下诊断日志。
    async fn send_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        let req = build_request(method, path, body);
        let idevice = tokio::time::timeout(CONNECT_TIMEOUT, self.provider.connect(self.port))
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    ictl_l10n::tr!("连接 WDA 端口超时", "connecting to the WDA port timed out"),
                )
            })?
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotConnected, e.to_string()))?;
        let mut sock = idevice.get_socket().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                ictl_l10n::tr!("连接没有套接字", "the connection has no socket"),
            )
        })?;
        let mut written = false;
        let fut = async {
            // After writing starts, a failed write/flush isn't evidence that the phone didn't act.
            let write_failed = |e: std::io::Error| std::io::Error::new(WRITE_FAILED, e);
            sock.write_all(&req).await.map_err(write_failed)?;
            sock.flush().await.map_err(write_failed)?;
            written = true;
            let buf = read_response(&mut sock, MAX_RESPONSE_BYTES).await.map_err(after_sent)?;
            parse_response(buf).map_err(after_sent)
        };
        tokio::time::timeout(timeout, fut).await.map_err(|_| {
            std::io::Error::new(
                if written { std::io::ErrorKind::TimedOut } else { WRITE_FAILED },
                ictl_l10n::trf!("请求超时（{} 秒）", "request timed out ({} s)", timeout.as_secs_f64()),
            )
        })?
    }
}

/// 一个 WDA 响应最多这么大（头部加正文）。最大的是 iPad 的截图（base64 的 PNG，十几 MB）和大界面的 `/source`；
/// 超过就当坏了，不跟着对方给的 Content-Length 无限分配
pub(crate) const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// 读够一个完整响应就停：WDA 收到 `Connection: close` 也不关连接，不能读到 EOF。
/// 头部只解析一次；正文按 Content-Length 判断，chunked 只在收到结束块后才完整解一次。
/// 超过 `limit`（含 Content-Length 声明的长度）返回 `InvalidData`
pub(crate) async fn read_response<R: tokio::io::AsyncRead + Unpin>(
    sock: &mut R,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let too_large = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            ictl_l10n::trf!("WDA 的响应超过 {} MB", "WDA's response is over {} MB", limit >> 20),
        )
    };
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut head: Option<(usize, Framing)> = None;
    loop {
        if head.is_none() {
            head = parse_head(&buf);
            // no end of the head in its first 64 KB: it isn't coming (and looking again after every read would scan
            // those 64 KB over and over up to the limit)
            if head.is_none() && buf.len() > MAX_HEAD {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    ictl_l10n::tr!("WDA 的响应头超过 64 KB", "WDA's response head is over 64 KB"),
                ));
            }
            if let Some((split, Framing::Length(n))) = head {
                if split.saturating_add(n) > limit {
                    return Err(too_large());
                }
                buf.reserve((split + n).saturating_sub(buf.len()));
            }
        }
        if let Some((split, framing)) = head
            && framing.complete(&buf[split..])
        {
            break;
        }
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if buf.len() + n > limit {
            return Err(too_large());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

pub(crate) fn build_request(method: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
    let body = body.unwrap_or_default();
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAccept: application/json\r\n");
    if method != "GET" || !body.is_empty() {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    let mut out = req.into_bytes();
    out.extend_from_slice(body);
    out
}

/// 响应正文怎么定长
#[derive(Debug, Clone, Copy, PartialEq)]
enum Framing {
    Length(usize),
    Chunked,
    /// 两者都没有：只能读到对方关连接
    UntilEof,
}

impl Framing {
    fn complete(self, body: &[u8]) -> bool {
        match self {
            Self::Length(n) => body.len() >= n,
            // 先看结尾是不是空行（结束块或 trailer 之后），是了才完整解一遍（避免每读 64KB 就把整个正文解一遍）
            Self::Chunked => body.ends_with(b"\r\n\r\n") && dechunk(body).is_some(),
            Self::UntilEof => false,
        }
    }
}

/// 响应头最长这么多（WDA 的头不到 1 KB）
const MAX_HEAD: usize = 64 * 1024;

/// 头部收齐了就返回 (正文起点, 定长方式)。头部很短，只在前 64KB 里找结束标记
fn parse_head(raw: &[u8]) -> Option<(usize, Framing)> {
    let window = &raw[..raw.len().min(MAX_HEAD)];
    let split = window.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let mut framing = Framing::UntilEof;
    for l in head.split("\r\n").skip(1) {
        let Some((k, v)) = l.split_once(':') else { continue };
        match (k.trim(), v.trim()) {
            ("content-length", n) => {
                if let Ok(n) = n.parse() {
                    framing = Framing::Length(n);
                }
            }
            ("transfer-encoding", "chunked") => return Some((split + 4, Framing::Chunked)),
            _ => {}
        }
    }
    Some((split + 4, framing))
}

/// 已经收到一个完整的响应了吗：头部结束，并且正文按 Content-Length 收齐或 chunked 收到结束块。
/// 两者都没有时只能等对方关连接（返回 false 继续读）。读循环里直接用 [`parse_head`] + [`Framing::complete`]
#[cfg(test)]
fn response_complete(raw: &[u8]) -> bool {
    parse_head(raw).is_some_and(|(split, f)| f.complete(&raw[split..]))
}

/// 解析完整的 HTTP 响应：状态码和正文。支持 Content-Length、chunked 和读到 EOF 三种正文。
/// 拿走缓冲区，正文原地截取不再复制（截图正文约 11MB）
pub(crate) fn parse_response(raw: impl Into<Vec<u8>>) -> std::io::Result<(u16, Vec<u8>)> {
    let mut raw = raw.into();
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_owned());
    let (split, framing) = parse_head(&raw)
        .ok_or_else(|| bad(ictl_l10n::tr!("HTTP 响应没有头部结束标记", "HTTP response has no end of headers")))?;
    let status: u16 = std::str::from_utf8(&raw[..split])
        .map_err(|_| bad(ictl_l10n::tr!("HTTP 头不是 UTF-8", "HTTP headers are not UTF-8")))?
        .split("\r\n")
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad(ictl_l10n::tr!("HTTP 状态行无效", "invalid HTTP status line")))?;
    match framing {
        Framing::Chunked => Ok((
            status,
            dechunk(&raw[split..])
                .ok_or_else(|| bad(ictl_l10n::tr!("chunked 正文不完整", "incomplete chunked body")))?,
        )),
        Framing::Length(n) if raw.len() - split < n => Err(bad(ictl_l10n::tr!(
            "正文比 Content-Length 短，连接提前断了",
            "body shorter than Content-Length, the connection closed early"
        ))),
        Framing::Length(n) => {
            raw.truncate(split + n);
            raw.drain(..split);
            Ok((status, raw))
        }
        Framing::UntilEof => {
            raw.drain(..split);
            Ok((status, raw))
        }
    }
}

/// The chunked body's data; `None` until it is whole: each chunk followed by its CRLF, the last one (size 0) by any
/// trailers and the empty line that ends them. A size too large for memory (`size + 2` overflowed) isn't one
fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&body[..line_end]).ok()?;
        // `size;extension=value`
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            // trailers, each a line, until an empty one
            loop {
                let end = body.windows(2).position(|w| w == b"\r\n")?;
                if end == 0 {
                    return Some(out);
                }
                body = &body[end + 2..];
            }
        }
        out.extend_from_slice(body.get(..size)?);
        if body.get(size..size.checked_add(2)?)? != b"\r\n" {
            return None;
        }
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_request_log_keeps_endpoint_and_time_without_query() {
        for kind in [std::io::ErrorKind::TimedOut, AFTER_SENT, std::io::ErrorKind::NotConnected] {
            let result = Err(std::io::Error::new(kind, "test failure"));
            let line = request_log("POST", "/session/A/elements?text=private", Duration::from_millis(1250), &result);
            assert!(line.contains("POST /session/A/elements"));
            assert!(line.contains("1.25s"));
            assert!(line.contains("test failure"));
            assert!(!line.contains("private"));
            assert!(!line.contains('?'));
        }
    }

    #[test]
    fn successful_request_log_does_not_include_response_body() {
        let result = Ok((200, b"private response".to_vec()));
        let line = request_log("GET", "/status", Duration::from_millis(10), &result);
        assert!(line.contains("GET /status"));
        assert!(line.contains("200"));
        assert!(line.contains("0.01s"));
        assert!(!line.contains("private response"));
    }

    #[test]
    fn request_has_length_only_with_body() {
        let get = String::from_utf8(build_request("GET", "/status", None)).unwrap();
        assert!(get.starts_with("GET /status HTTP/1.1\r\n"));
        assert!(!get.contains("Content-Length"));
        let post = String::from_utf8(build_request("POST", "/session", Some(b"{}"))).unwrap();
        assert!(post.contains("Content-Length: 2\r\n") && post.ends_with("\r\n\r\n{}"));
    }

    #[tokio::test]
    async fn oversized_responses_are_refused() {
        // declared too large: refused before reading the body
        let mut declared: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nabc";
        let e = read_response(&mut declared, 100).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        // no length (read until EOF) and more than the limit arrives
        let big = [b"HTTP/1.1 200 OK\r\n\r\n".as_slice(), &[b'x'; 200]].concat();
        let e = read_response(&mut big.as_slice(), 100).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        // within the limit: read whole, the rest of the stream left alone
        let mut ok: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        let raw = read_response(&mut ok, 100).await.unwrap();
        assert_eq!(parse_response(raw).unwrap(), (200, b"{}".to_vec()));
    }

    #[test]
    fn parses_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nServer: x\r\n\r\nhelloEXTRA";
        assert_eq!(parse_response(raw).unwrap(), (200, b"hello".to_vec()));
    }

    /// Chunk extensions and trailers are read through; a size that would overflow, or a chunk not followed by its
    /// CRLF, isn't a body; the end is only taken once the trailers' empty line came
    #[test]
    fn chunked_edges() {
        assert_eq!(dechunk(b"4;ext=1\r\nWiki\r\n0\r\nX-Checksum: 1\r\n\r\n").unwrap(), b"Wiki");
        assert_eq!(dechunk(b"0\r\n\r\n").unwrap(), b"");
        assert!(dechunk(b"0\r\nX-Checksum: 1\r\n").is_none(), "the trailers' end not there yet");
        assert!(dechunk(b"ffffffffffffffff\r\nabc").is_none(), "no overflow, no panic");
        assert!(dechunk(b"4\r\nWikiXX0\r\n\r\n").is_none(), "a chunk runs on past its size");
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\nX-T: 1\r\n\r\n";
        assert!(response_complete(raw), "complete with a trailer");
        assert_eq!(parse_response(raw.to_vec()).unwrap(), (200, b"ok".to_vec()));
    }

    /// A head that doesn't end within 64 KB is refused at once instead of scanned again after every read
    #[tokio::test]
    async fn an_endless_head_is_refused() {
        let mut endless = b"HTTP/1.1 200 OK\r\n".to_vec();
        endless.extend(std::iter::repeat_n(b'x', 200 * 1024));
        let e = read_response(&mut &endless[..], MAX_RESPONSE_BYTES).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn parses_chunked_body() {
        let raw = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), (404, b"abcde".to_vec()));
    }

    #[test]
    fn completeness_does_not_wait_for_eof() {
        // WDA 回 Content-Length 但不关连接：收齐正文就算完整
        assert!(response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{}"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Len"));
        assert!(response_complete(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n"));
        // 两者都没有：只能读到 EOF
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\n\r\n{}"));
    }

    #[test]
    fn body_until_eof_and_truncated_body() {
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\n\r\n{}").unwrap(), (200, b"{}".to_vec()));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").is_err());
        assert!(parse_response(b"garbage").is_err());
    }
}
