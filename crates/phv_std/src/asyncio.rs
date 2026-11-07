//! Work started by the `*Async` methods of std.net and std.db, and waited for
//! with `await()` (std.async).
//!
//! The work runs on a thread of the process's worker pool (`workers`), so the
//! script goes on meanwhile. Values cannot leave the script's thread, so the
//! work returns plain data (an HTTP response, rows, a Redis reply), which
//! becomes values when it is awaited.
//! Memory used by the work is not counted by the allocator of the script's
//! thread: each kind of work has a byte budget instead, taken from the memory
//! left to the script when it starts.
//!
//! Work that is never awaited is still waited for when the script ends (until
//! its deadline at most), so a request cannot leave work running after it:
//! the work keeps the request's worker, and its connection, busy until done.

use crate::{int, R};
use phv_vm::{Value, Vm};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Work one script may have running at once.
const MAX_PER_SCRIPT: usize = 16;

/// Work running in the whole process (each holds a thread of the pool).
pub(crate) const MAX_RUNNING: usize = 1024;
static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// Operations of `*Async` methods running now, in the process (for `/metrics`).
pub fn running() -> usize {
    RUNNING.load(Ordering::Relaxed)
}

/// What background work produced, in data that can cross threads.
pub(crate) enum Outcome {
    Http(Result<crate::httpclient::Response, String>),
    Db(Result<crate::netdb::Detached, String>),
    Redis(Result<crate::redisclient::Reply, String>),
}

enum Task {
    Running {
        rx: mpsc::Receiver<Outcome>,
        until: Instant,
        /// The database whose pool the work takes a connection from.
        pool: Option<String>,
    },
    /// The result, kept so that awaiting again gives it again.
    Done(Result<Value, (&'static str, String)>),
}

impl Drop for Task {
    /// Work dropped unawaited (when the script ends) is waited for.
    fn drop(&mut self) {
        if let Task::Running { rx, until, .. } = self {
            let left = until.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                let _ = rx.recv_timeout(left + Duration::from_secs(1));
            }
        }
    }
}

/// Runs `work` on a thread of the pool and returns the handle `await()` takes.
/// The work must end by `until` on its own; waiting stops there too.
/// `pool` names the database (and the size of its pool) the work takes a
/// connection from: a script may use at most half of a pool at once.
pub(crate) fn start(
    vm: &mut Vm,
    until: Instant,
    pool: Option<(String, usize)>,
    work: impl FnOnce() -> Outcome + Send + 'static,
) -> R {
    let running: Vec<&Option<String>> = vm
        .resources
        .iter()
        .flatten()
        .filter_map(|r| match r.downcast_ref() {
            Some(Task::Running { pool, .. }) => Some(pool),
            _ => None,
        })
        .collect();
    if running.len() >= MAX_PER_SCRIPT {
        return Err(vm.throw(
            "RuntimeException",
            format!("at most {MAX_PER_SCRIPT} operations can run at once; await some of them first"),
        ));
    }
    if let Some((name, size)) = &pool {
        let max = (size / 2).max(1);
        if running.iter().filter(|p| p.as_ref() == Some(name)).count() >= max {
            return Err(vm.throw(
                "RuntimeException",
                format!("at most {max} operations on the database \"{name}\" can run at once (half its pool); await some of them first"),
            ));
        }
    }
    if RUNNING.fetch_add(1, Ordering::SeqCst) >= MAX_RUNNING {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        return Err(vm.throw("RuntimeException", "the server is running too many background operations"));
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = crate::workers::spawn(move || {
        // Counted until the work ends, even if it panics.
        let _running = Running;
        let _ = tx.send(work());
    });
    if spawned.is_err() {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        return Err(vm.throw("RuntimeException", "cannot start a background operation"));
    }
    vm.resources.push(Some(Box::new(Task::Running { rx, until, pool: pool.map(|p| p.0) })));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
}

/// One unit of `RUNNING`, given back when dropped.
struct Running;

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A handle for work that is already done (a SQLite statement, which runs in
/// the script's thread: its errors are thrown when it is started).
pub(crate) fn ready(vm: &mut Vm, r: R) -> R {
    let v = r?;
    vm.resources.push(Some(Box::new(Task::Done(Ok(v)))));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
}

/// `__async_any(handle)` (and its typed forms): waits for the work and returns
/// its result, or throws its error.
pub fn wait(vm: &mut Vm, a: &[Value]) -> R {
    let i = int(&a[0]);
    let slot = usize::try_from(i).ok().and_then(|i| vm.resources.get_mut(i));
    let task = match slot.and_then(|s| s.take()).map(|b| b.downcast::<Task>()) {
        Some(Ok(t)) => *t,
        Some(Err(other)) => {
            vm.resources[i as usize] = Some(other);
            return Err(vm.throw("ValueError", "not a background operation"));
        }
        None => return Err(vm.throw("ValueError", "not a background operation")),
    };
    let result = match &task {
        Task::Done(r) => r.clone(),
        Task::Running { rx, until, .. } => {
            // A little past the work's own deadline, so its error arrives first.
            let left = until.saturating_duration_since(Instant::now()) + Duration::from_secs(1);
            match rx.recv_timeout(left) {
                Ok(outcome) => {
                    // Allocated on the work's thread, freed on this one: counted here.
                    phv_vm::alloc::charge_thread(outcome_size(&outcome));
                    convert(outcome)
                }
                Err(_) => {
                    vm.check_limits()?;
                    Err(("NetworkError", crate::netio::TIME_LIMIT.to_string()))
                }
            }
        }
    };
    // Done, or waited for past its deadline: dropping it waits no longer.
    let mut task = task;
    if let Task::Running { until, .. } = &mut task {
        *until = Instant::now();
    }
    drop(task);
    // The `Future` keeps the result; this handle keeps none (it would hold every
    // response of the run until the end).
    vm.resources[i as usize] = Some(Box::new(Task::Done(Err(("ValueError", "already awaited".to_string())))));
    vm.check_limits()?;
    match result {
        Ok(v) => Ok(v),
        Err((class, msg)) => Err(vm.throw(class, msg)),
    }
}

/// The bytes an outcome holds on the heap.
fn outcome_size(outcome: &Outcome) -> usize {
    match outcome {
        Outcome::Http(Ok(r)) => r.heap_size(),
        Outcome::Db(Ok(d)) => d.heap_size(),
        Outcome::Redis(Ok(r)) => r.heap_size(),
        Outcome::Http(Err(e)) | Outcome::Db(Err(e)) | Outcome::Redis(Err(e)) => e.capacity(),
    }
}

fn convert(outcome: Outcome) -> Result<Value, (&'static str, String)> {
    match outcome {
        Outcome::Http(r) => r.map(crate::httpclient::response_value).map_err(|e| ("NetworkError", e)),
        Outcome::Db(r) => r.map(crate::netdb::detached_value).map_err(|e| ("DatabaseError", e)),
        Outcome::Redis(r) => r.map(crate::netdb::redis_value).map_err(|e| ("DatabaseError", e)),
    }
}

pub fn lookup(name: &str) -> Option<phv_vm::BuiltinFn> {
    let f: phv_vm::BuiltinFn = match name {
        "__async_any" | "__async_map" | "__async_rows" | "__async_int" => wait,
        _ => return None,
    };
    Some(f)
}
