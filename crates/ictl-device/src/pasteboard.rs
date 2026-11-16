//! 手机的剪贴板：CoreDevice `com.apple.coredevice.pasteboardservice`（RemoteXPC，经隧道，免签名）。
//!
//! HID 后备路线用它输入中文等美式键盘按不出来的文字（[`crate::Tunnel::paste_text`]）：先存下用户原来的剪贴板，
//! 写入要输入的文字并读回核对，按 Cmd-V 粘贴，再把原来的剪贴板写回去。
//!
//! 读用 idevice 的 [`PasteboardServiceClient`]；写原来的内容要原样写回每一项、每一种类型，idevice 只提供写一段文字或一张
//! 图片，所以写由本模块自己发 `SET`（字段和 idevice `pasteboard_service.rs` 相同）。每次读写各开一条连接：上一条连接上
//! 晚到的 `PULL_REPLY` 不会被当成 `SET_REPLY`。写入都带 `sourceMetadata`，只写本机（见 [`set_message`]）。剪贴板的内容不写日志。

use std::borrow::Cow;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use idevice::{
    IdeviceError, ReadWrite, RemoteXpcClient, RsdService,
    core_device::{
        DataInclusionPolicy, GENERAL_PASTEBOARD, PasteboardPayload, PasteboardServiceClient, PasteboardSnapshot,
    },
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
    xpc::{Dictionary, XPCObject},
};

use crate::Result;

const SERVICE: &str = "com.apple.coredevice.pasteboardservice";

/// 写文字时声明的类型，和 idevice 的 `set_text` 一样
const TEXT_UTIS: [&str; 3] = ["public.utf8-plain-text", "public.plain-text", "public.text"];

/// 剪贴板里的一项：声明的类型（按优先顺序）和每种类型的数据
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Item {
    pub types: Vec<String>,
    pub data: Vec<(String, Vec<u8>)>,
}

/// 读到的剪贴板
#[derive(Debug, Clone)]
pub(crate) struct Contents {
    /// 每改一次加一。设备没给时为 None
    pub change_count: Option<i64>,
    /// 第一段文字（只看直接带着数据的项）
    pub text: Option<String>,
    /// 能原样写回时的全部内容：每一项的每种类型都直接带着数据。有「承诺」（要另外取的数据）或取数据出错的项时为 None
    pub items: Option<Vec<Item>>,
}

impl Contents {
    fn from_snapshot(s: &PasteboardSnapshot) -> Self {
        Self { change_count: s.change_count, text: s.text(), items: restorable(s) }
    }
}

/// 每一项的每种类型都直接带着数据时，按原来的顺序整理成能写回去的样子
fn restorable(s: &PasteboardSnapshot) -> Option<Vec<Item>> {
    s.items
        .iter()
        .map(|item| {
            let data = item
                .data
                .iter()
                .map(|e| match &e.payload {
                    PasteboardPayload::Inline(bytes) => Some((e.uti.clone(), bytes.clone())),
                    PasteboardPayload::Promised { .. } | PasteboardPayload::Error(_) => None,
                })
                .collect::<Option<Vec<_>>>()?;
            // 声明了类型却没给数据：写回去就少了，不算能还原
            if item.types.iter().any(|t| !data.iter().any(|(uti, _)| uti == t)) {
                return None;
            }
            Some(Item { types: item.types.clone(), data })
        })
        .collect()
}

/// 只有一段文字的剪贴板内容
pub(crate) fn text_items(text: &str) -> Vec<Item> {
    vec![Item {
        types: TEXT_UTIS.iter().map(|t| (*t).to_owned()).collect(),
        data: TEXT_UTIS.iter().map(|t| ((*t).to_owned(), text.as_bytes().to_vec())).collect(),
    }]
}

/// 写入方的标识（[`set_message`] 的 `sourceMetadata`）：每个进程一个
fn writer_nonce() -> &'static str {
    static NONCE: OnceLock<String> = OnceLock::new();
    NONCE.get_or_init(|| format!("iphone-control-{}", uuid::Uuid::new_v4()))
}

/// 本进程第几次写剪贴板（[`set_message`] 的 `sourceMetadata`）
static WRITES: AtomicI64 = AtomicI64::new(0);

/// `SET` 消息：用 `items` 替换整个通用剪贴板。
///
/// 一定要带 `sourceMetadata`：手机上的 dtpasteboardd 只在有它时才按「只写本机」写入（UIPasteboard
/// `setItemProviders:localOnly:expirationDate:`，Xcode 从电脑同步剪贴板也这样）。不带的话，写进去的内容会经「通用剪贴板」
/// 提供给机主的其他苹果设备，把那边的剪贴板也换掉。字段：写入方的 nonce、名字、递增的计数
fn set_message(items: &[Item]) -> XPCObject {
    let items = items
        .iter()
        .map(|item| {
            let mut data = Dictionary::new();
            for (uti, bytes) in &item.data {
                let mut payload = Dictionary::new();
                payload.insert("data".into(), XPCObject::Data(bytes.clone()));
                data.insert(uti.clone(), XPCObject::Dictionary(payload));
            }
            let mut d = Dictionary::new();
            d.insert("types".into(), XPCObject::Array(item.types.iter().cloned().map(XPCObject::String).collect()));
            d.insert("data".into(), XPCObject::Dictionary(data));
            XPCObject::Dictionary(d)
        })
        .collect();
    let mut msg = Dictionary::new();
    msg.insert("command".into(), XPCObject::String("SET".into()));
    msg.insert("pasteboardName".into(), XPCObject::String(GENERAL_PASTEBOARD.into()));
    msg.insert("items".into(), XPCObject::Array(items));
    let mut source = Dictionary::new();
    source.insert("nonce".into(), XPCObject::String(writer_nonce().into()));
    source.insert("pasteboardName".into(), XPCObject::String("iphone-control".into()));
    source.insert("changeCount".into(), XPCObject::Int64(WRITES.fetch_add(1, Ordering::Relaxed) + 1));
    msg.insert("sourceMetadata".into(), XPCObject::Dictionary(source));
    XPCObject::Dictionary(msg)
}

/// 只用来发 `SET` 的连接
struct Writer(RemoteXpcClient<Box<dyn ReadWrite>>);

impl RsdService for Writer {
    fn rsd_service_name() -> Cow<'static, str> {
        Cow::Borrowed(SERVICE)
    }

    async fn from_stream(stream: Box<dyn ReadWrite>) -> std::result::Result<Self, IdeviceError> {
        let mut inner = RemoteXpcClient::new(stream).await?;
        inner.do_handshake().await?;
        Ok(Self(inner))
    }
}

/// 读通用剪贴板。`matchSource`：数据是不是直接带着由放进去的 App 决定，不让手机现取每一项
/// （`allResolved` 遇到别的 App 承诺的大数据可能要等很久）
pub(crate) async fn read(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<Contents> {
    let mut c: PasteboardServiceClient<Box<dyn ReadWrite>> =
        PasteboardServiceClient::connect_rsd(adapter, handshake).await?;
    let s = c.get_with_policy(GENERAL_PASTEBOARD, DataInclusionPolicy::MatchSource).await?;
    Ok(Contents::from_snapshot(&s))
}

/// 用 `items` 替换通用剪贴板
pub(crate) async fn write(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake, items: &[Item]) -> Result<()> {
    let mut w = Writer::connect_rsd(adapter, handshake).await?;
    w.0.send_object(set_message(items), true).await?;
    // SET_REPLY 不带有用的信息，读掉就行
    w.0.recv().await?;
    Ok(())
}

/// 手机是否提供剪贴板服务
pub(crate) fn supported(handshake: &RsdHandshake) -> bool {
    handshake.services.contains_key(SERVICE)
}

#[cfg(test)]
mod tests {
    use idevice::core_device::{PasteboardEntry, PasteboardItem};

    use super::*;

    fn snapshot(items: Vec<PasteboardItem>) -> PasteboardSnapshot {
        PasteboardSnapshot { pasteboard_name: Some("general".into()), change_count: Some(7), items }
    }

    fn entry(uti: &str, payload: PasteboardPayload) -> PasteboardEntry {
        PasteboardEntry { uti: uti.into(), payload }
    }

    #[test]
    fn inline_items_can_be_written_back_as_they_were() {
        let s = snapshot(vec![
            PasteboardItem {
                index: 0,
                types: vec!["public.utf8-plain-text".into(), "public.html".into()],
                data: vec![
                    entry("public.utf8-plain-text", PasteboardPayload::Inline(b"hi".to_vec())),
                    entry("public.html", PasteboardPayload::Inline(b"<b>hi</b>".to_vec())),
                ],
            },
            PasteboardItem {
                index: 1,
                types: vec!["public.png".into()],
                data: vec![entry("public.png", PasteboardPayload::Inline(vec![0x89, 0x50]))],
            },
        ]);
        let c = Contents::from_snapshot(&s);
        assert_eq!(c.change_count, Some(7));
        assert_eq!(c.text.as_deref(), Some("hi"));
        let items = c.items.expect("all inline");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].types, ["public.utf8-plain-text", "public.html"]);
        assert_eq!(items[1].data, [("public.png".to_owned(), vec![0x89, 0x50])]);
    }

    #[test]
    fn an_empty_pasteboard_is_written_back_empty() {
        let c = Contents::from_snapshot(&snapshot(vec![]));
        assert_eq!(c.items, Some(vec![]));
        assert_eq!(c.text, None);
    }

    #[test]
    fn promised_or_failed_data_cannot_be_written_back() {
        for payload in [PasteboardPayload::Promised { size: Some(10) }, PasteboardPayload::Error("x".into())] {
            let s = snapshot(vec![PasteboardItem {
                index: 0,
                types: vec!["public.jpeg".into()],
                data: vec![entry("public.jpeg", payload)],
            }]);
            assert!(Contents::from_snapshot(&s).items.is_none());
        }
        // a declared type with no data at all
        let s = snapshot(vec![PasteboardItem {
            index: 0,
            types: vec!["public.utf8-plain-text".into(), "public.rtf".into()],
            data: vec![entry("public.utf8-plain-text", PasteboardPayload::Inline(b"a".to_vec()))],
        }]);
        assert!(Contents::from_snapshot(&s).items.is_none());
    }

    #[test]
    fn set_message_carries_every_item_and_type() {
        let XPCObject::Dictionary(msg) = set_message(&text_items("你好")) else { panic!("not a dictionary") };
        assert!(matches!(&msg["command"], XPCObject::String(s) if s == "SET"));
        assert!(matches!(&msg["pasteboardName"], XPCObject::String(s) if s == "general"));
        // 带 sourceMetadata 才只写本机，不经「通用剪贴板」同步到别的设备
        let XPCObject::Dictionary(source) = &msg["sourceMetadata"] else { panic!("sourceMetadata") };
        assert!(matches!(&source["nonce"], XPCObject::String(n) if n.starts_with("iphone-control-")));
        assert!(matches!(&source["pasteboardName"], XPCObject::String(n) if n == "iphone-control"));
        assert!(matches!(source["changeCount"], XPCObject::Int64(n) if n >= 1));
        let XPCObject::Array(items) = &msg["items"] else { panic!("items") };
        let XPCObject::Dictionary(item) = &items[0] else { panic!("item") };
        let XPCObject::Array(types) = &item["types"] else { panic!("types") };
        assert_eq!(types.len(), 3);
        let XPCObject::Dictionary(data) = &item["data"] else { panic!("data") };
        let XPCObject::Dictionary(utf8) = &data["public.utf8-plain-text"] else { panic!("utf8") };
        assert!(matches!(&utf8["data"], XPCObject::Data(b) if b == "你好".as_bytes()));
    }
}
