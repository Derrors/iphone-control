//! A remoteAXService session: the handshake Xcode 27's Device Hub does, the trees the phone pushes after every
//! change, and requests (one at a time).
//!
//! The handshake matters: the phone asks the host `hostAPIVersion` and wants every message acknowledged. A host
//! that doesn't answer still gets a first tree but no more pushes once WebDriverAgent isn't running (our rounds 1
//! and 2, `docs/dev/ios27-native.md`). Answering as Xcode does (7), acknowledging, and waiting for
//! `deviceAPIVersion` before `clientNeedsAccessibility:` is what Pinky does (DIYgod/pinky `rax.go`, Apache-2.0).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plist::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{oneshot, watch};

use crate::archive::{self, Obj, ObjectRef, Param};
use crate::dtx::{self, Frame};
use crate::tree::{self, Element, Pushed, attr};
use crate::{AxError, Result};

/// What Xcode 27's AXAuditRemoteDevice answers when the phone asks `hostAPIVersion`
const HOST_API_VERSION: u64 = 7;
/// The cached-tree client type Xcode uses
const CLIENT_TYPE: i64 = 2;
/// One request's answer takes 5–60 ms over USB
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Request types: the cases of `-[AXPTranslator processTranslatorRequest:]`
pub mod request {
    /// An application's element by process (parameter `pid`)
    pub const APP_OBJECT: i64 = 1;
    pub const ATTRIBUTE: i64 = 2;
    pub const FRONTMOST_APP: i64 = 4;
    /// Several attributes (parameter `attributes`)
    pub const ATTRIBUTES: i64 = 5;
    pub const TREE_DUMP: i64 = 11;
}

/// An `AXPTranslatorResponse`
#[derive(Debug, Clone)]
pub struct Response {
    pub request_type: i64,
    pub notification: i64,
    pub error: i64,
    pub object: Option<ObjectRef>,
    pub result: Obj,
}

impl Response {
    fn from_obj(o: &Obj) -> Option<Self> {
        if o.class() != Some("AXPTranslatorResponse") {
            return None;
        }
        let int = |k| o.field(k).and_then(Obj::as_int).unwrap_or(0) as i64;
        Some(Self {
            request_type: int("associatedRequestType"),
            notification: int("notification"),
            error: int("error"),
            object: o.field("associatedTranslationObject").and_then(tree::object_ref),
            result: o.field("resultData").cloned().unwrap_or(Obj::Null),
        })
    }
}

/// A pushed tree and when it came
#[derive(Debug, Clone)]
pub struct PushedAt {
    pub tree: Pushed,
    pub at: Instant,
}

type Waiter = Arc<Mutex<Option<oneshot::Sender<Response>>>>;

pub struct Session<S> {
    writer: Arc<tokio::sync::Mutex<WriteHalf<S>>>,
    waiter: Waiter,
    pushes: watch::Receiver<Option<Arc<PushedAt>>>,
    /// When the phone last said the screen is changing (a notification before the next tree)
    changes: watch::Receiver<Option<Instant>>,
    closed: watch::Receiver<bool>,
    requests: tokio::sync::Mutex<()>,
    next_id: AtomicU32,
    reader: tokio::task::JoinHandle<()>,
}

impl<S> Drop for Session<S> {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> Result<()> {
    w.write_all(bytes).await?;
    w.flush().await?;
    Ok(())
}

/// The reply a message that wants one gets: `hostAPIVersion` is answered, anything else acknowledged
fn answer(frame: &Frame) -> Option<Vec<u8>> {
    if !frame.expects_reply {
        return None;
    }
    let value =
        (frame.selector().as_deref() == Some("hostAPIVersion")).then(|| Value::Integer(HOST_API_VERSION.into()));
    Some(dtx::reply(frame, value))
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> Session<S> {
    /// Handshake on a fresh connection to the service, within `wait`
    pub async fn connect(stream: S, wait: Duration) -> Result<Self> {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let capabilities = archive::encode_int_dict(&[
            ("com.apple.private.DTXBlockCompression", 0),
            ("com.apple.private.DTXConnection", 1),
        ]);
        send(&mut writer, &dtx::invocation(1, "_notifyOfPublishedCapabilities:", vec![capabilities], false)).await?;
        send(&mut writer, &dtx::invocation(2, "deviceAPIVersion", Vec::new(), true)).await?;
        tokio::time::timeout(wait, async {
            loop {
                let f = dtx::read_frame(&mut reader).await?;
                if let Some(b) = answer(&f) {
                    send(&mut writer, &b).await?;
                }
                if f.identifier == 2 && f.conversation % 2 == 1 {
                    return Ok::<_, AxError>(());
                }
            }
        })
        .await
        .map_err(|_| AxError::Timeout("deviceAPIVersion"))??;
        // The translator answers only a client that needs accessibility; it then pushes the screen's tree after
        // every change
        let yes = archive::encode_scalar(Value::Boolean(true));
        send(&mut writer, &dtx::invocation(3, "clientNeedsAccessibility:", vec![yes], false)).await?;

        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        let waiter: Waiter = Arc::default();
        let (push_tx, pushes) = watch::channel(None);
        let (change_tx, changes) = watch::channel(None);
        let (closed_tx, closed) = watch::channel(false);
        let reader = tokio::spawn(read_loop(reader, writer.clone(), waiter.clone(), push_tx, change_tx, closed_tx));
        Ok(Self {
            writer,
            waiter,
            pushes,
            changes,
            closed,
            requests: tokio::sync::Mutex::new(()),
            next_id: AtomicU32::new(4),
            reader,
        })
    }
}

async fn read_loop<S: AsyncRead + AsyncWrite + Unpin + Send>(
    mut reader: ReadHalf<S>,
    writer: Arc<tokio::sync::Mutex<WriteHalf<S>>>,
    waiter: Waiter,
    pushes: watch::Sender<Option<Arc<PushedAt>>>,
    changes: watch::Sender<Option<Instant>>,
    closed: watch::Sender<bool>,
) {
    loop {
        let f = match dtx::read_frame(&mut reader).await {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!("remoteAXService closed: {e}");
                break;
            }
        };
        if let Some(b) = answer(&f)
            && send(&mut *writer.lock().await, &b).await.is_err()
        {
            break;
        }
        if f.selector().as_deref() != Some("processDataFromRemoteDevice:") {
            continue;
        }
        // an NSData holding the archived response
        let Some(Obj::Data(inner)) = f.first_arg() else { continue };
        let Some(r) = archive::decode(&inner).ok().as_ref().and_then(Response::from_obj) else { continue };
        tracing::debug!(
            "remoteAXService: response to request type {}, notification {}, error {}, result {}",
            r.request_type,
            r.notification,
            r.error,
            describe(&r.result)
        );
        if let Some(dump) = r.result.field("treeDump").and_then(Obj::as_array) {
            let kind = r.result.field("treeDumpType").and_then(Obj::as_str).unwrap_or("");
            if let Some(tree) = tree::from_dump(dump, kind) {
                pushes.send_replace(Some(Arc::new(PushedAt { tree, at: Instant::now() })));
            }
            continue;
        }
        if r.notification != 0 || r.request_type == request::TREE_DUMP {
            changes.send_replace(Some(Instant::now()));
            continue;
        }
        if let Some(w) = waiter.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = w.send(r);
        }
    }
    waiter.lock().unwrap_or_else(|p| p.into_inner()).take();
    closed.send_replace(true);
}

/// A result's shape for the debug log: its class and field names (no screen text)
fn describe(o: &Obj) -> String {
    match o {
        Obj::Object { class, fields } => {
            format!("{class}{{{}}}", fields.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(","))
        }
        Obj::Array(a) => format!("[{} items]", a.len()),
        Obj::Dict(d) => format!("{{{} entries}}", d.len()),
        Obj::Null => "null".into(),
        _ => "scalar".into(),
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> Session<S> {
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// The last tree the phone pushed
    pub fn latest(&self) -> Option<Arc<PushedAt>> {
        self.pushes.borrow().clone()
    }

    /// When the phone last said the screen is changing
    pub fn last_change(&self) -> Option<Instant> {
        *self.changes.borrow()
    }

    /// Waits up to `timeout` for a tree pushed after `since`, preferring a complete one: an initial tree is
    /// returned only when no complete one follows within `timeout`
    pub async fn wait_push(&self, since: Instant, timeout: Duration) -> Option<Arc<PushedAt>> {
        let mut rx = self.pushes.clone();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let cur = rx.borrow_and_update().clone();
            if let Some(p) = &cur
                && p.at > since
                && !p.tree.initial
            {
                return cur;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                _ => return cur.filter(|p| p.at > since),
            }
        }
    }

    /// One request, answered within [`REQUEST_TIMEOUT`]
    pub async fn request(
        &self,
        request_type: i64,
        object: Option<ObjectRef>,
        params: &[(&str, Param)],
    ) -> Result<Response> {
        if self.is_closed() {
            return Err(AxError::Closed);
        }
        let _one = self.requests.lock().await;
        let (tx, rx) = oneshot::channel();
        *self.waiter.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
        let body = archive::encode_data(&archive::encode_request(request_type, 0, object, params, CLIENT_TYPE));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        send(&mut *self.writer.lock().await, &dtx::invocation(id, "processDataFromHost:", vec![body], false)).await?;
        let r = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => return Err(AxError::Closed),
            Err(_) => {
                self.waiter.lock().unwrap_or_else(|p| p.into_inner()).take();
                return Err(AxError::Timeout("request"));
            }
        };
        if r.request_type != request_type {
            return Err(AxError::Protocol(format!("answer to request type {}, asked {request_type}", r.request_type)));
        }
        if r.error != 0 {
            return Err(AxError::Protocol(format!("request type {request_type} failed: error {}", r.error)));
        }
        Ok(r)
    }

    /// The frontmost application's element
    pub async fn frontmost_app(&self) -> Result<ObjectRef> {
        let r = self.request(request::FRONTMOST_APP, None, &[]).await?;
        tree::object_ref(&r.result).ok_or_else(|| AxError::Protocol("no frontmost app".into()))
    }

    /// The application element of process `pid`, in front or not
    pub async fn app_object(&self, pid: i64) -> Result<ObjectRef> {
        let r = self.request(request::APP_OBJECT, None, &[("pid", Param::Int(pid))]).await?;
        tree::object_ref(&r.result).ok_or_else(|| AxError::Protocol(format!("no application element for pid {pid}")))
    }

    /// The attributes [`attr::WALK`] of one element
    pub async fn attributes(&self, object: ObjectRef) -> Result<Obj> {
        let list = Param::Ints(attr::WALK.to_vec());
        Ok(self.request(request::ATTRIBUTES, Some(object), &[("attributes", list)]).await?.result)
    }

    /// `root` and the elements under it, depth first in reading order, each with [`attr::WALK`] (one request each, a
    /// few milliseconds over USB): at most `limit` of them, within `budget`. Elements with any of the traits `prune`
    /// are listed, but not walked into. Also says whether the walk got to the end.
    ///
    /// This is how the screen is read: the phone pushes a tree by itself only when the frontmost app changes (and on
    /// connecting), not when an app changes its own screen (iPhone 15 Pro Max, iOS 27.2.1, 2026-11-15; Pinky reads
    /// the same way, DIYgod/pinky `rax.go`). Asking for a tree dump (request type 11) closes the connection
    pub async fn walk(
        &self,
        root: ObjectRef,
        limit: usize,
        budget: Duration,
        prune: u64,
    ) -> Result<(Vec<Element>, bool)> {
        let deadline = Instant::now() + budget;
        let mut stack = vec![(root, None, 0usize)];
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        while let Some((o, parent, depth)) = stack.pop() {
            if out.len() >= limit || Instant::now() >= deadline {
                return Ok((out, false));
            }
            if depth > 40 || !seen.insert((o.pid, o.id)) {
                continue;
            }
            let a = self.attributes(o).await?;
            let (e, kids) = tree::element(Some(o), &a, parent, depth);
            let walk_into = depth == 0 || e.traits & prune == 0;
            out.push(e);
            let idx = out.len() - 1;
            for k in kids.into_iter().rev().filter(|_| walk_into) {
                stack.push((k, Some(idx), depth + 1));
            }
        }
        Ok((out, true))
    }

    /// The elements under `root` as [`Self::walk`] reads them, without the root: the `parent` indexes point into the
    /// returned list (None for the root's children)
    pub async fn walk_children(
        &self,
        root: ObjectRef,
        limit: usize,
        budget: Duration,
        prune: u64,
    ) -> Result<Vec<Element>> {
        let (mut els, _) = self.walk(root, limit + 1, budget, prune).await?;
        if !els.is_empty() {
            els.remove(0);
        }
        for e in &mut els {
            e.parent = e.parent.and_then(|p| p.checked_sub(1));
        }
        Ok(els)
    }

    /// Tells the phone this client no longer needs accessibility (it then closes the connection)
    pub async fn close(&self) {
        let no = archive::encode_scalar(Value::Boolean(false));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = dtx::invocation(id, "clientNeedsAccessibility:", vec![no], false);
        let _ =
            tokio::time::timeout(Duration::from_secs(1), async { send(&mut *self.writer.lock().await, &msg).await })
                .await;
        self.reader.abort();
    }
}
