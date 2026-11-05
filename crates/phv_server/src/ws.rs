//! WebSocket connections answered by `public/*.ws.phv` (`std.ws`).
//!
//! The script runs once for each event of a connection, as a page runs for a
//! request: when the connection opens (as part of the upgrade request, so a
//! status other than 200 refuses it), for each text message, and when it
//! closes. Between events a connection holds no VM and no worker, only its
//! socket, its kept values and the messages waiting to be sent to it; the
//! messages of one connection are handled one at a time.
//!
//! `publish()` hands a message to the hub, which puts it in the queue of each
//! connection that joined the topic. A connection whose queue grows past
//! `MAX_QUEUED` (a client that does not read) is closed.

use crate::{busy, simple, Page, Resp, State};
use futures_util::{SinkExt, StreamExt};
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use phv_vm::{WsCtx, WsEvent};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// WebSocket connections open at once.
pub const MAX_WEBSOCKETS: usize = 1024;
/// The largest message a client may send.
const MAX_MESSAGE: usize = 64 * 1024;
/// Bytes of messages waiting to be sent to one connection.
const MAX_QUEUED: usize = 1 << 20;
/// A ping is sent this often; a connection silent for `SILENT` is closed.
const PING_EVERY: Duration = Duration::from_secs(30);
const SILENT: Duration = Duration::from_secs(75);

/// What waits to be sent to a connection.
enum Out {
    Text(Arc<str>),
    Close(u16, String),
}

/// The queue of one connection.
pub struct Outbox {
    queue: Mutex<VecDeque<Out>>,
    bytes: AtomicUsize,
    /// More was queued than `MAX_QUEUED`: the connection is closed.
    overflow: std::sync::atomic::AtomicBool,
    ready: tokio::sync::Notify,
}

impl Outbox {
    fn new() -> Arc<Outbox> {
        Arc::new(Outbox {
            queue: Mutex::new(VecDeque::new()),
            bytes: AtomicUsize::new(0),
            overflow: std::sync::atomic::AtomicBool::new(false),
            ready: tokio::sync::Notify::new(),
        })
    }

    fn push(&self, out: Out) {
        let len = match &out {
            Out::Text(t) => t.len(),
            Out::Close(..) => 0,
        };
        if self.bytes.fetch_add(len, Ordering::SeqCst) + len > MAX_QUEUED {
            self.bytes.fetch_sub(len, Ordering::SeqCst);
            self.overflow.store(true, Ordering::SeqCst);
        } else if let Ok(mut q) = self.queue.lock() {
            q.push_back(out);
        }
        self.ready.notify_one();
    }

    fn pop(&self) -> Option<Out> {
        let out = self.queue.lock().ok()?.pop_front()?;
        if let Out::Text(t) = &out {
            self.bytes.fetch_sub(t.len(), Ordering::SeqCst);
        }
        Some(out)
    }
}

/// The connections of this server process and the topics they joined.
#[derive(Default)]
pub struct Hub {
    conns: HashMap<String, Arc<Outbox>>,
    topics: HashMap<String, HashSet<String>>,
    joined: HashMap<String, HashSet<String>>,
}

impl Hub {
    fn register(&mut self, id: &str) -> Arc<Outbox> {
        let o = Outbox::new();
        self.conns.insert(id.to_string(), o.clone());
        o
    }

    fn unregister(&mut self, id: &str) {
        self.conns.remove(id);
        for t in self.joined.remove(id).unwrap_or_default() {
            if let Some(members) = self.topics.get_mut(&t) {
                members.remove(id);
                if members.is_empty() {
                    self.topics.remove(&t);
                }
            }
        }
    }

    /// Carries out what an event's script asked for.
    pub fn apply(&mut self, ws: &WsCtx) {
        let Some(out) = self.conns.get(&ws.id).cloned() else { return };
        let joined = self.joined.entry(ws.id.clone()).or_default();
        for t in &ws.leaves {
            if joined.remove(t) {
                if let Some(members) = self.topics.get_mut(t) {
                    members.remove(&ws.id);
                    if members.is_empty() {
                        self.topics.remove(t);
                    }
                }
            }
        }
        for t in &ws.joins {
            if joined.len() < phv_std::wsserver::MAX_TOPICS && joined.insert(t.clone()) {
                self.topics.entry(t.clone()).or_default().insert(ws.id.clone());
            }
        }
        for m in &ws.sends {
            out.push(Out::Text(Arc::from(m.as_str())));
        }
        if let Some((code, reason)) = &ws.close {
            out.push(Out::Close(*code, reason.clone()));
        }
    }

    pub fn publish(&mut self, topic: &str, text: &str) {
        let Some(members) = self.topics.get(topic) else { return };
        let text: Arc<str> = Arc::from(text);
        for id in members {
            if let Some(o) = self.conns.get(id) {
                o.push(Out::Text(text.clone()));
            }
        }
    }
}

/// An upgrade to a WebSocket (RFC 6455) over HTTP/1.1.
pub fn is_upgrade(req: &Request<Incoming>) -> bool {
    let h = req.headers();
    let has = |name: &str, token: &str| {
        h.get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    };
    req.method() == hyper::Method::GET
        && req.version() == hyper::Version::HTTP_11
        && has("upgrade", "websocket")
        && has("connection", "upgrade")
        && h.get("sec-websocket-version").is_some_and(|v| v == "13")
        && h.contains_key("sec-websocket-key")
}

/// A browser sends the page's origin: a connection from another site (with
/// the visitor's cookies) is refused unless `[cors]` allows that site.
fn origin_allowed(state: &State, req: &Request<Incoming>) -> bool {
    let Some(origin) = req.headers().get("origin").and_then(|v| v.to_str().ok()) else { return true };
    let host = req.headers().get(hyper::header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let origin_host = origin.split_once("://").map_or("", |(_, rest)| rest);
    if !host.is_empty() && origin_host.eq_ignore_ascii_case(host) {
        return true;
    }
    // A WebSocket always carries the visitor's cookies: another site may open
    // one only when `[cors]` lists it by name with `credentials = true` (found
    // by the 1.0 audit: `origins = ["*"]` let any site connect as the visitor).
    origin != "null" && state.config.cors.as_ref().is_some_and(|c| !c.any && c.credentials && c.allows(origin))
}

/// The parts of the upgrade request, kept for the connection's later events.
#[derive(Clone)]
struct Origin {
    method: hyper::Method,
    uri: hyper::Uri,
    headers: hyper::HeaderMap,
}

impl Origin {
    fn parts(&self) -> hyper::http::request::Parts {
        let (mut parts, ()) = Request::builder()
            .method(self.method.clone())
            .uri(self.uri.clone())
            .body(())
            .expect("parts of a request that was received")
            .into_parts();
        parts.headers = self.headers.clone();
        parts
    }
}

/// What an event's run left: the connection's values and the cookies it set,
/// or the response that ends it (an error, or a refused opening).
type EventResult = Result<(WsCtx, Vec<HeaderValue>), Box<Resp>>;

/// Runs one event of the script with a worker.
async fn run_event(
    state: &Arc<State>,
    script: &Path,
    params: &[(String, String)],
    ip: &str,
    origin: &Origin,
    ws: WsCtx,
) -> EventResult {
    let Ok(Ok(permit)) = tokio::time::timeout(crate::QUEUE_WAIT, state.workers.clone().acquire_owned()).await else {
        return Err(Box::new(busy()));
    };
    let (st, script, params, ip, parts) =
        (state.clone(), script.to_path_buf(), params.to_vec(), ip.to_string(), origin.parts());
    let kind = if ws.event == WsEvent::Open { Page::Normal } else { Page::Event };
    let resp = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        st.execute(&script, params, ip, parts, bytes::Bytes::new(), kind, Some(ws))
    })
    .await
    .map_err(|_| Box::new(simple(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", "internal error".into())))?;
    match resp.extensions().get::<crate::WsDone>() {
        Some(done) if resp.status() == StatusCode::OK => {
            let cookies = resp.headers().get_all(hyper::header::SET_COOKIE).iter().cloned().collect();
            Ok((done.0.clone(), cookies))
        }
        _ => Err(Box::new(resp)),
    }
}

/// Answers an upgrade request for `script`: runs its Open event and, unless
/// it refuses, switches the connection to the WebSocket protocol.
pub async fn upgrade(
    state: &Arc<State>,
    script: PathBuf,
    params: Vec<(String, String)>,
    ip: String,
    mut req: Request<Incoming>,
) -> Resp {
    if !origin_allowed(state, &req) {
        return simple(
            StatusCode::FORBIDDEN,
            "text/plain",
            "WebSocket connections from other sites are refused".into(),
        );
    }
    let Ok(slot) = state.ws_slots.clone().try_acquire_owned() else { return busy() };
    let key = req.headers().get("sec-websocket-key").map(|k| k.as_bytes().to_vec()).unwrap_or_default();
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(&key);
    let on_upgrade = hyper::upgrade::on(&mut req);
    let origin = Origin { method: req.method().clone(), uri: req.uri().clone(), headers: req.headers().clone() };
    drop(req);

    let id = phv_std::random_hex(16);
    let outbox = state.hub.lock().unwrap_or_else(|e| e.into_inner()).register(&id);
    let open = WsCtx { event: WsEvent::Open, id: id.clone(), ..WsCtx::default() };
    let (kept, cookies) = match run_event(state, &script, &params, &ip, &origin, open).await {
        Ok(k) => k,
        Err(refused) => {
            state.hub.lock().unwrap_or_else(|e| e.into_inner()).unregister(&id);
            return *refused;
        }
    };

    let mut r = simple(StatusCode::SWITCHING_PROTOCOLS, "text/plain", String::new());
    let h = r.headers_mut();
    h.remove(hyper::header::CONTENT_TYPE);
    h.insert(hyper::header::UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(hyper::header::CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Ok(v) = HeaderValue::from_str(&accept) {
        h.insert("sec-websocket-accept", v);
    }
    // The cookies the Open event set (a new session) go with the switch.
    for c in cookies {
        h.append(hyper::header::SET_COOKIE, c);
    }
    let st = state.clone();
    tokio::spawn(async move {
        let _slot = slot;
        let Ok(upgraded) = on_upgrade.await else {
            st.hub.lock().unwrap_or_else(|e| e.into_inner()).unregister(&id);
            return;
        };
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE))
            .read_buffer_size(8 * 1024)
            .write_buffer_size(0)
            .max_write_buffer_size(MAX_QUEUED + MAX_MESSAGE);
        let stream = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(config)).await;
        let conn = Conn { state: st.clone(), script, params, ip, origin, outbox, kept };
        conn.run(stream).await;
        st.hub.lock().unwrap_or_else(|e| e.into_inner()).unregister(&id);
    });
    r
}

struct Conn {
    state: Arc<State>,
    script: PathBuf,
    params: Vec<(String, String)>,
    ip: String,
    origin: Origin,
    outbox: Arc<Outbox>,
    /// The connection's id and kept values (the rest is cleared per event).
    kept: WsCtx,
}

type Stream = WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;

impl Conn {
    fn event(&self, event: WsEvent, message: String) -> WsCtx {
        WsCtx { event, id: self.kept.id.clone(), message, state: self.kept.state.clone(), ..WsCtx::default() }
    }

    async fn run(mut self, mut stream: Stream) {
        let mut closing = self.state.ws_closing.subscribe();
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        let silent = SILENT.max(self.state.time_limit + PING_EVERY);
        let mut seen = Instant::now();
        let mut running: Option<tokio::task::JoinHandle<EventResult>> = None;
        let mut close: Option<(CloseCode, &'static str)> = None;
        let mut stopping = false;
        'conn: loop {
            // Send what is waiting first.
            if self.outbox.overflow.load(Ordering::SeqCst) {
                close = Some((CloseCode::Policy, "too many messages waiting"));
                break;
            }
            while let Some(out) = self.outbox.pop() {
                match out {
                    Out::Text(t) => {
                        if stream.send(Message::text(t.as_ref())).await.is_err() {
                            break 'conn;
                        }
                    }
                    Out::Close(code, reason) => {
                        let frame = CloseFrame { code: CloseCode::from(code), reason: reason.into() };
                        let _ = stream.close(Some(frame)).await;
                        break 'conn;
                    }
                }
            }
            tokio::select! {
                m = stream.next(), if running.is_none() => match m {
                    Some(Ok(Message::Text(t))) => {
                        seen = Instant::now();
                        // Counted like requests: the same key (IPv6 by /64) and store
                        // (found by the 1.0 audit: messages had a key of their own).
                        let ip = self.ip.parse::<std::net::IpAddr>().ok();
                        if let Some(ip) = ip {
                            if crate::rate_limited(&self.state, ip).await.is_some() {
                                close = Some((CloseCode::Policy, "too many messages"));
                                break;
                            }
                        }
                        let ev = self.event(WsEvent::Message, t.to_string());
                        let (st, script, params, ip, origin) =
                            (self.state.clone(), self.script.clone(), self.params.clone(), self.ip.clone(), self.origin.clone());
                        running = Some(tokio::spawn(async move {
                            run_event(&st, &script, &params, &ip, &origin, ev).await
                        }));
                    }
                    Some(Ok(Message::Binary(_))) => {
                        close = Some((CloseCode::Unsupported, "text messages only"));
                        break;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => seen = Instant::now(),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                },
                done = async { running.as_mut().expect("guarded").await }, if running.is_some() => {
                    running = None;
                    match done {
                        Ok(Ok((next, _))) => self.kept.state = next.state,
                        _ => {
                            close = Some((CloseCode::Error, "the server could not handle the message"));
                            break;
                        }
                    }
                }
                _ = self.outbox.ready.notified() => {}
                _ = ping.tick() => {
                    if seen.elapsed() > silent {
                        break;
                    }
                    if stream.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
                _ = closing.changed() => {
                    close = Some((CloseCode::Away, "the server is stopping"));
                    stopping = true;
                    break;
                }
            }
        }
        if let Some((code, reason)) = close {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                stream.close(Some(CloseFrame { code, reason: reason.into() })),
            )
            .await;
        }
        drop(stream);
        // A message still being handled finishes first; then the Close event.
        if let Some(r) = running.take() {
            if let Ok(Ok((next, _))) = r.await {
                self.kept.state = next.state;
            }
        }
        if !stopping {
            let ev = self.event(WsEvent::Close, String::new());
            let _ = run_event(&self.state, &self.script, &self.params, &self.ip, &self.origin, ev).await;
        }
    }
}
