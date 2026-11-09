// ----- Pool ------------------------------------------------------------------

use super::my::MyConn;
use super::pg::PgConn;
use super::CHECK_AFTER;
use crate::redisclient::RedisConn;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::Instant;

pub(super) trait Driver: Send + Sized + 'static {
    /// Returns the session to its initial state; false when the connection cannot be reused.
    fn reset(&mut self) -> bool;
    /// Whether a connection that sat idle still works.
    fn check(&mut self) -> bool;
    /// Whether the connection is known to be unusable, checked without a
    /// round trip: closed, or with bytes waiting that no command asked for
    /// (the next command would read them as its reply).
    fn closed(&mut self) -> bool {
        false
    }
}

pub(super) struct Pool<C> {
    state: Mutex<PoolState<C>>,
    freed: Condvar,
    max: usize,
}

struct PoolState<C> {
    idle: Vec<(C, Instant)>,
    /// Connections in use or idle.
    open: usize,
}

pub(super) type Registry<C> = LazyLock<Mutex<HashMap<String, Arc<Pool<C>>>>>;

pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn pool_for<C: Driver>(registry: &Registry<C>, key: &str, max: usize) -> Arc<Pool<C>> {
    lock(registry)
        .entry(key.to_string())
        .or_insert_with(|| {
            Arc::new(Pool { state: Mutex::new(PoolState { idle: Vec::new(), open: 0 }), freed: Condvar::new(), max })
        })
        .clone()
}

pub(super) enum AcquireError {
    Busy,
    Connect(String),
}

impl<C: Driver> Pool<C> {
    pub(super) fn acquire(
        self: &Arc<Self>,
        until: Instant,
        connect: impl FnOnce() -> Result<C, String>,
    ) -> Result<Pooled<C>, AcquireError> {
        let mut st = lock(&self.state);
        loop {
            if let Some((mut c, since)) = st.idle.pop() {
                drop(st);
                if !c.closed() && (since.elapsed() < CHECK_AFTER || c.check()) {
                    return Ok(Pooled { conn: Some(c), pool: self.clone(), broken: false, depth: 0, ended: None });
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
                    Ok(c) => Ok(Pooled { conn: Some(c), pool: self.clone(), broken: false, depth: 0, ended: None }),
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
pub(super) struct Pooled<C: Driver> {
    conn: Option<C>,
    pool: Arc<Pool<C>>,
    /// Set after a network error: the connection is closed instead of reused.
    pub(super) broken: bool,
    /// How many `transaction()` calls are open (inner ones are savepoints).
    pub(super) depth: u32,
    /// Why the database ended the transaction of the open `transaction()`
    /// calls before they did (`crate::db::Ended`): their statements are then
    /// refused, and their `COMMIT` raises.
    pub(super) ended: Option<crate::db::Ended>,
}

impl<C: Driver> Pooled<C> {
    pub(super) fn get(&mut self) -> &mut C {
        self.conn.as_mut().expect("pooled connection")
    }

    /// The connection, unless a `renew` failed to replace it.
    pub(super) fn conn(&mut self) -> Option<&mut C> {
        self.conn.as_mut()
    }

    /// Closes the connection and takes another from the pool (a new one when
    /// none is idle), waiting until `until` for one: for a connection found
    /// unusable (`Driver::closed`) before anything was sent on it. When that
    /// fails, none is held (`conn` is `None`) until a later call succeeds.
    pub(super) fn renew(
        &mut self,
        until: Instant,
        connect: impl FnOnce() -> Result<C, String>,
    ) -> Result<(), AcquireError> {
        if let Some(c) = self.conn.take() {
            drop(c);
            self.pool.discard();
        }
        self.broken = false;
        let mut fresh = self.pool.acquire(until, connect)?;
        self.conn = fresh.conn.take();
        Ok(())
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

pub(super) static PG_POOLS: Registry<PgConn> = LazyLock::new(Default::default);
pub(super) static MY_POOLS: Registry<MyConn> = LazyLock::new(Default::default);
pub(super) static REDIS_POOLS: Registry<RedisConn> = LazyLock::new(Default::default);
