//! The phone's unified log over `com.apple.os_trace_relay`, with our own `StartActivity` request.
//!
//! idevice's `OsTraceRelayClient::start_trace` always asks for `StreamFlags: 60`, which includes debug messages.
//! Streaming debug messages makes every process on the phone produce them (hundreds of lines a second) and visibly
//! slows the phone: gestures get slower and some fail. So we send the request ourselves with the flags we want and
//! read the entries with a bounds-checked parser.
//!
//! Packet layout follows idevice's `OsTraceRelayReceiver::next` (MIT, Copyright Jackson Coxson), itself based on
//! pymobiledevice3's `os_trace.py`.

use idevice::{Idevice, IdeviceService, os_trace_relay::OsTraceRelayClient, provider::IdeviceProvider};

use crate::{DeviceError, Result};

/// `StreamFlags` bits of the activity stream (Apple's private `os_activity_stream_flag_t`)
pub mod flags {
    pub const PAYLOAD: u64 = 0x04;
    pub const HISTORICAL: u64 = 0x08;
    pub const CALLSTACK: u64 = 0x10;
    pub const DEBUG: u64 = 0x20;
    /// What idevice and pymobiledevice3 send
    pub const IDEVICE_DEFAULT: u64 = PAYLOAD | HISTORICAL | CALLSTACK | DEBUG;
}

/// Fixed part of an entry before the strings
const HEADER: usize = 129;

/// One raw entry
#[derive(Debug, Clone)]
pub struct RawEntry {
    pub pid: u32,
    /// 0 notice, 1 info, 2 debug, 0x10 error, 0x11 fault
    pub level: u8,
    pub filename: String,
    pub subsystem: Option<(String, String)>,
    pub message: String,
}

pub struct OsLogStream(Idevice);

/// Opens the relay and starts streaming every process with `stream_flags`
pub async fn start(provider: &dyn IdeviceProvider, stream_flags: u64) -> Result<OsLogStream> {
    let mut idevice = OsTraceRelayClient::connect(provider).await?.idevice;
    let mut req = plist::Dictionary::new();
    req.insert("Request".into(), "StartActivity".into());
    req.insert("Pid".into(), plist::Value::Integer((-1i64).into()));
    req.insert("MessageFilter".into(), plist::Value::Integer(65_535u64.into()));
    req.insert("StreamFlags".into(), plist::Value::Integer(stream_flags.into()));
    let mut body = Vec::new();
    plist::Value::Dictionary(req).to_writer_binary(&mut body).map_err(|e| DeviceError::Idevice(e.to_string()))?;
    let mut msg = (body.len() as u32).to_be_bytes().to_vec();
    msg.extend_from_slice(&body);
    idevice.send_raw(&msg).await?;
    // one byte, then a length-prefixed plist with the status
    idevice.read_raw(1).await?;
    let len = u32::from_be_bytes(be4(&idevice.read_raw(4).await?)?) as usize;
    let res: plist::Value =
        plist::from_bytes(&idevice.read_raw(len).await?).map_err(|e| DeviceError::Idevice(e.to_string()))?;
    let status = res.as_dictionary().and_then(|d| d.get("Status")).and_then(|s| s.as_string());
    if status != Some("RequestSuccessful") {
        return Err(DeviceError::Idevice(format!("os_trace_relay StartActivity: {res:?}")));
    }
    Ok(OsLogStream(idevice))
}

impl OsLogStream {
    /// The next entry; an error means the stream is gone or out of step. An entry that doesn't parse is
    /// skipped (its length is known, so the stream stays in step)
    pub async fn next(&mut self) -> Result<RawEntry> {
        loop {
            if self.0.read_raw(1).await?[0] != 0x02 {
                return Err(DeviceError::Idevice("os_trace_relay: unexpected packet start".into()));
            }
            let len = u32::from_le_bytes(be4(&self.0.read_raw(4).await?)?) as usize;
            let packet = self.0.read_raw(len).await?;
            match parse(&packet) {
                Some(e) => return Ok(e),
                None => tracing::debug!("os_trace_relay: skipped an unreadable entry ({len} bytes)"),
            }
        }
    }
}

fn be4(b: &[u8]) -> Result<[u8; 4]> {
    b.get(..4).and_then(|s| s.try_into().ok()).ok_or_else(|| DeviceError::Idevice("short read".into()))
}

/// Parses one entry; `None` if it is shorter than its own sizes say
pub fn parse(p: &[u8]) -> Option<RawEntry> {
    let u16le = |at: usize| p.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32le = |at: usize| p.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if p.len() < HEADER {
        return None;
    }
    let pid = u32le(9)?;
    let level = *p.get(68)?;
    let image_name_size = u16le(107)?;
    let message_size = u16le(109)?;
    let subsystem_size = u32le(117)? as usize;
    let category_size = u32le(121)? as usize;
    let rest = &p[HEADER..];
    let nul = rest.iter().position(|&b| b == 0)?;
    let filename = String::from_utf8_lossy(&rest[..nul]).into_owned();
    let mut at = nul + 1 + image_name_size;
    let message = take(rest, &mut at, message_size)?;
    // the sizes can be set while the strings are left out (idevice does the same check)
    let subsystem = if subsystem_size > 0 && category_size > 0 && at < rest.len() {
        let sub = take(rest, &mut at, subsystem_size)?;
        Some((sub, take(rest, &mut at, category_size)?))
    } else {
        None
    };
    Some(RawEntry { pid, level, filename, subsystem, message })
}

/// The `n`-byte string at `*at` (NUL-terminated, the size includes the NUL), moving `at` past it
fn take(rest: &[u8], at: &mut usize, n: usize) -> Option<String> {
    let s = rest.get(*at..at.checked_add(n)?)?;
    *at += n;
    Some(String::from_utf8_lossy(s.strip_suffix(&[0]).unwrap_or(s)).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: u8, file: &str, msg: &str, label: Option<(&str, &str)>) -> Vec<u8> {
        let mut p = vec![0u8; HEADER];
        p[9..13].copy_from_slice(&42u32.to_le_bytes());
        p[68] = level;
        let image = b"img\0";
        p[107..109].copy_from_slice(&(image.len() as u16).to_le_bytes());
        p[109..111].copy_from_slice(&((msg.len() + 1) as u16).to_le_bytes());
        if let Some((s, c)) = label {
            p[117..121].copy_from_slice(&((s.len() + 1) as u32).to_le_bytes());
            p[121..125].copy_from_slice(&((c.len() + 1) as u32).to_le_bytes());
        }
        p.extend_from_slice(file.as_bytes());
        p.push(0);
        p.extend_from_slice(image);
        p.extend_from_slice(msg.as_bytes());
        p.push(0);
        if let Some((s, c)) = label {
            p.extend_from_slice(s.as_bytes());
            p.push(0);
            p.extend_from_slice(c.as_bytes());
            p.push(0);
        }
        p
    }

    #[test]
    fn parses_entries_and_rejects_short_ones() {
        let e = parse(&entry(0x10, "/usr/libexec/x", "boom", Some(("com.example", "net")))).unwrap();
        assert_eq!((e.pid, e.level, e.filename.as_str(), e.message.as_str()), (42, 0x10, "/usr/libexec/x", "boom"));
        assert_eq!(e.subsystem, Some(("com.example".into(), "net".into())));
        let e = parse(&entry(0, "/x", "hi", None)).unwrap();
        assert_eq!((e.message.as_str(), e.subsystem), ("hi", None));
        // cut anywhere: no panic, just None
        let full = entry(2, "/usr/bin/y", "message", Some(("s", "c")));
        for n in 0..full.len() {
            let _ = parse(&full[..n]);
        }
        assert!(parse(&full[..HEADER - 1]).is_none());
        // label sizes set but the strings left out
        let mut no_label = entry(0, "/x", "hi", None);
        no_label[117..121].copy_from_slice(&5u32.to_le_bytes());
        no_label[121..125].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(parse(&no_label).unwrap().subsystem, None);
        assert!(parse(&full[..full.len() - 3]).is_none());
    }
}
