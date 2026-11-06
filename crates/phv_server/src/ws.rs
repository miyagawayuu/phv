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
//!
//! The same script answers Server-Sent Events: a `GET` with `Accept:
//! text/event-stream` to its path (when no page answers it) runs the Open
//! event, then streams what is sent and published to the connection as
//! `data:` lines; the Close event runs when the client goes away. There are no
//! Message events (the client cannot send on the stream).

use crate::body::{Held, RespBody};
use crate::{busy, simple, Page, Resp, State};
use futures_util::{SinkExt, StreamExt};
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::{Request, Response, StatusCode};
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
/// An event stream gets a comment line this often, so proxies keep it open and
/// an HTTP/2 connection never looks stalled (`WRITE_STALL`, 30 s).
const EVENT_PING_EVERY: Duration = Duration::from_secs(15);
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
        state.metrics.reject(crate::metrics::Reject::Busy);
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
    let Ok(slot) = state.ws_slots.clone().try_acquire_owned() else {
        state.metrics.reject(crate::metrics::Reject::Busy);
        return busy();
    };
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

/// Whether a request asks for Server-Sent Events: a `GET` (or `HEAD`) that
/// accepts `text/event-stream` and is not a WebSocket upgrade.
pub fn wants_events(req: &Request<Incoming>) -> bool {
    (req.method() == hyper::Method::GET || req.method() == hyper::Method::HEAD)
        && !is_upgrade(req)
        && req.headers().get_all(hyper::header::ACCEPT).iter().filter_map(|v| v.to_str().ok()).any(|v| {
            v.split(',').any(|t| t.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

/// What one event stream may hold unsent, and the largest event: a message of
/// 1 MB with up to about 150,000 lines (found by the 1.1 audit: with 8 MB, a
/// few streams that did not read filled the events' budget).
const STREAM_HELD: usize = 2 << 20;

/// One message as an event: a `data:` line for each of its lines.
fn event_bytes(text: &str) -> Vec<u8> {
    let mut out = String::with_capacity(text.len() + 16);
    for line in text.split("\r\n").flat_map(|l| l.split(['\r', '\n'])) {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.into_bytes()
}

/// Answers a request for Server-Sent Events with `script` (`*.ws.phv`): runs
/// its Open event and, unless it refuses, streams the connection's messages.
pub async fn events(
    state: &Arc<State>,
    script: PathBuf,
    params: Vec<(String, String)>,
    ip: String,
    req: Request<Incoming>,
) -> Resp {
    if !origin_allowed(state, &req) {
        return simple(StatusCode::FORBIDDEN, "text/plain", "event streams for other sites are refused".into());
    }
    let Ok(slot) = state.ws_slots.clone().try_acquire_owned() else {
        state.metrics.reject(crate::metrics::Reject::Busy);
        return busy();
    };
    let head = req.method() == hyper::Method::HEAD;
    let abort = req.extensions().get::<crate::Abort>().cloned();
    let id = phv_std::random_hex(16);
    let origin = Origin { method: hyper::Method::GET, uri: req.uri().clone(), headers: req.headers().clone() };
    let outbox = state.hub.lock().unwrap_or_else(|e| e.into_inner()).register(&id);
    let open = WsCtx { event: WsEvent::Open, id: id.clone(), ..WsCtx::default() };
    let (kept, cookies) = match run_event(state, &script, &params, &ip, &origin, open).await {
        Ok(k) => k,
        Err(refused) => {
            state.hub.lock().unwrap_or_else(|e| e.into_inner()).unregister(&id);
            return *refused;
        }
    };
    // Two events in flight; the rest waits in the outbox (`MAX_QUEUED`).
    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(2);
    let mut r = Response::new(RespBody::Stream(rx));
    let h = r.headers_mut();
    h.insert(hyper::header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream; charset=utf-8"));
    h.insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // Proxies (nginx) pass each event on at once.
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    for c in cookies {
        h.append(hyper::header::SET_COOKIE, c);
    }
    // For one visitor: never compressed or kept by a cache.
    r.extensions_mut().insert(crate::static_files::Personal);
    r.extensions_mut().insert(crate::Served { kind: crate::metrics::Kind::Sse, route: None });
    if head {
        state.hub.lock().unwrap_or_else(|e| e.into_inner()).unregister(&id);
        *r.body_mut() = RespBody::full(bytes::Bytes::new());
        return r;
    }
    let st = state.clone();
    tokio::spawn(async move {
        let _slot = slot;
        let held = Held::new();
        let me = Registered { held: held.clone(), abort: abort.clone() };
        st.streams.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), me);
        let conn = Conn { state: st.clone(), script, params, ip, origin, outbox, kept };
        conn.stream_events(tx, held, abort).await;
        st.streams.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        st.hub.lock().unwrap_or_else(|e| e.into_inner()).unregister(&id);
    });
    r
}

/// An open event stream as the others see it (`State::streams`).
pub struct Registered {
    held: Arc<Held>,
    abort: Option<crate::Abort>,
}

/// A stream that has held output this long without sending all of it may be
/// dropped to make room for a stream that sent everything (its client reads).
const MAKE_ROOM_AFTER: Duration = Duration::from_secs(1);
/// How long such a stream waits for that room before it skips the event.
const ROOM_WAIT: Duration = Duration::from_millis(200);

/// How the sending of one chunk ended.
enum Sent {
    Yes,
    /// The client went away.
    Gone,
    /// The stream must end and its connection be dropped: it held output for
    /// too long, or more messages waited than its queue takes.
    Cut,
    /// The server is stopping.
    Stopping,
}

/// What a stream has to watch while it waits to hand over a chunk.
struct Watch<'a> {
    tx: &'a tokio::sync::mpsc::Sender<bytes::Bytes>,
    held: &'a Held,
    outbox: &'a Outbox,
    check: tokio::time::Interval,
    drain: Duration,
    closing: tokio::sync::watch::Receiver<bool>,
}

impl Watch<'_> {
    /// Whether the stream has held output for longer than it may.
    fn stuck(&self) -> bool {
        self.held.held_for().is_some_and(|d| d >= self.drain)
    }

    /// Hands `chunk` to the connection when it takes it, watching meanwhile
    /// that the stream does not hold output for too long.
    async fn send(&mut self, chunk: bytes::Bytes) -> Sent {
        loop {
            tokio::select! {
                p = self.tx.reserve() => {
                    return match p {
                        Ok(p) => {
                            p.send(chunk);
                            Sent::Yes
                        }
                        Err(_) => Sent::Gone,
                    }
                }
                _ = self.check.tick() => {
                    if self.stuck() {
                        return Sent::Cut;
                    }
                }
                _ = self.outbox.ready.notified() => {
                    if self.outbox.overflow.load(Ordering::SeqCst) {
                        return Sent::Cut;
                    }
                }
                _ = self.closing.changed() => return Sent::Stopping,
            }
        }
    }
}

/// Counts the event `bytes` for stream `me`, which holds nothing, when the
/// events' budget had no room for it: drops the connections of the other
/// streams that have held output for `MAKE_ROOM_AFTER`, then tries again
/// for up to `ROOM_WAIT`.
async fn make_room(st: &State, me: &str, held: &Arc<Held>, bytes: Vec<u8>) -> Result<bytes::Bytes, Vec<u8>> {
    {
        let streams = st.streams.lock().unwrap_or_else(|e| e.into_inner());
        for (id, s) in streams.iter() {
            if id != me && s.held.held_for().is_some_and(|d| d >= MAKE_ROOM_AFTER) {
                if let Some(a) = &s.abort {
                    a.now();
                }
            }
        }
    }
    let until = Instant::now() + ROOM_WAIT;
    let mut bytes = bytes;
    loop {
        if st.events_buffered.load(Ordering::Relaxed) + bytes.len() <= st.events_cap {
            match crate::body::counted_twice_within(bytes, &st.events_buffered, held, st.events_cap) {
                Ok(chunk) => return Ok(chunk),
                Err(b) => bytes = b,
            }
        }
        if Instant::now() >= until {
            return Err(bytes);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
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
    /// Sends the connection's messages as events until the client goes away,
    /// the script closes it or the server stops; then runs the Close event.
    ///
    /// `held` is what the stream has handed over and the connection has not
    /// sent yet; `abort` drops the stream's connection.
    async fn stream_events(
        self,
        tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
        held: Arc<Held>,
        abort: Option<crate::Abort>,
    ) {
        let st = self.state.clone();
        let mut ping = tokio::time::interval(EVENT_PING_EVERY);
        ping.tick().await;
        let drain = crate::event_drain();
        let mut check = tokio::time::interval((drain / 10).clamp(Duration::from_millis(10), Duration::from_secs(1)));
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        check.tick().await;
        let mut w =
            Watch { tx: &tx, held: &held, outbox: &self.outbox, check, drain, closing: st.ws_closing.subscribe() };
        // Ended while holding output its client has not taken (it does not
        // read, or reads too little): its connection is dropped, so what it
        // holds is freed at once.
        let mut cut = false;
        let mut stopping = false;
        'conn: loop {
            if self.outbox.overflow.load(Ordering::SeqCst) {
                cut = true;
                break;
            }
            while let Some(out) = self.outbox.pop() {
                match out {
                    Out::Text(t) => {
                        // Events count in the server's output until they are sent,
                        // as pages do (found by the 1.1 audit: 30 slow readers held
                        // 3.9 GB). A stream that still holds output is the one
                        // ended, when it would hold more than `STREAM_HELD` or the
                        // events' budget is full; one that sent everything goes on,
                        // so slow readers cannot end the others' streams.
                        let bytes = event_bytes(&t);
                        // A message of so many lines that its event passes the
                        // limit is not sent as an event.
                        if bytes.len() > STREAM_HELD {
                            st.metrics.reject(crate::metrics::Reject::Output);
                            continue;
                        }
                        let had = held.bytes();
                        if had + bytes.len() > STREAM_HELD {
                            st.metrics.reject(crate::metrics::Reject::Output);
                            cut = true;
                            break 'conn;
                        }
                        // Streams have a budget of their own (found by the 1.1 audit:
                        // counted with pages, streams that did not read made every
                        // page answer 503). A stream that holds output may fill three
                        // quarters of it: the rest is kept for streams that sent
                        // everything, whose clients read (found by the 1.1.1 review:
                        // room freed by one stream that did not read was taken by
                        // another, and readers missed events).
                        let cap = if had > 0 { st.events_cap - st.events_cap / 4 } else { st.events_cap };
                        let counted = crate::body::counted_twice_within(bytes, &st.events_buffered, &held, cap);
                        // One that has held output for a while (its client does not
                        // read) is ended; one that took its last event just now
                        // skips this one.
                        let lagging = held.held_for().is_some_and(|d| d >= MAKE_ROOM_AFTER);
                        let chunk = match counted {
                            Ok(chunk) => chunk,
                            Err(_) if had > 0 && lagging => {
                                st.metrics.reject(crate::metrics::Reject::Output);
                                cut = true;
                                break 'conn;
                            }
                            Err(_) if had > 0 => {
                                st.metrics.reject(crate::metrics::Reject::Output);
                                continue;
                            }
                            // When it is full, a stream that sent everything first
                            // drops the streams that have held output for a while
                            // (found by the 1.1.1 review: enough streams that did not
                            // read made readers miss events until they were dropped),
                            // then skips the event if there is still no room
                            // (delivery is at most once).
                            Err(bytes) => match make_room(&st, &self.kept.id, &held, bytes).await {
                                Ok(chunk) => chunk,
                                Err(_) => {
                                    st.metrics.reject(crate::metrics::Reject::Output);
                                    continue;
                                }
                            },
                        };
                        match w.send(chunk).await {
                            Sent::Yes => {}
                            Sent::Gone => break 'conn,
                            Sent::Cut => {
                                st.metrics.reject(crate::metrics::Reject::Output);
                                cut = true;
                                break 'conn;
                            }
                            Sent::Stopping => {
                                stopping = true;
                                break 'conn;
                            }
                        }
                        st.metrics.ws_out.fetch_add(1, Ordering::Relaxed);
                    }
                    Out::Close(..) => break 'conn,
                }
            }
            tokio::select! {
                _ = self.outbox.ready.notified() => {}
                _ = tx.closed() => break,
                _ = ping.tick() => match w.send(bytes::Bytes::from_static(b": ping\n\n")).await {
                    Sent::Yes => {}
                    Sent::Gone => break,
                    Sent::Cut => {
                        st.metrics.reject(crate::metrics::Reject::Output);
                        cut = true;
                        break;
                    }
                    Sent::Stopping => {
                        stopping = true;
                        break;
                    }
                },
                // Output not all sent within `drain` (found by the 1.1.1 review: a
                // client that read a little now and then kept up to 2 MB counted
                // for as long as it liked).
                _ = w.check.tick() => {
                    if w.stuck() {
                        st.metrics.reject(crate::metrics::Reject::Output);
                        cut = true;
                        break;
                    }
                }
                _ = w.closing.changed() => {
                    stopping = true;
                    break;
                }
            }
        }
        let Watch { mut check, .. } = w;
        drop(tx);
        if cut && held.bytes() > 0 {
            if let Some(a) = &abort {
                a.now();
            }
        }
        if !stopping {
            let ev = self.event(WsEvent::Close, String::new());
            let _ = run_event(&st, &self.script, &self.params, &self.ip, &self.origin, ev).await;
            // Closed by the script (or its client went away): the client may
            // take the rest, within the same time (found by the 1.1.1 review: over
            // HTTP/2, what was left after the end of the stream was not watched).
            while held.bytes() > 0 {
                if held.held_for().is_some_and(|d| d >= drain) {
                    if let Some(a) = &abort {
                        a.now();
                    }
                    break;
                }
                check.tick().await;
            }
        }
    }

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
                        self.state.metrics.ws_out.fetch_add(1, Ordering::Relaxed);
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
                        self.state.metrics.ws_in.fetch_add(1, Ordering::Relaxed);
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
