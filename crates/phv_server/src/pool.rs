//! The threads pages run on.
//!
//! A page runs on a thread of its own while the request waits for it (a
//! script may take CPU time and blocks while it does). Tokio's blocking pool
//! hands its threads work through one queue under one lock, and wakes
//! whichever idle thread the condition variable picks: measured with a
//! trivial page, that took 17 µs at the median and 30 µs on average, about a
//! tenth of the request. Here each thread waits on a slot of its own, and a
//! page goes to the thread that became idle last (its core is the least likely
//! to have gone to sleep), without a lock held while it is woken: about 8 µs.
//!
//! Nothing else changes: a thread runs one page at a time, from its start to
//! its teardown (the VM is dropped on the thread that ran it, as before), and
//! is then given the next one. Threads are made as they are needed, at most one
//! per worker (`[server] workers`): a thread is idle again before the worker it
//! ran for is free. A thread idle for `KEEP_ALIVE` ends, as tokio's do.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;

/// How long an idle thread waits for a page before it ends (tokio's blocking
/// threads wait as long).
const KEEP_ALIVE: Duration = Duration::from_secs(10);

/// The stack of a thread (as the runtime's threads: room for deep recursion).
pub(crate) const STACK: usize = 16 * 1024 * 1024;

/// The name of the threads: the one tokio's threads have, so the lines a panic
/// writes on standard error stay as they were.
const NAME: &str = "tokio-runtime-worker";

type Job = Box<dyn FnOnce() + Send>;

/// What a thread is given: a page to run (with the worker it holds), or the
/// end (the server stopped).
enum Work {
    Page(Job, OwnedSemaphorePermit),
    Stop,
}

/// Where an idle thread waits for its next page.
struct Slot {
    work: Mutex<Option<Work>>,
    ready: Condvar,
}

pub(crate) struct Pool {
    shared: Arc<Shared>,
}

struct Shared {
    /// The idle threads, the one idle last at the end; `None` once the pool
    /// is closed.
    idle: Mutex<Option<Vec<Arc<Slot>>>>,
    /// Entered on every thread, as on tokio's.
    runtime: Mutex<Option<tokio::runtime::Handle>>,
}

impl Pool {
    pub(crate) fn new() -> Pool {
        Pool { shared: Arc::new(Shared { idle: Mutex::new(Some(Vec::new())), runtime: Mutex::new(None) }) }
    }

    /// Runs `job` on a thread of the pool, holding `permit` (a worker) until
    /// the thread is idle again. Called from the runtime.
    pub(crate) fn run(&self, permit: OwnedSemaphorePermit, job: impl FnOnce() + Send + 'static) {
        let work = Work::Page(Box::new(job), permit);
        let slot = self.shared.idle.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(Vec::pop);
        let Some(slot) = slot else { return self.spawn(work) };
        *slot.work.lock().unwrap_or_else(|e| e.into_inner()) = Some(work);
        slot.ready.notify_one();
    }

    /// A new thread, starting with `work`. When none can be made, the page
    /// runs on tokio's blocking pool instead.
    fn spawn(&self, work: Work) {
        let runtime = {
            let mut r = self.shared.runtime.lock().unwrap_or_else(|e| e.into_inner());
            r.get_or_insert_with(tokio::runtime::Handle::current).clone()
        };
        let slot = Arc::new(Slot { work: Mutex::new(Some(work)), ready: Condvar::new() });
        let (shared, mine) = (self.shared.clone(), slot.clone());
        let started = std::thread::Builder::new().name(NAME.into()).stack_size(STACK).spawn({
            let runtime = runtime.clone();
            move || {
                let _entered = runtime.enter();
                serve(&shared, &mine)
            }
        });
        if started.is_err() {
            let work = slot.work.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(Work::Page(job, permit)) = work {
                runtime.spawn_blocking(move || {
                    job();
                    drop(permit);
                });
            }
        }
    }

    /// Ends the idle threads; the busy ones end after their page.
    pub(crate) fn close(&self) {
        let idle = self.shared.idle.lock().unwrap_or_else(|e| e.into_inner()).take();
        for slot in idle.into_iter().flatten() {
            *slot.work.lock().unwrap_or_else(|e| e.into_inner()) = Some(Work::Stop);
            slot.ready.notify_one();
        }
    }
}

/// A thread of the pool: runs the pages it is given until it has been idle
/// for `KEEP_ALIVE` or the pool is closed.
fn serve(shared: &Shared, slot: &Arc<Slot>) {
    loop {
        let work = {
            let mut work = slot.work.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(w) = work.take() {
                    break w;
                }
                let (w, waited) = slot.ready.wait_timeout(work, KEEP_ALIVE).unwrap_or_else(|e| e.into_inner());
                work = w;
                if waited.timed_out() && work.is_none() {
                    // Ends unless it was just given a page (then it is no
                    // longer in the list, and the page is on its way).
                    let mut idle = shared.idle.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(list) = idle.as_mut() {
                        if let Some(i) = list.iter().position(|s| Arc::ptr_eq(s, slot)) {
                            list.remove(i);
                            return;
                        }
                    }
                }
            }
        };
        let Work::Page(job, permit) = work else { return };
        // A page that panics ends here (`job` reports it), not the thread.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
        // Idle again before the worker is free: no more threads than workers.
        let open = match shared.idle.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            Some(list) => {
                list.push(slot.clone());
                true
            }
            None => false,
        };
        drop(permit);
        if !open {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Pages run one at a time on each thread, no more threads than workers
    /// are made, and an idle thread is reused.
    #[test]
    fn threads_are_reused_and_bounded_by_the_workers() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).build().unwrap();
        let workers = Arc::new(tokio::sync::Semaphore::new(3));
        let pool = Arc::new(Pool::new());
        let threads = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        rt.block_on(async {
            let mut done = Vec::new();
            for _ in 0..200 {
                let permit = workers.clone().acquire_owned().await.unwrap();
                let (tx, rx) = tokio::sync::oneshot::channel();
                let (threads, running, most) = (threads.clone(), running.clone(), most.clone());
                pool.run(permit, move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    threads.lock().unwrap().insert(std::thread::current().id());
                    assert_eq!(std::thread::current().name(), Some(NAME));
                    // The runtime is entered, as on tokio's threads.
                    assert!(tokio::runtime::Handle::try_current().is_ok());
                    std::thread::sleep(Duration::from_micros(200));
                    running.fetch_sub(1, Ordering::SeqCst);
                    let _ = tx.send(());
                });
                done.push(rx);
            }
            for rx in done {
                rx.await.unwrap();
            }
        });
        assert!(most.load(Ordering::SeqCst) <= 3);
        let made = threads.lock().unwrap().len();
        assert!((1..=3).contains(&made), "{made} threads for 3 workers");
        pool.close();
    }

    /// A page that panics does not end its thread, and its worker is freed.
    #[test]
    fn a_panic_frees_the_worker_and_keeps_the_thread() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).build().unwrap();
        let workers = Arc::new(tokio::sync::Semaphore::new(1));
        let pool = Pool::new();
        rt.block_on(async {
            let permit = workers.clone().acquire_owned().await.unwrap();
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            pool.run(permit, move || {
                let _tx = tx;
                panic!("on purpose");
            });
            // The sender went with the panic.
            assert!(rx.await.is_err());
            let permit = workers.clone().acquire_owned().await.unwrap();
            let (tx, rx) = tokio::sync::oneshot::channel();
            pool.run(permit, move || {
                let _ = tx.send(std::thread::current().id());
            });
            rx.await.unwrap();
            // The response can arrive before the thread returns to the idle list.
            let _permit = workers.acquire().await.unwrap();
            assert_eq!(pool.shared.idle.lock().unwrap().as_ref().map(Vec::len), Some(1));
        });
        pool.close();
    }
}
