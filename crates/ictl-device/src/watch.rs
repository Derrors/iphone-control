//! USB device attach/detach event stream via usbmuxd's `Listen` protocol.
//!
//! Connects via [`crate::usbmuxd_addr`] (honoring `USBMUXD_SOCKET_ADDRESS`), bounds the
//! connect + `Listen` handshake by [`crate::QUERY`], and then reads events indefinitely without a
//! short read timeout. Non-USB (network / unknown) `Attached` events and their matching `Detached`
//! events are filtered out with a bounded per-session set so Wi-Fi churn does not wake the
//! daemon, while unseen `Detached` IDs are still forwarded so real USB unplugs are never missed.

use std::collections::{HashMap, VecDeque};
use std::future::{Future, poll_fn};
use std::time::Duration;

use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdListenEvent};
use tokio::time::Instant;

use crate::{DeviceError, QUERY, Result, UsbDevice};

/// Maximum number of non-USB `device_id`s remembered per `Listen` session so unpaired `Attached`
/// events cannot grow memory without bound.
const MAX_TRACKED_NON_USB: usize = 1024;

/// A USB device hotplug event reported by usbmuxd's `Listen` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbEvent {
    /// A USB device was attached (including initial devices replayed right after `Listen`).
    Attached(UsbDevice),
    /// A device was detached and was not a known non-USB device in this `Listen` session.
    Detached { device_id: u64 },
}

/// Connects to the configured usbmuxd ([`crate::usbmuxd_addr`]), completes the `Listen` handshake
/// within the standard query timeout (15 s), invokes `on_connected`, and then delivers USB device
/// attach/detach events to `on_event` until the stream closes or errors.
pub async fn watch_devices(on_connected: impl FnOnce(), on_event: impl FnMut(UsbEvent)) -> Result<()> {
    watch_devices_at(&crate::usbmuxd_addr(), QUERY, on_connected, on_event).await
}

/// Same as [`watch_devices`], targeting an explicit [`UsbmuxdAddr`] and handshake timeout.
pub async fn watch_devices_at(
    addr: &UsbmuxdAddr,
    handshake_timeout: Duration,
    on_connected: impl FnOnce(),
    mut on_event: impl FnMut(UsbEvent),
) -> Result<()> {
    let deadline = Instant::now() + handshake_timeout;
    // 不用 UsbmuxdConnection::default()：它固定连 /var/run/usbmuxd，不看 USBMUXD_SOCKET_ADDRESS
    let mut mux =
        limit_until(ictl_l10n::text!("连接 usbmuxd", "connect to usbmuxd"), handshake_timeout, deadline, async {
            addr.connect(0).await.map_err(DeviceError::from)
        })
        .await?;
    let mut stream = limit_until(
        ictl_l10n::text!("监听 usbmuxd 设备事件", "listen for usbmuxd device events"),
        handshake_timeout,
        deadline,
        async { mux.listen().await.map_err(DeviceError::from) },
    )
    .await?;
    on_connected();

    let mut filter = UsbEventFilter::new(MAX_TRACKED_NON_USB);
    while let Some(item) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        let ev = item.map_err(DeviceError::from)?;
        if let Some(usb) = filter.filter(ev) {
            on_event(usb);
        }
    }
    Ok(())
}

async fn limit_until<T>(
    what: ictl_l10n::Text,
    budget: Duration,
    deadline: Instant,
    f: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout_at(deadline, f).await.map_err(|_| DeviceError::Timeout(what.with_secs(budget.as_secs())))?
}

/// Filters non-USB `Attached` and matching `Detached` events for one `Listen` session while
/// keeping memory strictly bounded.
#[derive(Debug)]
struct UsbEventFilter {
    non_usb: HashMap<u64, u64>,
    order: VecDeque<(u64, u64)>,
    next_seq: u64,
    cap: usize,
}

impl UsbEventFilter {
    fn new(cap: usize) -> Self {
        Self { non_usb: HashMap::new(), order: VecDeque::new(), next_seq: 0, cap }
    }

    fn filter(&mut self, event: UsbmuxdListenEvent) -> Option<UsbEvent> {
        match event {
            UsbmuxdListenEvent::Connected(dev) => {
                let id = u64::from(dev.device_id);
                if matches!(dev.connection_type, Connection::Usb) {
                    // Clear any stale non-USB entry if a mux ID is ever reused for USB.
                    self.non_usb.remove(&id);
                    Some(UsbEvent::Attached(UsbDevice { udid: dev.udid, device_id: dev.device_id }))
                } else {
                    self.track_non_usb(id);
                    None
                }
            }
            UsbmuxdListenEvent::Disconnected(id) => {
                if self.non_usb.remove(&id).is_some() {
                    None
                } else {
                    Some(UsbEvent::Detached { device_id: id })
                }
            }
        }
    }

    fn track_non_usb(&mut self, id: u64) {
        if self.cap == 0 {
            return;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.non_usb.insert(id, seq);
        self.order.push_back((seq, id));
        while self.non_usb.len() > self.cap {
            let Some((old_seq, old_id)) = self.order.pop_front() else {
                break;
            };
            if self.non_usb.get(&old_id) == Some(&old_seq) {
                self.non_usb.remove(&old_id);
            }
        }
        if self.order.len() > self.cap.saturating_mul(2) {
            self.order.retain(|&(s, id)| self.non_usb.get(&id) == Some(&s));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use idevice::usbmuxd::UsbmuxdDevice;

    use super::*;

    fn dev(id: u32, udid: &str, connection_type: Connection) -> UsbmuxdDevice {
        UsbmuxdDevice { connection_type, udid: udid.into(), device_id: id }
    }

    #[test]
    fn usb_event_filter_drops_network_and_unknown_but_keeps_usb_and_unseen_detach() {
        let mut f = UsbEventFilter::new(16);

        // USB attach & detach pass through.
        assert_eq!(
            f.filter(UsbmuxdListenEvent::Connected(dev(1, "USB-1", Connection::Usb))),
            Some(UsbEvent::Attached(UsbDevice { udid: "USB-1".into(), device_id: 1 }))
        );
        // Network (IPv4/IPv6) and Unknown attach are suppressed and remembered.
        assert_eq!(
            f.filter(UsbmuxdListenEvent::Connected(dev(
                2,
                "WIFI-4",
                Connection::Network(IpAddr::V4(Ipv4Addr::LOCALHOST))
            ))),
            None
        );
        assert_eq!(
            f.filter(UsbmuxdListenEvent::Connected(dev(
                3,
                "WIFI-6",
                Connection::Network(IpAddr::V6(Ipv6Addr::LOCALHOST))
            ))),
            None
        );
        assert_eq!(f.filter(UsbmuxdListenEvent::Connected(dev(4, "OTHER", Connection::Unknown("foo".into())))), None);

        // Matching non-USB detaches are suppressed; USB and unseen detaches pass through.
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(2)), None);
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(3)), None);
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(4)), None);
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(1)), Some(UsbEvent::Detached { device_id: 1 }));
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(99)), Some(UsbEvent::Detached { device_id: 99 }));

        // If an ID once seen as non-USB is later reused by USB, its detach must not be suppressed.
        assert_eq!(
            f.filter(UsbmuxdListenEvent::Connected(dev(
                5,
                "WIFI-5",
                Connection::Network(IpAddr::V4(Ipv4Addr::LOCALHOST))
            ))),
            None
        );
        assert_eq!(
            f.filter(UsbmuxdListenEvent::Connected(dev(5, "USB-5", Connection::Usb))),
            Some(UsbEvent::Attached(UsbDevice { udid: "USB-5".into(), device_id: 5 }))
        );
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(5)), Some(UsbEvent::Detached { device_id: 5 }));
    }

    #[test]
    fn usb_event_filter_bounds_tracked_non_usb_ids() {
        let mut f = UsbEventFilter::new(3);
        // Churn paired non-USB attach/detach cycles to ensure the FIFO queue compacts.
        for id in 1..=20u32 {
            assert_eq!(
                f.filter(UsbmuxdListenEvent::Connected(dev(
                    id,
                    "WIFI",
                    Connection::Network(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                ))),
                None
            );
            assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(u64::from(id))), None);
        }
        assert!(f.non_usb.is_empty());
        assert!(f.order.len() <= 6, "queue must stay bounded, got {}", f.order.len());

        // Unpaired non-USB attaches evict the oldest once past cap.
        for id in 100..=104u32 {
            assert_eq!(
                f.filter(UsbmuxdListenEvent::Connected(dev(
                    id,
                    "WIFI",
                    Connection::Network(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                ))),
                None
            );
        }
        assert_eq!(f.non_usb.len(), 3);
        // Oldest IDs (100, 101) were evicted, so a late detach falls back to forwarding.
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(100)), Some(UsbEvent::Detached { device_id: 100 }));
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(101)), Some(UsbEvent::Detached { device_id: 101 }));
        // Newest IDs (102, 103, 104) are still suppressed.
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(102)), None);
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(103)), None);
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(104)), None);

        // Removing and re-inserting an ID before compaction must not let the stale queue entry evict the fresh one.
        let net =
            |id| UsbmuxdListenEvent::Connected(dev(id, "WIFI", Connection::Network(IpAddr::V4(Ipv4Addr::LOCALHOST))));
        assert_eq!(f.filter(net(200)), None);
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(200)), None);
        assert_eq!(f.filter(net(201)), None);
        assert_eq!(f.filter(net(202)), None);
        assert_eq!(f.filter(net(200)), None);
        assert_eq!(f.filter(net(203)), None); // evicts 201, NOT the fresh 200
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(201)), Some(UsbEvent::Detached { device_id: 201 }));
        assert_eq!(f.filter(UsbmuxdListenEvent::Disconnected(200)), None);
    }

    #[cfg(unix)]
    mod unix_mock {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{UnixListener, UnixStream};

        use super::*;

        async fn read_mux_packet(stream: &mut UnixStream) -> (u32, u32, u32, plist::Dictionary) {
            let mut hdr = [0u8; 16];
            stream.read_exact(&mut hdr).await.unwrap();
            let size = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
            let version = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
            let message = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
            let tag = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
            let mut body = vec![0u8; (size - 16) as usize];
            stream.read_exact(&mut body).await.unwrap();
            let dict: plist::Dictionary = plist::from_bytes(&body).unwrap();
            (version, message, tag, dict)
        }

        async fn write_mux_plist(stream: &mut UnixStream, tag: u32, dict: &plist::Dictionary) {
            let mut xml = Vec::new();
            plist::to_writer_xml(&mut xml, dict).unwrap();
            let size = (xml.len() + 16) as u32;
            let mut pkt = Vec::with_capacity(size as usize);
            pkt.extend_from_slice(&size.to_le_bytes());
            pkt.extend_from_slice(&1u32.to_le_bytes()); // XML plist protocol version
            pkt.extend_from_slice(&8u32.to_le_bytes()); // plist message type
            pkt.extend_from_slice(&tag.to_le_bytes());
            pkt.extend_from_slice(&xml);
            stream.write_all(&pkt).await.unwrap();
        }

        fn result_plist(number: u64) -> plist::Dictionary {
            let mut d = plist::Dictionary::new();
            d.insert("MessageType".into(), plist::Value::String("Result".into()));
            d.insert("Number".into(), plist::Value::Integer(number.into()));
            d
        }

        fn attached_usb_plist(device_id: u32, udid: &str) -> plist::Dictionary {
            let mut props = plist::Dictionary::new();
            props.insert("ConnectionType".into(), plist::Value::String("USB".into()));
            props.insert("SerialNumber".into(), plist::Value::String(udid.into()));
            let mut d = plist::Dictionary::new();
            d.insert("MessageType".into(), plist::Value::String("Attached".into()));
            d.insert("DeviceID".into(), plist::Value::Integer(u64::from(device_id).into()));
            d.insert("Properties".into(), plist::Value::Dictionary(props));
            d
        }

        fn attached_network_plist(device_id: u32, udid: &str) -> plist::Dictionary {
            // Standard 16-byte sockaddr_in (macOS sin_len=0x10, AF_INET=2, 192.168.1.10)
            let sockaddr = vec![0x10, 0x02, 0x00, 0x00, 192, 168, 1, 10, 0, 0, 0, 0, 0, 0, 0, 0];
            let mut props = plist::Dictionary::new();
            props.insert("ConnectionType".into(), plist::Value::String("Network".into()));
            props.insert("SerialNumber".into(), plist::Value::String(udid.into()));
            props.insert("NetworkAddress".into(), plist::Value::Data(sockaddr));
            let mut d = plist::Dictionary::new();
            d.insert("MessageType".into(), plist::Value::String("Attached".into()));
            d.insert("DeviceID".into(), plist::Value::Integer(u64::from(device_id).into()));
            d.insert("Properties".into(), plist::Value::Dictionary(props));
            d
        }

        fn detached_plist(device_id: u64) -> plist::Dictionary {
            let mut d = plist::Dictionary::new();
            d.insert("MessageType".into(), plist::Value::String("Detached".into()));
            d.insert("DeviceID".into(), plist::Value::Integer(device_id.into()));
            d
        }

        /// Short directory under `/tmp` so Unix socket paths stay well below macOS's 104-byte `SUN_LEN`.
        fn short_temp_dir(tag: &str) -> std::path::PathBuf {
            let mut hex = uuid::Uuid::new_v4().simple().to_string();
            hex.truncate(12);
            let dir = std::path::Path::new("/tmp").join(format!("ictl-{tag}-{hex}"));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[tokio::test]
        async fn watch_devices_at_streams_usb_and_filters_network_over_usbmuxd_plist_protocol() {
            let dir = short_temp_dir("mw");
            let sock = dir.join("s.sock");
            let listener = UnixListener::bind(&sock).unwrap();

            let server = tokio::spawn(async move {
                let (mut conn, _) = listener.accept().await.unwrap();
                let (version, message, tag, req) = read_mux_packet(&mut conn).await;
                assert_eq!((version, message), (1, 8));
                assert_eq!(req.get("MessageType").and_then(|v| v.as_string()), Some("Listen"));
                write_mux_plist(&mut conn, tag, &result_plist(0)).await;
                write_mux_plist(&mut conn, 0, &attached_usb_plist(1, "UDID-USB-1")).await;
                write_mux_plist(&mut conn, 0, &attached_network_plist(2, "UDID-WIFI-2")).await;
                write_mux_plist(&mut conn, 0, &detached_plist(2)).await;
                write_mux_plist(&mut conn, 0, &detached_plist(99)).await;
                write_mux_plist(&mut conn, 0, &detached_plist(1)).await;
                drop(conn);
            });

            let addr = UsbmuxdAddr::UnixSocket(sock.to_string_lossy().into_owned());
            let connected = Arc::new(AtomicUsize::new(0));
            let connected_ref = connected.clone();
            let mut events = Vec::new();
            let res = watch_devices_at(
                &addr,
                Duration::from_secs(5),
                move || {
                    connected_ref.fetch_add(1, Ordering::Relaxed);
                },
                |ev| events.push(ev),
            )
            .await;

            // EOF from the peer ends the stream with a socket error (or Ok if upstream ever yields None).
            assert!(
                matches!(res, Ok(()) | Err(DeviceError::Idevice(_))),
                "expected clean stream end or socket error on EOF, got {res:?}"
            );
            assert_eq!(connected.load(Ordering::Relaxed), 1);
            assert_eq!(
                events,
                vec![
                    UsbEvent::Attached(UsbDevice { udid: "UDID-USB-1".into(), device_id: 1 }),
                    UsbEvent::Detached { device_id: 99 },
                    UsbEvent::Detached { device_id: 1 },
                ]
            );
            server.await.unwrap();
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn watch_devices_at_times_out_when_handshake_stalls() {
            let dir = short_temp_dir("ms");
            let sock = dir.join("s.sock");
            let listener = UnixListener::bind(&sock).unwrap();

            let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::spawn(async move {
                let (_conn, _) = listener.accept().await.unwrap();
                let _ = hold_rx.await;
            });

            let addr = UsbmuxdAddr::UnixSocket(sock.to_string_lossy().into_owned());
            let mut connected = false;
            let err = watch_devices_at(&addr, Duration::from_millis(150), || connected = true, |_| {})
                .await
                .expect_err("stalled Listen handshake must time out");

            assert!(!connected);
            assert!(matches!(err, DeviceError::Timeout(_)), "expected Timeout, got {err:?}");
            let _ = hold_tx.send(());
            server.await.unwrap();
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn watch_devices_at_errors_when_listen_rejected() {
            let dir = short_temp_dir("mr");
            let sock = dir.join("s.sock");
            let listener = UnixListener::bind(&sock).unwrap();

            let server = tokio::spawn(async move {
                let (mut conn, _) = listener.accept().await.unwrap();
                let (_, _, tag, _) = read_mux_packet(&mut conn).await;
                write_mux_plist(&mut conn, tag, &result_plist(2)).await;
            });

            let addr = UsbmuxdAddr::UnixSocket(sock.to_string_lossy().into_owned());
            let mut connected = false;
            let err = watch_devices_at(&addr, Duration::from_secs(5), || connected = true, |_| {})
                .await
                .expect_err("non-zero Listen Result must error");

            assert!(!connected);
            assert!(matches!(err, DeviceError::Idevice(_)), "expected Idevice error, got {err:?}");
            server.await.unwrap();
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
