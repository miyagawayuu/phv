//! Response bodies, and the guard that drops connections whose writes stall.

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A response body: in memory, or read from a file as it is sent (static files
/// and `response()->file()`), so large files never sit in memory whole.
pub enum RespBody {
    Full(Option<Bytes>),
    File { file: tokio::fs::File, left: u64, buf: Box<[u8]> },
}

impl RespBody {
    pub fn full(bytes: impl Into<Bytes>) -> RespBody {
        RespBody::Full(Some(bytes.into()))
    }

    pub fn file(file: tokio::fs::File, len: u64) -> RespBody {
        RespBody::File { file, left: len, buf: vec![0u8; 64 << 10].into_boxed_slice() }
    }
}

impl Body for RespBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match &mut *self {
            RespBody::Full(b) => Poll::Ready(b.take().filter(|b| !b.is_empty()).map(|b| Ok(Frame::data(b)))),
            RespBody::File { file, left, buf } => {
                if *left == 0 {
                    return Poll::Ready(None);
                }
                let want = buf.len().min(*left as usize);
                let mut rb = ReadBuf::new(&mut buf[..want]);
                match Pin::new(file).poll_read(cx, &mut rb) {
                    Poll::Ready(Ok(())) => {
                        let n = rb.filled().len();
                        if n == 0 {
                            // The file got shorter after its length was sent.
                            return Poll::Ready(Some(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "file truncated",
                            ))));
                        }
                        *left -= n as u64;
                        Poll::Ready(Some(Ok(Frame::data(Bytes::copy_from_slice(rb.filled())))))
                    }
                    Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            RespBody::Full(b) => b.as_ref().is_none_or(|b| b.is_empty()),
            RespBody::File { left, .. } => *left == 0,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            RespBody::Full(b) => SizeHint::with_exact(b.as_ref().map_or(0, |b| b.len() as u64)),
            RespBody::File { left, .. } => SizeHint::with_exact(*left),
        }
    }
}

/// A connection whose writes fail when no progress is made for `limit`: a
/// client that stops reading cannot hold a connection (and its response) forever.
pub struct StallGuard<S> {
    inner: S,
    limit: Duration,
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> StallGuard<S> {
    pub fn new(inner: S, limit: Duration) -> StallGuard<S> {
        StallGuard { inner, limit, timer: None }
    }

    /// Called when a write must wait: true once it has waited `limit`.
    fn stalled(&mut self, cx: &mut Context<'_>) -> bool {
        let limit = self.limit;
        let timer = self.timer.get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        timer.as_mut().poll(cx).is_ready()
    }
}

fn stall_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "the client stopped reading")
}

impl<S: AsyncRead + Unpin> AsyncRead for StallGuard<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for StallGuard<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_write(cx, data) {
            Poll::Pending if this.stalled(cx) => Poll::Ready(Err(stall_error())),
            Poll::Pending => Poll::Pending,
            ready => {
                this.timer = None;
                ready
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Pending if this.stalled(cx) => Poll::Ready(Err(stall_error())),
            Poll::Pending => Poll::Pending,
            ready => {
                this.timer = None;
                ready
            }
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
