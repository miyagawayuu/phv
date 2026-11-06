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
const MAGIC: &[u8; 4] = b"PHV1";
const ID_LEN: usize = 16;
const MAC_LEN: usize = 32;
/// The longest message the store may carry: a topic and a text at their
/// longest, and the frame around them.
pub const MAX_FRAME: usize = MAGIC.len() + ID_LEN + 2 + MAX_TOPIC + MAX_TEXT + MAC_LEN;
/// Waits before connecting again: 1 s, doubled up to this.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A quiet subscribed connection is asked to answer this often.
const PING_EVERY: Duration = Duration::from_secs(30);

pub struct Bus {
    tx: SyncSender<Vec<u8>>,
    queued_bytes: Arc<AtomicUsize>,
    key: Vec<u8>,
    instance: [u8; ID_LEN],
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
            let (spec, channel, queued, metrics, stop) =
                (spec.clone(), channel.clone(), queued_bytes.clone(), metrics.clone(), stop.clone());
            std::thread::Builder::new()
                .name("phv-publish".into())
                .spawn(move || send_loop(&spec, &channel, rx, &queued, &metrics, &stop))
                .expect("cannot start a thread");
        }
        {
            let (key, metrics, stop) = (key.clone(), metrics.clone(), stop.clone());
            std::thread::Builder::new()
                .name("phv-subscribe".into())
                .spawn(move || receive_loop(&spec, &channel, &key, instance, &hub, &metrics, &stop))
                .expect("cannot start a thread");
        }
        Bus { tx, queued_bytes, key, instance, metrics, stop }
    }

    /// Queues a message for the other processes (never waits).
    pub fn send(&self, topic: &str, text: &str) {
        let frame = encode(&self.key, &self.instance, topic, text);
        let len = frame.len();
        if self.queued_bytes.fetch_add(len, Ordering::SeqCst) + len > MAX_QUEUED_BYTES {
            self.queued_bytes.fetch_sub(len, Ordering::SeqCst);
            return self.dropped(1);
        }
        match self.tx.try_send(frame) {
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

/// `PHV1 | instance | topic length (u16) | topic | text | HMAC-SHA256 of all before`.
fn encode(key: &[u8], instance: &[u8; ID_LEN], topic: &str, text: &str) -> Vec<u8> {
    let mut f = Vec::with_capacity(MAGIC.len() + ID_LEN + 2 + topic.len() + text.len() + MAC_LEN);
    f.extend_from_slice(MAGIC);
    f.extend_from_slice(instance);
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
}

fn decode(key: &[u8], instance: &[u8; ID_LEN], f: &[u8]) -> Decoded {
    let head = MAGIC.len() + ID_LEN + 2;
    if f.len() < head + MAC_LEN || f.len() > MAX_FRAME || &f[..MAGIC.len()] != MAGIC {
        return Decoded::Invalid;
    }
    let (body, mac) = f.split_at(f.len() - MAC_LEN);
    if !phv_std::constant_time_eq(&phv_std::hmac_sha256(key, body), mac) {
        return Decoded::Invalid;
    }
    if &body[MAGIC.len()..MAGIC.len() + ID_LEN] == instance {
        return Decoded::Own;
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
    Decoded::Message(topic.to_string(), text.to_string())
}

fn send_loop(
    spec: &DatabaseSpec,
    channel: &str,
    rx: Receiver<Vec<u8>>,
    queued: &AtomicUsize,
    metrics: &Metrics,
    stop: &AtomicBool,
) {
    let mut backoff = Duration::from_secs(1);
    let mut down_until: Option<Instant> = None;
    while !stop.load(Ordering::SeqCst) {
        let first = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut batch = vec![first];
        while batch.len() < BATCH {
            match rx.try_recv() {
                Ok(f) => batch.push(f),
                Err(_) => break,
            }
        }
        queued.fetch_sub(batch.iter().map(Vec::len).sum(), Ordering::SeqCst);
        // While the store is down, messages are dropped at once instead of piling up.
        if down_until.is_some_and(|t| Instant::now() < t) {
            dropped(metrics, batch.len() as u64);
            continue;
        }
        match phv_std::redis_publish(spec, channel, &batch, Instant::now() + Duration::from_secs(5)) {
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
    // Waits up to `d`, a second at a time, unless the server stops.
    let pause = |d: Duration| {
        let until = Instant::now() + d;
        while !stop.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100).min(until - Instant::now()));
        }
    };
    while !stop.load(Ordering::SeqCst) {
        let mut sub = match phv_std::redis_subscribe(spec, channel, Instant::now() + Duration::from_secs(10)) {
            Ok(s) => s,
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
        backoff = Duration::from_secs(1);
        let mut quiet_since = Instant::now();
        let mut pinged: Option<Instant> = None;
        let lost = loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            match sub.next(Duration::from_secs(1), MAX_FRAME) {
                Ok(Pushed::Message(f)) => {
                    quiet_since = Instant::now();
                    match decode(key, &instance, &f) {
                        Decoded::Message(topic, text) => {
                            metrics.publish_received.fetch_add(1, Ordering::Relaxed);
                            hub.lock().unwrap_or_else(|e| e.into_inner()).publish(&topic, &text);
                        }
                        Decoded::Own => {}
                        Decoded::Invalid => {
                            metrics.publish_invalid.fetch_add(1, Ordering::Relaxed);
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
        let f = encode(&key, &them, "lobby", "hello\nworld");
        assert_eq!(decode(&key, &me, &f), Decoded::Message("lobby".into(), "hello\nworld".into()));
        assert_eq!(decode(&key, &them, &f), Decoded::Own);
        assert_eq!(decode(&other_key, &me, &f), Decoded::Invalid, "another site's key");
        for i in 0..f.len() {
            let mut g = f.clone();
            g[i] ^= 1;
            assert_eq!(decode(&key, &me, &g), Decoded::Invalid, "byte {i} changed");
        }
        assert_eq!(decode(&key, &me, &f[..f.len() - 1]), Decoded::Invalid);
        assert_eq!(decode(&key, &me, b""), Decoded::Invalid);
        // Rules of publish(), even when signed: a topic with a control character, too long, empty.
        for topic in ["a\u{7}b", &"t".repeat(MAX_TOPIC + 1), ""] {
            assert_eq!(decode(&key, &me, &encode(&key, &them, topic, "x")), Decoded::Invalid, "{topic:?}");
        }
        let big = "x".repeat(MAX_TEXT);
        assert!(matches!(decode(&key, &me, &encode(&key, &them, "t", &big)), Decoded::Message(..)));
        let too_big = "x".repeat(MAX_TEXT + 1);
        assert_eq!(decode(&key, &me, &encode(&key, &them, "t", &too_big)), Decoded::Invalid);
        // A topic length that runs past the end.
        let mut g = encode(&key, &them, "lobby", "");
        let at = MAGIC.len() + ID_LEN;
        g[at..at + 2].copy_from_slice(&100u16.to_be_bytes());
        let body_len = g.len() - MAC_LEN;
        let mac = phv_std::hmac_sha256(&key, &g[..body_len]);
        g[body_len..].copy_from_slice(&mac);
        assert_eq!(decode(&key, &me, &g), Decoded::Invalid);
    }
}
