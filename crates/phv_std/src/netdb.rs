//! Database servers named in phv.toml: PostgreSQL and MySQL behind the
//! `Database` class, and Redis. Scripts only name a server
//! (`Database::connect("main")`); the URL and its credentials stay in the
//! configuration.
//!
//! Connections come from a pool shared by every script in the process. A
//! connection goes back to the pool when the script ends or closes it, after
//! its session is reset (open transaction rolled back, settings cleared), so
//! nothing carries over from one request to the next.
//!
//! The script's limits hold on the client side too: statements stop at its
//! deadline even when the server ignores (or the script switched off) the
//! server's own timeout, and a result larger than the memory left to the
//! script is refused while it is read, before it fills the process.

use crate::db::{parts, render_sql, Dialect, ResultRows};
use crate::pgmeter::{Meter, PgClient, Trip};
use crate::redisclient::{RedisError, Reply};
use crate::{int, list, s};
use indexmap::IndexMap;
use phv_vm::{DatabaseSpec, Key, Value, Vm, VmError};
use postgres::types::ToSql;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::rc::Rc;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type R = Result<Value, VmError>;

/// Longest wait for a free connection or a new one when the script has no time limit.
const MAX_WAIT: Duration = Duration::from_secs(30);
/// Longest wait for a server to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection idle for longer is checked before it is handed out again.
const CHECK_AFTER: Duration = Duration::from_secs(30);
/// Longest time for the statements that reset or check a pooled connection.
const RESET_TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes those statements (and a connection's setup) may read.
const RESET_BUDGET: usize = 16 << 20;

// ----- Pool ------------------------------------------------------------------

trait Driver: Send + Sized + 'static {
    /// Returns the session to its initial state; false when the connection cannot be reused.
    fn reset(&mut self) -> bool;
    /// Whether a connection that sat idle still works.
    fn check(&mut self) -> bool;
    /// Whether the connection is known to be closed (checked without a round trip).
    fn closed(&self) -> bool {
        false
    }
}

struct Pool<C> {
    state: Mutex<PoolState<C>>,
    freed: Condvar,
    max: usize,
}

struct PoolState<C> {
    idle: Vec<(C, Instant)>,
    /// Connections in use or idle.
    open: usize,
}

type Registry<C> = LazyLock<Mutex<HashMap<String, Arc<Pool<C>>>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn pool_for<C: Driver>(registry: &Registry<C>, key: &str, max: usize) -> Arc<Pool<C>> {
    lock(registry)
        .entry(key.to_string())
        .or_insert_with(|| {
            Arc::new(Pool { state: Mutex::new(PoolState { idle: Vec::new(), open: 0 }), freed: Condvar::new(), max })
        })
        .clone()
}

enum AcquireError {
    Busy,
    Connect(String),
}

impl<C: Driver> Pool<C> {
    fn acquire(
        self: &Arc<Self>,
        until: Instant,
        connect: impl FnOnce() -> Result<C, String>,
    ) -> Result<Pooled<C>, AcquireError> {
        let mut st = lock(&self.state);
        loop {
            if let Some((mut c, since)) = st.idle.pop() {
                drop(st);
                if !c.closed() && (since.elapsed() < CHECK_AFTER || c.check()) {
                    return Ok(Pooled { conn: Some(c), pool: self.clone(), broken: false, depth: 0 });
                }
                drop(c);
                st = lock(&self.state);
                st.open -= 1;
                continue;
            }
            if st.open < self.max {
                st.open += 1;
                drop(st);
                return match connect() {
                    Ok(c) => Ok(Pooled { conn: Some(c), pool: self.clone(), broken: false, depth: 0 }),
                    Err(e) => {
                        self.discard();
                        Err(AcquireError::Connect(e))
                    }
                };
            }
            let now = Instant::now();
            if now >= until {
                return Err(AcquireError::Busy);
            }
            st = self.freed.wait_timeout(st, until - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn discard(&self) {
        lock(&self.state).open -= 1;
        self.freed.notify_one();
    }
}

/// A connection taken from a pool; dropping it resets the session and returns it.
struct Pooled<C: Driver> {
    conn: Option<C>,
    pool: Arc<Pool<C>>,
    /// Set after a network error: the connection is closed instead of reused.
    broken: bool,
    /// How many `transaction()` calls are open (inner ones are savepoints).
    depth: u32,
}

impl<C: Driver> Pooled<C> {
    fn get(&mut self) -> &mut C {
        self.conn.as_mut().expect("pooled connection")
    }
}

impl<C: Driver> Drop for Pooled<C> {
    fn drop(&mut self) {
        let Some(mut c) = self.conn.take() else { return };
        if !self.broken && c.reset() {
            lock(&self.pool.state).idle.push((c, Instant::now()));
            self.pool.freed.notify_one();
        } else {
            drop(c);
            self.pool.discard();
        }
    }
}

static PG_POOLS: Registry<PgConn> = LazyLock::new(Default::default);
static MY_POOLS: Registry<MyConn> = LazyLock::new(Default::default);
static REDIS_POOLS: Registry<RedisConn> = LazyLock::new(Default::default);

// ----- Connections held by a script ------------------------------------------

enum Link {
    Pg(Box<Pooled<PgConn>>),
    My(Box<Pooled<MyConn>>),
    Redis(Pooled<RedisConn>),
}

/// A script's handle to a server connection. Connecting to the same name twice
/// in one script shares the connection, so a script never waits on itself for
/// a free connection.
struct NetDb {
    name: String,
    link: Rc<RefCell<Link>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Postgres,
    MySql,
    Redis,
}

fn kind_of(url: &str) -> Kind {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Kind::Postgres
    } else if url.starts_with("mysql://") {
        Kind::MySql
    } else {
        Kind::Redis
    }
}

pub(crate) fn is_local_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The rustls provider every TLS connection uses (ring; installed once as the
/// process default for the clients that build their own configuration).
pub(crate) fn tls_init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn link_of(vm: &Vm, h: &Value) -> Result<Rc<RefCell<Link>>, VmError> {
    vm.resources
        .get(int(h) as usize)
        .and_then(|r| r.as_ref())
        .and_then(|b| b.downcast_ref::<NetDb>())
        .map(|d| d.link.clone())
        .ok_or_else(|| vm.throw("DatabaseError", "the database connection is closed"))
}

fn push(vm: &mut Vm, name: &str, link: Rc<RefCell<Link>>) -> R {
    vm.resources.push(Some(Box::new(NetDb { name: name.to_string(), link })));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
}

/// `__db_connect(name)` and `__redis_connect(name)`.
fn open(vm: &mut Vm, name: &str, want_redis: bool) -> R {
    let shared = vm
        .resources
        .iter()
        .flatten()
        .find_map(|r| r.downcast_ref::<NetDb>().filter(|d| d.name == name).map(|d| d.link.clone()));
    if let Some(link) = shared {
        let is_redis = matches!(&*link.borrow(), Link::Redis(_));
        if is_redis != want_redis {
            return Err(wrong_kind(vm, name, is_redis));
        }
        return push(vm, name, link);
    }
    let Some(spec) = vm.host.databases.get(name).cloned() else {
        return Err(vm.throw("DatabaseError", format!("no database named \"{name}\" in [databases] of phv.toml")));
    };
    let Some(url) = spec.url.clone() else {
        let var = spec.url_env.as_deref().unwrap_or("?");
        return Err(
            vm.throw("DatabaseError", format!("database \"{name}\": the environment variable {var} is not set"))
        );
    };
    let kind = kind_of(&url);
    if (kind == Kind::Redis) != want_redis {
        return Err(wrong_kind(vm, name, kind == Kind::Redis));
    }
    tls_init();
    let until = crate::deadline::until(vm, MAX_WAIT);
    let (memory, time_limit) = (vm.memory_limit(), vm.time_limit());
    let use_ = PoolUse::Scripts;
    let result = match kind {
        Kind::Postgres => {
            pooled(&PG_POOLS, &spec, &url, use_, until, |t| pg_connect(&url, &spec, t)).map(|p| Link::Pg(Box::new(p)))
        }
        Kind::MySql => pooled(&MY_POOLS, &spec, &url, use_, until, |t| my_connect(&url, &spec, t, memory, time_limit))
            .map(|p| Link::My(Box::new(p))),
        Kind::Redis => {
            pooled(&REDIS_POOLS, &spec, &url, use_, until, |t| redis_connect(&url, &spec, t)).map(Link::Redis)
        }
    };
    match result {
        Ok(link) => push(vm, name, Rc::new(RefCell::new(link))),
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

fn wrong_kind(vm: &Vm, name: &str, is_redis: bool) -> VmError {
    let hint =
        if is_redis { "a Redis server; use Redis::connect()" } else { "an SQL database; use Database::connect()" };
    vm.throw("DatabaseError", format!("database \"{name}\" is {hint}"))
}

pub fn connect(vm: &mut Vm, a: &[Value]) -> R {
    open(vm, s(&a[0]), false)
}

pub fn connect_redis(vm: &mut Vm, a: &[Value]) -> R {
    open(vm, s(&a[0]), true)
}

pub fn query(vm: &mut Vm, a: &[Value]) -> R {
    let link = link_of(vm, &a[0])?;
    let mut link = link.borrow_mut();
    match &mut *link {
        Link::Pg(c) => pg_query(vm, c, &a[1]),
        Link::My(c) => my_query(vm, c, &a[1]),
        Link::Redis(_) => unreachable!("Redis handle used as a database"),
    }
}

pub fn execute(vm: &mut Vm, a: &[Value]) -> R {
    let link = link_of(vm, &a[0])?;
    let mut link = link.borrow_mut();
    match &mut *link {
        Link::Pg(c) => pg_execute(vm, c, &a[1]),
        Link::My(c) => my_execute(vm, c, &a[1]),
        Link::Redis(_) => unreachable!("Redis handle used as a database"),
    }
}

/// `BEGIN` / `COMMIT` / `ROLLBACK` for `Database::transaction()`.
pub fn raw(vm: &mut Vm, a: &[Value]) -> R {
    let link = link_of(vm, &a[0])?;
    let mut link = link.borrow_mut();
    let cmd = s(&a[1]);
    match &mut *link {
        Link::Pg(c) => {
            let (sqls, depth) = crate::db::transaction_sql(cmd, c.depth);
            if cmd == "ROLLBACK" {
                c.depth = depth;
            }
            let r = pg_run(vm, c, |c, mut prelude| {
                for sql in &sqls {
                    let client = &c.get().client;
                    client.run(prelude.take(), client.client().batch_execute(sql))?;
                }
                Ok(())
            });
            // A transaction boundary: kept statements prepared before it are
            // not used inside the next transaction (see `PgConn::kept`).
            c.get().epoch += 1;
            if depth == 0 {
                if let Some(j) = c.get().jobs_tx.take() {
                    j.ended();
                }
            }
            r?;
            c.depth = depth;
        }
        Link::My(c) => {
            use mysql::prelude::Queryable;
            let (sqls, depth) = crate::db::transaction_sql(cmd, c.depth);
            if cmd == "ROLLBACK" {
                c.depth = depth;
            }
            let mut r = Ok(());
            for sql in &sqls {
                r = c.get().conn.query_drop(sql);
                if r.is_err() {
                    break;
                }
            }
            if depth == 0 {
                if let Some(j) = c.get().jobs_tx.take() {
                    j.ended();
                }
            }
            r.map_err(|e| my_error(vm, c, e))?;
            c.depth = depth;
        }
        Link::Redis(_) => unreachable!("Redis handle used as a database"),
    }
    Ok(Value::Null)
}

pub fn last_insert_id(vm: &mut Vm, a: &[Value]) -> R {
    let link = link_of(vm, &a[0])?;
    let mut link = link.borrow_mut();
    match &mut *link {
        Link::Pg(_) => {
            Err(vm
                .throw("DatabaseError", "PostgreSQL has no last insert id; use `INSERT ... RETURNING id` with first()"))
        }
        Link::My(c) => Ok(Value::Int(c.get().conn.last_insert_id().try_into().unwrap_or(i64::MAX))),
        Link::Redis(_) => unreachable!("Redis handle used as a database"),
    }
}

/// The fatal error for a result that does not fit in the memory left to the script.
fn out_of_memory(vm: &Vm) -> VmError {
    match vm.check_memory() {
        Err(e) => e,
        Ok(()) => vm.reserve(usize::MAX).expect_err("reserving usize::MAX fails"),
    }
}

/// The bytes a statement may read: about half the memory left to the script
/// (the values made from them take about as much again).
fn read_budget(vm: &Vm) -> usize {
    (vm.memory_left() / 2).max(64 << 10)
}

/// The time left before the script's limit (at least a millisecond, so a
/// server timeout set from it is never zero), or None without a limit.
fn remaining(vm: &Vm) -> Result<Option<Duration>, VmError> {
    vm.check_limits()?;
    Ok(crate::deadline::remaining(vm).map(|left| left.max(Duration::from_millis(1))))
}

/// Server timeouts end this long after the script's deadline, so the script's
/// own limit has always passed when the server stops a statement (otherwise a
/// statement cut a moment early would look like an ordinary error).
const TIMEOUT_MARGIN_MS: u64 = 100;

/// Whether the server timeout must be set again: it is lowered as the
/// deadline approaches, so a statement never runs much past the time limit.
fn needs_new_timeout(current: Option<u64>, ms: u64) -> bool {
    current.is_none_or(|t| ms < t * 3 / 4)
}

// ----- PostgreSQL ------------------------------------------------------------

/// Runs a prepared statement (after a prelude, see `PgClient::run`).
type RunStatement<'a, T> =
    &'a mut dyn FnMut(&PgClient, &postgres::Statement, Option<&str>) -> Result<T, postgres::Error>;

/// Statements a connection keeps prepared (the most recently used ones).
const KEPT_STATEMENTS: usize = 64;
/// Longer statements are prepared each time, not kept.
const KEPT_TEXT: usize = 4 << 10;
/// The texts of the statements a connection keeps, at most (64 KB for each of
/// up to 256 connections of a pool).
const KEPT_BYTES: usize = 64 << 10;

/// A statement kept prepared on a connection, by its text.
struct Kept {
    text: String,
    stmt: postgres::Statement,
    /// The `PgConn::epoch` it was prepared in.
    epoch: u64,
    /// Its name on the server (`statement_name`).
    name: Option<String>,
}

/// The name a statement was prepared under on the server. The client shows
/// it only in `Debug` (`Statement { name: "s12", .. }`).
fn statement_name(stmt: &postgres::Statement) -> Option<String> {
    let shown = format!("{stmt:?}");
    let rest = shown.strip_prefix("Statement { name: \"")?;
    Some(rest.split_once('"')?.0.to_string()).filter(|n| !n.is_empty())
}

/// The most statements a connection's client prepared for itself that the
/// reset checks (`PgConn::own`): the sentinel, and the client's statements
/// that look types up (three at most).
const OWN_STATEMENTS: usize = 16;

struct PgConn {
    client: PgClient,
    /// The `statement_timeout` last set on the session, in milliseconds.
    timeout_ms: Option<u64>,
    /// Stops reads and writes at the statement's deadline and byte budget.
    meter: Meter,
    /// Statements kept prepared, least recently used first. A kept statement
    /// whose tables changed since fails with an error of its own (`stale`);
    /// outside a transaction it is then prepared again and run once more.
    /// Inside one that is impossible (the error ends the transaction), so
    /// there only statements prepared in the same `epoch` are used.
    kept: Vec<Kept>,
    /// Changes at every transaction boundary and after every statement that
    /// may change the schema or the session (anything other than plain
    /// `SELECT` / `INSERT` / `UPDATE` / `DELETE` / `WITH` / `VALUES`).
    epoch: u64,
    /// A transaction the script began with its own `BEGIN` (not `transaction()`).
    in_own_transaction: bool,
    /// The jobs the script's open transaction added (`job_add_in_transaction`).
    jobs_tx: Option<JobsTx>,
    /// The deadline of an operation of PHV's own stores (sessions, jobs) on
    /// this connection: each of its statements is sent with a
    /// `statement_timeout` that ends there (see `store_prelude`).
    store_until: Option<Instant>,
    /// A statement prepared when the connection was made and never closed:
    /// gone only when a script deallocated the client's statements (see
    /// `PG_RESET`). Held so the client never closes it; the reset looks for
    /// it by its name, the first of `own`.
    #[allow(dead_code)]
    sentinel: postgres::Statement,
    /// The server's names of the statements the client prepared for itself
    /// and keeps for its whole life: the sentinel's, then those that look
    /// types up, as the reset finds them (see `PgConn::reset_session`).
    own: Vec<String>,
    /// Whether the client looked a type up (with statements of its own) since
    /// it was made.
    looked_up: bool,
}

/// Clears what a script can leave on a session, in one round trip. This is
/// `DISCARD ALL` without its `DEALLOCATE ALL`, which would also remove the
/// statements the client prepares for itself (to look up types, and the kept
/// statements) and break the connection for the next script; the statements
/// scripts prepare are deallocated by name. Outside a transaction `ROLLBACK`
/// only warns; inside one it ends it, so the reset itself does not run inside
/// the script's transaction.
///
/// The last statement lists the session's prepared statements: those of
/// SQL's `PREPARE` (`sql <name>`), which are deallocated, and those the
/// client prepared (`own <name>`). When a script deallocated statements the
/// client prepared for itself (`DEALLOCATE ALL`, or `DISCARD ALL`, perhaps
/// inside a function where PHV cannot see it), the client still holds those
/// it prepared to look up types, and would fail on them for every later
/// script ("prepared statement ... does not exist"); such a connection is
/// closed instead of reused (found by the 1.1.1 tests). Each statement is
/// looked for by its name, which a script cannot give its own statements
/// (found by the 1.2 audit: the sentinel was looked for by its text, which a
/// script could prepare again, and a script could deallocate the type
/// lookups alone).
const PG_RESET: &str = "ROLLBACK; CLOSE ALL; SET SESSION AUTHORIZATION DEFAULT; RESET ALL; UNLISTEN *; \
    SELECT pg_advisory_unlock_all(); DISCARD PLANS; DISCARD TEMP; DISCARD SEQUENCES; \
    SELECT CASE WHEN from_sql THEN 'sql ' ELSE 'own ' END || name FROM pg_prepared_statements";

/// What a statement's text is, for the kept statements: only plain data
/// statements are kept; the others are run as before, and may change the
/// schema or the transaction, so the kept statements are dropped after them.
#[derive(Clone, Copy, PartialEq, Debug)]
enum PgText {
    Plain,
    Begin,
    /// `COMMIT` / `ROLLBACK` / `END` / `ABORT` alone (`ROLLBACK TO ...` is `Other`).
    End,
    Other,
}

fn pg_text(text: &str) -> PgText {
    let words: Vec<String> =
        text.trim().trim_end_matches(';').split_ascii_whitespace().take(3).map(|w| w.to_ascii_uppercase()).collect();
    let word = |i: usize| words.get(i).map_or("", String::as_str);
    match word(0) {
        // `SELECT ... INTO` creates a table.
        "SELECT" | "WITH" if text.split_ascii_whitespace().any(|w| w.eq_ignore_ascii_case("INTO")) => PgText::Other,
        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "WITH" | "VALUES" => PgText::Plain,
        "BEGIN" | "START" => PgText::Begin,
        "COMMIT" | "ROLLBACK" | "END" | "ABORT"
            if matches!(word(1), "" | "WORK" | "TRANSACTION") && words.len() <= 2 =>
        {
            PgText::End
        }
        _ => PgText::Other,
    }
}

/// Whether an error of a kept statement may come from the statement being
/// older than the schema: it was deallocated, its result changed shape, or a
/// column it compares or sets changed type (its parameters keep the types
/// they had when it was prepared). Each is raised before the statement runs,
/// so running it again, prepared anew, cannot repeat what it did.
fn stale(e: &postgres::Error) -> bool {
    use postgres::error::SqlState;
    match e.as_db_error() {
        Some(db) => {
            let code = db.code();
            code == &SqlState::INVALID_SQL_STATEMENT_NAME
                || code == &SqlState::DATATYPE_MISMATCH
                || code == &SqlState::UNDEFINED_FUNCTION
                || (code == &SqlState::FEATURE_NOT_SUPPORTED
                    && db.message().contains("cached plan must not change result type"))
        }
        None => false,
    }
}

impl PgConn {
    /// Meters the statements PHV runs on its own (reset, check).
    fn meter_internal(&self) {
        self.meter.set(Some(Instant::now() + RESET_TIMEOUT), RESET_BUDGET);
    }

    fn reset_session(&mut self) -> bool {
        let Ok(rows) = self.client.simple_last_column(PG_RESET) else { return false };
        let mut on_server = HashSet::new();
        for row in &rows {
            if let Some(n) = row.strip_prefix("sql ") {
                if self.client.batch_execute(&format!("DEALLOCATE \"{}\"", n.replace('"', "\"\""))).is_err() {
                    return false;
                }
            } else if let Some(n) = row.strip_prefix("own ") {
                on_server.insert(n);
            }
        }
        // Every statement the client prepared for itself is still there.
        if !self.own.iter().all(|n| on_server.contains(n.as_str())) {
            return false;
        }
        // The client's other statements are the kept ones (any other was
        // closed when the client dropped it, before this reset): the rest are
        // those it prepared to look types up, which it keeps from now on.
        let kept: Option<HashSet<&str>> = self.kept.iter().map(|k| k.name.as_deref()).collect();
        let Some(kept) = kept else { return true };
        for n in on_server {
            if !kept.contains(n) && !self.own.iter().any(|o| o == n) && self.own.len() < OWN_STATEMENTS {
                self.own.push(n.to_string());
            }
        }
        // A type was looked up, but no statement that does is left: a script
        // deallocated them in the same run as the first lookup.
        !(self.looked_up && self.own.len() < 2)
    }

    /// Drops the kept statements (and the types the client looked up, which a
    /// script's `DEALLOCATE ALL` or a changed type would make wrong).
    fn forget_statements(&mut self) {
        self.kept.clear();
        self.client.client().clear_type_cache();
        self.epoch += 1;
    }

    /// The kept statement for `text`, when it may be used: inside a
    /// transaction only one prepared in the same epoch.
    fn kept(&mut self, text: &str, in_transaction: bool) -> Option<postgres::Statement> {
        let i = self.kept.iter().position(|k| k.text == text)?;
        let k = self.kept.remove(i);
        if in_transaction && k.epoch != self.epoch {
            return None;
        }
        let stmt = k.stmt.clone();
        self.kept.push(k);
        Some(stmt)
    }

    fn keep(&mut self, text: &str, stmt: &postgres::Statement) {
        if text.len() > KEPT_TEXT {
            return;
        }
        while !self.kept.is_empty()
            && (self.kept.len() >= KEPT_STATEMENTS
                || self.kept.iter().map(|k| k.text.len()).sum::<usize>() + text.len() > KEPT_BYTES)
        {
            self.kept.remove(0);
        }
        let name = statement_name(stmt);
        self.kept.push(Kept { text: text.to_string(), stmt: stmt.clone(), epoch: self.epoch, name });
    }

    /// Runs `run` with the statement for `text`: a kept one, or one prepared
    /// now (kept when `keep`). A `prelude` goes with the first request (see
    /// `PgClient::run`). A kept statement that turns out stale is prepared
    /// again and run once more; if that cannot be done (inside a
    /// transaction), its first error is returned.
    fn with_statement<T>(
        &mut self,
        text: &str,
        keep: bool,
        in_transaction: bool,
        prelude: Option<&str>,
        run: RunStatement<'_, T>,
    ) -> Result<T, postgres::Error> {
        let store = if prelude.is_none() { self.store_prelude() } else { None };
        let prelude = prelude.or(store.as_deref());
        if let Some(stmt) = keep.then(|| self.kept(text, in_transaction)).flatten() {
            match run(&self.client, &stmt, prelude) {
                Err(e) if stale(&e) => {
                    // The prelude, if any, ran before the statement failed.
                    self.forget_statements();
                    let again = self.client.run(None, self.client.client().prepare(text)).and_then(|fresh| {
                        self.note_types(&fresh);
                        let r = run(&self.client, &fresh, None);
                        self.keep(text, &fresh);
                        r
                    });
                    return match again {
                        Err(e2) if e2.code() == Some(&postgres::error::SqlState::IN_FAILED_SQL_TRANSACTION) => Err(e),
                        r => r,
                    };
                }
                r => return r,
            }
        }
        let stmt = self.client.run(prelude, self.client.client().prepare(text))?;
        self.note_types(&stmt);
        if keep {
            self.keep(text, &stmt);
        }
        run(&self.client, &stmt, None)
    }

    /// Notes when `stmt` has a type the client does not know by itself: the
    /// client looked it up, with statements it prepared for itself.
    fn note_types(&mut self, stmt: &postgres::Statement) {
        use postgres::types::Type;
        let looked_up = |t: &Type| Type::from_oid(t.oid()).is_none();
        if stmt.params().iter().any(looked_up) || stmt.columns().iter().any(|c| looked_up(c.type_())) {
            self.looked_up = true;
        }
    }
}

/// PHV's own statements (sessions, jobs), through the kept statements. During
/// an operation of a store each goes with a `statement_timeout` that ends at
/// the operation's deadline, so the server stops it there, lock waits
/// included (found by the 1.1.1 audit: a job added with a key that an open
/// transaction had added waited on the server for as long as that transaction
/// lasted, after the client had given up).
impl PgConn {
    fn store_prelude(&self) -> Option<String> {
        let until = self.store_until?;
        let ms = until.saturating_duration_since(Instant::now()).as_millis().clamp(1, i32::MAX as u128);
        Some(format!("SET statement_timeout = {ms}"))
    }

    /// Runs `sql` (simple statements) after the store's `statement_timeout`.
    fn store_batch(&self, sql: &str) -> Result<(), postgres::Error> {
        match self.store_prelude() {
            Some(set) => self.client.batch_execute(&format!("{set}; {sql}")),
            None => self.client.batch_execute(sql),
        }
    }

    /// `CREATE … IF NOT EXISTS` statements: when another connection creates the
    /// same table or index at the same moment, PostgreSQL reports a duplicate
    /// (in its catalog) instead of finding it, once that connection commits. The
    /// statements are run again, and then find it (found by CI: several
    /// processes making the shared table of job counts at once).
    fn store_create(&self, sql: &str) -> Result<(), postgres::Error> {
        use postgres::error::SqlState;
        let mut tries = 0;
        loop {
            match self.store_batch(sql) {
                Err(e)
                    if tries < 3
                        && [SqlState::UNIQUE_VIOLATION, SqlState::DUPLICATE_TABLE, SqlState::DUPLICATE_OBJECT]
                            .iter()
                            .any(|c| e.code() == Some(c)) =>
                {
                    tries += 1;
                }
                r => return r,
            }
        }
    }

    fn query_kept(
        &mut self,
        text: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<postgres::Row>, postgres::Error> {
        self.with_statement(text, true, false, None, &mut |client, stmt, prelude| {
            client.run(prelude, client.client().query(stmt, params))
        })
    }

    fn execute_kept(&mut self, text: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, postgres::Error> {
        self.with_statement(text, true, false, None, &mut |client, stmt, prelude| {
            client.run(prelude, client.client().execute(stmt, params))
        })
    }
}

impl Driver for PgConn {
    fn reset(&mut self) -> bool {
        if self.client.is_closed() {
            return false;
        }
        self.timeout_ms = None;
        self.in_own_transaction = false;
        self.jobs_tx = None;
        self.store_until = None;
        self.epoch += 1;
        self.meter_internal();
        let ok = self.reset_session();
        self.meter.set(None, RESET_BUDGET);
        ok && !self.client.is_closed()
    }

    fn check(&mut self) -> bool {
        self.meter_internal();
        let ok = self.client.is_valid(Duration::from_secs(5));
        self.meter.set(None, RESET_BUDGET);
        ok
    }

    fn closed(&self) -> bool {
        self.client.is_closed()
    }
}

fn pg_connect(url: &str, spec: &DatabaseSpec, timeout: Duration) -> Result<PgConn, String> {
    use tokio_postgres::config::{Host, SslMode};
    // The parser's own message could quote the URL, which may hold a password.
    let mut config: tokio_postgres::Config =
        url.parse().map_err(|_| "the URL is not a valid PostgreSQL URL".to_string())?;
    let local = config.get_hosts().iter().all(|h| match h {
        Host::Tcp(name) => is_local_host(name),
        #[allow(unreachable_patterns)]
        _ => true,
    }) && config.get_hostaddrs().iter().all(|ip| ip.is_loopback());
    // `sslmode=require` in the URL is kept even when phv.toml would allow plain connections.
    let tls = config.get_ssl_mode() == SslMode::Require || spec.tls.unwrap_or(!local);
    config.ssl_mode(if tls { SslMode::Require } else { SslMode::Disable });
    let meter = Meter::default();
    meter.set(Some(Instant::now() + timeout + RESET_TIMEOUT), RESET_BUDGET);
    let make_tls = || tokio_postgres_rustls::MakeRustlsConnect::new((*crate::netio::tls_config()).clone());
    let client = PgClient::connect(&config, tls.then(make_tls), timeout, meter.clone())?;
    let text = format!("SELECT 1 AS phv_{}", crate::random_hex(8));
    let sentinel = client.run(None, client.client().prepare(&text)).map_err(|e| pg_message(&e))?;
    // Its name, which no script has had the chance to prepare a statement under yet.
    let named = client
        .query("SELECT name FROM pg_prepared_statements WHERE NOT from_sql AND statement = $1", &[&text])
        .map_err(|e| pg_message(&e))?;
    let name = named
        .first()
        .and_then(|r| r.try_get::<_, String>(0).ok())
        .or_else(|| statement_name(&sentinel))
        .ok_or_else(|| "the server does not list the connection's statements".to_string())?;
    meter.set(None, RESET_BUDGET);
    Ok(PgConn {
        client,
        timeout_ms: None,
        meter,
        kept: Vec::new(),
        epoch: 0,
        in_own_transaction: false,
        jobs_tx: None,
        store_until: None,
        sentinel,
        own: vec![name],
        looked_up: false,
    })
}

pub(crate) fn pg_message(e: &postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => match db.detail() {
            Some(d) => format!("{} ({d})", db.message()),
            None => db.message().to_string(),
        },
        // Network and TLS errors are described without the server's address.
        None if network_cause(e).is_some() => network_cause(e).unwrap_or_default(),
        // Conversion errors keep the reason in the source ("error serializing parameter 0: ...").
        None => match std::error::Error::source(e) {
            Some(cause) => format!("{e}: {cause}"),
            None => e.to_string(),
        },
    }
}

/// A network or TLS failure somewhere in an error's sources, in fixed words.
fn network_cause(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return Some(crate::netio::describe(io));
        }
        if err.downcast_ref::<rustls::Error>().is_some() {
            return Some("the TLS connection failed".into());
        }
        cur = err.source();
    }
    None
}

/// Why a statement failed, as `pg_failure` sees it.
enum PgFailure {
    /// The result was larger than the byte budget.
    Budget,
    /// The client-side deadline passed.
    Deadline,
    /// The server stopped it (`statement_timeout`, set from the time limit).
    Canceled,
    Other,
}

/// Classifies a failed statement; the connection is marked broken when it
/// cannot be reused (closed, or stopped by the meter in the middle of a reply).
fn pg_failure(c: &mut Pooled<PgConn>, e: &postgres::Error) -> PgFailure {
    if c.get().client.is_closed() {
        c.broken = true;
    }
    // A prepared statement that does not exist, past the retry of a stale
    // kept one: perhaps one the client prepared for itself to look types up,
    // which a script deallocated where the reset could not tell. The client
    // would fail on it for every later script (found by the 1.2 audit).
    if e.code() == Some(&postgres::error::SqlState::INVALID_SQL_STATEMENT_NAME) {
        c.broken = true;
    }
    match c.get().meter.tripped() {
        Some(Trip::Budget) => {
            c.broken = true;
            PgFailure::Budget
        }
        Some(Trip::Deadline) => {
            c.broken = true;
            PgFailure::Deadline
        }
        None if e.code() == Some(&postgres::error::SqlState::QUERY_CANCELED) => PgFailure::Canceled,
        None => PgFailure::Other,
    }
}

fn pg_error(vm: &Vm, c: &mut Pooled<PgConn>, e: postgres::Error) -> VmError {
    match pg_failure(c, &e) {
        PgFailure::Budget => out_of_memory(vm),
        PgFailure::Deadline => {
            if let Err(fatal) = vm.check_limits() {
                return fatal;
            }
            vm.throw("DatabaseError", "the statement ran out of time")
        }
        PgFailure::Canceled => {
            if let Err(fatal) = vm.check_limits() {
                return fatal;
            }
            vm.throw("DatabaseError", pg_message(&e))
        }
        PgFailure::Other => vm.throw("DatabaseError", pg_message(&e)),
    }
}

/// Before a statement: sets the client-side deadline and byte budget, and
/// returns the `SET statement_timeout` to send with it when the server's
/// timeout must be lowered (`needs_new_timeout`) — which stops the statement
/// without closing the connection when the script has not changed it.
fn pg_limits(c: &mut Pooled<PgConn>, until: Option<Instant>, budget: usize) -> Option<(String, u64)> {
    c.get().meter.set(until.map(|u| u + Duration::from_millis(TIMEOUT_MARGIN_MS)), budget);
    let left = until?.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    let ms = left.as_millis().min(i32::MAX as u128) as u64;
    needs_new_timeout(c.get().timeout_ms, ms)
        .then(|| (format!("SET statement_timeout = {}", ms + TIMEOUT_MARGIN_MS), ms))
}

/// Runs one statement on a script's connection with its limits (see
/// `pg_limits`), recording the server timeout once it was set.
fn pg_run<T>(
    vm: &Vm,
    c: &mut Pooled<PgConn>,
    run: impl FnOnce(&mut Pooled<PgConn>, Option<&str>) -> Result<T, postgres::Error>,
) -> Result<T, VmError> {
    vm.check_limits()?;
    let set = pg_limits(c, vm.deadline(), read_budget(vm));
    let r = run(c, set.as_ref().map(|s| s.0.as_str()));
    let r = r.map_err(|e| pg_error(vm, c, e))?;
    if let Some((_, ms)) = set {
        c.get().timeout_ms = Some(ms);
    }
    Ok(r)
}

/// Runs a script's statement through the kept statements (`with_statement`),
/// then notes what it did to the transaction and the schema (`pg_text`).
fn pg_statement<T>(vm: &Vm, c: &mut Pooled<PgConn>, text: &str, run: RunStatement<'_, T>) -> Result<T, VmError> {
    let kind = pg_text(text);
    let in_transaction = c.depth > 0 || c.get().in_own_transaction;
    let r =
        pg_run(vm, c, |c, prelude| c.get().with_statement(text, kind == PgText::Plain, in_transaction, prelude, run));
    let conn = c.get();
    match kind {
        PgText::Plain => {}
        PgText::Begin | PgText::End => {
            if r.is_ok() {
                conn.in_own_transaction = kind == PgText::Begin;
            }
            conn.epoch += 1;
            if let Some(j) = conn.jobs_tx.take() {
                j.ended();
            }
        }
        PgText::Other => conn.forget_statements(),
    }
    r
}

fn pg_query(vm: &mut Vm, c: &mut Pooled<PgConn>, q: &Value) -> R {
    use postgres::types::ToSql;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::Postgres);
    let bound: Vec<PgParam> = params.iter().map(PgParam::new).collect();
    let mut out = ResultRows::new(Vec::new());
    let mut over = false;
    let vm_ref: &Vm = vm;
    pg_statement(vm_ref, c, &text, &mut |client, stmt, prelude| {
        out = ResultRows::new(stmt.columns().iter().map(|col| col.name().to_string()));
        let width = stmt.columns().len();
        client.query_each(prelude, stmt, bound.iter().map(|p| p as &dyn ToSql), |row| {
            let mut values = Vec::with_capacity(width);
            for i in 0..width {
                let PgOut(v) = row.try_get(i)?;
                values.push(v.into_value());
            }
            if out.push(vm_ref, values).is_err() {
                over = true;
                return Ok(false);
            }
            Ok(true)
        })
    })?;
    if over {
        // The rest of the result is not read: the connection is closed, not reused.
        c.broken = true;
        return Err(out_of_memory(vm));
    }
    Ok(out.into_value())
}

fn pg_execute(vm: &mut Vm, c: &mut Pooled<PgConn>, q: &Value) -> R {
    use postgres::types::ToSql;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::Postgres);
    let bound: Vec<PgParam> = params.iter().map(PgParam::new).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let n = pg_statement(vm, c, &text, &mut |client, stmt, prelude| {
        client.run(prelude, client.client().execute(stmt, &refs))
    })?;
    Ok(Value::Int(n.try_into().unwrap_or(i64::MAX)))
}

type BoxError = Box<dyn std::error::Error + Sync + Send>;

/// A PHV value bound to a PostgreSQL parameter, converted to the parameter's
/// type when it is sent.
#[derive(Debug)]
enum PgParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl PgParam {
    fn new(v: &Value) -> PgParam {
        match v {
            Value::Null => PgParam::Null,
            Value::Bool(b) => PgParam::Bool(*b),
            Value::Int(i) => PgParam::Int(*i),
            Value::Float(f) => PgParam::Float(*f),
            other => PgParam::Text(other.to_display_string()),
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            PgParam::Null => "null",
            PgParam::Bool(_) => "a bool",
            PgParam::Int(_) => "an int",
            PgParam::Float(_) => "a float",
            PgParam::Text(_) => "a string",
        }
    }
}

fn unsupported(what: &str, ty: &postgres::types::Type) -> BoxError {
    format!("cannot pass {what} as a parameter of type {ty}; cast it in the SQL, e.g. ({{$value}}::text)::{ty}").into()
}

impl postgres::types::ToSql for PgParam {
    fn to_sql(
        &self,
        ty: &postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<postgres::types::IsNull, BoxError> {
        pg_encode(self, ty, out)
    }

    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }

    postgres::types::to_sql_checked!();
}

fn pg_encode(
    v: &PgParam,
    ty: &postgres::types::Type,
    out: &mut bytes::BytesMut,
) -> Result<postgres::types::IsNull, BoxError> {
    use bytes::BufMut;
    use postgres::types::{IsNull, Kind, ToSql, Type};
    if let Kind::Domain(inner) = ty.kind() {
        return pg_encode(v, inner, out);
    }
    match v {
        PgParam::Null => return Ok(IsNull::Yes),
        PgParam::Bool(b) if *ty == Type::BOOL => return b.to_sql(ty, out),
        PgParam::Int(i) => match *ty {
            Type::INT2 => return i16::try_from(*i).map_err(|_| "integer out of range for smallint")?.to_sql(ty, out),
            Type::INT4 => return i32::try_from(*i).map_err(|_| "integer out of range for integer")?.to_sql(ty, out),
            Type::INT8 => return i.to_sql(ty, out),
            Type::FLOAT4 => return (*i as f32).to_sql(ty, out),
            Type::FLOAT8 => return (*i as f64).to_sql(ty, out),
            Type::NUMERIC => return numeric_encode(&i.to_string(), out),
            _ => {}
        },
        PgParam::Float(f) => match *ty {
            Type::FLOAT4 => return (*f as f32).to_sql(ty, out),
            Type::FLOAT8 => return f.to_sql(ty, out),
            Type::NUMERIC if f.is_finite() => return numeric_encode(&f.to_string(), out),
            _ => {}
        },
        PgParam::Text(text) => {
            let t = text.as_str();
            if is_text_type(ty) {
                out.put_slice(t.as_bytes());
                return Ok(IsNull::No);
            }
            match *ty {
                Type::JSON => {
                    out.put_slice(t.as_bytes());
                    return Ok(IsNull::No);
                }
                Type::JSONB => {
                    out.put_u8(1);
                    out.put_slice(t.as_bytes());
                    return Ok(IsNull::No);
                }
                Type::NUMERIC => return numeric_encode(t, out),
                Type::UUID => {
                    out.put_slice(&uuid_parse(t).ok_or_else(|| format!("\"{t}\" is not a UUID"))?);
                    return Ok(IsNull::No);
                }
                Type::DATE => return parse_date(t)?.to_sql(ty, out),
                Type::TIME => return parse_time(t)?.to_sql(ty, out),
                Type::TIMESTAMP => return parse_datetime(t)?.to_sql(ty, out),
                Type::TIMESTAMPTZ => return parse_datetime_tz(t)?.to_sql(ty, out),
                _ => {}
            }
        }
        _ => {}
    }
    Err(unsupported(v.type_name(), ty))
}

fn is_text_type(ty: &postgres::types::Type) -> bool {
    use postgres::types::{Kind, Type};
    matches!(*ty, Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN)
        || matches!(ty.kind(), Kind::Enum(_))
        || ty.name() == "citext"
}

fn parse_date(t: &str) -> Result<chrono::NaiveDate, BoxError> {
    chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").map_err(|_| format!("\"{t}\" is not a date (YYYY-MM-DD)").into())
}

fn parse_time(t: &str) -> Result<chrono::NaiveTime, BoxError> {
    chrono::NaiveTime::parse_from_str(t, "%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveTime::parse_from_str(t, "%H:%M"))
        .map_err(|_| format!("\"{t}\" is not a time (HH:MM:SS)").into())
}

fn parse_datetime(t: &str) -> Result<chrono::NaiveDateTime, BoxError> {
    chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f"))
        .or_else(|_| parse_date(t).map(|d| d.and_time(chrono::NaiveTime::MIN)).map_err(|_| ()))
        .map_err(|_| format!("\"{t}\" is not a timestamp (YYYY-MM-DD HH:MM:SS)").into())
}

/// A timestamp with an offset (`+09:00`, `Z`); without one it is taken as UTC.
fn parse_datetime_tz(t: &str) -> Result<chrono::DateTime<chrono::Utc>, BoxError> {
    let with_offset = chrono::DateTime::parse_from_rfc3339(t)
        .or_else(|_| chrono::DateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f%:z"))
        .or_else(|_| chrono::DateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f%#z"));
    match with_offset {
        Ok(d) => Ok(d.with_timezone(&chrono::Utc)),
        Err(_) => Ok(parse_datetime(t)?.and_utc()),
    }
}

fn uuid_parse(t: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = t.bytes().filter(|&b| b != b'-').collect();
    if hex.len() != 32 || t.len() > 36 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// Writes a decimal string in PostgreSQL's binary NUMERIC format.
fn numeric_encode(t: &str, out: &mut bytes::BytesMut) -> Result<postgres::types::IsNull, BoxError> {
    use bytes::BufMut;
    let bad = || -> BoxError { format!("\"{t}\" is not a number").into() };
    let s = t.trim();
    if s.eq_ignore_ascii_case("nan") {
        out.put_slice(&[0, 0, 0, 0, 0xC0, 0, 0, 0]);
        return Ok(postgres::types::IsNull::No);
    }
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (ip, fp) = body.split_once('.').unwrap_or((body, ""));
    let digits_only = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if (ip.is_empty() && fp.is_empty()) || !digits_only(ip) || !digits_only(fp) || fp.len() > 16383 || ip.len() > 131072
    {
        return Err(bad());
    }
    let ip = ip.trim_start_matches('0');
    let int_digits = format!("{}{ip}", "0".repeat((4 - ip.len() % 4) % 4));
    let frac_digits = format!("{fp}{}", "0".repeat((4 - fp.len() % 4) % 4));
    let group = |c: &[u8]| c.iter().fold(0i16, |n, d| n * 10 + (d - b'0') as i16);
    let mut groups: Vec<i16> =
        int_digits.as_bytes().chunks(4).chain(frac_digits.as_bytes().chunks(4)).map(group).collect();
    let mut weight = (int_digits.len() / 4) as i32 - 1;
    let lead = groups.iter().take_while(|&&g| g == 0).count();
    groups.drain(..lead);
    weight -= lead as i32;
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    let sign: u16 = if neg && !groups.is_empty() { 0x4000 } else { 0 };
    out.put_i16(groups.len() as i16);
    out.put_i16(i16::try_from(weight).map_err(|_| bad())?);
    out.put_u16(sign);
    out.put_u16(fp.len() as u16);
    for g in groups {
        out.put_i16(g);
    }
    Ok(postgres::types::IsNull::No)
}

/// Reads PostgreSQL's binary NUMERIC format as a decimal string (no precision is lost).
fn numeric_decode(raw: &[u8]) -> Result<String, BoxError> {
    use std::fmt::Write;
    let rd = |i: usize| -> Result<u16, BoxError> {
        raw.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]])).ok_or_else(|| "invalid numeric value".into())
    };
    let ndigits = rd(0)? as usize;
    let weight = rd(2)? as i16 as i32;
    let sign = rd(4)?;
    let dscale = rd(6)? as usize;
    match sign {
        0xC000 => return Ok("NaN".into()),
        0xD000 => return Ok("Infinity".into()),
        0xF000 => return Ok("-Infinity".into()),
        0 | 0x4000 => {}
        _ => return Err("invalid numeric value".into()),
    }
    let digits: Vec<i16> = (0..ndigits).map(|i| rd(8 + 2 * i).map(|d| d as i16)).collect::<Result<_, _>>()?;
    let digit = |i: i32| if i >= 0 { digits.get(i as usize).copied().unwrap_or(0) } else { 0 };
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                write!(s, "{}", digit(i)).unwrap();
            } else {
                write!(s, "{:04}", digit(i)).unwrap();
            }
        }
    }
    if dscale > 0 {
        let mut frac = String::with_capacity(dscale + 4);
        let mut i = weight + 1;
        while frac.len() < dscale {
            write!(frac, "{:04}", digit(i)).unwrap();
            i += 1;
        }
        frac.truncate(dscale);
        s.push('.');
        s.push_str(&frac);
    }
    Ok(s)
}

/// A column value read from PostgreSQL.
struct PgOut(Scalar);

impl<'a> postgres::types::FromSql<'a> for PgOut {
    fn from_sql(ty: &postgres::types::Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        pg_decode(ty, raw).map(PgOut)
    }

    fn from_sql_null(_: &postgres::types::Type) -> Result<Self, BoxError> {
        Ok(PgOut(Scalar::Null))
    }

    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }
}

fn pg_decode(ty: &postgres::types::Type, raw: &[u8]) -> Result<Scalar, BoxError> {
    use postgres::types::{FromSql, Kind, Type};
    use std::fmt::Write;
    if let Kind::Domain(inner) = ty.kind() {
        return pg_decode(inner, raw);
    }
    if is_text_type(ty) || *ty == Type::JSON {
        return Ok(Scalar::Str(String::from_utf8_lossy(raw).into_owned()));
    }
    Ok(match *ty {
        Type::BOOL => Scalar::Bool(bool::from_sql(ty, raw)?),
        Type::INT2 => Scalar::Int(i16::from_sql(ty, raw)? as i64),
        Type::INT4 => Scalar::Int(i32::from_sql(ty, raw)? as i64),
        Type::INT8 => Scalar::Int(i64::from_sql(ty, raw)?),
        Type::OID => Scalar::Int(u32::from_sql(ty, raw)? as i64),
        Type::FLOAT4 => Scalar::Float(f32::from_sql(ty, raw)? as f64),
        Type::FLOAT8 => Scalar::Float(f64::from_sql(ty, raw)?),
        Type::NUMERIC => Scalar::Str(numeric_decode(raw)?),
        Type::JSONB => match raw.split_first() {
            Some((1, rest)) => Scalar::Str(String::from_utf8_lossy(rest).into_owned()),
            _ => return Err("unsupported jsonb version".into()),
        },
        Type::UUID if raw.len() == 16 => {
            let mut s = String::with_capacity(36);
            for (i, b) in raw.iter().enumerate() {
                if matches!(i, 4 | 6 | 8 | 10) {
                    s.push('-');
                }
                write!(s, "{b:02x}").unwrap();
            }
            Scalar::Str(s)
        }
        Type::BYTEA => {
            let mut s = String::with_capacity(2 + raw.len() * 2);
            s.push_str("\\x");
            for b in raw {
                write!(s, "{b:02x}").unwrap();
            }
            Scalar::Str(s)
        }
        Type::DATE => Scalar::Str(chrono::NaiveDate::from_sql(ty, raw)?.format("%Y-%m-%d").to_string()),
        Type::TIME => Scalar::Str(chrono::NaiveTime::from_sql(ty, raw)?.format("%H:%M:%S%.f").to_string()),
        Type::TIMESTAMP => {
            Scalar::Str(chrono::NaiveDateTime::from_sql(ty, raw)?.format("%Y-%m-%d %H:%M:%S%.f").to_string())
        }
        Type::TIMESTAMPTZ => Scalar::Str(
            chrono::DateTime::<chrono::Utc>::from_sql(ty, raw)?.format("%Y-%m-%d %H:%M:%S%.f+00:00").to_string(),
        ),
        Type::VOID => Scalar::Null,
        _ => {
            return Err(
                format!("PHV cannot read values of type {ty}; cast the column in the SQL, e.g. column::text").into()
            )
        }
    })
}

// ----- MySQL -----------------------------------------------------------------

struct MyConn {
    conn: mysql::Conn,
    /// The `max_execution_time` last set on the session, in milliseconds.
    timeout_ms: Option<u64>,
    /// For the side connection that stops a statement at the deadline.
    opts: Arc<mysql::Opts>,
    /// The jobs the script's open transaction added (`job_add_in_transaction`).
    jobs_tx: Option<JobsTx>,
    /// Stops the statement of an operation of PHV's own stores at its
    /// deadline (see `my_store_limits`).
    store_kill: Option<KillAt>,
    /// Set when a statement of a store failed other than with the server's
    /// own error (a network error, a read that timed out): the connection may
    /// be out of step and is not reused (found by the 1.1.1 audit: resetting
    /// it waited for the read timeout again).
    failed: Arc<std::sync::atomic::AtomicBool>,
}

impl MyConn {
    /// Notes the errors of a store's statements (see `failed`).
    fn watch(&self) -> MyWatch {
        MyWatch(self.failed.clone())
    }
}

/// Marks a MySQL connection as not to be reused after an error other than the
/// server's own (see `MyConn::failed`).
struct MyWatch(Arc<std::sync::atomic::AtomicBool>);

impl MyWatch {
    fn raw(&self, e: mysql::Error) -> mysql::Error {
        if !matches!(e, mysql::Error::MySqlError(_)) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        e
    }

    fn msg(&self, e: mysql::Error) -> String {
        my_message(&self.raw(e))
    }
}

impl Driver for MyConn {
    fn reset(&mut self) -> bool {
        use mysql::prelude::Queryable;
        self.timeout_ms = None;
        self.jobs_tx = None;
        // A KILL QUERY sent (or being sent) at the deadline could stop the
        // next statement on the connection.
        let killed = self.store_kill.take().is_some_and(KillAt::disarm);
        if killed || self.failed.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        // Rolls back an open transaction and clears session variables and temporary tables.
        if self.conn.reset().is_err() {
            return false;
        }
        // COM_RESET_CONNECTION keeps the active role and the current database.
        if self.conn.query_drop("SET ROLE DEFAULT").is_err() {
            return false;
        }
        match self.opts.get_db_name() {
            Some(db) => self.conn.select_db(db).is_ok(),
            // Without a database in the URL there is none to go back to: a script that
            // chose one leaves a connection that is not reused.
            None => matches!(self.conn.query_first::<Option<String>, _>("SELECT DATABASE()"), Ok(Some(None))),
        }
    }

    fn check(&mut self) -> bool {
        self.conn.ping().is_ok()
    }
}

/// MySQL connects still running (see `netio::with_deadline_in`).
static MY_CONNECTING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn my_connect(
    url: &str,
    spec: &DatabaseSpec,
    timeout: Duration,
    memory: Option<usize>,
    time_limit: Option<Duration>,
) -> Result<MyConn, String> {
    // The parser's own message could quote the URL, which may hold a password.
    let opts = mysql::Opts::from_url(url).map_err(|_| "the URL is not a valid MySQL URL".to_string())?;
    let local = opts.get_socket().is_some() || is_local_host(&opts.get_ip_or_hostname());
    // `require_ssl` in the URL is kept even when phv.toml would allow plain connections.
    let tls = opts.get_ssl_opts().is_some() || spec.tls.unwrap_or(!local);
    let ssl = match (tls, opts.get_ssl_opts().cloned()) {
        (false, _) => None,
        (true, Some(o)) => Some(o),
        (true, None) => Some(mysql::SslOpts::default()),
    };
    // A server that stops answering is given up on a little after the time limit
    // (statements are stopped at the deadline with KILL QUERY before that).
    let io_timeout = time_limit.map_or(Duration::from_secs(300), |t| t + Duration::from_secs(10));
    // No packet (one row, mostly) larger than half the script's memory.
    let packet = memory.unwrap_or(128 << 20) / 2;
    let builder = mysql::OptsBuilder::from_opts(opts)
        .ssl_opts(ssl)
        .tcp_connect_timeout(Some(timeout))
        .read_timeout(Some(io_timeout))
        .write_timeout(Some(io_timeout))
        .max_allowed_packet(Some(packet));
    let opts = Arc::new(mysql::Opts::from(builder.clone()));
    // The driver looks the host up itself, with no deadline of its own, and
    // waits for a server's greeting as long as for any answer.
    let conn =
        crate::netio::with_deadline_in(&MY_CONNECTING, Instant::now() + timeout, move || mysql::Conn::new(builder))?
            .map_err(|e| my_message(&e))?;
    Ok(MyConn { conn, timeout_ms: None, opts, jobs_tx: None, store_kill: None, failed: Default::default() })
}

fn my_message(e: &mysql::Error) -> String {
    match e {
        mysql::Error::MySqlError(m) => m.message.clone(),
        // Network, TLS and connection errors are described without the server's address.
        mysql::Error::IoError(io) => crate::netio::describe(io),
        mysql::Error::TlsError(_) => "the TLS connection failed".into(),
        mysql::Error::CodecError(c) => match network_cause(c) {
            Some(m) => m,
            None => "the server sent an invalid packet (larger than the memory left to the script?)".into(),
        },
        mysql::Error::DriverError(mysql::DriverError::CouldNotConnect(_)) => "cannot connect to the server".into(),
        e => e.to_string(),
    }
}

/// `KILL QUERY`s being sent at once on the pool's threads (each on a side
/// connection of its own); more start a thread of their own, as before.
pub(crate) const MAX_KILLING: usize = 32;
static KILLING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A statement to stop at its deadline: the connection's options and id.
struct Watched {
    opts: Arc<mysql::Opts>,
    id: u32,
}

/// The statements being watched, by deadline (and a number, to tell apart
/// statements with the same deadline), and the thread that watches them.
struct Watchdog {
    due: Mutex<std::collections::BTreeMap<(Instant, u64), Watched>>,
    /// Woken when a statement with an earlier deadline than all others comes.
    sooner: Condvar,
    next: std::sync::atomic::AtomicU64,
}

static WATCHDOG: LazyLock<Option<&'static Watchdog>> = LazyLock::new(|| {
    let dog: &'static Watchdog = Box::leak(Box::new(Watchdog {
        due: Mutex::new(Default::default()),
        sooner: Condvar::new(),
        next: Default::default(),
    }));
    std::thread::Builder::new().name("phv-mysql-watchdog".into()).spawn(move || dog.watch()).ok().map(|_| dog)
});

impl Watchdog {
    /// Sends `KILL QUERY` for each statement whose deadline has passed.
    fn watch(&self) {
        let mut due = lock(&self.due);
        loop {
            let now = Instant::now();
            let next = due.first_key_value().map(|(k, _)| *k);
            match next {
                None => due = self.sooner.wait(due).unwrap_or_else(|e| e.into_inner()),
                Some(key) if key.0 > now => {
                    due = self.sooner.wait_timeout(due, key.0 - now).unwrap_or_else(|e| e.into_inner()).0;
                }
                Some(key) => {
                    let w = due.remove(&key).expect("first key");
                    drop(due);
                    kill_query(w);
                    due = lock(&self.due);
                }
            }
        }
    }
}

/// Stops the statement on connection `w.id` from a second connection.
fn kill_query(w: Watched) {
    let send = move || {
        use mysql::prelude::Queryable;
        let side = mysql::OptsBuilder::from_opts((*w.opts).clone())
            .tcp_connect_timeout(Some(Duration::from_secs(5)))
            .read_timeout(Some(Duration::from_secs(5)))
            .write_timeout(Some(Duration::from_secs(5)));
        if let Ok(mut side) = mysql::Conn::new(side) {
            let _ = side.query_drop(format!("KILL QUERY {}", w.id));
        }
    };
    use std::sync::atomic::Ordering;
    if KILLING.fetch_add(1, Ordering::SeqCst) < MAX_KILLING {
        let job = Arc::new(Mutex::new(Some(send)));
        let in_pool = job.clone();
        let pooled = crate::workers::spawn(move || {
            if let Some(send) = lock(&in_pool).take() {
                send();
            }
            KILLING.fetch_sub(1, Ordering::SeqCst);
        });
        if pooled.is_ok() {
            return;
        }
        KILLING.fetch_sub(1, Ordering::SeqCst);
        if let Some(send) = lock(&job).take() {
            let _ = std::thread::Builder::new().spawn(send);
        }
        return;
    }
    KILLING.fetch_sub(1, Ordering::SeqCst);
    let _ = std::thread::Builder::new().spawn(send);
}

/// Stops the statement running on connection `id` with `KILL QUERY` (from a
/// second connection) when it is still running at `deadline`. Dropping the
/// guard before then cancels it. One thread watches the deadlines of every
/// statement in the process.
enum KillAt {
    None,
    Watched((Instant, u64)),
    /// Watched by a thread of its own (when the watchdog could not start).
    Alone(std::sync::mpsc::Sender<()>),
}

impl KillAt {
    fn arm(opts: &Arc<mysql::Opts>, id: u32, deadline: Option<Instant>) -> KillAt {
        let Some(deadline) = deadline else { return KillAt::None };
        let w = Watched { opts: opts.clone(), id };
        let Some(dog) = *WATCHDOG else {
            let (stop, wait) = std::sync::mpsc::channel::<()>();
            std::thread::spawn(move || {
                let left = deadline.saturating_duration_since(Instant::now());
                if wait.recv_timeout(left) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                    kill_query(w);
                }
            });
            return KillAt::Alone(stop);
        };
        let key = (deadline, dog.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let mut due = lock(&dog.due);
        let first = due.first_key_value().is_none_or(|(k, _)| key < *k);
        due.insert(key, w);
        drop(due);
        if first {
            dog.sooner.notify_one();
        }
        KillAt::Watched(key)
    }
}

impl KillAt {
    /// Cancels the kill once the statement has ended. True when it was sent
    /// (or is being sent) already: it could stop the next statement on the
    /// connection, which is then not used again (found by the 1.0.0 audit).
    fn disarm(mut self) -> bool {
        let fired = match &self {
            KillAt::Watched(key) => WATCHDOG.is_some_and(|dog| lock(&dog.due).remove(key).is_none()),
            KillAt::Alone(stop) => stop.send(()).is_err(),
            KillAt::None => false,
        };
        self = KillAt::None;
        drop(self);
        fired
    }
}

impl Drop for KillAt {
    fn drop(&mut self) {
        match self {
            KillAt::Watched(key) => {
                if let Some(dog) = *WATCHDOG {
                    lock(&dog.due).remove(key);
                }
            }
            KillAt::Alone(stop) => {
                let _ = stop.send(());
            }
            KillAt::None => {}
        }
    }
}

fn my_kill_at(vm: &Vm, c: &mut Pooled<MyConn>) -> KillAt {
    let id = c.get().conn.connection_id();
    // The statement ends on the client side just after the script's deadline.
    KillAt::arm(&c.get().opts, id, crate::deadline::after(vm, Duration::from_millis(TIMEOUT_MARGIN_MS)))
}

fn my_error(vm: &Vm, c: &mut Pooled<MyConn>, e: mysql::Error) -> VmError {
    match &e {
        mysql::Error::MySqlError(m) => {
            // 3024: stopped by the max_execution_time set from the script's time limit;
            // 1317: stopped by KILL QUERY at the deadline.
            if m.code == 3024 || m.code == 1317 {
                if let Err(fatal) = vm.check_limits() {
                    return fatal;
                }
            }
        }
        _ => c.broken = true,
    }
    vm.throw("DatabaseError", my_message(&e))
}

/// Lowers the server's `max_execution_time` to `left` when needed (see
/// `needs_new_timeout`).
fn my_set_timeout(c: &mut Pooled<MyConn>, left: Duration) -> Result<(), mysql::Error> {
    use mysql::prelude::Queryable;
    let ms = left.as_millis().min(u32::MAX as u128) as u64;
    if needs_new_timeout(c.get().timeout_ms, ms) {
        c.get().conn.query_drop(format!("SET SESSION max_execution_time = {}", ms + TIMEOUT_MARGIN_MS))?;
        c.get().timeout_ms = Some(ms);
    }
    Ok(())
}

fn my_limit_time(vm: &Vm, c: &mut Pooled<MyConn>) -> Result<(), VmError> {
    let Some(left) = remaining(vm)? else { return Ok(()) };
    my_set_timeout(c, left).map_err(|e| my_error(vm, c, e))
}

fn my_params(params: &[Value]) -> mysql::Params {
    if params.is_empty() {
        return mysql::Params::Empty;
    }
    mysql::Params::Positional(
        params
            .iter()
            .map(|v| match v {
                Value::Null => mysql::Value::NULL,
                Value::Bool(b) => mysql::Value::Int(*b as i64),
                Value::Int(i) => mysql::Value::Int(*i),
                Value::Float(f) => mysql::Value::Double(*f),
                other => mysql::Value::Bytes(other.to_display_string().into_bytes()),
            })
            .collect(),
    )
}

fn my_value(v: mysql::Value, col: &mysql::Column) -> Result<Scalar, String> {
    use mysql::consts::ColumnType;
    Ok(match v {
        mysql::Value::NULL => Scalar::Null,
        mysql::Value::Int(i) => Scalar::Int(i),
        mysql::Value::UInt(u) => {
            Scalar::Int(i64::try_from(u).map_err(|_| format!("column {}: {u} is too large for int", col.name_str()))?)
        }
        mysql::Value::Float(f) => Scalar::Float(f as f64),
        mysql::Value::Double(f) => Scalar::Float(f),
        mysql::Value::Bytes(b) => Scalar::Str(String::from_utf8_lossy(&b).into_owned()),
        mysql::Value::Date(y, mo, d, h, mi, s, us) => Scalar::Str(match col.column_type() {
            ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => format!("{y:04}-{mo:02}-{d:02}"),
            _ if us > 0 => format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}"),
            _ => format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"),
        }),
        mysql::Value::Time(neg, days, h, mi, s, us) => {
            let sign = if neg { "-" } else { "" };
            let hours = days as u64 * 24 + h as u64;
            Scalar::Str(if us > 0 {
                format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}")
            } else {
                format!("{sign}{hours:02}:{mi:02}:{s:02}")
            })
        }
    })
}

fn my_query(vm: &mut Vm, c: &mut Pooled<MyConn>, q: &Value) -> R {
    use mysql::prelude::Queryable;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::MySql);
    my_limit_time(vm, c)?;
    let kill = my_kill_at(vm, c);
    let mut out = ResultRows::new(Vec::new());
    let mut convert_error = None;
    let mut over = false;
    let result: Result<(), mysql::Error> = (|| {
        let mut result = c.get().conn.exec_iter(text, my_params(params))?;
        let Some(set) = result.iter() else { return Ok(()) };
        let columns: Vec<mysql::Column> = set.columns().as_ref().to_vec();
        out = ResultRows::new(columns.iter().map(|col| col.name_str().into_owned()));
        for row in set {
            let raw = row?.unwrap();
            let mut values = Vec::with_capacity(columns.len());
            for (v, col) in raw.into_iter().zip(&columns) {
                match my_value(v, col) {
                    Ok(v) => values.push(v.into_value()),
                    Err(e) => {
                        convert_error = Some(e);
                        return Ok(());
                    }
                }
            }
            if out.push(vm, values).is_err() {
                over = true;
                return Ok(());
            }
        }
        Ok(())
    })();
    if kill.disarm() {
        c.broken = true;
    }
    result.map_err(|e| my_error(vm, c, e))?;
    if let Some(e) = convert_error {
        // The rest of the result was not read: the connection is out of step.
        c.broken = true;
        return Err(vm.throw("DatabaseError", e));
    }
    if over {
        c.broken = true;
        return Err(out_of_memory(vm));
    }
    Ok(out.into_value())
}

fn my_execute(vm: &mut Vm, c: &mut Pooled<MyConn>, q: &Value) -> R {
    use mysql::prelude::Queryable;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::MySql);
    my_limit_time(vm, c)?;
    let kill = my_kill_at(vm, c);
    let mut r = c.get().conn.exec_drop(&text, my_params(params));
    // Statements such as BEGIN cannot be prepared (error 1295). Without
    // parameters they run as plain text, but only when the text holds no `;`,
    // since the text protocol would run a second statement after it.
    let unsupported = matches!(&r, Err(mysql::Error::MySqlError(m)) if m.code == 1295);
    if unsupported && params.is_empty() && !text.contains(';') {
        r = c.get().conn.query_drop(&text);
    }
    if kill.disarm() {
        c.broken = true;
    }
    r.map_err(|e| my_error(vm, c, e))?;
    Ok(Value::Int(c.get().conn.affected_rows().try_into().unwrap_or(i64::MAX)))
}

// ----- Statements run in the background (queryAsync) --------------------------

/// A value read from a row, before it becomes a PHV value: rows read in the
/// background cross threads, which values cannot.
pub(crate) enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl Scalar {
    fn into_value(self) -> Value {
        match self {
            Scalar::Null => Value::Null,
            Scalar::Bool(b) => Value::Bool(b),
            Scalar::Int(i) => Value::Int(i),
            Scalar::Float(f) => Value::Float(f),
            Scalar::Str(t) => Value::str(t),
        }
    }

    /// Roughly the memory the value takes once it is a PHV value.
    fn size(&self) -> usize {
        match self {
            Scalar::Str(t) => 32 + t.len(),
            _ => 16,
        }
    }
}

/// What a statement run in the background returned.
pub(crate) enum Detached {
    Rows(Vec<String>, Vec<Vec<Scalar>>),
    Count(u64),
}

impl Detached {
    /// The bytes it holds on the heap.
    pub(crate) fn heap_size(&self) -> usize {
        match self {
            Detached::Count(_) => 0,
            Detached::Rows(names, rows) => {
                let cell = std::mem::size_of::<Scalar>();
                names.iter().map(|n| n.capacity() + 24).sum::<usize>()
                    + rows
                        .iter()
                        .map(|r| {
                            24 + r.capacity() * cell
                                + r.iter().map(|c| if let Scalar::Str(s) = c { s.capacity() } else { 0 }).sum::<usize>()
                        })
                        .sum::<usize>()
            }
        }
    }
}

/// The rows (as maps) or the number of changed rows.
pub(crate) fn detached_value(d: Detached) -> Value {
    match d {
        Detached::Count(n) => Value::Int(n.try_into().unwrap_or(i64::MAX)),
        Detached::Rows(names, rows) => {
            let names: Vec<Rc<String>> = names.into_iter().map(Rc::new).collect();
            let rows = rows
                .into_iter()
                .map(|r| {
                    let m: IndexMap<Key, Value> =
                        names.iter().zip(r).map(|(n, v)| (Key::Str(n.clone()), v.into_value())).collect();
                    Value::map(m)
                })
                .collect();
            Value::list(rows)
        }
    }
}

/// How long background work may take without a script time limit.
const DETACHED_WAIT: Duration = Duration::from_secs(300);

/// What a statement read in the background may take: a sixteenth of the
/// memory left to the script (other work may run at the same time).
fn detached_budget(vm: &Vm) -> usize {
    (vm.memory_left() / 16).max(64 << 10)
}

const OVER_BUDGET: &str = "the result is larger than the memory a background statement may use";

/// `__db_start(handle, query, execute)`: runs a statement on a connection of
/// its own, in the background (`queryAsync`, `executeAsync`). It runs outside
/// the script's transaction.
pub fn start_query(vm: &mut Vm, a: &[Value]) -> R {
    let name = handle_name(vm, &a[0])?;
    let (spec, url) = spec_of(vm, &name)?;
    let (text, params) = parts(&a[1]);
    let execute = matches!(a[2], Value::Bool(true));
    let until = crate::deadline::until(vm, DETACHED_WAIT);
    let budget = detached_budget(vm);
    let fail = format!("database \"{name}\": ");
    let pool = Some((name.clone(), spec.pool_size));
    match kind_of(&url) {
        Kind::Postgres => {
            let text = render_sql(text, Dialect::Postgres);
            let params: Vec<PgParam> = params.iter().map(PgParam::new).collect();
            crate::asyncio::start(vm, until, pool, move || {
                let r = pg_detached(&spec, &url, &text, &params, execute, until, budget);
                crate::asyncio::Outcome::Db(r.map_err(|e| format!("{fail}{e}")))
            })
        }
        Kind::MySql => {
            let text = render_sql(text, Dialect::MySql);
            let params = my_params(params);
            crate::asyncio::start(vm, until, pool, move || {
                let r = my_detached(&spec, &url, &text, params, execute, until, budget);
                crate::asyncio::Outcome::Db(r.map_err(|e| format!("{fail}{e}")))
            })
        }
        Kind::Redis => Err(wrong_kind(vm, &name, true)),
    }
}

/// `__redis_start(handle, command, args)`: one Redis command in the background.
pub fn start_redis(vm: &mut Vm, a: &[Value]) -> R {
    let name = handle_name(vm, &a[0])?;
    let (spec, _) = spec_of(vm, &name)?;
    let command = s(&a[1]).to_string();
    let args: Vec<String> = list(&a[2]).iter().map(|v| s(v).to_string()).collect();
    let allowed = command != "HGETALL"
        && REDIS_COMMANDS.iter().any(|&(n, lo, hi)| n == command && (lo..=hi).contains(&args.len()));
    if !allowed {
        return Err(VmError::Fatal(format!("__redis_start: unexpected command {command}")));
    }
    if let Some(key) = args.first().filter(|k| k.starts_with(PHV_KEYS)) {
        return Err(vm.throw("ValueError", phv_key_error(key)));
    }
    let until = crate::deadline::until(vm, MAX_WAIT);
    let budget = detached_budget(vm);
    let fail = format!("database \"{name}\": ");
    let pool = Some((name.clone(), spec.pool_size));
    crate::asyncio::start(vm, until, pool, move || {
        let r = (|| {
            let mut c = redis_conn(&spec, PoolUse::Scripts, until)?;
            let mut argv: Vec<&[u8]> = vec![command.as_bytes()];
            argv.extend(args.iter().map(|a| a.as_bytes()));
            match c.get().command(&argv, until, budget) {
                Ok(v) => Ok(v),
                Err(RedisError::Server(m)) => Err(m),
                Err(RedisError::TooLarge) => {
                    c.broken = true;
                    Err(OVER_BUDGET.to_string())
                }
                Err(RedisError::Broken(m)) => {
                    c.broken = true;
                    Err(m)
                }
            }
        })();
        crate::asyncio::Outcome::Redis(r.map_err(|e| format!("{fail}{e}")))
    })
}

/// The `[databases]` name of the connection behind handle `h`.
fn handle_name(vm: &Vm, h: &Value) -> Result<String, VmError> {
    vm.resources
        .get(int(h) as usize)
        .and_then(|r| r.as_ref())
        .and_then(|b| b.downcast_ref::<NetDb>())
        .map(|d| d.name.clone())
        .ok_or_else(|| vm.throw("DatabaseError", "the database connection is closed"))
}

fn spec_of(vm: &Vm, name: &str) -> Result<(DatabaseSpec, String), VmError> {
    let spec = vm.host.databases.get(name).cloned();
    let Some(spec) = spec else { return Err(vm.throw("DatabaseError", format!("no database named \"{name}\""))) };
    let Some(url) = spec.url.clone() else {
        let var = spec.url_env.as_deref().unwrap_or("?");
        return Err(
            vm.throw("DatabaseError", format!("database \"{name}\": the environment variable {var} is not set"))
        );
    };
    tls_init();
    Ok((spec, url))
}

/// Who a pooled connection is for: scripts (and their background work) share
/// one pool per database; the session store and the shared rate limits each
/// have a small pool of their own, so scripts cannot starve them.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PoolUse {
    Scripts,
    Sessions,
    RateLimits,
    Publish,
    Jobs,
    /// Jobs added near the cap, which wait for the store's lock: apart, so
    /// they never hold the connections of `Jobs` while they wait.
    JobLock,
    /// Jobs added with a key by scripts: each try is short (a key an open
    /// transaction holds is looked at again later, on a connection taken
    /// again), and on connections apart from those of the scheduler and of
    /// the adds near the cap (found by the 1.2 audit: waits on held keys
    /// took every connection those used).
    JobKey,
    /// The jobs of `[[schedule]]`: a pool of their own, which scripts never use.
    Schedule,
}

impl PoolUse {
    /// What the pool is for, in the error of a pool whose connections are all in use.
    fn what(self) -> &'static str {
        match self {
            PoolUse::Scripts => "scripts",
            PoolUse::Sessions => "sessions",
            PoolUse::RateLimits => "rate limits",
            PoolUse::Publish => "publishing",
            PoolUse::Jobs | PoolUse::JobLock | PoolUse::JobKey | PoolUse::Schedule => "the job store",
        }
    }
}

fn pool_key(spec: &DatabaseSpec, url: &str, use_: PoolUse) -> (String, usize) {
    let key = format!("{:?}|{}|{}", spec.tls, spec.pool_size, url);
    match use_ {
        PoolUse::Scripts => (key, spec.pool_size),
        PoolUse::Sessions => (format!("sessions|{key}"), spec.pool_size.min(4)),
        PoolUse::RateLimits => (format!("ratelimit|{key}"), spec.pool_size.min(4)),
        PoolUse::Publish => (format!("publish|{key}"), 1),
        PoolUse::Jobs => (format!("jobs|{key}"), spec.pool_size.min(4)),
        PoolUse::JobLock => (format!("joblock|{key}"), spec.pool_size.min(4)),
        PoolUse::JobKey => (format!("jobkey|{key}"), spec.pool_size.min(4)),
        PoolUse::Schedule => (format!("schedule|{key}"), spec.pool_size.min(2)),
    }
}

/// A connection from the pool of `spec` for `use_`, waiting until `until`
/// for a free one; a new connection gets the time left (at most
/// `CONNECT_TIMEOUT`, at least 100 ms).
fn pooled<C: Driver>(
    registry: &Registry<C>,
    spec: &DatabaseSpec,
    url: &str,
    use_: PoolUse,
    until: Instant,
    connect: impl FnOnce(Duration) -> Result<C, String>,
) -> Result<Pooled<C>, AcquireError> {
    let (key, size) = pool_key(spec, url, use_);
    let timeout = until.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(100), CONNECT_TIMEOUT);
    pool_for(registry, &key, size).acquire(until, || connect(timeout))
}

/// `pooled`, for work outside a script (background statements, sessions).
fn detached_conn<C: Driver>(
    registry: &Registry<C>,
    spec: &DatabaseSpec,
    url: &str,
    use_: PoolUse,
    until: Instant,
    connect: impl FnOnce(Duration) -> Result<C, String>,
) -> Result<Pooled<C>, String> {
    match pooled(registry, spec, url, use_, until, connect) {
        Ok(c) => Ok(c),
        // The size of the pool that was full (found by the 1.2 audit: the
        // error named pool_size for the smaller pools of PHV's own stores).
        Err(AcquireError::Busy) => Err(match pool_key(spec, url, use_).1 {
            n if use_ == PoolUse::Scripts => format!("all {n} connections are in use (raise pool_size?)"),
            n => format!("all {n} connections for {} are in use", use_.what()),
        }),
        Err(AcquireError::Connect(e)) => Err(format!("cannot connect: {e}")),
    }
}

fn pg_detached(
    spec: &DatabaseSpec,
    url: &str,
    text: &str,
    params: &[PgParam],
    execute: bool,
    until: Instant,
    budget: usize,
) -> Result<Detached, String> {
    use postgres::types::ToSql;
    let mut c = detached_conn(&PG_POOLS, spec, url, PoolUse::Scripts, until, |t| pg_connect(url, spec, t))?;
    let set = pg_limits(&mut c, Some(until), budget);
    let prelude = set.as_ref().map(|s| s.0.as_str());
    let kind = pg_text(text);
    let mut over = false;
    let r = if execute {
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        c.get().with_statement(text, kind == PgText::Plain, false, prelude, &mut |client, stmt, prelude| {
            client.run(prelude, client.client().execute(stmt, &refs)).map(Detached::Count)
        })
    } else {
        c.get().with_statement(text, kind == PgText::Plain, false, prelude, &mut |client, stmt, prelude| {
            let names: Vec<String> = stmt.columns().iter().map(|col| col.name().to_string()).collect();
            let mut rows = Vec::new();
            let mut used = 0usize;
            client.query_each(prelude, stmt, params.iter().map(|p| p as &dyn ToSql), |row| {
                let mut r = Vec::with_capacity(names.len());
                for i in 0..names.len() {
                    let PgOut(v) = row.try_get(i)?;
                    used += v.size();
                    r.push(v);
                }
                rows.push(r);
                if used > budget {
                    over = true;
                    return Ok(false);
                }
                Ok(true)
            })?;
            Ok(Detached::Rows(names, rows))
        })
    };
    if kind == PgText::Other {
        c.get().forget_statements();
    }
    let r = r.map_err(|e| match pg_failure(&mut c, &e) {
        PgFailure::Budget => OVER_BUDGET.to_string(),
        PgFailure::Deadline | PgFailure::Canceled => "the statement ran out of time".to_string(),
        PgFailure::Other => pg_message(&e),
    })?;
    if let Some((_, ms)) = set {
        c.get().timeout_ms = Some(ms);
    }
    if over {
        c.broken = true;
        return Err(OVER_BUDGET.into());
    }
    Ok(r)
}

fn my_detached(
    spec: &DatabaseSpec,
    url: &str,
    text: &str,
    params: mysql::Params,
    execute: bool,
    until: Instant,
    budget: usize,
) -> Result<Detached, String> {
    use mysql::prelude::Queryable;
    let left = until.saturating_duration_since(Instant::now());
    let mut c = detached_conn(&MY_POOLS, spec, url, PoolUse::Scripts, until, |t| {
        my_connect(url, spec, t, Some(budget * 2), Some(left))
    })?;
    let error = |c: &mut Pooled<MyConn>, e: mysql::Error| -> String {
        match &e {
            mysql::Error::MySqlError(m) if m.code == 3024 || m.code == 1317 => {
                return "the statement ran out of time".to_string();
            }
            mysql::Error::MySqlError(_) => {}
            _ => c.broken = true,
        }
        my_message(&e)
    };
    my_set_timeout(&mut c, left).map_err(|e| error(&mut c, e))?;
    let id = c.get().conn.connection_id();
    let kill = KillAt::arm(&c.get().opts, id, Some(until + Duration::from_millis(TIMEOUT_MARGIN_MS)));
    if execute {
        let r = c.get().conn.exec_drop(text, params);
        if kill.disarm() {
            c.broken = true;
        }
        r.map_err(|e| error(&mut c, e))?;
        return Ok(Detached::Count(c.get().conn.affected_rows()));
    }
    let mut names = Vec::new();
    let mut rows = Vec::new();
    let mut used = 0usize;
    let mut problem: Option<String> = None;
    let result: Result<(), mysql::Error> = (|| {
        let mut result = c.get().conn.exec_iter(text, params)?;
        let Some(set) = result.iter() else { return Ok(()) };
        let columns: Vec<mysql::Column> = set.columns().as_ref().to_vec();
        names = columns.iter().map(|col| col.name_str().into_owned()).collect();
        for row in set {
            let values = row?.unwrap();
            let mut r = Vec::with_capacity(columns.len());
            for (v, col) in values.into_iter().zip(&columns) {
                match my_value(v, col) {
                    Ok(v) => {
                        used += v.size();
                        r.push(v);
                    }
                    Err(e) => {
                        problem = Some(e);
                        return Ok(());
                    }
                }
            }
            rows.push(r);
            if used > budget {
                problem = Some(OVER_BUDGET.into());
                return Ok(());
            }
        }
        Ok(())
    })();
    if kill.disarm() {
        c.broken = true;
    }
    result.map_err(|e| error(&mut c, e))?;
    if let Some(e) = problem {
        // The rest of the result was not read: the connection is out of step.
        c.broken = true;
        return Err(e);
    }
    Ok(Detached::Rows(names, rows))
}

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

enum SessionConn {
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
fn store_conn(
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
fn pg_no_table(e: &postgres::Error) -> bool {
    e.code() == Some(&postgres::error::SqlState::UNDEFINED_TABLE)
}

fn my_no_table(e: &mysql::Error) -> bool {
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
struct LuaScript {
    text: std::borrow::Cow<'static, str>,
    sha: std::sync::OnceLock<String>,
}

impl LuaScript {
    const fn new(text: &'static str) -> LuaScript {
        LuaScript { text: std::borrow::Cow::Borrowed(text), sha: std::sync::OnceLock::new() }
    }

    fn owned(text: String) -> LuaScript {
        LuaScript { text: std::borrow::Cow::Owned(text), sha: std::sync::OnceLock::new() }
    }

    /// Runs the script with `args`: the number of keys, the keys, the other arguments.
    fn run(&self, c: &mut RedisConn, args: &[&[u8]], until: Instant, budget: usize) -> Result<Reply, RedisError> {
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

fn redis_run(c: &mut Pooled<RedisConn>, args: &[&[u8]], until: Instant) -> Result<Reply, String> {
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

/// The session in table `t` under `key`, if any.
fn pg_session_row(conn: &mut PgConn, t: &str, key: &str) -> Result<Option<SessionRow>, postgres::Error> {
    let rows = conn.query_kept(&format!("SELECT data, created, last, returned FROM {t} WHERE id = $1"), &[&key])?;
    let Some(r) = rows.first() else { return Ok(None) };
    Ok(Some(SessionRow {
        data: r.try_get(0)?,
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
            let text = String::from_utf8_lossy(&b);
            let damaged = || "a stored session is damaged".to_string();
            let (head, data) = text.split_once('\n').ok_or_else(damaged)?;
            let mut nums = head.split(' ').map(|n| n.parse::<u64>().ok());
            let (Some(Some(created)), Some(Some(last)), Some(Some(returned))) = (nums.next(), nums.next(), nums.next())
            else {
                return Err(damaged());
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
            let left = (row.created + limits.max_lifetime.as_secs()).saturating_sub(now);
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

// ----- Redis -----------------------------------------------------------------

use crate::redisclient::RedisConn;

impl Driver for RedisConn {
    fn reset(&mut self) -> bool {
        // Scripts can only run the data commands below, which leave no session state.
        true
    }

    fn check(&mut self) -> bool {
        self.command(&[b"PING"], Instant::now() + RESET_TIMEOUT, 1024).is_ok()
    }
}

fn redis_connect(url: &str, spec: &DatabaseSpec, timeout: Duration) -> Result<RedisConn, String> {
    RedisConn::connect(url, spec.tls, Instant::now() + timeout)
}

// ----- Background jobs in a database ([jobs] store = "db:<name>") ------------
//
// PostgreSQL and MySQL keep jobs in a table `phv_jobs`; Redis under `phv:jobs:`.
// A job is taken with a lease (a random token and a time): finishing it needs
// the token, so a job whose lease ran out and was taken by another process is
// not finished twice by the first. States: 0 waiting, 1 running, 2 given up,
// 3 done (kept a day when it has a key, so the key is not used again).

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
const JOB_COUNT_FRESH_MS: i64 = 1000;

/// How long one operation on the job store may take.
const JOB_WAIT: Duration = Duration::from_secs(10);
/// Jobs that ended with a key are kept this long (so the key is not used
/// again), and at most `JOB_MAX_KEPT` of them (the latest).
const JOB_KEEP_DONE_MS: i64 = 24 * 3600 * 1000;
pub const JOB_MAX_KEPT: usize = 100_000;
/// Jobs given up are kept this long, and at most `JOB_MAX_DEAD` of them.
const JOB_KEEP_DEAD_MS: i64 = 7 * 24 * 3600 * 1000;
pub const JOB_MAX_DEAD: usize = 10_000;
/// Rows one statement of a sweep removes at most (found by the 1.1.1 audit:
/// MySQL removed them in one statement however many there were).
const JOB_SWEEP_BATCH: i64 = 10_000;

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
struct JobLoad {
    n: i64,
    bytes: i64,
}

impl JobLoad {
    fn plus(self, other: JobLoad) -> JobLoad {
        JobLoad { n: self.n + other.n, bytes: self.bytes + other.bytes }
    }

    fn one(size: usize) -> JobLoad {
        JobLoad { n: 1, bytes: size as i64 }
    }

    /// Whether a job of `size` bytes no longer fits.
    fn full(&self, cap: JobCap, size: usize) -> bool {
        self.n >= cap.jobs as i64 || self.bytes + size as i64 > cap.bytes as i64
    }

    /// Whether it is so far below both caps that a count a second old says the same.
    fn far(&self, cap: JobCap) -> bool {
        self.n + JOB_COUNT_MARGIN < cap.jobs as i64 && self.bytes + JOB_BYTES_MARGIN < cap.bytes as i64
    }
}

/// Databases whose jobs tables this process has made ready (by URL and table).
static JOB_TABLES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Jobs tables made before 1.1.1 whose `size` column could not be added yet
/// (another transaction held the table), and when that was last tried: the
/// bytes of their jobs are not counted until it is added. Adding it waits
/// for the table this long at most (every statement on the table waits
/// behind it meanwhile), and is tried again after `JOB_SIZE_RETRY`.
static JOB_UNSIZED: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);
const JOB_ALTER_WAIT_MS: u64 = 200;
const JOB_SIZE_RETRY: Duration = Duration::from_secs(60);

/// Whether table `t` has the `size` column (see `JOB_UNSIZED`).
fn job_sized(t: &str) -> bool {
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
fn jobs_table(ns: Option<&str>) -> String {
    match ns {
        Some(n) if n.bytes().all(|b| b.is_ascii_hexdigit()) => format!("phv_jobs_{n}"),
        _ => "phv_jobs".to_string(),
    }
}

/// The Redis prefix of a site's jobs (see `jobs_table`).
fn jobs_prefix(ns: Option<&str>) -> String {
    match ns {
        Some(n) if n.bytes().all(|b| b.is_ascii_hexdigit()) => format!("phv:jobs:{n}:"),
        _ => "phv:jobs:".to_string(),
    }
}

fn job_conn(spec: &DatabaseSpec, ns: Option<&str>) -> Result<(SessionConn, Instant), String> {
    job_conn_before(spec, ns, None)
}

/// `job_conn`, waiting until `deadline` at most (a script's time left).
fn job_conn_before(
    spec: &DatabaseSpec,
    ns: Option<&str>,
    deadline: Option<Instant>,
) -> Result<(SessionConn, Instant), String> {
    job_conn_for(spec, ns, deadline, PoolUse::Jobs)
}

/// A connection of the job store for one operation (see `store_conn`), with
/// the store's tables made ready once per process.
fn job_conn_for(
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

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

fn job_redis_result(c: &mut Pooled<RedisConn>, r: Result<Reply, RedisError>) -> Result<Reply, String> {
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
fn job_redis(
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
static REDIS_JOB_ADD: LuaScript = LuaScript::new(
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
static REDIS_JOB_TAKE: LuaScript = LuaScript::new(
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
static REDIS_JOB_END: LuaScript = LuaScript::new(
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
static REDIS_JOB_SWEEP: LuaScript = LuaScript::new(
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
static REDIS_JOB_LIST: LuaScript = LuaScript::new(
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
static REDIS_JOB_REQUEUE: LuaScript = LuaScript::new(
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

/// Adds a job; `Ok(None)` when its key is taken, an error when the store
/// holds `cap` already.
///
/// Far below the caps the job is added at once, with a count of the store at
/// most a second old (one per process); near them it is added under the
/// store's lock (`job_add_locked`), and counted there only (found by the 1.1.1
/// audit: near the cap each job counted the table twice).
#[cfg(test)]
fn job_add(
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
    let (mut conn, until) = job_conn_for(spec, ns, Some(until), use_)?;
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
    let (mut conn, until) = job_conn_for(spec, ns, deadline, use_)?;
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

fn job_count_key(spec: &DatabaseSpec, table: &str) -> String {
    format!("{}|{table}", spec.url.as_deref().unwrap_or_default())
}

/// After a transaction that added jobs ended: its jobs are counted again.
fn forget_job_count(key: &str) {
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
fn pg_job_load(conn: &mut PgConn, t: &str, in_transaction: bool) -> Result<JobLoad, String> {
    pg_job_load_raw(conn, t, in_transaction).map_err(|e| pg_message(&e))
}

fn pg_job_load_raw(conn: &mut PgConn, t: &str, in_transaction: bool) -> Result<JobLoad, postgres::Error> {
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

fn my_job_load(conn: &mut MyConn, t: &str) -> Result<JobLoad, String> {
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
fn pg_job_lock(conn: &mut PgConn, t: &str, prelude: Option<&str>) -> Result<(), postgres::Error> {
    let key = job_lock_key(t);
    conn.with_statement("SELECT pg_advisory_xact_lock($1)", true, true, prelude, &mut |client, stmt, prelude| {
        client.run(prelude, client.client().execute(stmt, &[&key]))
    })
    .map(|_| ())
}

/// The table of the rows that transactions adding jobs lock (MySQL).
const MY_JOBS_LOCK_TABLE: &str = "CREATE TABLE IF NOT EXISTS phv_jobs_lock (t varchar(64) PRIMARY KEY)";

/// Takes the lock of table `t` until the transaction ends (MySQL): its row of
/// `phv_jobs_lock`, put back if someone removed it. A row lock is released at
/// commit or rollback, is seen by deadlock detection, and waits at most
/// `innodb_lock_wait_timeout`.
fn my_job_lock(conn: &mut mysql::Conn, t: &str) -> Result<(), mysql::Error> {
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
    use mysql::prelude::Queryable;
    let w = conn.watch();
    if let Some(u) = uniq {
        let probe = format!("SELECT id FROM {t} WHERE uniq = ? FOR UPDATE NOWAIT");
        match conn.conn.exec_first::<String, _, _>(probe, (u,)) {
            Ok(Some(_)) => return Ok(Tried::Done(None)),
            Ok(None) => {}
            // 3572: the row is locked (MySQL); 1205: the same in MariaDB.
            Err(mysql::Error::MySqlError(e)) if e.code == 3572 || e.code == 1205 => return Ok(Tried::Held),
            Err(e) => return Err(w.msg(e)),
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
struct JobsTx {
    held: JobLoad,
    added: JobLoad,
    /// The count to forget when the transaction ends (`forget_job_count`).
    count_key: String,
}

impl JobsTx {
    fn ended(self) {
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
    let r = match &mut *link {
        Link::Pg(c) => {
            if !(c.depth > 0 || c.get().in_own_transaction) {
                return None;
            }
            (|| {
                pg_run(vm, c, |c, prelude| pg_job_lock(c.get(), &t, prelude))?;
                if c.get().jobs_tx.is_none() {
                    let held = held()?;
                    c.get().jobs_tx = Some(JobsTx { held, added: JobLoad::default(), count_key });
                }
                if !c.get().jobs_tx.as_ref().is_some_and(fits) {
                    return Err(full());
                }
                let r = pg_run(vm, c, |c, prelude| pg_job_insert(c.get(), &t, true, job, run_at, uniq, now, prelude))?;
                if let (Some(()), Some(tx)) = (r, c.get().jobs_tx.as_mut()) {
                    tx.added = tx.added.plus(JobLoad::one(size));
                }
                Ok(r)
            })()
        }
        Link::My(c) if c.depth > 0 => (|| {
            my_limit_time(vm, c)?;
            let kill = my_kill_at(vm, c);
            let r = my_job_lock(&mut c.get().conn, &t);
            if kill.disarm() {
                c.broken = true;
            }
            r.map_err(|e| my_error(vm, c, e))?;
            if c.get().jobs_tx.is_none() {
                let held = held()?;
                c.get().jobs_tx = Some(JobsTx { held, added: JobLoad::default(), count_key });
            }
            if !c.get().jobs_tx.as_ref().is_some_and(fits) {
                return Err(full());
            }
            my_limit_time(vm, c)?;
            let kill = my_kill_at(vm, c);
            let r = my_job_insert_on(&mut c.get().conn, &t, job, run_at, uniq, now);
            if kill.disarm() {
                c.broken = true;
            }
            let r = r.map_err(|e| my_error(vm, c, e))?;
            if let (Some(()), Some(tx)) = (r, c.get().jobs_tx.as_mut()) {
                tx.added = tx.added.plus(JobLoad::one(size));
            }
            Ok(r)
        })(),
        _ => return None,
    };
    Some(r)
}

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
            let p = jobs_prefix(None);
            let mut n = 0;
            for set in ["due", "running", "dead"] {
                let key = format!("{p}{set}");
                let r = c.get().command(&[b"ZCARD", key.as_bytes()], until, 1024);
                match job_redis_result(c, r)? {
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

// ----- publish() between server processes (`[server] publish_store`) ----------

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

// ----- Shared rate limits ------------------------------------------------------

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
fn redis_pooled(spec: &DatabaseSpec, use_: PoolUse, until: Instant) -> Result<Pooled<RedisConn>, (String, bool)> {
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

fn redis_conn(spec: &DatabaseSpec, use_: PoolUse, until: Instant) -> Result<Pooled<RedisConn>, String> {
    redis_pooled(spec, use_, until).map_err(|(why, _)| why)
}

fn redis_failure(e: Option<crate::redisclient::RedisError>) -> String {
    match e {
        Some(crate::redisclient::RedisError::Server(m)) => m,
        Some(_) => "the server did not answer".into(),
        None => "the server sent an invalid reply".into(),
    }
}

/// Commands scripts may run, with their argument counts.
/// Keys PHV keeps in Redis itself (sessions, rate limits): scripts may not
/// read or write them, which would let a request forge a session.
const PHV_KEYS: &str = "phv:";

fn phv_key_error(key: &str) -> String {
    let shown: String = key.chars().take(40).collect();
    format!("Redis keys starting with `phv:` belong to PHV (sessions, rate limits, jobs): \"{shown}\"")
}

const REDIS_COMMANDS: &[(&str, usize, usize)] = &[
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
    let v = match c.get().command(&argv, until, budget) {
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
    let out = match (name, v) {
        // RESP2 answers HGETALL with a flat list of fields and values.
        ("HGETALL", Reply::Array(items)) => {
            let mut m = IndexMap::with_capacity(items.len() / 2);
            let mut it = items.into_iter().map(redis_value);
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                m.insert(Key::Str(Rc::new(k.to_display_string())), v);
            }
            Value::map(m)
        }
        (_, v) => redis_value(v),
    };
    vm.check_memory()?;
    Ok(out)
}

// ----- migrations (`phv db`) ---------------------------------------------------

/// A connection to a server database for `phv db`. Migrations are the
/// project's own SQL run from the command line, so no time or size limit applies.
pub struct MigrationConn(MigrationInner);

enum MigrationInner {
    Pg(Box<PgConn>),
    My(Box<MyConn>),
}

const MIGRATIONS_TABLE: &str = "CREATE TABLE IF NOT EXISTS phv_migrations (name varchar(255) PRIMARY KEY, \
                                checksum varchar(64) NOT NULL, applied_at varchar(40) NOT NULL)";

pub fn migration_conn(spec: &DatabaseSpec) -> Result<MigrationConn, String> {
    let Some(url) = spec.url.clone() else {
        return Err(format!("the environment variable {} is not set", spec.url_env.as_deref().unwrap_or("?")));
    };
    tls_init();
    let wait = Duration::from_secs(30);
    let mut conn = match kind_of(&url) {
        Kind::Postgres => {
            let c = pg_connect(&url, spec, wait)?;
            c.meter.set(None, usize::MAX);
            MigrationInner::Pg(Box::new(c))
        }
        Kind::MySql => MigrationInner::My(Box::new(my_connect(&url, spec, wait, None, None)?)),
        Kind::Redis => return Err("it is a Redis server; migrations are for SQL databases".into()),
    };
    match &mut conn {
        MigrationInner::Pg(c) => c.store_create(MIGRATIONS_TABLE).map_err(|e| pg_message(&e))?,
        MigrationInner::My(c) => {
            use mysql::prelude::Queryable;
            c.conn.query_drop(MIGRATIONS_TABLE).map_err(|e| my_message(&e))?
        }
    }
    Ok(MigrationConn(conn))
}

impl MigrationConn {
    /// The migrations applied so far: name and checksum.
    pub fn applied(&mut self) -> Result<Vec<(String, String)>, String> {
        match &mut self.0 {
            MigrationInner::Pg(c) => {
                let rows = c
                    .client
                    .query("SELECT name, checksum FROM phv_migrations ORDER BY name", &[])
                    .map_err(|e| pg_message(&e))?;
                let get = |e: postgres::Error| pg_message(&e);
                rows.iter().map(|r| Ok((r.try_get(0).map_err(get)?, r.try_get(1).map_err(get)?))).collect()
            }
            MigrationInner::My(c) => {
                use mysql::prelude::Queryable;
                c.conn.query("SELECT name, checksum FROM phv_migrations ORDER BY name").map_err(|e| my_message(&e))
            }
        }
    }

    /// Runs one migration and records it. On PostgreSQL both happen in one
    /// transaction; MySQL commits each schema change at once, so a failing
    /// migration may be left half applied there.
    pub fn apply(&mut self, name: &str, checksum: &str, sql: &str, at: &str) -> Result<(), String> {
        match &mut self.0 {
            MigrationInner::Pg(c) => {
                let client = &c.client;
                client.batch_execute("BEGIN").map_err(|e| pg_message(&e))?;
                let run = || -> Result<(), postgres::Error> {
                    client.batch_execute(sql)?;
                    client.execute(
                        "INSERT INTO phv_migrations (name, checksum, applied_at) VALUES ($1, $2, $3)",
                        &[&name, &checksum, &at],
                    )?;
                    client.batch_execute("COMMIT")
                };
                run().map_err(|e| {
                    let _ = client.batch_execute("ROLLBACK");
                    pg_message(&e)
                })
            }
            MigrationInner::My(c) => {
                use mysql::prelude::Queryable;
                c.conn.query_drop(sql).map_err(|e| my_message(&e))?;
                c.conn
                    .exec_drop(
                        "INSERT INTO phv_migrations (name, checksum, applied_at) VALUES (?, ?, ?)",
                        (name, checksum, at),
                    )
                    .map_err(|e| my_message(&e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(t: &str) -> String {
        let mut b = bytes::BytesMut::new();
        numeric_encode(t, &mut b).unwrap();
        numeric_decode(&b).unwrap()
    }

    #[test]
    fn numeric_round_trips() {
        for (input, want) in [
            ("0", "0"),
            ("1", "1"),
            ("-1", "-1"),
            ("12345678", "12345678"),
            ("10000", "10000"),
            ("0.5", "0.5"),
            ("-0.0001", "-0.0001"),
            ("123.4500", "123.4500"),
            ("0.000012", "0.000012"),
            ("99999999999999999999.99", "99999999999999999999.99"),
            ("+7", "7"),
            ("007.10", "7.10"),
            ("-0", "0"),
            ("NaN", "NaN"),
        ] {
            assert_eq!(roundtrip(input), want, "{input}");
        }
        let mut b = bytes::BytesMut::new();
        for bad in ["", ".", "1e5", "abc", "1.2.3", "--1"] {
            assert!(numeric_encode(bad, &mut b).is_err(), "{bad}");
        }
    }

    #[test]
    fn uuid_parses() {
        let u = uuid_parse("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(u[0], 0x55);
        assert_eq!(u[15], 0x00);
        assert!(uuid_parse("550e8400").is_none());
        assert!(uuid_parse("zz0e8400-e29b-41d4-a716-446655440000").is_none());
    }

    #[test]
    fn local_hosts() {
        for h in ["localhost", "LOCALHOST", "127.0.0.1", "::1", "[::1]"] {
            assert!(is_local_host(h), "{h}");
        }
        for h in ["db.example.com", "10.0.0.5", "localhost.example.com"] {
            assert!(!is_local_host(h), "{h}");
        }
    }

    #[test]
    fn statement_texts_are_told_apart() {
        for (text, kind) in [
            ("SELECT * FROM t", PgText::Plain),
            ("  select 1;", PgText::Plain),
            ("INSERT INTO t VALUES ($1)", PgText::Plain),
            ("WITH x AS (SELECT 1) SELECT * FROM x", PgText::Plain),
            ("SELECT * INTO t2 FROM t", PgText::Other),
            ("BEGIN", PgText::Begin),
            ("start transaction", PgText::Begin),
            ("COMMIT", PgText::End),
            ("rollback;", PgText::End),
            ("END WORK", PgText::End),
            ("ROLLBACK TO SAVEPOINT a", PgText::Other),
            ("COMMIT AND CHAIN", PgText::Other),
            ("ALTER TABLE t ADD COLUMN c int", PgText::Other),
            ("DEALLOCATE ALL", PgText::Other),
            ("/* c */ SELECT 1", PgText::Other),
            ("", PgText::Other),
        ] {
            assert_eq!(pg_text(text), kind, "{text}");
        }
    }

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

    /// A database job store holds at most `JobCap::bytes` of payloads, as the
    /// memory store does, and ending a job makes room again (found by the 1.1.1
    /// audit: only the jobs were counted, so a store could hold 6.4 GB).
    #[test]
    fn job_stores_cap_the_bytes_of_payloads() {
        let ns = crate::testing::namespace(0x30);
        let ns = Some(ns.as_str());
        for (var, spec) in test_servers() {
            let clear = || {
                while let Some(j) = job_take(&spec, ns, 60_000).unwrap() {
                    job_end(&spec, ns, &j, JobEnd::Done { keyed: false }).unwrap();
                }
                job_sweep(&spec, ns).unwrap();
            };
            clear();
            let cap = JobCap { jobs: 1000, bytes: 100_000 };
            let add = |size: usize| {
                let (id, payload) = (crate::random_hex(16), "x".repeat(size));
                job_add(&spec, ns, (&id, "big", &payload), now_ms(), None, cap, None)
            };
            for _ in 0..3 {
                assert_eq!(add(30_000), Ok(Some(())), "{var}");
            }
            let err = add(30_000).unwrap_err();
            assert!(err.starts_with("the job queue holds 1000 jobs or 0 MB"), "{var}: {err}");
            assert_eq!(add(1_000), Ok(Some(())), "{var}: a small job still fits");
            let j = job_take(&spec, ns, 60_000).unwrap().unwrap();
            assert!(job_end(&spec, ns, &j, JobEnd::Done { keyed: false }).unwrap());
            // Counts of the database are a second old at most.
            std::thread::sleep(Duration::from_millis(1100));
            assert_eq!(add(30_000), Ok(Some(())), "{var}: no room after a job ended");
            clear();
        }
    }

    /// A jobs table made before 1.1.1 gets its `size` column when first used;
    /// while another transaction holds the table, the store goes on without
    /// it (the bytes are not counted) instead of waiting, and adds it later.
    #[test]
    fn a_jobs_table_of_1_1_gets_its_size_column_later_when_held() {
        let ns = crate::testing::namespace(0x31);
        let ns = Some(ns.as_str());
        let t = jobs_table(ns);
        for (var, spec) in test_servers() {
            if var == "PHV_TEST_REDIS_URL" {
                continue;
            }
            let url = spec.url.clone().unwrap();
            let old = format!(
                "CREATE TABLE {t} (id varchar(32) PRIMARY KEY, name varchar(64) NOT NULL, payload text NOT NULL, \
                 run_at bigint NOT NULL, attempts integer NOT NULL, state smallint NOT NULL, lease varchar(32), \
                 lease_until bigint, uniq varchar(64) UNIQUE, error text, created bigint NOT NULL)"
            );
            let has_size = |spec: &DatabaseSpec| -> bool {
                let (mut conn, _) = job_conn(spec, ns).unwrap();
                let sql = format!(
                    "SELECT count(*) FROM information_schema.columns WHERE table_name = '{t}' \
                                   AND column_name = 'size'"
                );
                match &mut conn {
                    SessionConn::Pg(c) => c.get().client.query(&sql, &[]).unwrap()[0].get::<_, i64>(0) > 0,
                    SessionConn::My(c) => {
                        use mysql::prelude::Queryable;
                        c.get().conn.query_first::<i64, _>(sql).unwrap().unwrap() > 0
                    }
                    SessionConn::Redis(_) => unreachable!(),
                }
            };
            let wait = Duration::from_secs(5);
            // The table as 1.1 made it, held by an open transaction.
            let mut holder: Box<dyn FnMut(&str)> = if var == "PHV_TEST_POSTGRES_URL" {
                let c = pg_connect(&url, &spec, wait).unwrap();
                c.client.batch_execute(&format!("DROP TABLE IF EXISTS {t}; {old}")).unwrap();
                c.client.batch_execute(&format!("BEGIN; LOCK TABLE {t} IN ACCESS SHARE MODE")).unwrap();
                Box::new(move |sql| c.client.batch_execute(sql).unwrap())
            } else {
                use mysql::prelude::Queryable;
                let mut c = my_connect(&url, &spec, wait, None, None).unwrap();
                c.conn.query_drop(format!("DROP TABLE IF EXISTS {t}")).unwrap();
                c.conn.query_drop(&old).unwrap();
                c.conn.query_drop("START TRANSACTION").unwrap();
                c.conn.query_drop(format!("SELECT * FROM {t} LIMIT 1")).unwrap();
                Box::new(move |sql| c.conn.query_drop(sql).unwrap())
            };
            lock(&JOB_TABLES).remove(&format!("{url}|{t}"));
            lock(&JOB_UNSIZED).remove(&t);
            let started = Instant::now();
            let cap = JobCap { jobs: 1000, bytes: 1 << 20 };
            let add = || job_add(&spec, ns, (&crate::random_hex(16), "j", "payload"), now_ms(), None, cap, None);
            assert_eq!(add(), Ok(Some(())), "{var}");
            assert!(started.elapsed() < Duration::from_secs(3), "{var}: waited {:?}", started.elapsed());
            assert!(!job_sized(&t), "{var}: the column was added under the open transaction");
            let j = job_take(&spec, ns, 60_000).unwrap().unwrap();
            assert!(job_end(&spec, ns, &j, JobEnd::Done { keyed: false }).unwrap(), "{var}");
            // Once the transaction ended, the next try adds it.
            holder("COMMIT");
            lock(&JOB_UNSIZED).insert(t.clone(), Instant::now() - JOB_SIZE_RETRY);
            assert_eq!(add(), Ok(Some(())), "{var}");
            assert!(job_sized(&t) && has_size(&spec), "{var}: the column was not added");
            holder(&format!("DROP TABLE {t}"));
            lock(&JOB_TABLES).remove(&format!("{url}|{t}"));
        }
    }

    /// A site with a namespace also clears what 1.1 left in the shared session
    /// store once no site can use it, and nothing that is still alive (found
    /// by the 1.1.1 audit: only sites without a namespace swept it).
    #[test]
    fn sites_with_a_namespace_sweep_what_1_1_left() {
        // The store every site shared in 1.1 is one for every test process.
        let _unnamed = crate::testing::unnamed();
        let sweeper = crate::testing::namespace(0x32);
        let limits = SessionLimits {
            ttl: Duration::from_secs(3600),
            max_lifetime: Duration::from_secs(24 * 3600),
            max_sessions: 50_000,
            max_bytes: 256 << 20,
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let row = |created: u64, last: u64| SessionRow { data: "{}".into(), created, last, returned: true };
        let old = Some(SESSIONS_OF_1_1);
        for (var, spec) in test_servers() {
            let dead = format!("a-i2-dead-{}", crate::random_hex(8));
            let alive = format!("a-i2-alive-{}", crate::random_hex(8));
            if var == "PHV_TEST_REDIS_URL" {
                // As 1.1 stored them: the dead one has a second of its lifetime left.
                let lifetime = limits.max_lifetime.as_secs();
                assert!(session_insert(&spec, old, &dead, &row(now - lifetime + 1, now), &limits).unwrap());
                assert!(session_insert(&spec, old, &alive, &row(now, now), &limits).unwrap());
                std::thread::sleep(Duration::from_millis(2100));
                let until = Instant::now() + Duration::from_secs(5);
                let mut c = redis_conn(&spec, PoolUse::Sessions, until).unwrap();
                let mut counted = |key: &str| {
                    let r = redis_run(&mut c, &[b"ZSCORE", b"phv:sessions:back", key.as_bytes()], until).unwrap();
                    !matches!(r, Reply::Nil)
                };
                assert!(session_read(&spec, old, &dead).unwrap().is_none(), "Redis expired it");
                assert!(counted(&dead), "still counted in the shared bookkeeping");
                session_sweep(&spec, Some(&sweeper), &limits).unwrap();
                assert!(!counted(&dead), "{var}: the shared bookkeeping of an expired session was kept");
                assert!(counted(&alive), "{var}: a live session was forgotten");
            } else {
                let day = 24 * 3600;
                assert!(session_insert(&spec, old, &dead, &row(now - day - 60, now - day - 60), &limits).unwrap());
                assert!(session_insert(&spec, old, &alive, &row(now - 100, now - 100), &limits).unwrap());
                session_sweep(&spec, Some(&sweeper), &limits).unwrap();
                assert!(session_read(&spec, old, &dead).unwrap().is_none(), "{var}: an expired session was kept");
                assert!(session_read(&spec, old, &alive).unwrap().is_some(), "{var}: a live session was removed");
            }
            session_remove(&spec, old, &alive).unwrap();
        }
    }

    /// A site without a namespace keeps its sessions apart from the store
    /// every site shared in 1.1: its short ttl and small limits do not remove
    /// the sessions other sites kept there, and its own sessions of 1.1 are
    /// still read (found by the 1.2 audit: it swept the shared store by its
    /// own ttl and limits).
    /// The reset knows the client's statements by the names the server
    /// lists: those the client shows (`statement_name`) and the sentinel's.
    #[test]
    fn statements_are_known_by_their_names() {
        let Some(url) = std::env::var("PHV_TEST_POSTGRES_URL").ok().filter(|u| !u.is_empty()) else { return };
        let spec = DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 1 };
        let mut c = pg_connect(&url, &spec, Duration::from_secs(10)).unwrap();
        let stmt = c.client.run(None, c.client.client().prepare("SELECT 2 AS phv_named")).unwrap();
        let name = statement_name(&stmt).expect("the client shows the name");
        c.keep("SELECT 2 AS phv_named", &stmt);
        let listed = c.client.simple_last_column("SELECT name FROM pg_prepared_statements WHERE NOT from_sql").unwrap();
        assert!(listed.contains(&name) && listed.contains(&c.own[0]), "{listed:?}");
        assert!(c.reset(), "a reset of a sound connection");
        assert_eq!(c.own.len(), 1, "a kept statement is not one of the client's own");
    }

    #[test]
    fn a_site_without_a_namespace_keeps_to_its_own_sessions() {
        // The stores of sites without a namespace are one for every test process.
        let _unnamed = crate::testing::unnamed();
        let short = SessionLimits {
            ttl: Duration::from_secs(60),
            max_lifetime: Duration::from_secs(24 * 3600),
            max_sessions: 1,
            max_bytes: 256 << 20,
        };
        let long = SessionLimits { ttl: Duration::from_secs(7200), max_sessions: 50_000, ..short };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let row = |at: u64| SessionRow { data: "{}".into(), created: at, last: at, returned: true };
        let old = Some(SESSIONS_OF_1_1);
        for (var, spec) in test_servers() {
            // Kept by 1.1: another site's session, and one of this site's own,
            // both idle for two minutes (past this site's ttl).
            let theirs = format!("f3-theirs-{}", crate::random_hex(8));
            let mine = format!("f3-mine-{}", crate::random_hex(8));
            let redis = var == "PHV_TEST_REDIS_URL";
            // Redis takes the time of the last use from its own writes.
            let idle = if redis { now } else { now - 120 };
            assert!(session_insert(&spec, old, &theirs, &row(idle), &long).unwrap(), "{var}");
            assert!(session_insert(&spec, old, &mine, &row(idle), &long).unwrap(), "{var}");
            if redis {
                std::thread::sleep(Duration::from_millis(2100));
            }
            // The site without a namespace writes two sessions (past its
            // limit of one) and sweeps, with a ttl of one second in Redis.
            let brief = SessionLimits { ttl: if redis { Duration::from_secs(1) } else { short.ttl }, ..short };
            let new = [format!("f3-new-{}", crate::random_hex(8)), format!("f3-new-{}", crate::random_hex(8))];
            for k in &new {
                assert!(session_insert(&spec, None, k, &row(now), &brief).unwrap(), "{var}");
            }
            session_sweep(&spec, None, &brief).unwrap();
            assert!(session_read(&spec, old, &theirs).unwrap().is_some(), "{var}: another site's session was removed");
            if redis {
                let until = Instant::now() + Duration::from_secs(5);
                let mut c = redis_conn(&spec, PoolUse::Sessions, until).unwrap();
                let r = redis_run(&mut c, &[b"ZSCORE", b"phv:sessions:back", theirs.as_bytes()], until).unwrap();
                assert!(!matches!(r, Reply::Nil), "{var}: forgotten by the shared bookkeeping");
            }
            // Its own session of 1.1 is read, updated where it is, and logged out.
            assert!(session_read(&spec, None, &mine).unwrap().is_some(), "{var}: its own old session was not read");
            assert!(session_update(&spec, None, &mine, &row(now), &long).unwrap(), "{var}");
            assert!(session_remove(&spec, None, &mine).unwrap(), "{var}");
            assert!(session_read(&spec, old, &mine).unwrap().is_none(), "{var}");
            for k in new.iter().chain([&theirs]) {
                session_remove(&spec, old, k).unwrap();
                session_remove(&spec, None, k).unwrap();
            }
        }
    }

    #[test]
    fn placeholders_render_per_database() {
        let text = "SELECT * FROM \u{E001}t\u{E002} WHERE a = \u{E000} AND b ? 'k' AND c = \u{E000}";
        assert_eq!(render_sql(text, Dialect::Postgres), "SELECT * FROM \"t\" WHERE a = $1 AND b ? 'k' AND c = $2");
        assert_eq!(render_sql(text, Dialect::MySql), "SELECT * FROM `t` WHERE a = ? AND b ? 'k' AND c = ?");
    }
}
