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
use std::time::{Duration, Instant};

/// Counts a hit in one window: the key's hash holds one field per window
/// length, `<slot> <count>`, so resetting the key is a single DEL. The hash
/// lives as long as its longest window.
///
/// The window's number is taken from the Redis server's clock, not the
/// caller's (found by the 21st assessment: two servers whose clocks were 20
/// seconds apart numbered the window differently around each of its ends,
/// each started the count again after the other, and every hit was allowed
/// meanwhile). A server of 1.3.2 beside this one still numbers by its own
/// clock and writes the same field: the two share the count while its clock
/// agrees with the Redis server's.
///
/// The keys of a site are bounded (ARGV[3], `RATE_MAX_KEYS`, as the table
/// in the process; found by the same assessment: a key lived for its
/// whole window, up to a day, and nothing bounded their number, so a visitor
/// of a page that limits by a name the visitor types filled the server). Three
/// sorted sets under the site's prefix (KEYS[2]) hold the key's digest: `exp`
/// by when its hash expires (all of them: its size is the count), and `open`
/// (not at its limit) by when its window got its first hit, or `full` (at
/// its limit) by when it reached it, both in microseconds. A
/// new key that finds the site at its bound takes the place of the key that
/// has gone longest without reaching its limit. A key at its limit is never
/// given up before its hash expires: when every key of the site is at its
/// limit the script counts nothing and answers -1, and the caller counts the
/// hit in this process's own table (`ratelimit::hit`), which is bounded and
/// refuses no key, until the site has room again (found by the re-attack of
/// the 21st assessment's fixes: the newest limited key went, so a visitor
/// who had filled the site with limited keys reached the limit on a key,
/// sent one hit with a fresh key and had the limit back: 205 guesses in 247
/// requests against 5 a day; refusing the new key instead would lock every
/// new visitor out for the window). Entries whose hash has expired are
/// dropped a few per hit; one whose hash still lives (a 1.3.2 server
/// extended it) is kept with its new expiry. Keys only 1.3.2 servers wrote
/// are not in the sets: they are counted on in their hash, and join the sets
/// when there is room.
///
/// A key at its limit in one window stays in `full` while its hash holds
/// other windows (one of them may be the limited one).
///
/// Each hit is a dozen commands on one key and the three sets, each O(1) or
/// O(log n) in the site's keys, and the script runs as one step.
///
/// KEYS: the key's hash, the site's prefix. ARGV: the window in ms, the
/// limit, the bound, the key's digest. Returns the count (-1: not counted,
/// the site is full of keys at their limit) and the ms left of the window.
static RATE_HIT: LuaScript = LuaScript::new(
    r#"
local k, p = KEYS[1], KEYS[2]
local field, ms, max, cap, id = ARGV[1], tonumber(ARGV[1]), tonumber(ARGV[2]), tonumber(ARGV[3]), ARGV[4]
-- Redis before 5 replicates scripts as they are: TIME then needs this.
if redis.replicate_commands then redis.replicate_commands() end
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
local stamp = tonumber(t[1]) * 1000000 + tonumber(t[2])
local n = math.floor(now / ms)
local slot, left = string.format('%.0f', n), (n + 1) * ms - now
local exp, open, full = p .. 'exp', p .. 'open', p .. 'full'
local function forget(x)
  redis.call('ZREM', exp, x)
  redis.call('ZREM', open, x)
  redis.call('ZREM', full, x)
end
for _, x in ipairs(redis.call('ZRANGEBYSCORE', exp, '-inf', now, 'LIMIT', 0, 16)) do
  local ttl = redis.call('PTTL', 'phv:ratelimit:' .. x)
  if ttl > 0 then redis.call('ZADD', exp, now + ttl, x) else forget(x) end
end
local room = true
if not redis.call('ZSCORE', exp, id) then
  local tries = 0
  while redis.call('ZCARD', exp) >= cap and tries < 8 do
    local v = redis.call('ZRANGE', open, 0, 0)[1]
    if not v then break end
    redis.call('DEL', 'phv:ratelimit:' .. v)
    forget(v)
    tries = tries + 1
  end
  if redis.call('ZCARD', exp) >= cap then
    -- No key may go. A hash a 1.3.2 server wrote is counted on, outside the sets.
    if redis.call('EXISTS', k) == 0 then return {-1, left} end
    room = false
  end
end
local cur = redis.call('HGET', k, field)
local count = 0
if cur then
  local s, c = string.match(cur, '^(%d+) (%d+)$')
  if s == slot then count = tonumber(c) end
end
count = count + 1
redis.call('HSET', k, field, slot .. ' ' .. count)
-- The hash outlives the window a little (a 1.3.2 server's clock may be behind).
local ttl = redis.call('PTTL', k)
if ttl < left + 1000 then
  ttl = left + 1000
  redis.call('PEXPIRE', k, ttl)
end
if not room then return {count, left} end
redis.call('ZADD', exp, now + ttl, id)
if count >= max then
  redis.call('ZREM', open, id)
  if count == max then redis.call('ZADD', full, stamp, id) else redis.call('ZADD', full, 'NX', stamp, id) end
elseif not redis.call('ZSCORE', full, id) or redis.call('HLEN', k) == 1 then
  redis.call('ZREM', full, id)
  if count == 1 then redis.call('ZADD', open, stamp, id) else redis.call('ZADD', open, 'NX', stamp, id) end
end
return {count, left}
"#,
);

/// Forgets every window of a key, and the key in the site's sets (see
/// `RATE_HIT`). KEYS: the key's hash, the site's prefix. ARGV: the digest.
static RATE_RESET: LuaScript = LuaScript::new(
    r#"
redis.call('DEL', KEYS[1])
redis.call('ZREM', KEYS[2] .. 'exp', ARGV[1])
redis.call('ZREM', KEYS[2] .. 'open', ARGV[1])
redis.call('ZREM', KEYS[2] .. 'full', ARGV[1])
return 1
"#,
);

/// After the rate limit store fails to answer, it is not asked again for this
/// long: hits are counted as when it cannot be reached (see
/// `ratelimit::hit_shared`), without waiting for it on every request.
const RATE_STORE_BACKOFF: Duration = Duration::from_secs(5);

/// A wait for the store that ended at its caller's deadline before this long
/// says nothing about the store: the caller was a script at the end of its
/// own time limit (found by the 21st assessment: such a request marked the
/// store as failed, and for the next `RATE_STORE_BACKOFF` every `rate_limit()`
/// of the site refused at once). The store gets `lib.rs`'s `SHARED_WAIT` (2
/// seconds) from a script with time left.
///
/// A store that hangs must still be found out by scripts that never have
/// that long (`--time-limit 1`): `RATE_STORE_CUT_WAITS` such waits in a row,
/// with no answer of the store between them and this long from the start of
/// the first to the end of the last, mark it as failed (found by the
/// re-attack of the fixes: with a time limit of a second no wait ever marked
/// a hung store, and every request waited out its limit).
const RATE_STORE_PATIENCE: Duration = Duration::from_secs(1);
const RATE_STORE_CUT_WAITS: u32 = 3;

/// Per store (by URL): the waits cut short by their caller's deadline since
/// the store last answered, as when the first began and how many.
static RATE_STORE_CUT: LazyLock<Mutex<HashMap<String, (Instant, u32)>>> = LazyLock::new(Default::default);

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
    let started = Instant::now();
    let down = |why: &str| {
        // Cut short by the caller's own deadline: not the store's failure.
        let now = Instant::now();
        let at_deadline = now + Duration::from_millis(50) >= until;
        if at_deadline && until.saturating_duration_since(started) < RATE_STORE_PATIENCE {
            let mut cut = lock(&RATE_STORE_CUT);
            let (first, waits) = cut.entry(url.clone()).or_insert((started, 0));
            *waits += 1;
            if *waits < RATE_STORE_CUT_WAITS || now.saturating_duration_since(*first) < RATE_STORE_PATIENCE {
                return;
            }
        }
        lock(&RATE_STORE_DOWN).insert(url.clone(), (now, why.to_string()));
    };
    // The store answered (also with an error): what was cut short before says nothing.
    let answered = || {
        let mut cut = lock(&RATE_STORE_CUT);
        if !cut.is_empty() {
            cut.remove(&url);
        }
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
            answered();
            let mut failed = lock(&RATE_STORE_DOWN);
            if !failed.is_empty() {
                failed.remove(&url);
            }
            Ok(v)
        }
        Err(e) => {
            conn.broken = true;
            // A server that answered (with an error, or an unexpected reply) is up.
            let up = matches!(e, Some(RedisError::Server(_)) | None);
            let why = redis_failure(e);
            if up {
                answered();
            } else {
                down(&why);
            }
            Err(why)
        }
    }
}

/// The keys of a site the shared store holds at most: as many as the table
/// in the process holds windows (`ratelimit::MAX_KEYS`).
const RATE_MAX_KEYS: usize = 100_000;

/// The prefix of the sets that bound a site's rate limit keys (see
/// `RATE_HIT`), from a key as `ratelimit::shared_key` makes it: the site's
/// namespace (hex digits) comes before a NUL; a key of a site without one
/// has no NUL or starts with it, and such sites are bounded together (as
/// they share their sessions).
fn redis_rate_prefix(key: &str) -> String {
    match key.split_once('\0') {
        Some((n, _)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_hexdigit()) => format!("phv:ratelimits:{n}:"),
        _ => "phv:ratelimits:unnamed:".to_string(),
    }
}

/// Counts a hit in a fixed window on the Redis server `spec` (see
/// `ratelimit::hit_shared`). Every server process using the same server shares
/// the counters, and the server's clock numbers the window. The keys of the
/// site (named in `key`) are bounded together. Returns whether the hit is
/// allowed and the seconds until the window ends.
pub(crate) fn redis_hit(
    spec: &DatabaseSpec,
    key: &str,
    max: u32,
    window: Duration,
    until: Instant,
) -> Result<(bool, u64), String> {
    redis_hit_within(spec, key, max, window, until, RATE_MAX_KEYS)
}

/// `redis_hit` with the bound on the site's keys given (for the tests).
fn redis_hit_within(
    spec: &DatabaseSpec,
    key: &str,
    max: u32,
    window: Duration,
    until: Instant,
    cap: usize,
) -> Result<(bool, u64), String> {
    let ms = (window.as_millis().max(1) as u64).to_string();
    let (id, prefix) = (redis_rate_id(key), redis_rate_prefix(key));
    let rkey = format!("{RATE_KEY}{id}");
    let (max_s, cap_s) = (max.to_string(), cap.to_string());
    let (n, left) = with_rate_store(spec, until, |c| {
        let args: [&[u8]; 7] = [
            b"2",
            rkey.as_bytes(),
            prefix.as_bytes(),
            ms.as_bytes(),
            max_s.as_bytes(),
            cap_s.as_bytes(),
            id.as_bytes(),
        ];
        match RATE_HIT.run(c, &args, until, 1024) {
            Ok(Reply::Array(items)) => match items.as_slice() {
                [Reply::Int(n), Reply::Int(left)] => Ok((*n, *left)),
                _ => Err(None),
            },
            other => Err(other.err()),
        }
    })?;
    if n < 0 {
        // The site's keys are all at their limit: counted here (see `RATE_HIT`).
        return Ok((crate::ratelimit::hit(key, max, window), crate::ratelimit::retry_after(key, window)));
    }
    Ok((n <= i64::from(max), left.max(0) as u64 / 1000 + 1))
}

/// Forgets every window of `key` on the Redis server `spec`.
pub(crate) fn redis_reset(spec: &DatabaseSpec, key: &str, until: Instant) -> Result<(), String> {
    let (id, prefix) = (redis_rate_id(key), redis_rate_prefix(key));
    let rkey = format!("{RATE_KEY}{id}");
    // Also what this process counted while the site was full.
    crate::ratelimit::reset(key);
    with_rate_store(spec, until, |c| {
        let args: [&[u8]; 4] = [b"2", rkey.as_bytes(), prefix.as_bytes(), id.as_bytes()];
        RATE_RESET.run(c, &args, until, 1024).map(|_| ()).map_err(Some)
    })
}

/// The hashes of rate limit keys: `phv:ratelimit:<digest>`.
const RATE_KEY: &str = "phv:ratelimit:";

/// The digest that names a rate limit key: hashed, so its length is bounded.
fn redis_rate_id(key: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(key.as_bytes()))
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

    /// The Redis server of the database tests, when set.
    fn test_redis() -> Option<DatabaseSpec> {
        let url = std::env::var("PHV_TEST_REDIS_URL").ok().filter(|u| !u.is_empty());
        if url.is_none() {
            eprintln!("skipped: PHV_TEST_REDIS_URL is not set");
        }
        Some(DatabaseSpec { url: Some(url?), url_env: None, tls: None, pool_size: 4 })
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    /// Runs a command on the test server: the items of its reply as text
    /// (one for a reply that is not a list; "nil" for none).
    fn list(spec: &DatabaseSpec, args: &[&str]) -> Vec<String> {
        fn text(r: Reply) -> String {
            match r {
                Reply::Nil => "nil".into(),
                Reply::Int(n) => n.to_string(),
                Reply::Bulk(b) => String::from_utf8_lossy(&b).into_owned(),
                Reply::Simple(s) => s,
                Reply::Array(_) => "a list".into(),
            }
        }
        let mut c = redis_conn(spec, PoolUse::RateLimits, soon()).unwrap();
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        match c.get().command(&argv, soon(), 1 << 20).ok().expect("the Redis command") {
            Reply::Array(items) => items.into_iter().map(text).collect(),
            one => vec![text(one)],
        }
    }

    fn raw(spec: &DatabaseSpec, args: &[&str]) -> String {
        list(spec, args).remove(0)
    }

    fn int(r: String) -> i64 {
        r.parse().unwrap_or_else(|_| panic!("not a number: {r}"))
    }

    /// The time of the Redis server, in ms.
    fn server_ms(spec: &DatabaseSpec) -> u64 {
        let t = list(spec, &["TIME"]);
        t[0].parse::<u64>().unwrap() * 1000 + t[1].parse::<u64>().unwrap() / 1000
    }

    /// A key of a site of this run (as `ratelimit::shared_key` makes it), its hash and the site's sets.
    fn site_key(tag: u8, key: &str) -> (String, String, String) {
        let ns = crate::testing::namespace(tag);
        let shared = format!("{ns}\0app:{key}");
        (format!("{RATE_KEY}{}", redis_rate_id(&shared)), format!("phv:ratelimits:{ns}:"), shared)
    }

    #[test]
    fn the_sets_of_a_site_are_named_by_its_namespace() {
        assert_eq!(redis_rate_prefix("0123abcd\0app:login:x"), "phv:ratelimits:0123abcd:");
        // A site without a namespace: no NUL, or one first.
        assert_eq!(redis_rate_prefix("app:login:x"), "phv:ratelimits:unnamed:");
        assert_eq!(redis_rate_prefix("\0app:a\0b"), "phv:ratelimits:unnamed:");
        assert_eq!(redis_rate_prefix("not hex\0app:x"), "phv:ratelimits:unnamed:");
    }

    /// The window is numbered by the Redis server's clock: what a 1.3.2
    /// server whose clock runs ahead wrote for the next window does not start
    /// the count again, as it did when each server numbered the window by its
    /// own clock (found by the 21st assessment: two servers 20 seconds apart
    /// allowed 193 of 193 hits of a limit of 3 a minute); and the seconds
    /// to wait are those left of the server's window.
    #[test]
    fn hits_share_the_window_of_the_stores_clock() {
        let Some(spec) = test_redis() else { return };
        let window = Duration::from_secs(60);
        let (rkey, _, key) = site_key(0x61, &format!("clock-{}", crate::random_hex(6)));
        // Not across the end of a window.
        while server_ms(&spec) % 60_000 > 50_000 {
            std::thread::sleep(Duration::from_millis(500));
        }
        let slot = server_ms(&spec) / 60_000;
        for i in 1..=3 {
            let (allowed, wait) = redis_hit(&spec, &key, 3, window, soon()).unwrap();
            assert!(allowed, "hit {i}");
            let left = 60 - server_ms(&spec) % 60_000 / 1000;
            assert!(wait >= left && wait <= left + 2, "{wait} s to wait, {left} s left of the window");
        }
        // The window's number is the store's, whatever this process's clock says.
        assert_eq!(raw(&spec, &["HGET", &rkey, "60000"]), format!("{slot} 3"));
        let (allowed, wait) = redis_hit(&spec, &key, 3, window, soon()).unwrap();
        assert!(!allowed && wait <= 61, "the fourth hit: {allowed}, {wait} s");
        assert_eq!(raw(&spec, &["HGET", &rkey, "60000"]), format!("{slot} 4"));
        redis_reset(&spec, &key, soon()).unwrap();
        assert_eq!(raw(&spec, &["EXISTS", &rkey]), "0");
    }

    /// A site's keys are bounded: a new key that finds the site at its bound
    /// takes the place of the oldest key that is not at its limit; when every
    /// key is at its limit none goes, and the new key is counted in the
    /// process (found by the 21st assessment: each key lived for its window,
    /// up to a day, and a visitor added 20,507 in 100 seconds).
    #[test]
    fn the_shared_store_keeps_within_its_bound_and_keeps_limited_keys() {
        let Some(spec) = test_redis() else { return };
        let window = Duration::from_secs(3600);
        let run = crate::random_hex(6);
        let (_, p, _) = site_key(0x62, "");
        let key = |name: &str| site_key(0x62, &format!("{run}-{name}"));
        let hit = |name: &str, max: u32| redis_hit_within(&spec, &key(name).2, max, window, soon(), 10).unwrap().0;
        let exists = |name: &str| int(raw(&spec, &["EXISTS", &key(name).0])) == 1;
        let count = |set: &str| int(raw(&spec, &["ZCARD", &format!("{p}{set}")]));
        // A limit reached before the flood.
        assert!(hit("victim", 1));
        assert!(!hit("victim", 1));
        for i in 0..40 {
            assert!(hit(&format!("flood{i}"), 5));
            assert!(count("exp") <= 10, "{} keys after {i}", count("exp"));
        }
        assert_eq!((count("exp"), count("open"), count("full")), (10, 9, 1));
        assert!(exists("victim") && !hit("victim", 1), "the limited key was dropped");
        assert!(!exists("flood0") && exists("flood39"), "the oldest keys go first");
        let hashes = (0..40).filter(|i| exists(&format!("flood{i}"))).count();
        assert_eq!(hashes, 9, "a key dropped from the sets keeps no hash");
        // Limited keys take the places of the keys that are not at their limit.
        for i in 0..9 {
            assert!(hit(&format!("limited{i}"), 1));
        }
        assert_eq!((count("exp"), count("open"), count("full")), (10, 0, 10));
        // Every key is at its limit now: none is given up for a new one, and
        // the new one is counted all the same, in this process (found by the
        // re-attack of the fixes: the newest limited key went, so a limit
        // just reached was lifted by one hit with a fresh key).
        let _table = crate::ratelimit::serial_for_tests();
        assert!(hit("target", 1), "a new key is refused when the site is full");
        assert!(!hit("target", 1), "a new key is not counted when the site is full");
        for i in 0..40 {
            assert!(hit(&format!("fresh{i}"), 1));
            assert!(!hit(&format!("fresh{i}"), 1));
        }
        assert!(!hit("target", 1), "a limit reached in a full site was lifted by new keys");
        assert_eq!((count("exp"), count("open"), count("full")), (10, 0, 10));
        assert!(!exists("target") && !exists("fresh0"), "counted in the process, not on the server");
        assert!(exists("victim") && !hit("victim", 1));
        for i in 0..9 {
            assert!(exists(&format!("limited{i}")) && !hit(&format!("limited{i}"), 1), "limited{i} was given up");
        }
        // A hash a 1.3.2 server wrote is counted on where it is, outside the sets.
        let old = key("old");
        raw(&spec, &["HSET", &old.0, "3600000", &format!("{} 1", server_ms(&spec) / 3_600_000)]);
        raw(&spec, &["PEXPIRE", &old.0, "3600000"]);
        assert!(redis_hit_within(&spec, &old.2, 2, window, soon(), 10).unwrap().0, "the second of two");
        assert!(!redis_hit_within(&spec, &old.2, 2, window, soon(), 10).unwrap().0, "the count started again");
        assert_eq!(count("exp"), 10);
        raw(&spec, &["DEL", &old.0]);
        // A reset forgets what the process counted, too.
        redis_reset(&spec, &key("target").2, soon()).unwrap();
        assert!(hit("target", 1));
        redis_reset(&spec, &key("target").2, soon()).unwrap();
        // A key that is counted already needs no room.
        assert!(!hit("limited0", 1));
        assert_eq!(count("exp"), 10);
        // A reset takes the key out of the sets.
        redis_reset(&spec, &key("victim").2, soon()).unwrap();
        assert_eq!((count("exp"), count("full"), exists("victim")), (9, 9, false));
        // An entry whose hash is gone is forgotten; one whose hash lives on is kept.
        let (gone, kept) = (key("limited1"), key("limited2"));
        let digest = |k: &str| k.trim_start_matches(RATE_KEY).to_string();
        raw(&spec, &["DEL", &gone.0]);
        for k in [&gone.0, &kept.0] {
            raw(&spec, &["ZADD", &format!("{p}exp"), "1", &digest(k)]);
        }
        hit("limited0", 1);
        assert_eq!(count("exp"), 8);
        assert_eq!(raw(&spec, &["ZSCORE", &format!("{p}full"), &digest(&gone.0)]), "nil");
        assert!(int(raw(&spec, &["ZRANK", &format!("{p}exp"), &digest(&kept.0)])) >= 0);
        for set in ["exp", "open", "full"] {
            for id in list(&spec, &["ZRANGE", &format!("{p}{set}"), "0", "-1"]) {
                raw(&spec, &["DEL", &format!("{RATE_KEY}{id}")]);
            }
            raw(&spec, &["DEL", &format!("{p}{set}")]);
        }
    }

    /// Keys a 1.3.2 server wrote (the same hash, no entry in the sets) are
    /// counted on: a limited one stays limited, and becomes one of the
    /// site's bounded keys at its next hit here.
    #[test]
    fn keys_written_by_1_3_2_are_counted_on() {
        let Some(spec) = test_redis() else { return };
        let window = Duration::from_secs(60);
        let (rkey, p, key) = site_key(0x63, &format!("old-{}", crate::random_hex(6)));
        while server_ms(&spec) % 60_000 > 50_000 {
            std::thread::sleep(Duration::from_millis(500));
        }
        let slot = server_ms(&spec) / 60_000;
        // As 1.3.2's script left it: two hits of three, expiring with the window.
        raw(&spec, &["HSET", &rkey, "60000", &format!("{slot} 2")]);
        raw(&spec, &["PEXPIRE", &rkey, "61000"]);
        assert!(redis_hit(&spec, &key, 3, window, soon()).unwrap().0, "the third hit");
        assert!(!redis_hit(&spec, &key, 3, window, soon()).unwrap().0, "the count started again");
        let id = rkey.trim_start_matches(RATE_KEY);
        assert_ne!(raw(&spec, &["ZSCORE", &format!("{p}full"), id]), "nil");
        assert!(int(raw(&spec, &["PTTL", &rkey])) <= 61_000, "the expiry was not put off");
        redis_reset(&spec, &key, soon()).unwrap();
        assert_eq!(int(raw(&spec, &["ZCARD", &format!("{p}exp")])), 0);
    }

    /// A wait for the store that its caller's own deadline cut short does
    /// not mark the store as failed (found by the 21st assessment: a request
    /// at the end of its time limit made every `rate_limit()` of the site
    /// refuse for five seconds); a store that does not answer a caller with
    /// time left is still not asked again for a while.
    #[test]
    fn a_wait_cut_short_by_the_callers_deadline_does_not_fail_the_store() {
        // A server that accepts connections and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for s in listener.incoming().flatten() {
                held.push(s);
            }
        });
        let spec =
            DatabaseSpec { url: Some(format!("redis://127.0.0.1:{port}")), url_env: None, tls: None, pool_size: 4 };
        let window = Duration::from_secs(60);
        let down = |spec: &DatabaseSpec| lock(&RATE_STORE_DOWN).contains_key(spec.url.as_deref().unwrap());
        // A script with 50 ms left.
        let started = Instant::now();
        assert!(redis_hit(&spec, "k", 3, window, started + Duration::from_millis(50)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!down(&spec), "the caller's own deadline marked the store as failed");
        // A caller that gave the store its time.
        assert!(redis_hit(&spec, "k", 3, window, Instant::now() + Duration::from_millis(1200)).is_err());
        assert!(down(&spec), "a store that does not answer is not asked again at once");
        let again = Instant::now();
        assert!(redis_hit(&spec, "k", 3, window, Instant::now() + Duration::from_secs(2)).is_err());
        assert!(again.elapsed() < Duration::from_millis(500), "the store was asked again");
        lock(&RATE_STORE_DOWN).remove(spec.url.as_deref().unwrap());
    }

    /// A store that hangs is found out by callers that never have a second:
    /// after three waits in a row that their own deadlines cut short, a
    /// second in all and no answer between, it is not asked again for a
    /// while (found by the re-attack of the fixes: with `--time-limit 1` 20
    /// of 20 requests waited out their limit). An answer between such waits
    /// starts the count again.
    #[test]
    fn a_store_that_hangs_is_found_out_by_callers_with_little_time() {
        let hung = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = hung.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for s in hung.incoming().flatten() {
                held.push(s);
            }
        });
        let spec =
            DatabaseSpec { url: Some(format!("redis://127.0.0.1:{port}")), url_env: None, tls: None, pool_size: 4 };
        let url = spec.url.clone().unwrap();
        let window = Duration::from_secs(60);
        let down = || lock(&RATE_STORE_DOWN).contains_key(&url);
        let short = || Instant::now() + Duration::from_millis(400);
        for i in 1..=2 {
            assert!(redis_hit(&spec, "k", 3, window, short()).is_err());
            assert!(!down(), "marked as failed after {i} short waits");
        }
        assert!(redis_hit(&spec, "k", 3, window, short()).is_err());
        assert!(down(), "three short waits in a row, 1.2 seconds, and the store is still asked");
        let again = Instant::now();
        assert!(redis_hit(&spec, "k", 3, window, short()).is_err());
        assert!(again.elapsed() < Duration::from_millis(200), "the store was asked again");
        // Short waits a long time apart, or with an answer between them, are
        // not three in a row: only the count is checked here, as the store
        // of this test never answers.
        lock(&RATE_STORE_DOWN).remove(&url);
        lock(&RATE_STORE_CUT).remove(&url);
        assert!(redis_hit(&spec, "k", 3, window, Instant::now() + Duration::from_millis(50)).is_err());
        assert!(redis_hit(&spec, "k", 3, window, Instant::now() + Duration::from_millis(50)).is_err());
        assert!(redis_hit(&spec, "k", 3, window, Instant::now() + Duration::from_millis(50)).is_err());
        assert!(!down(), "three short waits within a fraction of a second marked the store as failed");
        lock(&RATE_STORE_CUT).remove(&url);
    }
}
