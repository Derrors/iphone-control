//! Wakes the daemon's supervision loop immediately on USB attach/detach events from usbmuxd.
//!
//! Runs alongside the 10-second `tick` interval in [`crate::serve`]: a USB event from usbmuxd's
//! `Listen` stream calls `wake.notify_one()` so hotplug discovery drops from up to 10 s to
//! milliseconds. When usbmuxd is not running (normal on Linux with no device plugged in) or the
//! stream drops, this task reconnects with exponential backoff and logs at `debug` level while the
//! periodic `tick` remains the fallback.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ictl_device::UsbEvent;
#[cfg(test)]
use ictl_device::UsbmuxdAddr;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Initial reconnect delay when usbmuxd `Listen` fails or drops.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
/// Maximum reconnect delay for usbmuxd `Listen`.
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Minimum time a `Listen` connection must stay up before a disconnect resets backoff to
/// [`BACKOFF_MIN`].
///
/// Resetting on any received event would defeat backoff if usbmuxd is crash-looping, because
/// `Listen` immediately replays an initial `Attached` event for every already-plugged device.
const STABLE_AFTER: Duration = Duration::from_secs(10);

/// Reconnect timing for the usbmuxd watcher.
#[derive(Debug, Clone, Copy)]
struct Policy {
    min_backoff: Duration,
    max_backoff: Duration,
    stable_after: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self { min_backoff: BACKOFF_MIN, max_backoff: BACKOFF_MAX, stable_after: STABLE_AFTER }
    }
}

/// Which usbmuxd endpoint the watcher connects to.
enum Target {
    System,
    #[cfg(test)]
    Custom {
        addr: UsbmuxdAddr,
        handshake_timeout: Duration,
    },
}

/// Exponential reconnect backoff for usbmuxd `Listen`.
#[derive(Debug, Clone, Copy)]
struct Backoff {
    current: Duration,
    min: Duration,
    max: Duration,
    stable_after: Duration,
}

impl Backoff {
    fn new(min: Duration, max: Duration, stable_after: Duration) -> Self {
        Self { current: min, min, max, stable_after }
    }

    /// Returns the delay to sleep before the next attempt and advances the backoff state.
    /// A session that stayed connected for at least `stable_after` resets the delay to `min`.
    fn next_delay(&mut self, connected_for: Option<Duration>) -> Duration {
        if connected_for.is_some_and(|d| d >= self.stable_after) {
            self.current = self.min;
        }
        let delay = self.current;
        self.current = self.current.saturating_mul(2).min(self.max);
        delay
    }
}

/// Spawns the background usbmuxd `Listen` watcher that notifies `wake` on USB device events.
///
/// The caller should `abort()` and `await` the returned [`JoinHandle`] during daemon shutdown so
/// the underlying usbmuxd socket closes immediately.
pub(crate) fn spawn(wake: Arc<Notify>) -> JoinHandle<()> {
    spawn_with(wake, Target::System, Policy::default())
}

fn spawn_with(wake: Arc<Notify>, target: Target, policy: Policy) -> JoinHandle<()> {
    tokio::spawn(run(wake, target, policy))
}

async fn run(wake: Arc<Notify>, target: Target, policy: Policy) {
    let mut backoff = Backoff::new(policy.min_backoff, policy.max_backoff, policy.stable_after);
    let mut logged_once = false;
    loop {
        let mut connected_at: Option<Instant> = None;
        let on_connected = || {
            connected_at = Some(Instant::now());
            if !logged_once {
                logged_once = true;
                tracing::info!(
                    "{}",
                    ictl_l10n::log_tr!("已订阅 usbmuxd 设备热插拔事件", "Subscribed to usbmuxd device events")
                );
            } else {
                tracing::debug!(
                    "{}",
                    ictl_l10n::log_tr!("已重新连上 usbmuxd 事件流", "Reconnected to usbmuxd Listen stream")
                );
            }
        };
        let on_event = |ev: UsbEvent| {
            match &ev {
                UsbEvent::Attached(dev) => tracing::debug!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "usbmuxd 事件：USB 设备接入 {}（编号 {}）",
                        "usbmuxd event: USB device attached {} (id {})",
                        dev.udid,
                        dev.device_id
                    )
                ),
                UsbEvent::Detached { device_id } => tracing::debug!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "usbmuxd 事件：设备断开（编号 {}）",
                        "usbmuxd event: device detached (id {})",
                        device_id
                    )
                ),
            }
            wake.notify_one();
        };
        let res = match &target {
            Target::System => ictl_device::watch_devices(on_connected, on_event).await,
            #[cfg(test)]
            Target::Custom { addr, handshake_timeout } => {
                ictl_device::watch_devices_at(addr, *handshake_timeout, on_connected, on_event).await
            }
        };
        let connected_for = connected_at.map(|t| t.elapsed());
        let delay = backoff.next_delay(connected_for);
        match res {
            Ok(()) => tracing::debug!(
                "{}",
                ictl_l10n::log_trf!(
                    "usbmuxd 事件流已关闭，{} 秒后重连",
                    "usbmuxd Listen stream closed, reconnecting in {} s",
                    delay.as_secs()
                )
            ),
            Err(e) => tracing::debug!(
                "{}",
                ictl_l10n::log_trf!(
                    "usbmuxd 事件流断开（{e}），{} 秒后重连",
                    "usbmuxd Listen stream failed ({e}), reconnecting in {} s",
                    delay.as_secs()
                )
            ),
        }
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_backoff_doubles_caps_and_resets_after_stable_session() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30), Duration::from_secs(10));
        assert_eq!(b.next_delay(None), Duration::from_secs(1));
        assert_eq!(b.next_delay(None), Duration::from_secs(2));
        assert_eq!(b.next_delay(None), Duration::from_secs(4));
        assert_eq!(b.next_delay(None), Duration::from_secs(8));
        // A short-lived connection (< stable_after) does NOT reset backoff.
        assert_eq!(b.next_delay(Some(Duration::from_secs(9))), Duration::from_secs(16));
        assert_eq!(b.next_delay(None), Duration::from_secs(30));
        assert_eq!(b.next_delay(None), Duration::from_secs(30));
        // A stable session (>= stable_after) resets backoff to min.
        assert_eq!(b.next_delay(Some(Duration::from_secs(10))), Duration::from_secs(1));
        assert_eq!(b.next_delay(None), Duration::from_secs(2));
    }

    #[cfg(unix)]
    mod unix_mock {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{UnixListener, UnixStream};

        use super::*;
        use crate::{Config, Daemon};

        async fn read_listen_request(stream: &mut UnixStream) -> u32 {
            let mut hdr = [0u8; 16];
            stream.read_exact(&mut hdr).await.unwrap();
            let size = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
            let tag = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
            let mut body = vec![0u8; (size - 16) as usize];
            stream.read_exact(&mut body).await.unwrap();
            let xml = String::from_utf8(body).unwrap();
            assert!(xml.contains("<string>Listen</string>"), "expected Listen request, got:\n{xml}");
            tag
        }

        async fn write_xml_plist(stream: &mut UnixStream, tag: u32, body_xml: &str) {
            let xml = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\
                 <plist version=\"1.0\"><dict>{body_xml}</dict></plist>"
            );
            let bytes = xml.as_bytes();
            let size = (bytes.len() + 16) as u32;
            let mut pkt = Vec::with_capacity(size as usize);
            pkt.extend_from_slice(&size.to_le_bytes());
            pkt.extend_from_slice(&1u32.to_le_bytes());
            pkt.extend_from_slice(&8u32.to_le_bytes());
            pkt.extend_from_slice(&tag.to_le_bytes());
            pkt.extend_from_slice(bytes);
            stream.write_all(&pkt).await.unwrap();
        }

        const RESULT_OK: &str = "<key>MessageType</key><string>Result</string><key>Number</key><integer>0</integer>";

        fn attached_usb(id: u32, udid: &str) -> String {
            format!(
                "<key>MessageType</key><string>Attached</string>\
                 <key>DeviceID</key><integer>{id}</integer>\
                 <key>Properties</key><dict>\
                   <key>ConnectionType</key><string>USB</string>\
                   <key>SerialNumber</key><string>{udid}</string>\
                 </dict>"
            )
        }

        fn attached_network(id: u32, udid: &str) -> String {
            use base64::Engine;
            // Standard 16-byte sockaddr_in (macOS sin_len=0x10, AF_INET=2, 192.168.1.10)
            let sockaddr = [0x10u8, 0x02, 0x00, 0x00, 192, 168, 1, 10, 0, 0, 0, 0, 0, 0, 0, 0];
            let sockaddr_b64 = base64::engine::general_purpose::STANDARD.encode(sockaddr);
            format!(
                "<key>MessageType</key><string>Attached</string>\
                 <key>DeviceID</key><integer>{id}</integer>\
                 <key>Properties</key><dict>\
                   <key>ConnectionType</key><string>Network</string>\
                   <key>SerialNumber</key><string>{udid}</string>\
                   <key>NetworkAddress</key><data>{sockaddr_b64}</data>\
                 </dict>"
            )
        }

        fn detached(id: u64) -> String {
            format!("<key>MessageType</key><string>Detached</string><key>DeviceID</key><integer>{id}</integer>")
        }

        /// End-to-end watcher test against a temporary Unix-socket mock usbmuxd:
        /// 1. Starts before usbmuxd exists and reconnects once the socket appears;
        /// 2. Suppresses Network attach/detach without waking `Daemon::wake`;
        /// 3. Wakes `Daemon::wake` immediately on USB attach;
        /// 4. Reconnects after disconnect and wakes `Daemon::wake` on USB detach;
        /// 5. Aborts cleanly on shutdown, immediately closing the usbmuxd socket.
        #[tokio::test]
        async fn usbmuxd_watcher_wakes_daemon_reconnects_after_drop_and_aborts_cleanly() {
            // Short path under `/tmp` so Unix socket paths stay well below macOS's 104-byte `SUN_LEN`.
            let mut hex = uuid::Uuid::new_v4().simple().to_string();
            hex.truncate(12);
            let dir = std::path::Path::new("/tmp").join(format!("ictl-dw-{hex}"));
            std::fs::create_dir_all(&dir).unwrap();
            let sock = dir.join("s.sock");
            let addr = UsbmuxdAddr::UnixSocket(sock.to_string_lossy().into_owned());

            let daemon = Daemon::new(Config::from_env(&dir));
            let policy = Policy {
                min_backoff: Duration::from_millis(20),
                max_backoff: Duration::from_millis(80),
                stable_after: Duration::from_millis(40),
            };
            let target = Target::Custom { addr, handshake_timeout: Duration::from_secs(2) };

            // Start watcher while socket does not yet exist (exercises connect failure + backoff).
            let watcher = spawn_with(daemon.wake.clone(), target, policy);
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!watcher.is_finished(), "watcher must keep retrying when usbmuxd is absent");

            // Now bind the mock usbmuxd socket.
            let listener = UnixListener::bind(&sock).unwrap();

            // Connection #1: Network churn must not wake; USB attach must wake promptly; then drop.
            let (mut conn1, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                .await
                .expect("watcher should connect once socket appears")
                .unwrap();
            let tag1 = read_listen_request(&mut conn1).await;
            write_xml_plist(&mut conn1, tag1, RESULT_OK).await;

            // Register interest on `wake` before sending network chatter.
            let notified = daemon.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            write_xml_plist(&mut conn1, 0, &attached_network(20, "WIFI-DEV")).await;
            write_xml_plist(&mut conn1, 0, &detached(20)).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(80), &mut notified).await.is_err(),
                "Network attach/detach must not wake the daemon"
            );

            // Now send a real USB attach and verify immediate wake.
            write_xml_plist(&mut conn1, 0, &attached_usb(1, "USB-DEV-1")).await;
            tokio::time::timeout(Duration::from_secs(1), notified)
                .await
                .expect("USB attach must wake Daemon::wake immediately");

            // Drop connection #1 to simulate usbmuxd restart.
            drop(conn1);

            // Connection #2: watcher reconnects after backoff and forwards USB detach.
            let (mut conn2, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                .await
                .expect("watcher should reconnect after stream drop")
                .unwrap();
            let tag2 = read_listen_request(&mut conn2).await;
            write_xml_plist(&mut conn2, tag2, RESULT_OK).await;

            let notified2 = daemon.wake.notified();
            tokio::pin!(notified2);
            notified2.as_mut().enable();
            write_xml_plist(&mut conn2, 0, &detached(1)).await;
            tokio::time::timeout(Duration::from_secs(1), notified2)
                .await
                .expect("USB detach after reconnect must wake Daemon::wake immediately");

            // Shutdown: aborting the watcher must finish promptly and close the peer socket.
            watcher.abort();
            let join = tokio::time::timeout(Duration::from_secs(1), watcher)
                .await
                .expect("watcher must exit immediately on abort");
            assert!(join.unwrap_err().is_cancelled());

            let mut buf = [0u8; 1];
            let n = tokio::time::timeout(Duration::from_secs(1), conn2.read(&mut buf))
                .await
                .expect("usbmuxd socket must close when watcher aborts")
                .unwrap();
            assert_eq!(n, 0, "expected EOF on peer socket after watcher abort");

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
