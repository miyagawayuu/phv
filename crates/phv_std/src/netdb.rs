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

use crate::db::{parts, render_sql, Dialect};
use crate::pgmeter::{Meter, PgClient, Trip};
use crate::redisclient::{RedisError, Reply};
use crate::{int, list, s};
use indexmap::IndexMap;
use phv_vm::{DatabaseSpec, Key, Value, Vm, VmError};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::IpAddr;
use std::rc::Rc;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

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
                if since.elapsed() < CHECK_AFTER || c.check() {
                    return Ok(Pooled { conn: Some(c), pool: self.clone(), broken: false });
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
                    Ok(c) => Ok(Pooled { conn: Some(c), pool: self.clone(), broken: false }),
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
    let now = Instant::now();
    let until = vm.deadline().map_or(now + MAX_WAIT, |d| d.min(now + MAX_WAIT));
    let connect_timeout = until.saturating_duration_since(now).clamp(Duration::from_millis(100), CONNECT_TIMEOUT);
    let key = format!("{:?}|{}|{}", spec.tls, spec.pool_size, url);
    let (memory, time_limit) = (vm.memory_limit(), vm.time_limit());
    let result = match kind {
        Kind::Postgres => pool_for(&PG_POOLS, &key, spec.pool_size)
            .acquire(until, || pg_connect(&url, &spec, connect_timeout))
            .map(|p| Link::Pg(Box::new(p))),
        Kind::MySql => pool_for(&MY_POOLS, &key, spec.pool_size)
            .acquire(until, || my_connect(&url, &spec, connect_timeout, memory, time_limit))
            .map(|p| Link::My(Box::new(p))),
        Kind::Redis => pool_for(&REDIS_POOLS, &key, spec.pool_size)
            .acquire(until, || redis_connect(&url, &spec, connect_timeout))
            .map(Link::Redis),
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
            pg_limit_time(vm, c)?;
            let r = c.get().client.batch_execute(cmd);
            r.map_err(|e| pg_error(vm, c, e))?;
        }
        Link::My(c) => {
            use mysql::prelude::Queryable;
            let r = c.get().conn.query_drop(cmd);
            r.map_err(|e| my_error(vm, c, e))?;
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

/// When a statement must end on the client side: just after the script's deadline.
fn statement_deadline(vm: &Vm) -> Option<Instant> {
    vm.deadline().map(|d| d + Duration::from_millis(TIMEOUT_MARGIN_MS))
}

/// The time left before the script's limit, or None without a limit.
fn remaining(vm: &Vm) -> Result<Option<Duration>, VmError> {
    vm.check_limits()?;
    Ok(vm.deadline().map(|d| d.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))))
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

struct PgConn {
    client: PgClient,
    /// The `statement_timeout` last set on the session, in milliseconds.
    timeout_ms: Option<u64>,
    /// Stops reads and writes at the statement's deadline and byte budget.
    meter: Meter,
}

/// Clears what a script can leave on a session. This is `DISCARD ALL` without
/// its `DEALLOCATE ALL`, which would also remove the statements the client
/// prepares for itself (to look up types) and break the connection for the
/// next script; the statements scripts prepare are deallocated by name.
const PG_RESET: &str = "CLOSE ALL; SET SESSION AUTHORIZATION DEFAULT; RESET ALL; UNLISTEN *; \
    SELECT pg_advisory_unlock_all(); DISCARD PLANS; DISCARD TEMP; DISCARD SEQUENCES";

impl PgConn {
    /// Meters the statements PHV runs on its own (reset, check).
    fn meter_internal(&self) {
        self.meter.set(Some(Instant::now() + RESET_TIMEOUT), RESET_BUDGET);
    }

    fn reset_session(&mut self) -> bool {
        // Outside a transaction ROLLBACK only warns; inside one it ends it, so the
        // reset itself does not run inside the script's transaction.
        if self.client.batch_execute("ROLLBACK").is_err() || self.client.batch_execute(PG_RESET).is_err() {
            return false;
        }
        let names: Vec<String> = match self.client.query("SELECT name FROM pg_prepared_statements WHERE from_sql", &[])
        {
            Ok(rows) => match rows.iter().map(|r| r.try_get::<_, String>(0)).collect() {
                Ok(n) => n,
                Err(_) => return false,
            },
            Err(_) => return false,
        };
        names.iter().all(|n| self.client.batch_execute(&format!("DEALLOCATE \"{}\"", n.replace('"', "\"\""))).is_ok())
    }
}

impl Driver for PgConn {
    fn reset(&mut self) -> bool {
        if self.client.is_closed() {
            return false;
        }
        self.timeout_ms = None;
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
}

static PG_TLS: LazyLock<tokio_postgres_rustls::MakeRustlsConnect> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_postgres_rustls::MakeRustlsConnect::new(config)
});

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
    let client = PgClient::connect(&config, tls.then(|| PG_TLS.clone()), timeout, meter.clone())?;
    meter.set(None, RESET_BUDGET);
    Ok(PgConn { client, timeout_ms: None, meter })
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

fn pg_error(vm: &Vm, c: &mut Pooled<PgConn>, e: postgres::Error) -> VmError {
    if c.get().client.is_closed() {
        c.broken = true;
    }
    match c.get().meter.tripped() {
        Some(Trip::Budget) => {
            c.broken = true;
            return out_of_memory(vm);
        }
        Some(Trip::Deadline) => {
            c.broken = true;
            if let Err(fatal) = vm.check_limits() {
                return fatal;
            }
            return vm.throw("DatabaseError", "the statement ran out of time");
        }
        None => {}
    }
    if e.code() == Some(&postgres::error::SqlState::QUERY_CANCELED) {
        // Stopped by the statement_timeout set from the script's time limit.
        if let Err(fatal) = vm.check_limits() {
            return fatal;
        }
    }
    vm.throw("DatabaseError", pg_message(&e))
}

/// Before a statement: the client-side deadline and budget, and the server's
/// `statement_timeout` (which stops the statement without closing the
/// connection when the script has not changed it).
fn pg_limit_time(vm: &Vm, c: &mut Pooled<PgConn>) -> Result<(), VmError> {
    c.get().meter.set(statement_deadline(vm), read_budget(vm));
    let Some(left) = remaining(vm)? else { return Ok(()) };
    let ms = left.as_millis().min(i32::MAX as u128) as u64;
    if needs_new_timeout(c.get().timeout_ms, ms) {
        let r = c.get().client.batch_execute(&format!("SET statement_timeout = {}", ms + TIMEOUT_MARGIN_MS));
        r.map_err(|e| pg_error(vm, c, e))?;
        c.get().timeout_ms = Some(ms);
    }
    Ok(())
}

fn pg_query(vm: &mut Vm, c: &mut Pooled<PgConn>, q: &Value) -> R {
    use postgres::types::ToSql;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::Postgres);
    pg_limit_time(vm, c)?;
    let stmt = c.get().client.prepare(&text);
    let stmt = stmt.map_err(|e| pg_error(vm, c, e))?;
    let names: Vec<Rc<String>> = stmt.columns().iter().map(|col| Rc::new(col.name().to_string())).collect();
    let bound: Vec<PgParam> = params.iter().map(PgParam::new).collect();
    let mut out = Vec::new();
    let mut over = false;
    let result = c.get().client.query_each(&stmt, bound.iter().map(|p| p as &dyn ToSql), |row| {
        let mut m = IndexMap::with_capacity(names.len());
        for (i, n) in names.iter().enumerate() {
            let PgOut(v) = row.try_get(i)?;
            m.insert(Key::Str(n.clone()), v.into_value());
        }
        out.push(Value::map(m));
        if vm.check_memory().is_err() {
            over = true;
            return Ok(false);
        }
        Ok(true)
    });
    result.map_err(|e| pg_error(vm, c, e))?;
    if over {
        // The rest of the result is not read: the connection is closed, not reused.
        c.broken = true;
        return Err(out_of_memory(vm));
    }
    Ok(Value::list(out))
}

fn pg_execute(vm: &mut Vm, c: &mut Pooled<PgConn>, q: &Value) -> R {
    use postgres::types::ToSql;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::Postgres);
    pg_limit_time(vm, c)?;
    let bound: Vec<PgParam> = params.iter().map(PgParam::new).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let n = c.get().client.execute(&text, &refs);
    let n = n.map_err(|e| pg_error(vm, c, e))?;
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
    opts: mysql::Opts,
}

impl Driver for MyConn {
    fn reset(&mut self) -> bool {
        use mysql::prelude::Queryable;
        self.timeout_ms = None;
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
    let opts = mysql::Opts::from(builder.clone());
    // The driver looks the host up itself, with no deadline of its own.
    let conn = crate::netio::with_deadline(Instant::now() + timeout, move || mysql::Conn::new(builder))?
        .map_err(|e| my_message(&e))?;
    Ok(MyConn { conn, timeout_ms: None, opts })
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

/// Stops the statement running on connection `id` with `KILL QUERY` (from a
/// second connection) when it is still running at `deadline`. Dropping the
/// guard before then cancels it.
struct KillAt {
    stop: Option<std::sync::mpsc::Sender<()>>,
}

impl KillAt {
    fn arm(opts: &mysql::Opts, id: u32, deadline: Option<Instant>) -> KillAt {
        let Some(deadline) = deadline else { return KillAt { stop: None } };
        let (stop, wait) = std::sync::mpsc::channel::<()>();
        let opts = opts.clone();
        std::thread::spawn(move || {
            let left = deadline.saturating_duration_since(Instant::now());
            if wait.recv_timeout(left) != Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                return;
            }
            use mysql::prelude::Queryable;
            let side = mysql::OptsBuilder::from_opts(opts)
                .tcp_connect_timeout(Some(Duration::from_secs(5)))
                .read_timeout(Some(Duration::from_secs(5)))
                .write_timeout(Some(Duration::from_secs(5)));
            if let Ok(mut side) = mysql::Conn::new(side) {
                let _ = side.query_drop(format!("KILL QUERY {id}"));
            }
        });
        KillAt { stop: Some(stop) }
    }
}

impl Drop for KillAt {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn my_kill_at(vm: &Vm, c: &mut Pooled<MyConn>) -> KillAt {
    let id = c.get().conn.connection_id();
    KillAt::arm(&c.get().opts, id, statement_deadline(vm))
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

fn my_limit_time(vm: &Vm, c: &mut Pooled<MyConn>) -> Result<(), VmError> {
    use mysql::prelude::Queryable;
    let Some(left) = remaining(vm)? else { return Ok(()) };
    let ms = left.as_millis().min(u32::MAX as u128) as u64;
    if needs_new_timeout(c.get().timeout_ms, ms) {
        let r = c.get().conn.query_drop(format!("SET SESSION max_execution_time = {}", ms + TIMEOUT_MARGIN_MS));
        r.map_err(|e| my_error(vm, c, e))?;
        c.get().timeout_ms = Some(ms);
    }
    Ok(())
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
    let mut out = Vec::new();
    let mut convert_error = None;
    let mut over = false;
    let result: Result<(), mysql::Error> = (|| {
        let mut result = c.get().conn.exec_iter(text, my_params(params))?;
        let Some(set) = result.iter() else { return Ok(()) };
        let columns: Vec<mysql::Column> = set.columns().as_ref().to_vec();
        let names: Vec<Rc<String>> = columns.iter().map(|col| Rc::new(col.name_str().into_owned())).collect();
        for row in set {
            let values = row?.unwrap();
            let mut m = IndexMap::with_capacity(names.len());
            for ((n, v), col) in names.iter().zip(values).zip(&columns) {
                match my_value(v, col) {
                    Ok(v) => {
                        m.insert(Key::Str(n.clone()), v.into_value());
                    }
                    Err(e) => {
                        convert_error = Some(e);
                        return Ok(());
                    }
                }
            }
            out.push(Value::map(m));
            if vm.check_memory().is_err() {
                over = true;
                return Ok(());
            }
        }
        Ok(())
    })();
    drop(kill);
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
    Ok(Value::list(out))
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
    drop(kill);
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
    let until = crate::netio::until(vm, DETACHED_WAIT);
    let budget = detached_budget(vm);
    let fail = format!("database \"{name}\": ");
    match kind_of(&url) {
        Kind::Postgres => {
            let text = render_sql(text, Dialect::Postgres);
            let params: Vec<PgParam> = params.iter().map(PgParam::new).collect();
            crate::asyncio::start(vm, until, move || {
                let r = pg_detached(&spec, &url, &text, &params, execute, until, budget);
                crate::asyncio::Outcome::Db(r.map_err(|e| format!("{fail}{e}")))
            })
        }
        Kind::MySql => {
            let text = render_sql(text, Dialect::MySql);
            let params = my_params(params);
            crate::asyncio::start(vm, until, move || {
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
    let until = crate::netio::until(vm, MAX_WAIT);
    let budget = detached_budget(vm);
    let fail = format!("database \"{name}\": ");
    crate::asyncio::start(vm, until, move || {
        let r = (|| {
            let mut c = redis_conn(&spec, until)?;
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

fn detached_conn<C: Driver>(
    registry: &Registry<C>,
    spec: &DatabaseSpec,
    url: &str,
    until: Instant,
    connect: impl FnOnce(Duration) -> Result<C, String>,
) -> Result<Pooled<C>, String> {
    let key = format!("{:?}|{}|{}", spec.tls, spec.pool_size, url);
    let timeout = until.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(100), CONNECT_TIMEOUT);
    match pool_for(registry, &key, spec.pool_size).acquire(until, || connect(timeout)) {
        Ok(c) => Ok(c),
        Err(AcquireError::Busy) => Err(format!("all {} connections are in use (raise pool_size?)", spec.pool_size)),
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
    let mut c = detached_conn(&PG_POOLS, spec, url, until, |t| pg_connect(url, spec, t))?;
    let error = |c: &mut Pooled<PgConn>, e: postgres::Error| -> String {
        if c.get().client.is_closed() {
            c.broken = true;
        }
        match c.get().meter.tripped() {
            Some(Trip::Budget) => {
                c.broken = true;
                OVER_BUDGET.to_string()
            }
            Some(Trip::Deadline) => {
                c.broken = true;
                "the statement ran out of time".to_string()
            }
            None if e.code() == Some(&postgres::error::SqlState::QUERY_CANCELED) => {
                "the statement ran out of time".to_string()
            }
            None => pg_message(&e),
        }
    };
    c.get().meter.set(Some(until + Duration::from_millis(TIMEOUT_MARGIN_MS)), budget);
    let ms = until.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as u64;
    if needs_new_timeout(c.get().timeout_ms, ms) {
        let r = c.get().client.batch_execute(&format!("SET statement_timeout = {}", ms + TIMEOUT_MARGIN_MS));
        r.map_err(|e| error(&mut c, e))?;
        c.get().timeout_ms = Some(ms);
    }
    if execute {
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let n = c.get().client.execute(text, &refs).map_err(|e| error(&mut c, e))?;
        return Ok(Detached::Count(n));
    }
    let stmt = c.get().client.prepare(text).map_err(|e| error(&mut c, e))?;
    let names: Vec<String> = stmt.columns().iter().map(|col| col.name().to_string()).collect();
    let mut rows = Vec::new();
    let mut used = 0usize;
    let mut over = false;
    let result = c.get().client.query_each(&stmt, params.iter().map(|p| p as &dyn ToSql), |row| {
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
    });
    result.map_err(|e| error(&mut c, e))?;
    if over {
        c.broken = true;
        return Err(OVER_BUDGET.into());
    }
    Ok(Detached::Rows(names, rows))
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
    let mut c = detached_conn(&MY_POOLS, spec, url, until, |t| my_connect(url, spec, t, Some(budget * 2), Some(left)))?;
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
    let ms = left.as_millis().min(u32::MAX as u128) as u64;
    if needs_new_timeout(c.get().timeout_ms, ms) {
        let r = c.get().conn.query_drop(format!("SET SESSION max_execution_time = {}", ms + TIMEOUT_MARGIN_MS));
        r.map_err(|e| error(&mut c, e))?;
        c.get().timeout_ms = Some(ms);
    }
    let id = c.get().conn.connection_id();
    let kill = KillAt::arm(&c.get().opts, id, Some(until + Duration::from_millis(TIMEOUT_MARGIN_MS)));
    if execute {
        let r = c.get().conn.exec_drop(text, params);
        drop(kill);
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
    drop(kill);
    result.map_err(|e| error(&mut c, e))?;
    if let Some(e) = problem {
        // The rest of the result was not read: the connection is out of step.
        c.broken = true;
        return Err(e);
    }
    Ok(Detached::Rows(names, rows))
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

// ----- Shared rate limits ------------------------------------------------------

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
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let slot = now / ms;
    let rkey = format!("{}{ms}:{slot}", redis_prefix(key));
    let mut conn = redis_conn(spec, until)?;
    let n = match conn.get().command(&[b"INCR", rkey.as_bytes()], until, 1024) {
        Ok(Reply::Int(n)) => n,
        other => {
            conn.broken = true;
            return Err(redis_failure(other.err()));
        }
    };
    if n == 1 {
        // The counter lives as long as its window (plus a margin for clock skew).
        let ttl = (ms + 1000).to_string();
        if conn.get().command(&[b"PEXPIRE", rkey.as_bytes(), ttl.as_bytes()], until, 1024).is_err() {
            conn.broken = true;
        }
    }
    Ok((n <= i64::from(max), ((slot + 1) * ms - now) / 1000 + 1))
}

/// Forgets every window of `key` on the Redis server `spec`.
pub(crate) fn redis_reset(spec: &DatabaseSpec, key: &str, until: Instant) -> Result<(), String> {
    let pattern = format!("{}*", redis_prefix(key));
    let mut conn = redis_conn(spec, until)?;
    let mut cursor = b"0".to_vec();
    loop {
        let reply =
            conn.get().command(&[b"SCAN", &cursor, b"MATCH", pattern.as_bytes(), b"COUNT", b"1000"], until, 1 << 20);
        let mut parts = match reply {
            Ok(Reply::Array(parts)) => parts,
            other => {
                conn.broken = true;
                return Err(redis_failure(other.err()));
            }
        };
        let (Some(Reply::Array(keys)), Some(Reply::Bulk(next))) = (parts.pop(), parts.pop()) else {
            conn.broken = true;
            return Err("the server sent an invalid reply".into());
        };
        for k in keys {
            if let Reply::Bulk(k) = k {
                if conn.get().command(&[b"DEL", &k], until, 1024).is_err() {
                    conn.broken = true;
                    return Err("the server did not answer".into());
                }
            }
        }
        if next == b"0" {
            return Ok(());
        }
        cursor = next;
    }
}

/// Counter keys of `key`: hashed, so they hold no glob characters for `SCAN`
/// and their length is bounded.
fn redis_prefix(key: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("phv:ratelimit:{:x}:", Sha256::digest(key.as_bytes()))
}

fn redis_conn(spec: &DatabaseSpec, until: Instant) -> Result<Pooled<RedisConn>, String> {
    let Some(url) = spec.url.clone() else {
        return Err(format!("the environment variable {} is not set", spec.url_env.as_deref().unwrap_or("?")));
    };
    if kind_of(&url) != Kind::Redis {
        return Err("it is not a Redis server".into());
    }
    tls_init();
    let key = format!("{:?}|{}|{}", spec.tls, spec.pool_size, url);
    let timeout = until.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(100), CONNECT_TIMEOUT);
    match pool_for(&REDIS_POOLS, &key, spec.pool_size).acquire(until, || redis_connect(&url, spec, timeout)) {
        Ok(c) => Ok(c),
        Err(AcquireError::Busy) => Err("all its connections are in use".into()),
        Err(AcquireError::Connect(e)) => Err(e),
    }
}

fn redis_failure(e: Option<crate::redisclient::RedisError>) -> String {
    match e {
        Some(crate::redisclient::RedisError::Server(m)) => m,
        Some(_) => "the server did not answer".into(),
        None => "the server sent an invalid reply".into(),
    }
}

/// Commands scripts may run, with their argument counts.
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
    fn placeholders_render_per_database() {
        let text = "SELECT * FROM \u{E001}t\u{E002} WHERE a = \u{E000} AND b ? 'k' AND c = \u{E000}";
        assert_eq!(render_sql(text, Dialect::Postgres), "SELECT * FROM \"t\" WHERE a = $1 AND b ? 'k' AND c = $2");
        assert_eq!(render_sql(text, Dialect::MySql), "SELECT * FROM `t` WHERE a = ? AND b ? 'k' AND c = ?");
    }
}
