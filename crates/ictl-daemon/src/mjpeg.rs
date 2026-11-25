//! WDA's MJPEG picture for the control page ([`protocol::Request::MjpegStream`], iOS 17–26).
//!
//! The viewer used to connect to the phone's port 9100 itself, the one place besides `wda-bridge` where something
//! other than the daemon reached for the phone (D2), and one that only worked over USB. Now it asks the daemon, which
//! finds the controlled phone, asks WDA for the stream and passes the answer on byte for byte: the viewer reads it
//! exactly as it read the phone (`video::hub`, one stream per phone for all its pages). The phone's side closes with
//! the page's.
//!
//! No device lock is taken: the stream doesn't touch what the lock protects (the tunnel, WDA's session), and a page
//! shouldn't wait behind a long action to see the screen. The device is the one the published snapshot names.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::core::Daemon;
use crate::protocol::Response;

/// Connecting to WDA's port and sending the request, at most
const OPEN_TIMEOUT: Duration = Duration::from_secs(8);

/// Serves one [`protocol::Request::MjpegStream`] connection until either side closes it
pub(crate) async fn stream(
    d: &Daemon,
    reader: BufReader<OwnedReadHalf>,
    mut w: OwnedWriteHalf,
    expect_udid: Option<String>,
) {
    let (phone, udid) = match tokio::time::timeout(OPEN_TIMEOUT, open(d, expect_udid.as_deref())).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return answer(&mut w, Response::error(e)).await,
        Err(_) => {
            let e = ictl_l10n::tr!("连接 WDA 的画面端口超时", "Connecting to WDA's picture port timed out");
            return answer(&mut w, Response::error(e)).await;
        }
    };
    let mut line = serde_json::to_string(&Response::text("streaming")).unwrap_or_default();
    line.push('\n');
    if w.write_all(line.as_bytes()).await.is_err() {
        return;
    }
    // ends when the daemon switches to another device: the page then asks for that one's picture
    relay(phone, reader, w, d.controlled_changes_from(&udid)).await;
}

async fn answer(w: &mut OwnedWriteHalf, r: Response) {
    let mut line = serde_json::to_string(&r).unwrap_or_default();
    line.push('\n');
    let _ = w.write_all(line.as_bytes()).await;
}

/// The controlled phone's MJPEG stream, asked for: the device the page shows (`expect_udid`) or else none
async fn open(d: &Daemon, expect_udid: Option<&str>) -> Result<(impl AsyncRead + AsyncWrite + Unpin, String), String> {
    let udid = d.controlled_udid().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
    if expect_udid.is_some_and(|u| !u.eq_ignore_ascii_case(&udid)) {
        return Err(ictl_l10n::tr!("正在控制的设备变了", "The controlled device changed").into());
    }
    let device = ictl_device::Device::find(Some(&udid)).await.map_err(|e| e.to_string())?;
    let mut phone = device.connect_port(ictl_wda::MJPEG_PORT).await.map_err(|e| e.to_string())?;
    phone.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").await.map_err(|e| e.to_string())?;
    Ok((phone, udid))
}

/// Passes what the phone sends to the client until either side is done, or `until` (the daemon switched devices).
/// The client sends nothing after its request: end of input from it means it went away (a still screen would
/// otherwise never show that, nothing being written)
async fn relay(
    phone: impl AsyncRead + AsyncWrite + Unpin,
    mut client_r: impl AsyncRead + Unpin,
    mut client_w: impl AsyncWrite + Unpin,
    until: impl std::future::Future<Output = ()>,
) {
    let (mut from_phone, _to_phone) = tokio::io::split(phone);
    tokio::select! {
        () = until => {}
        _ = tokio::io::copy(&mut from_phone, &mut client_w) => {}
        _ = async {
            let mut b = [0u8; 64];
            while matches!(client_r.read(&mut b).await, Ok(n) if n > 0) {}
        } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phone's bytes reach the client as they are; the client going away closes the phone's side, also on a
    /// still screen (nothing to write that would fail)
    #[tokio::test]
    async fn the_phone_s_bytes_pass_through_until_the_page_goes() {
        let (mut phone, phone_end) = tokio::io::duplex(1024);
        let (client, mut page) = tokio::io::duplex(1024);
        let (client_r, client_w) = tokio::io::split(client);
        let relayed = tokio::spawn(relay(phone_end, client_r, client_w, std::future::pending()));
        let part = b"HTTP/1.1 200 OK\r\n\r\n--wda\r\nContent-Length: 4\r\n\r\n\xff\xd8\xff\xd9\r\n";
        phone.write_all(part).await.unwrap();
        let mut got = vec![0u8; part.len()];
        page.read_exact(&mut got).await.unwrap();
        assert_eq!(got, part);
        drop(page);
        tokio::time::timeout(Duration::from_secs(2), relayed).await.expect("ends with the page").unwrap();
        assert_eq!(phone.read(&mut [0]).await.unwrap(), 0, "the phone's side is closed");
    }

    /// The daemon switching to another device ends the relay (the page asks again, for the new one)
    #[tokio::test]
    async fn a_switch_to_another_device_ends_the_stream() {
        let (d, dir) = crate::core::tests::test_daemon("mjpeg-switch");
        d.fake_controlled("phone");
        let (_phone, phone_end) = tokio::io::duplex(1024);
        let (client, _page) = tokio::io::duplex(1024);
        let (client_r, client_w) = tokio::io::split(client);
        let relayed = relay(phone_end, client_r, client_w, d.controlled_changes_from("phone"));
        tokio::pin!(relayed);
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut relayed).await.is_err(), "same device: goes on");
        d.fake_controlled("tablet");
        tokio::time::timeout(Duration::from_secs(1), relayed).await.expect("ended by the switch");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Asked for a device the daemon doesn't control (the page hasn't noticed a switch): refused, nothing opened
    #[tokio::test]
    async fn another_device_than_the_controlled_one_is_refused() {
        let (d, dir) = crate::core::tests::test_daemon("mjpeg-other");
        let e = open(&d, Some("phone")).await.err().unwrap();
        assert!(e.contains("not connected") || e.contains("未连接"), "{e}");
        d.fake_controlled("phone");
        let e = open(&d, Some("tablet")).await.err().unwrap();
        assert!(e.contains("changed") || e.contains("变了"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
