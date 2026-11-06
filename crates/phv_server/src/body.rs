//! Response bodies, and the guard that drops connections whose writes stall.

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The most of a body in memory handed to the connection at once. The
/// connection copies what it is given into its own buffer, so a client that
/// stops reading holds this much of it, while the rest stays shared (and
/// counted in the output total) until it is sent.
const FRAME: usize = 64 << 10;

/// A response body: in memory, or read from a file as it is sent (static files
/// and `response()->file()`), so large files never sit in memory whole.
pub enum RespBody {
    Full(Option<Bytes>),
    File {
        file: tokio::fs::File,
        left: u64,
        buf: Box<[u8]>,
    },
    /// Chunks sent as they come (Server-Sent Events), until the sender is dropped.
    Stream(tokio::sync::mpsc::Receiver<Bytes>),
    /// A page's output sent while it runs (`response()->flush()`). When the
    /// sender is dropped the body ends, or fails if `aborted` was set first
    /// (the page failed or stopped): the client sees a cut response, not a
    /// complete one.
    Chunks {
        rx: tokio::sync::mpsc::Receiver<Bytes>,
        aborted: Arc<std::sync::atomic::AtomicBool>,
    },
}

/// Output counted in a server-wide total until the last copy of its bytes is
/// dropped, that is, once the connection has written them (or is gone).
struct Counted {
    data: Vec<u8>,
    total: Arc<AtomicUsize>,
}

impl AsRef<[u8]> for Counted {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.total.fetch_sub(self.data.len(), Ordering::Relaxed);
    }
}

/// What one event stream holds unsent, and since when it has held any: a
/// client that reads keeps getting back to nothing held.
pub struct Held {
    state: Mutex<(usize, Instant)>,
}

impl Held {
    pub fn new() -> Arc<Held> {
        Arc::new(Held { state: Mutex::new((0, Instant::now())) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (usize, Instant)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn add(&self, n: usize) {
        let mut s = self.lock();
        if s.0 == 0 {
            s.1 = Instant::now();
        }
        s.0 += n;
    }

    fn sub(&self, n: usize) {
        let mut s = self.lock();
        s.0 = s.0.saturating_sub(n);
    }

    pub fn bytes(&self) -> usize {
        self.lock().0
    }

    /// How long it has held output without getting back to nothing (`None`
    /// while it holds nothing).
    pub fn held_for(&self) -> Option<Duration> {
        let s = self.lock();
        (s.0 > 0).then(|| s.1.elapsed())
    }
}

/// Output counted in the server's total and in one stream's until its last
/// copy is dropped.
struct CountedTwice {
    data: Vec<u8>,
    server: Arc<AtomicUsize>,
    held: Arc<Held>,
}

impl AsRef<[u8]> for CountedTwice {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl Drop for CountedTwice {
    fn drop(&mut self) {
        self.server.fetch_sub(self.data.len(), Ordering::Relaxed);
        self.held.sub(self.data.len());
    }
}

/// `data` counted in both `server` and `held` until it has been sent, unless
/// that would take `server` past `cap`: then nothing is counted and `data` is
/// given back. The size is added before it is compared, so callers at once
/// pass `cap` by none.
pub fn counted_twice_within(
    data: Vec<u8>,
    server: &Arc<AtomicUsize>,
    held: &Arc<Held>,
    cap: usize,
) -> Result<Bytes, Vec<u8>> {
    let len = data.len();
    if server.fetch_add(len, Ordering::Relaxed) + len > cap {
        server.fetch_sub(len, Ordering::Relaxed);
        return Err(data);
    }
    held.add(len);
    Ok(Bytes::from_owner(CountedTwice { data, server: server.clone(), held: held.clone() }))
}

/// `data` as bytes whose size counts in `total` until the last copy (or
/// slice) of them is dropped.
pub fn counted_bytes(data: Vec<u8>, total: &Arc<AtomicUsize>) -> Bytes {
    total.fetch_add(data.len(), Ordering::Relaxed);
    Bytes::from_owner(Counted { data, total: total.clone() })
}

impl RespBody {
    pub fn full(bytes: impl Into<Bytes>) -> RespBody {
        RespBody::Full(Some(bytes.into()))
    }

    /// A body whose size counts in `total` until it has been sent.
    pub fn counted(data: Vec<u8>, total: &Arc<AtomicUsize>) -> RespBody {
        RespBody::full(counted_bytes(data, total))
    }

    /// The bytes of a body held in memory (taken out of it), to replace them.
    pub fn take_bytes(&mut self) -> Option<Bytes> {
        match self {
            RespBody::Full(b) => b.take(),
            RespBody::File { .. } | RespBody::Stream(_) | RespBody::Chunks { .. } => None,
        }
    }

    pub fn file(file: tokio::fs::File, len: u64) -> RespBody {
        // 16 KB: an HTTP/2 connection may send several files at once.
        RespBody::File { file, left: len, buf: vec![0u8; 16 << 10].into_boxed_slice() }
    }
}

impl Body for RespBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match &mut *self {
            RespBody::Full(b) => {
                let Some(rest) = b.as_mut().filter(|b| !b.is_empty()) else {
                    *b = None;
                    return Poll::Ready(None);
                };
                // A slice keeps the whole body (and its count) alive until it is sent.
                let part = rest.split_to(rest.len().min(FRAME));
                if rest.is_empty() {
                    *b = None;
                }
                Poll::Ready(Some(Ok(Frame::data(part))))
            }
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
            RespBody::Stream(rx) => match rx.poll_recv(cx) {
                Poll::Ready(Some(b)) => Poll::Ready(Some(Ok(Frame::data(b)))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
            RespBody::Chunks { rx, aborted } => match rx.poll_recv(cx) {
                Poll::Ready(Some(b)) => Poll::Ready(Some(Ok(Frame::data(b)))),
                Poll::Ready(None) if aborted.load(Ordering::Acquire) => {
                    Poll::Ready(Some(Err(io::Error::new(io::ErrorKind::Interrupted, "the page stopped"))))
                }
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            RespBody::Full(b) => b.as_ref().is_none_or(|b| b.is_empty()),
            RespBody::File { left, .. } => *left == 0,
            RespBody::Stream(_) | RespBody::Chunks { .. } => false,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            RespBody::Full(b) => SizeHint::with_exact(b.as_ref().map_or(0, |b| b.len() as u64)),
            RespBody::File { left, .. } => SizeHint::with_exact(*left),
            RespBody::Stream(_) | RespBody::Chunks { .. } => SizeHint::default(),
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

    // Vectored writes let the connection queue the body's slices instead of
    // copying them, so the shared bytes stay counted until they are written.
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_write_vectored(cx, bufs) {
            Poll::Pending if this.stalled(cx) => Poll::Ready(Err(stall_error())),
            Poll::Pending => Poll::Pending,
            ready => {
                this.timer = None;
                ready
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body in memory is handed over in slices of at most 64 KB, each keeping
    /// the whole body counted until the last one is dropped.
    #[test]
    fn bodies_are_sent_in_counted_slices() {
        use std::task::Waker;
        let total = Arc::new(AtomicUsize::new(0));
        let mut body = RespBody::counted(vec![b'x'; 200_000], &total);
        let mut cx = Context::from_waker(Waker::noop());
        let mut parts = Vec::new();
        while let Poll::Ready(Some(Ok(f))) = Pin::new(&mut body).poll_frame(&mut cx) {
            parts.push(f.into_data().unwrap());
        }
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [65536, 65536, 65536, 3392]);
        drop(body);
        assert_eq!(total.load(Ordering::Relaxed), 200_000);
        parts.truncate(1);
        assert_eq!(total.load(Ordering::Relaxed), 200_000);
        parts.clear();
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn counted_output_is_released_with_its_last_copy() {
        let total = Arc::new(AtomicUsize::new(0));
        let mut body = RespBody::counted(vec![b'x'; 1000], &total);
        assert_eq!(total.load(Ordering::Relaxed), 1000);
        let RespBody::Full(bytes) = &mut body else { panic!("not in memory") };
        let sent = bytes.take().unwrap();
        drop(body);
        // The connection still holds the bytes it has not written yet.
        assert_eq!(total.load(Ordering::Relaxed), 1000);
        let part = sent.slice(500..);
        drop(sent);
        assert_eq!(total.load(Ordering::Relaxed), 1000);
        drop(part);
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }
}
