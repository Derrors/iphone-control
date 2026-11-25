//! The phone's own HEVC video as H.264 (`GET /video` when WDA's MJPEG isn't there: iOS 27 driven natively, or WDA
//! down), for pages that can't decode HEVC (Firefox, Chrome on most Linux). ffmpeg decodes the stream the daemon
//! hands on ([`super::device`]), cuts the padding off and encodes it with the encoder in use ([`Encoders`]); the
//! page gets the same messages as the H.264 picture of WDA's MJPEG (see the module docs): a portrait picture, turned
//! by the page from the interface's rotation as before.
//!
//! The phone's frames can't be skipped (each needs the ones before since the last keyframe), so falling behind is
//! met by stopping: frames stop going to ffmpeg while the page is behind or its queue is full, and once it caught up
//! the phone is asked for a keyframe and a new ffmpeg starts from it (its first frame is a keyframe too).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ictl_daemon::protocol::{SCREEN_KEYFRAME, ScreenMsg};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use super::flv::{self, Packet};
use super::{AbortOnDrop, Acks, Encoders, Flow, QUEUE, STREAM_IDLE, config_message, message, send_video};

/// The phone's frames waiting for ffmpeg; more and it can't keep up (a slow software encoder): start over from a
/// keyframe instead of falling further behind
const INPUT_QUEUE: usize = 32;
/// Largest message accepted from the daemon (a keyframe is well under 1 MB)
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// ffmpeg failing this many times in a row on one connection: the page is told (it falls back to MJPEG)
const TRIES: usize = 3;
/// Asking for a keyframe again while none came to start ffmpeg at
const ASK_EVERY: Duration = Duration::from_millis(500);

/// Transcoding pages at once, at most: each runs its own ffmpeg that decodes the phone's 60 fps HEVC and encodes it
/// again, a core or more apiece without a hardware decoder (and hardware encoders take only a few sessions)
pub const MAX_TRANSCODERS: usize = 4;

/// The places for transcoding pages ([`MAX_TRANSCODERS`]), shared by the viewer's connections
pub struct Slots {
    free: Arc<tokio::sync::Semaphore>,
    /// When "all taken" was last logged
    logged: Mutex<Option<Instant>>,
}

impl Default for Slots {
    fn default() -> Self {
        Self::new(MAX_TRANSCODERS)
    }
}

impl Slots {
    pub fn new(n: usize) -> Self {
        Self { free: Arc::new(tokio::sync::Semaphore::new(n)), logged: Mutex::new(None) }
    }

    /// A place for one page, held for as long as it watches; `None` when all are taken (logged once a minute)
    pub fn take(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let slot = self.free.clone().try_acquire_owned().ok();
        if slot.is_none() {
            let mut last = self.logged.lock().unwrap_or_else(|p| p.into_inner());
            if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                *last = Some(Instant::now());
                tracing::warn!(
                    "viewer: {MAX_TRANSCODERS} pages already get the phone's video transcoded to H.264; more get 503 until one closes"
                );
            }
        }
        slot
    }
}

/// One ffmpeg process: HEVC in, H.264 in FLV out
struct Transcoder {
    input: mpsc::Sender<Vec<u8>>,
    out: mpsc::Receiver<Result<Packet, String>>,
    errors: Arc<Mutex<VecDeque<String>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _child: Child,
}

impl Drop for Transcoder {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Transcoder {
    fn spawn(ffmpeg: &PathBuf, args: &[String]) -> Result<Self, String> {
        let mut child = Command::new(ffmpeg)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ictl_l10n::trf!("启动 ffmpeg 失败:{e}", "Starting ffmpeg failed: {e}"))?;
        let (Some(mut stdin), Some(mut stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err("ffmpeg pipes".into());
        };
        let (input, mut rx) = mpsc::channel::<Vec<u8>>(INPUT_QUEUE);
        let writer = tokio::spawn(async move {
            while let Some(f) = rx.recv().await {
                if stdin.write_all(&f).await.is_err() || stdin.flush().await.is_err() {
                    return;
                }
            }
        });
        let (tx, out) = mpsc::channel(16);
        let reader = tokio::spawn(async move {
            let mut p = flv::Parser::default();
            let mut buf = vec![0; 256 * 1024];
            loop {
                let n = match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                p.push(&buf[..n]);
                loop {
                    match p.next() {
                        Ok(Some(pk)) => {
                            if tx.send(Ok(pk)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                }
            }
        });
        let errors = Arc::new(Mutex::new(VecDeque::new()));
        let e = errors.clone();
        let logger = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                tracing::debug!("ffmpeg (HEVC): {l}");
                let mut q = e.lock().unwrap_or_else(|p| p.into_inner());
                if q.len() >= 4 {
                    q.pop_front();
                }
                q.push_back(l);
            }
        });
        Ok(Self { input, out, errors, tasks: vec![writer, reader, logger], _child: child })
    }

    /// Why it ended
    fn error(&self) -> String {
        let q = self.errors.lock().unwrap_or_else(|p| p.into_inner());
        if q.is_empty() { "ffmpeg ended".into() } else { q.iter().cloned().collect::<Vec<_>>().join("; ") }
    }
}

/// Asks the phone for a keyframe (through the daemon). False when the stream is gone
async fn ask(w: &mut tokio::net::unix::OwnedWriteHalf, asked: &mut Option<Instant>) -> bool {
    *asked = Some(Instant::now());
    w.write_all(&[SCREEN_KEYFRAME]).await.is_ok()
}

async fn next_packet(t: &mut Option<Transcoder>) -> Option<Result<Packet, String>> {
    match t {
        Some(t) => t.out.recv().await,
        None => std::future::pending().await,
    }
}

/// The encoder failed [`TRIES`] times in a row without a frame: the next one, as the MJPEG pictures do
/// ([`Encoders::give_up`]), and the command for it. False when there is none, or when ffmpeg only stood in for the
/// encoder in use (the built-in one), whose failure here isn't that encoder's: the page gets the error and may retry
async fn next_encoder(
    enc: &Encoders,
    cmd: &mut super::ffmpeg::HevcCommand,
    points: Option<(f64, f64)>,
    qp: u8,
    failures: &mut usize,
) -> bool {
    let Some(index) = cmd.encoder else { return false };
    if !enc.give_up(index).await {
        return false;
    }
    match enc.hevc_command(points, qp) {
        Ok(c) => {
            *cmd = c;
            *failures = 0;
            true
        }
        Err(_) => false,
    }
}

/// Sends the phone's video from the daemon's screen stream (`src`, `daemon_w` to ask for keyframes) to `client` as
/// H.264, until either ends. `points`: the screen in points (its padding is cut off by it). The HTTP head is already
/// sent
pub async fn serve(
    enc: Arc<Encoders>,
    qp: u8,
    points: Option<(f64, f64)>,
    acks: Arc<Acks>,
    mut src: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    mut daemon_w: tokio::net::unix::OwnedWriteHalf,
    client: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
) {
    let mut cmd = match enc.hevc_command(points, qp) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("no H.264 of the phone's video: {e}");
            return;
        }
    };
    let (ack_guard, mut acked) = acks.register();
    let mut flow = Flow::default();

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

    let mut session: Option<Transcoder> = None;
    // frames out of the current ffmpeg; one that fails before any is not the stream's fault
    let mut produced = false;
    let mut failures: usize = 0;
    // stopped: the page is behind or its queue was full; a keyframe and a new ffmpeg once it caught up
    let mut stopped = false;
    // when a keyframe was last asked for; asked again every [`ASK_EVERY`] until ffmpeg runs
    let mut asked: Option<Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = out_tx.closed() => return,
            m = msgs.recv() => {
                let Some((kind, body)) = m else { return };
                let key = match ScreenMsg::from_byte(kind) {
                    Some(ScreenMsg::Key) => true,
                    Some(ScreenMsg::Delta) => false,
                    _ => continue,
                };
                if stopped {
                    continue;
                }
                if flow.behind(Instant::now()) {
                    stopped = true;
                    session = None;
                    continue;
                }
                if session.is_none() {
                    // a new ffmpeg starts at a keyframe (ffmpeg decodes nothing before one)
                    if !key {
                        continue;
                    }
                    match Transcoder::spawn(&cmd.ffmpeg, &cmd.args) {
                        Ok(t) => {
                            session = Some(t);
                            produced = false;
                        }
                        Err(e) => {
                            tracing::debug!("{e}");
                            return;
                        }
                    }
                }
                let Some(t) = &session else { continue };
                match t.input.try_send(body) {
                    Ok(()) => {}
                    // ffmpeg can't keep up: start over from a keyframe
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        session = None;
                        if !ask(&mut daemon_w, &mut asked).await {
                            return;
                        }
                    }
                    // its input is gone: ffmpeg ended, a failure like any other (it was started again for ever)
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        let why = session.take().as_ref().map(Transcoder::error).unwrap_or_default();
                        tracing::debug!("HEVC to H.264 ended: {why}");
                        failures += 1;
                        if !produced && failures >= TRIES && !next_encoder(&enc, &mut cmd, points, qp, &mut failures).await {
                            return;
                        }
                        if !ask(&mut daemon_w, &mut asked).await {
                            return;
                        }
                    }
                }
            }
            p = next_packet(&mut session) => match p {
                Some(Ok(Packet::Config(avcc))) => {
                    let size = points.map_or((0, 0), |(w, h)| (w.min(h) as u32, w.max(h) as u32));
                    let Some(m) = config_message(&avcc, size, cmd.label, &ack_guard.id) else { continue };
                    flow.sent(m.len(), Instant::now());
                    if out_tx.send(m).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Packet::Frame { key, data })) => {
                    produced = true;
                    failures = 0;
                    let m = message(if key { 1 } else { 2 }, &data);
                    let len = m.len();
                    match out_tx.try_send(m) {
                        Ok(()) => flow.sent(len, Instant::now()),
                        // the page's queue is full: no more until it drained, then from a keyframe
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            stopped = true;
                            session = None;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
                end => {
                    let why = match end {
                        Some(Err(e)) => e,
                        _ => session.as_ref().map(Transcoder::error).unwrap_or_default(),
                    };
                    tracing::debug!("HEVC to H.264 ended: {why}");
                    session = None;
                    failures += 1;
                    if !produced && failures >= TRIES && !next_encoder(&enc, &mut cmd, points, qp, &mut failures).await {
                        return;
                    }
                    if !ask(&mut daemon_w, &mut asked).await {
                        return;
                    }
                }
            },
            r = acked.changed() => {
                if r.is_err() {
                    return;
                }
                flow.ack(*acked.borrow_and_update(), Instant::now());
                if stopped && !flow.behind(Instant::now()) && out_tx.capacity() == QUEUE {
                    stopped = false;
                    if !ask(&mut daemon_w, &mut asked).await {
                        return;
                    }
                }
            }
            _ = tick.tick() => {
                // the queue drained without a report (a page that doesn't report): go on
                if stopped && !flow.behind(Instant::now()) && out_tx.capacity() == QUEUE {
                    stopped = false;
                }
                // waiting for a keyframe to start ffmpeg at: ask (again: one asked for may not come)
                if !stopped
                    && session.is_none()
                    && asked.is_none_or(|t| t.elapsed() >= ASK_EVERY)
                    && !ask(&mut daemon_w, &mut asked).await
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcoding_pages_are_limited_and_their_places_come_back() {
        let slots = Slots::new(2);
        let (a, b) = (slots.take().unwrap(), slots.take().unwrap());
        assert!(slots.take().is_none(), "all taken");
        drop(a);
        let c = slots.take().expect("a page left: its place is free");
        assert!(slots.take().is_none());
        drop((b, c));
    }
}
