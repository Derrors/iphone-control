//! DTX framing for remoteAXService: reading whole messages, writing method invocations and replies.
//!
//! idevice's `RemoteServerClient` stops its read loop at a message it can't decode, so messages are read here
//! frame by frame. Invocations reuse idevice's message types.

use idevice::dvt::message::{Aux, AuxValue, Message, MessageHeader, PayloadHeader};
use plist::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::archive::{self, Obj};

/// DTX message types (the first byte of the payload header)
pub const MSG_ACK: u8 = 0;
pub const MSG_INVOCATION: u8 = 2;
pub const MSG_RETURN_VALUE: u8 = 3;

/// One whole message, its fragments joined
#[derive(Debug, Clone)]
pub struct Frame {
    pub identifier: u32,
    pub conversation: u32,
    pub channel: i32,
    pub expects_reply: bool,
    pub msg_type: u8,
    /// The auxiliary arguments that are byte strings (archives), in order
    pub aux: Vec<Vec<u8>>,
    /// The payload proper: the selector of an invocation, the return value of a reply (both archived)
    pub data: Vec<u8>,
}

impl Frame {
    /// The selector of an invocation
    pub fn selector(&self) -> Option<String> {
        if self.msg_type != MSG_INVOCATION || self.data.is_empty() {
            return None;
        }
        archive::decode(&self.data).ok().and_then(|o| o.as_str().map(str::to_owned))
    }

    /// The first auxiliary argument, unarchived (for `processDataFromRemoteDevice:` an NSData holding another
    /// archive)
    pub fn first_arg(&self) -> Option<Obj> {
        self.aux.first().and_then(|b| archive::decode(b).ok())
    }
}

/// The largest message accepted, its fragments joined (a big accessibility reply is a few MB)
pub const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Reads the next whole message
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Frame> {
    read_frame_within(reader, MAX_MESSAGE_BYTES).await
}

async fn read_frame_within<R: AsyncRead + Unpin>(reader: &mut R, limit: usize) -> std::io::Result<Frame> {
    let mut payload = Vec::new();
    loop {
        let mut h = [0u8; 32];
        reader.read_exact(&mut h).await?;
        let word = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        if word(0) != 0x1F3D_5B79 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not a DTX message"));
        }
        let fragment = u16::from_le_bytes([h[8], h[9]]);
        let fragments = u16::from_le_bytes([h[10], h[11]]);
        // With several fragments the first one is the header alone
        if fragments > 1 && fragment == 0 {
            continue;
        }
        let len = word(12) as usize;
        // the whole message, all its fragments together, not each one: a header can claim anything
        if payload.len().saturating_add(len) > limit {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "DTX message too large"));
        }
        // read as it comes instead of allocating what the header claims up front
        let read = (&mut *reader).take(len as u64).read_to_end(&mut payload).await?;
        if read < len {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "DTX message cut short"));
        }
        if fragment + 1 >= fragments {
            return Ok(parse(word(16), word(20), word(24) as i32, word(28) == 1, &payload));
        }
    }
}

fn parse(identifier: u32, conversation: u32, channel: i32, expects_reply: bool, p: &[u8]) -> Frame {
    let mut f =
        Frame { identifier, conversation, channel, expects_reply, msg_type: 0, aux: Vec::new(), data: Vec::new() };
    if p.len() < 16 {
        return f;
    }
    f.msg_type = p[0];
    let aux_len = u32::from_le_bytes([p[4], p[5], p[6], p[7]]) as usize;
    let total = (u32::from_le_bytes([p[8], p[9], p[10], p[11]]) as usize).min(p.len() - 16);
    let aux_len = aux_len.min(total);
    if aux_len > 0
        && let Ok(aux) = Aux::from_bytes(p[16..16 + aux_len].to_vec())
    {
        f.aux = aux
            .values
            .into_iter()
            .filter_map(|v| match v {
                AuxValue::Array(b) => Some(b),
                _ => None,
            })
            .collect();
    }
    f.data = p[16 + aux_len..16 + total].to_vec();
    f
}

/// A method invocation on the global channel; `args` are archives
pub fn invocation(identifier: u32, selector: &str, args: Vec<Vec<u8>>, expects_reply: bool) -> Vec<u8> {
    let aux = (!args.is_empty()).then(|| Aux::from_values(args.into_iter().map(AuxValue::Array).collect()));
    Message::new(
        MessageHeader::new(0, 1, identifier, 0, 0, expects_reply),
        PayloadHeader::method_invocation(),
        aux,
        Some(Value::String(selector.into())),
    )
    .serialize()
}

/// The reply to a message that wants one: an empty acknowledgement, or a return value
pub fn reply(to: &Frame, value: Option<Value>) -> Vec<u8> {
    let with_value = value.is_some();
    let mut b = Message::new(
        MessageHeader::new(0, 1, to.identifier, to.conversation + 1, to.channel, false),
        PayloadHeader::new(),
        None,
        value,
    )
    .serialize();
    if with_value {
        // The payload header follows the 32-byte message header; its first byte is the type, which idevice
        // doesn't let us set
        b[32] = MSG_RETURN_VALUE;
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_invocation_reads_back() {
        let args = vec![archive::encode_scalar(Value::Boolean(true))];
        let bytes = invocation(9, "clientNeedsAccessibility:", args, true);
        let f = read_frame(&mut &bytes[..]).await.unwrap();
        assert_eq!((f.identifier, f.conversation, f.expects_reply, f.msg_type), (9, 0, true, MSG_INVOCATION));
        assert_eq!(f.selector().as_deref(), Some("clientNeedsAccessibility:"));
        assert_eq!(f.first_arg(), Some(Obj::Bool(true)));
    }

    /// A fragment's 32-byte header and its bytes
    fn fragment(index: u16, count: u16, data: &[u8]) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend(0x1F3D_5B79u32.to_le_bytes());
        h.extend(32u32.to_le_bytes());
        h.extend(index.to_le_bytes());
        h.extend(count.to_le_bytes());
        h.extend((data.len() as u32).to_le_bytes());
        h.extend([5u32, 0, 0, 0].iter().flat_map(|w| w.to_le_bytes()));
        h.extend(data);
        h
    }

    #[tokio::test]
    async fn the_whole_message_is_limited_not_each_fragment() {
        // the first of several fragments is the header alone
        let parts = |n: usize| [fragment(0, 3, &[]), fragment(1, 3, &vec![1; n]), fragment(2, 3, &vec![2; n])].concat();
        assert!(read_frame_within(&mut &parts(40)[..], 100).await.is_ok());
        let e = read_frame_within(&mut &parts(60)[..], 100).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        // a header claiming more than arrives is an error, not a huge allocation
        let mut cut = fragment(0, 1, &[0; 10]);
        cut[12..16].copy_from_slice(&50u32.to_le_bytes());
        let e = read_frame_within(&mut &cut[..], 100).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn replies_answer_the_conversation() {
        let asked = read_frame(&mut &invocation(4, "hostAPIVersion", vec![], true)[..]).await.unwrap();
        let ack = read_frame(&mut &reply(&asked, None)[..]).await.unwrap();
        assert_eq!((ack.identifier, ack.conversation, ack.expects_reply, ack.msg_type), (4, 1, false, MSG_ACK));
        let answer = read_frame(&mut &reply(&asked, Some(Value::Integer(7u64.into())))[..]).await.unwrap();
        assert_eq!(answer.msg_type, MSG_RETURN_VALUE);
        assert_eq!(archive::decode(&answer.data).unwrap(), Obj::Int(7));
    }
}
