//! H.264 for the viewer's live picture (`GET /video`). WDA's MJPEG stream is 4 to 10 MB/s, too much for a tunnel
//! over the internet; H.264 of the same frames is about 1/15 of that at the same frame rate and, to the eye, the
//! same picture.
//!
//! Per connection: WDA's MJPEG over usbmuxd → frames equal to the previous one dropped (WDA repeats a still screen
//! about 25 times a second) → the encoder (on macOS VideoToolbox inside this program, `vt.rs`; else ffmpeg
//! ([`ffmpeg`]: hardware encoder first, `mpjpeg` in, FLV out)) → the page as
//! messages of one type byte, a 4-byte big-endian length and the payload:
//!
//! - `0` JSON: `codec` (WebCodecs string), `description` (the `avcC`, hex), `width`/`height` (the frames' size),
//!   `encoder` (its short name) and `stream` (the id for `POST /api/video/ack`). Before the first frame, and again whenever the encoder restarts (rotation, falling
//!   behind)
//! - `1` a keyframe, `2` any other frame, in the `avcC` format
//!
//! Falling behind: buffers between here and the page (the kernel's, an SSH tunnel's window of a few MB) take in
//! frames far faster than a slow link delivers them, so how full the queue here is says nothing; with a 30 KB/s
//! reader the picture was 40 s late. The page therefore reports how many bytes it has read (`POST /api/video/ack`,
//! every 100 ms while frames arrive, [`Acks`]). While the page is behind ([`Flow::behind`]: the oldest frame it
//! hasn't confirmed is older than the usual round trip plus [`SLACK`], or more is on the way than the link delivers
//! in that time), no new picture goes to the encoder; once the page has caught up, only the newest one does. Every encoded frame is still sent, so the chain of frames stays whole and no keyframe is needed (one would
//! take seconds on such a link): the frame rate drops to what the link carries and the picture stays about that
//! close to live. A page that never reports (an older one) only gets the queue check below.
//!
//! The queue to the page full (a page that doesn't report): frames are dropped until a keyframe, and a new encoder
//! session starts for one.

pub mod device;
mod ffmpeg;
mod flv;
mod mjpeg;

/// WDA repeats even still frames. A stalled source should release its tracked feed.
pub(super) const WDA_IDLE: std::time::Duration = std::time::Duration::from_secs(8);
mod sps;
#[cfg(target_os = "macos")]
mod vt;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

pub use ffmpeg::Encoders;
use flv::Packet;

/// `IPHONE_CONTROL_VIEWER_CODEC` allows the phone's own screen video ([`device`]): `auto` (the default) or `device`
pub fn device_allowed() -> bool {
    ictl_daemon::config::env("IPHONE_CONTROL_VIEWER_CODEC")
        .is_none_or(|v| v.eq_ignore_ascii_case("auto") || v.eq_ignore_ascii_case("device"))
}

/// Messages waiting for the page; more means it is falling behind
const QUEUE: usize = 8;
/// Restarting for a keyframe at most this often (a link too slow for the picture would otherwise restart all the
/// time)
const RESTART_EVERY: Duration = Duration::from_secs(1);
/// An encoder that keeps failing on a connection is given up for the next one after this many tries
const TRIES: usize = 3;
/// How much later than usual the page may confirm a frame before frames are skipped
const SLACK: Duration = Duration::from_millis(300);
/// The usual round trip is the shortest seen over this long
const BASE_WINDOW: Duration = Duration::from_secs(10);
/// The link's speed is the fastest delivery seen over this long
const RATE_WINDOW: Duration = Duration::from_secs(5);
/// No report for this long (one got lost, the page is gone or its timers are throttled): stop waiting for them
const REPORTS_STALE: Duration = Duration::from_secs(3);

/// Pages' reports of how much of each picture stream they have read, by stream id
#[derive(Default)]
pub struct Acks(std::sync::Mutex<std::collections::HashMap<String, watch::Sender<u64>>>);

impl Acks {
    /// The page read `bytes` of stream `id` so far. False for a stream that has ended
    pub fn report(&self, id: &str, bytes: u64) -> bool {
        let map = self.0.lock().unwrap_or_else(|p| p.into_inner());
        map.get(id).is_some_and(|tx| {
            tx.send_replace(bytes);
            true
        })
    }

    fn register(self: &Arc<Self>) -> (AckGuard, watch::Receiver<u64>) {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (tx, rx) = watch::channel(0);
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(id.clone(), tx);
        (AckGuard { acks: self.clone(), id }, rx)
    }
}

struct AckGuard {
    acks: Arc<Acks>,
    id: String,
}

impl Drop for AckGuard {
    fn drop(&mut self) {
        self.acks.0.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
    }
}

/// What was sent to the page and what it confirmed
#[derive(Default)]
struct Flow {
    sent: u64,
    /// End offset and time of each message not confirmed yet
    in_flight: VecDeque<(u64, Instant)>,
    /// Bytes confirmed so far, and when the last report with more of them came
    acked: u64,
    last_report: Option<Instant>,
    /// When the last report of any kind came
    heard: Option<Instant>,
    /// Round trips measured over [`BASE_WINDOW`]
    trips: VecDeque<(Instant, Duration)>,
    /// Delivery rates (bytes per second between two reports) over [`RATE_WINDOW`]
    rates: VecDeque<(Instant, f64)>,
    /// The page reports at all
    reports: bool,
}

impl Flow {
    fn sent(&mut self, len: usize, now: Instant) {
        self.sent += len as u64;
        self.in_flight.push_back((self.sent, now));
    }

    fn ack(&mut self, bytes: u64, now: Instant) {
        self.reports = true;
        self.heard = Some(now);
        if bytes > self.acked {
            if let Some(t) = self.last_report {
                let dt = now.saturating_duration_since(t).as_secs_f64();
                if dt > 0.01 {
                    self.rates.push_back((now, (bytes - self.acked) as f64 / dt));
                }
            }
            self.acked = bytes;
            self.last_report = Some(now);
        }
        while self.rates.front().is_some_and(|(t, _)| now.saturating_duration_since(*t) > RATE_WINDOW) {
            self.rates.pop_front();
        }
        let mut last = None;
        while let Some(&(end, at)) = self.in_flight.front()
            && end <= bytes
        {
            last = Some(at);
            self.in_flight.pop_front();
        }
        if let Some(at) = last {
            self.trips.push_back((now, now.saturating_duration_since(at)));
        }
        while self.trips.front().is_some_and(|(t, _)| now.saturating_duration_since(*t) > BASE_WINDOW) {
            self.trips.pop_front();
        }
    }

    /// The page is behind: its oldest unconfirmed message is older than a usual round trip plus [`SLACK`], or more
    /// is on the way than the link delivers in that time (without this, the pictures arriving in the first [`SLACK`]
    /// after catching up would all go out at once). The rate is what was delivered, which on a fast link is only
    /// what was sent; with the round trip in the allowance the normal amount on the way (rate × round trip) stays
    /// under it, so the limit doesn't feed on itself. Nothing on the way is never behind: one frame always goes
    fn behind(&self, now: Instant) -> bool {
        let Some((_, oldest)) = self.in_flight.front() else { return false };
        if !self.reports || self.heard.is_some_and(|t| now.saturating_duration_since(t) > REPORTS_STALE) {
            return false;
        }
        let allowed = self.trips.iter().map(|(_, d)| *d).min().unwrap_or_default() + SLACK;
        if now.saturating_duration_since(*oldest) > allowed {
            return true;
        }
        let rate = self.rates.iter().map(|(_, r)| *r).fold(0.0, f64::max);
        rate > 0.0 && (self.sent - self.acked) as f64 > rate * allowed.as_secs_f64()
    }
}

fn message(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(5 + payload.len());
    m.push(kind);
    m.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    m.extend_from_slice(payload);
    m
}

fn config_message(avcc: &[u8], size: (u32, u32), encoder: &str, stream: &str) -> Option<Vec<u8>> {
    // so the browser's decoder shows each frame at once instead of keeping a few (see `sps.rs`)
    let avcc = sps::low_delay(avcc).unwrap_or_else(|| avcc.to_vec());
    let avcc = avcc.as_slice();
    let hex: String = avcc.iter().map(|b| format!("{b:02x}")).collect();
    let v = json!({
        "stream": stream,
        "codec": flv::codec_string(avcc)?,
        "description": hex,
        "width": size.0,
        "height": size.1,
        "encoder": encoder,
    });
    Some(message(0, v.to_string().as_bytes()))
}

/// Sends the picture of `src` (WDA's MJPEG response) to `client` until either ends. The HTTP head is already sent
pub async fn serve(
    enc: Arc<Encoders>,
    acks: Arc<Acks>,
    src: impl AsyncRead + Unpin + Send + 'static,
    client: TcpStream,
) {
    let (mut client_rd, mut client_wr) = client.into_split();
    let (ack_guard, mut acked) = acks.register();
    let mut flow = Flow::default();

    // the phone's frames, repeated ones dropped; the newest wins if encoding falls behind
    let (frame_tx, mut frames) = watch::channel::<Option<Arc<Vec<u8>>>>(None);
    let reader = tokio::spawn(async move {
        let mut f = mjpeg::Frames::new(src);
        let mut last: Option<Arc<Vec<u8>>> = None;
        loop {
            match tokio::time::timeout(WDA_IDLE, f.next()).await {
                Ok(Ok(Some(j))) => {
                    if last.as_ref().is_some_and(|l| **l == j) {
                        continue;
                    }
                    let j = Arc::new(j);
                    last = Some(j.clone());
                    if frame_tx.send(Some(j)).is_err() {
                        return;
                    }
                }
                Err(_) | Ok(Ok(None)) => return,
                Ok(Err(e)) => {
                    tracing::debug!("MJPEG stream: {e}");
                    return;
                }
            }
        }
    });
    let _reader = AbortOnDrop(reader);

    // the page reads nothing after the request: a read that ends means it went away (a still screen sends nothing,
    // so a failed write wouldn't notice)
    let (gone_tx, mut gone) = tokio::sync::oneshot::channel::<()>();
    let watcher = tokio::spawn(async move {
        let mut b = [0u8; 256];
        while matches!(client_rd.read(&mut b).await, Ok(n) if n > 0) {}
        let _ = gone_tx.send(());
    });
    let _watcher = AbortOnDrop(watcher);

    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if client_wr.write_all(&m).await.is_err() {
                return;
            }
        }
    });
    let _writer = AbortOnDrop(writer);

    let mut session: Option<ffmpeg::Session> = None;
    let mut last: Option<Arc<Vec<u8>>> = None;
    // frames out of the current session; one that fails before any is the encoder's fault
    let mut produced = false;
    let mut failures = 0;
    // dropping frames until a keyframe (the queue to the page was full)
    let mut need_key = false;
    // the page is behind: the newest picture (`last`) waits instead of going to the encoder
    let mut waiting = false;
    let mut restarted = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    loop {
        let ev = tokio::select! {
            _ = &mut gone => return,
            r = frames.changed() => match r {
                Ok(()) => Ev::Frame(frames.borrow_and_update().clone()),
                // the phone's stream ended (unplugged, WDA restarted): the page reconnects
                Err(_) => return,
            },
            p = next_packet(&mut session) => Ev::Packet(p),
            r = acked.changed() => match r {
                Ok(()) => Ev::Acked(*acked.borrow_and_update()),
                Err(_) => return,
            },
            _ = tick.tick() => Ev::Tick,
        };
        match ev {
            Ev::Frame(None) => {}
            Ev::Frame(Some(j)) => {
                let Some(size) = mjpeg::jpeg_size(&j) else { continue };
                // a new size (the screen turned): a new session, whose first frame is a keyframe of that size
                if session.as_ref().is_none_or(|s| s.size != size) {
                    let Some(s) = start(&enc, size) else { return };
                    session = Some(s);
                    produced = false;
                }
                last = Some(j);
                if flow.behind(Instant::now()) {
                    waiting = true;
                } else if let (Some(s), Some(j)) = (&session, &last) {
                    s.push(j.clone());
                }
            }
            Ev::Packet(Some(Ok(Packet::Config(avcc)))) => {
                let (Some(s), Some(label)) = (&session, enc.label()) else { continue };
                let Some(m) = config_message(&avcc, s.size, label, &ack_guard.id) else { continue };
                flow.sent(m.len(), Instant::now());
                // small and needed before any frame: wait for room
                if out_tx.send(m).await.is_err() {
                    return;
                }
            }
            Ev::Packet(Some(Ok(Packet::Frame { key, data }))) => {
                produced = true;
                if need_key && !key {
                    continue;
                }
                need_key = false;
                let m = message(if key { 1 } else { 2 }, &data);
                let len = m.len();
                match out_tx.try_send(m) {
                    Ok(()) => flow.sent(len, Instant::now()),
                    Err(mpsc::error::TrySendError::Full(_)) => need_key = true,
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
            // ffmpeg ended or sent something unreadable
            Ev::Packet(end) => {
                let Some(s) = session.take() else { continue };
                let why = match end {
                    Some(Err(e)) => e,
                    _ => s.error(),
                };
                tracing::debug!("picture encoder ended: {why}");
                failures += 1;
                // never got a frame out (this size, these frames), or keeps failing: try the next encoder
                if (!produced || failures >= TRIES) && !enc.give_up(s.index).await {
                    return;
                }
                if !produced || failures >= TRIES {
                    failures = 0;
                }
                let Some(j) = &last else { continue };
                let Some(size) = mjpeg::jpeg_size(j) else { continue };
                let Some(s) = start(&enc, size) else { return };
                s.push(j.clone());
                session = Some(s);
                produced = false;
            }
            Ev::Acked(bytes) => {
                flow.ack(bytes, Instant::now());
                // caught up: the newest picture, encoded against the last one sent
                if waiting && !flow.behind(Instant::now()) {
                    waiting = false;
                    if let (Some(s), Some(j)) = (&session, &last) {
                        s.push(j.clone());
                    }
                }
            }
            Ev::Tick => {
                // the queue was full: once it has drained, a new session gives a keyframe now instead of at the next
                // scheduled one
                if need_key && out_tx.capacity() == QUEUE && restarted.elapsed() >= RESTART_EVERY {
                    let (Some(j), Some(old)) = (&last, &session) else { continue };
                    let size = old.size;
                    let Some(s) = start(&enc, size) else { return };
                    s.push(j.clone());
                    session = Some(s);
                    produced = false;
                    restarted = Instant::now();
                }
            }
        }
    }
}

enum Ev {
    Frame(Option<Arc<Vec<u8>>>),
    Packet(Option<Result<Packet, String>>),
    Acked(u64),
    Tick,
}

fn start(enc: &Encoders, size: (u32, u32)) -> Option<ffmpeg::Session> {
    enc.start(size.0, size.1).map_err(|e| tracing::debug!("{e}")).ok()
}

/// The MJPEG picture (`GET /stream`, when there is no H.264): the phone's frames with repeated ones dropped (WDA
/// repeats a still screen about 25 times a second, 4 to 10 MB/s for nothing), and only the newest frame when the
/// page reads slower than they come, so a slow link skips frames instead of falling behind more and more. Each frame
/// is followed right away by the next boundary and the next part's headers: Chrome shows a part only once it has
/// those (seen on Chrome 14x), and on a still screen no next frame would come. So the parts carry no
/// `Content-Length` (the next one isn't known yet); browsers cut at the boundary anyway. The HTTP response head is
/// written here
pub async fn serve_mjpeg(
    src: impl AsyncRead + Unpin + Send + 'static,
    mut client: impl tokio::io::AsyncWrite + Unpin,
    idle: Duration,
) {
    let (tx, mut rx) = watch::channel::<Option<Arc<Vec<u8>>>>(None);
    let reader = tokio::spawn(async move {
        let mut f = mjpeg::Frames::new(src);
        let mut last: Option<Vec<u8>> = None;
        while let Ok(Ok(Some(j))) = tokio::time::timeout(idle, f.next()).await {
            if last.as_ref() == Some(&j) {
                continue;
            }
            last = Some(j.clone());
            if tx.send(Some(Arc::new(j))).is_err() {
                return;
            }
        }
    });
    let _reader = AbortOnDrop(reader);
    const NEXT: &[u8] = b"\r\n--ictl\r\nContent-Type: image/jpeg\r\n\r\n";
    let head =
        b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=ictl\r\nCache-Control: no-store\r\n\
                 X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n";
    // the first part's boundary and headers, without the line break that ends a part
    let header = [head.as_slice(), &NEXT[2..]].concat();
    if !matches!(tokio::time::timeout(idle, client.write_all(&header)).await, Ok(Ok(()))) {
        return;
    }
    while rx.changed().await.is_ok() {
        let Some(j) = rx.borrow_and_update().clone() else { continue };
        let mut part = Vec::with_capacity(j.len() + NEXT.len());
        part.extend_from_slice(&j);
        part.extend_from_slice(NEXT);
        if !matches!(tokio::time::timeout(idle, client.write_all(&part)).await, Ok(Ok(()))) {
            return;
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn next_packet(s: &mut Option<ffmpeg::Session>) -> Option<Result<Packet, String>> {
    match s {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page that confirms in time is never behind; one whose confirmations stop coming is, once the oldest
    /// unconfirmed message is older than the usual round trip plus the slack
    #[test]
    fn behind_means_late_beyond_the_usual_round_trip() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut f = Flow::default();
        f.sent(100, ms(0));
        assert!(!f.behind(ms(5000)), "no reports yet: an older page, never throttled");
        f.ack(100, ms(50));
        f.sent(100, ms(100));
        f.sent(100, ms(140));
        f.ack(300, ms(190));
        assert!(!f.behind(ms(1000)), "everything confirmed");
        f.sent(100, ms(1000));
        assert!(!f.behind(ms(1340)), "within 50 ms round trip + 300 ms");
        assert!(f.behind(ms(1360)));
        f.ack(400, ms(1400));
        assert!(!f.behind(ms(1400)));
        // reports stop coming (one got lost): no waiting for them forever
        f.sent(100, ms(1500));
        assert!(f.behind(ms(2000)));
        assert!(!f.behind(ms(4500)), "nothing heard for over 3 s");
    }

    /// On a link that delivers 20 KB/s, a second 20 KB frame waits for the first: 300 ms of that link is 6 KB. On a
    /// link with a 200 ms round trip that keeps up, frames keep flowing
    #[test]
    fn no_more_on_the_way_than_the_link_carries_in_the_slack() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut f = Flow::default();
        f.sent(20_000, ms(0));
        f.ack(2_000, ms(100));
        f.ack(4_000, ms(200));
        assert!(f.behind(ms(250)), "16 KB still on the way, the link does 20 KB/s");
        f.ack(20_000, ms(1000));
        assert!(!f.behind(ms(1000)), "all delivered: the next frame goes");
        // 20 KB every 40 ms (500 KB/s) over a 200 ms round trip, reported every 100 ms as the page does: about
        // 120 KB is always on the way, and that is no reason to wait
        let mut g = Flow::default();
        for i in 0..100u64 {
            let t = i * 40;
            if t >= 200 && t % 100 == 0 {
                // what was sent up to 200 ms ago has arrived
                g.ack(((t - 200) / 40 + 1) * 20_000, ms(t));
            }
            assert!(!g.behind(ms(t)), "at {t} ms");
            g.sent(20_000, ms(t));
        }
    }

    /// A page reading 20 KB/s while the picture changes 15 times a second (several times what it can take, like a
    /// slow link behind an SSH tunnel) stays close to live: once the picture stops changing, its last frame arrives
    /// within a couple of seconds. Without the reports it was 40 s on the phone. Needs ffmpeg and takes about 15 s:
    /// `cargo test -p iphone-control slow_page -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn slow_page_stays_close_to_live() {
        use tokio::io::AsyncWriteExt;
        let enc = match Encoders::detect().await {
            Ok(e) => e,
            Err(m) => panic!("no H.264 encoder: {m}"),
        };
        // 90 different frames from ffmpeg's test source, cut with the same reader the phone's stream goes through;
        // without ffmpeg (macOS) a scrolling test picture from VideoToolbox's JPEG encoder
        let out = tokio::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=s=480x1040:r=15"])
            .args(["-frames:v", "90", "-c:v", "mjpeg", "-pix_fmt", "yuvj420p", "-q:v", "3", "-f", "mpjpeg", "pipe:1"])
            .output()
            .await;
        let mut frames = Vec::new();
        if let Ok(out) = out {
            let mut whole = b"HTTP/1.0 200 OK\r\n\r\n".to_vec();
            whole.extend(out.stdout);
            let mut f = mjpeg::Frames::new(&whole[..]);
            while let Some(j) = f.next().await.unwrap() {
                frames.push(j);
            }
        } else {
            #[cfg(target_os = "macos")]
            for n in 0..90 {
                frames.push(vt::sample_jpeg(480, 1040, n).unwrap());
            }
        }
        assert_eq!(frames.len(), 90);

        // the phone: those frames at 15 a second, then a still screen (the stream stays open)
        let (mut phone, src) = tokio::io::duplex(1 << 20);
        let feeder = tokio::spawn(async move {
            phone.write_all(b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace\r\n\r\n").await.unwrap();
            for j in &frames {
                let head =
                    format!("--BoundaryString\r\nContent-type: image/jpeg\r\nContent-Length: {}\r\n\r\n", j.len());
                phone.write_all(head.as_bytes()).await.unwrap();
                phone.write_all(j).await.unwrap();
                phone.write_all(b"\r\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(66)).await;
            }
            (Instant::now(), phone)
        });

        // the page, over a real TCP connection, reporting what it read every 100 ms like viewer.html
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut page = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
        let (server_side, _) = l.accept().await.unwrap();
        let acks = Arc::new(Acks::default());
        let _server = AbortOnDrop(tokio::spawn(serve(enc, acks.clone(), src, server_side)));
        const RATE: f64 = 20.0 * 1024.0;
        let (mut buf, mut total, mut frames_in) = (Vec::new(), 0u64, 0);
        let (mut stream, mut last_frame, mut reported) = (None::<String>, None::<Instant>, Instant::now());
        let started = Instant::now();
        loop {
            let mut chunk = [0u8; 2048];
            let n = match tokio::time::timeout(Duration::from_secs(4), page.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => n,
                Ok(_) => panic!("stream ended"),
                // nothing for 4 s: done once the picture has stopped
                Err(_) if feeder.is_finished() => break,
                Err(_) => continue,
            };
            total += n as u64;
            buf.extend_from_slice(&chunk[..n]);
            while buf.len() >= 5 {
                let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
                if buf.len() < 5 + len {
                    break;
                }
                if buf[0] == 0 {
                    let v: serde_json::Value = serde_json::from_slice(&buf[5..5 + len]).unwrap();
                    stream = v["stream"].as_str().map(str::to_owned);
                } else {
                    frames_in += 1;
                    last_frame = Some(Instant::now());
                }
                buf.drain(..5 + len);
            }
            if let Some(id) = &stream
                && reported.elapsed() >= Duration::from_millis(100)
            {
                acks.report(id, total);
                reported = Instant::now();
            }
            tokio::time::sleep(Duration::from_secs_f64(n as f64 / RATE)).await;
            assert!(started.elapsed() < Duration::from_secs(60), "took too long");
        }
        let (stopped, _phone) = feeder.await.unwrap();
        let late = last_frame.unwrap().saturating_duration_since(stopped);
        eprintln!("{frames_in} frames, {total} bytes; the last one arrived {late:?} after the picture stopped");
        assert!(late < Duration::from_secs(3), "fell {late:?} behind");
    }

    #[test]
    fn messages_have_type_and_length() {
        assert_eq!(message(2, b"abc"), [2, 0, 0, 0, 3, b'a', b'b', b'c']);
        let m = config_message(&[1, 0x64, 0, 0x28, 0xFF], (968, 2097), "VAAPI", "s1").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&m[5..]).unwrap();
        assert_eq!(v["codec"], "avc1.640028");
        assert_eq!(v["description"], "01640028ff");
        assert_eq!((v["width"].as_u64(), v["height"].as_u64()), (Some(968), Some(2097)));
        assert_eq!(v["stream"], "s1");
    }

    #[tokio::test]
    async fn reports_reach_their_stream_only() {
        let acks = Arc::new(Acks::default());
        let (g, mut rx) = acks.register();
        assert!(acks.report(&g.id, 42));
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 42);
        assert!(!acks.report("other", 1));
        let id = g.id.clone();
        drop(g);
        assert!(!acks.report(&id, 43), "gone with the stream");
    }
}
