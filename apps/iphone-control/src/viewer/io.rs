//! The viewer's connections under hyper: a time limit on writing, and lingering on close.
//!
//! hyper has neither. A client that stops reading would hold its connection's place for good (there are
//! [`super::MAX_CONNECTIONS`]): a write that makes no progress for [`super::WRITE_TIMEOUT`] fails here, which ends the
//! connection, whatever was being written (a page, an event, a picture). And a socket closed with request data it
//! never read sends a reset, so the client may never see an answer given before its body was read (401, 403, 413):
//! closing first ends this side, then reads what the client still sends until it closes too, for a second and 1 MB
//! at most.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// How long closing reads off what the client still sends
const LINGER: Duration = Duration::from_secs(1);
/// … and how much
const LINGER_BYTES: usize = 1 << 20;

/// `T` with a time limit on each write's progress and lingering on shutdown (see the module docs)
pub(super) struct Timed<T> {
    inner: T,
    limit: Duration,
    /// Set while a write is pending: it fails once this passes
    stalled: Option<Pin<Box<tokio::time::Sleep>>>,
    /// Closing: this side ended, the client's rest being read off until then
    lingering: Option<(Pin<Box<tokio::time::Sleep>>, usize)>,
}

impl<T> Timed<T> {
    pub(super) fn new(inner: T, limit: Duration) -> Self {
        Self { inner, limit, stalled: None, lingering: None }
    }

    /// A pending write: fails once it has waited for `limit`
    fn check_stall(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Error> {
        let limit = self.limit;
        let timer = self.stalled.get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        match timer.as_mut().poll(cx) {
            Poll::Ready(()) => {
                Poll::Ready(std::io::Error::new(std::io::ErrorKind::TimedOut, "the client stopped reading"))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Timed<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for Timed<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(r) => {
                self.stalled = None;
                Poll::Ready(r)
            }
            Poll::Pending => self.check_stall(cx).map(Err),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Ready(r) => {
                self.stalled = None;
                Poll::Ready(r)
            }
            Poll::Pending => self.check_stall(cx).map(Err),
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write_vectored(cx, bufs) {
            Poll::Ready(r) => {
                self.stalled = None;
                Poll::Ready(r)
            }
            Poll::Pending => self.check_stall(cx).map(Err),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    /// Ends this side (bounded like a write: over TLS it writes close_notify), then reads off what the client still
    /// sends until it closes, for [`LINGER`] and [`LINGER_BYTES`] at most (see the module docs)
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.lingering.is_none() {
            match Pin::new(&mut this.inner).poll_shutdown(cx) {
                Poll::Ready(_) => {
                    this.stalled = None;
                    this.lingering = Some((Box::pin(tokio::time::sleep(LINGER)), LINGER_BYTES));
                }
                Poll::Pending => {
                    return match this.check_stall(cx) {
                        // gave up ending it nicely: the connection is dropped anyway
                        Poll::Ready(_) => Poll::Ready(Ok(())),
                        Poll::Pending => Poll::Pending,
                    };
                }
            }
        }
        let (timer, left) = this.lingering.as_mut().expect("set above");
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ok(()));
        }
        let mut b = [0u8; 8192];
        loop {
            let mut rb = ReadBuf::new(&mut b);
            match Pin::new(&mut this.inner).poll_read(cx, &mut rb) {
                Poll::Ready(Ok(())) if !rb.filled().is_empty() => {
                    *left = left.saturating_sub(rb.filled().len());
                    if *left == 0 {
                        return Poll::Ready(Ok(()));
                    }
                }
                // closed by the client too, or broken
                Poll::Ready(_) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A client that reads nothing makes the write fail after the limit, instead of holding it for good
    #[tokio::test]
    async fn a_write_nobody_reads_gives_up() {
        let (a, _b) = tokio::io::duplex(1024);
        let mut t = Timed::new(a, Duration::from_millis(100));
        let t0 = std::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(5), t.write_all(&vec![0u8; 1 << 20])).await.unwrap();
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    /// Closing reads off what the client still sends, then ends: until it closes, or a second at most
    #[tokio::test]
    async fn closing_reads_off_the_rest_then_ends() {
        let (a, mut b) = tokio::io::duplex(1 << 16);
        b.write_all(b"an unread body").await.unwrap();
        let mut t = Timed::new(a, Duration::from_secs(1));
        // the client reads to the end, then keeps its side open for a while
        let client = tokio::spawn(async move {
            let mut got = Vec::new();
            b.read_to_end(&mut got).await.unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
            got
        });
        // the client keeps its side open: the second limits it
        let t0 = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), t.shutdown()).await.unwrap().unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(900), "{:?}", t0.elapsed());
        drop(t);
        assert!(client.await.unwrap().is_empty());
    }
}
