//! A response body written by a streaming handler: the picture, a task's events, a task's lease.
//!
//! The handlers write bytes to a connection ([`crate::video`] takes any `AsyncRead + AsyncWrite`). Under hyper they
//! write to a [`Pipe`] instead, and the response's body is a [`Streamed`]: it runs the handler's future each time
//! hyper asks for more and hands on what was written. The handler lives as long as the body: when hyper drops it
//! (the client went away, or stopped reading: [`super::io`]) the handler is dropped with it, as it ended with its
//! connection before. A write that has to wait for the client waits: the pipe holds a few pieces, then pushes back.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio_util::sync::PollSender;

/// Pieces written and not yet taken by hyper
const DEPTH: usize = 4;

/// Where a streaming handler writes. Reading from it waits for ever: the handler ends with its body, not by reading
/// the client's end of the connection
pub(super) struct Pipe {
    tx: PollSender<Bytes>,
}

/// The response body a [`Pipe`] fills, running its handler
pub(super) struct Streamed {
    handler: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    rx: mpsc::Receiver<Bytes>,
}

/// A pipe and the body it fills, the body running `handler` (given the pipe) as it is read
pub(super) fn streamed<F, Fut>(handler: F) -> Streamed
where
    F: FnOnce(Pipe) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(DEPTH);
    let fut = handler(Pipe { tx: PollSender::new(tx) });
    Streamed { handler: Some(Box::pin(fut)), rx }
}

impl Body for Streamed {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = &mut *self;
        // the handler makes progress whenever hyper wants more: it writes into the pipe, which wakes this
        if let Some(h) = this.handler.as_mut()
            && h.as_mut().poll(cx).is_ready()
        {
            this.handler = None;
        }
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(b)) => Poll::Ready(Some(Ok(Frame::data(b)))),
            // the handler is done and its pipe dropped: the body ends
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.handler.is_none() && self.rx.is_closed() && self.rx.is_empty()
    }
}

impl AsyncWrite for Pipe {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        match self.tx.poll_reserve(cx) {
            Poll::Ready(Ok(())) => match self.tx.send_item(Bytes::copy_from_slice(buf)) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(_) => Poll::Ready(Err(gone())),
            },
            Poll::Ready(Err(_)) => Poll::Ready(Err(gone())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.tx.close();
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for Pipe {
    fn poll_read(self: Pin<&mut Self>, _cx: &mut Context<'_>, _buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}

fn gone() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the client went away")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;

    /// What the handler writes is the body, in order, and the body ends with the handler
    #[tokio::test]
    async fn the_body_is_what_the_handler_writes() {
        let body = streamed(|mut p| async move {
            for i in 0..10u8 {
                p.write_all(&[i; 3]).await.unwrap();
            }
        });
        let all = tokio::time::timeout(std::time::Duration::from_secs(5), body.collect()).await.unwrap().unwrap();
        let want: Vec<u8> = (0..10u8).flat_map(|i| [i; 3]).collect();
        assert_eq!(all.to_bytes().as_ref(), want.as_slice());
    }

    /// Dropping the body (the client went away) drops the handler with it
    #[tokio::test]
    async fn the_handler_ends_with_its_body() {
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        struct Flag(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Flag {
            fn drop(&mut self) {
                let _ = self.0.take().map(|t| t.send(()));
            }
        }
        let flag = Flag(Some(tx));
        let mut body = streamed(move |mut p| async move {
            let _flag = flag;
            loop {
                if p.write_all(b"x").await.is_err() {
                    return;
                }
            }
        });
        // read a little: the handler is running
        let first = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        assert!(first.is_some());
        assert!(rx.try_recv().is_err(), "still running");
        drop(body);
        assert!(rx.try_recv().is_ok(), "dropped with the body");
    }
}
