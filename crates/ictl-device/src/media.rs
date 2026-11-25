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
//! 码率上限在起流时协商（[`FULL_BITRATE`]，弱网模式 [`LOW_BANDWIDTH_BITRATE`]），之后改不了：换上限要关掉 HID 会话重开。
//! 手机实测只认上限：报价里写更小的分辨率，它照样编原生分辨率；帧率也不随上限变（约 60 帧/秒，见 [`video_offer`]）。
//!
//! 起流部分移植自 idevice v0.1.68 `tools/src/coredevice_stream.rs`（MIT，Copyright Jackson Coxson），
//! 去掉了日志输出和应答转储，错误改成 `DeviceError`。

use idevice::{
    ReadWrite, RsdService,
    core_device::{
        CallInfoBlob, DisplayServiceClient, HevcDepacketizer, RtpPacket, build_frame_ack, build_keyframe_request,
        build_liveness, build_rctl, build_screen_audio_offer, build_screen_video_offer, build_start_audio_parameters,
        build_start_video_parameters, is_rtcp, negotiation::build_screen_media_blob,
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
/// 克隆便宜；HID 会话关掉（媒体流停止）后订阅者收到 `Closed`。所以这里只留弱引用：持有 feed 的订阅者
/// 要是握着发送端，通道就永远不关，媒体流换掉后它一直等不到帧
#[derive(Clone)]
pub struct ScreenFeed {
    frames: broadcast::WeakSender<ScreenFrame>,
    keyframe: mpsc::UnboundedSender<()>,
}

impl ScreenFeed {
    /// `None`: the media stream has stopped
    pub fn subscribe(&self) -> Option<broadcast::Receiver<ScreenFrame>> {
        self.frames.upgrade().map(|f| f.subscribe())
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
                    // 没人订阅时只记参数集，不拼帧（新订阅者会要关键帧，参数集补在它前面）
                    let watched = frames.receiver_count() > 0;
                    // 新的时间戳开始了新的一帧：上一帧要是还没随 marker 结束，先把它发出去
                    if let Some(Some(f)) = asm.new_timestamp(p.timestamp, watched) {
                        let _ = frames.send(f);
                    }
                    depack.push(p.sequence_number, p.timestamp, p.payload);
                    asm.push(&depack.take_output());
                    if p.marker && let Some(f) = asm.complete(watched) {
                        received = received.wrapping_add(1);
                        let _ = video.send_to(device_port, build_frame_ack(our_ssrc, p.timestamp)).await;
                        if let Some(f) = f {
                            let _ = frames.send(f);
                        }
                    }
                    if asm.take_dropped() {
                        keyframe_wanted = true;
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

/// 一帧最多这么大。手机的关键帧几十 KB 到一两 MB；超过就是丢了 marker 又一直是同一个时间戳之类的坏数据
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// 把解包器吐出的 Annex-B 攒成一帧：帧以 RTP marker 结束（或下一帧的时间戳开始）。记下最近的参数集，补到没带它们
/// 的关键帧前面
#[derive(Default)]
struct Assembler {
    current: Vec<u8>,
    timestamp: Option<u32>,
    /// 最近一次见到的 VPS、SPS、PPS（各自带起始码）
    params: [Option<Vec<u8>>; 3],
    /// 这一帧超过了 [`MAX_FRAME_BYTES`]：丢掉，到下一帧再收
    overflow: bool,
    /// 丢过超大的帧，还没告诉调用方（它要请手机补一个关键帧）
    dropped: bool,
}

impl Assembler {
    /// 新的时间戳开始了新的一帧：上一帧要是还没随 marker 结束，就在这里结束（返回值同 [`Self::complete`]）
    fn new_timestamp(&mut self, ts: u32, watched: bool) -> Option<Option<ScreenFrame>> {
        let done = if self.timestamp.is_some_and(|t| t != ts) { self.complete(watched) } else { None };
        self.timestamp = Some(ts);
        done
    }

    fn push(&mut self, annexb: &[u8]) {
        if self.overflow {
            return;
        }
        if self.current.len() + annexb.len() > MAX_FRAME_BYTES {
            self.current.clear();
            self.overflow = true;
            self.dropped = true;
            return;
        }
        self.current.extend_from_slice(annexb);
    }

    /// 丢过超大的帧吗（问一次就清掉）
    fn take_dropped(&mut self) -> bool {
        std::mem::take(&mut self.dropped)
    }

    /// 攒好的一帧（[`Self::complete`]，有人在看）
    #[cfg(test)]
    fn finish(&mut self) -> Option<ScreenFrame> {
        self.complete(true).flatten()
    }

    /// 结束攒着的一帧。`None`：没有画面数据（只有参数集、AUD，或超大被丢了），参数集留下；`Some(None)`：是一帧，
    /// 但没人看（`watched` 为 false），只记下参数集、不复制不分配（Agent 操作时媒体流开着，常常没人看）；
    /// `Some(Some(帧))`：要发给订阅者的一帧
    fn complete(&mut self, watched: bool) -> Option<Option<ScreenFrame>> {
        if std::mem::take(&mut self.overflow) {
            self.current.clear();
            return None;
        }
        let mut key = false;
        let mut has = [false; 3];
        let mut vcl = false;
        for n in nal_units(&self.current) {
            match nal_type(n) {
                Some(t @ 32..=34) => {
                    has[usize::from(t - 32)] = true;
                    let slot = &mut self.params[usize::from(t - 32)];
                    if slot.as_deref() != Some(n) {
                        *slot = Some(n.to_vec());
                    }
                }
                Some(t) if t <= 31 => {
                    vcl = true;
                    key |= (16..=23).contains(&t);
                }
                _ => {}
            }
        }
        if !vcl || !watched {
            // 留着缓冲区的容量，下一帧接着用
            self.current.clear();
            return vcl.then_some(None);
        }
        let data = std::mem::take(&mut self.current);
        let data = if key && has != [true; 3] && self.params.iter().all(Option::is_some) {
            let mut v: Vec<u8> = self.params.iter().flatten().flatten().copied().collect();
            v.extend(
                nal_units(&data)
                    .into_iter()
                    .filter(|n| !matches!(nal_type(n), Some(32..=34)))
                    .flat_map(|n| n.iter().copied()),
            );
            v
        } else {
            data
        };
        Some(Some(ScreenFrame { key, data: Arc::new(data) }))
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
    max_bitrate: u32,
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
    let offer = video_offer(&call_id, &call_info, ssrc, max_bitrate)
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
    let feed = ScreenFeed { frames: frames.downgrade(), keyframe };
    Ok(ScreenMediaStream { client, receive: receive(audio_udp, video_udp, ssrc, frames, keyframe_rx), feed })
}

/// What the screen video may use (bit/s), as idevice's offer has it: about 3 Mbit/s while swiping on an iPhone's home
/// screen, 1.9 Mbit/s still (iPhone 15 Pro Max, iOS 27.2.1)
pub const FULL_BITRATE: u32 = 6_000_000;
/// The control page's low-bandwidth mode: about half the traffic (1.4 Mbit/s swiping, 0.8 still) at the same frame
/// rate. A still screen looks the same; while things move, edges show blocks, gone about half a second after
pub const LOW_BANDWIDTH_BITRATE: u32 = 2_000_000;

/// The video offer with the bitrate cap `max_bitrate`. idevice's offer ([`build_screen_video_offer`]) asks for
/// [`FULL_BITRATE`]; for another cap its media blob is built again with that entry of the parameter list changed (the
/// phone takes it as `TXMaxBitrate`), and put into the same offer
fn video_offer(
    call_id: &str,
    info: &CallInfoBlob,
    ssrc: u32,
    max_bitrate: u32,
) -> std::result::Result<Vec<u8>, String> {
    let offer = build_screen_video_offer(call_id, info, ssrc).map_err(|e| e.to_string())?;
    if max_bitrate == FULL_BITRATE {
        return Ok(offer);
    }
    let blob = build_screen_media_blob(ssrc, ntp_now());
    let blob = with_max_bitrate(&blob, max_bitrate).ok_or("idevice's offer has no bitrate cap to change")?;
    let mut d: plist::Dictionary = plist::from_bytes(&offer).map_err(|e| e.to_string())?;
    d.insert(MEDIA_BLOB.into(), plist::Value::Data(zlib_stored(&blob)));
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &plist::Value::Dictionary(d)).map_err(|e| e.to_string())?;
    Ok(out)
}

/// The offer's key for the (zlib) media blob
const MEDIA_BLOB: &str = "avcMediaStreamNegotiatorMediaBlob";

/// The blob with the parameter `{1: 0, 2: FULL_BITRATE, 3: 0x20000}` (field 9) giving `max_bitrate` instead
fn with_max_bitrate(blob: &[u8], max_bitrate: u32) -> Option<Vec<u8>> {
    let old = bitrate_param(FULL_BITRATE);
    let at = blob.windows(old.len()).position(|w| w == old.as_slice())?;
    let mut out = blob[..at].to_vec();
    out.extend(bitrate_param(max_bitrate));
    out.extend_from_slice(&blob[at + old.len()..]);
    Some(out)
}

/// Field 9 `{1: 0, 2: value, 3: 0x20000}` as protobuf
fn bitrate_param(value: u32) -> Vec<u8> {
    fn varint(o: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            o.push((v as u8) | 0x80);
            v >>= 7;
        }
        o.push(v as u8);
    }
    let mut m = Vec::new();
    for (field, v) in [(1u64, 0u64), (2, u64::from(value)), (3, 0x20000)] {
        varint(&mut m, field << 3);
        varint(&mut m, v);
    }
    let mut o = Vec::new();
    varint(&mut o, (9 << 3) | 2);
    varint(&mut o, m.len() as u64);
    o.extend(m);
    o
}

/// zlib of `data` in stored (uncompressed) blocks: the blob is a few hundred bytes, and this needs no deflate
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = data.chunks(0xffff).collect();
    for (i, c) in chunks.iter().enumerate() {
        z.push(u8::from(i + 1 == chunks.len()));
        let n = c.len() as u16;
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&(!n).to_le_bytes());
        z.extend_from_slice(c);
    }
    if chunks.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    z
}

/// Now as NTP (seconds since 1900 in the high 32 bits), for the media blob
fn ntp_now() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    ((now.as_secs() + 2_208_988_800) << 32) | ((u64::from(now.subsec_nanos()) << 32) / 1_000_000_000)
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
        assert!(a.new_timestamp(1, true).is_none());
        a.push(&trail);
        assert!(a.new_timestamp(2, true).is_some_and(|f| f.is_some()));
    }

    /// Nobody watching: frames are counted (and acknowledged) but not copied; the parameter sets are still kept, so
    /// the first keyframe a new subscriber gets is decodable
    #[test]
    fn unwatched_frames_keep_only_the_parameter_sets() {
        let (vps, sps, pps) = ([0u8, 0, 0, 1, 0x40, 1, 9], [0u8, 0, 0, 1, 0x42, 1, 8], [0u8, 0, 0, 1, 0x44, 1, 7]);
        let idr = [0u8, 0, 0, 1, 0x28, 1, 0xAA];
        let mut a = Assembler::default();
        a.push(&[vps.as_slice(), &sps, &pps, &idr].concat());
        assert!(matches!(a.complete(false), Some(None)), "a frame, not delivered");
        a.push(&[0, 0, 0, 1, 0x46, 1, 0x50]);
        assert!(a.complete(false).is_none(), "no picture data: not a frame either way");
        a.push(&idr);
        let f = a.complete(true).flatten().unwrap();
        assert_eq!(*f.data, [vps.as_slice(), &sps, &pps, &idr].concat());
    }

    /// A frame over the limit is dropped whole (once), the next one comes through
    #[test]
    fn oversized_frames_are_dropped() {
        let idr = [0u8, 0, 0, 1, 0x28, 1, 0xAA];
        let mut a = Assembler::default();
        let big = vec![0xAB; MAX_FRAME_BYTES / 2 + 1];
        a.push(&idr);
        a.push(&big);
        a.push(&big);
        a.push(&idr);
        assert!(a.take_dropped());
        assert!(!a.take_dropped(), "told once");
        assert!(a.finish().is_none());
        a.push(&idr);
        assert_eq!(*a.finish().unwrap().data, idr);
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

    /// The cap is the one entry of idevice's blob that changes; the offer is idevice's but for that blob, which the
    /// phone reads (zlib, stored blocks)
    #[test]
    fn the_offer_carries_the_bitrate_cap() {
        let blob = build_screen_media_blob(7, 1 << 32);
        assert_eq!(with_max_bitrate(&blob, FULL_BITRATE).as_deref(), Some(blob.as_slice()));
        let low = with_max_bitrate(&blob, LOW_BANDWIDTH_BITRATE).expect("idevice's blob has the 6M cap");
        assert_ne!(low, blob);
        assert!(
            low.windows(bitrate_param(LOW_BANDWIDTH_BITRATE).len()).any(|w| w == bitrate_param(LOW_BANDWIDTH_BITRATE))
        );
        assert_eq!(low.len(), blob.len() - 1, "2_000_000 is one varint byte shorter");

        let info = CallInfoBlob { device_type: "Mac17,7".into(), ..Default::default() };
        let full = video_offer("C", &info, 7, FULL_BITRATE).unwrap();
        let offer: plist::Dictionary =
            plist::from_bytes(&video_offer("C", &info, 7, LOW_BANDWIDTH_BITRATE).unwrap()).unwrap();
        let theirs: plist::Dictionary = plist::from_bytes(&full).unwrap();
        assert_eq!(offer.keys().collect::<Vec<_>>(), theirs.keys().collect::<Vec<_>>());
        for k in theirs.keys().filter(|k| *k != MEDIA_BLOB) {
            assert_eq!(offer[k], theirs[k], "{k}");
        }
        let z = offer[MEDIA_BLOB].as_data().unwrap();
        assert!(idevice::core_device::MediaNegotiationBlob::from_negotiator_offer(z).is_ok(), "valid zlib");
        let raw = &z[7..z.len() - 4];
        assert!(
            raw.windows(bitrate_param(LOW_BANDWIDTH_BITRATE).len()).any(|w| w == bitrate_param(LOW_BANDWIDTH_BITRATE))
        );
        assert!(!raw.windows(bitrate_param(FULL_BITRATE).len()).any(|w| w == bitrate_param(FULL_BITRATE)));
    }

    #[test]
    fn stored_zlib_inflates() {
        for data in [vec![], b"abc".to_vec(), vec![7u8; 70_000]] {
            let z = zlib_stored(&data);
            // the decoder idevice uses for answers: a wrong checksum or block header fails
            assert!(idevice::core_device::MediaNegotiationBlob::from_negotiator_offer(&z).is_ok(), "{}", data.len());
        }
        assert_eq!(zlib_stored(b"a"), [0x78, 0x01, 1, 1, 0, 0xfe, 0xff, b'a', 0, 0x62, 0, 0x62]);
    }

    /// Someone holding the feed doesn't keep the channel open: once the media stream stops (its receive task holds
    /// the only sender), subscribers hear `Closed` and new ones get none
    #[tokio::test]
    async fn subscribers_hear_when_the_stream_stops() {
        let (frames, _) = broadcast::channel::<ScreenFrame>(4);
        let (keyframe, _rx) = mpsc::unbounded_channel();
        let feed = ScreenFeed { frames: frames.downgrade(), keyframe };
        let mut sub = feed.subscribe().expect("running");
        drop(frames);
        assert!(matches!(sub.recv().await, Err(broadcast::error::RecvError::Closed)));
        assert!(feed.subscribe().is_none());
    }
}
