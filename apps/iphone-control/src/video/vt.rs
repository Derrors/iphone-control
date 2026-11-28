//! macOS: H.264 with VideoToolbox inside this program, no ffmpeg needed. Tried before ffmpeg's `h264_videotoolbox`,
//! which stays as the fallback ([`super::ffmpeg::candidates`]).
//!
//! One thread per session: the phone's JPEG is decoded by VideoToolbox's JPEG decoder (the hardware one on Apple
//! silicon) into an NV12 pixel buffer, which goes straight to the H.264 encoder; neither touches the CPU's copy of the
//! picture. The encoder runs without frame reordering and with no frames held back (`MaxFrameDelayCount` 0), and
//! every frame is completed before the next one is taken, so nothing waits for a following frame: no copies are needed
//! after each frame (`kicks` measures 0, unlike ffmpeg). An odd size (the phone's frames at 75 %) is stretched to
//! the next even one by the JPEG decoder, as ffmpeg's `scale` does: the H.264 encoder would cut the last column and
//! row off instead. What comes out is already in the `avcC` format (NAL units with
//! 4-byte lengths); the `avcC` itself is the one VideoToolbox puts in the format description.
//!
//! Quality: the quantizer is pinned (`MinAllowedFrameQP` = `MaxAllowedFrameQP` = qp) where the encoder supports it
//! (macOS 12 and later, Apple silicon), the same constant-qp setting the hardware encoders get through ffmpeg; else
//! `Quality` is set from qp ([`quality_for`]).
//!
//! Keys added after macOS 11 are looked up at run time ([`key`]): linked directly, the program wouldn't start on an
//! older system.

#![allow(non_upper_case_globals)]

use std::collections::VecDeque;
use std::ffi::{CStr, c_void};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;

use super::flv::Packet;

mod sys;

use sys::*;

/// An owned Core Foundation object, released when dropped
struct Cf(*mut c_void);

impl Cf {
    fn new(p: *const c_void) -> Option<Self> {
        (!p.is_null()).then_some(Self(p as *mut c_void))
    }
    fn ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

/// A mutable CF dictionary with CF-typed keys and values
struct Dict(Cf);

impl Dict {
    fn new() -> Self {
        let d = unsafe {
            CFDictionaryCreateMutable(
                std::ptr::null(),
                0,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            )
        };
        Self(Cf::new(d).expect("CFDictionaryCreateMutable"))
    }
    fn set(&self, k: CFStringRef, v: CFTypeRef) {
        unsafe { CFDictionarySetValue(self.0.ptr(), k, v) }
    }
}

fn number_i32(v: i32) -> Cf {
    Cf::new(unsafe { CFNumberCreate(std::ptr::null(), kCFNumberSInt32Type, (&raw const v).cast()) }).expect("CFNumber")
}

fn number_f64(v: f64) -> Cf {
    Cf::new(unsafe { CFNumberCreate(std::ptr::null(), kCFNumberFloat64Type, (&raw const v).cast()) }).expect("CFNumber")
}

fn cfstr(s: &CStr) -> Cf {
    Cf::new(unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), kCFStringEncodingUTF8) })
        .expect("CFString")
}

/// A key exported by a framework, looked up at run time; `None` on a system that doesn't have it
fn key(name: &CStr) -> Option<CFStringRef> {
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) } as *const CFStringRef;
    if p.is_null() { None } else { Some(unsafe { *p }).filter(|k| !k.is_null()) }
}

fn time(value: i64, timescale: i32) -> CMTime {
    CMTime { value, timescale, flags: kCMTimeFlags_Valid, epoch: 0 }
}

fn check(status: OSStatus, what: &str) -> Result<(), String> {
    if status == 0 { Ok(()) } else { Err(format!("{what}: OSStatus {status}")) }
}

/// `Quality` (0 to 1) for an encoder that can't pin the quantizer (before macOS 12): qp 20 → 0.75, 0.03 less per
/// step. An estimate, not measured: on Apple silicon with macOS 12 or later the quantizer is pinned instead
pub fn quality_for(qp: u8) -> f64 {
    (0.75 - (f64::from(qp) - 20.0) * 0.03).clamp(0.1, 1.0)
}

// ------------------------------------------------------------------ the encoder's output

/// What the compression callback writes to
struct Sink {
    tx: mpsc::UnboundedSender<Result<Packet, String>>,
    /// H.264: the `avcC` sent last, so a new one goes out before the frame that needs it. JPEG: unused
    avcc: Mutex<Vec<u8>>,
    h264: bool,
    /// The session counting H.264 frames sent and not yet taken ([`Shared::queued`]); none for a JPEG
    queued: Option<Arc<Shared>>,
}

/// H.264 frames the encoding thread may have sent ahead of the caller: a caller that falls behind (a slow page) no
/// longer makes the channel grow without end; meanwhile the newest JPEG replaces the one waiting ([`Session::push`])
const MAX_AHEAD: usize = 8;

extern "C" fn on_encoded(refcon: *mut c_void, _frame: *mut c_void, status: OSStatus, _flags: u32, sample: *mut c_void) {
    let sink = unsafe { &*(refcon as *const Sink) };
    if status != 0 {
        let _ = sink.tx.send(Err(format!("VideoToolbox encoder: OSStatus {status}")));
        return;
    }
    // a frame the encoder dropped
    if sample.is_null() {
        return;
    }
    let Some(data) = (unsafe { sample_bytes(sample) }) else {
        let _ = sink.tx.send(Err("VideoToolbox encoder: no data".into()));
        return;
    };
    if !sink.h264 {
        let _ = sink.tx.send(Ok(Packet::Frame { key: true, data }));
        return;
    }
    let key = unsafe { is_sync(sample) };
    if key {
        let Some(avcc) = (unsafe { avcc_of(CMSampleBufferGetFormatDescription(sample)) }) else {
            let _ = sink.tx.send(Err("VideoToolbox encoder: no avcC".into()));
            return;
        };
        let mut last = sink.avcc.lock().unwrap_or_else(|p| p.into_inner());
        if *last != avcc {
            *last = avcc.clone();
            let _ = sink.tx.send(Ok(Packet::Config(avcc)));
        }
    }
    // counted before it is sent: the caller may take it at once, and its count must not go below zero
    if let Some(s) = &sink.queued {
        s.queued.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    if sink.tx.send(Ok(Packet::Frame { key, data })).is_err()
        && let Some(s) = &sink.queued
    {
        s.queued.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

unsafe fn sample_bytes(sample: *mut c_void) -> Option<Vec<u8>> {
    let bb = unsafe { CMSampleBufferGetDataBuffer(sample) };
    if bb.is_null() {
        return None;
    }
    let len = unsafe { CMBlockBufferGetDataLength(bb) };
    let mut v = vec![0u8; len];
    (unsafe { CMBlockBufferCopyDataBytes(bb, 0, len, v.as_mut_ptr().cast()) } == 0).then_some(v)
}

/// A keyframe: no `NotSync` attachment, or one that is false
unsafe fn is_sync(sample: *mut c_void) -> bool {
    let a = unsafe { CMSampleBufferGetSampleAttachmentsArray(sample, 0) };
    if a.is_null() || unsafe { CFArrayGetCount(a) } == 0 {
        return true;
    }
    let d = unsafe { CFArrayGetValueAtIndex(a, 0) };
    let v = unsafe { CFDictionaryGetValue(d, kCMSampleAttachmentKey_NotSync) };
    v.is_null() || unsafe { CFBooleanGetValue(v) } == 0
}

/// The `avcC` of a format description: the one VideoToolbox stores as a sample description atom, else built from
/// the parameter sets
unsafe fn avcc_of(desc: *mut c_void) -> Option<Vec<u8>> {
    if desc.is_null() {
        return None;
    }
    let atoms =
        unsafe { CMFormatDescriptionGetExtension(desc, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms) };
    if !atoms.is_null() {
        let name = cfstr(c"avcC");
        let d = unsafe { CFDictionaryGetValue(atoms, name.ptr()) };
        if !d.is_null() {
            // a CFIndex: negative would make a huge length
            let n = usize::try_from(unsafe { CFDataGetLength(d) }).unwrap_or(0);
            let p = unsafe { CFDataGetBytePtr(d) };
            if n > 6 && !p.is_null() {
                return Some(unsafe { std::slice::from_raw_parts(p, n) }.to_vec());
            }
        }
    }
    let (mut count, mut nal_len) = (0usize, 0i32);
    let mut sets = Vec::new();
    let mut i = 0;
    loop {
        let (mut p, mut n) = (std::ptr::null(), 0usize);
        let s = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(desc, i, &mut p, &mut n, &mut count, &mut nal_len)
        };
        if s != 0 || p.is_null() {
            return None;
        }
        sets.push(unsafe { std::slice::from_raw_parts(p, n) }.to_vec());
        i += 1;
        if i >= count {
            break;
        }
    }
    build_avcc(&sets, nal_len)
}

/// An `avcC` from the SPS (NAL type 7) and PPS (8) and the length size of the NAL units
fn build_avcc(sets: &[Vec<u8>], nal_len: i32) -> Option<Vec<u8>> {
    let sps: Vec<&Vec<u8>> = sets.iter().filter(|s| s.first().is_some_and(|b| b & 0x1f == 7)).collect();
    let pps: Vec<&Vec<u8>> = sets.iter().filter(|s| s.first().is_some_and(|b| b & 0x1f == 8)).collect();
    let first = sps.first().filter(|s| s.len() >= 4)?;
    // what the avcC's fields hold: 5 bits of SPS count, a byte of PPS count, 16-bit lengths (cut short, the
    // record would describe other bytes than follow)
    if pps.is_empty()
        || !(1..=4).contains(&nal_len)
        || sps.len() > 31
        || pps.len() > 255
        || sets.iter().any(|s| s.len() > usize::from(u16::MAX))
    {
        return None;
    }
    let mut v = vec![1, first[1], first[2], first[3], 0xFC | (nal_len as u8 - 1), 0xE0 | sps.len() as u8];
    for s in &sps {
        v.extend_from_slice(&(s.len() as u16).to_be_bytes());
        v.extend_from_slice(s);
    }
    v.push(pps.len() as u8);
    for p in &pps {
        v.extend_from_slice(&(p.len() as u16).to_be_bytes());
        v.extend_from_slice(p);
    }
    Some(v)
}

// ------------------------------------------------------------------ sessions

/// A VideoToolbox compression session (H.264 or JPEG) and the sink its callback writes to
struct Encoder {
    session: Cf,
    sink: *mut Sink,
    start: Instant,
    /// The last presentation time, so two frames in the same microsecond still go up
    last_pts: i64,
    hardware: bool,
}

impl Encoder {
    fn new(
        w: u32,
        h: u32,
        codec: u32,
        tune: impl Fn(*mut c_void) -> Result<(), String>,
        sink: Sink,
    ) -> Result<Self, String> {
        let spec = Dict::new();
        spec.set(unsafe { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder }, unsafe {
            kCFBooleanTrue
        });
        let sink = Box::into_raw(Box::new(sink));
        let mut s = std::ptr::null_mut();
        let st = unsafe {
            VTCompressionSessionCreate(
                std::ptr::null(),
                w as i32,
                h as i32,
                codec,
                spec.0.ptr(),
                std::ptr::null(),
                std::ptr::null(),
                on_encoded,
                sink.cast(),
                &mut s,
            )
        };
        if st != 0 || s.is_null() {
            drop(unsafe { Box::from_raw(sink) });
            return Err(format!("VTCompressionSessionCreate: OSStatus {st}"));
        }
        let mut e = Self { session: Cf(s), sink, start: Instant::now(), last_pts: -1, hardware: false };
        tune(s)?;
        check(unsafe { VTCompressionSessionPrepareToEncodeFrames(s) }, "VTCompressionSessionPrepareToEncodeFrames")?;
        let mut hw: CFTypeRef = std::ptr::null();
        if unsafe {
            VTSessionCopyProperty(
                s,
                kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                std::ptr::null(),
                (&raw mut hw).cast(),
            )
        } == 0
            && let Some(hw) = Cf::new(hw)
        {
            e.hardware = unsafe { CFBooleanGetValue(hw.ptr()) } != 0;
        }
        Ok(e)
    }

    /// Encodes one picture and waits until it has come out (the callback has run)
    fn encode(&mut self, image: *mut c_void) -> Result<(), String> {
        let pts = (self.start.elapsed().as_micros() as i64).max(self.last_pts + 1);
        self.last_pts = pts;
        let s = self.session.ptr();
        check(
            unsafe {
                VTCompressionSessionEncodeFrame(
                    s,
                    image,
                    time(pts, 1_000_000),
                    kCMTimeInvalid,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            "VTCompressionSessionEncodeFrame",
        )?;
        check(unsafe { VTCompressionSessionCompleteFrames(s, kCMTimeInvalid) }, "VTCompressionSessionCompleteFrames")
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            // every frame was completed in `encode`, so no callback is pending once this returns
            VTCompressionSessionCompleteFrames(self.session.ptr(), kCMTimeInvalid);
            VTCompressionSessionInvalidate(self.session.ptr());
        }
        // the session is released after the sink (fields drop after this): it no longer calls back once invalidated
        drop(unsafe { Box::from_raw(self.sink) });
    }
}

fn set(s: *mut c_void, k: CFStringRef, v: CFTypeRef, what: &str) -> Result<(), String> {
    check(unsafe { VTSessionSetProperty(s, k, v) }, what)
}

/// An H.264 encoder for the viewer: real time, no reordering, nothing held back, qp pinned where it can be
fn h264(w: u32, h: u32, qp: u8, sink: Sink) -> Result<(Encoder, bool), String> {
    let pinned = std::cell::Cell::new(false);
    let tune = |s: *mut c_void| -> Result<(), String> {
        let t = unsafe { kCFBooleanTrue };
        let f = unsafe { kCFBooleanFalse };
        set(s, unsafe { kVTCompressionPropertyKey_RealTime }, t, "RealTime")?;
        set(s, unsafe { kVTCompressionPropertyKey_AllowFrameReordering }, f, "AllowFrameReordering")?;
        set(
            s,
            unsafe { kVTCompressionPropertyKey_ProfileLevel },
            unsafe { kVTProfileLevel_H264_High_AutoLevel },
            "ProfileLevel",
        )?;
        set(
            s,
            unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
            number_i32(super::ffmpeg::GOP_FRAMES as i32).ptr(),
            "MaxKeyFrameInterval",
        )?;
        // not every encoder takes these; they only help
        let _ =
            set(s, unsafe { kVTCompressionPropertyKey_MaxFrameDelayCount }, number_i32(0).ptr(), "MaxFrameDelayCount");
        let _ =
            set(s, unsafe { kVTCompressionPropertyKey_ExpectedFrameRate }, number_i32(60).ptr(), "ExpectedFrameRate");
        // Tag VUI colorimetry explicitly (BT.709/sRGB primaries, sRGB transfer, BT.709 matrix) to match decoded JPEG
        // frames and prevent browsers from decoding untagged H.264 as Rec.601 without oversaturating sRGB content on
        // wide-gamut displays. Looked up at runtime for older macOS.
        for (prop, preferred, fallback) in [
            (c"kVTCompressionPropertyKey_ColorPrimaries", c"kCVImageBufferColorPrimaries_ITU_R_709_2", None),
            (
                c"kVTCompressionPropertyKey_TransferFunction",
                c"kCVImageBufferTransferFunction_sRGB",
                Some(c"kCVImageBufferTransferFunction_ITU_R_709_2"),
            ),
            (c"kVTCompressionPropertyKey_YCbCrMatrix", c"kCVImageBufferYCbCrMatrix_ITU_R_709_2", None),
        ] {
            if let Some(k) = key(prop)
                && let Some(v) = key(preferred).or_else(|| fallback.and_then(key))
            {
                let _ = set(s, k, v, "Colorimetry");
            }
        }
        if let (Some(min), Some(max)) =
            (key(c"kVTCompressionPropertyKey_MinAllowedFrameQP"), key(c"kVTCompressionPropertyKey_MaxAllowedFrameQP"))
        {
            let q = number_i32(i32::from(qp));
            pinned.set(
                set(s, min, q.ptr(), "MinAllowedFrameQP").is_ok() && set(s, max, q.ptr(), "MaxAllowedFrameQP").is_ok(),
            );
        }
        if !pinned.get() {
            set(s, unsafe { kVTCompressionPropertyKey_Quality }, number_f64(quality_for(qp)).ptr(), "Quality")?;
        }
        Ok(())
    };
    let e = Encoder::new(w, h, kCMVideoCodecType_H264, tune, sink)?;
    Ok((e, pinned.get()))
}

/// The size frames are encoded at: 4:2:0 needs even sizes, so an odd one grows by a pixel (as in `ffmpeg::args`)
fn even((w, h): (u32, u32)) -> (u32, u32) {
    (w + (w & 1), h + (h & 1))
}

/// VideoToolbox's JPEG decoder for frames of one size, giving NV12 pixel buffers of that size made even ([`even`])
struct Decoder {
    session: Cf,
    desc: Cf,
    out: *mut DecodedSlot,
    size: (u32, u32),
}

/// Where the decompression callback leaves its picture
#[derive(Default)]
struct DecodedSlot {
    image: Option<Cf>,
    status: OSStatus,
}

extern "C" fn on_decoded(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: OSStatus,
    _flags: u32,
    image: *mut c_void,
    _pts: CMTime,
    _dur: CMTime,
) {
    let slot = unsafe { &mut *(refcon as *mut DecodedSlot) };
    slot.status = status;
    if status == 0 && !image.is_null() {
        slot.image = Cf::new(unsafe { CFRetain(image) });
    }
}

impl Decoder {
    fn new(w: u32, h: u32) -> Result<Self, String> {
        let mut desc = std::ptr::null_mut();
        check(
            unsafe {
                CMVideoFormatDescriptionCreate(
                    std::ptr::null(),
                    kCMVideoCodecType_JPEG,
                    w as i32,
                    h as i32,
                    std::ptr::null(),
                    &mut desc,
                )
            },
            "CMVideoFormatDescriptionCreate",
        )?;
        Self::with_desc(Cf(desc), (w, h), even((w, h)))
    }

    /// A decoder for `desc` (frames of `size`) giving NV12 pixel buffers of `out` size
    fn with_desc(desc: Cf, size: (u32, u32), (ew, eh): (u32, u32)) -> Result<Self, String> {
        let attrs = Dict::new();
        attrs.set(
            unsafe { kCVPixelBufferPixelFormatTypeKey },
            number_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32).ptr(),
        );
        attrs.set(unsafe { kCVPixelBufferIOSurfacePropertiesKey }, Dict::new().0.ptr());
        attrs.set(unsafe { kCVPixelBufferWidthKey }, number_i32(ew as i32).ptr());
        attrs.set(unsafe { kCVPixelBufferHeightKey }, number_i32(eh as i32).ptr());
        let out = Box::into_raw(Box::<DecodedSlot>::default());
        let record = DecompressionCallbackRecord { callback: on_decoded, refcon: out.cast() };
        let mut s = std::ptr::null_mut();
        let st = unsafe {
            VTDecompressionSessionCreate(std::ptr::null(), desc.ptr(), std::ptr::null(), attrs.0.ptr(), &record, &mut s)
        };
        if st != 0 || s.is_null() {
            drop(unsafe { Box::from_raw(out) });
            return Err(format!("VTDecompressionSessionCreate: OSStatus {st}"));
        }
        Ok(Self { session: Cf(s), desc, out, size })
    }

    /// The JPEG (or other frame) as a pixel buffer (decoded synchronously)
    fn decode(&mut self, jpeg: &[u8]) -> Result<Cf, String> {
        let mut bb = std::ptr::null_mut();
        // the block points at `jpeg` without copying (kCFAllocatorNull: never freed by Core Media); it is released
        // below, after the decoder is done with it
        check(
            unsafe {
                CMBlockBufferCreateWithMemoryBlock(
                    std::ptr::null(),
                    jpeg.as_ptr() as *mut c_void,
                    jpeg.len(),
                    kCFAllocatorNull,
                    std::ptr::null(),
                    0,
                    jpeg.len(),
                    0,
                    &mut bb,
                )
            },
            "CMBlockBufferCreateWithMemoryBlock",
        )?;
        let bb = Cf(bb);
        let mut sample = std::ptr::null_mut();
        let len = jpeg.len();
        check(
            unsafe {
                CMSampleBufferCreateReady(
                    std::ptr::null(),
                    bb.ptr(),
                    self.desc.ptr(),
                    1,
                    0,
                    std::ptr::null(),
                    1,
                    &len,
                    &mut sample,
                )
            },
            "CMSampleBufferCreateReady",
        )?;
        let sample = Cf(sample);
        // the callback ([`on_decoded`]) writes the slot through its own pointer while the calls below run: no
        // reference to it is held across them (two live `&mut` to one place are undefined behaviour), only short
        // accesses through the raw pointer before and after
        // SAFETY: `self.out` is the Box made in `new`, alive until `drop`; nothing else touches it meanwhile
        unsafe { *self.out = DecodedSlot::default() };
        let s = self.session.ptr();
        check(
            unsafe {
                VTDecompressionSessionDecodeFrame(s, sample.ptr(), 0, std::ptr::null_mut(), std::ptr::null_mut())
            },
            "VTDecompressionSessionDecodeFrame",
        )?;
        unsafe { VTDecompressionSessionWaitForAsynchronousFrames(s) };
        // SAFETY: as above; the callback has run (the wait returns once every frame is out)
        let (status, image) = unsafe { ((*self.out).status, (*self.out).image.take()) };
        check(status, "decoder")?;
        image.ok_or_else(|| "decoder: no picture".to_owned())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            VTDecompressionSessionWaitForAsynchronousFrames(self.session.ptr());
            VTDecompressionSessionInvalidate(self.session.ptr());
        }
        drop(unsafe { Box::from_raw(self.out) });
    }
}

// ------------------------------------------------------------------ the session the viewer uses

/// What the caller and the encoding thread share
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    /// H.264 frames sent and not yet taken ([`Session::next`]); the encoding thread waits while there are
    /// [`MAX_AHEAD`]. Counted as sent, not as given to the encoder: a frame the encoder drops sends nothing
    queued: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct State {
    /// The next frame to encode; a newer one replaces it
    next: Option<Arc<Vec<u8>>>,
    stop: bool,
    /// Frames given to the encoder so far, copies included
    written: usize,
    /// The last errors
    errors: VecDeque<String>,
}

/// One encoding thread for frames of one size. Same use as an ffmpeg session: `push` the newest JPEG, `next` the
/// packets
pub struct Session {
    shared: Arc<Shared>,
    out: mpsc::UnboundedReceiver<Result<Packet, String>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap_or_else(|p| p.into_inner()).stop = true;
        self.shared.wake.notify_all();
    }
}

/// What the thread found out about the encoder once it was set up
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub hardware: bool,
    /// The quantizer is pinned to qp (else `Quality` approximates it)
    pub pinned_qp: bool,
}

impl Session {
    /// Starts the thread; `kicks`: copies after each frame (0 unless measured otherwise). `info` is told once the
    /// encoder is set up
    pub fn start(w: u32, h: u32, qp: u8, kicks: usize, info: Option<tokio::sync::oneshot::Sender<Info>>) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            wake: Condvar::new(),
            queued: std::sync::atomic::AtomicUsize::new(0),
        });
        let (tx, out) = mpsc::unbounded_channel();
        let sh = shared.clone();
        let spawned = std::thread::Builder::new().name("ictl-videotoolbox".into()).spawn(move || {
            if let Err(e) = run(&sh, w, h, qp, kicks, tx.clone(), info) {
                note(&sh, &e);
                let _ = tx.send(Err(e));
            }
        });
        if let Err(e) = spawned {
            note(&shared, &e.to_string());
        }
        Self { shared, out }
    }

    pub fn push(&self, jpeg: Arc<Vec<u8>>) {
        self.shared.state.lock().unwrap_or_else(|p| p.into_inner()).next = Some(jpeg);
        self.shared.wake.notify_all();
    }

    /// The next packet; `None` once the thread has ended. Cancel-safe
    pub async fn next(&mut self) -> Option<Result<Packet, String>> {
        let p = self.out.recv().await;
        if let Some(Ok(Packet::Frame { .. })) = &p {
            // counted before it was sent ([`on_encoded`]): at least one
            self.shared.queued.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            // under the lock: the thread checks the count with it held, so this wake-up can't come in between
            let _held = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
            self.shared.wake.notify_all();
        }
        p
    }

    pub fn written(&self) -> usize {
        self.shared.state.lock().unwrap_or_else(|p| p.into_inner()).written
    }

    pub fn error(&self) -> String {
        let s = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if s.errors.is_empty() {
            ictl_l10n::tr!("VideoToolbox 编码停了", "VideoToolbox encoding stopped").into()
        } else {
            s.errors.iter().cloned().collect::<Vec<_>>().join("; ")
        }
    }
}

fn note(sh: &Shared, e: &str) {
    let mut s = sh.state.lock().unwrap_or_else(|p| p.into_inner());
    if s.errors.len() == 3 {
        s.errors.pop_front();
    }
    s.errors.push_back(e.to_owned());
}

fn run(
    sh: &Arc<Shared>,
    w: u32,
    h: u32,
    qp: u8,
    kicks: usize,
    tx: mpsc::UnboundedSender<Result<Packet, String>>,
    info: Option<tokio::sync::oneshot::Sender<Info>>,
) -> Result<(), String> {
    let (ew, eh) = even((w, h));
    let sink = Sink { tx, avcc: Mutex::default(), h264: true, queued: Some(sh.clone()) };
    let (mut enc, pinned_qp) = h264(ew, eh, qp, sink)?;
    let mut dec = Decoder::new(w, h)?;
    if let Some(i) = info {
        let _ = i.send(Info { hardware: enc.hardware, pinned_qp });
    }
    loop {
        let jpeg = {
            let mut s = sh.state.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if s.stop {
                    return Ok(());
                }
                // the caller is behind: wait for it (the frame waiting meanwhile is replaced by newer ones)
                if sh.queued.load(std::sync::atomic::Ordering::SeqCst) < MAX_AHEAD
                    && let Some(j) = s.next.take()
                {
                    break j;
                }
                s = sh.wake.wait(s).unwrap_or_else(|p| p.into_inner());
            }
        };
        if super::mjpeg::jpeg_size(&jpeg) != Some(dec.size) {
            return Err(format!("JPEG of another size than {}×{}", dec.size.0, dec.size.1));
        }
        let image = dec.decode(&jpeg)?;
        // counted before it goes in: the frame may reach the caller before `encode` returns
        sh.state.lock().unwrap_or_else(|p| p.into_inner()).written += 1 + kicks;
        for _ in 0..=kicks {
            enc.encode(image.ptr())?;
        }
    }
}

// ------------------------------------------------------------------ a test picture without ffmpeg

/// A JPEG of `w`×`h` from VideoToolbox's JPEG encoder: a list of rows with dark "text" and coloured icons,
/// scrolled by `n` so consecutive pictures differ, about as large as a phone screen of that size. Blocks the calling
/// thread for a moment
pub fn sample_jpeg(w: u32, h: u32, n: u32) -> Result<Vec<u8>, String> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let image = pattern(w, h, n)?;
    let tune = |s: *mut c_void| set(s, unsafe { kVTCompressionPropertyKey_Quality }, number_f64(0.8).ptr(), "Quality");
    let sink = Sink { tx, avcc: Mutex::default(), h264: false, queued: None };
    let mut e = Encoder::new(w, h, kCMVideoCodecType_JPEG, tune, sink)?;
    e.encode(image.ptr())?;
    drop(e);
    match rx.try_recv() {
        Ok(Ok(Packet::Frame { data, .. })) if super::mjpeg::jpeg_size(&data) == Some((w, h)) => Ok(data),
        Ok(Ok(_)) => Err("VideoToolbox JPEG encoder: not a JPEG of that size".into()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("VideoToolbox JPEG encoder: nothing came out".into()),
    }
}

/// An NV12 picture to encode (see [`sample_jpeg`])
fn pattern(w: u32, h: u32, n: u32) -> Result<Cf, String> {
    let attrs = Dict::new();
    attrs.set(unsafe { kCVPixelBufferIOSurfacePropertiesKey }, Dict::new().0.ptr());
    let mut pb = std::ptr::null_mut();
    let st = unsafe {
        CVPixelBufferCreate(
            std::ptr::null(),
            w as usize,
            h as usize,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            attrs.0.ptr(),
            &mut pb,
        )
    };
    check(st, "CVPixelBufferCreate")?;
    let pb = Cf(pb);
    unsafe {
        check(CVPixelBufferLockBaseAddress(pb.ptr(), 0), "CVPixelBufferLockBaseAddress")?;
        let (bw, bh) = (CVPixelBufferGetWidth(pb.ptr()), CVPixelBufferGetHeight(pb.ptr()));
        let (y, ys) =
            (CVPixelBufferGetBaseAddressOfPlane(pb.ptr(), 0), CVPixelBufferGetBytesPerRowOfPlane(pb.ptr(), 0));
        let (uv, uvs) =
            (CVPixelBufferGetBaseAddressOfPlane(pb.ptr(), 1), CVPixelBufferGetBytesPerRowOfPlane(pb.ptr(), 1));
        // a list scrolling 4 rows a picture: rows of 48, two in three with a line of "text" (glyphs of 2-pixel strokes
        // in 9-pixel cells, words of 3 to 8), a coloured icon on the left of every third row
        let scroll = n as usize * 4;
        let glyph = |cell: usize, x: usize, y: usize| -> bool {
            // y in 0..12, x in 0..9: a few stroke shapes chosen by the cell number
            let mid = (5..7).contains(&x);
            let even = cell.is_multiple_of(2);
            let v = x < 2 || (cell.is_multiple_of(3) && mid);
            let hz = (cell % 4 == 1 && y < 2) || (cell % 5 == 2 && y >= 10) || (cell % 7 == 3 && (5..7).contains(&y));
            (v && even) || hz || (!even && mid)
        };
        for r in 0..bh {
            let row = std::slice::from_raw_parts_mut(y.add(r * ys), bw);
            let (item, line) = ((r + scroll) / 48, (r + scroll) % 48);
            let has_text = item % 3 != 2 && (18..30).contains(&line);
            for (c, p) in row.iter_mut().enumerate() {
                let mut dark = false;
                if has_text && c >= 64 && c < bw.saturating_sub(24) {
                    let cell = (c - 64) / 9 + item * 13;
                    // a space after every 3 to 8 letters
                    let word_len = 3 + (cell / 11) % 6;
                    let in_space = (cell % (word_len + 1)) == word_len;
                    dark = !in_space && glyph(cell, (c - 64) % 9, line - 18);
                }
                // separator line at the bottom of each row
                *p = if dark {
                    35
                } else if line == 47 {
                    200
                } else {
                    245
                };
            }
        }
        for r in 0..bh.div_ceil(2) {
            let row = std::slice::from_raw_parts_mut(uv.add(r * uvs), bw.div_ceil(2) * 2);
            let (item, line) = ((r * 2 + scroll) / 48, (r * 2 + scroll) % 48);
            for (c, p) in row.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let icon = item % 3 == 0 && (10..38).contains(&line) && (6..20).contains(&c);
                p[0] = if icon { 90 + (item % 5) as u8 * 20 } else { 128 };
                p[1] = if icon { 200 - (item % 4) as u8 * 25 } else { 128 };
            }
        }
        CVPixelBufferUnlockBaseAddress(pb.ptr(), 0);
    }
    Ok(pb)
}

#[cfg(test)]
mod tests;

/// Measuring the encoder on a recording of the phone's real picture: traffic, size per frame, time per frame and
/// SSIM of what the page would show against what the encoder was given. Records come from the viewer's `/stream`
/// (repeated frames already dropped), one JPEG per file named `<ms since start>.jpg`:
/// `ICTL_RECORDING=<dir> ICTL_QPS=20,24 cargo test --release -p iphone-control measure_recording -- --ignored --nocapture`
#[cfg(test)]
mod measure;
