// ----- Background jobs in a database ([jobs] store = "db:<name>") ------------
//
// PostgreSQL and MySQL keep jobs in a table `phv_jobs`; Redis under `phv:jobs:`.
// A job is taken with a lease (a random token and a time): finishing it needs
// the token, so a job whose lease ran out and was taken by another process is
// not finished twice by the first. States: 0 waiting, 1 running, 2 given up,
// 3 done (kept a day when it has a key, so the key is not used again).

use super::handle::tls_init;
use super::jobs_add::MY_JOBS_LOCK_TABLE;
use super::my::MyConn;
use super::pg::{pg_message, PgConn};
use super::pool::{lock, Pooled};
use super::query_async::PoolUse;
use super::sessions::{store_conn, LuaScript, SessionConn};
use crate::redisclient::{RedisConn, RedisError, Reply};
use phv_vm::DatabaseSpec;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One job taken to be run.
#[derive(Clone, Debug)]
pub struct JobRow {
    pub id: String,
    pub name: String,
    pub payload: String,
    /// Runs so far, this one included.
    pub attempts: u32,
    pub lease: String,
}

/// A job as `phv jobs list` shows it.
#[derive(Clone, Debug)]
pub struct JobInfo {
    pub id: String,
    pub name: String,
    /// "waiting", "running" or "failed".
    pub state: &'static str,
    pub attempts: u32,
    /// Unix time in milliseconds: when it runs next, or when it failed.
    pub at: i64,
    pub error: String,
}

/// Below the caps by more than these, the count of jobs may be a second old:
/// jobs are added without the store's lock.
const JOB_COUNT_MARGIN: i64 = 10_000;
const JOB_BYTES_MARGIN: i64 = 256 << 20;
/// A count of a store taken (exactly) less than this long ago stands, with the
/// jobs added under the store's lock since (see `phv_jobs_counts`).
pub(super) const JOB_COUNT_FRESH_MS: i64 = 1000;

/// How long one operation on the job store may take.
pub(super) const JOB_WAIT: Duration = Duration::from_secs(10);
/// Jobs that ended with a key are kept this long (so the key is not used
/// again), and at most `JOB_MAX_KEPT` of them (the latest).
pub(super) const JOB_KEEP_DONE_MS: i64 = 24 * 3600 * 1000;
pub const JOB_MAX_KEPT: usize = 100_000;
/// Jobs given up are kept this long, and at most `JOB_MAX_DEAD` of them.
pub(super) const JOB_KEEP_DEAD_MS: i64 = 7 * 24 * 3600 * 1000;
pub const JOB_MAX_DEAD: usize = 10_000;
/// Rows one statement of a sweep removes at most (found by the 1.1.1 audit:
/// MySQL removed them in one statement however many there were).
pub(super) const JOB_SWEEP_BATCH: i64 = 10_000;

/// What a job store holds at most: waiting and running jobs, and the bytes of
/// their payloads (found by the 1.1.1 audit: only the jobs were counted, so a
/// store could hold 6.4 GB of payloads).
#[derive(Clone, Copy, Debug)]
pub struct JobCap {
    pub jobs: usize,
    pub bytes: usize,
}

impl JobCap {
    /// The error of a full store (`std.jobs` makes it a RuntimeException).
    pub fn full(&self) -> String {
        format!("the job queue holds {} jobs or {} MB already", self.jobs, self.bytes >> 20)
    }
}

/// What a store holds: waiting and running jobs, and their payload bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(super) struct JobLoad {
    pub(super) n: i64,
    pub(super) bytes: i64,
}

impl JobLoad {
    pub(super) fn plus(self, other: JobLoad) -> JobLoad {
        JobLoad { n: self.n + other.n, bytes: self.bytes + other.bytes }
    }

    pub(super) fn one(size: usize) -> JobLoad {
        JobLoad { n: 1, bytes: size as i64 }
    }

    /// Whether a job of `size` bytes no longer fits.
    pub(super) fn full(&self, cap: JobCap, size: usize) -> bool {
        self.n >= cap.jobs as i64 || self.bytes + size as i64 > cap.bytes as i64
    }

    /// Whether it is so far below both caps that a count a second old says the same.
    pub(super) fn far(&self, cap: JobCap) -> bool {
        self.n + JOB_COUNT_MARGIN < cap.jobs as i64 && self.bytes + JOB_BYTES_MARGIN < cap.bytes as i64
    }
}

/// Databases whose jobs tables this process has made ready (by URL and table).
pub(super) static JOB_TABLES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Jobs tables made before 1.1.1 whose `size` column could not be added yet
/// (another transaction held the table), and when that was last tried: the
/// bytes of their jobs are not counted until it is added. Adding it waits
/// for the table this long at most (every statement on the table waits
/// behind it meanwhile), and is tried again after `JOB_SIZE_RETRY`.
pub(super) static JOB_UNSIZED: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);
const JOB_ALTER_WAIT_MS: u64 = 200;
pub(super) const JOB_SIZE_RETRY: Duration = Duration::from_secs(60);

/// Whether table `t` has the `size` column (see `JOB_UNSIZED`).
pub(super) fn job_sized(t: &str) -> bool {
    !lock(&JOB_UNSIZED).contains_key(t)
}

/// Notes whether adding the `size` column to `t` worked.
fn note_sized(t: &str, sized: bool) {
    let mut pending = lock(&JOB_UNSIZED);
    if sized {
        pending.remove(t);
    } else {
        pending.insert(t.to_string(), Instant::now());
    }
}

/// The table of a site's jobs: one of its own when it has a namespace
/// (`Host::store_namespace`, hex digits), so sites sharing a database do not
/// take each other's jobs (found by the 1.1 audit).
pub(super) fn jobs_table(ns: Option<&str>) -> String {
    match ns {
        Some(n) if n.bytes().all(|b| b.is_ascii_hexdigit()) => format!("phv_jobs_{n}"),
        _ => "phv_jobs".to_string(),
    }
}

/// The Redis prefix of a site's jobs (see `jobs_table`).
pub(super) fn jobs_prefix(ns: Option<&str>) -> String {
    match ns {
        Some(n) if n.bytes().all(|b| b.is_ascii_hexdigit()) => format!("phv:jobs:{n}:"),
        _ => "phv:jobs:".to_string(),
    }
}

pub(super) fn job_conn(spec: &DatabaseSpec, ns: Option<&str>) -> Result<(SessionConn, Instant), String> {
    job_conn_before(spec, ns, None)
}

/// `job_conn`, waiting until `deadline` at most (a script's time left).
pub(super) fn job_conn_before(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    deadline: Option<Instant>,
) -> Result<(SessionConn, Instant), String> {
    job_conn_for(spec, ns, deadline, PoolUse::Jobs)
}

/// A connection of the job store for one operation (see `store_conn`), with
/// the store's tables made ready once per process.
pub(super) fn job_conn_for(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    deadline: Option<Instant>,
    use_: PoolUse,
) -> Result<(SessionConn, Instant), String> {
    let t = jobs_table(ns);
    let Some(url) = spec.url.clone() else {
        return Err(format!("the environment variable {} is not set", spec.url_env.as_deref().unwrap_or("?")));
    };
    tls_init();
    // No longer than the script may still run (found by the 1.1 audit: a store
    // that did not answer held a page for 10 s, whatever its time limit).
    let until = deadline.map_or(Instant::now() + JOB_WAIT, |d| d.min(Instant::now() + JOB_WAIT));
    let mut conn = store_conn(spec, &url, use_, until, JOB_WAIT)?;
    let made = format!("{url}|{t}");
    let retry = lock(&JOB_UNSIZED).get(&t).is_some_and(|at| at.elapsed() >= JOB_SIZE_RETRY);
    if !matches!(conn, SessionConn::Redis(_)) && (retry || !lock(&JOB_TABLES).contains(&made)) {
        match &mut conn {
            SessionConn::Pg(c) => pg_job_tables(c.get(), &t).map_err(|e| pg_message(&e))?,
            SessionConn::My(c) => my_job_tables(c.get(), &t)?,
            SessionConn::Redis(_) => {}
        }
        lock(&JOB_TABLES).insert(made);
    }
    Ok((conn, until))
}

/// The count of each store's jobs kept by those that add jobs under the
/// store's lock (`job_add_locked`): the jobs and bytes counted at `at` (Unix
/// ms), with the jobs added under the lock since. A transaction that adds jobs
/// sets `at` to 0 (see `job_tx_load`), so the next to take the lock counts again.
const JOBS_COUNTS_TABLE: &str = "CREATE TABLE IF NOT EXISTS phv_jobs_counts (t varchar(64) PRIMARY KEY, \
    n bigint NOT NULL, bytes bigint NOT NULL, at bigint NOT NULL)";

/// Creates the jobs table `t` (PostgreSQL), with what later versions added to it.
fn pg_job_tables(conn: &mut PgConn, t: &str) -> Result<(), postgres::Error> {
    conn.store_create(&format!(
        "CREATE TABLE IF NOT EXISTS {t} (id varchar(32) PRIMARY KEY, name varchar(64) NOT NULL, \
         payload text NOT NULL, run_at bigint NOT NULL, attempts integer NOT NULL, state smallint NOT NULL, \
         lease varchar(32), lease_until bigint, uniq varchar(64) UNIQUE, error text, created bigint NOT NULL, \
         size integer NOT NULL DEFAULT 0); {JOBS_COUNTS_TABLE}"
    ))?;
    // Only what is missing: CREATE INDEX and ALTER TABLE wait for every open
    // transaction that added a job, even when there is nothing to do.
    let index = format!("{t}_due");
    let rows = conn.query_kept(
        "SELECT (SELECT count(*) FROM information_schema.columns WHERE table_schema = current_schema() \
         AND table_name = $1 AND column_name = 'size'), \
         (SELECT count(*) FROM pg_indexes WHERE schemaname = current_schema() AND indexname = $2)",
        &[&t, &index],
    )?;
    let (sized, indexed): (i64, i64) = rows.first().map_or(Ok((0, 0)), |r| Ok((r.try_get(0)?, r.try_get(1)?)))?;
    if indexed == 0 {
        conn.store_create(&format!("CREATE INDEX IF NOT EXISTS {index} ON {t} (state, run_at)"))?;
    }
    // The bytes of each job's payload (since 1.1.1); jobs added before count none.
    if sized == 0 {
        let alter = conn.store_batch(&format!(
            "BEGIN; SET LOCAL lock_timeout = {JOB_ALTER_WAIT_MS}; \
             ALTER TABLE {t} ADD COLUMN IF NOT EXISTS size integer NOT NULL DEFAULT 0; COMMIT"
        ));
        match alter {
            Err(e) if e.code() == Some(&postgres::error::SqlState::LOCK_NOT_AVAILABLE) => {
                conn.client.batch_execute("ROLLBACK")?;
                note_sized(t, false);
            }
            r => {
                r?;
                note_sized(t, true);
            }
        }
    }
    Ok(())
}

/// Creates the jobs table `t` (MySQL), with what later versions added to it.
fn my_job_tables(conn: &mut MyConn, t: &str) -> Result<(), String> {
    use mysql::prelude::Queryable;
    let w = conn.watch();
    let my = |e: mysql::Error| w.msg(e);
    let c = &mut conn.conn;
    c.query_drop(format!(
        "CREATE TABLE IF NOT EXISTS {t} (id varchar(32) PRIMARY KEY, name varchar(64) NOT NULL, \
         payload mediumtext NOT NULL, run_at bigint NOT NULL, attempts integer NOT NULL, \
         state smallint NOT NULL, lease varchar(32), lease_until bigint, uniq varchar(64) UNIQUE, \
         error text, created bigint NOT NULL, size integer NOT NULL DEFAULT 0, INDEX {t}_due (state, run_at))"
    ))
    .map_err(my)?;
    let sized: Option<i64> = c
        .exec_first(
            "SELECT count(*) FROM information_schema.columns WHERE table_schema = DATABASE() \
             AND table_name = ? AND column_name = 'size'",
            (t,),
        )
        .map_err(my)?;
    if sized == Some(0) {
        c.query_drop(format!("SET SESSION lock_wait_timeout = {}", JOB_ALTER_WAIT_MS.div_ceil(1000))).map_err(my)?;
        let alter = c.query_drop(format!("ALTER TABLE {t} ADD COLUMN size integer NOT NULL DEFAULT 0"));
        c.query_drop(format!("SET SESSION lock_wait_timeout = {}", JOB_WAIT.as_secs())).map_err(my)?;
        match alter {
            Err(mysql::Error::MySqlError(e)) if e.code == 1205 => note_sized(t, false),
            // 1060: another process added it at the same moment.
            Err(mysql::Error::MySqlError(e)) if e.code == 1060 => note_sized(t, true),
            r => {
                r.map_err(my)?;
                note_sized(t, true);
            }
        }
    }
    // The row that transactions adding jobs lock (see `my_job_lock`): looked
    // for without a lock first, as `INSERT IGNORE` of a row that is there waits
    // for the transaction holding it.
    c.query_drop(MY_JOBS_LOCK_TABLE).map_err(my)?;
    if c.exec_first::<String, _, _>("SELECT t FROM phv_jobs_lock WHERE t = ?", (t,)).map_err(my)?.is_none() {
        c.exec_drop("INSERT IGNORE INTO phv_jobs_lock (t) VALUES (?)", (t,)).map_err(my)?;
    }
    c.query_drop(JOBS_COUNTS_TABLE).map_err(my)?;
    Ok(())
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

pub(super) fn job_redis_result<T>(c: &mut Pooled<RedisConn>, r: Result<T, RedisError>) -> Result<T, String> {
    match r {
        Ok(r) => Ok(r),
        Err(RedisError::Server(m)) => Err(m),
        Err(RedisError::TooLarge) => {
            c.broken = true;
            Err("the reply was too large".into())
        }
        Err(RedisError::Broken(m)) => {
            c.broken = true;
            Err(m)
        }
    }
}

/// Runs a job script with `prefix` as its key (`args` start with "0", the
/// count of keys, which becomes 1).
pub(super) fn job_redis(
    c: &mut Pooled<RedisConn>,
    script: &LuaScript,
    prefix: &str,
    args: &[&[u8]],
    until: Instant,
) -> Result<Reply, String> {
    debug_assert_eq!(args.first(), Some(&&b"0"[..]));
    let mut with_key: Vec<&[u8]> = Vec::with_capacity(args.len() + 1);
    with_key.push(b"1");
    with_key.push(prefix.as_bytes());
    with_key.extend_from_slice(&args[1..]);
    let r = script.run(c.get(), &with_key, until, 32 << 20);
    job_redis_result(c, r)
}

// In Redis, `bytes` is the size of the payloads of the waiting and running
// jobs, and `kept` the keys of jobs that ended, by when they ended.

/// ARGV: id, name, payload, run_at, uniq ("" for none), cap, now, cap of bytes.
/// 1 added, 0 the key is taken, -1 the queue is full.
pub(super) static REDIS_JOB_ADD: LuaScript = LuaScript::new(
    r#"
local p = KEYS[1]
if ARGV[5] ~= '' and redis.call('SET', p .. 'key:' .. ARGV[5], ARGV[1], 'NX') == false then return 0 end
local bytes = math.max(tonumber(redis.call('GET', p .. 'bytes') or '0'), 0)
if redis.call('ZCARD', p .. 'due') + redis.call('ZCARD', p .. 'running') >= tonumber(ARGV[6])
  or bytes + #ARGV[3] > tonumber(ARGV[8]) then
  if ARGV[5] ~= '' then redis.call('DEL', p .. 'key:' .. ARGV[5]) end
  return -1
end
redis.call('HSET', p .. 'job:' .. ARGV[1], 'name', ARGV[2], 'payload', ARGV[3], 'attempts', 0, 'uniq', ARGV[5], 'created', ARGV[7])
redis.call('ZADD', p .. 'due', ARGV[4], ARGV[1])
redis.call('SET', p .. 'bytes', bytes + #ARGV[3])
return 1
"#,
);

/// ARGV: now, lease_until, token. The job taken: {id, name, payload, attempts}, or nil.
pub(super) static REDIS_JOB_TAKE: LuaScript = LuaScript::new(
    r#"
local p = KEYS[1]
local id = redis.call('ZRANGEBYSCORE', p .. 'running', '-inf', '(' .. ARGV[1], 'LIMIT', 0, 1)[1]
if id then
  redis.call('ZREM', p .. 'running', id)
else
  id = redis.call('ZRANGEBYSCORE', p .. 'due', '-inf', ARGV[1], 'LIMIT', 0, 1)[1]
  if not id then return false end
  redis.call('ZREM', p .. 'due', id)
end
local k = p .. 'job:' .. id
if redis.call('EXISTS', k) == 0 then return false end
redis.call('ZADD', p .. 'running', ARGV[2], id)
local n = redis.call('HINCRBY', k, 'attempts', 1)
redis.call('HSET', k, 'lease', ARGV[3])
local f = redis.call('HMGET', k, 'name', 'payload')
return {id, f[1], f[2], n}
"#,
);

/// ARGV: id, token, "done" | "retry" | "dead", time, error, most jobs given
/// up kept, most keys of ended jobs kept.
pub(super) static REDIS_JOB_END: LuaScript = LuaScript::new(
    r#"
local p = KEYS[1]
local k = p .. 'job:' .. ARGV[1]
if redis.call('HGET', k, 'lease') ~= ARGV[2] then return 0 end
redis.call('ZREM', p .. 'running', ARGV[1])
redis.call('HDEL', k, 'lease')
local uniq = redis.call('HGET', k, 'uniq')
if ARGV[3] ~= 'retry' then
  redis.call('DECRBY', p .. 'bytes', redis.call('HSTRLEN', k, 'payload'))
end
if ARGV[3] == 'done' then
  redis.call('DEL', k)
  if uniq and uniq ~= '' then
    redis.call('EXPIRE', p .. 'key:' .. uniq, 86400)
    redis.call('ZADD', p .. 'kept', ARGV[4], uniq)
    local over = redis.call('ZCARD', p .. 'kept') - tonumber(ARGV[7])
    if over > 0 then
      -- The oldest go first; a key a waiting job took again (no expiry) stays.
      for _, u in ipairs(redis.call('ZRANGE', p .. 'kept', 0, over - 1)) do
        if redis.call('PTTL', p .. 'key:' .. u) > 0 then redis.call('DEL', p .. 'key:' .. u) end
      end
      redis.call('ZREMRANGEBYRANK', p .. 'kept', 0, over - 1)
    end
  end
elseif ARGV[3] == 'retry' then
  redis.call('HSET', k, 'error', ARGV[5])
  redis.call('ZADD', p .. 'due', ARGV[4], ARGV[1])
else
  redis.call('HSET', k, 'error', ARGV[5])
  redis.call('ZADD', p .. 'dead', ARGV[4], ARGV[1])
  local over = redis.call('ZCARD', p .. 'dead') - tonumber(ARGV[6])
  if over > 0 then
    for _, old in ipairs(redis.call('ZRANGE', p .. 'dead', 0, over - 1)) do
      local u = redis.call('HGET', p .. 'job:' .. old, 'uniq')
      if u and u ~= '' then redis.call('DEL', p .. 'key:' .. u) end
      redis.call('DEL', p .. 'job:' .. old)
    end
    redis.call('ZREMRANGEBYRANK', p .. 'dead', 0, over - 1)
  end
end
return 1
"#,
);

/// ARGV: before (jobs given up before it are removed), kept before (keys of
/// ended jobs, which Redis expires itself, are forgotten). The byte count is
/// set right when no job waits or runs (jobs added before 1.1.1 were not counted).
pub(super) static REDIS_JOB_SWEEP: LuaScript = LuaScript::new(
    r#"
local p = KEYS[1]
local old = redis.call('ZRANGEBYSCORE', p .. 'dead', '-inf', ARGV[1], 'LIMIT', 0, 1000)
for _, id in ipairs(old) do
  local u = redis.call('HGET', p .. 'job:' .. id, 'uniq')
  if u and u ~= '' then redis.call('DEL', p .. 'key:' .. u) end
  redis.call('DEL', p .. 'job:' .. id)
  redis.call('ZREM', p .. 'dead', id)
end
redis.call('ZREMRANGEBYSCORE', p .. 'kept', '-inf', ARGV[2])
if redis.call('ZCARD', p .. 'due') + redis.call('ZCARD', p .. 'running') == 0
  or tonumber(redis.call('GET', p .. 'bytes') or '0') < 0 then
  redis.call('SET', p .. 'bytes', 0)
end
return #old
"#,
);

/// ARGV: "all" | "failed". Flat list: id, name, state, attempts, at, error, ...
pub(super) static REDIS_JOB_LIST: LuaScript = LuaScript::new(
    r#"
local p = KEYS[1]
local out = {}
local sets = ARGV[1] == 'failed' and {'dead'} or {'due', 'running', 'dead'}
local states = {due = 'waiting', running = 'running', dead = 'failed'}
for _, s in ipairs(sets) do
  local ids = redis.call('ZRANGE', p .. s, 0, 999, 'WITHSCORES')
  for i = 1, #ids, 2 do
    local f = redis.call('HMGET', p .. 'job:' .. ids[i], 'name', 'attempts', 'error')
    table.insert(out, ids[i]); table.insert(out, f[1] or ''); table.insert(out, states[s])
    table.insert(out, f[2] or '0'); table.insert(out, ids[i + 1]); table.insert(out, f[3] or '')
  end
end
return out
"#,
);

/// ARGV: id, now. 1 if a job given up was put back.
pub(super) static REDIS_JOB_REQUEUE: LuaScript = LuaScript::new(
    r#"
local p = KEYS[1]
if redis.call('ZREM', p .. 'dead', ARGV[1]) == 0 then return 0 end
redis.call('HSET', p .. 'job:' .. ARGV[1], 'attempts', 0)
redis.call('HDEL', p .. 'job:' .. ARGV[1], 'error')
redis.call('ZADD', p .. 'due', ARGV[2], ARGV[1])
redis.call('INCRBY', p .. 'bytes', redis.call('HSTRLEN', p .. 'job:' .. ARGV[1], 'payload'))
return 1
"#,
);
