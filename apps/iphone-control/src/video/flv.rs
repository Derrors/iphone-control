//! Reads the FLV that ffmpeg writes. Every frame is a tag that carries its length, so a frame can go to the page
//! as soon as its last byte is in; in a raw H.264 stream the end of a frame only shows when the next one starts, and
//! the next one is held back by ffmpeg (see `ffmpeg.rs`). Only what ffmpeg's FLV muxer writes for H.264 is handled.

/// What one FLV video tag holds
#[derive(Debug, PartialEq, Eq)]
pub enum Packet {
    /// The decoder configuration (`avcC`), before the first frame of each encoder session
    Config(Vec<u8>),
    /// One encoded frame, NAL units with 4-byte lengths (the `avcC` format)
    Frame { key: bool, data: Vec<u8> },
}

/// A tag larger than this means the stream is out of step
const MAX_TAG: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    /// Past the file header
    started: bool,
}

impl Parser {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The next complete packet, if the bytes for one are in
    pub fn next(&mut self) -> Result<Option<Packet>, String> {
        loop {
            if !self.started {
                if self.buf.len() < 9 {
                    return Ok(None);
                }
                if &self.buf[..3] != b"FLV" {
                    return Err("ffmpeg's output isn't FLV".into());
                }
                // the header's own length, then the first "previous tag size" (0)
                let skip = u32::from_be_bytes([self.buf[5], self.buf[6], self.buf[7], self.buf[8]]) as usize + 4;
                if self.buf.len() < skip {
                    return Ok(None);
                }
                self.buf.drain(..skip);
                self.started = true;
            }
            if self.buf.len() < 11 {
                return Ok(None);
            }
            let size = u32::from_be_bytes([0, self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if size > MAX_TAG {
                return Err(format!("FLV tag of {size} bytes, out of step"));
            }
            let end = 11 + size + 4;
            if self.buf.len() < end {
                return Ok(None);
            }
            let kind = self.buf[0] & 0x1F;
            let tag: Vec<u8> = self.buf[11..11 + size].to_vec();
            self.buf.drain(..end);
            // 18 is the metadata, 8 audio (there is none)
            if kind != 9 || tag.len() < 5 {
                continue;
            }
            if tag[0] & 0x0F != 7 {
                return Err(format!("FLV video codec {} isn't H.264", tag[0] & 0x0F));
            }
            // byte 1: 0 sequence header, 1 frame, 2 end of sequence; then a 3-byte composition time
            match tag[1] {
                0 => return Ok(Some(Packet::Config(tag[5..].to_vec()))),
                1 => return Ok(Some(Packet::Frame { key: tag[0] >> 4 == 1, data: tag[5..].to_vec() })),
                _ => {}
            }
        }
    }
}

/// The WebCodecs codec string for an `avcC`: profile, compatibility flags and level, e.g. `avc1.640028`
pub fn codec_string(avcc: &[u8]) -> Option<String> {
    let p = avcc.get(1..4)?;
    Some(format!("avc1.{:02x}{:02x}{:02x}", p[0], p[1], p[2]))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn tag(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut t = vec![kind];
        t.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        t.extend_from_slice(&[0; 7]);
        t.extend_from_slice(body);
        t.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
        t
    }

    pub(crate) fn header() -> Vec<u8> {
        let mut h = b"FLV\x01\x01\x00\x00\x00\x09".to_vec();
        h.extend_from_slice(&[0; 4]);
        h
    }

    #[test]
    fn tags_come_out_whole_however_the_bytes_arrive() {
        let avcc = [1u8, 0x64, 0x00, 0x28, 0xFF];
        let mut s = header();
        s.extend(tag(18, b"metadata"));
        s.extend(tag(9, &[[0x17, 0, 0, 0, 0].as_slice(), &avcc].concat()));
        s.extend(tag(9, &[0x17, 1, 0, 0, 0, 0xAA, 0xBB]));
        s.extend(tag(9, &[0x27, 1, 0, 0, 0, 0xCC]));
        // one byte at a time: nothing comes out early, everything comes out once its last byte is in
        let mut p = Parser::default();
        let mut got = Vec::new();
        for b in &s {
            p.push(&[*b]);
            while let Some(x) = p.next().unwrap() {
                got.push(x);
            }
        }
        assert_eq!(
            got,
            [
                Packet::Config(avcc.to_vec()),
                Packet::Frame { key: true, data: vec![0xAA, 0xBB] },
                Packet::Frame { key: false, data: vec![0xCC] },
            ]
        );
        assert_eq!(codec_string(&avcc).as_deref(), Some("avc1.640028"));
    }

    #[test]
    fn other_input_is_an_error() {
        let mut p = Parser::default();
        p.push(b"HTTP/1.1 200 OK\r\n");
        assert!(p.next().is_err());
        let mut p = Parser::default();
        p.push(&header());
        p.push(&tag(9, &[0x1C, 1, 0, 0, 0]));
        assert!(p.next().is_err(), "HEVC isn't expected");
    }
}
