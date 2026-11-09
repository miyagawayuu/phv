//! The admin port (`[admin] listen`): `/health`, `/ready` and `/metrics`, on a
//! listener of its own, so nothing is added to the site's own paths and a
//! firewall can keep the port inside the network.
//!
//! - `/health`: 200 while the server runs, 503 once it is stopping.
//! - `/ready`: 503 too while a store the server uses (sessions, rate limits,
//!   `publish()`, jobs) failed the last time it was used. No probe makes the
//!   server connect anywhere.
//! - `/metrics`: the Prometheus text format; with `[admin] token_env`, only
//!   with `Authorization: Bearer <token>`.
//!
//! Connections that send nothing cannot keep a probe out: see `serve`.

use crate::{body::RespBody, simple, State};
use bytes::Bytes;
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tokio::net::TcpListener;

/// Connections the admin port serves at once.
const MAX_ADMIN_CONNECTIONS: usize = 64;
/// Connections one client address (an IPv6 /64) keeps open at once.
const MAX_ADMIN_PER_ADDRESS: usize = 4;
/// A client must send its request (headers) within this time. Probes send
/// theirs at once.
const ADMIN_HEADER_TIMEOUT: Duration = Duration::from_secs(1);

/// An open connection of the admin port.
struct Open {
    id: u64,
    client: IpAddr,
    /// Its request came (it is answered at once, so it ends soon).
    asked: Arc<AtomicBool>,
    task: tokio::task::AbortHandle,
    /// Its socket (shared with its task), to see whether bytes of a request
    /// wait on it before the task has read them (a probe sends its request at
    /// once, before its task may have run).
    stream: Arc<tokio::net::TcpStream>,
}

impl Open {
    /// Has sent nothing yet (or has closed its side).
    fn silent(&self) -> bool {
        if self.asked.load(Ordering::SeqCst) {
            return false;
        }
        // The socket is non-blocking (tokio's), so this never waits.
        let mut byte = [std::mem::MaybeUninit::<u8>::uninit()];
        !matches!(socket2::SockRef::from(&*self.stream).peek(&mut byte), Ok(n) if n > 0)
    }
}

/// The admin port's open connections, oldest first, and how many each client has.
#[derive(Default)]
struct Conns {
    open: VecDeque<Open>,
    per_client: HashMap<IpAddr, usize>,
    next: u64,
}

impl Conns {
    fn remove_at(&mut self, i: usize) -> Option<Open> {
        let o = self.open.remove(i)?;
        if let Some(n) = self.per_client.get_mut(&o.client) {
            *n -= 1;
            if *n == 0 {
                self.per_client.remove(&o.client);
            }
        }
        Some(o)
    }

    fn remove(&mut self, id: u64) {
        if let Some(i) = self.open.iter().position(|o| o.id == id) {
            self.remove_at(i);
        }
    }

    /// Makes room for a connection from `client`: past the limits, the oldest
    /// connection that has sent no request (of that client, when it is the
    /// client's limit) is closed. False when none can be.
    fn make_room(&mut self, client: IpAddr) -> bool {
        let full_client = self.per_client.get(&client).copied().unwrap_or(0) >= MAX_ADMIN_PER_ADDRESS;
        let full = self.open.len() >= MAX_ADMIN_CONNECTIONS;
        if !full_client && !full {
            return true;
        }
        let candidate = |o: &Open| (!full_client || o.client == client) && o.silent();
        match self.open.iter().position(candidate) {
            Some(i) => {
                if let Some(o) = self.remove_at(i) {
                    // The socket closes once its task and `o` let go of it.
                    o.task.abort();
                }
                true
            }
            None => false,
        }
    }
}

/// The address a client is counted under: IPv6 by its /64, which one
/// client usually has whole.
fn client_of(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
}

/// The admin connection's stream: it marks the connection as having asked
/// once the end of a request head (`\r\n\r\n`) is read, so a probe whose head
/// was read and is being parsed is not taken for a silent connection (bytes not
/// read yet are seen by `Open::silent`'s peek).
struct Marked {
    inner: Arc<tokio::net::TcpStream>,
    asked: Arc<AtomicBool>,
    /// How many bytes of `\r\n\r\n` the bytes read so far end with.
    matched: usize,
}

impl tokio::io::AsyncRead for Marked {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let n = loop {
            ready!(self.inner.poll_read_ready(cx))?;
            match self.inner.try_read(buf.initialize_unfilled()) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        };
        let read = &buf.initialize_unfilled()[..n];
        const END: &[u8] = b"\r\n\r\n";
        let mut m = self.matched;
        let mut asked = false;
        for &b in read {
            m = if b == END[m] {
                m + 1
            } else if b == b'\r' {
                1
            } else {
                0
            };
            if m == END.len() {
                asked = true;
                m = 0;
            }
        }
        self.matched = m;
        if asked {
            self.asked.store(true, Ordering::SeqCst);
        }
        buf.advance(n);
        Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for Marked {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        loop {
            ready!(self.inner.poll_write_ready(cx))?;
            match self.inner.try_write(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                done => return Poll::Ready(done),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(socket2::SockRef::from(&*self.inner).shutdown(std::net::Shutdown::Write))
    }
}

/// Removes its connection from `Conns` when the connection's task ends,
/// also when it is aborted.
struct Registered {
    conns: Arc<Mutex<Conns>>,
    id: u64,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.conns.lock().unwrap_or_else(PoisonError::into_inner).remove(self.id);
    }
}

/// Serves the admin port until the runtime stops (after the site's
/// connections, so `/health` answers 503 while requests finish).
///
/// Connections that send nothing cannot keep probes out (found by the 19th
/// assessment, R22-21: 64 silent connections from one client delayed
/// `/health` by 5 s): a client keeps at most `MAX_ADMIN_PER_ADDRESS` open, a
/// new connection past a limit closes the oldest one that has sent no request,
/// and a request must come within `ADMIN_HEADER_TIMEOUT`.
pub(crate) async fn serve(state: Arc<State>, listener: TcpListener, token: Option<Vec<u8>>) {
    let conns = Arc::new(Mutex::new(Conns::default()));
    let token = Arc::new(token);
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        // A second handle on the socket, to peek at it (see `Open::silent`).
        let stream = Arc::new(stream);
        let client = client_of(peer.ip());
        {
            let mut c = conns.lock().unwrap_or_else(PoisonError::into_inner);
            if !c.make_room(client) {
                // Every connection there is answering a request: this one is closed.
                continue;
            }
            let id = c.next;
            c.next += 1;
            let asked = Arc::new(AtomicBool::new(false));
            let (state, token, flag, marked) = (state.clone(), token.clone(), asked.clone(), asked.clone());
            let shared = stream.clone();
            let registered = Registered { conns: conns.clone(), id };
            let task = tokio::spawn(async move {
                let _registered = registered;
                let service = service_fn(move |req| {
                    flag.store(true, Ordering::SeqCst);
                    answer(state.clone(), token.clone(), req)
                });
                let mut b = http1::Builder::new();
                b.timer(TokioTimer::new())
                    .header_read_timeout(ADMIN_HEADER_TIMEOUT)
                    .keep_alive(false)
                    .max_buf_size(16 * 1024);
                let stream = Marked { inner: stream, asked: marked, matched: 0 };
                let conn = b.serve_connection(TokioIo::new(stream), service);
                let _ = tokio::time::timeout(Duration::from_secs(30), conn).await;
            });
            // The task removes itself when it ends; it cannot do so before the lock is let go.
            c.open.push_back(Open { id, client, asked, task: task.abort_handle(), stream: shared });
            *c.per_client.entry(client).or_insert(0) += 1;
        }
        // The new connection's task runs before more are accepted.
        tokio::task::yield_now().await;
    }
}

async fn answer(
    state: Arc<State>,
    token: Arc<Option<Vec<u8>>>,
    req: Request<Incoming>,
) -> Result<Response<RespBody>, Infallible> {
    let text = |status: StatusCode, body: String| {
        let mut r = simple(status, "text/plain; charset=utf-8", body);
        r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
        r
    };
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return Ok(text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n".into()));
    }
    let stopping = state.stopping.load(Ordering::SeqCst);
    Ok(match req.uri().path() {
        "/health" if stopping => text(StatusCode::SERVICE_UNAVAILABLE, "stopping\n".into()),
        "/health" => text(StatusCode::OK, "ok\n".into()),
        "/ready" => {
            let down = state.metrics.health.down();
            if stopping {
                text(StatusCode::SERVICE_UNAVAILABLE, "stopping\n".into())
            } else if !down.is_empty() {
                text(StatusCode::SERVICE_UNAVAILABLE, format!("unavailable: {}\n", down.join(", ")))
            } else {
                text(StatusCode::OK, "ready\n".into())
            }
        }
        "/metrics" => {
            if let Some(t) = token.as_ref() {
                let sent = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .unwrap_or("");
                if !phv_std::constant_time_eq(sent.as_bytes(), t) {
                    let mut r = text(StatusCode::UNAUTHORIZED, "unauthorized\n".into());
                    r.headers_mut().insert("www-authenticate", HeaderValue::from_static("Bearer"));
                    return Ok(r);
                }
            }
            let body = state.metrics_text();
            let mut r = Response::new(RespBody::full(Bytes::from(body)));
            r.headers_mut()
                .insert("content-type", HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"));
            r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
            r
        }
        _ => text(StatusCode::NOT_FOUND, "not found\n".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request that waits on a connection is seen through its second
    /// handle before the connection's task reads it, so the connection is not
    /// closed as a silent one.
    #[test]
    fn a_request_waiting_on_a_connection_is_seen() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let _silent_client = std::net::TcpStream::connect(addr).unwrap();
            let (silent, _) = listener.accept().await.unwrap();
            let mut asking_client = std::net::TcpStream::connect(addr).unwrap();
            std::io::Write::write_all(&mut asking_client, b"GET /health HTTP/1.1\r\n\r\n").unwrap();
            let (asking, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let open = |s: tokio::net::TcpStream| {
                let task = tokio::spawn(async {});
                let asked = Arc::new(AtomicBool::new(false));
                Open { id: 0, client: addr.ip(), asked, task: task.abort_handle(), stream: Arc::new(s) }
            };
            assert!(open(silent).silent());
            assert!(!open(asking).silent());
        });
    }
}
