// ----- Statements run in the background (queryAsync) --------------------------

use super::handle::{kind_of, tls_init, wrong_kind, Kind, NetDb, TIMEOUT_MARGIN_MS};
use super::my::{my_connect, my_message, my_params, my_set_timeout, my_value, KillAt, MyConn};
use super::pg::{
    check_statement, pg_connect, pg_failure, pg_limits, pg_message, pg_text, PgFailure, PgOut, PgParam, PgText,
};
use super::pool::{pool_for, AcquireError, Driver, Pooled, Registry, MY_POOLS, PG_POOLS};
use super::ratelimit::{phv_key_error, redis_conn, PHV_KEYS, REDIS_COMMANDS};
use super::{CONNECT_TIMEOUT, MAX_WAIT, R};
use crate::db::{parts, render_sql, Dialect};
use crate::redisclient::RedisError;
use crate::{int, list, s};
use indexmap::IndexMap;
use phv_vm::{DatabaseSpec, Key, Value, Vm, VmError};
use std::rc::Rc;
use std::time::{Duration, Instant};

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
    pub(super) fn into_value(self) -> Value {
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
            check_statement(vm, &text, Kind::Postgres)?;
            let params: Vec<PgParam> = params.iter().map(PgParam::new).collect();
            crate::asyncio::start(vm, until, pool, move || {
                let r = pg_detached(&spec, &url, &text, &params, execute, until, budget);
                crate::asyncio::Outcome::Db(r.map_err(|e| format!("{fail}{e}")))
            })
        }
        Kind::MySql => {
            let text = render_sql(text, Dialect::MySql);
            check_statement(vm, &text, Kind::MySql)?;
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
pub(super) fn pooled<C: Driver>(
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
pub(super) fn detached_conn<C: Driver>(
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
        PgFailure::Deadline | PgFailure::Canceled => crate::netio::TIME_LIMIT.to_string(),
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
                return crate::netio::TIME_LIMIT.to_string();
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
