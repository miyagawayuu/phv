// ----- Background jobs: taking, ending, sweeping, listing, adopting ---------

use super::handle::{kind_of, tls_init, Kind};
use super::jobs::{
    job_conn, job_conn_before, job_conn_within, job_redis, job_redis_result, job_sized, jobs_prefix, jobs_table,
    now_ms, JobCap, JobInfo, JobLoad, JobRow, JOB_KEEP_DEAD_MS, JOB_KEEP_DONE_MS, JOB_MAX_DEAD, JOB_MAX_KEPT,
    JOB_SWEEP_BATCH, JOB_TABLES, JOB_WAIT, REDIS_JOB_END, REDIS_JOB_LIST, REDIS_JOB_REQUEUE, REDIS_JOB_SWEEP,
    REDIS_JOB_TAKE,
};
use super::jobs_add::{
    forget_job_count, job_count_key, my_job_load, my_job_lock, pg_job_load, pg_job_load_raw, pg_job_lock,
};
use super::my::MyConn;
use super::pg::{pg_message, PgConn};
use super::pool::lock;
use super::query_async::PoolUse;
use super::sessions::{my_no_table, pg_no_table, store_conn, LuaScript, SessionConn};
use crate::redisclient::Reply;
use phv_vm::DatabaseSpec;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Makes sure the store's table exists (before a script's transaction adds to it:
/// MySQL would commit the transaction to create it).
/// Once they are, it takes no connection (Redis needs no table).
pub fn job_store_ready(spec: &DatabaseSpec, ns: Option<&str>, deadline: Option<Instant>) -> Result<(), String> {
    if let Some(url) = &spec.url {
        if kind_of(url) == Kind::Redis || lock(&JOB_TABLES).contains(&format!("{url}|{}", jobs_table(ns))) {
            return Ok(());
        }
    }
    job_conn_before(spec, ns, deadline).map(|_| ())
}

/// The largest payload a job is taken with: far above what `enqueue()` adds
/// (64 KB) and below what one reply of any store may hold (8 MB on
/// PostgreSQL and MySQL, 32 MB on Redis).
const JOB_MAX_TAKEN: usize = 1 << 20;

/// How many jobs whose payload is over `JOB_MAX_TAKEN` one `job_take` gives
/// up before it leaves the rest to the next.
const JOB_TAKE_TRIES: usize = 16;

/// The longest name of a job (`jobs::job_name`), and the longest id a Redis
/// store is asked to send (PHV's are 32 hex digits).
const JOB_MAX_NAME: i64 = 64;

/// What `job_take_once` took: a job to run, one whose payload or name (of so
/// many bytes) was not read, or an entry of a Redis store that was removed
/// because its id (of so many bytes) is none PHV writes.
enum Taken {
    Job(JobRow),
    TooLarge(JobRow, i64),
    LongName(JobRow, i64),
    Removed(i64),
}

impl Taken {
    /// The job leased with `row`, whose payload of `size` bytes is `payload`
    /// unless it was over `JOB_MAX_TAKEN` (then the store did not send it).
    fn of(mut row: JobRow, payload: Option<String>, size: i64) -> Taken {
        match payload {
            Some(p) => {
                row.payload = p;
                Taken::Job(row)
            }
            None => Taken::TooLarge(row, size),
        }
    }
}

/// Takes the next job due (or one whose lease ran out) for `lease_ms`.
///
/// A job is leased first and its payload read after, and only when it is at
/// most `JOB_MAX_TAKEN` bytes: a larger one (no `enqueue()` adds it; another
/// writer of the database, or a damaged row) is given up as failed, with an
/// error that says so, and the next job is taken. (Found by the 21st
/// assessment: MySQL read the payload in the transaction that chose the job;
/// a row over the 8 MB a reply may hold failed the read, the transaction
/// ended, the row stayed first in line, and every runner of every process
/// took it again: no job of the site ran any more. PostgreSQL and Redis
/// leased it first, and failed on it once per lease, for ever.) The same
/// holds for the other texts a store sends with a job: the name and the id
/// are 64 and 32 bytes at most in the tables of PostgreSQL and MySQL; a
/// Redis store sends neither when longer than 64 bytes (a job with such a
/// name is given up, an entry with such an id removed).
pub fn job_take(spec: &DatabaseSpec, ns: Option<&str>, lease_ms: i64) -> Result<Option<JobRow>, String> {
    for _ in 0..JOB_TAKE_TRIES {
        match job_take_once(spec, ns, lease_ms)? {
            None => return Ok(None),
            Some(Taken::Job(job)) => return Ok(Some(job)),
            Some(Taken::TooLarge(job, size)) => {
                let error = format!(
                    "the payload ({size} bytes) is over the {} MB a job is run with: enqueue() did not add it",
                    JOB_MAX_TAKEN >> 20
                );
                eprintln!("warning: job {} ({}) was given up: {error}", job.name.escape_debug(), job.id.escape_debug());
                job_end(spec, ns, &job, JobEnd::Dead { error: &error })?;
            }
            Some(Taken::LongName(job, size)) => {
                let error =
                    format!("the name ({size} bytes) is over the {JOB_MAX_NAME} bytes of a job's name: enqueue() did not add it");
                eprintln!("warning: job {} was given up: {error}", job.id.escape_debug());
                job_end(spec, ns, &job, JobEnd::Dead { error: &error })?;
            }
            Some(Taken::Removed(size)) => {
                eprintln!(
                    "warning: an entry of the job store was removed: its id ({size} bytes) is over the {JOB_MAX_NAME} \
                     bytes a job is taken with: enqueue() did not add it"
                );
            }
        }
    }
    Ok(None)
}

fn job_take_once(spec: &DatabaseSpec, ns: Option<&str>, lease_ms: i64) -> Result<Option<Taken>, String> {
    let t = jobs_table(ns);
    let (mut conn, until) = job_conn(spec, ns)?;
    let now = now_ms();
    let token = crate::random_hex(16);
    let lease_until = now + lease_ms;
    let cap = JOB_MAX_TAKEN as i64;
    match &mut conn {
        SessionConn::Redis(c) => {
            let (now_s, until_s, cap_s) = (now.to_string(), lease_until.to_string(), cap.to_string());
            let args: [&[u8]; 5] = [b"0", now_s.as_bytes(), until_s.as_bytes(), token.as_bytes(), cap_s.as_bytes()];
            match job_redis(c, &REDIS_JOB_TAKE, &jobs_prefix(ns), &args, until)? {
                Reply::Nil => Ok(None),
                Reply::Array(items) => {
                    let text = |r: &Reply| match r {
                        Reply::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
                        _ => String::new(),
                    };
                    let attempts = match items.get(3) {
                        Some(Reply::Int(n)) => *n as u32,
                        _ => 1,
                    };
                    let (Some(id), Some(name), Some(payload)) = (items.first(), items.get(1), items.get(2)) else {
                        return Err("the server sent an invalid reply".into());
                    };
                    let number = |i: usize| match items.get(i) {
                        Some(Reply::Int(n)) => *n,
                        _ => 0,
                    };
                    if matches!(id, Reply::Nil) {
                        return Ok(Some(Taken::Removed(number(6))));
                    }
                    let (size, named) = (number(4), number(5));
                    if named > JOB_MAX_NAME {
                        let row = JobRow {
                            id: text(id),
                            name: String::new(),
                            payload: String::new(),
                            attempts,
                            lease: token,
                        };
                        return Ok(Some(Taken::LongName(row, named)));
                    }
                    // A job without a payload field has an empty one, as before.
                    let payload = (size <= cap).then(|| text(payload));
                    let row = JobRow { id: text(id), name: text(name), payload: String::new(), attempts, lease: token };
                    Ok(Some(Taken::of(row, payload, size)))
                }
                _ => Err("the server sent an invalid reply".into()),
            }
        }
        SessionConn::Pg(c) => {
            let rows = c
                .get()
                .query_kept(
                    &format!(
                        "UPDATE {t} SET state = 1, attempts = LEAST(attempts, 1000) + 1, lease = $1, lease_until = $2 \
                     WHERE id = (SELECT id FROM {t} WHERE (state = 0 AND run_at <= $3) \
                     OR (state = 1 AND lease_until < $3) ORDER BY run_at LIMIT 1 FOR UPDATE SKIP LOCKED) \
                     RETURNING id, name, CASE WHEN octet_length(payload) <= $4::bigint THEN payload END, attempts, \
                     octet_length(payload)::bigint"
                    ),
                    &[&token, &lease_until, &now, &cap],
                )
                .map_err(|e| pg_message(&e))?;
            let Some(r) = rows.first() else { return Ok(None) };
            let get = |e: postgres::Error| pg_message(&e);
            let row = JobRow {
                id: r.try_get(0).map_err(get)?,
                name: r.try_get(1).map_err(get)?,
                payload: String::new(),
                attempts: r.try_get::<_, i32>(3).map_err(get)?.max(0) as u32,
                lease: token,
            };
            Ok(Some(Taken::of(row, r.try_get(2).map_err(get)?, r.try_get(4).map_err(get)?)))
        }
        SessionConn::My(c) => {
            use mysql::prelude::Queryable;
            let w = c.get().watch();
            let my = |e: mysql::Error| w.msg(e);
            let conn = &mut c.get().conn;
            // The job is chosen and leased without its payload, which is
            // read once the lease is committed (see `job_take`).
            conn.query_drop("START TRANSACTION").map_err(my)?;
            let found: Option<(String, i64)> = conn
                .exec_first(
                    format!(
                        "SELECT id, attempts FROM {t} WHERE (state = 0 AND run_at <= ?) \
                     OR (state = 1 AND lease_until < ?) ORDER BY run_at LIMIT 1 FOR UPDATE SKIP LOCKED"
                    ),
                    (now, now),
                )
                .map_err(my)?;
            let Some((id, attempts)) = found else {
                conn.query_drop("COMMIT").map_err(my)?;
                return Ok(None);
            };
            conn.exec_drop(
                format!("UPDATE {t} SET state = 1, attempts = LEAST(attempts, 1000) + 1, lease = ?, lease_until = ? WHERE id = ?"),
                (&token, lease_until, &id),
            )
            .map_err(my)?;
            conn.query_drop("COMMIT").map_err(my)?;
            let read: Option<(String, Option<String>, i64)> = conn
                .exec_first(
                    format!(
                        "SELECT name, IF(LENGTH(payload) <= ?, payload, NULL), LENGTH(payload) FROM {t} \
                         WHERE id = ? AND lease = ?"
                    ),
                    (cap, &id, &token),
                )
                .map_err(my)?;
            // Gone meanwhile (removed by hand): nothing was taken.
            let Some((name, payload, size)) = read else { return Ok(None) };
            let attempts = (attempts.min(1000) + 1).max(0) as u32;
            let row = JobRow { id, name, payload: String::new(), attempts, lease: token };
            Ok(Some(Taken::of(row, payload, size)))
        }
    }
}

/// How a run ended.
pub enum JobEnd<'a> {
    /// It finished; a job with a key is kept a day (`keyed`).
    Done { keyed: bool },
    /// It failed and runs again at `at` (Unix ms).
    Retry { at: i64, error: &'a str },
    /// It failed and is given up.
    Dead { error: &'a str },
}

/// Records how a job's run ended; `false` when its lease was lost meanwhile.
pub fn job_end(spec: &DatabaseSpec, ns: Option<&str>, job: &JobRow, end: JobEnd) -> Result<bool, String> {
    let t = jobs_table(ns);
    let (mut conn, until) = job_conn(spec, ns)?;
    let now = now_ms();
    let zero = if job_sized(&t) { ", size = 0" } else { "" };
    match &mut conn {
        SessionConn::Redis(c) => {
            let (mode, at, error) = match &end {
                JobEnd::Done { .. } => ("done", now, ""),
                JobEnd::Retry { at, error } => ("retry", *at, *error),
                JobEnd::Dead { error } => ("dead", now, *error),
            };
            let (at, cap, kept) = (at.to_string(), JOB_MAX_DEAD.to_string(), JOB_MAX_KEPT.to_string());
            let args: [&[u8]; 8] = [
                b"0",
                job.id.as_bytes(),
                job.lease.as_bytes(),
                mode.as_bytes(),
                at.as_bytes(),
                error.as_bytes(),
                cap.as_bytes(),
                kept.as_bytes(),
            ];
            match job_redis(c, &REDIS_JOB_END, &jobs_prefix(ns), &args, until)? {
                Reply::Int(n) => Ok(n == 1),
                _ => Err("the server sent an invalid reply".into()),
            }
        }
        SessionConn::Pg(c) => {
            let conn = c.get();
            let (id, lease) = (&job.id, &job.lease);
            let n = match end {
                JobEnd::Done { keyed: false } => {
                    conn.execute_kept(&format!("DELETE FROM {t} WHERE id = $1 AND lease = $2"), &[id, lease])
                }
                // Kept only when the job has a key (found by the 1.1 audit: every
                // finished job was kept a day).
                JobEnd::Done { keyed: true } => conn
                    .execute_kept(
                        &format!(
                            "UPDATE {t} SET state = 3, run_at = $3, payload = ''{zero}, lease = NULL \
                             WHERE id = $1 AND lease = $2 AND uniq IS NOT NULL"
                        ),
                        &[id, lease, &now],
                    )
                    .and_then(|n| match n {
                        0 => conn.execute_kept(&format!("DELETE FROM {t} WHERE id = $1 AND lease = $2"), &[id, lease]),
                        n => Ok(n),
                    }),
                JobEnd::Retry { at, error } => conn.execute_kept(
                    &format!(
                        "UPDATE {t} SET state = 0, run_at = $3, lease = NULL, error = $4 WHERE id = $1 AND lease = $2"
                    ),
                    &[id, lease, &at, &error],
                ),
                JobEnd::Dead { error } => conn.execute_kept(
                    &format!(
                        "UPDATE {t} SET state = 2, run_at = $3, lease = NULL, error = $4 WHERE id = $1 AND lease = $2"
                    ),
                    &[id, lease, &now, &error],
                ),
            };
            n.map(|n| n > 0).map_err(|e| pg_message(&e))
        }
        SessionConn::My(c) => {
            use mysql::prelude::Queryable;
            let w = c.get().watch();
            let conn = &mut c.get().conn;
            let (id, lease) = (&job.id, &job.lease);
            let r = match end {
                JobEnd::Done { keyed: false } => {
                    conn.exec_drop(format!("DELETE FROM {t} WHERE id = ? AND lease = ?"), (id, lease))
                }
                JobEnd::Done { keyed: true } => conn
                    .exec_drop(
                        format!(
                            "UPDATE {t} SET state = 3, run_at = ?, payload = ''{zero}, lease = NULL \
                             WHERE id = ? AND lease = ? AND uniq IS NOT NULL"
                        ),
                        (now, id, lease),
                    )
                    .and_then(|()| match conn.affected_rows() {
                        0 => conn.exec_drop(format!("DELETE FROM {t} WHERE id = ? AND lease = ?"), (id, lease)),
                        _ => Ok(()),
                    }),
                JobEnd::Retry { at, error } => conn.exec_drop(
                    format!("UPDATE {t} SET state = 0, run_at = ?, lease = NULL, error = ? WHERE id = ? AND lease = ?"),
                    (at, error, id, lease),
                ),
                JobEnd::Dead { error } => conn.exec_drop(
                    format!("UPDATE {t} SET state = 2, run_at = ?, lease = NULL, error = ? WHERE id = ? AND lease = ?"),
                    (now, error, id, lease),
                ),
            };
            r.map_err(|e| w.msg(e))?;
            Ok(conn.affected_rows() > 0)
        }
    }
}

/// Removes jobs given up more than a week ago (and past `JOB_MAX_DEAD`), and
/// the kept rows of keyed jobs after a day (and past `JOB_MAX_KEPT`), at most
/// `JOB_SWEEP_BATCH` rows per statement; what is left when the operation's
/// time is up is removed by the next sweep.
pub fn job_sweep(spec: &DatabaseSpec, ns: Option<&str>) -> Result<(), String> {
    let t = jobs_table(ns);
    let (mut conn, until) = job_conn(spec, ns)?;
    let now = now_ms();
    let (done_before, dead_before) = (now - JOB_KEEP_DONE_MS, now - JOB_KEEP_DEAD_MS);
    let batch = JOB_SWEEP_BATCH;
    // Runs one batch after another while each removes a full batch and time is left.
    let in_batches = |run: &mut dyn FnMut() -> Result<i64, String>| -> Result<(), String> {
        while until.saturating_duration_since(Instant::now()) > JOB_WAIT / 4 {
            if run()? < batch {
                break;
            }
        }
        Ok(())
    };
    match &mut conn {
        SessionConn::Redis(c) => {
            let (before, kept_before) = (dead_before.to_string(), done_before.to_string());
            let args: [&[u8]; 3] = [b"0", before.as_bytes(), kept_before.as_bytes()];
            job_redis(c, &REDIS_JOB_SWEEP, &jobs_prefix(ns), &args, until).map(|_| ())
        }
        SessionConn::Pg(c) => {
            let conn = c.get();
            let pg = |e: postgres::Error| pg_message(&e);
            for (state, before) in [(3, done_before), (2, dead_before)] {
                let sql = format!(
                    "DELETE FROM {t} WHERE id IN (SELECT id FROM {t} WHERE state = {state} AND run_at < $1 LIMIT $2)"
                );
                in_batches(&mut || conn.execute_kept(&sql, &[&before, &batch]).map(|n| n as i64).map_err(pg))?;
            }
            for (state, keep) in [(2, JOB_MAX_DEAD), (3, JOB_MAX_KEPT)] {
                let sql = format!(
                    "DELETE FROM {t} WHERE id IN (SELECT id FROM {t} WHERE state = {state} \
                     ORDER BY run_at DESC OFFSET $1 LIMIT $2)"
                );
                let keep = keep as i64;
                in_batches(&mut || conn.execute_kept(&sql, &[&keep, &batch]).map(|n| n as i64).map_err(pg))?;
            }
            Ok(())
        }
        SessionConn::My(c) => {
            use mysql::prelude::Queryable;
            let w = c.get().watch();
            let conn = &mut c.get().conn;
            let mut delete = |sql: &str, at: i64| -> Result<i64, String> {
                conn.exec_drop(sql, (at, batch)).map_err(|e| w.msg(e))?;
                Ok(conn.affected_rows() as i64)
            };
            for (state, before) in [(3, done_before), (2, dead_before)] {
                let sql = format!("DELETE FROM {t} WHERE state = {state} AND run_at < ? LIMIT ?");
                in_batches(&mut || delete(&sql, before))?;
            }
            for (state, keep) in [(2, JOB_MAX_DEAD), (3, JOB_MAX_KEPT)] {
                let cut: Option<i64> = conn
                    .exec_first(
                        format!("SELECT run_at FROM {t} WHERE state = {state} ORDER BY run_at DESC LIMIT 1 OFFSET ?"),
                        (keep as i64,),
                    )
                    .map_err(|e| w.msg(e))?;
                if let Some(cut) = cut {
                    let sql = format!("DELETE FROM {t} WHERE state = {state} AND run_at <= ? LIMIT ?");
                    let mut delete = |at: i64| -> Result<i64, String> {
                        conn.exec_drop(&sql, (at, batch)).map_err(|e| w.msg(e))?;
                        Ok(conn.affected_rows() as i64)
                    };
                    in_batches(&mut || delete(cut))?;
                }
            }
            Ok(())
        }
    }
}

/// The jobs of the store (at most 1,000 of each state): all, or those given up.
pub fn job_list(spec: &DatabaseSpec, ns: Option<&str>, failed_only: bool) -> Result<Vec<JobInfo>, String> {
    let t = jobs_table(ns);
    let (mut conn, until) = job_conn(spec, ns)?;
    let state_name = |s: i64| match s {
        0 => "waiting",
        1 => "running",
        _ => "failed",
    };
    let filter = if failed_only { "state = 2" } else { "state IN (0, 1, 2)" };
    let sql = format!(
        "SELECT id, name, state, attempts, run_at, COALESCE(error, '') FROM {t} WHERE {filter} \
         ORDER BY state, run_at LIMIT 3000"
    );
    match &mut conn {
        SessionConn::Redis(c) => {
            let which: &[u8] = if failed_only { b"failed" } else { b"all" };
            let Reply::Array(items) = job_redis(c, &REDIS_JOB_LIST, &jobs_prefix(ns), &[b"0", which], until)? else {
                return Err("the server sent an invalid reply".into());
            };
            let text = |r: &Reply| match r {
                Reply::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
                _ => String::new(),
            };
            Ok(items
                .chunks(6)
                .filter(|c| c.len() == 6)
                .map(|c| JobInfo {
                    id: text(&c[0]),
                    name: text(&c[1]),
                    state: match text(&c[2]).as_str() {
                        "waiting" => "waiting",
                        "running" => "running",
                        _ => "failed",
                    },
                    attempts: text(&c[3]).parse().unwrap_or(0),
                    at: text(&c[4]).parse::<f64>().unwrap_or(0.0) as i64,
                    error: text(&c[5]),
                })
                .collect())
        }
        SessionConn::Pg(c) => {
            let rows = c.get().query_kept(&sql, &[]).map_err(|e| pg_message(&e))?;
            let get = |e: postgres::Error| pg_message(&e);
            rows.iter()
                .map(|r| {
                    Ok(JobInfo {
                        id: r.try_get(0).map_err(get)?,
                        name: r.try_get(1).map_err(get)?,
                        state: state_name(i64::from(r.try_get::<_, i16>(2).map_err(get)?)),
                        attempts: r.try_get::<_, i32>(3).map_err(get)?.max(0) as u32,
                        at: r.try_get(4).map_err(get)?,
                        error: r.try_get(5).map_err(get)?,
                    })
                })
                .collect()
        }
        SessionConn::My(c) => {
            use mysql::prelude::Queryable;
            let w = c.get().watch();
            let rows: Vec<(String, String, i64, i64, i64, String)> = c.get().conn.query(&sql).map_err(|e| w.msg(e))?;
            Ok(rows
                .into_iter()
                .map(|(id, name, state, attempts, at, error)| JobInfo {
                    id,
                    name,
                    state: state_name(state),
                    attempts: attempts.max(0) as u32,
                    at,
                    error,
                })
                .collect())
        }
    }
}

/// The waiting, running and given-up jobs that a site with namespace `ns`
/// left in the store every site shared before 1.1 gave sites namespaces
/// (`jobs_table(None)`, `jobs_prefix(None)`): they are not taken, and
/// `phv jobs list` says how many there are. One query, and nothing is made
/// (a table that was never made is an error, as a store that does not
/// answer is). 0 for a site whose store is the shared one itself.
pub fn job_count_shared(spec: &DatabaseSpec, ns: Option<&str>) -> Result<i64, String> {
    let t = jobs_table(None);
    if jobs_table(ns) == t {
        return Ok(0);
    }
    let Some(url) = spec.url.clone() else {
        return Err(format!("the environment variable {} is not set", spec.url_env.as_deref().unwrap_or("?")));
    };
    tls_init();
    let until = Instant::now() + JOB_WAIT;
    let mut conn = store_conn(spec, &url, PoolUse::Jobs, until, JOB_WAIT)?;
    let sql = format!("SELECT count(*) FROM {t} WHERE state IN (0, 1, 2)");
    match &mut conn {
        SessionConn::Redis(c) => {
            // The three counts in one pipeline: one round trip.
            let p = jobs_prefix(None);
            let keys: Vec<String> = ["due", "running", "dead"].iter().map(|set| format!("{p}{set}")).collect();
            let commands: Vec<Vec<&[u8]>> = keys.iter().map(|k| vec![b"ZCARD" as &[u8], k.as_bytes()]).collect();
            let r = c.get().pipeline(&commands, until, 4096);
            let mut n = 0;
            for reply in job_redis_result(c, r)? {
                match reply? {
                    Reply::Int(k) => n += k,
                    _ => return Err("the server sent an invalid reply".into()),
                }
            }
            Ok(n)
        }
        SessionConn::Pg(c) => {
            let rows = c.get().query_kept(&sql, &[]).map_err(|e| pg_message(&e))?;
            rows.first().map_or(Ok(0), |r| r.try_get::<_, i64>(0).map_err(|e| pg_message(&e)))
        }
        SessionConn::My(c) => {
            use mysql::prelude::Queryable;
            let w = c.get().watch();
            let n: Option<i64> = c.get().conn.query_first(&sql).map_err(|e| w.msg(e))?;
            Ok(n.unwrap_or(0))
        }
    }
}

/// Whether the store of a site with namespace `ns` is the one every site
/// shared before 1.1 gave sites namespaces (a site without one, or whose
/// namespace is not hex digits): such a site has nothing left there to adopt.
pub fn job_store_is_shared(ns: Option<&str>) -> bool {
    jobs_table(ns) == jobs_table(None)
}

/// What `job_adopt` did with one job of the shared store, or would do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Adopted {
    /// Moved to the site's own store (in a dry run: would be).
    Moved,
    /// Running: a job is moved only between its runs.
    Running,
    /// Another connection held the row, or had taken it to run, when it was
    /// to be moved (PostgreSQL, MySQL): left for another run of the command.
    Locked,
    /// The site's own store has a job with its key already.
    KeyTaken,
    /// The site's own store is full (`JobCap`): left in the shared store.
    NoRoom,
}

/// A job of the shared store that `job_adopt` looked at.
#[derive(Clone, Debug)]
pub struct AdoptedJob {
    pub id: String,
    pub name: String,
    /// "waiting", "running" or "failed".
    pub state: &'static str,
    pub outcome: Adopted,
}

/// A job of the shared store that may be adopted, as listed.
struct LeftJob {
    id: String,
    name: String,
    /// 0 waiting, 1 running, 2 given up.
    state: i64,
    uniq: Option<String>,
    /// The bytes of its payload.
    size: i64,
}

/// Rows the adoption lists from the shared table in one statement, and jobs
/// it moves in one transaction at most (and the most bytes of their
/// payloads): each statement and transaction is one operation of the store,
/// within `JOB_ADOPT_WAIT` and its budget of bytes. A batch is moved in a
/// few statements whatever its size (found by the 18th assessment: a batch
/// took four round trips per job within a page's 10 seconds, so a store 4 ms
/// away never finished one); a batch that takes more than a quarter of the
/// time halves the next.
const JOB_ADOPT_PAGE: i64 = 10_000;
const JOB_ADOPT_BATCH: usize = 1000;
const JOB_ADOPT_BATCH_BYTES: i64 = 4 << 20;
/// How long one operation of `phv jobs adopt` may take (a page listed, a
/// batch moved, one call of the Redis script): a command's time, not a page's.
pub(super) const JOB_ADOPT_WAIT: Duration = Duration::from_secs(60);
/// Ids one call of the Redis script looks at (see `REDIS_JOB_ADOPT`).
const JOB_ADOPT_REDIS_PAGE: usize = 1000;
/// Calls of the Redis script so far in this process (for the tests).
pub(super) static REDIS_ADOPT_CALLS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
type AdoptHook = Option<Box<dyn FnMut(&str)>>;

#[cfg(test)]
thread_local! {
    /// Called between two pages of the Redis adoption, with the set (for the
    /// tests: another client of the shared store changes it there).
    pub(super) static BETWEEN_ADOPT_PAGES: std::cell::RefCell<AdoptHook> = const { std::cell::RefCell::new(None) };
}

fn adopt_state_name(state: i64) -> &'static str {
    match state {
        0 => "waiting",
        1 => "running",
        _ => "failed",
    }
}

/// Moves the jobs that a site with namespace `ns` left in the store every
/// site shared before 1.1 gave sites namespaces (`jobs_table(None)`,
/// `jobs_prefix(None)`) into the site's own store (`apply`), or says what
/// would be moved (a dry run). The rows of the shared store do not record
/// their site: only the waiting and given-up jobs whose name is one of
/// `names` (the job scripts of the site) are taken, in the order of their ids;
/// a running job is skipped and reported. The site's own caps (`cap`) hold:
/// a waiting job that does not fit is left (`NoRoom`), and so is one whose key
/// the site's store has already (`KeyTaken`).
///
/// A job is never in both stores, and never lost: on PostgreSQL and MySQL
/// a batch of up to `JOB_ADOPT_BATCH` jobs is inserted into the site's table
/// and deleted from the shared one in one transaction, which holds the
/// site's job lock (so the count against the caps is exact, as for
/// `enqueue()` in a transaction), in a few statements over the whole batch;
/// the shared rows are taken with `FOR UPDATE SKIP LOCKED`, so a row another
/// connection holds is skipped (`Locked`), not waited for, and only the rows
/// the site's table accepted are deleted. In Redis a script moves them a
/// page of ids at a time (`JOB_ADOPT_REDIS_PAGE`), so other clients wait a
/// few milliseconds at most. Each operation has `JOB_ADOPT_WAIT`. `names`
/// empty, or a shared table that was never made, adopt nothing. An error
/// names the jobs moved before it.
pub fn job_adopt(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    names: &[String],
    apply: bool,
    cap: JobCap,
) -> Result<Vec<AdoptedJob>, String> {
    let src = jobs_table(None);
    let dst = jobs_table(ns);
    if src == dst {
        return Err("the site has no namespace: the shared store is its own".into());
    }
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let Some(url) = spec.url.as_deref() else {
        return Err(format!("the environment variable {} is not set", spec.url_env.as_deref().unwrap_or("?")));
    };
    if kind_of(url) == Kind::Redis {
        return redis_job_adopt(spec, ns, names, apply, cap);
    }
    let conn_of = || job_conn_within(spec, ns, JOB_ADOPT_WAIT);
    // What is there, a page at a time (each its own operation of the store).
    let mut left: Vec<LeftJob> = Vec::new();
    let mut after = String::new();
    loop {
        let (mut conn, _) = conn_of()?;
        let page = match &mut conn {
            SessionConn::Pg(c) => pg_left_jobs(c.get(), &src, names, &after)?,
            SessionConn::My(c) => my_left_jobs(c.get(), &src, names, &after)?,
            SessionConn::Redis(_) => unreachable!("Redis adopts in one script"),
        };
        let n = page.len() as i64;
        if let Some(last) = page.last() {
            after = last.id.clone();
        }
        left.extend(page);
        if n < JOB_ADOPT_PAGE {
            break;
        }
    }
    // Only the exact names, whatever the store's comparison.
    left.retain(|j| names.contains(&j.name));
    let mut out: Vec<AdoptedJob> = left
        .iter()
        .map(|j| AdoptedJob {
            id: j.id.clone(),
            name: j.name.clone(),
            state: adopt_state_name(j.state),
            outcome: if j.state == 1 { Adopted::Running } else { Adopted::Moved },
        })
        .collect();
    if !apply {
        // The keys the site's store has, and whether the rest fit under its caps.
        let taken = sql_keys_taken(spec, ns, &dst, left.iter().filter(|j| j.state != 1))?;
        let (mut conn, _) = conn_of()?;
        let mut load = match &mut conn {
            SessionConn::Pg(c) => pg_job_load(c.get(), &dst, false)?,
            SessionConn::My(c) => my_job_load(c.get(), &dst)?,
            SessionConn::Redis(_) => unreachable!("Redis adopts in one script"),
        };
        drop(conn);
        for (j, o) in left.iter().zip(out.iter_mut()) {
            if j.state == 1 {
                continue;
            }
            if j.uniq.as_deref().is_some_and(|u| taken.contains(u)) {
                o.outcome = Adopted::KeyTaken;
            } else if j.state == 0 {
                if load.full(cap, j.size as usize) {
                    o.outcome = Adopted::NoRoom;
                } else {
                    load = load.plus(JobLoad::one(j.size as usize));
                }
            }
        }
        return Ok(out);
    }
    // The move, in transactions of at most `most` jobs: each counts the site's
    // store under its lock, then takes the next jobs that fit (`adopt_batch`).
    let mut moved = 0usize;
    let mut i = 0;
    let mut most = JOB_ADOPT_BATCH;
    while i < left.len() {
        let started = Instant::now();
        let (mut conn, _) = conn_of().map_err(|e| adopt_failed(e, moved))?;
        let r = match &mut conn {
            SessionConn::Pg(c) => pg_job_adopt_batch(c.get(), &src, &dst, &left, &mut out, &mut i, most, cap),
            SessionConn::My(c) => my_job_adopt_batch(c.get(), &src, &dst, &left, &mut out, &mut i, most, cap),
            SessionConn::Redis(_) => unreachable!("Redis adopts in one script"),
        };
        moved += r.map_err(|e| adopt_failed(e, moved))?;
        // A slow store gets smaller batches, so that each stays within its time.
        let took = started.elapsed();
        if took > JOB_ADOPT_WAIT / 4 {
            most = (most / 2).max(1);
        } else if took < JOB_ADOPT_WAIT / 16 {
            most = (most * 2).min(JOB_ADOPT_BATCH);
        }
    }
    forget_job_count(&job_count_key(spec, &src));
    forget_job_count(&job_count_key(spec, &dst));
    Ok(out)
}

/// The next jobs of `left` from `i` on to move in one transaction, as
/// indexes: at most `most` jobs and `JOB_ADOPT_BATCH_BYTES` of payloads. A
/// running job is passed over. A waiting job that does not fit under `cap`
/// with the site's `load` (counted under the store's lock) and the batch
/// before it ends the batch; when it is the first of the batch, nothing
/// before it can make room, so it is marked `NoRoom` and passed over. `i` is
/// moved past what was taken or marked.
fn adopt_batch(
    left: &[LeftJob],
    out: &mut [AdoptedJob],
    i: &mut usize,
    most: usize,
    load: JobLoad,
    cap: JobCap,
) -> Vec<usize> {
    let mut picked = Vec::new();
    let mut load = load;
    let mut bytes = 0;
    while *i < left.len() && picked.len() < most {
        let j = &left[*i];
        if j.state == 1 {
            *i += 1;
            continue;
        }
        if j.state == 0 && load.full(cap, j.size as usize) {
            if !picked.is_empty() {
                break;
            }
            out[*i].outcome = Adopted::NoRoom;
            *i += 1;
            continue;
        }
        if !picked.is_empty() && bytes + j.size > JOB_ADOPT_BATCH_BYTES {
            break;
        }
        bytes += j.size;
        if j.state == 0 {
            load = load.plus(JobLoad::one(j.size as usize));
        }
        picked.push(*i);
        *i += 1;
    }
    picked
}

/// What became of each job of a batch: moved, refused by the site's table
/// (a job with its key is there), or not held (another connection has it, or
/// it was taken to run meanwhile).
fn adopt_outcomes(
    left: &[LeftJob],
    out: &mut [AdoptedJob],
    picked: &[usize],
    held: &HashSet<String>,
    moved: &HashSet<String>,
) {
    for &k in picked {
        out[k].outcome = if moved.contains(&left[k].id) {
            Adopted::Moved
        } else if held.contains(&left[k].id) {
            Adopted::KeyTaken
        } else {
            Adopted::Locked
        };
    }
}

fn adopt_failed(e: String, moved: usize) -> String {
    match moved {
        0 => e,
        1 => format!("{e} (1 job was moved before it)"),
        n => format!("{e} ({n} jobs were moved before it)"),
    }
}

/// The jobs of `names` in the shared table `src` with ids after `after`, up
/// to `JOB_ADOPT_PAGE` (PostgreSQL). None when the table was never made.
fn pg_left_jobs(conn: &mut PgConn, src: &str, names: &[String], after: &str) -> Result<Vec<LeftJob>, String> {
    let sql = format!(
        "SELECT id, name, state, uniq, octet_length(payload) FROM {src} \
         WHERE state IN (0, 1, 2) AND name = ANY($1) AND id > $2 ORDER BY id LIMIT {JOB_ADOPT_PAGE}"
    );
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let rows = match conn.client.query(&sql, &[&names, &after]) {
        Ok(rows) => rows,
        Err(e) if pg_no_table(&e) => return Ok(Vec::new()),
        Err(e) => return Err(pg_message(&e)),
    };
    let get = |e: postgres::Error| pg_message(&e);
    rows.iter()
        .map(|r| {
            Ok(LeftJob {
                id: r.try_get(0).map_err(get)?,
                name: r.try_get(1).map_err(get)?,
                state: i64::from(r.try_get::<_, i16>(2).map_err(get)?),
                uniq: r.try_get(3).map_err(get)?,
                size: i64::from(r.try_get::<_, i32>(4).map_err(get)?),
            })
        })
        .collect()
}

/// `pg_left_jobs` on MySQL. The names are compared byte for byte, as
/// PostgreSQL and Redis compare them, not by the column's collation, which
/// ignores case and accents by default (found by the 19th assessment: a
/// shared job named `SEND_MAIL` was moved with the site's `send_mail`).
fn my_left_jobs(conn: &mut MyConn, src: &str, names: &[String], after: &str) -> Result<Vec<LeftJob>, String> {
    use mysql::prelude::Queryable;
    let w = conn.watch();
    let marks = vec!["CAST(? AS BINARY)"; names.len()].join(", ");
    let sql = format!(
        "SELECT id, name, state, uniq, LENGTH(payload) FROM {src} \
         WHERE state IN (0, 1, 2) AND CAST(name AS BINARY) IN ({marks}) AND id > ? ORDER BY id \
         LIMIT {JOB_ADOPT_PAGE}"
    );
    let mut params: Vec<mysql::Value> = names.iter().map(|n| mysql::Value::from(n.as_str())).collect();
    params.push(mysql::Value::from(after));
    let rows: Vec<(String, String, i64, Option<String>, i64)> =
        match conn.conn.exec(&sql, mysql::Params::Positional(params)) {
            Ok(rows) => rows,
            Err(e) if my_no_table(&e) => return Ok(Vec::new()),
            Err(e) => return Err(w.msg(e)),
        };
    Ok(rows.into_iter().map(|(id, name, state, uniq, size)| LeftJob { id, name, state, uniq, size }).collect())
}

/// The keys among those of `jobs` that the site's table `dst` holds already
/// (a dry run's look; the move finds them by its insert).
fn sql_keys_taken<'a>(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    dst: &str,
    jobs: impl Iterator<Item = &'a LeftJob>,
) -> Result<HashSet<String>, String> {
    let keys: Vec<&str> = jobs.filter_map(|j| j.uniq.as_deref()).collect();
    let mut taken = HashSet::new();
    for chunk in keys.chunks(2000) {
        let (mut conn, _) = job_conn_within(spec, ns, JOB_ADOPT_WAIT)?;
        match &mut conn {
            SessionConn::Pg(c) => {
                let sql = format!("SELECT uniq FROM {dst} WHERE uniq = ANY($1)");
                let rows = c.get().client.query(&sql, &[&chunk]).map_err(|e| pg_message(&e))?;
                for r in &rows {
                    taken.insert(r.try_get::<_, String>(0).map_err(|e| pg_message(&e))?);
                }
            }
            SessionConn::My(c) => {
                use mysql::prelude::Queryable;
                let w = c.get().watch();
                let marks = vec!["?"; chunk.len()].join(", ");
                let sql = format!("SELECT uniq FROM {dst} WHERE uniq IN ({marks})");
                let params: Vec<mysql::Value> = chunk.iter().map(|k| mysql::Value::from(*k)).collect();
                let rows: Vec<String> =
                    c.get().conn.exec(&sql, mysql::Params::Positional(params)).map_err(|e| w.msg(e))?;
                taken.extend(rows);
            }
            SessionConn::Redis(_) => unreachable!("Redis adopts in one script"),
        }
    }
    Ok(taken)
}

/// Moves the next batch of `left` (from `i`; see `adopt_batch`) from `src` to
/// `dst` in one transaction that holds the lock of `dst` (PostgreSQL), in
/// three statements over the batch: the rows this transaction holds, then
/// the insert into `dst` with the delete of what it accepted from `src` (one
/// statement: a row a key of `dst` refuses is left where it was). Each job's
/// outcome goes in `out`; the jobs moved.
#[allow(clippy::too_many_arguments)]
fn pg_job_adopt_batch(
    conn: &mut PgConn,
    src: &str,
    dst: &str,
    left: &[LeftJob],
    out: &mut [AdoptedJob],
    i: &mut usize,
    most: usize,
    cap: JobCap,
) -> Result<usize, String> {
    let run = |conn: &mut PgConn, i: &mut usize, out: &mut [AdoptedJob]| -> Result<usize, postgres::Error> {
        conn.store_batch("BEGIN")?;
        pg_job_lock(conn, dst, None)?;
        let load = pg_job_load_raw(conn, dst, true)?;
        let picked = adopt_batch(left, out, i, most, load, cap);
        if picked.is_empty() {
            conn.client.batch_execute("COMMIT")?;
            return Ok(0);
        }
        let ids: Vec<&str> = picked.iter().map(|&k| left[k].id.as_str()).collect();
        let rows = conn.client.query(
            &format!("SELECT id FROM {src} WHERE id = ANY($1) AND state IN (0, 2) FOR UPDATE SKIP LOCKED"),
            &[&ids],
        )?;
        let held: HashSet<String> = rows.iter().map(|r| r.try_get(0)).collect::<Result<_, _>>()?;
        let held_ids: Vec<&str> = held.iter().map(String::as_str).collect();
        let mut moved = HashSet::new();
        if !held_ids.is_empty() {
            let sized = job_sized(dst);
            let sql = format!(
                "WITH i AS (INSERT INTO {dst} (id, name, payload, run_at, attempts, state, uniq, error, created{}) \
                 SELECT id, name, payload, run_at, attempts, state, uniq, error, created{} FROM {src} \
                 WHERE id = ANY($1) ON CONFLICT DO NOTHING RETURNING id) \
                 DELETE FROM {src} WHERE id = ANY($1) AND id IN (SELECT id FROM i) RETURNING id",
                if sized { ", size" } else { "" },
                if sized { ", octet_length(payload)" } else { "" }
            );
            for r in conn.client.query(&sql, &[&held_ids])? {
                moved.insert(r.try_get::<_, String>(0)?);
            }
        }
        adopt_outcomes(left, out, &picked, &held, &moved);
        // Both tables are counted again by the next to add under the lock.
        conn.client.execute("UPDATE phv_jobs_counts SET at = 0 WHERE t = $1 OR t = $2", &[&src, &dst])?;
        conn.client.batch_execute("COMMIT")?;
        Ok(moved.len())
    };
    match run(conn, i, out) {
        Ok(n) => Ok(n),
        Err(e) => {
            let _ = conn.client.batch_execute("ROLLBACK");
            Err(pg_message(&e))
        }
    }
}

/// `pg_job_adopt_batch` on MySQL. The rows are inserted with a mark in
/// `lease` (empty for waiting and failed jobs), which tells the rows the
/// statement added from those a key of `dst` refused: `ON DUPLICATE KEY
/// UPDATE id = id` leaves those as they are, and any other refusal (a CHECK
/// constraint, a value too long) is an error with the server's message
/// (found by the 18th assessment: `INSERT IGNORE` made every refusal "a job
/// with its key"). The marked rows are then deleted from `src` and
/// unmarked, all in the transaction: a failure anywhere rolls it back, so a
/// job is never in both stores.
#[allow(clippy::too_many_arguments)]
fn my_job_adopt_batch(
    conn: &mut MyConn,
    src: &str,
    dst: &str,
    left: &[LeftJob],
    out: &mut [AdoptedJob],
    i: &mut usize,
    most: usize,
    cap: JobCap,
) -> Result<usize, String> {
    use mysql::prelude::Queryable;
    let w = conn.watch();
    let my = |e: mysql::Error| w.msg(e);
    conn.conn.query_drop("START TRANSACTION").map_err(my)?;
    let run = |conn: &mut MyConn, i: &mut usize, out: &mut [AdoptedJob]| -> Result<usize, String> {
        my_job_lock(&mut conn.conn, dst).map_err(my)?;
        let load = my_job_load(conn, dst)?;
        let picked = adopt_batch(left, out, i, most, load, cap);
        let c = &mut conn.conn;
        if picked.is_empty() {
            c.query_drop("COMMIT").map_err(my)?;
            return Ok(0);
        }
        let marks = |n: usize| vec!["?"; n].join(", ");
        let params = |ids: &[&str]| mysql::Params::Positional(ids.iter().map(|id| mysql::Value::from(*id)).collect());
        let ids: Vec<&str> = picked.iter().map(|&k| left[k].id.as_str()).collect();
        let held: Vec<String> = c
            .exec(
                format!(
                    "SELECT id FROM {src} WHERE id IN ({}) AND state IN (0, 2) FOR UPDATE SKIP LOCKED",
                    marks(ids.len())
                ),
                params(&ids),
            )
            .map_err(my)?;
        let held: HashSet<String> = held.into_iter().collect();
        let held_ids: Vec<&str> = held.iter().map(String::as_str).collect();
        let mut moved = HashSet::new();
        if !held_ids.is_empty() {
            let mark = crate::random_hex(16);
            let sized = job_sized(dst);
            let sql = format!(
                "INSERT INTO {dst} (id, name, payload, run_at, attempts, state, uniq, error, created, lease{}) \
                 SELECT id, name, payload, run_at, attempts, state, uniq, error, created, ?{} FROM {src} \
                 WHERE id IN ({}) ON DUPLICATE KEY UPDATE {dst}.id = {dst}.id",
                if sized { ", size" } else { "" },
                if sized { ", LENGTH(payload)" } else { "" },
                marks(held_ids.len())
            );
            let mut with_mark = vec![mysql::Value::from(mark.as_str())];
            with_mark.extend(held_ids.iter().map(|id| mysql::Value::from(*id)));
            c.exec_drop(&sql, mysql::Params::Positional(with_mark.clone())).map_err(my)?;
            let inserted: Vec<String> = c
                .exec(
                    format!("SELECT id FROM {dst} WHERE id IN ({}) AND lease = ?", marks(held_ids.len())),
                    mysql::Params::Positional(
                        with_mark[1..].iter().cloned().chain(with_mark[..1].iter().cloned()).collect(),
                    ),
                )
                .map_err(my)?;
            if !inserted.is_empty() {
                let inserted_ids: Vec<&str> = inserted.iter().map(String::as_str).collect();
                c.exec_drop(
                    format!("DELETE FROM {src} WHERE id IN ({})", marks(inserted.len())),
                    params(&inserted_ids),
                )
                .map_err(my)?;
                c.exec_drop(
                    format!("UPDATE {dst} SET lease = NULL WHERE id IN ({})", marks(inserted.len())),
                    params(&inserted_ids),
                )
                .map_err(my)?;
            }
            moved = inserted.into_iter().collect();
        }
        adopt_outcomes(left, out, &picked, &held, &moved);
        c.exec_drop("UPDATE phv_jobs_counts SET at = 0 WHERE t = ? OR t = ?", (src, dst)).map_err(my)?;
        c.query_drop("COMMIT").map_err(my)?;
        Ok(moved.len())
    };
    match run(conn, i, out) {
        Ok(n) => Ok(n),
        Err(e) => {
            let _ = conn.conn.query_drop("ROLLBACK");
            Err(e)
        }
    }
}

/// KEYS: the shared prefix, the site's. ARGV: "dry" | "move", the site's cap
/// of jobs, its cap of bytes, the set to look at ("running", "due" or
/// "dead"), the place to go on from (the score and the id of the last id
/// looked at; "" and "" for the start), the count of ids to look at, what
/// the site's store holds (jobs, bytes: -1 to count it), then the names of
/// the site's jobs. Reply: the ids looked at, the ids removed from the set,
/// the site's jobs and bytes after this call, a flat list, for each job of
/// those names: id, name, state, outcome ("moved", "running", "key",
/// "full"), then the score and the id of the last id looked at. A running
/// job is reported and left; a job whose key the site's store has, or that
/// does not fit under its caps, is left too. Moving a job renames its hash
/// and its key, and moves its id from one set to the other with its score;
/// the shared byte count goes down by its payload and the site's is set to
/// what it counted. One call looks at a page of ids, so other clients of
/// the server wait a few milliseconds at most (found by the 18th assessment:
/// one call over the whole store held them 0.7 s). A page starts after the
/// last id looked at, by its score and id (the set's own order), counted
/// when the page is read: ids that another client removed meanwhile, ahead
/// of it or the last id itself, do not make it skip any (found by the 19th
/// assessment: paged by rank, a page started past some of the site's jobs).
static REDIS_JOB_ADOPT: LuaScript = LuaScript::new(
    r#"
local s, d = KEYS[1], KEYS[2]
local move = ARGV[1] == 'move'
local cap, capb = tonumber(ARGV[2]), tonumber(ARGV[3])
local set, after_score, after_id, count = ARGV[4], ARGV[5], ARGV[6], tonumber(ARGV[7])
local n, bytes = tonumber(ARGV[8]), tonumber(ARGV[9])
local names = {}
for i = 10, #ARGV do names[ARGV[i]] = true end
if move or n < 0 then
  n = redis.call('ZCARD', d .. 'due') + redis.call('ZCARD', d .. 'running')
  bytes = math.max(tonumber(redis.call('GET', d .. 'bytes') or '0'), 0)
end
-- Whether a <= b, byte by byte (the order of members of one score).
local function not_after(a, b)
  for i = 1, math.min(#a, #b) do
    local x, y = string.byte(a, i), string.byte(b, i)
    if x ~= y then return x < y end
  end
  return #a <= #b
end
-- The rank of the first id after (after_score, after_id) in set z: the ids of
-- lower scores, and those of that score up to after_id (a binary search).
local function start_of(z)
  if after_score == '' then return 0 end
  local below = redis.call('ZCOUNT', z, '-inf', '(' .. after_score)
  local lo, hi = 0, redis.call('ZCOUNT', z, after_score, after_score)
  while lo < hi do
    local mid = math.floor((lo + hi) / 2)
    local m = redis.call('ZRANGEBYSCORE', z, after_score, after_score, 'LIMIT', mid, 1)[1]
    if m and not_after(m, after_id) then lo = mid + 1 else hi = mid end
  end
  return below + lo
end
local out = {}
local function put(id, name, state, what)
  out[#out + 1] = id; out[#out + 1] = name; out[#out + 1] = state; out[#out + 1] = what
end
local looked, removed, moved = 0, 0, 0
local start = start_of(s .. set)
local ids = redis.call('ZRANGE', s .. set, start, start + count - 1, 'WITHSCORES')
looked = #ids / 2
local last_score, last_id = after_score, after_id
if looked > 0 then last_score, last_id = ids[#ids], ids[#ids - 1] end
if set == 'running' then
  for i = 1, #ids, 2 do
    local id = ids[i]
    local name = redis.call('HGET', s .. 'job:' .. id, 'name')
    if name and names[name] then put(id, name, 'running', 'running') end
  end
else
  local state = set == 'due' and 'waiting' or 'failed'
  for i = 1, #ids, 2 do
    local id = ids[i]
    local k = s .. 'job:' .. id
    local f = redis.call('HMGET', k, 'name', 'uniq')
    local name, uniq = f[1], f[2]
    if name and names[name] then
      local size = redis.call('HSTRLEN', k, 'payload')
      if uniq and uniq ~= '' and redis.call('EXISTS', d .. 'key:' .. uniq) == 1 then
        put(id, name, state, 'key')
      elseif set == 'due' and (n >= cap or bytes + size > capb) then
        put(id, name, state, 'full')
      else
        if move then
          redis.call('RENAME', k, d .. 'job:' .. id)
          if uniq and uniq ~= '' then
            if redis.call('EXISTS', s .. 'key:' .. uniq) == 1 then
              redis.call('RENAME', s .. 'key:' .. uniq, d .. 'key:' .. uniq)
            else
              redis.call('SET', d .. 'key:' .. uniq, id)
            end
          end
          redis.call('ZREM', s .. set, id)
          redis.call('ZADD', d .. set, ids[i + 1], id)
          removed = removed + 1
          if set == 'due' then
            redis.call('DECRBY', s .. 'bytes', size)
            moved = moved + 1
          end
        end
        if set == 'due' then n = n + 1; bytes = bytes + size end
        put(id, name, state, 'moved')
      end
    end
  end
end
if moved > 0 then redis.call('SET', d .. 'bytes', bytes) end
return {looked, removed, n, bytes, out, last_score, last_id}
"#,
);

/// `job_adopt` on Redis: the script (see `REDIS_JOB_ADOPT`) over each set of
/// the shared store, a page of ids per call, each page after the last id the
/// call before looked at.
fn redis_job_adopt(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    names: &[String],
    apply: bool,
    cap: JobCap,
) -> Result<Vec<AdoptedJob>, String> {
    let (mut conn, until) = job_conn_within(spec, ns, JOB_ADOPT_WAIT)?;
    let SessionConn::Redis(c) = &mut conn else { unreachable!("the URL is Redis") };
    let (src, dst) = (jobs_prefix(None), jobs_prefix(ns));
    let (jobs_cap, bytes_cap) = (cap.jobs.to_string(), cap.bytes.to_string());
    let mode: &[u8] = if apply { b"move" } else { b"dry" };
    let page = JOB_ADOPT_REDIS_PAGE.to_string();
    let text = |r: &Reply| match r {
        Reply::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    };
    let int = |r: Option<&Reply>| match r {
        Some(Reply::Int(n)) => Some(*n),
        _ => None,
    };
    let invalid = || "the server sent an invalid reply".to_string();
    let mut out = Vec::new();
    // What the site's store holds, carried from one call to the next in a
    // dry run (a move reads it from the store, which it keeps up to date).
    let (mut n, mut bytes) = (-1i64, -1i64);
    for set in ["running", "due", "dead"] {
        // The score and the id of the last id looked at.
        let (mut after_score, mut after_id) = (Vec::new(), Vec::new());
        loop {
            let (n_s, bytes_s) = (n.to_string(), bytes.to_string());
            let mut args: Vec<&[u8]> = vec![
                b"2",
                src.as_bytes(),
                dst.as_bytes(),
                mode,
                jobs_cap.as_bytes(),
                bytes_cap.as_bytes(),
                set.as_bytes(),
                &after_score,
                &after_id,
                page.as_bytes(),
                n_s.as_bytes(),
                bytes_s.as_bytes(),
            ];
            args.extend(names.iter().map(|n| n.as_bytes()));
            REDIS_ADOPT_CALLS.fetch_add(1, Ordering::Relaxed);
            let r = REDIS_JOB_ADOPT.run(c.get(), &args, until, 32 << 20);
            let Reply::Array(reply) = job_redis_result(c, r)? else {
                return Err(invalid());
            };
            let (Some(looked), Some(_removed), Some(jobs), Some(size)) =
                (int(reply.first()), int(reply.get(1)), int(reply.get(2)), int(reply.get(3)))
            else {
                return Err(invalid());
            };
            let (Some(Reply::Array(items)), Some(Reply::Bulk(score)), Some(Reply::Bulk(id))) =
                (reply.get(4), reply.get(5), reply.get(6))
            else {
                return Err(invalid());
            };
            (n, bytes) = (jobs, size);
            out.extend(items.chunks(4).filter(|c| c.len() == 4).map(|c| AdoptedJob {
                id: text(&c[0]),
                name: text(&c[1]),
                state: match text(&c[2]).as_str() {
                    "waiting" => "waiting",
                    "running" => "running",
                    _ => "failed",
                },
                outcome: match text(&c[3]).as_str() {
                    "running" => Adopted::Running,
                    "key" => Adopted::KeyTaken,
                    "full" => Adopted::NoRoom,
                    _ => Adopted::Moved,
                },
            }));
            if looked < JOB_ADOPT_REDIS_PAGE as i64 {
                break;
            }
            (after_score, after_id) = (score.clone(), id.clone());
            #[cfg(test)]
            BETWEEN_ADOPT_PAGES.with(|h| {
                if let Some(f) = h.borrow_mut().as_mut() {
                    f(set)
                }
            });
        }
    }
    Ok(out)
}

/// Puts a job that was given up back in the queue (with its runs counted
/// from zero); `false` if there is no such job.
pub fn job_requeue(spec: &DatabaseSpec, ns: Option<&str>, id: &str) -> Result<bool, String> {
    let t = jobs_table(ns);
    let (mut conn, until) = job_conn(spec, ns)?;
    let now = now_ms();
    match &mut conn {
        SessionConn::Redis(c) => {
            let now_s = now.to_string();
            match job_redis(c, &REDIS_JOB_REQUEUE, &jobs_prefix(ns), &[b"0", id.as_bytes(), now_s.as_bytes()], until)? {
                Reply::Int(n) => Ok(n == 1),
                _ => Err("the server sent an invalid reply".into()),
            }
        }
        SessionConn::Pg(c) => c
            .get()
            .execute_kept(
                &format!(
                    "UPDATE {t} SET state = 0, attempts = 0, run_at = $2, error = NULL WHERE id = $1 AND state = 2"
                ),
                &[&id, &now],
            )
            .map(|n| n > 0)
            .map_err(|e| pg_message(&e)),
        SessionConn::My(c) => {
            use mysql::prelude::Queryable;
            let w = c.get().watch();
            let conn = &mut c.get().conn;
            conn.exec_drop(
                format!("UPDATE {t} SET state = 0, attempts = 0, run_at = ?, error = NULL WHERE id = ? AND state = 2"),
                (now, id),
            )
            .map_err(|e| w.msg(e))?;
            Ok(conn.affected_rows() > 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::jobs_add::job_add;
    use super::*;

    /// The servers of the database tests (see crates/phv_driver/tests/databases.rs), when set.
    fn test_servers() -> Vec<(&'static str, DatabaseSpec)> {
        ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL", "PHV_TEST_REDIS_URL"]
            .into_iter()
            .filter_map(|var| {
                let url = std::env::var(var).ok().filter(|u| !u.is_empty());
                if url.is_none() {
                    eprintln!("skipped: {var} is not set");
                }
                Some((var, DatabaseSpec { url: Some(url?), url_env: None, tls: None, pool_size: 4 }))
            })
            .collect()
    }

    /// Puts a waiting job whose payload is `len` times `ch` in the store as
    /// another writer of the database would (no cap on the payload), due at
    /// `run_at`.
    fn plant(spec: &DatabaseSpec, ns: Option<&str>, id: &str, ch: &str, len: usize, run_at: i64) {
        let t = jobs_table(ns);
        let (mut conn, until) = job_conn(spec, ns).unwrap();
        match &mut conn {
            SessionConn::Redis(c) => {
                let p = jobs_prefix(ns);
                let (key, due, at) = (format!("{p}job:{id}"), format!("{p}due"), run_at.to_string());
                let payload = ch.repeat(len);
                let hset: [&[u8]; 8] =
                    [b"HSET", key.as_bytes(), b"name", b"echo", b"payload", payload.as_bytes(), b"attempts", b"0"];
                c.get().command(&hset, until, 1024).ok().expect("the Redis command");
                c.get()
                    .command(&[b"ZADD", due.as_bytes(), at.as_bytes(), id.as_bytes()], until, 1024)
                    .ok()
                    .expect("the Redis command");
            }
            SessionConn::Pg(c) => {
                let sql = format!(
                    "INSERT INTO {t} (id, name, payload, run_at, attempts, state, created) \
                     VALUES ($1, 'echo', repeat($2::text, $3::int), $4, 0, 0, 1)"
                );
                c.get().execute_kept(&sql, &[&id, &ch, &(len as i32), &run_at]).unwrap();
            }
            SessionConn::My(c) => {
                use mysql::prelude::Queryable;
                let sql = format!(
                    "INSERT INTO {t} (id, name, payload, run_at, attempts, state, created) \
                     VALUES (?, 'echo', REPEAT(?, ?), ?, 0, 0, 1)"
                );
                c.get().conn.exec_drop(sql, (id, ch, len, run_at)).unwrap();
            }
        }
    }

    /// Removes a job that was given up.
    fn remove(spec: &DatabaseSpec, ns: Option<&str>, id: &str) {
        let t = jobs_table(ns);
        let (mut conn, until) = job_conn(spec, ns).unwrap();
        match &mut conn {
            SessionConn::Redis(c) => {
                let p = jobs_prefix(ns);
                let (key, dead) = (format!("{p}job:{id}"), format!("{p}dead"));
                c.get().command(&[b"DEL", key.as_bytes()], until, 1024).ok().expect("the Redis command");
                c.get()
                    .command(&[b"ZREM", dead.as_bytes(), id.as_bytes()], until, 1024)
                    .ok()
                    .expect("the Redis command");
            }
            SessionConn::Pg(c) => {
                c.get().execute_kept(&format!("DELETE FROM {t} WHERE id = $1"), &[&id]).unwrap();
            }
            SessionConn::My(c) => {
                use mysql::prelude::Queryable;
                c.get().conn.exec_drop(format!("DELETE FROM {t} WHERE id = ?"), (id,)).unwrap();
            }
        }
    }

    /// A job whose payload no reply of the store can hold (planted by another
    /// writer of the database: `enqueue()` adds 64 KB at most) is given up as
    /// failed when its turn comes, and the jobs after it run (found by the
    /// 21st assessment: on MySQL a row of 9 MB failed the read that chose the
    /// job and stayed first in line, so no job of the site ran any more; on
    /// PostgreSQL and Redis it was taken again at every lease, for ever).
    #[test]
    fn a_job_too_large_to_read_is_given_up_and_the_queue_goes_on() {
        let ns = crate::testing::namespace(0x36);
        let ns = Some(ns.as_str());
        for (var, spec) in test_servers() {
            let clear = || {
                while let Some(j) = job_take(&spec, ns, 60_000).unwrap() {
                    job_end(&spec, ns, &j, JobEnd::Done { keyed: false }).unwrap();
                }
            };
            job_store_ready(&spec, ns, None).unwrap();
            clear();
            let now = now_ms();
            let big = format!("big{}", crate::random_hex(6));
            plant(&spec, ns, &big, "B", 9 << 20, now - 5000);
            // One at the bound is run; one byte more is not.
            let (most, over) = (format!("most{}", crate::random_hex(6)), format!("over{}", crate::random_hex(6)));
            plant(&spec, ns, &most, "m", JOB_MAX_TAKEN, now - 4000);
            plant(&spec, ns, &over, "o", JOB_MAX_TAKEN + 1, now - 3000);
            let after = crate::random_hex(16);
            let cap = JobCap { jobs: 1000, bytes: 1 << 30 };
            assert_eq!(
                job_add(&spec, ns, (&after, "echo", "after"), now - 1000, None, cap, None),
                Ok(Some(())),
                "{var}"
            );

            let first = job_take(&spec, ns, 60_000).unwrap().unwrap_or_else(|| panic!("{var}: no job was taken"));
            assert_eq!(
                (first.id.as_str(), first.payload.len(), first.attempts),
                (most.as_str(), JOB_MAX_TAKEN, 1),
                "{var}"
            );
            let second = job_take(&spec, ns, 60_000).unwrap().unwrap_or_else(|| panic!("{var}: the queue stopped"));
            assert_eq!((second.id.as_str(), second.payload.as_str()), (after.as_str(), "after"), "{var}");
            assert!(job_take(&spec, ns, 60_000).unwrap().is_none(), "{var}: nothing else waits");
            for job in [&first, &second] {
                assert!(job_end(&spec, ns, job, JobEnd::Done { keyed: false }).unwrap(), "{var}");
            }
            // Both are among the failed jobs, with the reason; neither waits or runs.
            let jobs = job_list(&spec, ns, false).unwrap();
            for (id, size) in [(&big, 9 << 20), (&over, JOB_MAX_TAKEN + 1)] {
                let j = jobs.iter().find(|j| &j.id == id).unwrap_or_else(|| panic!("{var}: {id} is gone"));
                assert_eq!(j.state, "failed", "{var}: {id}");
                assert!(
                    j.error.contains(&format!("the payload ({size} bytes) is over the 1 MB")),
                    "{var}: {}",
                    j.error
                );
            }
            assert!(jobs.iter().all(|j| j.state == "failed" || (j.id != most && j.id != after)), "{var}");
            // Put back by hand (`phv jobs retry`), such a job is given up again.
            assert!(job_requeue(&spec, ns, &big).unwrap(), "{var}");
            assert!(job_take(&spec, ns, 60_000).unwrap().is_none(), "{var}");
            assert!(job_list(&spec, ns, true).unwrap().iter().any(|j| j.id == big), "{var}");
            for id in [&big, &over] {
                remove(&spec, ns, id);
            }
        }
    }

    /// Nothing a Redis store sends with a job is longer than PHV writes: a
    /// job whose name is over 64 bytes is given up as failed, an entry whose
    /// id is over 64 bytes is removed, and the jobs after them run (found by
    /// the re-attack of the 21st assessment's fixes: a name of 33 MB made the
    /// reply too large to read, the job stayed leased and was taken again at
    /// every lease). `phv jobs list` shows such a job, its name cut. (The
    /// tables of PostgreSQL and MySQL hold no such name or id.)
    #[test]
    fn a_redis_job_with_a_name_or_id_too_long_to_read_does_not_stop_the_queue() {
        let ns = crate::testing::namespace(0x37);
        let ns = Some(ns.as_str());
        for (var, spec) in test_servers() {
            if var != "PHV_TEST_REDIS_URL" {
                continue;
            }
            while let Some(j) = job_take(&spec, ns, 60_000).unwrap() {
                job_end(&spec, ns, &j, JobEnd::Done { keyed: false }).unwrap();
            }
            let now = now_ms();
            let p = jobs_prefix(ns);
            let command = |args: &[&[u8]]| {
                let (mut conn, until) = job_conn(&spec, ns).unwrap();
                let SessionConn::Redis(c) = &mut conn else { unreachable!() };
                c.get().command(args, until, 1 << 20).ok().expect("the Redis command");
            };
            let due = format!("{p}due");
            // A name of 33 MB, an id of 33 MB (with and without a hash), then a job.
            let (named, long_id) = (format!("named{}", crate::random_hex(6)), "i".repeat(33 << 20));
            let long_name = "n".repeat(33 << 20);
            let key = format!("{p}job:{named}");
            command(&[b"HSET", key.as_bytes(), b"name", long_name.as_bytes(), b"payload", b"x", b"attempts", b"0"]);
            command(&[b"ZADD", due.as_bytes(), (now - 5000).to_string().as_bytes(), named.as_bytes()]);
            let key = format!("{p}job:{long_id}");
            command(&[b"HSET", key.as_bytes(), b"name", b"echo", b"payload", b"x", b"attempts", b"0"]);
            command(&[b"ZADD", due.as_bytes(), (now - 4000).to_string().as_bytes(), long_id.as_bytes()]);
            let bare = "b".repeat(100);
            command(&[b"ZADD", due.as_bytes(), (now - 3000).to_string().as_bytes(), bare.as_bytes()]);
            let after = crate::random_hex(16);
            let cap = JobCap { jobs: 1000, bytes: 1 << 30 };
            assert_eq!(job_add(&spec, ns, (&after, "echo", "after"), now - 1000, None, cap, None), Ok(Some(())));

            let job = job_take(&spec, ns, 60_000).unwrap().expect("the queue stopped");
            assert_eq!((job.id.as_str(), job.name.as_str(), job.payload.as_str()), (after.as_str(), "echo", "after"));
            assert!(job_end(&spec, ns, &job, JobEnd::Done { keyed: false }).unwrap());
            assert!(job_take(&spec, ns, 60_000).unwrap().is_none());
            let jobs = job_list(&spec, ns, false).unwrap();
            assert_eq!(jobs.len(), 1, "the entries with long ids are gone, the job that ran too");
            let j = &jobs[0];
            assert_eq!((j.id.as_str(), j.state, j.name.len()), (named.as_str(), "failed", 64));
            assert!(j.error.contains(&format!("the name ({} bytes) is over the 64 bytes", 33 << 20)), "{}", j.error);
            remove(&spec, ns, &named);
        }
    }
}
