//! Raw CoreFoundation, CoreMedia, CoreVideo, and VideoToolbox C FFI declarations.

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
pub type DecompressionCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, *mut c_void, CMTime, CMTime);

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
    pub fn CFDictionaryCreateMutable(a: CFAllocatorRef, cap: isize, k: *const Opaque, v: *const Opaque) -> *mut c_void;
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
    pub fn VTSessionCopyProperty(s: *mut c_void, key: CFStringRef, a: CFAllocatorRef, out: *mut c_void) -> OSStatus;
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
