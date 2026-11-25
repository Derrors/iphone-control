//! One reader of a phone's MJPEG stream (WDA's port 9100) for every page that watches it.
//!
//! WDA captures and compresses a picture for each connection to its MJPEG port: three pages open meant three
//! captures, enough to push WDA over the CPU limit iOS kills a process at. Here the first page for a phone opens the
//! stream, the frames (repeated ones dropped) go to a [`watch`] channel, and later pages subscribe to it. The reader
//! stops when the last page goes (the [`Subscription`]s hold it) or the stream ends (every page then reconnects and
//! the next one opens a new stream).
//!
//! Only the source is shared. Each page keeps its own H.264 encoder ([`super::serve`]): how fast pictures go into
//! the encoder follows what that page's link takes, so the chain of frames stays whole on a slow link without
//! waiting for keyframes. A shared encoder would have to hold every page to the slowest one.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Weak};

use tokio::io::AsyncRead;
use tokio::sync::watch;

use super::{AbortOnDrop, STREAM_IDLE, mjpeg};

/// One JPEG frame
pub type Frame = Arc<Vec<u8>>;

/// The phones' shared MJPEG streams, by UDID
pub struct MjpegHub {
    sources: tokio::sync::Mutex<HashMap<String, Weak<Source>>>,
    /// A stream that brings no frame for this long has stalled (WDA repeats even a still screen)
    idle: std::time::Duration,
}

impl Default for MjpegHub {
    fn default() -> Self {
        Self::with_idle(STREAM_IDLE)
    }
}

struct Source {
    /// Kept to tell whether the reader is still there ([`watch::Receiver::has_changed`] fails once it ended)
    frames: watch::Receiver<Option<Frame>>,
    _reader: AbortOnDrop,
}

/// A page's share of a phone's stream: the newest frame, and the stream kept open while it lives
pub struct Subscription {
    pub frames: watch::Receiver<Option<Frame>>,
    _source: Arc<Source>,
}

impl MjpegHub {
    pub fn with_idle(idle: std::time::Duration) -> Self {
        Self { sources: Default::default(), idle }
    }

    /// Subscribes to the stream of `udid`, opening it with `connect` (which also sends the request) if no page
    /// watches it yet. The newest frame counts as new for the subscriber, so a page that comes to a still screen
    /// gets a picture at once
    pub async fn subscribe<S, F, Fut>(&self, udid: &str, connect: F) -> anyhow::Result<Subscription>
    where
        S: AsyncRead + Unpin + Send + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<S>>,
    {
        // held while connecting: two pages opened at once share one stream
        let mut sources = self.sources.lock().await;
        sources.retain(|_, w| w.strong_count() > 0);
        let live = sources.get(udid).and_then(Weak::upgrade).filter(|s| s.frames.has_changed().is_ok());
        let source = match live {
            Some(s) => s,
            None => {
                let src = connect().await?;
                let (tx, rx) = watch::channel(None);
                let s = Arc::new(Source { frames: rx, _reader: AbortOnDrop(tokio::spawn(pump(src, tx, self.idle))) });
                sources.insert(udid.to_owned(), Arc::downgrade(&s));
                s
            }
        };
        let mut frames = source.frames.clone();
        frames.mark_changed();
        Ok(Subscription { frames, _source: source })
    }

    /// How many phones' streams are open (for tests)
    #[cfg(test)]
    async fn open(&self) -> usize {
        self.sources.lock().await.values().filter(|w| w.strong_count() > 0).count()
    }
}

/// Reads frames off `src` into `tx` until the stream ends, stalls for `idle` or nobody subscribes any more
async fn pump(src: impl AsyncRead + Unpin, tx: watch::Sender<Option<Frame>>, idle: std::time::Duration) {
    let mut f = mjpeg::Frames::new(src);
    let mut last: Option<Frame> = None;
    loop {
        match tokio::time::timeout(idle, f.next()).await {
            Ok(Ok(Some(j))) => {
                // WDA repeats a still screen about 25 times a second
                if last.as_deref() == Some(&j) {
                    continue;
                }
                let j = Arc::new(j);
                last = Some(j.clone());
                if tx.send(Some(j)).is_err() {
                    return;
                }
            }
            Err(_) | Ok(Ok(None)) => return,
            Ok(Err(e)) => {
                tracing::debug!("MJPEG stream: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn part(j: &[u8]) -> Vec<u8> {
        [format!("--wda\r\nContent-Length: {}\r\n\r\n", j.len()).as_bytes(), j, b"\r\n"].concat()
    }

    /// Five pages on one phone open one stream; each gets the frame that is there when it comes; the stream closes
    /// with the last page, and the next page opens a new one
    #[tokio::test]
    async fn pages_share_one_stream_per_phone() {
        let hub = MjpegHub::default();
        let opened = Arc::new(AtomicUsize::new(0));
        let (mut phone, src) = tokio::io::duplex(4096);
        let mut src = Some(src);
        let mut subs = Vec::new();
        for i in 0..5 {
            let (opened, src) = (opened.clone(), src.take());
            let sub = hub
                .subscribe("A", || async move {
                    opened.fetch_add(1, Ordering::SeqCst);
                    Ok(src.expect("connected once"))
                })
                .await
                .unwrap();
            subs.push(sub);
            if i == 0 {
                phone.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
                phone.write_all(&part(b"\xff\xd8one\xff\xd9")).await.unwrap();
                // the first frame is in before the others subscribe
                let first = &mut subs[0].frames;
                tokio::time::timeout(Duration::from_secs(2), first.wait_for(Option::is_some)).await.unwrap().unwrap();
            }
        }
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert_eq!(hub.open().await, 1);
        for (i, s) in subs.iter_mut().enumerate() {
            // a still screen: the frame already there counts as new for every later page (the first saw it come)
            assert_eq!(s.frames.has_changed().unwrap(), i > 0);
            assert_eq!(s.frames.borrow_and_update().as_deref().map(Vec::as_slice), Some(&b"\xff\xd8one\xff\xd9"[..]));
        }
        // a repeated frame is dropped; a new one reaches every page
        phone.write_all(&part(b"\xff\xd8one\xff\xd9")).await.unwrap();
        phone.write_all(&part(b"\xff\xd8two\xff\xd9")).await.unwrap();
        for s in &mut subs {
            tokio::time::timeout(Duration::from_secs(2), s.frames.changed()).await.unwrap().unwrap();
            assert_eq!(s.frames.borrow_and_update().as_deref().map(Vec::as_slice), Some(&b"\xff\xd8two\xff\xd9"[..]));
        }
        drop(subs);
        // the reader is gone with the last page: the phone sees the stream closed
        tokio::time::timeout(Duration::from_secs(2), async { assert_eq!(phone.read(&mut [0]).await.unwrap(), 0) })
            .await
            .unwrap();
        let (_phone2, src2) = tokio::io::duplex(64);
        let o = opened.clone();
        let _again = hub
            .subscribe("A", || async move {
                o.fetch_add(1, Ordering::SeqCst);
                Ok(src2)
            })
            .await
            .unwrap();
        assert_eq!(opened.load(Ordering::SeqCst), 2);
    }

    /// A stream that ended isn't handed to the next page, even while an earlier page still holds it
    #[tokio::test]
    async fn an_ended_stream_is_opened_again() {
        let hub = MjpegHub::default();
        let (phone, src) = tokio::io::duplex(64);
        let mut old = hub.subscribe("A", || async { Ok(src) }).await.unwrap();
        drop(phone);
        // the page sees the end (and reconnects)
        tokio::time::timeout(Duration::from_secs(2), async { while old.frames.changed().await.is_ok() {} })
            .await
            .unwrap();
        let (_phone, src) = tokio::io::duplex(64);
        let opened = Arc::new(AtomicUsize::new(0));
        let o = opened.clone();
        let _new = hub
            .subscribe("A", || async move {
                o.fetch_add(1, Ordering::SeqCst);
                Ok(src)
            })
            .await
            .unwrap();
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        // a failed connection is reported and leaves nothing behind
        let e = hub.subscribe::<tokio::io::DuplexStream, _, _>("B", || async { anyhow::bail!("no WDA") }).await;
        assert!(e.is_err());
    }
}
