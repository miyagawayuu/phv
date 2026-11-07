// ----- publish() between server processes (`[server] publish_store`) ----------

use super::handle::{kind_of, tls_init, Kind};
use super::query_async::PoolUse;
use super::ratelimit::{redis_failure, redis_pooled};
use crate::redisclient::{RedisConn, Reply};
use phv_vm::DatabaseSpec;
use std::time::{Duration, Instant};

/// Sends each message to `channel` with PUBLISH, in one pipeline, on the
/// connection the server keeps for it. Waits until `until` at most.
pub fn redis_publish(spec: &DatabaseSpec, channel: &str, messages: &[Vec<u8>], until: Instant) -> Result<(), String> {
    let mut conn = redis_pooled(spec, PoolUse::Publish, until).map_err(|(why, _)| why)?;
    let c = conn.get();
    let sent = (|| {
        for m in messages {
            c.send(&[b"PUBLISH", channel.as_bytes(), m], until)?;
        }
        // Each reply is a count of subscribers (`:12\r\n`). What is read ahead
        // belongs to the replies still to come, so each one's budget covers
        // them too (found by the 1.1 audit: past 205 messages with ten
        // subscribers, the read-ahead passed a fixed 1 KB and the batch failed).
        for left in (0..messages.len()).rev() {
            c.next_reply(until, 1024 + 32 * left)?;
        }
        Ok(())
    })();
    match sent {
        Ok(()) => Ok(()),
        Err(e) => {
            conn.broken = true;
            Err(redis_failure(Some(e)))
        }
    }
}

/// A connection subscribed to one channel of a Redis server: it receives the
/// messages published there.
pub struct RedisSubscriber {
    conn: RedisConn,
}

/// What a subscribed connection received.
pub enum Pushed {
    Message(Vec<u8>),
    /// A message longer than allowed, thrown away as it arrived.
    TooLarge,
    /// The answer to `ping()`.
    Pong,
    /// Nothing within the time asked for.
    Nothing,
}

/// Opens a connection of its own to `spec` (not from a pool) and subscribes
/// it to `channel`.
pub fn redis_subscribe(spec: &DatabaseSpec, channel: &str, until: Instant) -> Result<RedisSubscriber, String> {
    let Some(url) = spec.url.clone() else {
        let var = spec.url_env.as_deref().unwrap_or("?");
        return Err(format!("the environment variable {var} is not set"));
    };
    if kind_of(&url) != Kind::Redis {
        return Err("it is not a Redis server".into());
    }
    tls_init();
    let mut conn = RedisConn::connect(&url, spec.tls, until)?;
    conn.send(&[b"SUBSCRIBE", channel.as_bytes()], until).map_err(|e| redis_failure(Some(e)))?;
    match conn.next_reply(until, 1024).map_err(|e| redis_failure(Some(e)))? {
        Reply::Array(items) if matches!(items.first(), Some(Reply::Bulk(k)) if k == b"subscribe") => {}
        _ => return Err("the server sent an invalid reply".into()),
    }
    Ok(RedisSubscriber { conn })
}

impl RedisSubscriber {
    /// The next message, waiting up to `wait` for one to start; a message
    /// longer than `max` bytes breaks the connection without being read whole.
    pub fn next(&mut self, wait: Duration, max: usize) -> Result<Pushed, String> {
        use crate::redisclient::Push;
        let fail = |e| redis_failure(Some(e));
        if !self.conn.wait_data(Instant::now() + wait, max + 1024).map_err(fail)? {
            return Ok(Pushed::Nothing);
        }
        // Once a message has begun, the rest of it is due soon.
        match self.conn.next_push(Instant::now() + Duration::from_secs(10), max).map_err(fail)? {
            Push::Message(m) => Ok(Pushed::Message(m)),
            Push::TooLarge => Ok(Pushed::TooLarge),
            Push::Other(kind) if kind == b"pong" => Ok(Pushed::Pong),
            Push::Other(_) => Err("the server sent an unexpected reply".into()),
        }
    }

    /// Asks the server to answer (`Pushed::Pong`), to find a dead connection.
    pub fn ping(&mut self) -> Result<(), String> {
        self.conn.send(&[b"PING"], Instant::now() + Duration::from_secs(10)).map_err(|e| redis_failure(Some(e)))
    }
}
