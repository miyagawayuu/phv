//! Work started by the `*Async` methods of std.net and std.db, and waited for
//! with `await()` (std.async).
//!
//! The work runs on its own thread, so the script goes on meanwhile. Values
//! cannot leave the script's thread, so the work returns plain data (an HTTP
//! response, rows, a Redis reply), which becomes values when it is awaited.
//! Memory used by the work is not counted by the allocator of the script's
//! thread: each kind of work has a byte budget instead, taken from the memory
//! left to the script when it starts.

use crate::{int, R};
use phv_vm::{Value, Vm};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Work one script may have running at once.
const MAX_PER_SCRIPT: usize = 16;

/// Work running in the whole process (each holds a thread).
const MAX_RUNNING: usize = 256;
static RUNNING: AtomicUsize = AtomicUsize::new(0);

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
    },
    /// The result, kept so that awaiting again gives it again.
    Done(Result<Value, (&'static str, String)>),
}

/// Runs `work` on its own thread and returns the handle `await()` takes.
/// The work must end by `until` on its own; waiting stops there too.
pub(crate) fn start(vm: &mut Vm, until: Instant, work: impl FnOnce() -> Outcome + Send + 'static) -> R {
    let running =
        vm.resources.iter().flatten().filter(|r| matches!(r.downcast_ref(), Some(Task::Running { .. }))).count();
    if running >= MAX_PER_SCRIPT {
        return Err(vm.throw(
            "RuntimeException",
            format!("at most {MAX_PER_SCRIPT} operations can run at once; await some of them first"),
        ));
    }
    if RUNNING.fetch_add(1, Ordering::SeqCst) >= MAX_RUNNING {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        return Err(vm.throw("RuntimeException", "the server is running too many background operations"));
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new().name("phv-async".into()).spawn(move || {
        let _ = tx.send(work());
        RUNNING.fetch_sub(1, Ordering::SeqCst);
    });
    if spawned.is_err() {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        return Err(vm.throw("RuntimeException", "cannot start a background operation"));
    }
    vm.resources.push(Some(Box::new(Task::Running { rx, until })));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
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
    let result = match task {
        Task::Done(r) => r,
        Task::Running { rx, until } => {
            // A little past the work's own deadline, so its error arrives first.
            let left = until.saturating_duration_since(Instant::now()) + Duration::from_secs(1);
            match rx.recv_timeout(left) {
                Ok(outcome) => convert(outcome),
                Err(_) => {
                    vm.check_limits()?;
                    Err(("NetworkError", "the operation timed out".to_string()))
                }
            }
        }
    };
    vm.resources[i as usize] = Some(Box::new(Task::Done(result.clone())));
    vm.check_limits()?;
    match result {
        Ok(v) => Ok(v),
        Err((class, msg)) => Err(vm.throw(class, msg)),
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
