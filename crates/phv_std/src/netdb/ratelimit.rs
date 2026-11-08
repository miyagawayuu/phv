// ----- Shared rate limits ------------------------------------------------------

use super::handle::{kind_of, link_of, out_of_memory, read_budget, remaining, tls_init, Kind, Link};
use super::pool::{lock, AcquireError, Pooled, REDIS_POOLS};
use super::query_async::{handle_name, pooled, renew, spec_of, PoolUse};
use super::redis::redis_connect;
use super::sessions::LuaScript;
use super::{MAX_WAIT, R};
use crate::redisclient::{RedisConn, RedisError, Reply};
use crate::{list, s};
use indexmap::IndexMap;
use phv_vm::{DatabaseSpec, Key, Value, Vm, VmError};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Counts a hit in one window: the key's hash holds one field per window
/// length, `<slot> <count>`, so resetting the key is a single DEL. The hash
/// lives as long as its longest window.
static RATE_HIT: LuaScript = LuaScript::new(
    r#"
local k, field, slot, ms = KEYS[1], ARGV[1], ARGV[2], tonumber(ARGV[3])
local cur = redis.call('HGET', k, field)
local count = 0
if cur then
  local s, c = string.match(cur, '^(%d+) (%d+)$')
  if s == slot then count = tonumber(c) end
end
count = count + 1
redis.call('HSET', k, field, slot .. ' ' .. count)
if redis.call('PTTL', k) < ms then redis.call('PEXPIRE', k, ms) end
return count
"#,
);

/// After the rate limit store fails to answer, it is not asked again for this
/// long: hits are counted as when it cannot be reached (see
/// `ratelimit::hit_shared`), without waiting for it on every request.
const RATE_STORE_BACKOFF: Duration = Duration::from_secs(5);

/// Rate limit stores (by URL) that failed recently: when, and why.
static RATE_STORE_DOWN: LazyLock<Mutex<HashMap<String, (Instant, String)>>> = LazyLock::new(Default::default);

/// Runs `op` on a connection to the rate limit store `spec`, unless the store
/// failed less than `RATE_STORE_BACKOFF` ago (then its error is returned at once).
fn with_rate_store<T>(
    spec: &DatabaseSpec,
    until: Instant,
    op: impl FnOnce(&mut RedisConn) -> Result<T, Option<RedisError>>,
) -> Result<T, String> {
    let url = spec.url.clone().unwrap_or_default();
    if let Some((at, why)) = lock(&RATE_STORE_DOWN).get(&url) {
        if at.elapsed() < RATE_STORE_BACKOFF {
            return Err(why.clone());
        }
    }
    let down = |why: &str| {
        lock(&RATE_STORE_DOWN).insert(url.clone(), (Instant::now(), why.to_string()));
    };
    let mut conn = match redis_pooled(spec, PoolUse::RateLimits, until) {
        Ok(c) => c,
        Err((why, unreachable)) => {
            if unreachable {
                down(&why);
            }
            return Err(why);
        }
    };
    match op(conn.get()) {
        Ok(v) => {
            let mut failed = lock(&RATE_STORE_DOWN);
            if !failed.is_empty() {
                failed.remove(&url);
            }
            Ok(v)
        }
        Err(e) => {
            conn.broken = true;
            // A server that answered (with an error, or an unexpected reply) is up.
            let answered = matches!(e, Some(RedisError::Server(_)) | None);
            let why = redis_failure(e);
            if !answered {
                down(&why);
            }
            Err(why)
        }
    }
}

/// Counts a hit in a fixed window on the Redis server `spec` (see
/// `ratelimit::hit_shared`). Every server process using the same server shares
/// the counters. Returns whether the hit is allowed and the seconds until the
/// window ends.
pub(crate) fn redis_hit(
    spec: &DatabaseSpec,
    key: &str,
    max: u32,
    window: Duration,
    until: Instant,
) -> Result<(bool, u64), String> {
    let ms = window.as_millis().max(1) as u64;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let slot = now / ms;
    let rkey = redis_rate_key(key);
    let (field, slot_s) = (ms.to_string(), slot.to_string());
    // The hash outlives the window a little (clock skew between servers).
    let keep = ((slot + 1) * ms - now + 1000).to_string();
    let n = with_rate_store(spec, until, |c| {
        let args: [&[u8]; 5] = [b"1", rkey.as_bytes(), field.as_bytes(), slot_s.as_bytes(), keep.as_bytes()];
        match RATE_HIT.run(c, &args, until, 1024) {
            Ok(Reply::Int(n)) => Ok(n),
            other => Err(other.err()),
        }
    })?;
    Ok((n <= i64::from(max), ((slot + 1) * ms - now) / 1000 + 1))
}

/// Forgets every window of `key` on the Redis server `spec`.
pub(crate) fn redis_reset(spec: &DatabaseSpec, key: &str, until: Instant) -> Result<(), String> {
    let rkey = redis_rate_key(key);
    with_rate_store(spec, until, |c| c.command(&[b"DEL", rkey.as_bytes()], until, 1024).map(|_| ()).map_err(Some))
}

/// The hash of a rate limit key: hashed, so its length is bounded.
fn redis_rate_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("phv:ratelimit:{:x}", Sha256::digest(key.as_bytes()))
}

/// A connection to the Redis server `spec`, or why there is none and whether
/// that is because the server could not be reached.
pub(super) fn redis_pooled(
    spec: &DatabaseSpec,
    use_: PoolUse,
    until: Instant,
) -> Result<Pooled<RedisConn>, (String, bool)> {
    let Some(url) = spec.url.clone() else {
        let var = spec.url_env.as_deref().unwrap_or("?");
        return Err((format!("the environment variable {var} is not set"), false));
    };
    if kind_of(&url) != Kind::Redis {
        return Err(("it is not a Redis server".into(), false));
    }
    tls_init();
    match pooled(&REDIS_POOLS, spec, &url, use_, until, |t| redis_connect(&url, spec, t)) {
        Ok(c) => Ok(c),
        Err(AcquireError::Busy) => Err(("all its connections are in use".into(), false)),
        Err(AcquireError::Connect(e)) => Err((e, true)),
    }
}

pub(super) fn redis_conn(spec: &DatabaseSpec, use_: PoolUse, until: Instant) -> Result<Pooled<RedisConn>, String> {
    redis_pooled(spec, use_, until).map_err(|(why, _)| why)
}

pub(super) fn redis_failure(e: Option<crate::redisclient::RedisError>) -> String {
    match e {
        Some(crate::redisclient::RedisError::Server(m)) => m,
        Some(_) => "the server did not answer".into(),
        None => "the server sent an invalid reply".into(),
    }
}

/// Commands scripts may run, with their argument counts.
/// Keys PHV keeps in Redis itself (sessions, rate limits): scripts may not
/// read or write them, which would let a request forge a session.
pub(super) const PHV_KEYS: &str = "phv:";

pub(super) fn phv_key_error(key: &str) -> String {
    let shown: String = key.chars().take(40).collect();
    format!("Redis keys starting with `phv:` belong to PHV (sessions, rate limits, jobs): \"{shown}\"")
}

pub(super) const REDIS_COMMANDS: &[(&str, usize, usize)] = &[
    ("GET", 1, 1),
    ("SET", 2, 4),
    ("DEL", 1, 1),
    ("EXISTS", 1, 1),
    ("INCRBY", 2, 2),
    ("EXPIRE", 2, 2),
    ("TTL", 1, 1),
    ("HGET", 2, 2),
    ("HSET", 3, 3),
    ("HDEL", 2, 2),
    ("HGETALL", 1, 1),
    ("RPUSH", 2, 2),
    ("LPOP", 1, 1),
    ("LRANGE", 3, 3),
];

pub(crate) fn redis_value(v: Reply) -> Value {
    match v {
        Reply::Nil => Value::Null,
        Reply::Int(i) => Value::Int(i),
        Reply::Bulk(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
        Reply::Simple(s) if s == "OK" => Value::Bool(true),
        Reply::Simple(s) => Value::str(s),
        Reply::Array(items) => Value::list(items.into_iter().map(redis_value).collect()),
    }
}

/// The connection of the script's Redis handle `h`, ready for a command.
/// One on which bytes arrived while the script was not using it (a reply the
/// server sent late, which the next command would read as its own: found by
/// the 1.2.1 audit), or that the server closed meanwhile, is out of step;
/// nothing has been sent on it since, so another connection from the pool
/// takes its place (the errors of `Redis::connect` when none can be had).
fn ready<'a>(
    vm: &mut Vm,
    h: &Value,
    c: &'a mut Pooled<RedisConn>,
    until: Instant,
) -> Result<&'a mut RedisConn, VmError> {
    if c.conn().is_some_and(RedisConn::in_step) {
        return Ok(c.get());
    }
    let name = handle_name(vm, h)?;
    let (spec, url) = spec_of(vm, &name)?;
    match renew(c, until, |t| redis_connect(&url, &spec, t)) {
        Ok(()) => Ok(c.get()),
        Err(AcquireError::Busy) => {
            vm.check_limits()?;
            Err(vm.throw(
                "DatabaseError",
                format!("all {} connections to database \"{name}\" are in use (raise pool_size?)", spec.pool_size),
            ))
        }
        Err(AcquireError::Connect(e)) => {
            Err(vm.throw("DatabaseError", format!("cannot connect to database \"{name}\": {e}")))
        }
    }
}

/// `__redis(handle, command, args)`: runs one allowed command.
pub fn redis_command(vm: &mut Vm, a: &[Value]) -> R {
    let link = link_of(vm, &a[0])?;
    let mut link = link.borrow_mut();
    let Link::Redis(c) = &mut *link else { unreachable!("database handle used as Redis") };
    let name = s(&a[1]);
    let args = list(&a[2]);
    let allowed = REDIS_COMMANDS.iter().any(|&(n, lo, hi)| n == name && (lo..=hi).contains(&args.len()));
    if !allowed {
        return Err(VmError::Fatal(format!("__redis: unexpected command {name}")));
    }
    if let Some(key) = args.first().map(s).filter(|k| k.starts_with(PHV_KEYS)) {
        return Err(vm.throw("ValueError", phv_key_error(key)));
    }
    let left = remaining(vm)?;
    let until = Instant::now() + left.unwrap_or(MAX_WAIT);
    let mut argv: Vec<&[u8]> = vec![name.as_bytes()];
    argv.extend(args.iter().map(|a| s(a).as_bytes()));
    let budget = read_budget(vm);
    let conn = ready(vm, &a[0], c, until)?;
    let v = match conn.command(&argv, until, budget) {
        Ok(v) => v,
        Err(RedisError::Server(m)) => return Err(vm.throw("DatabaseError", m)),
        Err(RedisError::TooLarge) => {
            c.broken = true;
            return Err(out_of_memory(vm));
        }
        Err(RedisError::Broken(m)) => {
            c.broken = true;
            vm.check_limits()?;
            return Err(vm.throw("DatabaseError", m));
        }
    };
    let out = redis_command_value(name, v).map_err(|m| vm.throw("DatabaseError", m))?;
    vm.check_memory()?;
    Ok(out)
}

/// The value of the reply `v` to the command `name`; an error for a reply
/// of a shape the command never has.
fn redis_command_value(name: &str, v: Reply) -> Result<Value, String> {
    match (name, v) {
        // RESP2 answers HGETALL with a flat list of fields and values: a
        // field without a value is not a hash (found by the 1.2.1 audit: it
        // was dropped).
        ("HGETALL", Reply::Array(items)) => {
            if items.len() % 2 != 0 {
                return Err("the server sent an unexpected reply".into());
            }
            let mut m = IndexMap::with_capacity(items.len() / 2);
            let mut it = items.into_iter().map(redis_value);
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                m.insert(Key::Str(Rc::new(k.to_display_string())), v);
            }
            Ok(Value::map(m))
        }
        (_, v) => Ok(redis_value(v)),
    }
}

/// `__redis_pipeline(handle, commands)`: sends the allowed commands (each a
/// list of its name and arguments) in one pipeline and returns their replies
/// in order. An error reply is a `DatabaseError` value in its position; a
/// connection error throws for the whole call. Nothing is sent when a command
/// is not allowed, a key is PHV's, or the pipeline is past its caps.
pub fn redis_pipeline(vm: &mut Vm, a: &[Value]) -> R {
    use crate::redisclient::{MAX_PIPELINE_BYTES, MAX_PIPELINE_COMMANDS};
    let link = link_of(vm, &a[0])?;
    let mut link = link.borrow_mut();
    let Link::Redis(c) = &mut *link else { unreachable!("database handle used as Redis") };
    let commands = list(&a[1]);
    if commands.len() > MAX_PIPELINE_COMMANDS {
        return Err(vm.throw("ValueError", format!("a Redis pipeline sends at most {MAX_PIPELINE_COMMANDS} commands")));
    }
    let mut argv: Vec<Vec<&[u8]>> = Vec::with_capacity(commands.len());
    let mut names = Vec::with_capacity(commands.len());
    let mut bytes = 0usize;
    for command in commands.iter() {
        let parts = list(command);
        let name = parts.first().map(s).unwrap_or("");
        let args = parts.get(1..).unwrap_or(&[]);
        let allowed = REDIS_COMMANDS.iter().any(|&(n, lo, hi)| n == name && (lo..=hi).contains(&args.len()));
        if !allowed {
            return Err(VmError::Fatal(format!("__redis_pipeline: unexpected command {name}")));
        }
        if let Some(key) = args.first().map(s).filter(|k| k.starts_with(PHV_KEYS)) {
            return Err(vm.throw("ValueError", phv_key_error(key)));
        }
        let encoded: Vec<&[u8]> = parts.iter().map(|p| s(p).as_bytes()).collect();
        bytes = bytes.saturating_add(RedisConn::encoded_len(&encoded));
        if bytes > MAX_PIPELINE_BYTES {
            return Err(vm.throw(
                "ValueError",
                format!("a Redis pipeline sends at most {} MB of commands", MAX_PIPELINE_BYTES >> 20),
            ));
        }
        names.push(name);
        argv.push(encoded);
    }
    let left = remaining(vm)?;
    let until = Instant::now() + left.unwrap_or(MAX_WAIT);
    let budget = read_budget(vm);
    let conn = ready(vm, &a[0], c, until)?;
    let replies = match conn.pipeline(&argv, until, budget) {
        Ok(r) => r,
        Err(RedisError::Server(m)) => return Err(vm.throw("DatabaseError", m)),
        Err(RedisError::TooLarge) => {
            c.broken = true;
            return Err(out_of_memory(vm));
        }
        Err(RedisError::Broken(m)) => {
            c.broken = true;
            vm.check_limits()?;
            return Err(vm.throw("DatabaseError", m));
        }
    };
    let mut out = Vec::with_capacity(replies.len());
    for (name, reply) in names.into_iter().zip(replies) {
        out.push(match reply {
            Ok(v) => redis_command_value(name, v).map_err(|m| vm.throw("DatabaseError", m))?,
            // The exception value as `__redis` would throw it, in the reply's place.
            Err(m) => match vm.throw("DatabaseError", m) {
                VmError::Exception(e) => e.value,
                other => return Err(other),
            },
        });
    }
    vm.check_memory()?;
    Ok(Value::list(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HGETALL` answers fields and values in pairs: a reply with a field
    /// and no value is not a hash and is refused (found by the 1.2.1 audit:
    /// the last field was dropped).
    #[test]
    fn an_hgetall_reply_with_a_field_without_a_value_is_refused() {
        let bulk = |s: &str| Reply::Bulk(s.as_bytes().to_vec());
        let v = redis_command_value("HGETALL", Reply::Array(vec![bulk("name"), bulk("Ann")])).unwrap();
        assert!(matches!(&v, Value::Map(m) if m.len() == 1));
        let odd = Reply::Array(vec![bulk("name"), bulk("Ann"), bulk("age")]);
        assert_eq!(redis_command_value("HGETALL", odd).err().as_deref(), Some("the server sent an unexpected reply"));
        let empty = redis_command_value("HGETALL", Reply::Array(Vec::new())).unwrap();
        assert!(matches!(&empty, Value::Map(m) if m.is_empty()));
        // Other commands keep an array as a list, whatever its length.
        let list = redis_command_value("LRANGE", Reply::Array(vec![bulk("x")])).unwrap();
        assert!(matches!(&list, Value::List(l) if l.len() == 1));
    }
}
