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

mod sys {
    use std::ffi::{c_char, c_void};

    pub type CFTypeRef = *const c_void;
    pub type CFStringRef = *const c_void;
    pub type CFAllocatorRef = *const c_void;
    pub type OSStatus = i32;

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct CMTime {
        pub value: i64,
        pub timescale: i32,
        pub flags: u32,
        pub epoch: i64,
    }

    #[repr(C)]
    pub struct CMSampleTimingInfo {
        pub duration: CMTime,
        pub pts: CMTime,
        pub dts: CMTime,
    }

    /// The dictionary callback structs, only ever passed by address
    #[repr(C)]
    pub struct Opaque {
        _p: [u8; 0],
    }

    pub type CompressionCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, *mut c_void);
    pub type DecompressionCallback =
        extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, *mut c_void, CMTime, CMTime);

    #[repr(C)]
    pub struct DecompressionCallbackRecord {
        pub callback: DecompressionCallback,
        pub refcon: *mut c_void,
    }

    pub const kCFNumberSInt32Type: isize = 3;
    pub const kCFNumberFloat64Type: isize = 6;
    pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
    pub const kCMTimeFlags_Valid: u32 = 1;
    /// 'avc1', 'jpeg'
    pub const kCMVideoCodecType_H264: u32 = u32::from_be_bytes(*b"avc1");
    pub const kCMVideoCodecType_JPEG: u32 = u32::from_be_bytes(*b"jpeg");
    /// '420v': NV12, video range (what ffmpeg's `format=nv12` gives too)
    pub const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: u32 = u32::from_be_bytes(*b"420v");

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFAllocatorNull: CFAllocatorRef;
        pub static kCFBooleanTrue: CFTypeRef;
        pub static kCFBooleanFalse: CFTypeRef;
        pub static kCFTypeDictionaryKeyCallBacks: Opaque;
        pub static kCFTypeDictionaryValueCallBacks: Opaque;
        pub fn CFRelease(cf: CFTypeRef);
        pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
        pub fn CFDictionaryCreateMutable(
            a: CFAllocatorRef,
            cap: isize,
            k: *const Opaque,
            v: *const Opaque,
        ) -> *mut c_void;
        pub fn CFDictionarySetValue(d: *mut c_void, k: *const c_void, v: *const c_void);
        pub fn CFDictionaryGetValue(d: CFTypeRef, k: *const c_void) -> *const c_void;
        pub fn CFNumberCreate(a: CFAllocatorRef, t: isize, v: *const c_void) -> CFTypeRef;
        pub fn CFArrayGetCount(a: CFTypeRef) -> isize;
        pub fn CFArrayGetValueAtIndex(a: CFTypeRef, i: isize) -> *const c_void;
        pub fn CFDataGetLength(d: CFTypeRef) -> isize;
        pub fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
        pub fn CFStringCreateWithCString(a: CFAllocatorRef, s: *const c_char, enc: u32) -> CFStringRef;
        pub fn CFBooleanGetValue(b: CFTypeRef) -> u8;
    }

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        pub static kCMTimeInvalid: CMTime;
        pub static kCMSampleAttachmentKey_NotSync: CFStringRef;
        pub static kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: CFStringRef;
        pub fn CMSampleBufferGetDataBuffer(s: *mut c_void) -> *mut c_void;
        pub fn CMSampleBufferGetFormatDescription(s: *mut c_void) -> *mut c_void;
        pub fn CMSampleBufferGetSampleAttachmentsArray(s: *mut c_void, create: u8) -> CFTypeRef;
        pub fn CMBlockBufferGetDataLength(b: *mut c_void) -> usize;
        pub fn CMBlockBufferCopyDataBytes(b: *mut c_void, offset: usize, len: usize, dest: *mut c_void) -> OSStatus;
        pub fn CMFormatDescriptionGetExtension(d: *mut c_void, key: CFStringRef) -> CFTypeRef;
        pub fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            d: *mut c_void,
            i: usize,
            ptr: *mut *const u8,
            size: *mut usize,
            count: *mut usize,
            nal_len: *mut i32,
        ) -> OSStatus;
        pub fn CMVideoFormatDescriptionCreate(
            a: CFAllocatorRef,
            codec: u32,
            w: i32,
            h: i32,
            ext: CFTypeRef,
            out: *mut *mut c_void,
        ) -> OSStatus;
        pub fn CMBlockBufferCreateWithMemoryBlock(
            a: CFAllocatorRef,
            mem: *mut c_void,
            len: usize,
            block_alloc: CFAllocatorRef,
            custom: *const c_void,
            offset: usize,
            data_len: usize,
            flags: u32,
            out: *mut *mut c_void,
        ) -> OSStatus;
        pub fn CMSampleBufferCreateReady(
            a: CFAllocatorRef,
            data: *mut c_void,
            desc: *mut c_void,
            n: isize,
            ntiming: isize,
            timing: *const CMSampleTimingInfo,
            nsizes: isize,
            sizes: *const usize,
            out: *mut *mut c_void,
        ) -> OSStatus;
    }

    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        pub static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
        pub static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
        pub static kCVPixelBufferWidthKey: CFStringRef;
        pub static kCVPixelBufferHeightKey: CFStringRef;
        pub fn CVPixelBufferCreate(
            a: CFAllocatorRef,
            w: usize,
            h: usize,
            fmt: u32,
            attrs: CFTypeRef,
            out: *mut *mut c_void,
        ) -> i32;
        pub fn CVPixelBufferLockBaseAddress(b: *mut c_void, flags: u64) -> i32;
        pub fn CVPixelBufferUnlockBaseAddress(b: *mut c_void, flags: u64) -> i32;
        pub fn CVPixelBufferGetBaseAddressOfPlane(b: *mut c_void, plane: usize) -> *mut u8;
        pub fn CVPixelBufferGetBytesPerRowOfPlane(b: *mut c_void, plane: usize) -> usize;
        pub fn CVPixelBufferGetWidth(b: *mut c_void) -> usize;
        pub fn CVPixelBufferGetHeight(b: *mut c_void) -> usize;
    }

    #[link(name = "VideoToolbox", kind = "framework")]
    unsafe extern "C" {
        pub static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;
        pub static kVTCompressionPropertyKey_RealTime: CFStringRef;
        pub static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
        pub static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
        pub static kVTCompressionPropertyKey_MaxFrameDelayCount: CFStringRef;
        pub static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
        pub static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
        pub static kVTCompressionPropertyKey_Quality: CFStringRef;
        pub static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFStringRef;
        pub static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
        pub fn VTSessionSetProperty(s: *mut c_void, key: CFStringRef, value: CFTypeRef) -> OSStatus;
        pub fn VTSessionCopyProperty(s: *mut c_void, key: CFStringRef, a: CFAllocatorRef, out: *mut c_void)
        -> OSStatus;
        pub fn VTCompressionSessionCreate(
            a: CFAllocatorRef,
            w: i32,
            h: i32,
            codec: u32,
            spec: CFTypeRef,
            source_attrs: CFTypeRef,
            data_alloc: CFAllocatorRef,
            callback: CompressionCallback,
            refcon: *mut c_void,
            out: *mut *mut c_void,
        ) -> OSStatus;
        pub fn VTCompressionSessionPrepareToEncodeFrames(s: *mut c_void) -> OSStatus;
        pub fn VTCompressionSessionEncodeFrame(
            s: *mut c_void,
            image: *mut c_void,
            pts: CMTime,
            duration: CMTime,
            props: CFTypeRef,
            frame_refcon: *mut c_void,
            flags_out: *mut u32,
        ) -> OSStatus;
        pub fn VTCompressionSessionCompleteFrames(s: *mut c_void, until: CMTime) -> OSStatus;
        pub fn VTCompressionSessionInvalidate(s: *mut c_void);
        pub fn VTDecompressionSessionCreate(
            a: CFAllocatorRef,
            desc: *mut c_void,
            spec: CFTypeRef,
            dest_attrs: CFTypeRef,
            callback: *const DecompressionCallbackRecord,
            out: *mut *mut c_void,
        ) -> OSStatus;
        pub fn VTDecompressionSessionDecodeFrame(
            s: *mut c_void,
            sample: *mut c_void,
            flags: u32,
            frame_refcon: *mut c_void,
            flags_out: *mut u32,
        ) -> OSStatus;
        pub fn VTDecompressionSessionWaitForAsynchronousFrames(s: *mut c_void) -> OSStatus;
        pub fn VTDecompressionSessionInvalidate(s: *mut c_void);
    }
}

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
}

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
    let _ = sink.tx.send(Ok(Packet::Frame { key, data }));
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
            let n = unsafe { CFDataGetLength(d) } as usize;
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
    if pps.is_empty() || !(1..=4).contains(&nal_len) {
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
        let slot = unsafe { &mut *self.out };
        *slot = DecodedSlot::default();
        let s = self.session.ptr();
        check(
            unsafe {
                VTDecompressionSessionDecodeFrame(s, sample.ptr(), 0, std::ptr::null_mut(), std::ptr::null_mut())
            },
            "VTDecompressionSessionDecodeFrame",
        )?;
        unsafe { VTDecompressionSessionWaitForAsynchronousFrames(s) };
        check(slot.status, "decoder")?;
        slot.image.take().ok_or_else(|| "decoder: no picture".to_owned())
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
        let shared = Arc::new(Shared { state: Mutex::default(), wake: Condvar::new() });
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
        self.out.recv().await
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
    sh: &Shared,
    w: u32,
    h: u32,
    qp: u8,
    kicks: usize,
    tx: mpsc::UnboundedSender<Result<Packet, String>>,
    info: Option<tokio::sync::oneshot::Sender<Info>>,
) -> Result<(), String> {
    let (ew, eh) = even((w, h));
    let (mut enc, pinned_qp) = h264(ew, eh, qp, Sink { tx, avcc: Mutex::default(), h264: true })?;
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
                if let Some(j) = s.next.take() {
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
    let mut e = Encoder::new(w, h, kCMVideoCodecType_JPEG, tune, Sink { tx, avcc: Mutex::default(), h264: false })?;
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
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn avcc_from_parameter_sets() {
        let sps = vec![0x67, 0x64, 0x00, 0x28, 0xAC];
        let pps = vec![0x68, 0xEE, 0x3C];
        let a = build_avcc(&[sps, pps], 4).unwrap();
        assert_eq!(a, [1, 0x64, 0x00, 0x28, 0xFF, 0xE1, 0, 5, 0x67, 0x64, 0x00, 0x28, 0xAC, 1, 0, 3, 0x68, 0xEE, 0x3C]);
        assert!(build_avcc(&[vec![0x68, 1]], 4).is_none(), "no SPS");
    }

    #[test]
    fn odd_sizes_grow_to_even() {
        assert_eq!(even((968, 2097)), (968, 2098));
        assert_eq!(even((481, 1041)), (482, 1042));
        assert_eq!(even((480, 1040)), (480, 1040));
    }

    #[test]
    fn quality_follows_qp() {
        assert_eq!(quality_for(20), 0.75);
        assert!(quality_for(24) < quality_for(20) && quality_for(1) <= 1.0 && quality_for(51) >= 0.1);
    }

    /// The native encoder on this Mac, when VideoToolbox can encode here (a CI VM may not): a test picture of an
    /// odd size (like the phone's 75 % frames) comes out as a keyframe after its `avcC`, the next one as a delta
    /// frame, and each comes out as soon as it went in, without a following frame
    #[tokio::test]
    async fn encodes_natively_without_holding_frames() {
        let (w, h) = (481, 1041);
        let pics: Vec<Arc<Vec<u8>>> = match (0..3).map(|n| sample_jpeg(w, h, n)).collect::<Result<Vec<_>, _>>() {
            Ok(p) => p.into_iter().map(Arc::new).collect(),
            Err(e) => {
                eprintln!("VideoToolbox can't make JPEG here ({e}), skipped");
                return;
            }
        };
        let (itx, irx) = tokio::sync::oneshot::channel();
        let mut s = Session::start(w, h, 20, 0, Some(itx));
        let info = match tokio::time::timeout(Duration::from_secs(10), irx).await {
            Ok(Ok(i)) => i,
            _ => {
                eprintln!("no VideoToolbox H.264 encoder here ({}), skipped", s.error());
                return;
            }
        };
        eprintln!("{info:?}");
        let mut got = Vec::new();
        for p in &pics {
            s.push(p.clone());
            loop {
                let pk = tokio::time::timeout(Duration::from_secs(5), s.next())
                    .await
                    .expect("a frame within 5 s of its picture, with nothing pushed after it")
                    .expect("session alive")
                    .unwrap_or_else(|e| panic!("{e}"));
                let frame = matches!(pk, Packet::Frame { .. });
                got.push(pk);
                if frame {
                    break;
                }
            }
        }
        let Packet::Config(avcc) = &got[0] else { panic!("config first: {got:?}") };
        assert!(super::super::flv::codec_string(avcc).unwrap().starts_with("avc1.64"), "High profile");
        assert_eq!(avcc[4] & 3, 3, "4-byte NAL lengths");
        assert!(matches!(got[1], Packet::Frame { key: true, .. }));
        assert!(matches!(got[2], Packet::Frame { key: false, .. }));
        assert!(matches!(got[3], Packet::Frame { key: false, .. }));
        assert_eq!(s.written(), 3);
        // every frame is NAL units with 4-byte lengths that add up
        for p in &got[1..] {
            let Packet::Frame { data, .. } = p else { unreachable!() };
            let mut i = 0;
            while i < data.len() {
                let n = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
                i += 4 + n;
            }
            assert_eq!(i, data.len());
        }
    }
}

/// Measuring the encoder on a recording of the phone's real picture: traffic, size per frame, time per frame and
/// SSIM of what the page would show against what the encoder was given. Records come from the viewer's `/stream`
/// (repeated frames already dropped), one JPEG per file named `<ms since start>.jpg`:
/// `ICTL_RECORDING=<dir> ICTL_QPS=20,24 cargo test --release -p iphone-control measure_recording -- --ignored --nocapture`
#[cfg(test)]
mod measure {
    use std::collections::BTreeMap;

    use super::*;

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
            a: CFAllocatorRef,
            count: usize,
            ptrs: *const *const u8,
            sizes: *const usize,
            nal_len: i32,
            out: *mut *mut c_void,
        ) -> OSStatus;
    }

    /// An H.264 decoder for the `avcC` (the page's side), giving NV12 of the frames' size
    fn h264_decoder(avcc: &[u8], size: (u32, u32)) -> Decoder {
        // avcC: 5 bytes, SPS count & 0x1f, then length-prefixed SPS; PPS count, length-prefixed PPS
        let mut sets: Vec<&[u8]> = Vec::new();
        let mut i = 5;
        // the SPS list, then the PPS list
        for _ in 0..2 {
            let n = (avcc[i] & 0x1f) as usize;
            i += 1;
            for _ in 0..n {
                let l = u16::from_be_bytes([avcc[i], avcc[i + 1]]) as usize;
                sets.push(&avcc[i + 2..i + 2 + l]);
                i += 2 + l;
            }
        }
        let ptrs: Vec<*const u8> = sets.iter().map(|s| s.as_ptr()).collect();
        let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
        let mut desc = std::ptr::null_mut();
        let st = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                std::ptr::null(),
                sets.len(),
                ptrs.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut desc,
            )
        };
        assert_eq!(st, 0, "H.264 format description");
        Decoder::with_desc(Cf(desc), size, size).unwrap()
    }

    /// The Y, Cb and Cr planes of an NV12 buffer
    fn planes(pb: &Cf) -> (usize, usize, Vec<u8>, Vec<u8>, Vec<u8>) {
        unsafe {
            CVPixelBufferLockBaseAddress(pb.ptr(), 1);
            let (w, h) = (CVPixelBufferGetWidth(pb.ptr()), CVPixelBufferGetHeight(pb.ptr()));
            let (y, ys) =
                (CVPixelBufferGetBaseAddressOfPlane(pb.ptr(), 0), CVPixelBufferGetBytesPerRowOfPlane(pb.ptr(), 0));
            let (uv, uvs) =
                (CVPixelBufferGetBaseAddressOfPlane(pb.ptr(), 1), CVPixelBufferGetBytesPerRowOfPlane(pb.ptr(), 1));
            let mut yy = Vec::with_capacity(w * h);
            for r in 0..h {
                yy.extend_from_slice(std::slice::from_raw_parts(y.add(r * ys), w));
            }
            let (cw, ch) = (w / 2, h / 2);
            let (mut u, mut v) = (Vec::with_capacity(cw * ch), Vec::with_capacity(cw * ch));
            for r in 0..ch {
                let row = std::slice::from_raw_parts(uv.add(r * uvs), cw * 2);
                for p in row.as_chunks::<2>().0 {
                    u.push(p[0]);
                    v.push(p[1]);
                }
            }
            CVPixelBufferUnlockBaseAddress(pb.ptr(), 1);
            (w, h, yy, u, v)
        }
    }

    /// SSIM of one plane the way ffmpeg's `ssim` filter computes it: 8×8 windows every 4 pixels
    fn ssim(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
        const C1: f64 = 0.01 * 0.01 * 255.0 * 255.0;
        const C2: f64 = 0.03 * 0.03 * 255.0 * 255.0;
        let (mut total, mut n) = (0.0, 0usize);
        let mut y = 0;
        while y + 8 <= h {
            let mut x = 0;
            while x + 8 <= w {
                let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0f64, 0f64, 0f64, 0f64, 0f64);
                for r in y..y + 8 {
                    for c in x..x + 8 {
                        let (p, q) = (f64::from(a[r * w + c]), f64::from(b[r * w + c]));
                        sa += p;
                        sb += q;
                        saa += p * p;
                        sbb += q * q;
                        sab += p * q;
                    }
                }
                let (ma, mb) = (sa / 64.0, sb / 64.0);
                let (va, vb, cov) = (saa / 64.0 - ma * ma, sbb / 64.0 - mb * mb, sab / 64.0 - ma * mb);
                total += ((2.0 * ma * mb + C1) * (2.0 * cov + C2)) / ((ma * ma + mb * mb + C1) * (va + vb + C2));
                n += 1;
                x += 4;
            }
            y += 4;
        }
        total / n.max(1) as f64
    }

    fn pct(v: &mut [f64], p: f64) -> f64 {
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * p).round() as usize]
    }

    #[tokio::test]
    #[ignore]
    async fn measure_recording() {
        let dir = std::env::var(ictl_daemon::protocol::env::RECORDING).expect("ICTL_RECORDING=<dir of <ms>.jpg>");
        let mut files: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let ms = p.file_stem()?.to_str()?.parse().ok()?;
                (p.extension()? == "jpg").then_some((ms, p))
            })
            .collect();
        files.sort();
        let frames: Vec<(u64, Arc<Vec<u8>>)> =
            files.iter().map(|(ms, p)| (*ms, Arc::new(std::fs::read(p).unwrap()))).collect();
        let size = super::super::mjpeg::jpeg_size(&frames[0].1).unwrap();
        let frames: Vec<_> =
            frames.into_iter().filter(|(_, j)| super::super::mjpeg::jpeg_size(j) == Some(size)).collect();
        let span = frames.last().unwrap().0 - frames[0].0;
        let jpeg_bytes: usize = frames.iter().map(|(_, j)| j.len()).sum();
        eprintln!(
            "recording: {} frames of {}x{} over {:.1} s; MJPEG (repeats dropped) {:.0} KB/s on average",
            frames.len(),
            size.0,
            size.1,
            span as f64 / 1000.0,
            jpeg_bytes as f64 / 1024.0 / (span as f64 / 1000.0)
        );
        let qps: Vec<u8> = std::env::var(ictl_daemon::protocol::env::QPS)
            .unwrap_or_else(|_| "20".into())
            .split(',')
            .filter_map(|q| q.trim().parse().ok())
            .collect();
        for qp in qps {
            let (itx, irx) = tokio::sync::oneshot::channel();
            let mut s = Session::start(size.0, size.1, qp, 0, Some(itx));
            let info = irx.await.unwrap();
            let mut reference = Decoder::new(size.0, size.1).unwrap();
            let mut avcc = Vec::new();
            let mut page: Option<Decoder> = None;
            let (mut ssims, mut ssims_y, mut times, mut sizes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let mut per_second: BTreeMap<u64, (usize, usize)> = BTreeMap::new();
            let mut keys = 0;
            for (ms, j) in &frames {
                let t = Instant::now();
                s.push(j.clone());
                let (key, data) = loop {
                    match s.next().await.unwrap().unwrap() {
                        Packet::Config(c) => avcc = c,
                        Packet::Frame { key, data } => break (key, data),
                    }
                };
                times.push(t.elapsed().as_secs_f64() * 1000.0);
                sizes.push(data.len() as f64);
                keys += usize::from(key);
                let e = per_second.entry((ms - frames[0].0) / 1000).or_default();
                e.0 += data.len();
                e.1 += j.len();
                let dec = page.get_or_insert_with(|| h264_decoder(&avcc, even(size)));
                let shown = planes(&dec.decode(&data).unwrap());
                let given = planes(&reference.decode(j).unwrap());
                let (w, h) = (shown.0, shown.1);
                let sy = ssim(&given.2, &shown.2, w, h);
                let su = ssim(&given.3, &shown.3, w / 2, h / 2);
                let sv = ssim(&given.4, &shown.4, w / 2, h / 2);
                ssims_y.push(sy);
                // ffmpeg's "All" for 4:2:0
                ssims.push((4.0 * sy + su + sv) / 6.0);
            }
            let rates: Vec<String> = per_second
                .iter()
                .map(|(sec, (h264, jpeg))| format!("{sec}s {}/{}", h264 / 1024, jpeg / 1024))
                .collect();
            let mut busy: Vec<f64> = per_second.values().map(|(b, _)| *b as f64 / 1024.0).collect();
            let total: usize = sizes.iter().map(|s| *s as usize).sum();
            eprintln!("\nqp {qp} ({info:?})");
            eprintln!(
                "  H.264 {:.0} KB in all = {:.1}% of MJPEG; {:.0} KB/s on average, per second median {:.0} max {:.0} KB/s",
                total as f64 / 1024.0,
                100.0 * total as f64 / jpeg_bytes as f64,
                total as f64 / 1024.0 / (span as f64 / 1000.0),
                pct(&mut busy.clone(), 0.5),
                pct(&mut busy, 1.0)
            );
            eprintln!(
                "  frames {} ({} key), bytes median {:.0} p90 {:.0} max {:.0}",
                sizes.len(),
                keys,
                pct(&mut sizes.clone(), 0.5),
                pct(&mut sizes.clone(), 0.9),
                pct(&mut sizes, 1.0)
            );
            eprintln!(
                "  SSIM all median {:.4} p10 {:.4} min {:.4}; Y median {:.4} min {:.4}",
                pct(&mut ssims.clone(), 0.5),
                pct(&mut ssims.clone(), 0.1),
                pct(&mut ssims, 0.0),
                pct(&mut ssims_y.clone(), 0.5),
                pct(&mut ssims_y, 0.0)
            );
            eprintln!(
                "  push to frame: median {:.1} ms, p90 {:.1}, max {:.1}",
                pct(&mut times.clone(), 0.5),
                pct(&mut times.clone(), 0.9),
                pct(&mut times, 1.0)
            );
            eprintln!("  KB/s H.264/MJPEG each second: {}", rates.join(", "));
        }
    }
}
