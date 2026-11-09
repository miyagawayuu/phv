// ----- Connections held by a script ------------------------------------------

use super::my::{my_connect, my_error, my_execute, my_fits, my_query, MyConn};
use super::pg::{pg_connect, pg_execute, pg_query, pg_run, PgConn};
use super::pool::{AcquireError, Driver, Pooled, MY_POOLS, PG_POOLS, REDIS_POOLS};
use super::query_async::{pooled, pooled_fitting, PoolUse};
use super::redis::redis_connect;
use super::{MAX_WAIT, R};
use crate::db::{parts, transaction_step, Dialect, Ended, TxConn};
use crate::redisclient::RedisConn;
use crate::{int, s};
use phv_vm::{Value, Vm, VmError};
use std::cell::RefCell;
use std::net::IpAddr;
use std::rc::Rc;
use std::time::Duration;

pub(super) enum Link {
    Pg(Box<Pooled<PgConn>>),
    My(Box<Pooled<MyConn>>),
    Redis(Pooled<RedisConn>),
}

/// A script's handle to a server connection. Connecting to the same name twice
/// in one script shares the connection, so a script never waits on itself for
/// a free connection.
pub(super) struct NetDb {
    pub(super) name: String,
    pub(super) link: Rc<RefCell<Link>>,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Kind {
    Postgres,
    MySql,
    Redis,
}

pub(super) fn kind_of(url: &str) -> Kind {
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

pub(super) fn link_of(vm: &Vm, h: &Value) -> Result<Rc<RefCell<Link>>, VmError> {
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
        Kind::MySql => pooled_fitting(&MY_POOLS, &spec, &url, use_, until, my_fits(memory, time_limit), |t| {
            my_connect(&url, &spec, t, memory, time_limit)
        })
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

pub(super) fn wrong_kind(vm: &Vm, name: &str, is_redis: bool) -> VmError {
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
        Link::Pg(c) => guarded(vm, c, &a[1], Dialect::Postgres, |vm, c| pg_query(vm, c, &a[1])),
        Link::My(c) => guarded(vm, c, &a[1], Dialect::MySql, |vm, c| my_query(vm, c, &a[1])),
        Link::Redis(_) => unreachable!("Redis handle used as a database"),
    }
}

pub fn execute(vm: &mut Vm, a: &[Value]) -> R {
    let link = link_of(vm, &a[0])?;
    let mut link = link.borrow_mut();
    match &mut *link {
        Link::Pg(c) => guarded(vm, c, &a[1], Dialect::Postgres, |vm, c| pg_execute(vm, c, &a[1])),
        Link::My(c) => guarded(vm, c, &a[1], Dialect::MySql, |vm, c| my_execute(vm, c, &a[1])),
        Link::Redis(_) => unreachable!("Redis handle used as a database"),
    }
}

/// Runs a script's statement inside the open `transaction()` calls, if any:
/// refused once the database ended their transaction (it would run outside
/// any transaction and stay when `transaction()` throws), and a statement
/// that ends it (`COMMIT`, `ROLLBACK` written as SQL; on MySQL `BEGIN` too)
/// is noted, so that their `COMMIT` raises (see `crate::db::Ended`).
fn guarded<C: Driver>(
    vm: &mut Vm,
    c: &mut Pooled<C>,
    q: &Value,
    dialect: Dialect,
    run: impl FnOnce(&mut Vm, &mut Pooled<C>) -> R,
) -> R {
    if c.depth == 0 {
        return run(vm, c);
    }
    if let Some(ended) = c.ended {
        return Err(vm.throw("DatabaseError", ended.refused()));
    }
    let r = run(vm, c);
    if c.ended.is_none() && super::pg::ends_transaction(parts(q).0, dialect) {
        c.ended = Some(Ended::ByStatement);
    }
    r
}

/// `BEGIN` / `COMMIT` / `ROLLBACK` for `Database::transaction()` (see
/// `crate::db::transaction_step`).
pub fn raw(vm: &mut Vm, a: &[Value]) -> R {
    let link = link_of(vm, &a[0])?;
    let mut link = link.borrow_mut();
    let cmd = s(&a[1]);
    let r = match &mut *link {
        Link::Pg(c) => {
            let r = transaction_step(vm, &mut **c, cmd);
            // A transaction boundary: kept statements prepared before it are
            // not used inside the next transaction (see `PgConn::kept`).
            c.get().epoch += 1;
            r
        }
        Link::My(c) => transaction_step(vm, &mut **c, cmd),
        Link::Redis(_) => unreachable!("Redis handle used as a database"),
    };
    r.map(|()| Value::Null)
}

impl TxConn for Pooled<PgConn> {
    fn depth(&self) -> u32 {
        self.depth
    }

    fn set_depth(&mut self, depth: u32) {
        self.depth = depth;
    }

    fn ended(&self) -> Option<Ended> {
        self.ended
    }

    fn set_ended(&mut self, ended: Option<Ended>) {
        self.ended = ended;
    }

    /// A `COMMIT` or `RELEASE SAVEPOINT` refused because the transaction is
    /// aborted (a statement in it failed) raises `Ended::Aborted`'s error.
    fn run(&mut self, vm: &Vm, sql: &str) -> Result<(), VmError> {
        let ends = !sql.starts_with("SAVEPOINT");
        let mut aborted = false;
        let r = pg_run(vm, self, |c, prelude| {
            let client = &c.get().client;
            let r = client.run(prelude, client.client().batch_execute(sql));
            aborted = matches!(&r, Err(e) if e.code() == Some(&postgres::error::SqlState::IN_FAILED_SQL_TRANSACTION));
            r
        });
        match r {
            Err(VmError::Exception(_)) if aborted && ends => {
                Err(vm.throw("DatabaseError", Ended::Aborted.not_committed()))
            }
            r => r,
        }
    }

    /// `COMMIT` of an aborted transaction rolls it back without an error
    /// (only its command tag says `ROLLBACK`, and the client does not show
    /// it): `SELECT 1` before it, in the same round trip, fails in an
    /// aborted transaction, and the server then skips the `COMMIT`.
    fn commit(&mut self, vm: &Vm) -> Result<(), VmError> {
        self.run(vm, "SELECT 1; COMMIT")
    }

    fn finished(&mut self, _: &mut Vm, rolled_back: bool) {
        if let Some(j) = self.get().jobs_tx.take() {
            j.ended();
        }
        // Perhaps still in the transaction: not given to another script.
        if !rolled_back {
            self.broken = true;
        }
    }
}

impl TxConn for Pooled<MyConn> {
    fn depth(&self) -> u32 {
        self.depth
    }

    fn set_depth(&mut self, depth: u32) {
        self.depth = depth;
    }

    fn ended(&self) -> Option<Ended> {
        self.ended
    }

    fn set_ended(&mut self, ended: Option<Ended>) {
        self.ended = ended;
    }

    fn run(&mut self, vm: &Vm, sql: &str) -> Result<(), VmError> {
        use mysql::prelude::Queryable;
        let r = self.get().conn.query_drop(sql);
        r.map_err(|e| my_error(vm, self, e))
    }

    fn finished(&mut self, _: &mut Vm, rolled_back: bool) {
        if let Some(j) = self.get().jobs_tx.take() {
            j.ended();
        }
        if !rolled_back {
            self.broken = true;
        }
    }
}

/// The SQL dialect of a server connection handle (std.orm), or None when it
/// is closed or a Redis connection.
pub(crate) fn dialect_of(vm: &Vm, h: &Value) -> Option<crate::db::Dialect> {
    let link = link_of(vm, h).ok()?;
    let link = link.borrow();
    match &*link {
        Link::Pg(_) => Some(crate::db::Dialect::Postgres),
        Link::My(_) => Some(crate::db::Dialect::MySql),
        Link::Redis(_) => None,
    }
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
pub(super) fn out_of_memory(vm: &Vm) -> VmError {
    match vm.check_memory() {
        Err(e) => e,
        Ok(()) => vm.reserve(usize::MAX).expect_err("reserving usize::MAX fails"),
    }
}

/// The bytes a statement may read: about half the memory left to the script
/// (the values made from them take about as much again).
pub(super) fn read_budget(vm: &Vm) -> usize {
    (vm.memory_left() / 2).max(64 << 10)
}

/// The time left before the script's limit (at least a millisecond, so a
/// server timeout set from it is never zero), or None without a limit.
pub(super) fn remaining(vm: &Vm) -> Result<Option<Duration>, VmError> {
    vm.check_limits()?;
    Ok(crate::deadline::remaining(vm).map(|left| left.max(Duration::from_millis(1))))
}

/// Server timeouts end this long after the script's deadline, so the script's
/// own limit has always passed when the server stops a statement (otherwise a
/// statement cut a moment early would look like an ordinary error).
pub(super) const TIMEOUT_MARGIN_MS: u64 = 100;

/// Whether the server timeout must be set again: it is lowered as the
/// deadline approaches, so a statement never runs much past the time limit.
pub(super) fn needs_new_timeout(current: Option<u64>, ms: u64) -> bool {
    current.is_none_or(|t| ms < t * 3 / 4)
}
