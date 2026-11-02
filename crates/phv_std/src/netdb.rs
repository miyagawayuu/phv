//! Database servers named in phv.toml: PostgreSQL and MySQL behind the
//! `Database` class, and Redis. Scripts only name a server
//! (`Database::connect("main")`); the URL and its credentials stay in the
//! configuration.
//!
//! Connections come from a pool shared by every script in the process. A
//! connection goes back to the pool when the script ends or closes it, after
//! its session is reset (open transaction rolled back, settings cleared), so
//! nothing carries over from one request to the next.

use crate::db::{parts, render_sql, Dialect};
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

fn is_local_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The rustls provider every TLS connection uses (ring; installed once as the
/// process default for the clients that build their own configuration).
fn tls_init() {
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
    let key = format!("{:?}|{}", spec.tls, url);
    let result = match kind {
        Kind::Postgres => pool_for(&PG_POOLS, &key, spec.pool_size)
            .acquire(until, || pg_connect(&url, &spec, connect_timeout))
            .map(|p| Link::Pg(Box::new(p))),
        Kind::MySql => pool_for(&MY_POOLS, &key, spec.pool_size)
            .acquire(until, || my_connect(&url, &spec, connect_timeout))
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
    client: postgres::Client,
    /// The `statement_timeout` last set on the session, in milliseconds.
    timeout_ms: Option<u64>,
}

impl Driver for PgConn {
    fn reset(&mut self) -> bool {
        if self.client.is_closed() {
            return false;
        }
        self.timeout_ms = None;
        // DISCARD ALL cannot run inside a transaction: roll back first if one is open.
        self.client.batch_execute("DISCARD ALL").is_ok()
            || (self.client.batch_execute("ROLLBACK").is_ok() && self.client.batch_execute("DISCARD ALL").is_ok())
    }

    fn check(&mut self) -> bool {
        self.client.is_valid(Duration::from_secs(5)).is_ok()
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
    use postgres::config::{Host, SslMode};
    // The parser's own message could quote the URL, which may hold a password.
    let mut config: postgres::Config = url.parse().map_err(|_| "the URL is not a valid PostgreSQL URL".to_string())?;
    let local = config.get_hosts().iter().all(|h| match h {
        Host::Tcp(name) => is_local_host(name),
        #[allow(unreachable_patterns)]
        _ => true,
    }) && config.get_hostaddrs().iter().all(|ip| ip.is_loopback());
    let tls = spec.tls.unwrap_or(!local);
    config.connect_timeout(timeout);
    let client = if tls {
        config.ssl_mode(SslMode::Require);
        config.connect(PG_TLS.clone())
    } else {
        config.ssl_mode(SslMode::Disable);
        config.connect(postgres::NoTls)
    }
    .map_err(|e| pg_message(&e))?;
    Ok(PgConn { client, timeout_ms: None })
}

fn pg_message(e: &postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => match db.detail() {
            Some(d) => format!("{} ({d})", db.message()),
            None => db.message().to_string(),
        },
        // Conversion errors keep the reason in the source ("error serializing parameter 0: ...").
        None => match std::error::Error::source(e) {
            Some(cause) => format!("{e}: {cause}"),
            None => e.to_string(),
        },
    }
}

fn pg_error(vm: &Vm, c: &mut Pooled<PgConn>, e: postgres::Error) -> VmError {
    if c.get().client.is_closed() {
        c.broken = true;
    }
    if e.code() == Some(&postgres::error::SqlState::QUERY_CANCELED) {
        // Stopped by the statement_timeout set from the script's time limit.
        if let Err(fatal) = vm.check_limits() {
            return fatal;
        }
    }
    vm.throw("DatabaseError", pg_message(&e))
}

fn pg_limit_time(vm: &Vm, c: &mut Pooled<PgConn>) -> Result<(), VmError> {
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
    use postgres::fallible_iterator::FallibleIterator;
    use postgres::types::ToSql;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::Postgres);
    pg_limit_time(vm, c)?;
    let stmt = c.get().client.prepare(&text);
    let stmt = stmt.map_err(|e| pg_error(vm, c, e))?;
    let names: Vec<Rc<String>> = stmt.columns().iter().map(|col| Rc::new(col.name().to_string())).collect();
    let bound: Vec<PgParam> = params.iter().map(PgParam::new).collect();
    let mut out = Vec::new();
    let result: Result<(), postgres::Error> = (|| {
        let mut rows = c.get().client.query_raw(&stmt, bound.iter().map(|p| p as &dyn ToSql))?;
        while let Some(row) = rows.next()? {
            let mut m = IndexMap::with_capacity(names.len());
            for (i, n) in names.iter().enumerate() {
                let PgOut(v) = row.try_get(i)?;
                m.insert(Key::Str(n.clone()), v);
            }
            out.push(Value::map(m));
            if out.len() % 256 == 0 && vm.check_memory().is_err() {
                break;
            }
        }
        Ok(())
    })();
    result.map_err(|e| pg_error(vm, c, e))?;
    vm.check_memory()?;
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
struct PgOut(Value);

impl<'a> postgres::types::FromSql<'a> for PgOut {
    fn from_sql(ty: &postgres::types::Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        pg_decode(ty, raw).map(PgOut)
    }

    fn from_sql_null(_: &postgres::types::Type) -> Result<Self, BoxError> {
        Ok(PgOut(Value::Null))
    }

    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }
}

fn pg_decode(ty: &postgres::types::Type, raw: &[u8]) -> Result<Value, BoxError> {
    use postgres::types::{FromSql, Kind, Type};
    use std::fmt::Write;
    if let Kind::Domain(inner) = ty.kind() {
        return pg_decode(inner, raw);
    }
    if is_text_type(ty) || *ty == Type::JSON {
        return Ok(Value::str(String::from_utf8_lossy(raw).into_owned()));
    }
    Ok(match *ty {
        Type::BOOL => Value::Bool(bool::from_sql(ty, raw)?),
        Type::INT2 => Value::Int(i16::from_sql(ty, raw)? as i64),
        Type::INT4 => Value::Int(i32::from_sql(ty, raw)? as i64),
        Type::INT8 => Value::Int(i64::from_sql(ty, raw)?),
        Type::OID => Value::Int(u32::from_sql(ty, raw)? as i64),
        Type::FLOAT4 => Value::Float(f32::from_sql(ty, raw)? as f64),
        Type::FLOAT8 => Value::Float(f64::from_sql(ty, raw)?),
        Type::NUMERIC => Value::str(numeric_decode(raw)?),
        Type::JSONB => match raw.split_first() {
            Some((1, rest)) => Value::str(String::from_utf8_lossy(rest).into_owned()),
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
            Value::str(s)
        }
        Type::BYTEA => {
            let mut s = String::with_capacity(2 + raw.len() * 2);
            s.push_str("\\x");
            for b in raw {
                write!(s, "{b:02x}").unwrap();
            }
            Value::str(s)
        }
        Type::DATE => Value::str(chrono::NaiveDate::from_sql(ty, raw)?.format("%Y-%m-%d").to_string()),
        Type::TIME => Value::str(chrono::NaiveTime::from_sql(ty, raw)?.format("%H:%M:%S%.f").to_string()),
        Type::TIMESTAMP => {
            Value::str(chrono::NaiveDateTime::from_sql(ty, raw)?.format("%Y-%m-%d %H:%M:%S%.f").to_string())
        }
        Type::TIMESTAMPTZ => Value::str(
            chrono::DateTime::<chrono::Utc>::from_sql(ty, raw)?.format("%Y-%m-%d %H:%M:%S%.f+00:00").to_string(),
        ),
        Type::VOID => Value::Null,
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
}

impl Driver for MyConn {
    fn reset(&mut self) -> bool {
        self.timeout_ms = None;
        // Rolls back an open transaction and clears session variables and temporary tables.
        self.conn.reset().is_ok()
    }

    fn check(&mut self) -> bool {
        self.conn.ping().is_ok()
    }
}

fn my_connect(url: &str, spec: &DatabaseSpec, timeout: Duration) -> Result<MyConn, String> {
    // The parser's own message could quote the URL, which may hold a password.
    let opts = mysql::Opts::from_url(url).map_err(|_| "the URL is not a valid MySQL URL".to_string())?;
    let local = opts.get_socket().is_some() || is_local_host(&opts.get_ip_or_hostname());
    let tls = spec.tls.unwrap_or(!local);
    let ssl = match (tls, opts.get_ssl_opts().cloned()) {
        (false, _) => None,
        (true, Some(o)) => Some(o),
        (true, None) => Some(mysql::SslOpts::default()),
    };
    let builder = mysql::OptsBuilder::from_opts(opts)
        .ssl_opts(ssl)
        .tcp_connect_timeout(Some(timeout))
        .read_timeout(Some(Duration::from_secs(300)))
        .write_timeout(Some(Duration::from_secs(300)));
    let conn = mysql::Conn::new(builder).map_err(|e| my_message(&e))?;
    Ok(MyConn { conn, timeout_ms: None })
}

fn my_message(e: &mysql::Error) -> String {
    match e {
        mysql::Error::MySqlError(m) => m.message.clone(),
        e => e.to_string(),
    }
}

fn my_error(vm: &Vm, c: &mut Pooled<MyConn>, e: mysql::Error) -> VmError {
    match &e {
        mysql::Error::MySqlError(m) => {
            // 3024: stopped by the max_execution_time set from the script's time limit.
            if m.code == 3024 {
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

fn my_value(v: mysql::Value, col: &mysql::Column) -> Result<Value, String> {
    use mysql::consts::ColumnType;
    Ok(match v {
        mysql::Value::NULL => Value::Null,
        mysql::Value::Int(i) => Value::Int(i),
        mysql::Value::UInt(u) => {
            Value::Int(i64::try_from(u).map_err(|_| format!("column {}: {u} is too large for int", col.name_str()))?)
        }
        mysql::Value::Float(f) => Value::Float(f as f64),
        mysql::Value::Double(f) => Value::Float(f),
        mysql::Value::Bytes(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
        mysql::Value::Date(y, mo, d, h, mi, s, us) => Value::str(match col.column_type() {
            ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => format!("{y:04}-{mo:02}-{d:02}"),
            _ if us > 0 => format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}"),
            _ => format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"),
        }),
        mysql::Value::Time(neg, days, h, mi, s, us) => {
            let sign = if neg { "-" } else { "" };
            let hours = days as u64 * 24 + h as u64;
            Value::str(if us > 0 {
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
    let mut out = Vec::new();
    let mut convert_error = None;
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
                        m.insert(Key::Str(n.clone()), v);
                    }
                    Err(e) => {
                        convert_error = Some(e);
                        return Ok(());
                    }
                }
            }
            out.push(Value::map(m));
            if out.len() % 256 == 0 && vm.check_memory().is_err() {
                return Ok(());
            }
        }
        Ok(())
    })();
    result.map_err(|e| my_error(vm, c, e))?;
    if let Some(e) = convert_error {
        // The rest of the result was not read: the connection is out of step.
        c.broken = true;
        return Err(vm.throw("DatabaseError", e));
    }
    if let Err(e) = vm.check_memory() {
        c.broken = true;
        return Err(e);
    }
    Ok(Value::list(out))
}

fn my_execute(vm: &mut Vm, c: &mut Pooled<MyConn>, q: &Value) -> R {
    use mysql::prelude::Queryable;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::MySql);
    my_limit_time(vm, c)?;
    let mut r = c.get().conn.exec_drop(&text, my_params(params));
    // Statements such as BEGIN cannot be prepared (error 1295). Without
    // parameters they run as plain text, but only when the text holds no `;`,
    // since the text protocol would run a second statement after it.
    let unsupported = matches!(&r, Err(mysql::Error::MySqlError(m)) if m.code == 1295);
    if unsupported && params.is_empty() && !text.contains(';') {
        r = c.get().conn.query_drop(&text);
    }
    r.map_err(|e| my_error(vm, c, e))?;
    Ok(Value::Int(c.get().conn.affected_rows().try_into().unwrap_or(i64::MAX)))
}

// ----- Redis -----------------------------------------------------------------

struct RedisConn(redis::Connection);

impl Driver for RedisConn {
    fn reset(&mut self) -> bool {
        // Scripts can only run the data commands below, which leave no session state.
        true
    }

    fn check(&mut self) -> bool {
        redis::cmd("PING").query::<String>(&mut self.0).is_ok()
    }
}

fn redis_connect(url: &str, spec: &DatabaseSpec, timeout: Duration) -> Result<RedisConn, String> {
    use redis::ConnectionAddr;
    let client = redis::Client::open(url).map_err(|_| "the URL is not a valid Redis URL".to_string())?;
    let (local, tls) = match client.get_connection_info().addr() {
        ConnectionAddr::Tcp(host, _) => (is_local_host(host), false),
        ConnectionAddr::TcpTls { host, .. } => (is_local_host(host), true),
        _ => (true, false),
    };
    if !tls && spec.tls.unwrap_or(!local) {
        return Err("the server is not on this machine: use a rediss:// URL (TLS), or set tls = false".into());
    }
    let conn = client.get_connection_with_timeout(timeout).map_err(|e| e.to_string())?;
    Ok(RedisConn(conn))
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

fn redis_value(v: redis::Value) -> Value {
    match v {
        redis::Value::Nil => Value::Null,
        redis::Value::Int(i) => Value::Int(i),
        redis::Value::BulkString(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
        redis::Value::SimpleString(s) => Value::str(s),
        redis::Value::Okay => Value::Bool(true),
        redis::Value::Boolean(b) => Value::Bool(b),
        redis::Value::Double(f) => Value::Float(f),
        redis::Value::Array(items) | redis::Value::Set(items) => {
            Value::list(items.into_iter().map(redis_value).collect())
        }
        redis::Value::Map(pairs) => Value::map(
            pairs
                .into_iter()
                .map(|(k, v)| (Key::Str(Rc::new(redis_value(k).to_display_string())), redis_value(v)))
                .collect(),
        ),
        _ => Value::Null,
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
    let mut cmd = redis::cmd(name);
    for arg in args.iter() {
        cmd.arg(s(arg));
    }
    let conn = &mut c.get().0;
    let r = conn
        .set_read_timeout(left)
        .and_then(|_| conn.set_write_timeout(left))
        .and_then(|_| cmd.query::<redis::Value>(conn));
    let v = match r {
        Ok(v) => v,
        Err(e) => {
            if e.is_io_error() || e.is_timeout() || e.is_connection_dropped() || e.is_unrecoverable_error() {
                c.broken = true;
                vm.check_limits()?;
            }
            return Err(vm.throw("DatabaseError", e.to_string()));
        }
    };
    let out = match (name, v) {
        // RESP2 answers HGETALL with a flat list of fields and values.
        ("HGETALL", redis::Value::Array(items)) => {
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
