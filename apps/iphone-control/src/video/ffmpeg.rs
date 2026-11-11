//! The encoder: an ffmpeg process per picture connection, picked from what this machine has, hardware first.
//!
//! ffmpeg runs as a separate program, not linked in: the builds that Linux distributions ship have the hardware
//! encoders, and x264 being GPL doesn't reach this MIT program. Without ffmpeg the page keeps MJPEG.
//!
//! Candidates in order ([`candidates`]): NVENC (NVIDIA), VAAPI on each render node (AMD, Intel; first with the JPEG
//! decoded by the GPU too, then decoded by ffmpeg on the CPU), QSV (Intel), V4L2 M2M (ARM boards), VideoToolbox
//! (macOS, through ffmpeg for now), x264 (software). The first one that encodes a test frame is used; one that fails
//! on the real frames is dropped for the next ([`Encoders::give_up`]).
//!
//! Held frames: ffmpeg keeps the newest frame until the next one arrives (one frame with VAAPI on an AMD 780M:
//! the `mpjpeg` input and the decoder wait for more). The phone's repeated frames are dropped before ffmpeg, so on a
//! screen that stopped changing the last change would wait, up to forever. Each frame is therefore written again
//! `kicks` times right after itself: the copies push it out (16 ms instead of up to seconds on that machine) and
//! encode to about 100 bytes each. `kicks` is measured when an encoder is picked: frames written minus frames out
//! once the output has settled ([`Encoders::calibrate`]).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ictl_daemon::config::env;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::flv::{self, Packet};
use super::mjpeg;

/// Default quantizer: about x264's crf 19, which looks the same as the source in normal use (see the docs)
pub const DEFAULT_QP: u8 = 20;
/// A keyframe every this many frames. Frames only flow while the screen changes, and a page that falls behind gets a
/// keyframe by restarting the encoder, so keyframes are rarely needed and cost about as much as 40 other frames
const GOP: &str = "600";
/// More copies than this would mean the measurement went wrong; the copies cost little, so it is only a cap
const MAX_KICKS: usize = 4;
/// Size of the test frame (any size the real frames could have; small enough to be quick)
const SAMPLE: (u32, u32) = (480, 1040);
/// How long picking one encoder may take, start-up of the GPU included
const CALIBRATE_FOR: Duration = Duration::from_secs(8);

/// What `IPHONE_CONTROL_VIEWER_CODEC` accepts besides `auto` and `mjpeg`
pub const IDS: [&str; 6] = ["nvenc", "vaapi", "qsv", "v4l2m2m", "videotoolbox", "x264"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// NVIDIA; `hw_jpeg`: the JPEG is decoded by the GPU too
    Nvenc { hw_jpeg: bool },
    /// VAAPI on this render node (AMD, Intel)
    Vaapi { node: PathBuf, hw_jpeg: bool },
    /// Intel Quick Sync
    Qsv,
    /// The video encoder of ARM boards (Raspberry Pi 4 and the like)
    V4l2m2m,
    /// macOS
    VideoToolbox,
    /// Software
    X264,
}

impl Kind {
    pub fn id(&self) -> &'static str {
        match self {
            Self::Nvenc { .. } => "nvenc",
            Self::Vaapi { .. } => "vaapi",
            Self::Qsv => "qsv",
            Self::V4l2m2m => "v4l2m2m",
            Self::VideoToolbox => "videotoolbox",
            Self::X264 => "x264",
        }
    }

    /// Short name, shown on the page
    pub fn label(&self) -> &'static str {
        match self {
            Self::Nvenc { .. } => "NVENC",
            Self::Vaapi { .. } => "VAAPI",
            Self::Qsv => "QSV",
            Self::V4l2m2m => "V4L2",
            Self::VideoToolbox => "VideoToolbox",
            Self::X264 => "x264",
        }
    }

    /// The longer description for the terminal
    pub fn describe(&self) -> String {
        let jpeg = |hw: bool| {
            if hw {
                ictl_l10n::tr!("JPEG 也由显卡解码", "JPEG decoded by the GPU too")
            } else {
                ictl_l10n::tr!("JPEG 由 CPU 解码", "JPEG decoded on the CPU")
            }
        };
        match self {
            Self::Nvenc { hw_jpeg } => {
                ictl_l10n::trf!("NVENC 硬件编码({})", "NVENC hardware encoding ({})", jpeg(*hw_jpeg))
            }
            Self::Vaapi { node, hw_jpeg } => ictl_l10n::trf!(
                "VAAPI 硬件编码({},{})",
                "VAAPI hardware encoding ({}, {})",
                node.display(),
                jpeg(*hw_jpeg)
            ),
            Self::Qsv => ictl_l10n::tr!("Intel QSV 硬件编码", "Intel QSV hardware encoding").into(),
            Self::V4l2m2m => ictl_l10n::tr!("V4L2 M2M 硬件编码", "V4L2 M2M hardware encoding").into(),
            Self::VideoToolbox => ictl_l10n::tr!("VideoToolbox 硬件编码", "VideoToolbox hardware encoding").into(),
            Self::X264 => ictl_l10n::tr!("x264 软件编码(占 CPU)", "x264 software encoding (uses the CPU)").into(),
        }
    }
}

/// The encoders worth trying on this machine, best first
pub fn candidates() -> Vec<Kind> {
    let mut v = Vec::new();
    if cfg!(target_os = "macos") {
        v.push(Kind::VideoToolbox);
    }
    if cfg!(target_os = "linux") {
        if Path::new("/dev/nvidiactl").exists() {
            v.extend([Kind::Nvenc { hw_jpeg: true }, Kind::Nvenc { hw_jpeg: false }]);
        }
        let nodes = render_nodes();
        for n in &nodes {
            v.push(Kind::Vaapi { node: n.clone(), hw_jpeg: true });
            v.push(Kind::Vaapi { node: n.clone(), hw_jpeg: false });
        }
        if nodes.iter().any(|n| gpu_vendor(n).as_deref() == Some("0x8086")) {
            v.push(Kind::Qsv);
        }
        if cfg!(any(target_arch = "aarch64", target_arch = "arm"))
            && std::fs::read_dir("/dev")
                .is_ok_and(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("video")))
        {
            v.push(Kind::V4l2m2m);
        }
    }
    v.push(Kind::X264);
    v
}

fn render_nodes() -> Vec<PathBuf> {
    let Ok(d) = std::fs::read_dir("/dev/dri") else { return Vec::new() };
    let mut v: Vec<PathBuf> = d
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("renderD")))
        .collect();
    v.sort();
    v
}

/// PCI vendor of a render node's GPU (`0x8086` Intel, `0x1002` AMD, `0x10de` NVIDIA)
fn gpu_vendor(node: &Path) -> Option<String> {
    let name = node.file_name()?.to_string_lossy().into_owned();
    std::fs::read_to_string(format!("/sys/class/drm/{name}/device/vendor")).ok().map(|s| s.trim().to_owned())
}

/// Bit rate for the encoders that only take one: 4 Mbit/s at qp 20, doubling every 6 steps down (as qp does)
fn bitrate(qp: u8) -> String {
    let kbps = 4000.0 * 2f64.powf((f64::from(DEFAULT_QP) - f64::from(qp)) / 6.0);
    format!("{}k", kbps.round() as u64)
}

/// The full ffmpeg command line (without the program) for frames of `w`×`h`. `old`: ffmpeg before 5.1
pub fn args(kind: &Kind, w: u32, h: u32, qp: u8, old: bool) -> Vec<String> {
    // 4:2:0 needs even sizes (the phone's frames are 968×2097 at 75%): stretched by at most one pixel
    let (w, h) = (w + (w & 1), h + (h & 1));
    let qp_s = qp.to_string();
    let sw_scale = format!("scale={w}:{h},format=nv12");
    let (input, filter, codec): (Vec<String>, String, Vec<String>) = match kind {
        Kind::Nvenc { hw_jpeg } => (
            if *hw_jpeg { strings(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]) } else { Vec::new() },
            if *hw_jpeg { format!("scale_cuda={w}:{h}:format=nv12") } else { sw_scale },
            strings(&[
                "-c:v",
                "h264_nvenc",
                "-preset",
                "p4",
                "-tune",
                "ll",
                "-rc",
                "constqp",
                "-qp",
                &qp_s,
                "-delay",
                "0",
                "-zerolatency",
                "1",
            ]),
        ),
        Kind::Vaapi { node, hw_jpeg } => {
            let node = node.to_string_lossy().into_owned();
            let mut codec = strings(&["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", &qp_s]);
            // the default keeps several frames in flight (about 200 ms on an AMD 780M); the option is from ffmpeg 5
            if !old {
                codec.extend(strings(&["-async_depth", "1"]));
            }
            if *hw_jpeg {
                (
                    strings(&["-hwaccel", "vaapi", "-hwaccel_device", &node, "-hwaccel_output_format", "vaapi"]),
                    format!("scale_vaapi=w={w}:h={h}:format=nv12"),
                    codec,
                )
            } else {
                (strings(&["-vaapi_device", &node]), format!("{sw_scale},hwupload"), codec)
            }
        }
        Kind::Qsv => (
            Vec::new(),
            sw_scale,
            strings(&["-c:v", "h264_qsv", "-global_quality", &qp_s, "-look_ahead", "0", "-async_depth", "1"]),
        ),
        Kind::V4l2m2m => (Vec::new(), sw_scale, strings(&["-c:v", "h264_v4l2m2m", "-b:v", &bitrate(qp)])),
        Kind::VideoToolbox => {
            (Vec::new(), sw_scale, strings(&["-c:v", "h264_videotoolbox", "-realtime", "1", "-b:v", &bitrate(qp)]))
        }
        // crf one below the quantizer gives about the same size and quality as the hardware encoders at that qp
        Kind::X264 => (
            Vec::new(),
            format!("scale={w}:{h}"),
            strings(&[
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-tune",
                "zerolatency",
                "-crf",
                &qp.saturating_sub(1).to_string(),
                "-pix_fmt",
                "yuv420p",
            ]),
        ),
    };
    let mut a = strings(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostats",
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
        "-fflags",
        "nobuffer",
    ]);
    a.extend(input);
    a.extend(strings(&[
        "-f",
        "mpjpeg",
        "-i",
        "pipe:0",
        if old { "-vsync" } else { "-fps_mode" },
        "passthrough",
        "-an",
    ]));
    a.extend(strings(&["-vf", &filter]));
    a.extend(codec);
    a.extend(strings(&[
        "-bf",
        "0",
        "-g",
        GOP,
        "-f",
        "flv",
        "-flvflags",
        "no_duration_filesize",
        "-flush_packets",
        "1",
    ]));
    a.push("pipe:1".into());
    a
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// `IPHONE_CONTROL_VIEWER_QP`: 1 (best, largest) to 51 (worst, smallest); anything else is the default
pub fn qp_from(v: Option<&str>) -> u8 {
    v.and_then(|v| v.parse::<u8>().ok()).filter(|q| (1..=51).contains(q)).unwrap_or(DEFAULT_QP)
}

/// Whether `ffmpeg -version`'s first line names a version before 5.1 (no `-fps_mode`). Builds from git
/// (`N-…`) and anything that doesn't parse count as new
pub fn is_old(version_line: &str) -> bool {
    let Some(v) = version_line.split_whitespace().nth(2) else { return false };
    let v = v.trim_start_matches('n');
    let mut parts = v.split(|c: char| !c.is_ascii_digit());
    let (Some(Ok(major)), minor) = (parts.next().map(str::parse::<u32>), parts.next()) else { return false };
    let minor = minor.and_then(|m| m.parse::<u32>().ok()).unwrap_or(0);
    (major, minor) < (5, 1)
}

/// The one in use and how many copies follow each frame
#[derive(Debug, Clone, Copy)]
struct Current {
    index: usize,
    kicks: usize,
}

/// The encoders of this machine and the one in use, shared by every picture connection
pub struct Encoders {
    ffmpeg: PathBuf,
    qp: u8,
    old: bool,
    list: Vec<Kind>,
    /// A test frame (from ffmpeg itself)
    sample: Arc<Vec<u8>>,
    current: Mutex<Option<Current>>,
    /// Only one picks at a time (picking starts processes and takes a second or two)
    picking: tokio::sync::Mutex<()>,
}

impl Encoders {
    /// Finds and checks an encoder. `Err`: why the page keeps MJPEG (no ffmpeg, none works, or turned off)
    pub async fn detect() -> Result<Arc<Self>, String> {
        let want = env("IPHONE_CONTROL_VIEWER_CODEC").map(|v| v.to_ascii_lowercase());
        let want = want.as_deref().filter(|v| *v != "auto");
        if want == Some("mjpeg") {
            return Err(ictl_l10n::tr!("IPHONE_CONTROL_VIEWER_CODEC=mjpeg", "IPHONE_CONTROL_VIEWER_CODEC=mjpeg").into());
        }
        let mut list = candidates();
        if let Some(w) = want {
            if !IDS.contains(&w) {
                return Err(ictl_l10n::trf!(
                    "IPHONE_CONTROL_VIEWER_CODEC={w} 不认识,可以是 auto、mjpeg、{}",
                    "IPHONE_CONTROL_VIEWER_CODEC={w} isn't known; it can be auto, mjpeg, {}",
                    IDS.join("、")
                ));
            }
            list.retain(|k| k.id() == w);
            if list.is_empty() {
                return Err(ictl_l10n::trf!(
                    "IPHONE_CONTROL_VIEWER_CODEC={w},但这台机器上没有它",
                    "IPHONE_CONTROL_VIEWER_CODEC={w}, but this machine doesn't have it"
                ));
            }
        }
        let mut found = None;
        for p in ictl_daemon::config::ffmpeg_paths() {
            if let Some(v) = version_line(&p).await {
                found = Some((p, v));
                break;
            }
        }
        let (ffmpeg, version) = found.ok_or_else(|| {
            ictl_l10n::tr!(
                "没找到 ffmpeg;装上它画面改用 H.264,流量约为现在的 1/15",
                "ffmpeg not found; with it installed the picture uses H.264 at about 1/15 of the traffic"
            )
            .to_owned()
        })?;
        let sample = Arc::new(sample_jpeg(&ffmpeg).await?);
        let e = Arc::new(Self {
            ffmpeg,
            qp: qp_from(env("IPHONE_CONTROL_VIEWER_QP").as_deref()),
            old: is_old(&version),
            list,
            sample,
            current: Mutex::new(None),
            picking: tokio::sync::Mutex::new(()),
        });
        e.pick_from(0).await?;
        Ok(e)
    }

    fn current(&self) -> Option<Current> {
        *self.current.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The encoder in use (for the page), `None` once none is left
    pub fn label(&self) -> Option<&'static str> {
        self.current().map(|c| self.list[c.index].label())
    }

    /// The encoder in use and its settings (for the terminal)
    pub fn describe(&self) -> String {
        match self.current() {
            Some(c) => ictl_l10n::trf!("{},qp {}", "{}, qp {}", self.list[c.index].describe(), self.qp),
            None => ictl_l10n::tr!("没有可用的编码器", "no working encoder").into(),
        }
    }

    /// Starts an encoder session for frames of `w`×`h`
    pub fn start(&self, w: u32, h: u32) -> Result<Session, String> {
        let c = self.current().ok_or_else(|| ictl_l10n::tr!("没有可用的编码器", "no working encoder").to_owned())?;
        Session::spawn(&self.ffmpeg, &args(&self.list[c.index], w, h, self.qp, self.old), c.kicks, (w, h), c.index)
    }

    /// The encoder at `index` failed on real frames: use the next one that works. False when none is left
    pub async fn give_up(&self, index: usize) -> bool {
        let _one = self.picking.lock().await;
        match self.current() {
            // another connection moved on already
            Some(c) if c.index != index => return true,
            None => return false,
            Some(_) => {}
        }
        tracing::warn!(
            "{}",
            ictl_l10n::log_trf!(
                "画面编码器 {} 处理真实画面失败,换下一个",
                "Picture encoder {} failed on the real frames, trying the next one",
                self.list[index].describe()
            )
        );
        self.pick_inner(index + 1).await.is_ok()
    }

    async fn pick_from(&self, start: usize) -> Result<(), String> {
        let _one = self.picking.lock().await;
        self.pick_inner(start).await
    }

    async fn pick_inner(&self, start: usize) -> Result<(), String> {
        let mut last = String::new();
        for i in start..self.list.len() {
            match self.calibrate(&self.list[i]).await {
                Ok(kicks) => {
                    tracing::debug!("picture encoder {}: {} copies after each frame", self.list[i].id(), kicks);
                    *self.current.lock().unwrap_or_else(|p| p.into_inner()) = Some(Current { index: i, kicks });
                    return Ok(());
                }
                Err(e) => {
                    tracing::debug!("picture encoder {} doesn't work: {e}", self.list[i].id());
                    last = format!("{}: {e}", self.list[i].label());
                }
            }
        }
        *self.current.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Err(ictl_l10n::trf!("没有能用的 H.264 编码器({last})", "No working H.264 encoder ({last})"))
    }

    /// Checks that `kind` encodes, and measures how many frames ffmpeg holds back (see the module docs): the test
    /// frame is written until a frame comes out, then, once nothing more comes, written minus out is what stays held
    async fn calibrate(&self, kind: &Kind) -> Result<usize, String> {
        let (w, h) = SAMPLE;
        let mut s = Session::spawn(&self.ffmpeg, &args(kind, w, h, self.qp, self.old), 0, SAMPLE, 0)?;
        let deadline = Instant::now() + CALIBRATE_FOR;
        let mut out = 0;
        while out == 0 {
            if Instant::now() > deadline {
                return Err(ictl_l10n::tr!("测试帧编码超时", "timed out encoding a test frame").into());
            }
            s.push(self.sample.clone());
            // the writer sends it before the next round (it is small), so every push is one frame written
            out += s.frames_within(Duration::from_millis(300)).await?;
        }
        out += s.frames_within(Duration::from_millis(500)).await?;
        let written = s.written();
        Ok(written.saturating_sub(out).min(MAX_KICKS))
    }
}

/// `ffmpeg -version`'s first line; `None` when it doesn't run
async fn version_line(ffmpeg: &Path) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(ffmpeg).arg("-version").stdin(Stdio::null()).stderr(Stdio::null()).output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().to_owned())
}

/// A 4:2:0 JPEG like the phone's, drawn by ffmpeg's test source
async fn sample_jpeg(ffmpeg: &Path) -> Result<Vec<u8>, String> {
    let size = format!("testsrc2=s={}x{}", SAMPLE.0, SAMPLE.1);
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", &size, "-frames:v", "1"])
            .args(["-c:v", "mjpeg", "-pix_fmt", "yuvj420p", "-q:v", "5", "-f", "mjpeg", "pipe:1"])
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| ictl_l10n::tr!("ffmpeg 生成测试帧超时", "ffmpeg timed out making a test frame").to_owned())?
    .map_err(|e| e.to_string())?;
    if !out.status.success() || mjpeg::jpeg_size(&out.stdout).is_none() {
        return Err(ictl_l10n::trf!(
            "ffmpeg 生成测试帧失败:{}",
            "ffmpeg failed to make a test frame: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// One ffmpeg process encoding frames of one size
pub struct Session {
    /// The frames' size (the JPEG's, before rounding to even)
    pub size: (u32, u32),
    /// Which encoder ([`Encoders::give_up`])
    pub index: usize,
    input: watch::Sender<Option<Arc<Vec<u8>>>>,
    out: mpsc::Receiver<Result<Packet, String>>,
    written: Arc<AtomicUsize>,
    errors: Arc<Mutex<VecDeque<String>>>,
    tasks: Vec<JoinHandle<()>>,
    _child: Child,
}

impl Drop for Session {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Session {
    fn spawn(ffmpeg: &Path, args: &[String], kicks: usize, size: (u32, u32), index: usize) -> Result<Self, String> {
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
        let (input, mut rx) = watch::channel::<Option<Arc<Vec<u8>>>>(None);
        let written = Arc::new(AtomicUsize::new(0));
        let w = written.clone();
        // the newest frame wins: if ffmpeg falls behind, frames in between are skipped rather than queued
        let writer = tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some(j) = rx.borrow_and_update().clone() else { continue };
                let part = mjpeg::part(&j);
                for _ in 0..=kicks {
                    if stdin.write_all(&part).await.is_err() {
                        return;
                    }
                }
                if stdin.flush().await.is_err() {
                    return;
                }
                w.fetch_add(1 + kicks, Ordering::Relaxed);
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
                tracing::debug!("ffmpeg: {l}");
                let mut e = e.lock().unwrap_or_else(|p| p.into_inner());
                if e.len() == 3 {
                    e.pop_front();
                }
                e.push_back(l);
            }
        });
        Ok(Self { size, index, input, out, written, errors, tasks: vec![writer, reader, logger], _child: child })
    }

    /// Encode this frame next (replaces one not yet written)
    pub fn push(&self, jpeg: Arc<Vec<u8>>) {
        self.input.send_replace(Some(jpeg));
    }

    /// The next packet; `None` when ffmpeg ended. Cancel-safe
    pub async fn next(&mut self) -> Option<Result<Packet, String>> {
        self.out.recv().await
    }

    /// Frames written to ffmpeg so far, copies included
    fn written(&self) -> usize {
        self.written.load(Ordering::Relaxed)
    }

    /// Frames that come out within `d`; an error when ffmpeg fails or ends
    async fn frames_within(&mut self, d: Duration) -> Result<usize, String> {
        let until = tokio::time::Instant::now() + d;
        let mut n = 0;
        loop {
            match tokio::time::timeout_at(until, self.next()).await {
                Err(_) => return Ok(n),
                Ok(Some(Ok(Packet::Frame { .. }))) => n += 1,
                Ok(Some(Ok(Packet::Config(_)))) => {}
                Ok(Some(Err(e))) => return Err(e),
                Ok(None) => return Err(self.error()),
            }
        }
    }

    /// What ffmpeg said before it ended
    pub fn error(&self) -> String {
        let e = self.errors.lock().unwrap_or_else(|p| p.into_inner());
        if e.is_empty() {
            ictl_l10n::tr!("ffmpeg 退出了", "ffmpeg exited").into()
        } else {
            e.iter().cloned().collect::<Vec<_>>().join("; ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_before_5_1_are_old() {
        assert!(is_old("ffmpeg version 4.4.2-0ubuntu0.22.04.1 Copyright (c) 2000-2021 the FFmpeg developers"));
        assert!(is_old("ffmpeg version 5.0.1 Copyright"));
        assert!(!is_old("ffmpeg version 5.1.6-0+deb12u1 Copyright"));
        assert!(!is_old("ffmpeg version 8.1.1-mediasrv Copyright (c) 2000-2026 the FFmpeg developers"));
        assert!(!is_old("ffmpeg version n7.1 Copyright"));
        assert!(!is_old("ffmpeg version N-118302-g1234abcd Copyright"));
        assert!(!is_old(""));
    }

    #[test]
    fn qp_is_checked() {
        assert_eq!(qp_from(None), 20);
        assert_eq!(qp_from(Some("24")), 24);
        assert_eq!(qp_from(Some("0")), 20);
        assert_eq!(qp_from(Some("52")), 20);
        assert_eq!(qp_from(Some("high")), 20);
        assert_eq!(bitrate(20), "4000k");
        assert_eq!(bitrate(26), "2000k");
    }

    /// The VAAPI line as measured on an AMD 780M: GPU JPEG decoding, size rounded to even, one frame in flight,
    /// FLV out with every packet flushed
    #[test]
    fn vaapi_arguments() {
        let k = Kind::Vaapi { node: "/dev/dri/renderD128".into(), hw_jpeg: true };
        let a = args(&k, 968, 2097, 20, false).join(" ");
        assert!(
            a.contains(
                "-hwaccel vaapi -hwaccel_device /dev/dri/renderD128 -hwaccel_output_format vaapi -f mpjpeg -i pipe:0"
            ),
            "{a}"
        );
        assert!(a.contains("-fps_mode passthrough"), "{a}");
        assert!(
            a.contains("-vf scale_vaapi=w=968:h=2098:format=nv12 -c:v h264_vaapi -rc_mode CQP -qp 20 -async_depth 1"),
            "{a}"
        );
        assert!(a.ends_with("-bf 0 -g 600 -f flv -flvflags no_duration_filesize -flush_packets 1 pipe:1"), "{a}");
        let old = args(&k, 968, 2097, 20, true).join(" ");
        assert!(old.contains("-vsync passthrough") && !old.contains("async_depth"), "{old}");
        let sw =
            args(&Kind::Vaapi { node: "/dev/dri/renderD128".into(), hw_jpeg: false }, 2097, 968, 20, false).join(" ");
        assert!(
            sw.contains("-vaapi_device /dev/dri/renderD128 -f mpjpeg")
                && sw.contains("scale=2098:968,format=nv12,hwupload"),
            "{sw}"
        );
    }

    #[test]
    fn x264_uses_crf_one_below() {
        let a = args(&Kind::X264, 968, 2097, 20, false).join(" ");
        assert!(a.contains("-c:v libx264 -preset veryfast -tune zerolatency -crf 19"), "{a}");
    }

    #[test]
    fn software_is_always_last() {
        assert_eq!(candidates().last(), Some(&Kind::X264));
    }

    /// End to end with this machine's ffmpeg, when there is one: an encoder is picked and a frame comes out with
    /// its decoder configuration
    #[tokio::test]
    async fn encodes_with_the_local_ffmpeg() {
        if version_line(Path::new("ffmpeg")).await.is_none() {
            eprintln!("no ffmpeg here, skipped");
            return;
        }
        let e = match Encoders::detect().await {
            Ok(e) => e,
            Err(m) => {
                eprintln!("no H.264 encoder here ({m}), skipped");
                return;
            }
        };
        let mut s = e.start(SAMPLE.0, SAMPLE.1).unwrap();
        s.push(e.sample.clone());
        let mut config = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout_at(deadline, s.next()).await.expect("a frame within 10 s") {
                Some(Ok(Packet::Config(c))) => config = Some(c),
                Some(Ok(Packet::Frame { key, .. })) => {
                    assert!(key, "the first frame is a keyframe");
                    break;
                }
                other => panic!("{other:?} ({})", s.error()),
            }
        }
        assert!(flv::codec_string(&config.expect("config before the frame")).unwrap().starts_with("avc1."));
    }
}
