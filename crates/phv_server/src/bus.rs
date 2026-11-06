//! `[server] publish_store`: carries `publish()` to the other server processes
//! of the site through a Redis server (PUBLISH / SUBSCRIBE).
//!
//! - Each process delivers its own messages to its own connections first, as
//!   without a store, then queues them for the others; a script never waits for
//!   Redis. The queue is bounded (`MAX_QUEUED` messages, `MAX_QUEUED_BYTES`):
//!   when it is full, or Redis cannot be reached, messages for the other
//!   processes are dropped and counted.
//! - One thread sends the queue in pipelines; one thread keeps a subscribed
//!   connection and hands each message to the connections of this process. A
//!   connection lost is opened again after 1 s, then 2, 4, ... up to 30 s.
//! - Messages are signed (HMAC-SHA256 with a key derived from `[security]
//!   secret_env`) and carry the id of the process that sent them: a message
//!   that is not signed with the site's key, is malformed or breaks the rules of
//!   `publish()` is dropped, and a process skips its own. The channel's name is
//!   derived from the key too, so sites sharing a Redis server do not see each
//!   other's messages.
//! - Delivery is at most once: messages published while a process is not
//!   connected never reach it.

use crate::metrics::{Metrics, StoreKind};
use crate::ws::Hub;
use phv_std::wsserver::{MAX_TEXT, MAX_TOPIC};
use phv_std::Pushed;
use phv_vm::DatabaseSpec;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Messages waiting to be sent to the other processes.
const MAX_QUEUED: usize = 10_000;
/// Bytes of messages waiting to be sent.
const MAX_QUEUED_BYTES: usize = 32 << 20;
/// Messages sent in one pipeline.
const BATCH: usize = 256;
const MAGIC: &[u8; 4] = b"PHV2";
const ID_LEN: usize = 16;
const MAC_LEN: usize = 32;
/// The longest message the store may carry: a topic and a text at their
/// longest, and the frame around them.
pub const MAX_FRAME: usize = HEAD + MAX_TOPIC + MAX_TEXT + MAC_LEN;
/// Magic, instance, time (ms), sequence number, topic length.
const HEAD: usize = 4 + ID_LEN + 8 + 8 + 2;
/// How far a message's time may be from the receiver's clock (found by the
/// 1.1 audit: a captured message could be published again at any time).
const MAX_AGE_MS: i64 = 60_000;
/// The processes whose last sequence number is remembered.
const MAX_SENDERS: usize = 4096;
/// Waits before connecting again: 1 s, doubled up to this.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A quiet subscribed connection is asked to answer this often.
const PING_EVERY: Duration = Duration::from_secs(30);

pub struct Bus {
    /// Topic and text of each message for the other processes.
    tx: SyncSender<(String, String)>,
    queued_bytes: Arc<AtomicUsize>,
    metrics: Arc<Metrics>,
    stop: Arc<AtomicBool>,
}

impl Bus {
    /// Starts the threads of the bus for the Redis server `spec`, with the
    /// site's secret.
    pub fn start(spec: DatabaseSpec, secret: &[u8], hub: Arc<Mutex<Hub>>, metrics: Arc<Metrics>) -> Bus {
        let key = phv_std::server_key(secret, "phv publish v1");
        let mut instance = [0u8; ID_LEN];
        getrandom::fill(&mut instance).expect("operating system random number generator failed");
        let channel = format!("phv:publish:{}", hex(&phv_std::hmac_sha256(&key, b"channel")[..8]));
        let (tx, rx) = sync_channel(MAX_QUEUED);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (spec, channel, queued, metrics, stop, key) =
                (spec.clone(), channel.clone(), queued_bytes.clone(), metrics.clone(), stop.clone(), key.clone());
            let signer = Signer { key, instance, seq: 0 };
            std::thread::Builder::new()
                .name("phv-publish".into())
                .spawn(move || send_loop(&spec, &channel, rx, signer, &queued, &metrics, &stop))
                .expect("cannot start a thread");
        }
        {
            let (key, metrics, stop) = (key.clone(), metrics.clone(), stop.clone());
            std::thread::Builder::new()
                .name("phv-subscribe".into())
                .spawn(move || receive_loop(&spec, &channel, &key, instance, &hub, &metrics, &stop))
                .expect("cannot start a thread");
        }
        Bus { tx, queued_bytes, metrics, stop }
    }

    /// Queues a message for the other processes (never waits).
    pub fn send(&self, topic: &str, text: &str) {
        let len = topic.len() + text.len();
        if self.queued_bytes.fetch_add(len, Ordering::SeqCst) + len > MAX_QUEUED_BYTES {
            self.queued_bytes.fetch_sub(len, Ordering::SeqCst);
            return self.dropped(1);
        }
        match self.tx.try_send((topic.to_string(), text.to_string())) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.queued_bytes.fetch_sub(len, Ordering::SeqCst);
                self.dropped(1);
            }
        }
    }

    fn dropped(&self, n: u64) {
        dropped(&self.metrics, n);
    }

    /// Stops the threads (within a second).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Counts dropped messages, and says so at most once a minute.
fn dropped(metrics: &Metrics, n: u64) {
    static WARNED: Mutex<Option<Instant>> = Mutex::new(None);
    metrics.publish_dropped.fetch_add(n, Ordering::Relaxed);
    let mut w = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if w.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
        *w = Some(Instant::now());
        eprintln!("warning: publish(): messages for the other server processes were dropped (the queue is full or the publish_store cannot be reached)");
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// `PHV2 | instance | time in ms (i64) | sequence number (u64) | topic length
/// (u16) | topic | text | HMAC-SHA256 of all before`.
fn encode(key: &[u8], instance: &[u8; ID_LEN], time: i64, seq: u64, topic: &str, text: &str) -> Vec<u8> {
    let mut f = Vec::with_capacity(HEAD + topic.len() + text.len() + MAC_LEN);
    f.extend_from_slice(MAGIC);
    f.extend_from_slice(instance);
    f.extend_from_slice(&time.to_be_bytes());
    f.extend_from_slice(&seq.to_be_bytes());
    f.extend_from_slice(&(topic.len() as u16).to_be_bytes());
    f.extend_from_slice(topic.as_bytes());
    f.extend_from_slice(text.as_bytes());
    let mac = phv_std::hmac_sha256(key, &f);
    f.extend_from_slice(&mac);
    f
}

/// A message received from the store.
#[derive(Debug, PartialEq, Eq)]
enum Decoded {
    /// From another process: topic and text.
    Message(String, String),
    /// From this process.
    Own,
    Invalid,
    /// Signed, but seen before (or older than one seen): published again.
    Replayed,
    /// Signed, but sent more than `MAX_AGE_MS` from this process's clock, or
    /// before this process last subscribed: published again, or the clocks of
    /// the processes differ. `age` is how long before this process's time it
    /// was sent, in ms.
    Clock {
        from: [u8; ID_LEN],
        age: i64,
    },
}

/// What is remembered of a process heard from.
struct Sender {
    /// Its last sequence number.
    seq: u64,
    /// When it was last heard from (ms, this process's clock).
    at: i64,
    /// How far ahead of this process's clock its clock is, at most (its
    /// time minus this process's at receipt, the largest seen: the network
    /// only makes it smaller, and a message published again smaller still).
    offset: i64,
}

/// What a message must be to be new: the last sequence number of each
/// process heard from, and when this process last subscribed.
#[derive(Default)]
struct Seen {
    last: HashMap<[u8; ID_LEN], Sender>,
    /// When this process last subscribed, or subscribed again after losing
    /// its connection (ms). A message sent before that, while it was not
    /// subscribed, was never owed to it (delivery is at most once), so it is
    /// refused: from a process not heard from yet by its time (found by the
    /// 1.1 audit: after a restart a captured message was accepted again), and
    /// from one heard from by its time on this process's clock (found by the
    /// 1.1.1 audit: messages sent while the connection was down, and captured,
    /// were accepted once it was back). A second is allowed for the time a
    /// subscription takes.
    since: i64,
}

/// Why `Seen::fresh` refused a message.
#[derive(Debug, PartialEq)]
enum Stale {
    /// Seen before, or older than one seen.
    Seen,
    /// Sent before this process last subscribed.
    Early,
}

impl Seen {
    fn new(since: i64) -> Seen {
        Seen { last: HashMap::new(), since }
    }

    /// This process subscribed (again) at `now`, or asked to then.
    fn subscribed(&mut self, now: i64) {
        self.since = now;
    }

    /// Whether `seq` from `from`, sent at `time`, is new; remembers it.
    fn fresh(&mut self, from: [u8; ID_LEN], seq: u64, time: i64, now: i64) -> Result<(), Stale> {
        let offset = time - now;
        if let Some(s) = self.last.get(&from) {
            if seq <= s.seq {
                return Err(Stale::Seen);
            }
            if time - s.offset < self.since - 1000 {
                return Err(Stale::Early);
            }
        } else if time < self.since - 1000 {
            return Err(Stale::Early);
        } else if self.last.len() >= MAX_SENDERS {
            // Processes not heard from within the window can be forgotten:
            // their old messages are refused by their time.
            self.last.retain(|_, s| now - s.at <= 2 * MAX_AGE_MS);
            if self.last.len() >= MAX_SENDERS {
                return Err(Stale::Seen);
            }
        }
        let offset = self.last.get(&from).map_or(offset, |s| s.offset.max(offset));
        self.last.insert(from, Sender { seq, at: now, offset });
        Ok(())
    }
}

/// The warning, once for each process, about a message signed with the
/// site's key that was refused for its time.
fn clock_warning(from: &[u8; ID_LEN], age: i64) -> String {
    let when = if age >= 0 { format!("{age} ms ago") } else { format!("{} ms ahead", -age) };
    format!(
        "warning: publish_store: a message from server process {} was refused for its time (sent {when} by this \
         server's clock, or before this process subscribed again); if this repeats, keep the servers' clocks in step",
        hex(&from[..4])
    )
}

fn decode(key: &[u8], instance: &[u8; ID_LEN], now: i64, seen: &mut Seen, f: &[u8]) -> Decoded {
    let head = HEAD;
    if f.len() < head + MAC_LEN || f.len() > MAX_FRAME || &f[..MAGIC.len()] != MAGIC {
        return Decoded::Invalid;
    }
    let (body, mac) = f.split_at(f.len() - MAC_LEN);
    if !phv_std::constant_time_eq(&phv_std::hmac_sha256(key, body), mac) {
        return Decoded::Invalid;
    }
    let from: [u8; ID_LEN] = body[MAGIC.len()..MAGIC.len() + ID_LEN].try_into().expect("ID_LEN bytes");
    if &from == instance {
        return Decoded::Own;
    }
    let at = MAGIC.len() + ID_LEN;
    let time = i64::from_be_bytes(body[at..at + 8].try_into().expect("8 bytes"));
    let seq = u64::from_be_bytes(body[at + 8..at + 16].try_into().expect("8 bytes"));
    if (now - time).abs() > MAX_AGE_MS {
        return Decoded::Clock { from, age: now - time };
    }
    let tlen = u16::from_be_bytes([body[head - 2], body[head - 1]]) as usize;
    if tlen == 0 || tlen > MAX_TOPIC || head + tlen > body.len() {
        return Decoded::Invalid;
    }
    let (Ok(topic), Ok(text)) =
        (std::str::from_utf8(&body[head..head + tlen]), std::str::from_utf8(&body[head + tlen..]))
    else {
        return Decoded::Invalid;
    };
    if text.len() > MAX_TEXT || topic.chars().any(char::is_control) {
        return Decoded::Invalid;
    }
    match seen.fresh(from, seq, time, now) {
        Ok(()) => Decoded::Message(topic.to_string(), text.to_string()),
        Err(Stale::Seen) => Decoded::Replayed,
        Err(Stale::Early) => Decoded::Clock { from, age: now - time },
    }
}

/// Signs and numbers the messages of this process, in the order they are sent
/// (found by the 1.1 audit: numbered by the scripts, two at once could queue
/// them out of order, and the receiver dropped the lower one).
struct Signer {
    key: Vec<u8>,
    instance: [u8; ID_LEN],
    seq: u64,
}

impl Signer {
    fn frame(&mut self, topic: &str, text: &str) -> Vec<u8> {
        self.seq += 1;
        encode(&self.key, &self.instance, now_ms(), self.seq, topic, text)
    }
}

fn send_loop(
    spec: &DatabaseSpec,
    channel: &str,
    rx: Receiver<(String, String)>,
    mut signer: Signer,
    queued: &AtomicUsize,
    metrics: &Metrics,
    stop: &AtomicBool,
) {
    let mut backoff = Duration::from_secs(1);
    let mut down_until: Option<Instant> = None;
    while !stop.load(Ordering::SeqCst) {
        let first = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => {
                // Without messages to send, the store is tried again when the
                // back-off ends, so /ready recovers (found by the 1.1 audit: it
                // stayed 503 until the next message).
                if down_until.is_some_and(|t| Instant::now() >= t) {
                    // A PUBLISH, so a store that takes connections but refuses
                    // messages is not taken for ready (found by the 1.1 audit).
                    let probe = format!("{channel}:probe");
                    match phv_std::redis_publish(spec, &probe, &[Vec::new()], Instant::now() + Duration::from_secs(5)) {
                        Ok(()) => {
                            metrics.health.ok(StoreKind::Publish);
                            down_until = None;
                            backoff = Duration::from_secs(1);
                        }
                        Err(_) => {
                            metrics.health.failed(StoreKind::Publish);
                            down_until = Some(Instant::now() + backoff);
                            backoff = (backoff * 2).min(MAX_BACKOFF);
                        }
                    }
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut batch = vec![first];
        while batch.len() < BATCH {
            match rx.try_recv() {
                Ok(f) => batch.push(f),
                Err(_) => break,
            }
        }
        queued.fetch_sub(batch.iter().map(|(t, x)| t.len() + x.len()).sum(), Ordering::SeqCst);
        // While the store is down, messages are dropped at once instead of piling up.
        if down_until.is_some_and(|t| Instant::now() < t) {
            dropped(metrics, batch.len() as u64);
            continue;
        }
        let frames: Vec<Vec<u8>> = batch.iter().map(|(t, x)| signer.frame(t, x)).collect();
        match phv_std::redis_publish(spec, channel, &frames, Instant::now() + Duration::from_secs(5)) {
            Ok(()) => {
                metrics.publish_sent.fetch_add(batch.len() as u64, Ordering::Relaxed);
                metrics.health.ok(StoreKind::Publish);
                down_until = None;
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                metrics.health.failed(StoreKind::Publish);
                dropped(metrics, batch.len() as u64);
                if down_until.is_none() {
                    eprintln!("warning: the publish_store cannot be used ({e})");
                }
                down_until = Some(Instant::now() + backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

fn receive_loop(
    spec: &DatabaseSpec,
    channel: &str,
    key: &[u8],
    instance: [u8; ID_LEN],
    hub: &Mutex<Hub>,
    metrics: &Metrics,
    stop: &AtomicBool,
) {
    let mut backoff = Duration::from_secs(1);
    let mut warned = false;
    // Kept across connections: a frame seen before a reconnect is still refused.
    let mut seen = Seen::new(now_ms());
    // Processes whose messages were refused for their time, said once each.
    let mut warned_clock: std::collections::HashSet<[u8; ID_LEN]> = Default::default();
    // Waits up to `d`, a second at a time, unless the server stops.
    let pause = |d: Duration| {
        let until = Instant::now() + d;
        while !stop.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100).min(until - Instant::now()));
        }
    };
    while !stop.load(Ordering::SeqCst) {
        // Before asking: nothing sent before this was owed to this process.
        let asked = now_ms();
        let mut sub = match phv_std::redis_subscribe(spec, channel, Instant::now() + Duration::from_secs(10)) {
            Ok(s) => {
                seen.subscribed(asked);
                s
            }
            Err(e) => {
                metrics.health.failed(StoreKind::Publish);
                if !warned {
                    eprintln!("warning: the publish_store cannot be reached ({e}); trying again");
                    warned = true;
                }
                pause(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        metrics.health.ok(StoreKind::Publish);
        if warned {
            eprintln!("the publish_store can be used again");
        }
        // The wait before connecting again starts over only once a connection
        // has lasted (found by the 1.1 audit: a server that took the
        // subscription and then sent garbage was connected to every second).
        let connected = Instant::now();
        let mut quiet_since = Instant::now();
        let mut pinged: Option<Instant> = None;
        let lost = loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            match sub.next(Duration::from_secs(1), MAX_FRAME) {
                Ok(Pushed::Message(f)) => {
                    quiet_since = Instant::now();
                    match decode(key, &instance, now_ms(), &mut seen, &f) {
                        Decoded::Message(topic, text) => {
                            metrics.publish_received.fetch_add(1, Ordering::Relaxed);
                            hub.lock().unwrap_or_else(|e| e.into_inner()).publish(&topic, &text);
                        }
                        Decoded::Own => {}
                        Decoded::Invalid | Decoded::Replayed => {
                            metrics.publish_invalid.fetch_add(1, Ordering::Relaxed);
                        }
                        Decoded::Clock { from, age } => {
                            metrics.publish_clock.fetch_add(1, Ordering::Relaxed);
                            if warned_clock.len() < MAX_SENDERS && warned_clock.insert(from) {
                                eprintln!("{}", clock_warning(&from, age));
                            }
                        }
                    }
                }
                Ok(Pushed::TooLarge) => {
                    quiet_since = Instant::now();
                    metrics.publish_invalid.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Pushed::Pong) => {
                    quiet_since = Instant::now();
                    pinged = None;
                }
                Ok(Pushed::Nothing) => {
                    if pinged.is_some_and(|p| p.elapsed() >= Duration::from_secs(10)) {
                        break "the server did not answer".to_string();
                    }
                    if pinged.is_none() && quiet_since.elapsed() >= PING_EVERY {
                        if let Err(e) = sub.ping() {
                            break e;
                        }
                        pinged = Some(Instant::now());
                    }
                }
                Err(e) => break e,
            }
        };
        metrics.health.failed(StoreKind::Publish);
        eprintln!("warning: the connection to the publish_store was lost ({lost}); connecting again");
        warned = true;
        if connected.elapsed() >= Duration::from_secs(30) {
            backoff = Duration::from_secs(1);
        }
        pause(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_signed_and_checked() {
        let (key, other_key) = (b"k".repeat(32), b"o".repeat(32));
        let (me, them) = ([1u8; ID_LEN], [2u8; ID_LEN]);
        let now = now_ms();
        let mut n = 0;
        // A new frame each time (its sequence number goes up).
        let mut enc = |topic: &str, text: &str| {
            n += 1;
            encode(&key, &them, now, n, topic, text)
        };
        let check = |f: &[u8]| decode(&key, &me, now, &mut Seen::new(0), f);
        let f = enc("lobby", "hello\nworld");
        assert_eq!(check(&f), Decoded::Message("lobby".into(), "hello\nworld".into()));
        assert_eq!(decode(&key, &them, now, &mut Seen::new(0), &f), Decoded::Own);
        assert_eq!(decode(&other_key, &me, now, &mut Seen::new(0), &f), Decoded::Invalid, "another site's key");
        for i in 0..f.len() {
            let mut g = f.clone();
            g[i] ^= 1;
            assert_ne!(check(&g), Decoded::Message("lobby".into(), "hello\nworld".into()), "byte {i} changed");
            assert!(matches!(check(&g), Decoded::Invalid), "byte {i} changed");
        }
        assert_eq!(check(&f[..f.len() - 1]), Decoded::Invalid);
        assert_eq!(check(b""), Decoded::Invalid);
        // Rules of publish(), even when signed: a topic with a control character, too long, empty.
        for topic in ["a\u{7}b", &"t".repeat(MAX_TOPIC + 1), ""] {
            assert_eq!(check(&enc(topic, "x")), Decoded::Invalid, "{topic:?}");
        }
        let big = "x".repeat(MAX_TEXT);
        assert!(matches!(check(&enc("t", &big)), Decoded::Message(..)));
        let too_big = "x".repeat(MAX_TEXT + 1);
        assert_eq!(check(&enc("t", &too_big)), Decoded::Invalid);
        // A topic length that runs past the end.
        let mut g = enc("lobby", "");
        let at = HEAD - 2;
        g[at..at + 2].copy_from_slice(&100u16.to_be_bytes());
        let body_len = g.len() - MAC_LEN;
        let mac = phv_std::hmac_sha256(&key, &g[..body_len]);
        g[body_len..].copy_from_slice(&mac);
        assert_eq!(check(&g), Decoded::Invalid);
    }

    /// A signed message published again is refused: seen before, or too old
    /// (found by the 1.1 audit: a captured message reached visitors again,
    /// also after the processes restarted).
    #[test]
    fn replayed_frames_are_refused() {
        let key = b"k".repeat(32);
        let (me, them) = ([1u8; ID_LEN], [2u8; ID_LEN]);
        let now = now_ms();
        let mut seen = Seen::new(0);
        let f1 = encode(&key, &them, now, 1, "lobby", "one");
        let f2 = encode(&key, &them, now, 2, "lobby", "two");
        assert!(matches!(decode(&key, &me, now, &mut seen, &f1), Decoded::Message(..)));
        assert_eq!(decode(&key, &me, now, &mut seen, &f1), Decoded::Replayed, "the same frame again");
        assert!(matches!(decode(&key, &me, now, &mut seen, &f2), Decoded::Message(..)));
        assert_eq!(decode(&key, &me, now, &mut seen, &f1), Decoded::Replayed, "an older one");
        // Too old, or from too far ahead, even for a receiver that never saw it
        // (refused for its time: counted apart, see `clock_warning`).
        for t in [now - MAX_AGE_MS - 1, now + MAX_AGE_MS + 1] {
            let f = encode(&key, &them, t, 9, "lobby", "x");
            assert_eq!(decode(&key, &me, now, &mut Seen::new(0), &f), Decoded::Clock { from: them, age: now - t });
        }
        // Another process's numbers are its own.
        let other = encode(&key, &[3u8; ID_LEN], now, 1, "lobby", "x");
        assert!(matches!(decode(&key, &me, now, &mut seen, &other), Decoded::Message(..)));
        // A process that subscribed after a message was sent refuses it from a
        // sender it has not heard from (a restart does not open a window).
        let mut restarted = Seen::new(now);
        let before = encode(&key, &[4u8; ID_LEN], now - 5000, 1, "lobby", "x");
        assert!(matches!(decode(&key, &me, now, &mut restarted, &before), Decoded::Clock { .. }));
        let after = encode(&key, &[4u8; ID_LEN], now, 2, "lobby", "x");
        assert!(matches!(decode(&key, &me, now, &mut restarted, &after), Decoded::Message(..)));
    }

    /// After its connection to the store was lost and opened again, a process
    /// refuses the messages a sender it knows sent while it was away, which
    /// it was never owed (found by the 1.1.1 audit: a captured message sent
    /// meanwhile was accepted, its number being higher than the last one
    /// seen), measured by the sender's clock as this process saw it: a sender
    /// whose clock is ahead or behind keeps its messages after the gap.
    #[test]
    fn frames_sent_before_subscribing_again_are_refused() {
        let key = b"k".repeat(32);
        let me = [1u8; ID_LEN];
        let t0 = 1_000_000_000;
        for skew in [0i64, 20_000, -20_000] {
            let them = [9u8; ID_LEN];
            // Subscribed a minute before (a sender not heard from yet is
            // judged by its own clock).
            let mut seen = Seen::new(t0 - 60_000);
            // Heard from before the gap (their clock is `skew` ms off; the
            // network took 5 ms).
            let first = encode(&key, &them, t0 + 1000 + skew, 1, "lobby", "before");
            assert!(matches!(decode(&key, &me, t0 + 1005, &mut seen, &first), Decoded::Message(..)), "{skew}");
            // The connection is lost at t0 + 2 s and back at t0 + 10 s; they
            // sent number 2 at t0 + 3 s, which is published again later.
            let away = encode(&key, &them, t0 + 3000 + skew, 2, "lobby", "away");
            seen.subscribed(t0 + 10_000);
            assert_eq!(
                decode(&key, &me, t0 + 10_500, &mut seen, &away),
                Decoded::Clock { from: them, age: 7500 - skew },
                "{skew}"
            );
            // What they send once it is back is delivered, then not twice.
            let back = encode(&key, &them, t0 + 10_200 + skew, 3, "lobby", "back");
            assert!(matches!(decode(&key, &me, t0 + 10_205, &mut seen, &back), Decoded::Message(..)), "{skew}");
            assert_eq!(decode(&key, &me, t0 + 10_300, &mut seen, &back), Decoded::Replayed);
            // A message sent while it subscribed again (within a second) is kept.
            seen.subscribed(t0 + 20_000);
            let racing = encode(&key, &them, t0 + 19_500 + skew, 4, "lobby", "racing");
            assert!(matches!(decode(&key, &me, t0 + 20_010, &mut seen, &racing), Decoded::Message(..)), "{skew}");
        }
        // The warning names the sender and how far off its time was, never its text.
        let w = clock_warning(&[0xab; ID_LEN], 7500);
        assert!(w.starts_with("warning: publish_store:") && w.contains("abababab") && w.contains("7500 ms ago"), "{w}");
        assert!(clock_warning(&[0; ID_LEN], -61_000).contains("61000 ms ahead"));
    }
}
