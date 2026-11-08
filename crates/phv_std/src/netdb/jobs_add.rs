// ----- Background jobs: adding one --------------------------------------------

use super::handle::{kind_of, Kind, Link, NetDb};
use super::jobs::{
    job_conn_before, job_conn_for, job_redis, job_sized, jobs_prefix, jobs_table, now_ms, JobCap, JobLoad,
    JOB_COUNT_FRESH_MS, JOB_WAIT, REDIS_JOB_ADD,
};
use super::my::{my_run, MyConn};
use super::pg::{pg_message, pg_run, stale, PgConn};
use super::pool::{lock, Pooled};
use super::query_async::PoolUse;
use super::sessions::SessionConn;
use crate::redisclient::Reply;
use phv_vm::{DatabaseSpec, Vm, VmError};
use postgres::types::ToSql;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Adds a job; `Ok(None)` when its key is taken, an error when the store
/// holds `cap` already.
///
/// Far below the caps the job is added at once, with a count of the store at
/// most a second old (one per process); near them it is added under the
/// store's lock (`job_add_locked`), and counted there only (found by the 1.1.1
/// audit: near the cap each job counted the table twice).
#[cfg(test)]
pub(super) fn job_add(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    job: (&str, &str, &str),
    run_at: i64,
    uniq: Option<&str>,
    cap: JobCap,
    deadline: Option<Instant>,
) -> Result<Option<()>, String> {
    job_add_by(spec, ns, job, run_at, uniq, cap, deadline, JobAdder::Script)
}

/// Who adds a job: a script (`enqueue()`), or the scheduler of `[[schedule]]`,
/// whose adds have connections of their own (`PoolUse::Schedule`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum JobAdder {
    Script,
    Schedule,
}

/// How long one try of adding a job with a key waits on the server for the
/// key's row, when another transaction added the same key and has not ended.
const JOB_KEY_TRY_MS: u64 = 10;
/// How many adds of a process may wait for keys held by open transactions on
/// a store at once; the others fail at once (`KEY_HELD`).
pub const JOB_KEY_WAITERS: usize = 8;
const KEY_HELD: &str = "the key is held by a transaction that has not ended";

/// The adds waiting for held keys now, by store (`job_count_key`).
static KEY_WAITERS: LazyLock<Mutex<HashMap<String, usize>>> = LazyLock::new(Default::default);

/// One of the `JOB_KEY_WAITERS` places of a store, while an add waits.
struct KeyWaiter(String);

impl KeyWaiter {
    fn take(store: &str) -> Option<KeyWaiter> {
        let mut waiters = lock(&KEY_WAITERS);
        let n = waiters.entry(store.to_string()).or_insert(0);
        if *n >= JOB_KEY_WAITERS {
            return None;
        }
        *n += 1;
        Some(KeyWaiter(store.to_string()))
    }
}

impl Drop for KeyWaiter {
    fn drop(&mut self) {
        let mut waiters = lock(&KEY_WAITERS);
        if let Some(n) = waiters.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                waiters.remove(&self.0);
            }
        }
    }
}

/// What one try of adding a job came to: done (`None` when its key is taken),
/// or its key is held by a transaction that has not ended yet.
enum Tried {
    Done(Option<()>),
    Held,
}

/// `job_add` for `adder`.
///
/// A job whose key an open transaction holds (added it, not yet ended) is
/// tried again a little later, until the deadline, on a connection taken
/// again each time: no connection is held while it waits, so such waits
/// never take the connections the scheduler, the adds near the cap or the
/// other adds use. At most `JOB_KEY_WAITERS` scripts of a process wait for a
/// store's keys at once (found by the 1.2 audit: four waits took every
/// connection the scheduler used, and a scheduled run was lost).
#[allow(clippy::too_many_arguments)]
pub fn job_add_by(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    job: (&str, &str, &str),
    run_at: i64,
    uniq: Option<&str>,
    cap: JobCap,
    deadline: Option<Instant>,
    adder: JobAdder,
) -> Result<Option<()>, String> {
    // One deadline for all the tries.
    let until = deadline.map_or(Instant::now() + JOB_WAIT, |d| d.min(Instant::now() + JOB_WAIT));
    let mut waiting = None;
    let mut pause = Duration::from_millis(10);
    loop {
        match job_add_once(spec, ns, job, run_at, uniq, cap, until, adder)? {
            Tried::Done(r) => return Ok(r),
            Tried::Held => {
                if adder == JobAdder::Script && waiting.is_none() {
                    let store = job_count_key(spec, &jobs_table(ns));
                    let place = KeyWaiter::take(&store).ok_or_else(|| {
                        format!("{KEY_HELD} ({JOB_KEY_WAITERS} other adds wait for held keys already)")
                    })?;
                    waiting = Some(place);
                }
                if Instant::now() + pause >= until {
                    return Err(KEY_HELD.into());
                }
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(200));
            }
        }
    }
}

/// One try of `job_add_by`, by `until`.
#[allow(clippy::too_many_arguments)]
fn job_add_once(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    job: (&str, &str, &str),
    run_at: i64,
    uniq: Option<&str>,
    cap: JobCap,
    until: Instant,
    adder: JobAdder,
) -> Result<Tried, String> {
    let (id, name, payload) = job;
    let t = jobs_table(ns);
    let size = payload.len();
    let count_key = job_count_key(spec, &t);
    let lock_use = if adder == JobAdder::Schedule { PoolUse::Schedule } else { PoolUse::JobLock };
    // Near the caps by this process's count of the last second: straight to the lock.
    let redis = spec.url.as_deref().is_some_and(|u| kind_of(u) == Kind::Redis);
    if let Some(load) = cached_job_load(&count_key).filter(|l| !redis && !l.far(cap)) {
        if load.full(cap, size) {
            return Err(cap.full());
        }
        return job_add_locked(spec, ns, job, run_at, uniq, cap, Some(until), lock_use).map(Tried::Done);
    }
    // Jobs with a key apart from the others (see `PoolUse::JobKey`), and the
    // scheduler's apart from the scripts'.
    let use_ = match adder {
        JobAdder::Schedule => PoolUse::Schedule,
        JobAdder::Script if uniq.is_some() => PoolUse::JobKey,
        JobAdder::Script => PoolUse::Jobs,
    };
    let (mut conn, until) = job_conn_for(spec, ns, Some(until), use_, JOB_WAIT)?;
    let now = now_ms();
    let load = match &mut conn {
        SessionConn::Redis(c) => {
            let (run_at, cap_s, now_s, bytes_s) =
                (run_at.to_string(), cap.jobs.to_string(), now.to_string(), cap.bytes.to_string());
            let args: [&[u8]; 9] = [
                b"0",
                id.as_bytes(),
                name.as_bytes(),
                payload.as_bytes(),
                run_at.as_bytes(),
                uniq.unwrap_or("").as_bytes(),
                cap_s.as_bytes(),
                now_s.as_bytes(),
                bytes_s.as_bytes(),
            ];
            return match job_redis(c, &REDIS_JOB_ADD, &jobs_prefix(ns), &args, until)? {
                Reply::Int(1) => Ok(Tried::Done(Some(()))),
                Reply::Int(0) => Ok(Tried::Done(None)),
                Reply::Int(_) => Err(cap.full()),
                _ => Err("the server sent an invalid reply".into()),
            };
        }
        SessionConn::Pg(c) => match cached_job_load(&count_key) {
            Some(l) => l,
            None => pg_job_load(c.get(), &t, false)?,
        },
        SessionConn::My(c) => match cached_job_load(&count_key) {
            Some(l) => l,
            None => my_job_load(c.get(), &t)?,
        },
    };
    cache_job_load(&count_key, load, now, false);
    if load.full(cap, size) {
        return Err(cap.full());
    }
    if load.far(cap) {
        return match &mut conn {
            SessionConn::Pg(c) => {
                let conn = c.get();
                // A held key waits `JOB_KEY_TRY_MS` at most (the connection
                // is reset when it goes back to the pool).
                let prelude = conn.store_prelude().filter(|_| uniq.is_some());
                let prelude = prelude.map(|set| format!("{set}; SET lock_timeout = {JOB_KEY_TRY_MS}"));
                match pg_job_insert(conn, &t, false, job, run_at, uniq, now, prelude.as_deref()) {
                    Err(e) if uniq.is_some() && e.code() == Some(&postgres::error::SqlState::LOCK_NOT_AVAILABLE) => {
                        Ok(Tried::Held)
                    }
                    r => r.map(Tried::Done).map_err(|e| pg_message(&e)),
                }
            }
            SessionConn::My(c) => my_job_try_insert(c.get(), &t, job, run_at, uniq, now),
            SessionConn::Redis(_) => unreachable!("Redis added the job above"),
        };
    }
    drop(conn);
    job_add_locked(spec, ns, job, run_at, uniq, cap, Some(until), lock_use).map(Tried::Done)
}

/// `job_add` near the caps: in a transaction of its own that takes the store's
/// lock first, as transactions that add jobs do (see `job_add_in_transaction`),
/// so jobs are counted and added one at a time and none goes past the caps.
/// The count is the one kept in `phv_jobs_counts` while it is fresh, so a
/// burst of jobs near the cap does not count the table for each. On the
/// connections of `use_`: `JobLock` for scripts, `Schedule` for the scheduler.
#[allow(clippy::too_many_arguments)]
fn job_add_locked(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    job: (&str, &str, &str),
    run_at: i64,
    uniq: Option<&str>,
    cap: JobCap,
    deadline: Option<Instant>,
    use_: PoolUse,
) -> Result<Option<()>, String> {
    use mysql::prelude::Queryable;
    let t = jobs_table(ns);
    let (mut conn, until) = job_conn_for(spec, ns, deadline, use_, JOB_WAIT)?;
    let now = now_ms();
    let size = job.2.len();
    let count_key = job_count_key(spec, &t);
    // Waits for the lock no longer than the operation may take.
    let wait_ms = until.saturating_duration_since(Instant::now()).as_millis().max(1);
    let (added, load, at) = match &mut conn {
        SessionConn::Redis(_) => unreachable!("Redis adds jobs in one script"),
        SessionConn::Pg(c) => {
            let conn = c.get();
            // A statement kept from before may have been made stale by a
            // change of the table (its error ends the transaction): once
            // more, then with statements prepared in the transaction.
            match pg_job_add_locked(conn, &t, (job, run_at, uniq, now), cap, wait_ms, true) {
                Err(e) if stale(&e) => {
                    let _ = conn.client.batch_execute("ROLLBACK");
                    conn.forget_statements();
                    pg_job_add_locked(conn, &t, (job, run_at, uniq, now), cap, wait_ms, false)
                }
                r => r,
            }
            .map_err(|e| pg_message(&e))?
        }
        SessionConn::My(c) => {
            let conn = c.get();
            let w = conn.watch();
            let my = |e: mysql::Error| w.msg(e);
            conn.conn.query_drop("SET TRANSACTION ISOLATION LEVEL READ COMMITTED").map_err(my)?;
            conn.conn.query_drop("START TRANSACTION").map_err(my)?;
            my_job_lock(&mut conn.conn, &t).map_err(my)?;
            let (load, at) = match my_job_counted(&mut conn.conn, &t, now).map_err(my)? {
                Some(kept) => kept,
                None => (my_job_load(conn, &t)?, now),
            };
            let added =
                if load.full(cap, size) { None } else { Some(my_job_insert(conn, &t, job, run_at, uniq, now, until)?) };
            let load = if let Some(Some(())) = added { load.plus(JobLoad::one(size)) } else { load };
            my_job_note(&mut conn.conn, &t, load, at).map_err(my)?;
            conn.conn.query_drop("COMMIT").map_err(my)?;
            (added, load, at)
        }
    };
    cache_job_load(&count_key, load, at, true);
    added.ok_or_else(|| cap.full())
}

/// A job, when it runs, its key and the time it was added.
type NewJob<'a> = ((&'a str, &'a str, &'a str), i64, Option<&'a str>, i64);

/// `job_add_locked` on PostgreSQL, in three round trips: the transaction, the
/// lock and the kept count; the job (through a kept statement when
/// `use_kept`, even one prepared before the transaction); the new count and
/// the commit. What was added (`None` when the store is full), what the
/// store holds, and when it was counted.
fn pg_job_add_locked(
    conn: &mut PgConn,
    t: &str,
    new: NewJob<'_>,
    cap: JobCap,
    wait_ms: u128,
    use_kept: bool,
) -> Result<(Option<Option<()>>, JobLoad, i64), postgres::Error> {
    let (job, run_at, uniq, now) = new;
    let size = job.2.len();
    // Read committed: the count after the lock sees every job committed before it.
    let set = conn.store_prelude().map(|s| format!("{s}; ")).unwrap_or_default();
    let sql = format!(
        "{set}BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL lock_timeout = {wait_ms}; \
         SELECT pg_advisory_xact_lock({}); SELECT n, bytes, at FROM phv_jobs_counts WHERE t = '{t}'",
        job_lock_key(t)
    );
    let messages = conn.client.run(None, conn.client.client().simple_query(&sql))?;
    conn.epoch += 1;
    let kept = messages
        .iter()
        .filter_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) if r.len() == 3 => Some(r),
            _ => None,
        })
        .next_back()
        .and_then(|r| Some((r.get(0)?.parse().ok()?, r.get(1)?.parse().ok()?, r.get(2)?.parse().ok()?)));
    let (load, at) = match fresh_count(kept, now) {
        Some(kept) => kept,
        None => (pg_job_load_raw(conn, t, true)?, now),
    };
    let added = if load.full(cap, size) {
        None
    } else {
        Some(pg_job_insert(conn, t, !use_kept, job, run_at, uniq, now, None)?)
    };
    let load = if let Some(Some(())) = added { load.plus(JobLoad::one(size)) } else { load };
    conn.store_batch(&format!(
        "INSERT INTO phv_jobs_counts (t, n, bytes, at) VALUES ('{t}', {}, {}, {at}) \
         ON CONFLICT (t) DO UPDATE SET n = EXCLUDED.n, bytes = EXCLUDED.bytes, at = EXCLUDED.at; COMMIT",
        load.n, load.bytes
    ))?;
    conn.epoch += 1;
    Ok((added, load, at))
}

/// What each store held, by `job_count_key`, and when it was counted.
static JOB_COUNTS: LazyLock<Mutex<HashMap<String, (Instant, JobLoad)>>> = LazyLock::new(Default::default);

pub(super) fn job_count_key(spec: &DatabaseSpec, table: &str) -> String {
    format!("{}|{table}", spec.url.as_deref().unwrap_or_default())
}

/// After a transaction that added jobs ended: its jobs are counted again.
pub(super) fn forget_job_count(key: &str) {
    lock(&JOB_COUNTS).remove(key);
}

/// What the store held when this process last counted it, if that was less
/// than a second ago.
fn cached_job_load(key: &str) -> Option<JobLoad> {
    lock(&JOB_COUNTS)
        .get(key)
        .filter(|(when, _)| when.elapsed() < Duration::from_millis(JOB_COUNT_FRESH_MS as u64))
        .map(|(_, l)| *l)
}

/// Notes what the store held at `at` (Unix ms); a count already noted is kept
/// unless `replace` (one taken under the store's lock).
fn cache_job_load(key: &str, load: JobLoad, at: i64, replace: bool) {
    let age = Duration::from_millis((now_ms() - at).clamp(0, JOB_COUNT_FRESH_MS) as u64);
    let when = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
    let mut counts = lock(&JOB_COUNTS);
    if replace || counts.get(key).is_none_or(|(w, _)| w.elapsed() >= Duration::from_millis(JOB_COUNT_FRESH_MS as u64)) {
        counts.insert(key.to_string(), (when, load));
    }
}

/// The waiting and running jobs of table `t` and their bytes, counted now
/// (inside a transaction, through the statements kept for it: see `PgConn::kept`).
pub(super) fn pg_job_load(conn: &mut PgConn, t: &str, in_transaction: bool) -> Result<JobLoad, String> {
    pg_job_load_raw(conn, t, in_transaction).map_err(|e| pg_message(&e))
}

pub(super) fn pg_job_load_raw(conn: &mut PgConn, t: &str, in_transaction: bool) -> Result<JobLoad, postgres::Error> {
    let bytes = if job_sized(t) { "coalesce(sum(size), 0)::bigint" } else { "0::bigint" };
    let rows = conn.with_statement(
        &format!("SELECT count(*), {bytes} FROM {t} WHERE state IN (0, 1)"),
        true,
        in_transaction,
        None,
        &mut |client, stmt, prelude| client.run(prelude, client.client().query(stmt, &[])),
    )?;
    let get = |i: usize| rows.first().and_then(|r| r.try_get::<_, i64>(i).ok()).unwrap_or(0);
    Ok(JobLoad { n: get(0), bytes: get(1) })
}

pub(super) fn my_job_load(conn: &mut MyConn, t: &str) -> Result<JobLoad, String> {
    use mysql::prelude::Queryable;
    let w = conn.watch();
    let r: Option<(i64, i64)> = conn
        .conn
        .query_first(format!(
            "SELECT count(*), {} FROM {t} WHERE state IN (0, 1)",
            if job_sized(t) { "CAST(coalesce(sum(size), 0) AS SIGNED)" } else { "0" }
        ))
        .map_err(|e| w.msg(e))?;
    let (n, bytes) = r.unwrap_or((0, 0));
    Ok(JobLoad { n, bytes })
}

/// The count kept in `phv_jobs_counts` for table `t`, and when it was taken,
/// if that was less than `JOB_COUNT_FRESH_MS` before `now`.
fn fresh_count(row: Option<(i64, i64, i64)>, now: i64) -> Option<(JobLoad, i64)> {
    let (n, bytes, at) = row?;
    (at > now - JOB_COUNT_FRESH_MS && at <= now + JOB_COUNT_FRESH_MS).then_some((JobLoad { n, bytes }, at))
}

fn pg_job_counted(
    conn: &mut PgConn,
    t: &str,
    in_tx: bool,
    now: i64,
) -> Result<Option<(JobLoad, i64)>, postgres::Error> {
    let rows = conn.with_statement(
        "SELECT n, bytes, at FROM phv_jobs_counts WHERE t = $1",
        true,
        in_tx,
        None,
        &mut |client, stmt, prelude| client.run(prelude, client.client().query(stmt, &[&t])),
    )?;
    let row = match rows.first() {
        Some(r) => Some((r.try_get(0)?, r.try_get(1)?, r.try_get(2)?)),
        None => None,
    };
    Ok(fresh_count(row, now))
}

fn pg_job_note(conn: &mut PgConn, t: &str, in_tx: bool, load: JobLoad, at: i64) -> Result<(), postgres::Error> {
    conn.with_statement(
        "INSERT INTO phv_jobs_counts (t, n, bytes, at) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (t) DO UPDATE SET n = EXCLUDED.n, bytes = EXCLUDED.bytes, at = EXCLUDED.at",
        true,
        in_tx,
        None,
        &mut |client, stmt, prelude| {
            client.run(prelude, client.client().execute(stmt, &[&t, &load.n, &load.bytes, &at]))
        },
    )
    .map(|_| ())
}

fn my_job_counted(conn: &mut mysql::Conn, t: &str, now: i64) -> Result<Option<(JobLoad, i64)>, mysql::Error> {
    use mysql::prelude::Queryable;
    let row: Option<(i64, i64, i64)> = conn.exec_first("SELECT n, bytes, at FROM phv_jobs_counts WHERE t = ?", (t,))?;
    Ok(fresh_count(row, now))
}

fn my_job_note(conn: &mut mysql::Conn, t: &str, load: JobLoad, at: i64) -> Result<(), mysql::Error> {
    use mysql::prelude::Queryable;
    conn.exec_drop(
        "INSERT INTO phv_jobs_counts (t, n, bytes, at) VALUES (?, ?, ?, ?) \
         ON DUPLICATE KEY UPDATE n = VALUES(n), bytes = VALUES(bytes), at = VALUES(at)",
        (t, load.n, load.bytes, at),
    )
}

/// The key of the advisory lock of a jobs table (PostgreSQL).
fn job_lock_key(t: &str) -> i64 {
    let hex = crate::sha256_hex(format!("phv jobs lock\0{t}").as_bytes());
    u64::from_str_radix(&hex[..16], 16).unwrap_or(0) as i64
}

/// Takes the lock of table `t` until the transaction ends (PostgreSQL).
/// Taking it again in the same transaction returns at once.
pub(super) fn pg_job_lock(conn: &mut PgConn, t: &str, prelude: Option<&str>) -> Result<(), postgres::Error> {
    let key = job_lock_key(t);
    conn.with_statement("SELECT pg_advisory_xact_lock($1)", true, true, prelude, &mut |client, stmt, prelude| {
        client.run(prelude, client.client().execute(stmt, &[&key]))
    })
    .map(|_| ())
}

/// The table of the rows that transactions adding jobs lock (MySQL).
pub(super) const MY_JOBS_LOCK_TABLE: &str = "CREATE TABLE IF NOT EXISTS phv_jobs_lock (t varchar(64) PRIMARY KEY)";

/// Takes the lock of table `t` until the transaction ends (MySQL): its row of
/// `phv_jobs_lock`, put back if someone removed it. A row lock is released at
/// commit or rollback, is seen by deadlock detection, and waits at most
/// `innodb_lock_wait_timeout`.
pub(super) fn my_job_lock(conn: &mut mysql::Conn, t: &str) -> Result<(), mysql::Error> {
    use mysql::prelude::Queryable;
    let sql = "SELECT t FROM phv_jobs_lock WHERE t = ? FOR UPDATE";
    if conn.exec_first::<String, _, _>(sql, (t,))?.is_none() {
        conn.exec_drop("INSERT IGNORE INTO phv_jobs_lock (t) VALUES (?)", (t,))?;
        conn.exec_first::<String, _, _>(sql, (t,))?;
    }
    Ok(())
}

/// Adds a job to table `t`; `None` when its key is taken.
#[allow(clippy::too_many_arguments)]
fn pg_job_insert(
    conn: &mut PgConn,
    t: &str,
    in_transaction: bool,
    job: (&str, &str, &str),
    run_at: i64,
    uniq: Option<&str>,
    now: i64,
    prelude: Option<&str>,
) -> Result<Option<()>, postgres::Error> {
    let (id, name, payload) = job;
    let size = payload.len() as i32;
    let sized = job_sized(t);
    let sql = &format!(
        "INSERT INTO {t} (id, name, payload, run_at, attempts, state, uniq, created{}) \
               VALUES ($1, $2, $3, $4, 0, 0, $5, $6{}) ON CONFLICT (uniq) DO NOTHING",
        if sized { ", size" } else { "" },
        if sized { ", $7" } else { "" }
    );
    let n = conn.with_statement(sql, true, in_transaction, prelude, &mut |client, stmt, prelude| {
        let params: [&(dyn ToSql + Sync); 7] = [&id, &name, &payload, &run_at, &uniq, &now, &size];
        client.run(prelude, client.client().execute(stmt, &params[..if sized { 7 } else { 6 }]))
    })?;
    Ok((n > 0).then_some(()))
}

/// What adding a job with a key inside a script's transaction came to.
enum TxInsert {
    Added,
    /// The key is taken: nothing was added.
    Taken,
    /// The key is held by a transaction that has not ended.
    Held,
}

/// The savepoint around a job with a key added inside a script's transaction.
const JOB_SAVEPOINT: &str = "phv_job_key";

/// Adds a job with a key inside a script's transaction (PostgreSQL), waiting
/// for a key that another open transaction holds `wait_ms` at most, as
/// outside a transaction (found by the 1.2 audit: it waited until that
/// transaction ended or the script's time limit, holding the store's lock).
/// The insert runs in a savepoint with a `lock_timeout` of its own (the
/// transaction's own, when that is shorter): a held key leaves the
/// transaction usable, with its `lock_timeout` as it was. When the job was
/// added, the savepoint is still open and the `lock_timeout` is still set:
/// the caller ends them (`pg_job_tx_end`) once it knows the job fits. Returns
/// what was done and the transaction's own `lock_timeout` (ms).
#[allow(clippy::too_many_arguments)]
fn pg_job_tx_insert(
    conn: &mut PgConn,
    t: &str,
    job: (&str, &str, &str),
    run_at: i64,
    uniq: &str,
    now: i64,
    prelude: Option<&str>,
    wait_ms: u128,
) -> Result<(TxInsert, u64), postgres::Error> {
    use tokio_postgres::SimpleQueryMessage;
    let set = prelude.map(|p| format!("{p}; ")).unwrap_or_default();
    // `s.v` is read before `set_config` runs: the transaction's own value.
    let sql = format!(
        "{set}SAVEPOINT {JOB_SAVEPOINT}; \
         SELECT set_config('lock_timeout', (CASE WHEN s.v > 0 AND s.v < {wait_ms} THEN s.v ELSE {wait_ms} END)::text, \
         true), s.v::text FROM (SELECT setting::bigint AS v FROM pg_settings WHERE name = 'lock_timeout') s"
    );
    let messages = conn.client.run(None, conn.client.client().simple_query(&sql))?;
    let old: u64 = messages
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) if r.len() == 2 => r.get(1),
            _ => None,
        })
        .next_back()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    match pg_job_insert(conn, t, true, job, run_at, Some(uniq), now, None) {
        Ok(Some(())) => Ok((TxInsert::Added, old)),
        Ok(None) => {
            pg_job_tx_end(conn, true, old, None)?;
            Ok((TxInsert::Taken, old))
        }
        Err(e) if e.code() == Some(&postgres::error::SqlState::LOCK_NOT_AVAILABLE) => {
            pg_job_tx_end(conn, false, old, None)?;
            Ok((TxInsert::Held, old))
        }
        Err(e) => Err(e),
    }
}

/// Ends the savepoint of `pg_job_tx_insert`: released when the job is
/// `kept`, else rolled back to (which also puts the `lock_timeout` back).
fn pg_job_tx_end(
    conn: &mut PgConn,
    kept: bool,
    old_lock_timeout: u64,
    prelude: Option<&str>,
) -> Result<(), postgres::Error> {
    let set = prelude.map(|p| format!("{p}; ")).unwrap_or_default();
    let sql = if kept {
        format!("{set}RELEASE SAVEPOINT {JOB_SAVEPOINT}; SET LOCAL lock_timeout = {old_lock_timeout}")
    } else {
        format!("{set}ROLLBACK TO SAVEPOINT {JOB_SAVEPOINT}; RELEASE SAVEPOINT {JOB_SAVEPOINT}")
    };
    conn.client.batch_execute(&sql)
}

/// What a look at a job's key came to (MySQL).
enum KeyProbe {
    Free,
    Taken,
    Held,
}

/// Looks for the key `u` in table `t` without waiting (`FOR UPDATE NOWAIT`):
/// a key another transaction added and has not committed is `Held`.
fn my_key_probe(conn: &mut mysql::Conn, t: &str, u: &str) -> Result<KeyProbe, mysql::Error> {
    use mysql::prelude::Queryable;
    let probe = format!("SELECT id FROM {t} WHERE uniq = ? FOR UPDATE NOWAIT");
    match conn.exec_first::<String, _, _>(probe, (u,)) {
        Ok(Some(_)) => Ok(KeyProbe::Taken),
        Ok(None) => Ok(KeyProbe::Free),
        // 3572: the row is locked (MySQL); 1205: the same in MariaDB.
        Err(mysql::Error::MySqlError(e)) if e.code == 3572 || e.code == 1205 => Ok(KeyProbe::Held),
        Err(e) => Err(e),
    }
}

/// Whether a script's transaction sees the key `u` in table `t` (MySQL): a
/// plain read, which takes no lock, and sees the transaction's own jobs.
fn my_key_seen(conn: &mut mysql::Conn, t: &str, u: &str) -> Result<bool, mysql::Error> {
    use mysql::prelude::Queryable;
    let seen: Option<String> = conn.exec_first(format!("SELECT id FROM {t} WHERE uniq = ?"), (u,))?;
    Ok(seen.is_some())
}

/// `my_key_probe` on a connection of the store, taken for this one look
/// (`PoolUse::JobKey`): a statement of its own, so the look leaves no lock
/// behind. Inside a script's transaction the probe cannot run on the
/// script's connection: with no row found it would lock the gap of the key's
/// index until the transaction ends, where the jobs without a key go, so
/// every other add of the site would wait for the transaction (found by the
/// 1.2 tests, the shape of the 1.1.1 finding).
fn my_key_probe_store(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    t: &str,
    u: &str,
    until: Instant,
) -> Result<KeyProbe, String> {
    let (mut conn, _) = job_conn_for(spec, ns, Some(until), PoolUse::JobKey, JOB_WAIT)?;
    match &mut conn {
        SessionConn::My(c) => {
            let conn = c.get();
            let w = conn.watch();
            my_key_probe(&mut conn.conn, t, u).map_err(|e| w.msg(e))
        }
        _ => Err("the job store is not a MySQL server".into()),
    }
}

/// Adds a job on a connection of the store, by `until`.
///
/// A job with a key first looks for the key without waiting (`NOWAIT`), and
/// while another transaction holds it (added it, not yet ended), looks again
/// a little later. An `INSERT` would wait in the server's queue for that row,
/// and while it waits InnoDB also locks the gap before the row, where jobs
/// without a key go when the key is the lowest: every other job would wait
/// too (found by the 1.1.1 audit).
#[allow(clippy::too_many_arguments)]
fn my_job_insert(
    conn: &mut MyConn,
    t: &str,
    job: (&str, &str, &str),
    run_at: i64,
    uniq: Option<&str>,
    now: i64,
    until: Instant,
) -> Result<Option<()>, String> {
    let mut pause = Duration::from_millis(10);
    loop {
        match my_job_try_insert(conn, t, job, run_at, uniq, now)? {
            Tried::Done(r) => return Ok(r),
            Tried::Held => {
                if Instant::now() + pause >= until {
                    return Err(KEY_HELD.into());
                }
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(200));
            }
        }
    }
}

/// One try of `my_job_insert`: a key that another transaction holds is not waited for.
fn my_job_try_insert(
    conn: &mut MyConn,
    t: &str,
    job: (&str, &str, &str),
    run_at: i64,
    uniq: Option<&str>,
    now: i64,
) -> Result<Tried, String> {
    let w = conn.watch();
    if let Some(u) = uniq {
        match my_key_probe(&mut conn.conn, t, u).map_err(|e| w.msg(e))? {
            KeyProbe::Taken => return Ok(Tried::Done(None)),
            KeyProbe::Held => return Ok(Tried::Held),
            KeyProbe::Free => {}
        }
    }
    my_job_insert_on(&mut conn.conn, t, job, run_at, uniq, now).map(Tried::Done).map_err(|e| w.msg(e))
}

fn my_job_insert_on(
    conn: &mut mysql::Conn,
    t: &str,
    job: (&str, &str, &str),
    run_at: i64,
    uniq: Option<&str>,
    now: i64,
) -> Result<Option<()>, mysql::Error> {
    use mysql::prelude::Queryable;
    let (id, name, payload) = job;
    if job_sized(t) {
        conn.exec_drop(
            format!(
                "INSERT IGNORE INTO {t} (id, name, payload, run_at, attempts, state, uniq, created, size) \
                 VALUES (?, ?, ?, ?, 0, 0, ?, ?, ?)"
            ),
            (id, name, payload, run_at, uniq, now, payload.len() as i64),
        )?;
    } else {
        conn.exec_drop(
            format!(
                "INSERT IGNORE INTO {t} (id, name, payload, run_at, attempts, state, uniq, created) \
                 VALUES (?, ?, ?, ?, 0, 0, ?, ?)"
            ),
            (id, name, payload, run_at, uniq, now),
        )?;
    }
    Ok((conn.affected_rows() > 0).then_some(()))
}

/// The jobs a script's open transaction added, counted with those the store
/// held when the transaction took the store's lock (which a transaction may
/// not see: they are counted on a connection of the store, outside it).
pub(super) struct JobsTx {
    held: JobLoad,
    added: JobLoad,
    /// The count to forget when the transaction ends (`forget_job_count`).
    count_key: String,
}

impl JobsTx {
    pub(super) fn ended(self) {
        if self.added.n > 0 {
            forget_job_count(&self.count_key);
        }
    }
}

/// For a script's transaction that has just taken the store's lock, which it
/// holds until it ends: what the store holds, on a connection of the store
/// (far below the caps, this process's count of the last second; else the
/// count kept in `phv_jobs_counts` while it is fresh, or a count taken now).
/// The kept count is then marked as stale, as what the transaction adds is
/// known only when it ends: the next to take the lock counts again.
fn job_tx_load(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    t: &str,
    cap: JobCap,
    deadline: Option<Instant>,
) -> Result<JobLoad, String> {
    let far = cached_job_load(&job_count_key(spec, t)).filter(|l| l.far(cap));
    let (mut conn, _) = job_conn_before(spec, ns, deadline)?;
    let now = now_ms();
    match &mut conn {
        SessionConn::Pg(c) => {
            let conn = c.get();
            let pg = |e: postgres::Error| pg_message(&e);
            let load = match far {
                Some(load) => load,
                None => match pg_job_counted(conn, t, false, now).map_err(pg)? {
                    Some((load, _)) => load,
                    None => pg_job_load(conn, t, false)?,
                },
            };
            pg_job_note(conn, t, false, load, 0).map_err(pg)?;
            Ok(load)
        }
        SessionConn::My(c) => {
            let conn = c.get();
            let w = conn.watch();
            let load = match far {
                Some(load) => load,
                None => match my_job_counted(&mut conn.conn, t, now).map_err(|e| w.msg(e))? {
                    Some((load, _)) => load,
                    None => my_job_load(conn, t)?,
                },
            };
            my_job_note(&mut conn.conn, t, load, 0).map_err(|e| w.msg(e))?;
            Ok(load)
        }
        SessionConn::Redis(_) => Ok(JobLoad::default()),
    }
}

/// Adds a job on the script's own connection to `db` when the script is in
/// a `transaction()` on it (PostgreSQL, MySQL): the job is added only if the
/// transaction commits. `None` when the script has no such transaction.
///
/// The caps hold for transactions running at once too (found by the 1.1
/// audit: each counted only what it could see, and passed the cap by what it
/// added): each `enqueue()` first takes the store's lock, which the
/// transaction holds until it ends, so transactions that add jobs to a store
/// run their `enqueue()`s one after the other. What the store held is counted
/// once, when the transaction first takes the lock (`job_tx_load`), and what
/// it adds is added to it (found by the 1.1.1 audit: near the cap each
/// `enqueue()` of a transaction counted the table again).
///
/// A job with a key that another open transaction holds is waited for as
/// outside a transaction: no longer than the script may still run, 10 seconds
/// at most (`JOB_WAIT`), then `KEY_HELD`; and before the store's lock is
/// taken, so a waiting page holds no other add of the site (found by the 1.2
/// audit: the wait lasted until that transaction ended or the time limit,
/// with the store's lock held). PostgreSQL waits on the insert itself, with a
/// `lock_timeout` in a savepoint (`pg_job_tx_insert`); MySQL looks at the key
/// without waiting, on a connection of the store, again a little later
/// (`my_key_probe_store`; at most `JOB_KEY_WAITERS` such waits at once, as
/// outside a transaction).
pub(crate) fn job_add_in_transaction(
    vm: &mut Vm,
    spec: &DatabaseSpec,
    db: &str,
    job: (&str, &str, &str),
    run_at: i64,
    uniq: Option<&str>,
    cap: JobCap,
) -> Option<Result<Option<()>, VmError>> {
    let ns = vm.host.store_namespace.clone();
    let t = jobs_table(ns.as_deref());
    let link = vm
        .resources
        .iter()
        .flatten()
        .filter_map(|r| r.downcast_ref::<NetDb>())
        .find(|d| d.name == db)
        .map(|d| d.link.clone())?;
    let mut link = link.borrow_mut();
    let now = now_ms();
    let size = job.2.len();
    let vm: &Vm = vm;
    let full = || vm.throw("RuntimeException", format!("enqueue(): {}", cap.full()));
    let held = || {
        job_tx_load(spec, ns.as_deref(), &t, cap, vm.deadline())
            .map_err(|e| vm.throw("DatabaseError", format!("enqueue(): {e}")))
    };
    let count_key = job_count_key(spec, &t);
    // Whether the job fits, with what the transaction added so far.
    let fits = |tx: &JobsTx| !tx.held.plus(tx.added).full(cap, size);
    // How long a held key is waited for: as outside a transaction.
    let wait = vm.deadline().map_or(JOB_WAIT, |d| d.saturating_duration_since(Instant::now()).min(JOB_WAIT));
    let key_held = || vm.check_limits().and_then(|_| Err(vm.throw("DatabaseError", format!("enqueue(): {KEY_HELD}"))));
    let r = match &mut *link {
        Link::Pg(c) => {
            if !(c.depth > 0 || c.get().in_own_transaction) {
                return None;
            }
            (|| {
                // A job with a key is added first, in a savepoint: the
                // transaction's `lock_timeout`, to put back with the next
                // statement, while the savepoint is open.
                let mut saved = None;
                if let Some(u) = uniq {
                    let wait_ms = wait.as_millis().max(1);
                    let (r, old) = pg_run(vm, c, |c, prelude| {
                        pg_job_tx_insert(c.get(), &t, job, run_at, u, now, prelude, wait_ms)
                    })?;
                    match r {
                        TxInsert::Taken => return Ok(None),
                        TxInsert::Held => return key_held(),
                        TxInsert::Added => saved = Some(old),
                    }
                }
                let restore = saved.map(|old| format!("SET LOCAL lock_timeout = {old}"));
                let end = |c: &mut Pooled<PgConn>, kept: bool| {
                    pg_run(vm, c, |c, prelude| pg_job_tx_end(c.get(), kept, saved.unwrap_or(0), prelude))
                };
                pg_run(vm, c, |c, prelude| {
                    let prelude = preludes(prelude, restore.as_deref());
                    pg_job_lock(c.get(), &t, prelude.as_deref())
                })?;
                if c.get().jobs_tx.is_none() {
                    let held = match held() {
                        Ok(held) => held,
                        Err(e) => {
                            if saved.is_some() {
                                let _ = end(c, false);
                            }
                            return Err(e);
                        }
                    };
                    c.get().jobs_tx = Some(JobsTx { held, added: JobLoad::default(), count_key });
                }
                if !c.get().jobs_tx.as_ref().is_some_and(fits) {
                    if saved.is_some() {
                        end(c, false)?;
                    }
                    return Err(full());
                }
                let r = match saved {
                    Some(_) => {
                        end(c, true)?;
                        Some(())
                    }
                    None => {
                        pg_run(vm, c, |c, prelude| pg_job_insert(c.get(), &t, true, job, run_at, uniq, now, prelude))?
                    }
                };
                if let (Some(()), Some(tx)) = (r, c.get().jobs_tx.as_mut()) {
                    tx.added = tx.added.plus(JobLoad::one(size));
                }
                Ok(r)
            })()
        }
        Link::My(c) if c.depth > 0 => (|| {
            if let Some(u) = uniq {
                // A key the transaction sees (its own jobs too): nothing to add.
                if my_run(vm, c, |conn| my_key_seen(conn, &t, u))? {
                    return Ok(None);
                }
                let until = Instant::now() + wait;
                let mut pause = Duration::from_millis(10);
                let mut waiting = None;
                loop {
                    vm.check_limits()?;
                    let probe = my_key_probe_store(spec, ns.as_deref(), &t, u, until)
                        .map_err(|e| vm.throw("DatabaseError", format!("enqueue(): {e}")))?;
                    match probe {
                        KeyProbe::Taken => return Ok(None),
                        KeyProbe::Free => break,
                        KeyProbe::Held => {
                            if waiting.is_none() {
                                let place = KeyWaiter::take(&count_key).ok_or_else(|| {
                                    let why =
                                        format!("{KEY_HELD} ({JOB_KEY_WAITERS} other adds wait for held keys already)");
                                    vm.throw("DatabaseError", format!("enqueue(): {why}"))
                                })?;
                                waiting = Some(place);
                            }
                            if Instant::now() + pause >= until {
                                return key_held();
                            }
                            std::thread::sleep(pause);
                            pause = (pause * 2).min(Duration::from_millis(200));
                        }
                    }
                }
            }
            my_run(vm, c, |conn| my_job_lock(conn, &t))?;
            if c.get().jobs_tx.is_none() {
                let held = held()?;
                c.get().jobs_tx = Some(JobsTx { held, added: JobLoad::default(), count_key });
            }
            if !c.get().jobs_tx.as_ref().is_some_and(fits) {
                return Err(full());
            }
            let r = my_run(vm, c, |conn| my_job_insert_on(conn, &t, job, run_at, uniq, now))?;
            if let (Some(()), Some(tx)) = (r, c.get().jobs_tx.as_mut()) {
                tx.added = tx.added.plus(JobLoad::one(size));
            }
            Ok(r)
        })(),
        _ => return None,
    };
    Some(r)
}

/// `first` and `then` as one prelude (see `PgClient::run`).
fn preludes(first: Option<&str>, then: Option<&str>) -> Option<String> {
    match (first, then) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        (None, None) => None,
    }
}
