//! `iphone-control wda-bridge`: loopback TCP bridge to WebDriverAgent on port 8100, with Host checks.

use std::path::Path;
use std::time::Duration;

use anyhow::bail;

use crate::client::target_device;

/// 显式开启：127.0.0.1:port → 手机上的 WDA 8100。只绑本机回环地址。
/// 本机的其他用户、浏览器里的网页都能访问到，而 WDA 没有认证。
/// 浏览器那条路（DNS rebinding）靠检查 Host 头挡住；本机其他用户挡不住，用完就停。
pub(crate) async fn wda_bridge(socket: &Path, port: u16) -> anyhow::Result<()> {
    // 和守护进程控制同一台（WDA 只在那台上跑着）
    let target = target_device(socket, None).await?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!(
        "{}",
        ictl_l10n::trf!(
            "WDA 桥接 127.0.0.1:{port} → 手机 {} 的 8100 端口（Ctrl-C 停止）",
            "WDA bridge 127.0.0.1:{port} → port 8100 on phone {} (Ctrl-C to stop)",
            target.udid
        )
    );
    println!(
        "{}",
        ictl_l10n::tr!(
            "注意：本机所有用户都能经这个端口控制手机（WDA 没有认证），用完就停掉。",
            "Note: every user on this machine can control the phone through this port (WDA has no authentication); stop it when you're done."
        )
    );
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (client, _) = listener.accept().await?;
        // 连接太多就不接新的（见 MAX_CONNECTIONS）
        let Ok(slot) = slots.clone().try_acquire_owned() else { continue };
        let (socket, udid) = (socket.to_owned(), target.udid.clone());
        tokio::spawn(async move {
            if let Err(e) = bridge_conn(client, &socket, &udid).await {
                tracing::warn!("{}", ictl_l10n::log_trf!("桥接连接出错：{e}", "Bridge connection error: {e}"));
            }
            drop(slot);
        });
    }
}

/// 先读到第一个请求的头部，Host 必须是本机回环地址才转发：恶意网页借 DNS rebinding 发到 127.0.0.1 的请求，
/// Host 头是它自己的域名。转发经守护进程（`Request::WdaForward`）：控制这台设备的实例开这条连接，任务进行中、
/// 切换设备时它会拒绝或结束（docs/dev/multi-instance-plan.md 4.5），本命令不自己连手机
async fn bridge_conn(mut client: tokio::net::TcpStream, socket: &Path, udid: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let (head, body) = read_head(&mut client).await?;
    if !host_is_loopback(&head) {
        client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    let mut remote = match ictl_daemon::protocol::open_wda_forward(socket, udid).await? {
        Ok(r) => r,
        Err(why) => {
            let body = format!("{why}\n");
            let head = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            client.write_all(head.as_bytes()).await?;
            client.write_all(body.as_bytes()).await?;
            anyhow::bail!("{why}");
        }
    };
    remote.write_all(&head).await?;
    remote.write_all(&body).await?;
    tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}

/// 本机端口（`wda-bridge`、`viewer`）同时接的连接数上限，满了新连接直接断开（比如开了太多标签页）
const MAX_CONNECTIONS: usize = 32;
pub(crate) const MAX_HEAD: usize = 16 * 1024;
/// 客户端发请求头、请求体时单次读的等待上限：连上不发的连接不能一直占着名额
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// 整个请求头（以及 TLS 握手）的时限：每隔几秒发一个字节的连接也不能一直占着名额
pub(crate) const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// 读到请求头结束；返回头部和跟着一起到的那部分请求体
async fn read_head(client: &mut (impl tokio::io::AsyncRead + Unpin)) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    read_head_from(client, Vec::new()).await
}

/// [`read_head`] for the next request on a connection kept open: `data` is what already came of it
async fn read_head_from(
    client: &mut (impl tokio::io::AsyncRead + Unpin),
    data: Vec<u8>,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    use tokio::io::AsyncReadExt;
    let read = async {
        let mut data = data;
        let mut buf = [0u8; 4096];
        loop {
            if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let body = data.split_off(i + 4);
                return Ok((data, body));
            }
            if data.len() > MAX_HEAD {
                bail!("{}", ictl_l10n::tr!("请求头太长", "request headers too long"));
            }
            let n = tokio::time::timeout(READ_TIMEOUT, client.read(&mut buf)).await??;
            if n == 0 {
                bail!("{}", ictl_l10n::tr!("连接已关闭", "connection closed"));
            }
            data.extend_from_slice(&buf[..n]);
        }
    };
    tokio::time::timeout(HEAD_TIMEOUT, read)
        .await
        .map_err(|_| anyhow::anyhow!("{}", ictl_l10n::tr!("请求头没有按时发完", "request headers took too long")))?
}

/// HTTP 请求头里的 Host 是不是 `localhost` / `127.0.0.1` / `[::1]`（端口不限）
fn host_is_loopback(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head);
    let Some(host) = text.split("\r\n").skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("host").then(|| v.trim().to_ascii_lowercase())
    }) else {
        return false;
    };
    let name = match host.strip_prefix('[') {
        Some(rest) => format!("[{}]", rest.split(']').next().unwrap_or_default()),
        None => host.split(':').next().unwrap_or_default().to_owned(),
    };
    matches!(name.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::host_is_loopback;

    #[test]
    fn bridge_only_accepts_loopback_host() {
        assert!(host_is_loopback(b"GET /status HTTP/1.1\r\nHost: 127.0.0.1:8100\r\n\r\n"));
        assert!(host_is_loopback(b"GET / HTTP/1.1\r\nhost: localhost\r\n\r\n"));
        assert!(host_is_loopback(b"GET / HTTP/1.1\r\nHost: [::1]:8100\r\n\r\n"));
        assert!(!host_is_loopback(b"GET / HTTP/1.1\r\nHost: evil.example:8100\r\n\r\n"));
        assert!(!host_is_loopback(b"GET / HTTP/1.1\r\nHost: localhost.evil.example\r\n\r\n"));
        assert!(!host_is_loopback(b"GET / HTTP/1.1\r\n\r\n"));
    }
}
