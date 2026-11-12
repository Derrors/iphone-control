//! Telling the page's decoder that frames are never reordered, so it shows each one as soon as it is decoded.
//!
//! An SPS without the VUI's bitstream restriction (`max_num_reorder_frames`) leaves a decoder to assume the worst:
//! as many frames reordered as the level's picture buffer holds. Chrome's hardware decoder on macOS then keeps that
//! many frames before it outputs one (4 for the phone's 990×2151 at level 4.2, measured), `optimizeForLatency` or not:
//! the picture runs 4 frames late, and when the screen stops changing no frame follows to push the last 4 out, so the
//! page stays on a frame from the middle of the last movement. VideoToolbox writes no VUI at all.
//!
//! [`low_delay`] rewrites the SPS in the `avcC` with the restriction saying 0 frames are reordered (true: B-frames
//! are off everywhere) and the buffer is the reference frames only. Only the `avcC` matters: the frames carry no SPS.
//! An SPS that already says 0 is left as it is.

/// The `avcC` with every SPS saying no frame is reordered; `None` when it can't be read (then send it as it is)
pub fn low_delay(avcc: &[u8]) -> Option<Vec<u8>> {
    let n_sps = usize::from(*avcc.get(5)? & 0x1f);
    let mut out = avcc.get(..6)?.to_vec();
    let mut i = 6;
    for _ in 0..n_sps {
        let len = usize::from(u16::from_be_bytes([*avcc.get(i)?, *avcc.get(i + 1)?]));
        let sps = avcc.get(i + 2..i + 2 + len)?;
        let new = sps_low_delay(sps)?;
        out.extend_from_slice(&u16::try_from(new.len()).ok()?.to_be_bytes());
        out.extend_from_slice(&new);
        i += 2 + len;
    }
    // the PPS and, for High profiles, the extension: unchanged
    out.extend_from_slice(avcc.get(i..)?);
    Some(out)
}

/// What [`read`] found in an SPS
#[derive(Debug, PartialEq)]
struct Sps {
    max_num_ref_frames: u32,
    /// Bit position (in the RBSP) of the field to rewrite from: `bitstream_restriction_flag`, or
    /// `vui_parameters_present_flag` when there is no VUI
    cut: usize,
    vui: bool,
    /// `max_num_reorder_frames`, when the VUI has the restriction
    reorder: Option<u32>,
}

fn sps_low_delay(nal: &[u8]) -> Option<Vec<u8>> {
    let (&header, ebsp) = nal.split_first()?;
    if header & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(ebsp);
    let s = read(&rbsp)?;
    if s.reorder == Some(0) {
        return Some(nal.to_vec());
    }
    let mut w = Writer::default();
    let mut r = Reader::new(&rbsp);
    for _ in 0..s.cut {
        w.bit(r.u(1)?);
    }
    if !s.vui {
        // vui_parameters_present_flag, then no aspect ratio, overscan, signal type, chroma location, timing, NAL or
        // VCL HRD, picture structure
        w.bit(1);
        for _ in 0..8 {
            w.bit(0);
        }
    }
    w.bit(1); // bitstream_restriction_flag
    w.bit(1); // motion_vectors_over_pic_boundaries_flag
    w.ue(2); // max_bytes_per_pic_denom (the default)
    w.ue(1); // max_bits_per_mb_denom (the default)
    w.ue(16); // log2_max_mv_length_horizontal (the default)
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(s.max_num_ref_frames.max(1)); // max_dec_frame_buffering
    w.bit(1); // rbsp_stop_one_bit
    let mut out = vec![header];
    out.extend(escape(&w.finish()));
    Some(out)
}

/// Reads an SPS RBSP (after the NAL header) up to the VUI's bitstream restriction
fn read(rbsp: &[u8]) -> Option<Sps> {
    let mut r = Reader::new(rbsp);
    let profile = r.u(8)?;
    r.u(8)?;
    r.u(8)?;
    r.ue()?;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.u(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.u(1)?;
        if r.u(1)? == 1 {
            for list in 0..if chroma == 3 { 12 } else { 8 } {
                if r.u(1)? == 1 {
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..if list < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?;
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.u(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue()?;
    r.u(1)?;
    r.ue()?;
    r.ue()?;
    if r.u(1)? == 0 {
        r.u(1)?;
    }
    r.u(1)?;
    if r.u(1)? == 1 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let at_vui = r.pos;
    if r.u(1)? == 0 {
        return Some(Sps { max_num_ref_frames, cut: at_vui, vui: false, reorder: None });
    }
    if r.u(1)? == 1 && r.u(8)? == 255 {
        r.u(32)?;
    }
    if r.u(1)? == 1 {
        r.u(1)?;
    }
    if r.u(1)? == 1 {
        r.u(4)?;
        if r.u(1)? == 1 {
            r.u(24)?;
        }
    }
    if r.u(1)? == 1 {
        r.ue()?;
        r.ue()?;
    }
    if r.u(1)? == 1 {
        r.u(32)?;
        r.u(32)?;
        r.u(1)?;
    }
    let hrd = |r: &mut Reader| -> Option<()> {
        let count = r.ue()? + 1;
        r.u(8)?;
        for _ in 0..count {
            r.ue()?;
            r.ue()?;
            r.u(1)?;
        }
        r.u(20)?;
        Some(())
    };
    let nal_hrd = r.u(1)? == 1;
    if nal_hrd {
        hrd(&mut r)?;
    }
    let vcl_hrd = r.u(1)? == 1;
    if vcl_hrd {
        hrd(&mut r)?;
    }
    if nal_hrd || vcl_hrd {
        r.u(1)?;
    }
    r.u(1)?;
    let cut = r.pos;
    let reorder = if r.u(1)? == 1 {
        r.u(1)?;
        for _ in 0..4 {
            r.ue()?;
        }
        Some(r.ue()?)
    } else {
        None
    };
    Some(Sps { max_num_ref_frames, cut, vui: true, reorder })
}

/// Emulation prevention removed (`00 00 03` → `00 00`)
fn unescape(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0;
    for &b in ebsp {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Emulation prevention added: a `03` after two zeros wherever the next byte is 0 to 3
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.u(zeros)?)
    }

    fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
    bits: usize,
}

impl Writer {
    fn bit(&mut self, b: u32) {
        if self.bits.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if b != 0 {
            *self.bytes.last_mut().expect("pushed above") |= 0x80 >> (self.bits % 8);
        }
        self.bits += 1;
    }

    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        for _ in 1..len {
            self.bit(0);
        }
        for i in (0..len).rev() {
            self.bit(((x >> i) & 1) as u32);
        }
    }

    /// The bytes, zero-padded to a whole byte (after the stop bit the caller wrote)
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// The `avcC` VideoToolbox gave for the phone's 990×2151 picture (iPhone 18 Pro Max, macOS 27): High profile,
    /// level 4.2, one reference frame, no VUI. After the rewrite the SPS says no frame is reordered, everything
    /// else (the fields before, the PPS, the High-profile extension) is unchanged, and doing it again changes nothing
    #[test]
    fn videotoolbox_sps_gets_no_reordering() {
        let avcc = hex("0164002affe1000c2764002aac5680f8043fa95001000428ee3cb0fdf8f800");
        let sps = &avcc[8..20];
        let before = read(&unescape(&sps[1..])).unwrap();
        assert_eq!(before, Sps { max_num_ref_frames: 1, cut: before.cut, vui: false, reorder: None });

        let fixed = low_delay(&avcc).unwrap();
        assert_eq!(&fixed[..6], &avcc[..6], "profile and level as they were");
        let len = usize::from(u16::from_be_bytes([fixed[6], fixed[7]]));
        let new_sps = &fixed[8..8 + len];
        let after = read(&unescape(&new_sps[1..])).unwrap();
        assert_eq!((after.vui, after.reorder, after.max_num_ref_frames), (true, Some(0), 1));
        assert_eq!(after.cut - before.cut, 9, "the VUI's 9 flags up to the restriction");
        // the bits before the VUI are the same
        let (a, b) = (unescape(&sps[1..]), unescape(&new_sps[1..]));
        let (mut ra, mut rb) = (Reader::new(&a), Reader::new(&b));
        for _ in 0..before.cut {
            assert_eq!(ra.u(1), rb.u(1));
        }
        assert_eq!(&fixed[8 + len..], &avcc[20..], "PPS and extension untouched");
        assert_eq!(low_delay(&fixed).unwrap(), fixed, "already says 0: unchanged");
    }

    /// An SPS with a VUI (signal type, timing) but no restriction keeps those and gets the restriction after them
    #[test]
    fn vui_without_restriction_keeps_its_fields() {
        // Baseline SPS built here: profile 66, level 31, sps id 0, log2_max_frame_num 4, poc type 2, 1 ref frame,
        // 40×23 macroblocks, frame_mbs_only, direct_8x8, no cropping; VUI with full-range video signal and timing
        let mut w = Writer::default();
        for (v, n) in [(66, 8), (0, 8), (31, 8)] {
            for i in (0..n).rev() {
                w.bit((v >> i) & 1);
            }
        }
        for v in [0, 0, 2, 1] {
            w.ue(v);
        }
        w.bit(0);
        w.ue(39);
        w.ue(22);
        w.bit(1);
        w.bit(1);
        w.bit(0);
        w.bit(1); // VUI
        w.bit(0);
        w.bit(0);
        w.bit(1); // video signal type: format 5, full range, no colour description
        for b in [1, 0, 1, 1, 0] {
            w.bit(b);
        }
        w.bit(0);
        w.bit(1); // timing
        for _ in 0..64 {
            w.bit(1);
        }
        w.bit(1);
        for _ in 0..4 {
            w.bit(0); // no NAL HRD, no VCL HRD, no pic struct, no bitstream restriction
        }
        w.bit(1); // rbsp_stop_one_bit
        let rbsp = w.finish();
        let s = read(&rbsp).unwrap();
        assert_eq!((s.vui, s.reorder, s.max_num_ref_frames), (true, None, 1));
        let mut nal = vec![0x67];
        nal.extend(escape(&rbsp));
        let fixed = sps_low_delay(&nal).unwrap();
        let after = read(&unescape(&fixed[1..])).unwrap();
        assert_eq!((after.vui, after.reorder, after.cut), (true, Some(0), s.cut));
    }

    #[test]
    fn escaping_round_trips() {
        let raw = [0, 0, 0, 0, 1, 0, 0, 3, 0, 0, 2, 5];
        let e = escape(&raw);
        assert_eq!(e, [0, 0, 3, 0, 0, 3, 1, 0, 0, 3, 3, 0, 0, 3, 2, 5]);
        assert_eq!(unescape(&e), raw);
    }

    #[test]
    fn unreadable_input_is_none() {
        assert_eq!(low_delay(&[1, 2]), None);
        assert_eq!(low_delay(&hex("0164002affe10002ff00")), None, "not an SPS");
    }
}
