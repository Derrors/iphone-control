//! The viewer's connections under hyper: a time limit on writing, and lingering on close.
//!
//! hyper has neither. A client that stops reading would hold its connection's place for good (there are
//! [`super::MAX_CONNECTIONS`]): a write that makes no progress for [`super::WRITE_TIMEOUT`] fails here, which ends the
//! connection, whatever was being written (a page, an event, a picture). And a socket closed with request data it
//! never read sends a reset, so the client may never see an answer given before its body was read (401, 403, 413):
//! closing first ends this side, then reads what the client still sends until it closes too, for a second and 1 MB
//! at most.
//!
//! [`HeadTap`] keeps the request heads as they came, for the checks hyper's parsed head no longer allows.

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
    /// Closing, until then: this side ended yet, and how much more of the client's rest is read off
    closing: Option<(Pin<Box<tokio::time::Sleep>>, bool, usize)>,
}

impl<T> Timed<T> {
    pub(super) fn new(inner: T, limit: Duration) -> Self {
        Self { inner, limit, stalled: None, closing: None }
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

    /// Ends this side, then reads off what the client still sends until it closes: both within [`LINGER`] (over TLS
    /// ending writes close_notify, which a client that reads nothing never takes), and [`LINGER_BYTES`] at most (see
    /// the module docs). A connection that is broken already ends at once
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let Self { inner, closing, .. } = &mut *self;
        let (timer, shut, left) =
            closing.get_or_insert_with(|| (Box::pin(tokio::time::sleep(LINGER)), false, LINGER_BYTES));
        // the time is up: the connection is dropped anyway
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Ok(()));
        }
        if !*shut {
            match Pin::new(&mut *inner).poll_shutdown(cx) {
                Poll::Ready(Ok(())) => *shut = true,
                // broken: nothing more will come
                Poll::Ready(Err(_)) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
        let mut b = [0u8; 8192];
        loop {
            let mut rb = ReadBuf::new(&mut b);
            match Pin::new(&mut *inner).poll_read(cx, &mut rb) {
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

/// The request heads as they came, for the checks that read one ([`super::Head::parse`] and the others): hyper hands
/// on what it parsed, and drops some of what those checks refuse (a second `Content-Length`, one beside
/// `Transfer-Encoding`). Fed with what the connection reads ([`Tapped`]); each request takes its head
/// ([`HeadTap::take`]), and the body after it is skipped by its length, so the next head is where the next request
/// starts. A chunked body isn't followed: such a request ends its connection (it is never kept open)
#[derive(Default)]
pub(super) struct HeadTap {
    /// From where the next head starts (or the rest of a body yet to be skipped, see `skip`)
    buf: Vec<u8>,
    /// Bytes of the last body not read yet: skipped as they come
    skip: u64,
    /// Lost track (a chunked body, or more unparsed than a connection holds): no more heads
    lost: bool,
}

/// More than hyper keeps unparsed ([`super::server::serve_conn`] limits its buffer to about this) plus a read
const TAP_MAX: usize = 2 * crate::MAX_HEAD + 8192;

impl HeadTap {
    fn record(&mut self, bytes: &[u8]) {
        if self.lost {
            return;
        }
        let skipped = usize::try_from(self.skip).unwrap_or(usize::MAX).min(bytes.len());
        self.skip -= skipped as u64;
        self.buf.extend_from_slice(&bytes[skipped..]);
        if self.buf.len() > TAP_MAX {
            self.lost();
        }
    }

    fn lost(&mut self) {
        self.lost = true;
        self.buf = Vec::new();
    }

    /// The next request's head, through its blank line; then its body, `body` bytes (`None`: chunked), is skipped.
    /// `None` when it isn't there (lost track, or no blank line: hyper reads `\n` alone as a line end, the checks
    /// don't)
    pub(super) fn take(&mut self, body: Option<u64>) -> Option<Vec<u8>> {
        if self.lost {
            return None;
        }
        // empty lines before a request are skipped (RFC 9112 2.2), by hyper too
        let start = self.buf.iter().position(|b| *b != b'\r' && *b != b'\n').unwrap_or(self.buf.len());
        let end = start + self.buf[start..].windows(4).position(|w| w == b"\r\n\r\n")? + 4;
        let head = self.buf[start..end].to_vec();
        self.buf.drain(..end);
        match body {
            Some(n) => {
                let here = usize::try_from(n).unwrap_or(usize::MAX).min(self.buf.len());
                self.buf.drain(..here);
                self.skip = n - here as u64;
            }
            None => self.lost(),
        }
        Some(head)
    }
}

/// `T`, with what is read from it fed to a [`HeadTap`]
pub(super) struct Tapped<T> {
    inner: T,
    tap: std::sync::Arc<std::sync::Mutex<HeadTap>>,
}

impl<T> Tapped<T> {
    pub(super) fn new(inner: T, tap: std::sync::Arc<std::sync::Mutex<HeadTap>>) -> Self {
        Self { inner, tap }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Tapped<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = r
            && buf.filled().len() > before
        {
            self.tap.lock().unwrap_or_else(|p| p.into_inner()).record(&buf.filled()[before..]);
        }
        r
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Tapped<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
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

    /// A writer whose end never goes out (a TLS close_notify to a client that reads nothing), or that is broken
    struct Stuck {
        broken: bool,
    }

    impl AsyncRead for Stuck {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for Stuck {
        fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if self.broken { Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())) } else { Poll::Pending }
        }
    }

    /// Ending a connection takes the second of lingering at most, also when the end itself can't be written (not the
    /// write limit); a broken one ends at once
    #[tokio::test]
    async fn closing_takes_a_second_at_most_and_none_when_broken() {
        let mut t = Timed::new(Stuck { broken: false }, Duration::from_secs(30));
        let t0 = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), t.shutdown()).await.unwrap().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        let mut t = Timed::new(Stuck { broken: true }, Duration::from_secs(30));
        let t0 = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), t.shutdown()).await.unwrap().unwrap();
        assert!(t0.elapsed() < Duration::from_millis(200), "{:?}", t0.elapsed());
    }

    /// Each request's head as it came, the bodies between them skipped by their length, however the bytes arrive
    #[test]
    fn the_tap_finds_each_head_past_the_bodies() {
        let first = "POST /api/tap HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n";
        let second = "GET /api/info HTTP/1.1\r\nHost: h\r\n\r\n";
        let third = "POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 3\r\n\r\n";
        let all = format!("\r\n{first}hello{second}{third}0\r\n\r\nGET / HTTP/1.1\r\n\r\n");
        for step in [1, 3, 7, all.len()] {
            let mut tap = HeadTap::default();
            let mut fed = 0;
            let mut feed = |tap: &mut HeadTap, upto: usize| {
                while fed < upto.min(all.len()) {
                    let n = step.min(all.len() - fed);
                    tap.record(&all.as_bytes()[fed..fed + n]);
                    fed += n;
                }
            };
            // the first head, before its body has come
            feed(&mut tap, 2 + first.len());
            assert_eq!(tap.take(Some(5)).as_deref(), Some(first.as_bytes()), "step {step}");
            feed(&mut tap, all.len());
            assert_eq!(tap.take(Some(0)).as_deref(), Some(second.as_bytes()));
            assert_eq!(tap.take(None).as_deref(), Some(third.as_bytes()));
            // a chunked body isn't followed: nothing after it is taken for a head
            assert_eq!(tap.take(Some(0)), None);
        }
        // no blank line (yet): not a head
        let mut tap = HeadTap::default();
        tap.record(b"GET / HTTP/1.1\nHost: h\n\n");
        assert_eq!(tap.take(Some(0)), None);
    }
}
