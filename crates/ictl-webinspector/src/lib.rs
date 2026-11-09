//! Client for the iOS Web Inspector service (`com.apple.webinspector`, started through lockdown).
//!
//! It is what Safari's Develop menu talks to. The phone needs Settings → Safari → Advanced → Web Inspector on, and
//! a page is only listed when it is inspectable: Safari tabs, and WebViews of apps built for debugging or with
//! `isInspectable` set.
//!
//! The service speaks binary plists (a 4-byte big-endian length, then the plist), each
//! `{__selector, __argument}`. The flow:
//!
//! 1. `_rpc_reportIdentifier:` with our connection ID; the phone answers with the connected applications
//!    (`_rpc_reportConnectedApplicationList:`).
//! 2. `_rpc_forwardGetListing:` per application → `_rpc_applicationSentListing:` with its pages.
//! 3. `_rpc_forwardSocketSetup:` attaches to a page; `_rpc_forwardSocketData:` carries Web Inspector protocol
//!    JSON to it and `_rpc_applicationSentData:` back. Current WebKit wraps that in the `Target` domain
//!    (`Target.sendMessageToTarget` / `Target.dispatchMessageFromTarget`); older versions take commands directly.
//! 4. `_rpc_forwardDidClose:` detaches.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use idevice::{Idevice, IdeviceError, IdeviceService, ReadWrite, provider::IdeviceProvider};
use plist::{Dictionary, Value};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;

/// Messages larger than this are refused (a listing or an evaluation result is far smaller)
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// Writing one message; a phone that stops reading must not hold the caller forever
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Device(#[from] IdeviceError),
    #[error("Web Inspector connection: {0}")]
    Io(#[from] std::io::Error),
    #[error("Web Inspector message: {0}")]
    Plist(#[from] plist::Error),
    #[error("Web Inspector: {0}")]
    Protocol(String),
    /// The page didn't answer in time. The connection stays usable: reads are cancel-safe and late answers for the
    /// closed session are dropped
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    /// The page refused a command (for example an object that went away when the page navigated). The connection
    /// stays usable
    #[error("Web Inspector: {0}")]
    Command(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// An application that hosts inspectable content (Safari, an app, or a WebContent process acting for one)
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub id: String,
    pub name: String,
    pub bundle_id: String,
    /// For a WebContent proxy: the application it renders for
    pub host: Option<String>,
    pub active: bool,
}

/// An inspectable page, WebView or JavaScript context
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub app_id: String,
    pub id: u64,
    /// `WIRTypeWebPage`, `WIRTypeWeb`, `WIRTypeJavaScript`, `WIRTypeServiceWorker`, … without the `WIRType` prefix
    pub kind: String,
    pub title: String,
    pub url: String,
    /// Another debugger (Safari's Web Inspector, for example) is attached
    pub in_use: bool,
}

/// What a JavaScript evaluation returned
#[derive(Debug, Clone, PartialEq)]
pub enum Eval {
    /// The value, serialized to JSON by the page (`returnByValue`); `undefined` becomes `None`
    Value(Option<serde_json::Value>),
    /// The expression threw; the exception's description
    Thrown(String),
}

pub struct WebInspector {
    socket: Box<dyn ReadWrite>,
    connection: String,
    apps: BTreeMap<String, App>,
    listings: HashMap<String, Vec<Page>>,
    /// Inspector protocol messages that arrived for a session, by sender ID
    data: HashMap<String, Vec<serde_json::Value>>,
    /// Senders of the sessions open right now; data for any other sender (a late answer after a timeout) is dropped
    open_sessions: HashSet<String>,
    /// Bytes read but not yet a whole message: reading appends here, so a read cancelled by a timeout loses nothing
    rbuf: Vec<u8>,
    /// Pieces of a message the phone split up (`WIRPartialMessageKey`)
    partial: Vec<u8>,
    reported: bool,
}

impl IdeviceService for WebInspector {
    fn service_name() -> std::borrow::Cow<'static, str> {
        "com.apple.webinspector".into()
    }

    async fn from_stream(idevice: Idevice) -> std::result::Result<Self, IdeviceError> {
        let socket = idevice.get_socket().ok_or(IdeviceError::NoEstablishedConnection)?;
        Ok(Self {
            socket,
            connection: uuid::Uuid::new_v4().to_string().to_uppercase(),
            apps: BTreeMap::new(),
            listings: HashMap::new(),
            data: HashMap::new(),
            open_sessions: HashSet::new(),
            rbuf: Vec::new(),
            partial: Vec::new(),
            reported: false,
        })
    }

    fn get_socket(&mut self) -> Option<&mut Box<dyn ReadWrite>> {
        Some(&mut self.socket)
    }
}

impl WebInspector {
    /// Connects through lockdown and announces this client
    pub async fn open(provider: &dyn IdeviceProvider) -> Result<Self> {
        let mut w = Self::connect(provider).await?;
        w.send("_rpc_reportIdentifier:", Dictionary::new()).await?;
        Ok(w)
    }

    /// Every inspectable page of every application. Waits up to `wait` for the applications to answer; pages of
    /// applications that don't answer in time are left out
    pub async fn pages(&mut self, wait: Duration) -> Result<(Vec<App>, Vec<Page>)> {
        let deadline = Instant::now() + wait;
        // asked again every time: on a kept connection, applications may have come and gone since
        self.reported = false;
        self.send("_rpc_getConnectedApplications:", Dictionary::new()).await?;
        while !self.reported {
            if !self.recv_until(deadline).await? {
                // unlike a slow page, a phone that doesn't report its applications means the connection is stuck
                return Err(Error::Protocol("the phone didn't report its applications in time".into()));
            }
        }
        self.listings.clear();
        let ids: Vec<String> = self.apps.keys().cloned().collect();
        for id in &ids {
            let mut a = Dictionary::new();
            a.insert("WIRApplicationIdentifierKey".into(), id.clone().into());
            self.send("_rpc_forwardGetListing:", a).await?;
        }
        // suspended apps (in the background) don't answer; only wait for the active ones and the WebContent processes
        // working for them
        let awake = |a: &App, apps: &BTreeMap<String, App>| {
            a.active || a.host.as_ref().and_then(|h| apps.get(h)).is_some_and(|h| h.active)
        };
        let waiting: Vec<String> = self.apps.values().filter(|a| awake(a, &self.apps)).map(|a| a.id.clone()).collect();
        while waiting.iter().any(|id| !self.listings.contains_key(id)) {
            if !self.recv_until(deadline).await? {
                break;
            }
        }
        let pages = ids.iter().flat_map(|id| self.listings.get(id).cloned().unwrap_or_default()).collect();
        Ok((self.apps.values().cloned().collect(), pages))
    }

    /// Evaluates `expression` in a page and returns its value. A promise is awaited
    pub async fn evaluate(&mut self, page: &Page, expression: &str, timeout: Duration) -> Result<Eval> {
        let deadline = Instant::now() + timeout;
        let sender = uuid::Uuid::new_v4().to_string().to_uppercase();
        self.open_sessions.insert(sender.clone());
        let mut setup = self.page_args(page);
        setup.insert("WIRSenderKey".into(), sender.clone().into());
        setup.insert("WIRMessageDataTypeChunkSupportedKey".into(), 0.into());
        setup.insert("WIRAutomaticallyPause".into(), false.into());
        self.send("_rpc_forwardSocketSetup:", setup).await?;

        let mut session = Session { sender, target: None, next_id: 1 };
        // current WebKit announces the page target first; older versions take commands directly
        let probe = (Instant::now() + Duration::from_millis(1500)).min(deadline);
        while session.target.is_none() {
            let got = self.take_data(&session.sender);
            for m in got {
                if m["method"] == "Target.targetCreated" && m["params"]["targetInfo"]["type"] == "page" {
                    session.target = m["params"]["targetInfo"]["targetId"].as_str().map(str::to_owned);
                }
            }
            if session.target.is_some() || !self.recv_until(probe).await? {
                break;
            }
        }

        let result = self.run_eval(page, &mut session, expression, deadline).await;
        let mut close = self.page_args(page);
        close.insert("WIRSenderKey".into(), session.sender.clone().into());
        let _ = self.send("_rpc_forwardDidClose:", close).await;
        self.data.remove(&session.sender);
        self.open_sessions.remove(&session.sender);
        result
    }

    async fn run_eval(&mut self, page: &Page, s: &mut Session, expression: &str, deadline: Instant) -> Result<Eval> {
        // Evaluate to a remote object first: with returnByValue WebKit would serialize a promise right away (as {}).
        // Then await a promise, or fetch an object's value; primitives already carry it
        let r = self.command(page, s, "Runtime.evaluate", json!({"expression": expression}), deadline).await?;
        let id = r["result"]["objectId"].clone();
        // WebKit marks promises by class name (Chrome uses subtype "promise"); functions have no JSON value
        let r = if r["wasThrown"] == true || id.is_null() || r["result"]["type"] == "function" {
            r
        } else if r["result"]["className"] == "Promise" || r["result"]["subtype"] == "promise" {
            let p = json!({"promiseObjectId": id, "returnByValue": true});
            self.command(page, s, "Runtime.awaitPromise", p, deadline).await?
        } else {
            let f =
                json!({"objectId": id, "functionDeclaration": "function() { return this; }", "returnByValue": true});
            let v = self.command(page, s, "Runtime.callFunctionOn", f, deadline).await?;
            // a DOM node or similar serializes to {}; its description ("body", "div#app") says more
            let empty = v["result"]["value"].as_object().is_some_and(|o| o.is_empty());
            let plain = r["result"]["className"] == "Object" || r["result"]["subtype"] == "array";
            if v["result"].get("value").is_some() && !(empty && !plain) { v } else { r }
        };
        Ok(eval_result(&r))
    }

    /// Sends one Web Inspector protocol command to the page and waits for its result
    async fn command(
        &mut self,
        page: &Page,
        s: &mut Session,
        method: &str,
        params: serde_json::Value,
        deadline: Instant,
    ) -> Result<serde_json::Value> {
        let id = s.next_id;
        s.next_id += 1;
        let inner = json!({"id": id, "method": method, "params": params});
        let msg = match &s.target {
            Some(t) => json!({"id": id + 1_000_000, "method": "Target.sendMessageToTarget",
                "params": {"targetId": t, "message": inner.to_string()}}),
            None => inner,
        };
        let mut a = self.page_args(page);
        a.insert("WIRSessionIdentifierKey".into(), s.sender.clone().into());
        a.insert("WIRSenderKey".into(), s.sender.clone().into());
        a.insert("WIRSocketDataKey".into(), Value::Data(msg.to_string().into_bytes()));
        self.send("_rpc_forwardSocketData:", a).await?;
        loop {
            for m in self.take_data(&s.sender) {
                if let Some(r) = response_for(&m, id) {
                    return r;
                }
            }
            if !self.recv_until(deadline).await? {
                return Err(Error::Timeout("the page to answer"));
            }
        }
    }

    fn page_args(&self, page: &Page) -> Dictionary {
        let mut a = Dictionary::new();
        a.insert("WIRApplicationIdentifierKey".into(), page.app_id.clone().into());
        a.insert("WIRPageIdentifierKey".into(), page.id.into());
        a
    }

    fn take_data(&mut self, sender: &str) -> Vec<serde_json::Value> {
        self.data.remove(sender).unwrap_or_default()
    }

    async fn send(&mut self, selector: &str, mut args: Dictionary) -> Result<()> {
        args.insert("WIRConnectionIdentifierKey".into(), self.connection.clone().into());
        let mut msg = Dictionary::new();
        msg.insert("__selector".into(), selector.into());
        msg.insert("__argument".into(), Value::Dictionary(args));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &Value::Dictionary(msg))?;
        let mut framed = (body.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&body);
        let write = async {
            self.socket.write_all(&framed).await?;
            self.socket.flush().await
        };
        // a write cut off half-way leaves the stream out of step: report it as a broken connection (Io)
        tokio::time::timeout(SEND_TIMEOUT, write).await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "the phone stopped reading Web Inspector messages")
        })??;
        Ok(())
    }

    /// Reads and handles one message; `false` when `deadline` passed first
    async fn recv_until(&mut self, deadline: Instant) -> Result<bool> {
        let Ok(msg) = tokio::time::timeout_at(deadline, self.read_message()).await else {
            return Ok(false);
        };
        let msg = msg?;
        if let Some(m) = self.unchunk(msg)? {
            self.handle(m);
        }
        Ok(true)
    }

    /// Reads one message. Cancel-safe: bytes go into [`Self::rbuf`] first, and a single `read` call loses nothing when
    /// it is cancelled, so a timeout around this never leaves the stream out of step
    async fn read_message(&mut self) -> Result<Dictionary> {
        loop {
            if let Some(body) = take_frame(&mut self.rbuf)? {
                return Ok(plist::from_bytes(&body)?);
            }
            let mut chunk = [0u8; 64 * 1024];
            let n = self.socket.read(&mut chunk).await?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            self.rbuf.extend_from_slice(&chunk[..n]);
        }
    }

    fn unchunk(&mut self, msg: Dictionary) -> Result<Option<Dictionary>> {
        unchunk_into(&mut self.partial, msg)
    }

    fn handle(&mut self, msg: Dictionary) {
        let selector = msg.get("__selector").and_then(Value::as_string).unwrap_or_default();
        let empty = Dictionary::new();
        let arg = msg.get("__argument").and_then(Value::as_dictionary).unwrap_or(&empty);
        match selector {
            "_rpc_reportConnectedApplicationList:" => {
                self.apps.clear();
                if let Some(list) = arg.get("WIRApplicationDictionaryKey").and_then(Value::as_dictionary) {
                    for v in list.values() {
                        if let Some(a) = v.as_dictionary().and_then(parse_app) {
                            self.apps.insert(a.id.clone(), a);
                        }
                    }
                }
                self.reported = true;
            }
            "_rpc_applicationConnected:" | "_rpc_applicationUpdated:" => {
                if let Some(a) = parse_app(arg) {
                    self.apps.insert(a.id.clone(), a);
                }
            }
            "_rpc_applicationDisconnected:" => {
                if let Some(id) = arg.get("WIRApplicationIdentifierKey").and_then(Value::as_string) {
                    self.apps.remove(id);
                    self.listings.remove(id);
                }
            }
            "_rpc_applicationSentListing:" => {
                if let Some(id) = arg.get("WIRApplicationIdentifierKey").and_then(Value::as_string) {
                    self.listings.insert(id.to_owned(), parse_listing(id, arg));
                }
            }
            "_rpc_applicationSentData:" => {
                let dest = arg.get("WIRDestinationKey").and_then(Value::as_string);
                let data = arg.get("WIRMessageDataKey").and_then(Value::as_data);
                if let (Some(dest), Some(data)) = (dest, data)
                    && self.open_sessions.contains(dest)
                    && let Ok(v) = serde_json::from_slice::<serde_json::Value>(data)
                {
                    self.data.entry(dest.to_owned()).or_default().push(v);
                }
            }
            other => tracing::trace!("webinspector: {other}"),
        }
    }
}

/// One whole frame (4-byte big-endian length, then the body) from the front of `buf`, if it has arrived
fn take_frame(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    let Some(len) = buf.get(..4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize) else {
        return Ok(None);
    };
    if len > MAX_MESSAGE {
        return Err(Error::Protocol(format!("message of {len} bytes")));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let body = buf[4..4 + len].to_vec();
    buf.drain(..4 + len);
    Ok(Some(body))
}

/// Puts split messages back together: parts come as `WIRPartialMessageKey`, the last as `WIRFinalMessageKey`
fn unchunk_into(partial: &mut Vec<u8>, msg: Dictionary) -> Result<Option<Dictionary>> {
    let arg = msg.get("__argument").and_then(Value::as_dictionary);
    if let Some(part) = arg.and_then(|a| a.get("WIRPartialMessageKey")).and_then(Value::as_data) {
        if partial.len() + part.len() > MAX_MESSAGE {
            partial.clear();
            return Err(Error::Protocol(format!("split message over {MAX_MESSAGE} bytes")));
        }
        partial.extend_from_slice(part);
        return Ok(None);
    }
    if let Some(last) = arg.and_then(|a| a.get("WIRFinalMessageKey")).and_then(Value::as_data) {
        partial.extend_from_slice(last);
        let whole = std::mem::take(partial);
        return Ok(Some(plist::from_bytes(&whole)?));
    }
    Ok(Some(msg))
}

struct Session {
    sender: String,
    /// The page target when WebKit uses the `Target` domain
    target: Option<String>,
    next_id: u64,
}

fn parse_app(d: &Dictionary) -> Option<App> {
    let s = |k: &str| d.get(k).and_then(Value::as_string).unwrap_or_default().to_owned();
    let id = s("WIRApplicationIdentifierKey");
    if id.is_empty() {
        return None;
    }
    let proxy = d.get("WIRIsApplicationProxyKey").and_then(Value::as_boolean).unwrap_or(false);
    Some(App {
        name: s("WIRApplicationNameKey"),
        bundle_id: s("WIRApplicationBundleIdentifierKey"),
        host: proxy.then(|| s("WIRHostApplicationIdentifierKey")).filter(|h| !h.is_empty()),
        active: d.get("WIRIsApplicationActiveKey").and_then(Value::as_unsigned_integer).unwrap_or(0) != 0,
        id,
    })
}

fn parse_listing(app_id: &str, arg: &Dictionary) -> Vec<Page> {
    let Some(listing) = arg.get("WIRListingKey").and_then(Value::as_dictionary) else {
        return Vec::new();
    };
    let mut pages: Vec<Page> = listing
        .values()
        .filter_map(Value::as_dictionary)
        .filter_map(|p| {
            let s = |k: &str| p.get(k).and_then(Value::as_string).unwrap_or_default().to_owned();
            Some(Page {
                app_id: app_id.to_owned(),
                id: p.get("WIRPageIdentifierKey")?.as_unsigned_integer()?,
                kind: s("WIRTypeKey").trim_start_matches("WIRType").to_owned(),
                title: s("WIRTitleKey"),
                url: s("WIRURLKey"),
                in_use: p.get("WIRConnectionIdentifierKey").is_some(),
            })
        })
        .collect();
    pages.sort_by_key(|p| p.id);
    pages
}

/// The result of command `id` in an inspector message, unwrapping `Target.dispatchMessageFromTarget`
fn response_for(m: &serde_json::Value, id: u64) -> Option<Result<serde_json::Value>> {
    let inner;
    let m = if m["method"] == "Target.dispatchMessageFromTarget" {
        inner = serde_json::from_str::<serde_json::Value>(m["params"]["message"].as_str()?).ok()?;
        &inner
    } else {
        m
    };
    if m["id"].as_u64() != Some(id) {
        return None;
    }
    Some(match m.get("error") {
        Some(e) => Err(Error::Command(e["message"].as_str().unwrap_or("command failed").to_owned())),
        None => Ok(m["result"].clone()),
    })
}

fn eval_result(r: &serde_json::Value) -> Eval {
    let v = &r["result"];
    if r["wasThrown"] == true {
        return Eval::Thrown(v["description"].as_str().unwrap_or("exception").to_owned());
    }
    if v["type"] == "undefined" {
        return Eval::Value(None);
    }
    match v.get("value") {
        Some(x) => Eval::Value(Some(x.clone())),
        // functions, symbols and objects that can't be serialized come back without a value
        None => Eval::Value(v.get("description").cloned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(v: serde_json::Value) -> Dictionary {
        plist::to_value(&v).unwrap().into_dictionary().unwrap()
    }

    /// Frames come out whole however the bytes arrive, so a read cut off by a timeout loses nothing
    #[test]
    fn frames_survive_partial_reads() {
        let frame = |body: &[u8]| [(body.len() as u32).to_be_bytes().to_vec(), body.to_vec()].concat();
        let stream = [frame(b"hello"), frame(b""), frame(b"world!")].concat();
        let mut buf = Vec::new();
        let mut out = Vec::new();
        for byte in stream {
            buf.push(byte);
            while let Some(f) = take_frame(&mut buf).unwrap() {
                out.push(f);
            }
        }
        assert_eq!(out, [b"hello".to_vec(), Vec::new(), b"world!".to_vec()]);
        assert!(buf.is_empty());
        let mut huge = (u32::MAX).to_be_bytes().to_vec();
        assert!(take_frame(&mut huge).is_err());
    }

    #[test]
    fn split_messages_are_capped() {
        let part = |n: usize| dict(json!({"__argument": {}})).tap_part(n);
        let mut partial = vec![0u8; MAX_MESSAGE - 10];
        assert!(unchunk_into(&mut partial, part(20)).is_err());
        assert!(partial.is_empty(), "the pieces are dropped");
        assert!(unchunk_into(&mut partial, part(20)).unwrap().is_none(), "a new message can start");
    }

    trait TapPart {
        fn tap_part(self, n: usize) -> Dictionary;
    }
    impl TapPart for Dictionary {
        fn tap_part(mut self, n: usize) -> Dictionary {
            let mut a = Dictionary::new();
            a.insert("WIRPartialMessageKey".into(), Value::Data(vec![0; n]));
            self.insert("__argument".into(), Value::Dictionary(a));
            self
        }
    }

    #[test]
    fn parses_apps_and_listing() {
        let app = dict(json!({"WIRApplicationIdentifierKey": "PID:812", "WIRApplicationNameKey": "WebContent",
            "WIRApplicationBundleIdentifierKey": "com.apple.WebKit.WebContent", "WIRIsApplicationProxyKey": true,
            "WIRHostApplicationIdentifierKey": "PID:640", "WIRIsApplicationActiveKey": 1}));
        let a = parse_app(&app).unwrap();
        assert_eq!((a.id.as_str(), a.host.as_deref(), a.active), ("PID:812", Some("PID:640"), true));

        let mut arg = dict(json!({"WIRApplicationIdentifierKey": "PID:812"}));
        let listing = dict(json!({
            "2": {"WIRPageIdentifierKey": 2, "WIRTypeKey": "WIRTypeWebPage", "WIRTitleKey": "B", "WIRURLKey": "https://b"},
            "1": {"WIRPageIdentifierKey": 1, "WIRTypeKey": "WIRTypeWebPage", "WIRTitleKey": "A", "WIRURLKey": "capacitor://localhost",
                  "WIRConnectionIdentifierKey": "OTHER"}
        }));
        arg.insert("WIRListingKey".into(), Value::Dictionary(listing));
        let pages = parse_listing("PID:812", &arg);
        assert_eq!(pages.len(), 2);
        assert_eq!((pages[0].id, pages[0].kind.as_str(), pages[0].in_use), (1, "WebPage", true));
        assert_eq!(pages[1].url, "https://b");
    }

    #[test]
    fn unwraps_target_messages() {
        let wrapped = json!({"method": "Target.dispatchMessageFromTarget",
            "params": {"targetId": "page-1", "message": "{\"id\":3,\"result\":{\"result\":{\"type\":\"number\",\"value\":2}}}"}});
        let r = response_for(&wrapped, 3).unwrap().unwrap();
        assert_eq!(eval_result(&r), Eval::Value(Some(json!(2))));
        assert!(response_for(&wrapped, 4).is_none());
        let direct = json!({"id": 1, "error": {"message": "no such method"}});
        assert!(matches!(response_for(&direct, 1), Some(Err(Error::Command(m))) if m == "no such method"));
    }

    #[test]
    fn evaluation_results() {
        assert_eq!(eval_result(&json!({"result": {"type": "undefined"}})), Eval::Value(None));
        assert_eq!(
            eval_result(&json!({"result": {"type": "object", "value": {"a": [1]}}})),
            Eval::Value(Some(json!({"a": [1]})))
        );
        assert_eq!(
            eval_result(&json!({"result": {"type": "object", "description": "ReferenceError: x"}, "wasThrown": true})),
            Eval::Thrown("ReferenceError: x".into())
        );
    }

    #[test]
    fn split_messages_are_joined() {
        let mut whole = Vec::new();
        let msg = dict(json!({"__selector": "_rpc_reportSetup:", "__argument": {}}));
        plist::to_writer_binary(&mut whole, &Value::Dictionary(msg.clone())).unwrap();
        let (a, b) = whole.split_at(whole.len() / 2);
        let wrap = |k: &str, d: &[u8]| {
            let mut arg = Dictionary::new();
            arg.insert(k.into(), Value::Data(d.to_vec()));
            let mut m = Dictionary::new();
            m.insert("__argument".into(), Value::Dictionary(arg));
            m
        };
        let mut partial = Vec::new();
        assert!(unchunk_into(&mut partial, wrap("WIRPartialMessageKey", a)).unwrap().is_none());
        assert_eq!(unchunk_into(&mut partial, wrap("WIRFinalMessageKey", b)).unwrap(), Some(msg));
        assert!(partial.is_empty());
    }
}
