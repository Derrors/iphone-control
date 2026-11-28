//! A remoteAXService session: the handshake Xcode 27's Device Hub does, the trees the phone pushes after every
//! change, and requests (one at a time).
//!
//! The handshake matters: the phone asks the host `hostAPIVersion` and wants every message acknowledged. A host
//! that doesn't answer still gets a first tree but no more pushes once WebDriverAgent isn't running (our rounds 1
//! and 2, `docs/dev/ios27-native.md`). Answering as Xcode does (7), acknowledging, and waiting for
//! `deviceAPIVersion` before `clientNeedsAccessibility:` keeps it answering without WDA (round 3).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plist::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, watch};

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

/// Why [`Session::walk_report`] stopped before visiting the whole tree
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkCutoff {
    /// Reached the element count limit
    ElementLimit,
    /// Reached the time budget
    Timeout,
    /// Reached the max nesting depth
    DepthLimit,
    /// One or more live elements could not be read
    Unreadable,
}

/// A pushed tree and when it came
#[derive(Debug, Clone)]
pub struct PushedAt {
    pub tree: Pushed,
    pub at: Instant,
}

/// Where answers go while requests are out (one caller at a time, [`Session::requests`])
type Waiter = Arc<Mutex<Option<mpsc::UnboundedSender<Response>>>>;
/// Attribute requests sent at once in a walk: answers carry the element they are for, so they need no waiting one by
/// one (a walk of 57 elements took 2.5 s one at a time over USB)
const BATCH: usize = 64;

pub struct Session<S> {
    writer: Arc<tokio::sync::Mutex<WriteHalf<S>>>,
    waiter: Waiter,
    pushes: watch::Receiver<Option<Arc<PushedAt>>>,
    /// When the phone last said the screen is changing (a notification before the next tree)
    changes: watch::Receiver<Option<Instant>>,
    closed: watch::Receiver<bool>,
    /// A request stopped halfway (cancelled, or its write failed): what is on the stream is no longer known (half a
    /// message written, an answer still to come for nobody), so the session counts as closed and is opened again
    abandoned: AtomicBool,
    requests: tokio::sync::Mutex<()>,
    next_id: AtomicU32,
    reader: tokio::task::JoinHandle<()>,
}

impl<S> Drop for Session<S> {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// One request on the stream: unless [`InFlight::done`] is called, dropping it (the request was cancelled, or failed
/// writing) abandons the session ([`Session::is_closed`]) and stops its reader, which would otherwise go on answering
/// the phone on a stream that may hold half a message; the answer channel is let go either way
struct InFlight<'a> {
    abandoned: &'a AtomicBool,
    waiter: &'a Waiter,
    reader: &'a tokio::task::JoinHandle<()>,
    done: bool,
}

impl InFlight<'_> {
    fn done(mut self) {
        self.done = true;
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.abandoned.store(true, Ordering::SeqCst);
            self.reader.abort();
        }
        self.waiter.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
}

/// Writing one request, at most this long: a stream that takes no more means the phone or the link stopped
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A write that didn't finish in time: the link, not the service (an unanswered service is [`AxError::Timeout`],
/// which the daemon restarts it for)
fn write_stalled() -> AxError {
    AxError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "writing a request took too long"))
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
            abandoned: AtomicBool::new(false),
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
        // a reply that can't be written in time: the link stopped, the session is done
        if let Some(b) = answer(&f)
            && !matches!(
                tokio::time::timeout(WRITE_TIMEOUT, async { send(&mut *writer.lock().await, &b).await }).await,
                Ok(Ok(()))
            )
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
        if let Some(w) = waiter.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
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
    /// Registers for the next answers, once the stream is known to be open (checked after registering: a stream
    /// that closed in between would otherwise leave the request waiting out its timeout)
    fn start(&self) -> Result<(mpsc::UnboundedReceiver<Response>, InFlight<'_>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.waiter.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
        let flight = InFlight { abandoned: &self.abandoned, waiter: &self.waiter, reader: &self.reader, done: false };
        if self.is_closed() {
            flight.done();
            return Err(AxError::Closed);
        }
        Ok((rx, flight))
    }

    pub fn is_closed(&self) -> bool {
        *self.closed.borrow() || self.abandoned.load(Ordering::SeqCst)
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

    /// One request, answered within `REQUEST_TIMEOUT`. An answer to another request (one that came too late
    /// for it) is passed over
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
        let (mut rx, flight) = self.start()?;
        let mut closed = self.closed.clone();
        let r = async {
            let body = archive::encode_data(&archive::encode_request(request_type, 0, object, params, CLIENT_TYPE));
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let msg = dtx::invocation(id, "processDataFromHost:", vec![body], false);
            match tokio::time::timeout(WRITE_TIMEOUT, async { send(&mut *self.writer.lock().await, &msg).await }).await
            {
                Ok(r) => r?,
                Err(_) => return Err(write_stalled()),
            }
            let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
            loop {
                let r = tokio::select! {
                    r = tokio::time::timeout_at(deadline, rx.recv()) => match r {
                        Ok(Some(r)) => r,
                        Ok(None) => return Err(AxError::Closed),
                        Err(_) => return Ok(Err(AxError::Timeout("request"))),
                    },
                    _ = closed.wait_for(|c| *c) => return Err(AxError::Closed),
                };
                let same_object = object.is_none_or(|o| r.object.is_none_or(|a| (a.pid, a.id) == (o.pid, o.id)));
                if r.request_type == request_type && same_object {
                    return Ok(Ok(r));
                }
                tracing::debug!("remoteAXService: passed over a late answer to request type {}", r.request_type);
            }
        }
        .await;
        // written whole: an unanswered request (a timeout) leaves the stream as it was; a late answer is passed over
        let r = match r {
            Ok(r) => {
                flight.done();
                r
            }
            Err(e) => return Err(e),
        };
        let r = r?;
        if r.error != 0 {
            return Err(AxError::Protocol(format!("request type {request_type} failed: error {}", r.error)));
        }
        Ok(r)
    }

    /// The attributes [`attr::WALK`] of several elements, asked for at once: by element, those answered (an element
    /// that failed, or gone meanwhile, is left out)
    pub async fn attributes_of(&self, objects: &[ObjectRef]) -> Result<HashMap<ObjectRef, Obj>> {
        if self.is_closed() {
            return Err(AxError::Closed);
        }
        let mut out = HashMap::new();
        if objects.is_empty() {
            return Ok(out);
        }
        let _one = self.requests.lock().await;
        let (mut rx, flight) = self.start()?;
        let mut closed = self.closed.clone();
        let r = async {
            let list = Param::Ints(attr::WALK.to_vec());
            let write = async {
                let mut w = self.writer.lock().await;
                for o in objects {
                    let req = archive::encode_request(
                        request::ATTRIBUTES,
                        0,
                        Some(*o),
                        &[("attributes", list.clone())],
                        CLIENT_TYPE,
                    );
                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                    let msg = dtx::invocation(id, "processDataFromHost:", vec![archive::encode_data(&req)], false);
                    w.write_all(&msg).await?;
                }
                w.flush().await?;
                Ok::<_, AxError>(())
            };
            let many = WRITE_TIMEOUT + Duration::from_millis(5) * objects.len() as u32;
            match tokio::time::timeout(many, write).await {
                Ok(r) => r?,
                Err(_) => return Err(write_stalled()),
            }
            let mut pending: Vec<ObjectRef> = objects.to_vec();
            let deadline =
                tokio::time::Instant::now() + REQUEST_TIMEOUT + Duration::from_millis(20) * objects.len() as u32;
            while !pending.is_empty() {
                let r = tokio::select! {
                    r = tokio::time::timeout_at(deadline, rx.recv()) => match r {
                        Ok(Some(r)) => r,
                        Ok(None) => return Err(AxError::Closed),
                        Err(_) => return Ok(Err(AxError::Timeout("attributes"))),
                    },
                    _ = closed.wait_for(|c| *c) => return Err(AxError::Closed),
                };
                if r.request_type != request::ATTRIBUTES {
                    continue;
                }
                // the element it answers for; without one, the oldest still waiting (answers come in order)
                let at = match r.object {
                    Some(a) => pending.iter().position(|o| (o.pid, o.id) == (a.pid, a.id)),
                    None => Some(0),
                };
                let Some(at) = at else { continue };
                let o = pending.remove(at);
                if r.error == 0 {
                    out.insert(o, r.result);
                }
            }
            Ok(Ok(()))
        }
        .await;
        match r {
            Ok(r) => {
                flight.done();
                r.map(|()| out)
            }
            Err(e) => Err(e),
        }
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

    /// `root` and the elements under it, depth first in reading order, each with [`attr::WALK`]: at most `limit` of
    /// them, within `budget`. Each level's elements are asked for at once ([`Self::attributes_of`]). Elements with
    /// any of the traits `prune` are listed, but not walked into. Also says whether the walk got to the end.
    ///
    /// This is how the screen is read: the phone pushes a tree by itself only when the frontmost app changes (and on
    /// connecting), not when an app changes its own screen (iPhone 15 Pro Max, iOS 27.2.1, 2026-11-15).
    /// Asking for a tree dump (request type 11) closes the connection
    pub async fn walk(
        &self,
        root: ObjectRef,
        limit: usize,
        budget: Duration,
        prune: u64,
    ) -> Result<(Vec<Element>, bool)> {
        let (els, cutoff) = self.walk_report(root, limit, budget, prune).await?;
        Ok((els, cutoff.is_none()))
    }

    /// Same as [`Self::walk`], returning the specific [`WalkCutoff`] reason when the walk stopped early
    pub async fn walk_report(
        &self,
        root: ObjectRef,
        limit: usize,
        budget: Duration,
        prune: u64,
    ) -> Result<(Vec<Element>, Option<WalkCutoff>)> {
        let deadline = Instant::now() + budget;
        let mut attrs: HashMap<ObjectRef, Obj> = HashMap::new();
        let mut seen = HashSet::from([(root.pid, root.id)]);
        let mut frontier = VecDeque::from([(root, 0, None)]);
        let mut missing = Vec::new();
        let mut cutoff = None;
        while !frontier.is_empty() {
            if attrs.len() >= limit {
                cutoff = Some(WalkCutoff::ElementLimit);
                break;
            }
            if Instant::now() >= deadline {
                cutoff = Some(WalkCutoff::Timeout);
                break;
            }
            let n = frontier.len().min(BATCH).min(limit - attrs.len());
            let chunk: Vec<_> = frontier.drain(..n).collect();
            let objects: Vec<ObjectRef> = chunk.iter().map(|(o, _, _)| *o).collect();
            let got = self.attributes_of(&objects).await?;
            for (o, depth, parent) in chunk {
                let Some(a) = got.get(&o) else {
                    if let Some(parent) = parent {
                        missing.push((parent, o));
                    } else {
                        cutoff.get_or_insert(WalkCutoff::Unreadable);
                    }
                    continue;
                };
                let (e, kids) = tree::element(Some(o), a, None, depth);
                if depth > 0 && e.traits & prune != 0 {
                    continue;
                }
                if depth >= 40 {
                    if !kids.is_empty() {
                        cutoff.get_or_insert(WalkCutoff::DepthLimit);
                    }
                    continue;
                }
                for k in kids {
                    if seen.insert((k.pid, k.id)) {
                        frontier.push_back((k, depth + 1, Some(o)));
                    }
                }
            }
            attrs.extend(got);
        }
        // An attribute error alone cannot distinguish a removed child from an unreadable live one.
        // Re-read each affected parent once, only when the rest of the walk completed within budget.
        if cutoff.is_none() && !missing.is_empty() {
            if Instant::now() < deadline {
                let parents: HashSet<_> = missing.iter().map(|(parent, _)| *parent).collect();
                let parents: Vec<_> = parents.into_iter().collect();
                let fresh = self.attributes_of(&parents).await?;
                let all_gone = missing.iter().all(|(parent, child)| {
                    fresh.get(parent).is_some_and(|a| {
                        let (_, kids) = tree::element(Some(*parent), a, None, 0);
                        !kids.contains(child) && kids.iter().all(|k| seen.contains(&(k.pid, k.id)))
                    })
                });
                if !all_gone {
                    cutoff = Some(WalkCutoff::Unreadable);
                }
            } else {
                cutoff = Some(WalkCutoff::Timeout);
            }
        }
        Ok((tree::assemble(root, &attrs), cutoff))
    }

    /// The elements under `root` as [`Self::walk`] reads them, without the root: the `parent` indexes point into the
    /// returned list (None for the root's children)
    pub async fn walk_children(
        &self,
        root: ObjectRef,
        limit: usize,
        budget: Duration,
        prune: u64,
    ) -> Result<(Vec<Element>, bool)> {
        let (els, cutoff) = self.walk_children_report(root, limit, budget, prune).await?;
        Ok((els, cutoff.is_none()))
    }

    /// Same as [`Self::walk_children`], returning the specific [`WalkCutoff`] reason when the walk stopped early
    pub async fn walk_children_report(
        &self,
        root: ObjectRef,
        limit: usize,
        budget: Duration,
        prune: u64,
    ) -> Result<(Vec<Element>, Option<WalkCutoff>)> {
        let (mut els, cutoff) = self.walk_report(root, limit + 1, budget, prune).await?;
        if !els.is_empty() {
            els.remove(0);
        }
        for e in &mut els {
            e.parent = e.parent.and_then(|p| p.checked_sub(1));
        }
        Ok((els, cutoff))
    }

    /// Tells the phone this client no longer needs accessibility (it then closes the connection)
    pub async fn close(&self) {
        // half a request may be on the stream: nothing more is written, the connection just goes
        if self.abandoned.load(Ordering::SeqCst) {
            self.reader.abort();
            return;
        }
        let no = archive::encode_scalar(Value::Boolean(false));
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = dtx::invocation(id, "clientNeedsAccessibility:", vec![no], false);
        let _ =
            tokio::time::timeout(Duration::from_secs(1), async { send(&mut *self.writer.lock().await, &msg).await })
                .await;
        self.reader.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A phone that answers the handshake and then reads everything without answering, until `hang_up` fires
    async fn phone(hang_up: tokio::sync::oneshot::Receiver<()>) -> Session<tokio::io::DuplexStream> {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(theirs);
            let serve = async {
                while let Ok(f) = dtx::read_frame(&mut r).await {
                    if f.selector().as_deref() == Some("deviceAPIVersion") {
                        let _ = send(&mut w, &dtx::reply(&f, Some(Value::Integer(7u64.into())))).await;
                    }
                }
            };
            tokio::select! {
                () = serve => {}
                _ = hang_up => {}
            }
        });
        Session::connect(ours, Duration::from_secs(2)).await.unwrap()
    }

    /// A request dropped halfway (a background check that gave way to an action) leaves the stream in an unknown
    /// state: the session counts as closed, so the next request opens a new one instead of reading a stale answer
    #[tokio::test]
    async fn a_cancelled_request_abandons_the_session() {
        let (_keep, rx) = tokio::sync::oneshot::channel();
        let s = phone(rx).await;
        assert!(!s.is_closed());
        let cancelled = tokio::time::timeout(Duration::from_millis(50), s.request(request::FRONTMOST_APP, None, &[]));
        assert!(cancelled.await.is_err());
        assert!(s.is_closed(), "abandoned");
        assert!(matches!(s.request(request::FRONTMOST_APP, None, &[]).await, Err(AxError::Closed)));
        // nothing reads or answers on that stream any more
        tokio::time::timeout(Duration::from_secs(1), async {
            while !s.reader.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the reader stopped");
    }

    /// Not answered in time (the service is stuck, which the daemon may restart it for): the request was written
    /// whole, so the session stays usable
    #[tokio::test]
    async fn an_unanswered_request_keeps_the_session() {
        let (_keep, rx) = tokio::sync::oneshot::channel();
        let s = phone(rx).await;
        assert!(matches!(s.request(request::FRONTMOST_APP, None, &[]).await, Err(AxError::Timeout(_))));
        assert!(!s.is_closed());
    }

    /// The phone hangs up while a request waits: it ends at once, not when its time is up
    #[tokio::test]
    async fn a_closed_connection_ends_the_wait() {
        let (hang_up, rx) = tokio::sync::oneshot::channel();
        let s = phone(rx).await;
        let t0 = Instant::now();
        let asked = s.request(request::FRONTMOST_APP, None, &[]);
        let (r, ()) = tokio::join!(asked, async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = hang_up.send(());
        });
        assert!(matches!(r, Err(AxError::Closed)), "{r:?}");
        assert!(t0.elapsed() < REQUEST_TIMEOUT, "{:?}", t0.elapsed());
    }
}
