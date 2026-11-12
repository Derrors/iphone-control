//! The phone's own screen video (`GET /video?src=device`): the CoreDevice screen media stream (HEVC, iOS 27 and
//! later) as the daemon hands it on ([`ictl_daemon::protocol::Request::ScreenStream`]), passed to the page in the
//! same messages as the H.264 picture (see the module docs). Nothing is encoded here.
//!
//! Differences from the H.264 picture:
//! - the config message has `annexb: true` and no `description`: frames are Annex-B with their parameter sets in the
//!   keyframes, which is how WebCodecs takes HEVC without one;
//! - the picture is the phone's portrait framebuffer padded to multiples of 16 (1320×2868 arrives as 1328×2896): the
//!   page cuts the padding off and turns it by the `rotation` the daemon sends ([`ScreenMsg::Info`], type 3);
//! - falling behind can't be met by encoding fewer frames: frames other than keyframes are dropped until a keyframe,
//!   which is asked for (one is 15–50 KB and arrives about 20 ms later).
//!
//! Browsers that can't decode HEVC (most without hardware decoding: Firefox, Chrome on most Linux) get the H.264
//! picture or MJPEG instead; the page decides (`isConfigSupported`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use ictl_daemon::protocol::{SCREEN_KEYFRAME, ScreenMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::{AbortOnDrop, Acks, Flow, QUEUE, STREAM_IDLE, message, send_video};

/// Asking for keyframes at most this often while dropping frames
const ASK_EVERY: Duration = Duration::from_millis(500);
/// Largest message accepted from the daemon (a keyframe is well under 1 MB)
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// Opens the daemon's screen stream for `udid`. `Err`: why there is none (an older iOS, no device, the phone
/// locked when the tunnel had to be opened…); the caller answers the page with that
pub async fn open(
    socket: &std::path::Path,
    udid: &str,
) -> Result<(tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>, tokio::net::unix::OwnedWriteHalf), String> {
    match ictl_daemon::protocol::open_screen_stream(socket, Some(udid)).await {
        Ok(r) => r,
        Err(e) => Err(e.to_string()),
    }
}

/// Passes the stream to `client` until either ends. The HTTP head is already sent
pub async fn serve(
    mut src: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    mut daemon_w: tokio::net::unix::OwnedWriteHalf,
    acks: Arc<Acks>,
    client: TcpStream,
) {
    let (ack_guard, mut acked) = acks.register();
    let mut flow = Flow::default();

    // messages from the daemon, read on their own so a slow page never stalls the daemon's side
    let (msg_tx, mut msgs) = mpsc::channel::<(u8, Vec<u8>)>(8);
    let reader = tokio::spawn(async move {
        loop {
            let mut head = [0u8; 5];
            if src.read_exact(&mut head).await.is_err() {
                return;
            }
            let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
            if len > MAX_MESSAGE {
                return;
            }
            let mut body = vec![0u8; len];
            if src.read_exact(&mut body).await.is_err() || msg_tx.send((head[0], body)).await.is_err() {
                return;
            }
        }
    });
    let _reader = AbortOnDrop(reader);

    let (out_tx, out_rx) = mpsc::channel(QUEUE);
    let _writer = AbortOnDrop(tokio::spawn(send_video(client, out_rx, STREAM_IDLE)));

    // dropping frames until a keyframe (the page fell behind, or its queue was full)
    let mut need_key = false;
    let mut asked: Option<Instant> = None;
    loop {
        tokio::select! {
            _ = out_tx.closed() => return,
            m = msgs.recv() => {
                let Some((kind, body)) = m else { return };
                let now = Instant::now();
                match ScreenMsg::from_byte(kind) {
                    Some(ScreenMsg::Config) => {
                        let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&body) else { continue };
                        v["stream"] = ack_guard.id.clone().into();
                        v["annexb"] = true.into();
                        v["description"] = "".into();
                        v["encoder"] = "iPhone".into();
                        let m = message(0, v.to_string().as_bytes());
                        flow.sent(m.len(), now);
                        if out_tx.send(m).await.is_err() {
                            return;
                        }
                    }
                    Some(ScreenMsg::Info) => {
                        let m = message(3, &body);
                        flow.sent(m.len(), now);
                        if out_tx.send(m).await.is_err() {
                            return;
                        }
                    }
                    Some(k @ (ScreenMsg::Key | ScreenMsg::Delta)) => {
                        let key = k == ScreenMsg::Key;
                        if !key && (need_key || flow.behind(now)) {
                            need_key = true;
                            if asked.is_none_or(|t| t.elapsed() >= ASK_EVERY) {
                                asked = Some(now);
                                if daemon_w.write_all(&[SCREEN_KEYFRAME]).await.is_err() {
                                    return;
                                }
                            }
                            continue;
                        }
                        // a keyframe while the page is still behind waits for the next one
                        if key && flow.behind(now) {
                            need_key = true;
                            continue;
                        }
                        let m = message(if key { 1 } else { 2 }, &body);
                        let len = m.len();
                        match out_tx.try_send(m) {
                            Ok(()) => {
                                flow.sent(len, now);
                                if key {
                                    need_key = false;
                                }
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => need_key = true,
                            Err(mpsc::error::TrySendError::Closed(_)) => return,
                        }
                    }
                    None => {}
                }
            }
            r = acked.changed() => {
                if r.is_err() {
                    return;
                }
                let bytes = *acked.borrow_and_update();
                flow.ack(bytes, Instant::now());
                // caught up after dropping frames: a keyframe now rather than at the next request
                if need_key && !flow.behind(Instant::now()) && asked.is_none_or(|t| t.elapsed() >= ASK_EVERY) {
                    asked = Some(Instant::now());
                    if daemon_w.write_all(&[SCREEN_KEYFRAME]).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}
