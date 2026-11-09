// ----- Sessions stored in a database ([security] session_store = "db:<name>") --
//
// A session is inserted only under a new id and updated only while it exists,
// so a write never brings back a session that a logout removed meanwhile.
//
// Each site keeps its sessions apart from the other sites on the database: in
// a table `phv_sessions_<namespace>` (PostgreSQL, MySQL) and, in Redis, with
// bookkeeping keys `phv:sessions:<namespace>:*` of its own, so the
// `session_ttl` and the limits of one site never remove another's sessions
// (found by the 1.1.1 audit). A site without a namespace has a table and keys
// of its own too, `phv_sessions_unnamed` and `phv:sessions:unnamed:*` (shared
// with every other site without one), apart from the store every site shared
// in 1.1 (found by the 1.2 audit: its ttl and limits removed the sessions
// other sites had kept there).
//
// Sessions that 1.1 kept in `phv_sessions` are still read and updated where
// they are, and removed from both tables; new and regenerated sessions go to
// the site's own table. A session is in one table only, so each step stays
// atomic, and upgrading signs no one out. What is left in the shared store is
// removed only once no site can use it (see `session_sweep`).

use super::handle::{kind_of, tls_init, Kind};
use super::my::{my_connect, my_message, KillAt, MyConn};
use super::pg::{pg_connect, pg_message, PgConn};
use super::pool::{lock, Pooled, MY_POOLS, PG_POOLS};
use super::query_async::{detached_conn, PoolUse};
use super::ratelimit::redis_conn;
use crate::redisclient::{RedisConn, RedisError, Reply};
use phv_vm::DatabaseSpec;
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One stored session: its values as JSON, Unix times, and whether its
/// visitor came back (sent the cookie) at least once.
pub struct SessionRow {
    pub data: String,
    pub created: u64,
    pub last: u64,
    pub returned: bool,
}

/// How long sessions live and how many the store keeps.
pub struct SessionLimits {
    pub ttl: Duration,
    pub max_lifetime: Duration,
    pub max_sessions: usize,
    pub max_bytes: usize,
}

/// How long one session operation may take.
const SESSION_WAIT: Duration = Duration::from_secs(5);

/// A session that has not come back yet lives at most this long.
const NEW_SESSION_TTL: Duration = Duration::from_secs(60 * 60);

/// The table and the Redis bookkeeping that every site shared in 1.1 (and
/// sites without a namespace in 1.1.1).
const SESSIONS_SHARED: &str = "phv_sessions";
const REDIS_SESSIONS_SHARED: &str = "phv:sessions:";
/// Those of the sites without a namespace (since 1.2). Not hex digits: no
/// namespace names them.
const SESSIONS_UNNAMED: &str = "phv_sessions_unnamed";
const REDIS_SESSIONS_UNNAMED: &str = "phv:sessions:unnamed:";

/// The namespace that names the store every site shared in 1.1 itself, to
/// write sessions there as 1.1 did (for the tests of the upgrade). No site
/// has it: a site's namespace is hex digits.
#[doc(hidden)]
pub const SESSIONS_OF_1_1: &str = "1.1";

/// The table of a site's sessions (see `jobs_table`).
fn sessions_table(ns: Option<&str>) -> String {
    match ns {
        Some(SESSIONS_OF_1_1) => SESSIONS_SHARED.to_string(),
        Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_hexdigit()) => format!("phv_sessions_{n}"),
        _ => SESSIONS_UNNAMED.to_string(),
    }
}

/// The prefix of a site's session bookkeeping in Redis (see `sessions_table`).
fn redis_sessions_prefix(ns: Option<&str>) -> String {
    match ns {
        Some(SESSIONS_OF_1_1) => REDIS_SESSIONS_SHARED.to_string(),
        Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_hexdigit()) => format!("phv:sessions:{n}:"),
        _ => REDIS_SESSIONS_UNNAMED.to_string(),
    }
}

/// Session tables this process has created (by URL and table).
static SESSION_TABLES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

pub(super) enum SessionConn {
    Pg(Box<Pooled<PgConn>>),
    My(Box<Pooled<MyConn>>),
    Redis(Pooled<RedisConn>),
}

/// How long after an operation's deadline a store's statement is given up on
/// the client side. The server stops it at the deadline itself (PostgreSQL's
/// `statement_timeout`, MySQL's lock wait timeouts and `KILL QUERY`), so its
/// answer normally comes first and the connection is reused.
const STORE_MARGIN: Duration = Duration::from_millis(500);

/// A connection of PHV's own stores (sessions, jobs) for one operation that
/// must end by `until`: every statement on it stops there, lock waits
/// included, on the server as well as on the client (found by the 1.1.1
/// audit: a job added with a key another open transaction had added waited
/// on that transaction's row past the page's time limit).
pub(super) fn store_conn(
    spec: &DatabaseSpec,
    url: &str,
    use_: PoolUse,
    until: Instant,
    wait: Duration,
) -> Result<SessionConn, String> {
    Ok(match kind_of(url) {
        Kind::Redis => SessionConn::Redis(redis_conn(spec, use_, until)?),
        Kind::Postgres => {
            let mut c = detached_conn(&PG_POOLS, spec, url, use_, until, |t| pg_connect(url, spec, t))?;
            let conn = c.get();
            conn.meter.set(Some(until + STORE_MARGIN), 8 << 20);
            conn.store_until = Some(until);
            SessionConn::Pg(Box::new(c))
        }
        Kind::MySql => {
            let mut c = detached_conn(&MY_POOLS, spec, url, use_, until, |t| {
                my_connect(url, spec, t, Some(8 << 20), Some(wait))
            })?;
            my_store_limits(c.get(), until)?;
            SessionConn::My(Box::new(c))
        }
    })
}

/// Bounds the statements of a store's operation on a MySQL connection: row
/// and metadata lock waits end at `until` (rounded up to a second, the
/// server's unit), and whatever still runs a moment after it is stopped with
/// `KILL QUERY` (the connection is then not reused).
fn my_store_limits(conn: &mut MyConn, until: Instant) -> Result<(), String> {
    use mysql::prelude::Queryable;
    let id = conn.conn.connection_id();
    conn.store_kill = Some(KillAt::arm(&conn.opts, id, Some(until + STORE_MARGIN)));
    let secs = until.saturating_duration_since(Instant::now()).as_secs_f64().ceil().max(1.0) as u64;
    let w = conn.watch();
    conn.conn
        .query_drop(format!("SET SESSION innodb_lock_wait_timeout = {secs}, lock_wait_timeout = {secs}"))
        .map_err(|e| w.msg(e))
}

/// A connection for the sessions of the site with namespace `ns`, and the
/// site's table (created once per process and database).
fn session_conn(spec: &DatabaseSpec, ns: Option<&str>) -> Result<(SessionConn, Instant, String), String> {
    let Some(url) = spec.url.clone() else {
        return Err(format!("the environment variable {} is not set", spec.url_env.as_deref().unwrap_or("?")));
    };
    tls_init();
    let until = Instant::now() + SESSION_WAIT;
    let mut conn = store_conn(spec, &url, PoolUse::Sessions, until, SESSION_WAIT)?;
    let t = sessions_table(ns);
    let made = format!("{url}|{t}");
    if !matches!(conn, SessionConn::Redis(_)) && !lock(&SESSION_TABLES).contains(&made) {
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {t} (id varchar(64) PRIMARY KEY, data text NOT NULL, \
             created bigint NOT NULL, last bigint NOT NULL, returned smallint NOT NULL)"
        );
        match &mut conn {
            SessionConn::Pg(c) => c.get().store_create(&sql).map_err(|e| pg_message(&e))?,
            SessionConn::My(c) => {
                use mysql::prelude::Queryable;
                let w = c.get().watch();
                c.get().conn.query_drop(sql).map_err(|e| w.msg(e))?
            }
            SessionConn::Redis(_) => {}
        }
        lock(&SESSION_TABLES).insert(made);
    }
    Ok((conn, until, t))
}

/// Whether a statement failed because its table does not exist (the shared
/// table of 1.1, on a database 1.1 never used).
pub(super) fn pg_no_table(e: &postgres::Error) -> bool {
    e.code() == Some(&postgres::error::SqlState::UNDEFINED_TABLE)
}

pub(super) fn my_no_table(e: &mysql::Error) -> bool {
    matches!(e, mysql::Error::MySqlError(m) if m.code == 1146)
}

/// Removes the session `x` from the bookkeeping under prefix `q`, without
/// removing the session itself.
const REDIS_SESSION_FORGET: &str = r#"
local function forget(q, x)
  local sz = redis.call('HGET', q .. 'size', x)
  if sz then
    redis.call('HDEL', q .. 'size', x)
    redis.call('INCRBY', q .. 'bytes', -tonumber(sz))
  end
  redis.call('ZREM', q .. 'new', x)
  redis.call('ZREM', q .. 'back', x)
  redis.call('ZREM', q .. 'keep', x)
end
"#;

/// Writes a session in Redis: `mode` "insert" (only if absent) or "update"
/// (only if present). Each session is `phv:session:<key>` with an expiry; two
/// sorted sets (new and returned sessions, by last use), a hash of sizes and a
/// byte counter keep the site's sessions within its limits, evicting new
/// sessions first, then the newest of the returned ones (a third sorted set,
/// by creation). KEYS: the site's prefix of these (`redis_sessions_prefix`),
/// and the shared prefix of 1.1 when it is another ("" otherwise): a session
/// 1.1 counted there is forgotten there, so the other sites' writes do not
/// remove it.
static REDIS_SESSION_WRITE: LazyLock<LuaScript> = LazyLock::new(|| {
    LuaScript::owned(format!(
        "{REDIS_SESSION_FORGET}{}",
        r#"
local p, o = KEYS[1], KEYS[2]
local id, value, px, back, now, size, mode = ARGV[1], ARGV[2], ARGV[3], ARGV[4], tonumber(ARGV[5]), tonumber(ARGV[6]), ARGV[7]
local ttl, new_ttl, max_n, max_b = tonumber(ARGV[8]), tonumber(ARGV[9]), tonumber(ARGV[10]), tonumber(ARGV[11])
local k = 'phv:session:' .. id
local exists = redis.call('EXISTS', k) == 1
if (mode == 'update' and not exists) or (mode == 'insert' and exists) then return 0 end
local function drop(x)
  local sz = tonumber(redis.call('HGET', p .. 'size', x) or '0')
  redis.call('DEL', 'phv:session:' .. x)
  redis.call('HDEL', p .. 'size', x)
  redis.call('ZREM', p .. 'new', x)
  redis.call('ZREM', p .. 'back', x)
  redis.call('ZREM', p .. 'keep', x)
  return redis.call('INCRBY', p .. 'bytes', -sz)
end
if o ~= '' then forget(o, id) end
-- Forget sessions Redis has expired since (a few per write).
for _, x in ipairs(redis.call('ZRANGEBYSCORE', p .. 'new', '-inf', now - new_ttl, 'LIMIT', 0, 50)) do drop(x) end
for _, x in ipairs(redis.call('ZRANGEBYSCORE', p .. 'back', '-inf', now - ttl, 'LIMIT', 0, 50)) do drop(x) end
local old = tonumber(redis.call('HGET', p .. 'size', id) or '0')
redis.call('SET', k, value, 'PX', px)
redis.call('HSET', p .. 'size', id, size)
local total = redis.call('INCRBY', p .. 'bytes', size - old)
if back == '1' then
  local created = tonumber(string.match(value, '^(%d+)')) or now
  redis.call('ZREM', p .. 'new', id)
  redis.call('ZADD', p .. 'back', now, id)
  redis.call('ZADD', p .. 'keep', -created, id)
else
  redis.call('ZADD', p .. 'new', now, id)
end
local count = redis.call('ZCARD', p .. 'new') + redis.call('ZCARD', p .. 'back')
-- The oldest session of a set, other than the one being written.
local function oldest(z)
  for _, x in ipairs(redis.call('ZRANGE', z, 0, 1)) do
    if x ~= id then return x end
  end
  return nil
end
while count > max_n or total > max_b do
  local v = oldest(p .. 'new') or oldest(p .. 'keep')
  if not v then break end
  total = drop(v)
  count = count - 1
end
return 1
"#
    ))
});

/// Removes a session from Redis with its bookkeeping (the site's, and the
/// shared one of 1.1: KEYS as for `REDIS_SESSION_WRITE`); 1 if it existed.
static REDIS_SESSION_REMOVE: LazyLock<LuaScript> = LazyLock::new(|| {
    LuaScript::owned(format!(
        "{REDIS_SESSION_FORGET}{}",
        r#"
local p, o, id = KEYS[1], KEYS[2], ARGV[1]
local existed = redis.call('EXISTS', 'phv:session:' .. id)
redis.call('DEL', 'phv:session:' .. id)
forget(p, id)
if o ~= '' then forget(o, id) end
return existed
"#
    ))
});

/// The two KEYS of the session scripts for the site with namespace `ns`.
fn redis_session_keys(ns: Option<&str>) -> (String, &'static str) {
    let p = redis_sessions_prefix(ns);
    let o = if p == REDIS_SESSIONS_SHARED { "" } else { REDIS_SESSIONS_SHARED };
    (p, o)
}

fn redis_session_key(key: &str) -> String {
    format!("phv:session:{key}")
}

/// A session in Redis: `<created> <last> <returned 0/1>`, a newline, and the data.
fn redis_session_text(row: &SessionRow) -> String {
    format!("{} {} {}\n{}", row.created, row.last, u8::from(row.returned), row.data)
}

/// A Lua script PHV runs on Redis. It is sent by its SHA-1 (`EVALSHA`), and
/// in full (`EVAL`) only when the server does not have it yet.
pub(super) struct LuaScript {
    text: std::borrow::Cow<'static, str>,
    sha: std::sync::OnceLock<String>,
}

impl LuaScript {
    pub(super) const fn new(text: &'static str) -> LuaScript {
        LuaScript { text: std::borrow::Cow::Borrowed(text), sha: std::sync::OnceLock::new() }
    }

    fn owned(text: String) -> LuaScript {
        LuaScript { text: std::borrow::Cow::Owned(text), sha: std::sync::OnceLock::new() }
    }

    /// Runs the script with `args`: the number of keys, the keys, the other arguments.
    pub(super) fn run(
        &self,
        c: &mut RedisConn,
        args: &[&[u8]],
        until: Instant,
        budget: usize,
    ) -> Result<Reply, RedisError> {
        let sha = self.sha.get_or_init(|| {
            use sha1::{Digest, Sha1};
            format!("{:x}", Sha1::digest(self.text.as_bytes()))
        });
        let mut argv: Vec<&[u8]> = Vec::with_capacity(args.len() + 2);
        argv.extend([b"EVALSHA" as &[u8], sha.as_bytes()]);
        argv.extend_from_slice(args);
        match c.command(&argv, until, budget) {
            Err(RedisError::Server(m)) if m.starts_with("NOSCRIPT") => {
                argv[0] = b"EVAL";
                argv[1] = self.text.as_bytes();
                c.command(&argv, until, budget)
            }
            r => r,
        }
    }
}

/// `redis_run` for one of PHV's scripts.
fn redis_script(
    c: &mut Pooled<RedisConn>,
    script: &LuaScript,
    args: &[&[u8]],
    until: Instant,
) -> Result<Reply, String> {
    let r = script.run(c.get(), args, until, 2 << 20);
    redis_result(c, r)
}

pub(super) fn redis_run(c: &mut Pooled<RedisConn>, args: &[&[u8]], until: Instant) -> Result<Reply, String> {
    let r = c.get().command(args, until, 2 << 20);
    redis_result(c, r)
}

fn redis_result(c: &mut Pooled<RedisConn>, r: Result<Reply, RedisError>) -> Result<Reply, String> {
    match r {
        Ok(r) => Ok(r),
        Err(RedisError::Server(m)) => Err(m),
        Err(RedisError::TooLarge) => {
            c.broken = true;
            Err("the session is too large".into())
        }
        Err(RedisError::Broken(m)) => {
            c.broken = true;
            Err(m)
        }
    }
}

/// What `session_read` gives for a stored session it cannot read (its
/// header, or its data, is not what PHV writes; only a writer of the store
/// can make one): a session that expired long ago, so the server removes it
/// as it removes any expired session, instead of taking it for a failure of
/// the store that is never cleared (found by the 19th assessment: one such
/// Redis record made `/ready` fail until another visitor's request, and
/// stayed).
fn damaged_session() -> SessionRow {
    SessionRow { data: "{}".into(), created: 0, last: 0, returned: false }
}

/// The session in table `t` under `key`, if any.
fn pg_session_row(conn: &mut PgConn, t: &str, key: &str) -> Result<Option<SessionRow>, postgres::Error> {
    let rows = conn.query_kept(&format!("SELECT data, created, last, returned FROM {t} WHERE id = $1"), &[&key])?;
    let Some(r) = rows.first() else { return Ok(None) };
    // Text that is not UTF-8 (a database whose encoding is SQL_ASCII) is damage.
    let Ok(data) = r.try_get::<_, String>(0) else { return Ok(Some(damaged_session())) };
    Ok(Some(SessionRow {
        data,
        created: r.try_get::<_, i64>(1)?.max(0) as u64,
        last: r.try_get::<_, i64>(2)?.max(0) as u64,
        returned: r.try_get::<_, i16>(3)? != 0,
    }))
}

fn my_session_row(conn: &mut mysql::Conn, t: &str, key: &str) -> Result<Option<SessionRow>, mysql::Error> {
    use mysql::prelude::Queryable;
    let row: Option<(String, i64, i64, i64)> =
        conn.exec_first(format!("SELECT data, created, last, returned FROM {t} WHERE id = ?"), (key,))?;
    Ok(row.map(|(data, created, last, returned)| SessionRow {
        data,
        created: created.max(0) as u64,
        last: last.max(0) as u64,
        returned: returned != 0,
    }))
}

/// The session stored under `key` (a hash of its id) for the site with
/// namespace `ns`, if any: in the site's table, or else where 1.1 kept it.
pub fn session_read(spec: &DatabaseSpec, ns: Option<&str>, key: &str) -> Result<Option<SessionRow>, String> {
    let (mut conn, until, t) = session_conn(spec, ns)?;
    match &mut conn {
        SessionConn::Redis(c) => {
            let k = redis_session_key(key);
            let Reply::Bulk(b) = redis_run(c, &[b"GET", k.as_bytes()], until)? else { return Ok(None) };
            // Not UTF-8: refused like any other damage (the file store does
            // too), never read with replacement characters.
            let Ok(text) = String::from_utf8(b) else { return Ok(Some(damaged_session())) };
            let Some((head, data)) = text.split_once('\n') else { return Ok(Some(damaged_session())) };
            let mut nums = head.split(' ').map(|n| n.parse::<u64>().ok());
            let (Some(Some(created)), Some(Some(last)), Some(Some(returned))) = (nums.next(), nums.next(), nums.next())
            else {
                return Ok(Some(damaged_session()));
            };
            Ok(Some(SessionRow { data: data.to_string(), created, last, returned: returned != 0 }))
        }
        SessionConn::Pg(c) => {
            let conn = c.get();
            if let Some(row) = pg_session_row(conn, &t, key).map_err(|e| pg_message(&e))? {
                return Ok(Some(row));
            }
            if t == SESSIONS_SHARED {
                return Ok(None);
            }
            match pg_session_row(conn, SESSIONS_SHARED, key) {
                Err(e) if pg_no_table(&e) => Ok(None),
                r => r.map_err(|e| pg_message(&e)),
            }
        }
        SessionConn::My(c) => {
            let w = c.get().watch();
            let conn = &mut c.get().conn;
            if let Some(row) = my_session_row(conn, &t, key).map_err(|e| w.msg(e))? {
                return Ok(Some(row));
            }
            if t == SESSIONS_SHARED {
                return Ok(None);
            }
            match my_session_row(conn, SESSIONS_SHARED, key).map_err(|e| w.raw(e)) {
                Err(e) if my_no_table(&e) => Ok(None),
                r => r.map_err(|e| my_message(&e)),
            }
        }
    }
}

/// Stores a session under a new `key`; false if one is there already.
pub fn session_insert(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    key: &str,
    row: &SessionRow,
    limits: &SessionLimits,
) -> Result<bool, String> {
    session_store(spec, ns, key, row, limits, true)
}

/// Replaces the session under `key`; false (and nothing is stored) if it is gone.
pub fn session_update(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    key: &str,
    row: &SessionRow,
    limits: &SessionLimits,
) -> Result<bool, String> {
    session_store(spec, ns, key, row, limits, false)
}

/// Updates the session under `key` in table `t`; whether it is there.
fn pg_session_update(conn: &mut PgConn, t: &str, key: &str, row: &SessionRow) -> Result<bool, postgres::Error> {
    let (last, returned) = (row.last as i64, i16::from(row.returned));
    conn.execute_kept(
        &format!("UPDATE {t} SET data = $2, last = $3, returned = $4 WHERE id = $1"),
        &[&key, &row.data, &last, &returned],
    )
    .map(|n| n > 0)
}

fn my_session_update(conn: &mut mysql::Conn, t: &str, key: &str, row: &SessionRow) -> Result<bool, mysql::Error> {
    use mysql::prelude::Queryable;
    let (last, returned) = (row.last as i64, i16::from(row.returned));
    conn.exec_drop(
        format!("UPDATE {t} SET data = ?, last = ?, returned = ? WHERE id = ?"),
        (&row.data, last, returned, key),
    )?;
    // MySQL counts only rows it changed: an unchanged row is still there.
    if conn.affected_rows() > 0 {
        return Ok(true);
    }
    let found: Option<i64> = conn.exec_first(format!("SELECT 1 FROM {t} WHERE id = ?"), (key,))?;
    Ok(found.is_some())
}

fn session_store(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    key: &str,
    row: &SessionRow,
    limits: &SessionLimits,
    insert: bool,
) -> Result<bool, String> {
    let (mut conn, until, t) = session_conn(spec, ns)?;
    let returned = i16::from(row.returned);
    let (created, last) = (row.created as i64, row.last as i64);
    match &mut conn {
        SessionConn::Redis(c) => {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            // Until the visitor comes back, a session lives an hour at most; never past its lifetime.
            let mut life = if row.returned { limits.ttl } else { limits.ttl.min(NEW_SESSION_TTL) };
            let left = row.created.saturating_add(limits.max_lifetime.as_secs()).saturating_sub(now);
            life = life.min(Duration::from_secs(left.max(1)));
            let px = life.as_millis().max(1).to_string();
            let value = redis_session_text(row);
            let args: Vec<String> = vec![
                now.to_string(),
                value.len().to_string(),
                if insert { "insert" } else { "update" }.into(),
                limits.ttl.as_secs().to_string(),
                NEW_SESSION_TTL.as_secs().to_string(),
                limits.max_sessions.to_string(),
                limits.max_bytes.to_string(),
            ];
            let (p, o) = redis_session_keys(ns);
            let mut argv: Vec<&[u8]> = vec![
                b"2",
                p.as_bytes(),
                o.as_bytes(),
                key.as_bytes(),
                value.as_bytes(),
                px.as_bytes(),
                if row.returned { b"1" } else { b"0" },
            ];
            argv.extend(args.iter().map(|a| a.as_bytes()));
            match redis_script(c, &REDIS_SESSION_WRITE, &argv, until)? {
                Reply::Int(n) => Ok(n == 1),
                _ => Err("the server sent an invalid reply".into()),
            }
        }
        SessionConn::Pg(c) => {
            let conn = c.get();
            let pg = |e: postgres::Error| pg_message(&e);
            if insert {
                return conn
                    .execute_kept(
                        &format!(
                            "INSERT INTO {t} (id, data, created, last, returned) VALUES ($1, $2, $3, $4, $5) \
                             ON CONFLICT (id) DO NOTHING"
                        ),
                        &[&key, &row.data, &created, &last, &returned],
                    )
                    .map(|n| n > 0)
                    .map_err(pg);
            }
            if pg_session_update(conn, &t, key, row).map_err(pg)? {
                return Ok(true);
            }
            if t == SESSIONS_SHARED {
                return Ok(false);
            }
            // A session 1.1 stored is updated where it is.
            match pg_session_update(conn, SESSIONS_SHARED, key, row) {
                Err(e) if pg_no_table(&e) => Ok(false),
                r => r.map_err(pg),
            }
        }
        SessionConn::My(c) => {
            use mysql::prelude::Queryable;
            let w = c.get().watch();
            let my = |e: mysql::Error| w.msg(e);
            let conn = &mut c.get().conn;
            if insert {
                conn.exec_drop(
                    format!("INSERT IGNORE INTO {t} (id, data, created, last, returned) VALUES (?, ?, ?, ?, ?)"),
                    (key, &row.data, created, last, returned),
                )
                .map_err(my)?;
                return Ok(conn.affected_rows() > 0);
            }
            if my_session_update(conn, &t, key, row).map_err(my)? {
                return Ok(true);
            }
            if t == SESSIONS_SHARED {
                return Ok(false);
            }
            match my_session_update(conn, SESSIONS_SHARED, key, row).map_err(|e| w.raw(e)) {
                Err(e) if my_no_table(&e) => Ok(false),
                r => r.map_err(|e| my_message(&e)),
            }
        }
    }
}

/// Removes the session stored under `key` (in the site's table and where 1.1
/// kept it); true if it was there.
pub fn session_remove(spec: &DatabaseSpec, ns: Option<&str>, key: &str) -> Result<bool, String> {
    let (mut conn, until, t) = session_conn(spec, ns)?;
    match &mut conn {
        SessionConn::Redis(c) => {
            let (p, o) = redis_session_keys(ns);
            match redis_script(c, &REDIS_SESSION_REMOVE, &[b"2", p.as_bytes(), o.as_bytes(), key.as_bytes()], until)? {
                Reply::Int(n) => Ok(n == 1),
                _ => Err("the server sent an invalid reply".into()),
            }
        }
        SessionConn::Pg(c) => {
            let conn = c.get();
            let pg = |e: postgres::Error| pg_message(&e);
            let mut n = conn.execute_kept(&format!("DELETE FROM {t} WHERE id = $1"), &[&key]).map_err(pg)?;
            if t != SESSIONS_SHARED {
                match conn.execute_kept(&format!("DELETE FROM {SESSIONS_SHARED} WHERE id = $1"), &[&key]) {
                    Err(e) if pg_no_table(&e) => {}
                    r => n += r.map_err(pg)?,
                }
            }
            Ok(n > 0)
        }
        SessionConn::My(c) => {
            use mysql::prelude::Queryable;
            let w = c.get().watch();
            let my = |e: mysql::Error| my_message(&e);
            let conn = &mut c.get().conn;
            conn.exec_drop(format!("DELETE FROM {t} WHERE id = ?"), (key,)).map_err(|e| w.msg(e))?;
            let mut n = conn.affected_rows();
            if t != SESSIONS_SHARED {
                match conn
                    .exec_drop(format!("DELETE FROM {SESSIONS_SHARED} WHERE id = ?"), (key,))
                    .map_err(|e| w.raw(e))
                {
                    Err(e) if my_no_table(&e) => {}
                    r => {
                        r.map_err(my)?;
                        n += conn.affected_rows();
                    }
                }
            }
            Ok(n > 0)
        }
    }
}

/// Removes the site's sessions unused for `ttl` or older than `max_lifetime`,
/// and, when more than `max_sessions` (or `max_bytes` of data) are left, those
/// whose visitor never came back first, then the least recently used, down to
/// 90%. (Redis keeps within its limits as sessions are written.)
///
/// Each site also clears what 1.1 left in the shared store, without touching
/// a session that is still alive for any site (found by the 1.1.1 audit: only
/// sites without a namespace swept it, and by the 1.2 audit: by their own ttl
/// and limits): rows of the shared table older than `max_lifetime` (24 hours,
/// the same for every site), which every site has expired, and in Redis the
/// shared bookkeeping of sessions Redis has expired since. Both in bounded
/// batches.
pub fn session_sweep(spec: &DatabaseSpec, ns: Option<&str>, limits: &SessionLimits) -> Result<(), String> {
    let (mut conn, until, t) = session_conn(spec, ns)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (idle, old) = (now - limits.ttl.as_secs() as i64, now - limits.max_lifetime.as_secs() as i64);
    let (max_n, max_b) = (limits.max_sessions as i64, limits.max_bytes as i64);
    if t != SESSIONS_SHARED {
        sweep_shared_sessions(&mut conn, old, until)?;
    }
    // Deleting a tenth at a time until both limits hold (each pass is bounded).
    for _ in 0..20 {
        let (count, bytes): (i64, i64) = match &mut conn {
            SessionConn::Redis(_) => return Ok(()),
            SessionConn::Pg(c) => {
                let conn = c.get();
                let pg = |e: postgres::Error| pg_message(&e);
                conn.execute_kept(&format!("DELETE FROM {t} WHERE last < $1 OR created < $2"), &[&idle, &old])
                    .map_err(pg)?;
                let r = conn
                    .query_kept(&format!("SELECT count(*), coalesce(sum(length(data)), 0)::bigint FROM {t}"), &[])
                    .map_err(pg)?;
                (r[0].try_get(0).map_err(pg)?, r[0].try_get(1).map_err(pg)?)
            }
            SessionConn::My(c) => {
                use mysql::prelude::Queryable;
                let w = c.get().watch();
                let my = |e: mysql::Error| w.msg(e);
                let conn = &mut c.get().conn;
                conn.exec_drop(format!("DELETE FROM {t} WHERE last < ? OR created < ?"), (idle, old)).map_err(my)?;
                let r: Option<(i64, i64)> = conn
                    .query_first(format!("SELECT count(*), CAST(coalesce(sum(length(data)), 0) AS SIGNED) FROM {t}"))
                    .map_err(my)?;
                r.unwrap_or((0, 0))
            }
        };
        if count <= max_n && bytes <= max_b {
            return Ok(());
        }
        let excess = (count - max_n * 9 / 10).max(count / 10).max(1);
        match &mut conn {
            SessionConn::Pg(c) => {
                c.get()
                    .execute_kept(
                        &format!(
                            "DELETE FROM {t} WHERE id IN (SELECT id FROM {t} ORDER BY returned, \
                             CASE WHEN returned = 1 THEN -created ELSE last END LIMIT $1)"
                        ),
                        &[&excess],
                    )
                    .map_err(|e| pg_message(&e))?;
            }
            SessionConn::My(c) => {
                use mysql::prelude::Queryable;
                let w = c.get().watch();
                c.get()
                    .conn
                    .exec_drop(
                        format!(
                            "DELETE FROM {t} ORDER BY returned, CASE WHEN returned = 1 THEN -created ELSE last END \
                             LIMIT ?"
                        ),
                        (excess,),
                    )
                    .map_err(|e| w.msg(e))?;
            }
            SessionConn::Redis(_) => {}
        }
    }
    Ok(())
}

/// Rows of the shared table (or entries of the shared Redis bookkeeping) that
/// one pass of `sweep_shared_sessions` looks at, and passes at most.
const SHARED_SWEEP_BATCH: i64 = 1000;
const SHARED_SWEEP_PASSES: usize = 20;

/// Forgets, under the shared prefix of 1.1 (KEYS[1]), sessions Redis no
/// longer has: the oldest ARGV[1] of each set are looked at. A session that
/// still exists is left alone, whichever site it is of. The number forgotten.
static REDIS_SESSION_SWEEP_SHARED: LazyLock<LuaScript> = LazyLock::new(|| {
    LuaScript::owned(format!(
        "{REDIS_SESSION_FORGET}{}",
        r#"
local o, n, gone = KEYS[1], tonumber(ARGV[1]), 0
for _, z in ipairs({'new', 'back'}) do
  for _, x in ipairs(redis.call('ZRANGE', o .. z, 0, n - 1)) do
    if redis.call('EXISTS', 'phv:session:' .. x) == 0 then
      forget(o, x)
      gone = gone + 1
    end
  end
end
return gone
"#
    ))
});

/// For a site with a namespace: removes, in bounded batches, what 1.1 left in
/// the shared store and no site can still use (see `session_sweep`).
fn sweep_shared_sessions(conn: &mut SessionConn, old: i64, until: Instant) -> Result<(), String> {
    let batch = SHARED_SWEEP_BATCH;
    for _ in 0..SHARED_SWEEP_PASSES {
        let gone = match conn {
            SessionConn::Redis(c) => {
                let n = batch.to_string();
                let args: [&[u8]; 3] = [b"1", REDIS_SESSIONS_SHARED.as_bytes(), n.as_bytes()];
                match redis_script(c, &REDIS_SESSION_SWEEP_SHARED, &args, until)? {
                    Reply::Int(n) => n,
                    _ => return Err("the server sent an invalid reply".into()),
                }
            }
            SessionConn::Pg(c) => {
                let sql = format!(
                    "DELETE FROM {SESSIONS_SHARED} WHERE id IN \
                     (SELECT id FROM {SESSIONS_SHARED} WHERE created < $1 LIMIT $2)"
                );
                match c.get().execute_kept(&sql, &[&old, &batch]) {
                    Err(e) if pg_no_table(&e) => return Ok(()),
                    r => r.map_err(|e| pg_message(&e))? as i64,
                }
            }
            SessionConn::My(c) => {
                use mysql::prelude::Queryable;
                let w = c.get().watch();
                let conn = &mut c.get().conn;
                let sql = format!("DELETE FROM {SESSIONS_SHARED} WHERE created < ? LIMIT ?");
                match conn.exec_drop(sql, (old, batch)).map_err(|e| w.raw(e)) {
                    Err(e) if my_no_table(&e) => return Ok(()),
                    r => r.map_err(|e| my_message(&e))?,
                }
                conn.affected_rows() as i64
            }
        };
        if gone < batch {
            break;
        }
    }
    Ok(())
}
