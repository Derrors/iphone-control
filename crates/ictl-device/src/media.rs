//! 启动 CoreDevice 屏幕媒体流。HID 事件只有在媒体流存在时才会被系统接受（否则静默丢弃），
//! 所以 HID 会话期间要一直持有它。
//!
//! 流本身是手机硬件编码的 HEVC（RTP，payload type 100，不加密），约 60 帧/秒、原生分辨率（宽高补到 16 的倍数，
//! 1320×2868 的屏编成 1328×2896）。收下来还原成一帧帧的 Annex-B（[`ScreenFeed`]），给控制台当画面用；
//! 没人订阅时照样收、照样解包（便宜），只是不往外发。
//!
//! 回传给手机的 RTCP 照 Device Hub 的做法（上游 idevice 抓包整理）：每收完一帧回一个帧确认，每 50 毫秒一个 RCTL，
//! 每秒一个接收报告。上游说不回 RCTL 的话，剧烈运动时编码器和接收端的参考帧会对不上、画面花掉而解码不报错。
//! 关键帧只在开头有一个（KeyFrameInterval 0），之后要靠 PLI/FIR 请求（实测 15～20 毫秒后到）。
//!
//! 起流部分移植自 idevice v0.1.68 `tools/src/coredevice_stream.rs`（MIT，Copyright Jackson Coxson），
//! 去掉了日志输出和应答转储，错误改成 `DeviceError`。

use idevice::{
    ReadWrite, RsdService,
    core_device::{
        CallInfoBlob, DisplayServiceClient, HevcDepacketizer, RtpPacket, build_frame_ack, build_keyframe_request,
        build_liveness, build_rctl, build_screen_audio_offer, build_screen_video_offer, build_start_audio_parameters,
        build_start_video_parameters, is_rtcp,
    },
    rsd::RsdHandshake,
    tcp::handle::{AdapterHandle, UdpSocketHandle},
};

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};

use crate::{DeviceError, Result, limit};

/// 每一步的时限。超时不能在外层直接取消：音频流可能已经起来了，必须走到 stop
const STEP: Duration = Duration::from_secs(10);

/// 我们（控制端）支持的特性，不是设备的。Device Hub 屏幕共享发 140；
/// 发设备的掩码会让协商出无效的视频配置。
const CLIENT_SUPPORTED_FEATURES: u64 = 140;

/// 活着的媒体流。两个 UDP 套接字必须一直绑定，丢掉就会解绑端口。
///
/// 手机按码率不停地发 RTP 包，jktcp 给每个 UDP 端口的是无界队列：不读的话包一直堆在内存里，HID 会话开多久就
/// 堆多久。`receive` 任务一直读：音频丢掉，视频解包成帧（[`ScreenFeed`]），随媒体流结束
pub(crate) struct ScreenMediaStream {
    client: DisplayServiceClient<Box<dyn ReadWrite>>,
    receive: tokio::task::JoinHandle<()>,
    feed: ScreenFeed,
}

impl Drop for ScreenMediaStream {
    fn drop(&mut self) {
        // 任务持有两个套接字：中止后它们随之释放、解绑端口；订阅者随之收到流结束
        self.receive.abort();
    }
}

/// 一帧画面：Annex-B（起始码分隔的 NAL），关键帧前面带着 VPS/SPS/PPS（手机按请求补发的关键帧不带，这里补上），
/// 所以从任何一个关键帧开始都能解
#[derive(Debug, Clone)]
pub struct ScreenFrame {
    pub key: bool,
    pub data: Arc<Vec<u8>>,
}

/// 屏幕媒体流的画面：订阅（[`Self::subscribe`]）拿到之后的每一帧，要关键帧用 [`Self::request_keyframe`]。
/// 克隆便宜；HID 会话关掉（媒体流停止）后订阅者收到 `Closed`
#[derive(Clone)]
pub struct ScreenFeed {
    frames: broadcast::Sender<ScreenFrame>,
    keyframe: mpsc::UnboundedSender<()>,
}

impl ScreenFeed {
    pub fn subscribe(&self) -> broadcast::Receiver<ScreenFrame> {
        self.frames.subscribe()
    }

    /// 请手机尽快发一个关键帧（新订阅者、跟不上丢了帧时）
    pub fn request_keyframe(&self) {
        let _ = self.keyframe.send(());
    }
}

/// 一秒多的帧：订阅者落后更多就收到 `Lagged`，要从下一个关键帧接着来
const FEED_CAPACITY: usize = 90;
/// 关键帧请求最多这么频繁（PLI/FIR 每个都会让手机编一个 15～50 KB 的关键帧）
const KEYFRAME_EVERY: Duration = Duration::from_millis(300);

/// 收两个套接字上的数据，直到它们关闭：音频丢掉；视频解包、组帧、发给订阅者，并回传 RTCP（见模块说明）
fn receive(
    audio: UdpSocketHandle,
    video: UdpSocketHandle,
    our_ssrc: u32,
    frames: broadcast::Sender<ScreenFrame>,
    mut keyframe: mpsc::UnboundedReceiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut depack = HevcDepacketizer::new();
        let mut asm = Assembler::default();
        // 手机发 RTP 的端口（RTCP 回到这里），收到第一个包才知道
        let mut device_port = 0u16;
        let (mut media_ssrc, mut first_seq, mut last_seq) = (0u32, None::<u16>, 0u16);
        let (mut received, mut fir_seq) = (0u16, 0u8);
        let mut last_keyframe_ask = None::<Instant>;
        let mut keyframe_wanted = false;
        let start = Instant::now();
        let mut rctl = tokio::time::interval(Duration::from_millis(50));
        let mut report = tokio::time::interval(Duration::from_secs(1));
        loop {
            // mpsc 的接收可以安全取消：没收完的包留在队列里，下一轮照样读到
            tokio::select! {
                r = audio.recv() => if r.is_err() { return },
                r = video.recv() => {
                    let Ok(dg) = r else { return };
                    if is_rtcp(&dg.data) {
                        continue;
                    }
                    let Some(p) = RtpPacket::parse(&dg.data) else { continue };
                    device_port = dg.source_port;
                    media_ssrc = p.ssrc;
                    first_seq.get_or_insert(p.sequence_number);
                    last_seq = p.sequence_number;
                    // 新的时间戳开始了新的一帧：上一帧要是还没随 marker 结束，先把它发出去
                    if let Some(f) = asm.new_timestamp(p.timestamp) {
                        let _ = frames.send(f);
                    }
                    depack.push(p.sequence_number, p.timestamp, p.payload);
                    asm.push(&depack.take_output());
                    if p.marker && let Some(f) = asm.finish() {
                        received = received.wrapping_add(1);
                        let _ = video.send_to(device_port, build_frame_ack(our_ssrc, p.timestamp)).await;
                        let _ = frames.send(f);
                    }
                }
                r = keyframe.recv() => {
                    if r.is_none() {
                        return;
                    }
                    keyframe_wanted = true;
                }
                _ = rctl.tick() => {
                    if device_port != 0 {
                        let clock = (start.elapsed().as_millis() & 0xffff) as u16;
                        let rel = last_seq.wrapping_sub(first_seq.unwrap_or(last_seq));
                        let _ = video.send_to(device_port, build_rctl(our_ssrc, clock, received, rel)).await;
                    }
                }
                _ = report.tick() => {
                    if device_port != 0 {
                        let _ = video.send_to(device_port, build_liveness(our_ssrc, "iphone-control", &[])).await;
                    }
                }
            }
            if keyframe_wanted && device_port != 0 && last_keyframe_ask.is_none_or(|t| t.elapsed() >= KEYFRAME_EVERY) {
                keyframe_wanted = false;
                last_keyframe_ask = Some(Instant::now());
                fir_seq = fir_seq.wrapping_add(1);
                let ask = build_keyframe_request(our_ssrc, "iphone-control", media_ssrc, &[], fir_seq);
                let _ = video.send_to(device_port, ask).await;
            }
        }
    })
}

/// 把解包器吐出的 Annex-B 攒成一帧：帧以 RTP marker 结束（或下一帧的时间戳开始）。记下最近的参数集，补到没带它们
/// 的关键帧前面
#[derive(Default)]
struct Assembler {
    current: Vec<u8>,
    timestamp: Option<u32>,
    /// 最近一次见到的 VPS、SPS、PPS（各自带起始码）
    params: [Option<Vec<u8>>; 3],
}

impl Assembler {
    fn new_timestamp(&mut self, ts: u32) -> Option<ScreenFrame> {
        let done = if self.timestamp.is_some_and(|t| t != ts) { self.finish() } else { None };
        self.timestamp = Some(ts);
        done
    }

    fn push(&mut self, annexb: &[u8]) {
        self.current.extend_from_slice(annexb);
    }

    /// 攒好的一帧；没有画面数据（只有参数集、AUD）时不算一帧，参数集留下
    fn finish(&mut self) -> Option<ScreenFrame> {
        let data = std::mem::take(&mut self.current);
        let nals = nal_units(&data);
        let mut key = false;
        let mut has = [false; 3];
        let mut vcl = false;
        for n in &nals {
            match nal_type(n) {
                Some(t @ 32..=34) => {
                    has[usize::from(t - 32)] = true;
                    self.params[usize::from(t - 32)] = Some(n.to_vec());
                }
                Some(t) if t <= 31 => {
                    vcl = true;
                    key |= (16..=23).contains(&t);
                }
                _ => {}
            }
        }
        if !vcl {
            return None;
        }
        let data = if key && has != [true; 3] && self.params.iter().all(Option::is_some) {
            let mut v: Vec<u8> = self.params.iter().flatten().flatten().copied().collect();
            v.extend(nals.iter().filter(|n| !matches!(nal_type(n), Some(32..=34))).flat_map(|n| n.iter().copied()));
            v
        } else {
            data
        };
        Some(ScreenFrame { key, data: Arc::new(data) })
    }
}

/// Annex-B 里的各个 NAL（各自带着起始码）
pub(crate) fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(if i > 0 && data[i - 1] == 0 { i - 1 } else { i });
            i += 3;
        } else {
            i += 1;
        }
    }
    starts.iter().enumerate().map(|(k, &s)| &data[s..starts.get(k + 1).copied().unwrap_or(data.len())]).collect()
}

/// 一个带起始码的 NAL 的类型（HEVC：首字节的第 2～7 位）
fn nal_type(nal: &[u8]) -> Option<u8> {
    let body = if nal.starts_with(&[0, 0, 0, 1]) { &nal[4..] } else { nal.strip_prefix(&[0, 0, 1])? };
    body.first().map(|b| (b >> 1) & 0x3f)
}

/// WebCodecs 的 HEVC 编码串（例如 `hvc1.1.6.L150.B0`），从一帧（关键帧，带 SPS）里的 SPS 算出来
pub fn hevc_codec(frame: &[u8]) -> Option<String> {
    let sps = nal_units(frame).into_iter().find(|n| nal_type(n) == Some(33))?;
    let body = if sps.starts_with(&[0, 0, 0, 1]) { &sps[4..] } else { &sps[3..] };
    // 去掉防竞争字节，跳过 2 字节的 NAL 头和 1 字节（vps id、max_sub_layers、temporal_id_nesting）
    let mut raw = Vec::with_capacity(body.len());
    let mut zeros = 0;
    for &b in body.get(2..)? {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        raw.push(b);
    }
    // profile_tier_level：profile_space(2) tier(1) profile_idc(5)，32 位兼容标志，48 位约束标志，level_idc
    let ptl = raw.get(1..13)?;
    let (space, tier, profile) = (ptl[0] >> 6, (ptl[0] >> 5) & 1, ptl[0] & 0x1f);
    let compat = u32::from_be_bytes(ptl[1..5].try_into().ok()?).reverse_bits();
    let constraints: Vec<String> = {
        let c = &ptl[5..11];
        let last = c.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
        c[..last].iter().map(|b| format!("{b:X}")).collect()
    };
    let space = ["", "A", "B", "C"][usize::from(space)];
    let mut s = format!("hvc1.{space}{profile}.{compat:X}.{}{}", if tier == 1 { 'H' } else { 'L' }, ptl[11]);
    for c in constraints {
        s.push('.');
        s.push_str(&c);
    }
    Some(s)
}

impl ScreenMediaStream {
    pub async fn stop(mut self) {
        stop(&mut self.client).await;
    }

    pub fn feed(&self) -> ScreenFeed {
        self.feed.clone()
    }
}

pub(crate) async fn start(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
    display_id: i64,
) -> Result<ScreenMediaStream> {
    let fail = |what: &str, e: &dyn std::fmt::Display| {
        DeviceError::Idevice(ictl_l10n::trf!("媒体流 {what}：{e}", "media stream {what}: {e}"))
    };

    let mut client = limit(ictl_l10n::tr!("连接 displayservice", "connect to displayservice"), STEP, async {
        DisplayServiceClient::connect_rsd(adapter, handshake)
            .await
            .map_err(|e| fail(ictl_l10n::tr!("连接 displayservice", "connect to displayservice"), &e))
    })
    .await?;
    let audio_udp =
        adapter.bind_udp(0).await.map_err(|e| fail(ictl_l10n::tr!("绑定音频 UDP", "bind audio UDP"), &e))?;
    let video_udp =
        adapter.bind_udp(0).await.map_err(|e| fail(ictl_l10n::tr!("绑定视频 UDP", "bind video UDP"), &e))?;
    let receiver_ip = adapter.host_ip().to_string();
    let sender_ip = adapter.peer_ip().to_string();

    // 描述本端的 VCCallInfoBlob，取值与上游抓到的、设备接受的 Device Hub 报价一致
    let call_info = CallInfoBlob {
        call_id: 0,
        client_version: 1,
        device_type: "Mac17,7".into(),
        framework_version: "2205.3.1".into(),
        os_version: "25F71".into(),
        device_name: None,
        audio_device_uid: None,
    };
    let session = uuid::Uuid::new_v4();

    // 先起音频流建立屏幕共享会话，再在同一会话上起视频流
    let audio_call_id = uuid::Uuid::new_v4().to_string().to_uppercase();
    let audio_offer = build_screen_audio_offer(&audio_call_id, &call_info)
        .map_err(|e| fail(ictl_l10n::tr!("音频报价", "audio offer"), &e))?;
    let audio_params = build_start_audio_parameters(
        &receiver_ip,
        audio_udp.local_port(),
        &sender_ip,
        50000,
        audio_offer,
        CLIENT_SUPPORTED_FEATURES,
        session,
    );

    let call_id = uuid::Uuid::new_v4().to_string().to_uppercase();
    let ssrc = uuid::Uuid::new_v4().as_u128() as u32;
    let offer = build_screen_video_offer(&call_id, &call_info, ssrc)
        .map_err(|e| fail(ictl_l10n::tr!("视频报价", "video offer"), &e))?;
    let params = build_start_video_parameters(
        &receiver_ip,
        video_udp.local_port(),
        &sender_ip,
        50001,
        offer,
        CLIENT_SUPPORTED_FEATURES,
        display_id,
        session,
    );
    let started = limit(ictl_l10n::tr!("启动媒体流", "start the media stream"), STEP * 2, async {
        client
            .start_media_stream(audio_params)
            .await
            .map_err(|e| fail(ictl_l10n::tr!("启动音频流", "start the audio stream"), &e))?;
        client
            .start_media_stream(params)
            .await
            .map_err(|e| fail(ictl_l10n::tr!("启动视频流", "start the video stream"), &e))
    })
    .await;
    if let Err(e) = started {
        // 音频流可能已经起来了（包括超时的情况），要停掉：stopAll 连同会话一起结束，否则手机端一直挂着
        stop(&mut client).await;
        return Err(e);
    }

    let (frames, _) = broadcast::channel(FEED_CAPACITY);
    let (keyframe, keyframe_rx) = mpsc::unbounded_channel();
    let feed = ScreenFeed { frames: frames.clone(), keyframe };
    Ok(ScreenMediaStream { client, receive: receive(audio_udp, video_udp, ssrc, frames, keyframe_rx), feed })
}

async fn stop(client: &mut DisplayServiceClient<Box<dyn ReadWrite>>) {
    match tokio::time::timeout(STEP, client.stop_media_stream()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::debug!("stop_media_stream: {e}"),
        Err(_) => tracing::warn!(
            "{}",
            ictl_l10n::log_tr!(
                "停止屏幕媒体流超时，手机上可能还挂着屏幕共享会话",
                "Stopping the screen media stream timed out; a screen sharing session may still be open on the phone"
            )
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手机按请求补发的关键帧只有 IDR 切片：前面补上开头那一帧带的参数集；没有画面数据的不算一帧
    #[test]
    fn keyframes_get_the_parameter_sets() {
        let (vps, sps, pps) = ([0u8, 0, 0, 1, 0x40, 1, 9], [0u8, 0, 0, 1, 0x42, 1, 8], [0u8, 0, 0, 1, 0x44, 1, 7]);
        let idr = [0u8, 0, 0, 1, 0x28, 1, 0xAA];
        let trail = [0u8, 0, 0, 1, 0x02, 1, 0xBB];
        let mut a = Assembler::default();
        a.push(&[vps.as_slice(), &sps, &pps, &idr].concat());
        let first = a.finish().unwrap();
        assert!(first.key);
        assert_eq!(*first.data, [vps.as_slice(), &sps, &pps, &idr].concat());
        a.push(&trail);
        assert!(!a.finish().unwrap().key);
        a.push(&idr);
        let later = a.finish().unwrap();
        assert_eq!(*later.data, [vps.as_slice(), &sps, &pps, &idr].concat(), "parameter sets added");
        a.push(&[0, 0, 0, 1, 0x46, 1, 0x50]);
        assert!(a.finish().is_none(), "an access unit delimiter alone isn't a frame");
        // a frame without a marker ends when the next timestamp starts
        assert!(a.new_timestamp(1).is_none());
        a.push(&trail);
        assert!(a.new_timestamp(2).is_some());
    }

    /// The SPS the iPhone 18 Pro Max (iOS 27.2.1) sends: Main profile, level 5.0
    #[test]
    fn codec_string_from_the_phones_sps() {
        let sps = [
            0u8, 0, 0, 1, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xB0, 0x00, 0x00, 0x03, 0x00, 0x00,
            0x03, 0x00, 0x96, 0xA0, 0x02,
        ];
        assert_eq!(hevc_codec(&sps).as_deref(), Some("hvc1.1.6.L150.B0"));
        assert_eq!(hevc_codec(&[0, 0, 0, 1, 0x28, 1, 0xAA]), None, "no SPS");
    }
}
