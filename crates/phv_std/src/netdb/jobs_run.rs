// ----- Background jobs: taking, ending, sweeping, listing, adopting ---------

use super::handle::{kind_of, tls_init, Kind};
use super::jobs::{
    job_conn, job_conn_before, job_redis, job_redis_result, job_sized, jobs_prefix, jobs_table, now_ms, JobCap,
    JobInfo, JobLoad, JobRow, JOB_KEEP_DEAD_MS, JOB_KEEP_DONE_MS, JOB_MAX_DEAD, JOB_MAX_KEPT, JOB_SWEEP_BATCH,
    JOB_TABLES, JOB_WAIT, REDIS_JOB_END, REDIS_JOB_LIST, REDIS_JOB_REQUEUE, REDIS_JOB_SWEEP, REDIS_JOB_TAKE,
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
use postgres::types::ToSql;
use std::collections::HashSet;
use std::time::Instant;

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

/// Takes the next job due (or one whose lease ran out) for `lease_ms`.
pub fn job_take(spec: &DatabaseSpec, ns: Option<&str>, lease_ms: i64) -> Result<Option<JobRow>, String> {
    let t = jobs_table(ns);
    let (mut conn, until) = job_conn(spec, ns)?;
    let now = now_ms();
    let token = crate::random_hex(16);
    let lease_until = now + lease_ms;
    match &mut conn {
        SessionConn::Redis(c) => {
            let (now_s, until_s) = (now.to_string(), lease_until.to_string());
            let args: [&[u8]; 4] = [b"0", now_s.as_bytes(), until_s.as_bytes(), token.as_bytes()];
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
                    Ok(Some(JobRow { id: text(id), name: text(name), payload: text(payload), attempts, lease: token }))
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
                     RETURNING id, name, payload, attempts"
                    ),
                    &[&token, &lease_until, &now],
                )
                .map_err(|e| pg_message(&e))?;
            let Some(r) = rows.first() else { return Ok(None) };
            let get = |e: postgres::Error| pg_message(&e);
            Ok(Some(JobRow {
                id: r.try_get(0).map_err(get)?,
                name: r.try_get(1).map_err(get)?,
                payload: r.try_get(2).map_err(get)?,
                attempts: r.try_get::<_, i32>(3).map_err(get)?.max(0) as u32,
                lease: token,
            }))
        }
        SessionConn::My(c) => {
            use mysql::prelude::Queryable;
            let w = c.get().watch();
            let my = |e: mysql::Error| w.msg(e);
            let conn = &mut c.get().conn;
            conn.query_drop("START TRANSACTION").map_err(my)?;
            let found: Option<(String, String, String, i64)> = conn
                .exec_first(
                    format!(
                        "SELECT id, name, payload, attempts FROM {t} WHERE (state = 0 AND run_at <= ?) \
                     OR (state = 1 AND lease_until < ?) ORDER BY run_at LIMIT 1 FOR UPDATE SKIP LOCKED"
                    ),
                    (now, now),
                )
                .map_err(my)?;
            let Some((id, name, payload, attempts)) = found else {
                conn.query_drop("COMMIT").map_err(my)?;
                return Ok(None);
            };
            conn.exec_drop(
                format!("UPDATE {t} SET state = 1, attempts = LEAST(attempts, 1000) + 1, lease = ?, lease_until = ? WHERE id = ?"),
                (&token, lease_until, &id),
            )
            .map_err(my)?;
            conn.query_drop("COMMIT").map_err(my)?;
            Ok(Some(JobRow { id, name, payload, attempts: (attempts.min(1000) + 1).max(0) as u32, lease: token }))
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
/// it moves in one transaction (and the most bytes of their payloads): each
/// statement and transaction is one operation of the store, within its time
/// and its budget of bytes.
const JOB_ADOPT_PAGE: i64 = 10_000;
const JOB_ADOPT_BATCH: usize = 500;
const JOB_ADOPT_BATCH_BYTES: i64 = 4 << 20;

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
/// each job is deleted from the shared table and inserted into the site's
/// in one transaction (a savepoint per job, up to `JOB_ADOPT_BATCH` jobs per
/// transaction, which holds the site's job lock, so the count against the
/// caps is exact as for `enqueue()` in a transaction); the shared row is
/// taken with `FOR UPDATE SKIP LOCKED`, so a row another connection holds is
/// skipped (`Locked`), not waited for. In Redis one script moves them, and a
/// script runs alone. `names` empty, or a shared table that was never made,
/// adopt nothing. An error names the jobs moved before it.
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
    // What is there, a page at a time (each its own operation of the store).
    let mut left: Vec<LeftJob> = Vec::new();
    let mut after = String::new();
    loop {
        let (mut conn, _) = job_conn(spec, ns)?;
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
        let (mut conn, _) = job_conn(spec, ns)?;
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
    // The move, in transactions of at most `JOB_ADOPT_BATCH` jobs.
    let mut moved = 0usize;
    let mut i = 0;
    while i < left.len() {
        let mut end = i;
        let mut bytes = 0;
        while end < left.len()
            && end - i < JOB_ADOPT_BATCH
            && (end == i || bytes + left[end].size <= JOB_ADOPT_BATCH_BYTES)
        {
            bytes += left[end].size;
            end += 1;
        }
        let batch = &left[i..end];
        let outcomes = &mut out[i..end];
        let (mut conn, until) = job_conn(spec, ns).map_err(|e| adopt_failed(e, moved))?;
        let r = match &mut conn {
            SessionConn::Pg(c) => pg_job_adopt_batch(c.get(), &src, &dst, batch, outcomes, cap),
            SessionConn::My(c) => my_job_adopt_batch(c.get(), &src, &dst, batch, outcomes, cap, until),
            SessionConn::Redis(_) => unreachable!("Redis adopts in one script"),
        };
        moved += r.map_err(|e| adopt_failed(e, moved))?;
        i = end;
    }
    forget_job_count(&job_count_key(spec, &src));
    forget_job_count(&job_count_key(spec, &dst));
    Ok(out)
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

/// `pg_left_jobs` on MySQL.
fn my_left_jobs(conn: &mut MyConn, src: &str, names: &[String], after: &str) -> Result<Vec<LeftJob>, String> {
    use mysql::prelude::Queryable;
    let w = conn.watch();
    let marks = vec!["?"; names.len()].join(", ");
    let sql = format!(
        "SELECT id, name, state, uniq, LENGTH(payload) FROM {src} \
         WHERE state IN (0, 1, 2) AND name IN ({marks}) AND id > ? ORDER BY id LIMIT {JOB_ADOPT_PAGE}"
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
        let (mut conn, _) = job_conn(spec, ns)?;
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

/// Moves `batch` from `src` to `dst` in one transaction that holds the lock
/// of `dst` (PostgreSQL); see `job_adopt`. Each job's outcome goes in
/// `outcomes`; the jobs moved.
fn pg_job_adopt_batch(
    conn: &mut PgConn,
    src: &str,
    dst: &str,
    batch: &[LeftJob],
    outcomes: &mut [AdoptedJob],
    cap: JobCap,
) -> Result<usize, String> {
    let pg = |e: postgres::Error| pg_message(&e);
    let run =
        |conn: &mut PgConn, src: &str, dst: &str, outcomes: &mut [AdoptedJob]| -> Result<usize, postgres::Error> {
            conn.store_batch("BEGIN")?;
            pg_job_lock(conn, dst, None)?;
            let mut load = pg_job_load_raw(conn, dst, true)?;
            let sized = job_sized(dst);
            let take = format!(
                "DELETE FROM {src} WHERE id = (SELECT id FROM {src} WHERE id = $1 AND state IN (0, 2) \
             FOR UPDATE SKIP LOCKED) RETURNING name, payload, run_at, attempts, state, uniq, error, created"
            );
            let put = format!(
                "INSERT INTO {dst} (id, name, payload, run_at, attempts, state, uniq, error, created{}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9{}) ON CONFLICT DO NOTHING",
                if sized { ", size" } else { "" },
                if sized { ", $10" } else { "" }
            );
            let mut moved = 0;
            for (job, out) in batch.iter().zip(outcomes.iter_mut()) {
                if job.state == 1 {
                    continue;
                }
                if job.state == 0 && load.full(cap, job.size as usize) {
                    out.outcome = Adopted::NoRoom;
                    continue;
                }
                conn.client.batch_execute("SAVEPOINT phv_adopt")?;
                let rows = conn.client.query(&take, &[&job.id])?;
                let Some(r) = rows.first() else {
                    conn.client.batch_execute("ROLLBACK TO SAVEPOINT phv_adopt")?;
                    out.outcome = Adopted::Locked;
                    continue;
                };
                let name: String = r.try_get(0)?;
                let payload: String = r.try_get(1)?;
                let run_at: i64 = r.try_get(2)?;
                let attempts: i32 = r.try_get(3)?;
                let state: i16 = r.try_get(4)?;
                let uniq: Option<String> = r.try_get(5)?;
                let error: Option<String> = r.try_get(6)?;
                let created: i64 = r.try_get(7)?;
                let size = payload.len() as i32;
                let params: [&(dyn ToSql + Sync); 10] =
                    [&job.id, &name, &payload, &run_at, &attempts, &state, &uniq, &error, &created, &size];
                let n = conn.client.execute(&put, &params[..if sized { 10 } else { 9 }])?;
                if n == 0 {
                    conn.client.batch_execute("ROLLBACK TO SAVEPOINT phv_adopt")?;
                    out.outcome = Adopted::KeyTaken;
                    continue;
                }
                conn.client.batch_execute("RELEASE SAVEPOINT phv_adopt")?;
                out.outcome = Adopted::Moved;
                moved += 1;
                if state == 0 {
                    load = load.plus(JobLoad::one(payload.len()));
                }
            }
            // Both tables are counted again by the next to add under the lock.
            conn.client.execute("UPDATE phv_jobs_counts SET at = 0 WHERE t = $1 OR t = $2", &[&src, &dst])?;
            conn.client.batch_execute("COMMIT")?;
            Ok(moved)
        };
    match run(conn, src, dst, outcomes) {
        Ok(n) => Ok(n),
        Err(e) => {
            let _ = conn.client.batch_execute("ROLLBACK");
            Err(pg(e))
        }
    }
}

/// `pg_job_adopt_batch` on MySQL.
fn my_job_adopt_batch(
    conn: &mut MyConn,
    src: &str,
    dst: &str,
    batch: &[LeftJob],
    outcomes: &mut [AdoptedJob],
    cap: JobCap,
    until: Instant,
) -> Result<usize, String> {
    use mysql::prelude::Queryable;
    let w = conn.watch();
    let my = |e: mysql::Error| w.msg(e);
    conn.conn.query_drop("START TRANSACTION").map_err(my)?;
    let run = |conn: &mut MyConn, outcomes: &mut [AdoptedJob]| -> Result<usize, String> {
        my_job_lock(&mut conn.conn, dst).map_err(my)?;
        let mut load = my_job_load(conn, dst)?;
        let sized = job_sized(dst);
        let c = &mut conn.conn;
        let take = format!(
            "SELECT name, payload, run_at, attempts, state, uniq, error, created FROM {src} \
             WHERE id = ? AND state IN (0, 2) FOR UPDATE SKIP LOCKED"
        );
        let put = format!(
            "INSERT IGNORE INTO {dst} (id, name, payload, run_at, attempts, state, uniq, error, created{}) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?{})",
            if sized { ", size" } else { "" },
            if sized { ", ?" } else { "" }
        );
        let mut moved = 0;
        for (job, out) in batch.iter().zip(outcomes.iter_mut()) {
            if job.state == 1 {
                continue;
            }
            if job.state == 0 && load.full(cap, job.size as usize) {
                out.outcome = Adopted::NoRoom;
                continue;
            }
            if Instant::now() >= until {
                return Err("the store did not answer in time".into());
            }
            c.query_drop("SAVEPOINT phv_adopt").map_err(my)?;
            type Taken = (String, String, i64, i64, i64, Option<String>, Option<String>, i64);
            let row: Option<Taken> = c.exec_first(&take, (&job.id,)).map_err(my)?;
            let Some((name, payload, run_at, attempts, state, uniq, error, created)) = row else {
                c.query_drop("ROLLBACK TO SAVEPOINT phv_adopt").map_err(my)?;
                out.outcome = Adopted::Locked;
                continue;
            };
            let mut params: Vec<mysql::Value> = vec![
                mysql::Value::from(job.id.as_str()),
                mysql::Value::from(name.as_str()),
                mysql::Value::from(payload.as_str()),
                mysql::Value::from(run_at),
                mysql::Value::from(attempts),
                mysql::Value::from(state),
                mysql::Value::from(uniq.as_deref()),
                mysql::Value::from(error.as_deref()),
                mysql::Value::from(created),
            ];
            if sized {
                params.push(mysql::Value::from(payload.len() as i64));
            }
            c.exec_drop(&put, mysql::Params::Positional(params)).map_err(my)?;
            if c.affected_rows() == 0 {
                c.query_drop("ROLLBACK TO SAVEPOINT phv_adopt").map_err(my)?;
                out.outcome = Adopted::KeyTaken;
                continue;
            }
            c.exec_drop(format!("DELETE FROM {src} WHERE id = ?"), (&job.id,)).map_err(my)?;
            c.query_drop("RELEASE SAVEPOINT phv_adopt").map_err(my)?;
            out.outcome = Adopted::Moved;
            moved += 1;
            if state == 0 {
                load = load.plus(JobLoad::one(payload.len()));
            }
        }
        c.exec_drop("UPDATE phv_jobs_counts SET at = 0 WHERE t = ? OR t = ?", (src, dst)).map_err(my)?;
        c.query_drop("COMMIT").map_err(my)?;
        Ok(moved)
    };
    match run(conn, outcomes) {
        Ok(n) => Ok(n),
        Err(e) => {
            let _ = conn.conn.query_drop("ROLLBACK");
            Err(e)
        }
    }
}

/// KEYS: the shared prefix, the site's. ARGV: "dry" | "move", the site's cap
/// of jobs, its cap of bytes, then the names of the site's jobs. Flat list,
/// for each job of those names in the shared store: id, name, state,
/// outcome ("moved", "running", "key", "full"). A running job is reported
/// and left; a job whose key the site's store has, or that does not fit under
/// its caps, is left too. Moving a job renames its hash and its key, and
/// moves its id from one set to the other with its score; the shared byte
/// count goes down by its payload and the site's is set to what it counted.
static REDIS_JOB_ADOPT: LuaScript = LuaScript::new(
    r#"
local s, d = KEYS[1], KEYS[2]
local move = ARGV[1] == 'move'
local cap, capb = tonumber(ARGV[2]), tonumber(ARGV[3])
local names = {}
for i = 4, #ARGV do names[ARGV[i]] = true end
local out = {}
local function put(id, name, state, what)
  out[#out + 1] = id; out[#out + 1] = name; out[#out + 1] = state; out[#out + 1] = what
end
for _, id in ipairs(redis.call('ZRANGE', s .. 'running', 0, -1)) do
  local name = redis.call('HGET', s .. 'job:' .. id, 'name')
  if name and names[name] then put(id, name, 'running', 'running') end
end
local n = redis.call('ZCARD', d .. 'due') + redis.call('ZCARD', d .. 'running')
local bytes = math.max(tonumber(redis.call('GET', d .. 'bytes') or '0'), 0)
local moved = 0
for _, set in ipairs({'due', 'dead'}) do
  local state = set == 'due' and 'waiting' or 'failed'
  local ids = redis.call('ZRANGE', s .. set, 0, -1, 'WITHSCORES')
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
return out
"#,
);

/// `job_adopt` on Redis: one script (see `REDIS_JOB_ADOPT`).
fn redis_job_adopt(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    names: &[String],
    apply: bool,
    cap: JobCap,
) -> Result<Vec<AdoptedJob>, String> {
    let (mut conn, until) = job_conn(spec, ns)?;
    let SessionConn::Redis(c) = &mut conn else { unreachable!("the URL is Redis") };
    let (src, dst) = (jobs_prefix(None), jobs_prefix(ns));
    let (jobs_cap, bytes_cap) = (cap.jobs.to_string(), cap.bytes.to_string());
    let mode: &[u8] = if apply { b"move" } else { b"dry" };
    let mut args: Vec<&[u8]> =
        vec![b"2", src.as_bytes(), dst.as_bytes(), mode, jobs_cap.as_bytes(), bytes_cap.as_bytes()];
    args.extend(names.iter().map(|n| n.as_bytes()));
    let r = REDIS_JOB_ADOPT.run(c.get(), &args, until, 32 << 20);
    let Reply::Array(items) = job_redis_result(c, r)? else {
        return Err("the server sent an invalid reply".into());
    };
    let text = |r: &Reply| match r {
        Reply::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    };
    Ok(items
        .chunks(4)
        .filter(|c| c.len() == 4)
        .map(|c| AdoptedJob {
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
        })
        .collect())
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
