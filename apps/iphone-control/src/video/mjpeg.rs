//! Reads WDA's MJPEG stream: an HTTP response whose body is `multipart/x-mixed-replace`. WDA gives every part a
//! `Content-Length`, so a frame is taken as soon as its last byte is in, without waiting for the next boundary.

use std::io;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

/// Largest JPEG accepted (a full-size frame at the highest quality is well under 2 MB)
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_LINE: u64 = 8 * 1024;

pub struct Frames<R> {
    r: BufReader<R>,
    /// Past the HTTP response head
    in_body: bool,
}

impl<R: AsyncRead + Unpin> Frames<R> {
    pub fn new(r: R) -> Self {
        Self { r: BufReader::with_capacity(256 * 1024, r), in_body: false }
    }

    /// The next JPEG; `None` when the stream ends
    pub async fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        if !self.in_body {
            let Some(status) = self.line().await? else { return Ok(None) };
            if String::from_utf8_lossy(&status).split(' ').nth(1) != Some("200") {
                return Err(bad("the MJPEG server didn't answer 200"));
            }
            loop {
                match self.line().await? {
                    None => return Ok(None),
                    Some(l) if l.is_empty() => break,
                    Some(_) => {}
                }
            }
            self.in_body = true;
        }
        // blank lines before the boundary, then the boundary and the part's headers up to a blank line
        let mut len = None;
        let mut in_part = false;
        loop {
            let Some(l) = self.line().await? else { return Ok(None) };
            if l.is_empty() {
                if in_part {
                    break;
                }
                continue;
            }
            in_part = true;
            let l = String::from_utf8_lossy(&l);
            if let Some((k, v)) = l.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                len = v.trim().parse::<usize>().ok();
            }
        }
        let len = len.ok_or_else(|| bad("a part without Content-Length"))?;
        if len > MAX_FRAME {
            return Err(bad("frame too large"));
        }
        let mut buf = vec![0; len];
        self.r.read_exact(&mut buf).await?;
        Ok(Some(buf))
    }

    /// One line without its line break; `None` at the end of the stream
    async fn line(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        let n = (&mut self.r).take(MAX_LINE).read_until(b'\n', &mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        if !line.ends_with(b"\n") {
            return Err(bad("header line too long or cut off"));
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        Ok(Some(line))
    }
}

fn bad(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m)
}

/// The part ffmpeg's `mpjpeg` input reads: its own boundary, the length, the JPEG
pub fn part(jpeg: &[u8]) -> Vec<u8> {
    let mut p = format!("--ictl\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", jpeg.len()).into_bytes();
    p.extend_from_slice(jpeg);
    p.extend_from_slice(b"\r\n");
    p
}

/// Width and height from a JPEG's frame header (SOF), without decoding it
pub fn jpeg_size(j: &[u8]) -> Option<(u32, u32)> {
    if j.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    while i + 4 <= j.len() {
        if j[i] != 0xFF {
            return None;
        }
        let m = j[i + 1];
        match m {
            // fill byte
            0xFF => i += 1,
            // markers without a length
            0x01 | 0xD0..=0xD7 => i += 2,
            // start of scan: the frame header comes before it
            0xDA => return None,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                let s = j.get(i + 5..i + 9)?;
                let h = u16::from_be_bytes([s[0], s[1]]);
                let w = u16::from_be_bytes([s[2], s[3]]);
                return (w > 0 && h > 0).then_some((u32::from(w), u32::from(h)));
            }
            _ => i += 2 + usize::from(u16::from_be_bytes([j[i + 2], j[i + 3]])),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG that is only the markers `jpeg_size` reads: SOI, an APP0 to skip, SOF0, SOS
    pub(crate) fn fake_jpeg(w: u16, h: u16, fill: u8) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, fill, fill];
        j.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        j.extend_from_slice(&h.to_be_bytes());
        j.extend_from_slice(&w.to_be_bytes());
        j.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        j.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xD9]);
        j
    }

    #[test]
    fn size_comes_from_the_frame_header() {
        assert_eq!(jpeg_size(&fake_jpeg(968, 2097, 0)), Some((968, 2097)));
        assert_eq!(jpeg_size(&fake_jpeg(2097, 968, 0)), Some((2097, 968)));
        assert_eq!(jpeg_size(b"not a jpeg"), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), None);
    }

    /// The stream as WDA sends it: response head, then parts with a blank line before each boundary
    #[tokio::test]
    async fn frames_are_cut_by_their_length() {
        let (a, b) = (fake_jpeg(10, 20, 1), fake_jpeg(10, 20, 2));
        let mut s = b"HTTP/1.0 200 OK\r\nServer: WDA MJPEG Server\r\nContent-Type: multipart/x-mixed-replace; boundary=--BoundaryString\r\n\r\n".to_vec();
        for j in [&a, &b] {
            s.extend_from_slice(
                format!("--BoundaryString\r\nContent-type: image/jpeg\r\nContent-Length: {}\r\n\r\n", j.len())
                    .as_bytes(),
            );
            s.extend_from_slice(j);
            s.extend_from_slice(b"\r\n\r\n");
        }
        let mut f = Frames::new(&s[..]);
        assert_eq!(f.next().await.unwrap(), Some(a));
        assert_eq!(f.next().await.unwrap(), Some(b));
        assert_eq!(f.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn refuses_an_error_answer_and_parts_without_length() {
        assert!(Frames::new(&b"HTTP/1.0 404 Not Found\r\n\r\n"[..]).next().await.is_err());
        let s = b"HTTP/1.0 200 OK\r\n\r\n--x\r\nContent-Type: image/jpeg\r\n\r\nabc";
        assert!(Frames::new(&s[..]).next().await.is_err());
    }

    #[test]
    fn part_is_what_mpjpeg_reads() {
        let p = part(b"JPEG");
        assert_eq!(p, b"--ictl\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\n\r\nJPEG\r\n");
    }
}
