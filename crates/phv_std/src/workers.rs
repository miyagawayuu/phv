//! Threads shared by the background work of the whole process: `*Async`
//! operations (`asyncio`), name lookups and MySQL connects that are given up
//! on at a deadline (`netio::with_deadline`), and `KILL QUERY` for MySQL
//! statements past their deadline.
//!
//! A job never waits for a thread: when no thread is idle, a new one is
//! started, up to `MAX_THREADS`. Every kind of job is counted against a cap of
//! its own (`asyncio::MAX_RUNNING`, `netio::MAX_PENDING`, ...) before it is
//! sent here, and `MAX_THREADS` is the sum of those caps, so the cap here is
//! only reached for the moment a finished job takes to return its thread.
//! A thread that has been idle for `IDLE` ends.

use std::collections::VecDeque;
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

/// The caps of the kinds of work that run here, added up.
pub(crate) const MAX_THREADS: usize =
    crate::asyncio::MAX_RUNNING + 2 * crate::netio::MAX_PENDING + crate::netdb::MAX_KILLING;

/// An idle thread ends after this long.
const IDLE: Duration = Duration::from_secs(5);

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    /// Threads waiting for a job.
    idle: usize,
    /// Threads alive (running a job or idle).
    threads: usize,
}

struct Pool {
    state: Mutex<State>,
    work: Condvar,
}

static POOL: LazyLock<Pool> = LazyLock::new(|| Pool { state: Mutex::new(State::default()), work: Condvar::new() });

fn lock() -> MutexGuard<'static, State> {
    POOL.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `job` on a thread of the pool, at once. Fails only when no thread
/// could be started for it.
pub(crate) fn spawn(job: impl FnOnce() + Send + 'static) -> Result<(), ()> {
    let mut st = lock();
    st.queue.push_back(Box::new(job));
    // Idle threads that were woken but have not taken a job yet still count
    // as idle, so each job in the queue has a thread of its own.
    if st.idle >= st.queue.len() || st.threads >= MAX_THREADS {
        POOL.work.notify_one();
        return Ok(());
    }
    st.threads += 1;
    let started = std::thread::Builder::new().name("phv-worker".into()).spawn(worker);
    if started.is_err() {
        st.threads -= 1;
        st.queue.pop_back();
        return Err(());
    }
    Ok(())
}

fn worker() {
    let mut st = lock();
    loop {
        if let Some(job) = st.queue.pop_front() {
            drop(st);
            // A job that panics loses its own result, not the thread.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            st = lock();
            continue;
        }
        st.idle += 1;
        let (guard, wait) = POOL.work.wait_timeout(st, IDLE).unwrap_or_else(|e| e.into_inner());
        st = guard;
        st.idle -= 1;
        if wait.timed_out() && st.queue.is_empty() {
            st.threads -= 1;
            return;
        }
    }
}

/// Threads alive in the pool, and how many of them are idle.
#[cfg(test)]
pub(crate) fn counts() -> (usize, usize) {
    let st = lock();
    (st.threads, st.idle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Instant;

    /// Jobs that wait for each other all run at once (none waits in the
    /// queue for another to end), and the threads are reused afterwards.
    #[test]
    fn jobs_run_at_once_and_threads_are_reused() {
        let n = 24;
        let barrier = Arc::new(Barrier::new(n + 1));
        for _ in 0..n {
            let b = barrier.clone();
            spawn(move || {
                b.wait();
            })
            .unwrap();
        }
        // Every job reached the barrier: they ran at the same time.
        barrier.wait();
        let ids = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let done = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        for _ in 0..200 {
            let (ids, done, tx) = (ids.clone(), done.clone(), tx.clone());
            spawn(move || {
                ids.lock().unwrap().insert(std::thread::current().id());
                if done.fetch_add(1, Ordering::SeqCst) + 1 == 200 {
                    tx.send(()).unwrap();
                }
            })
            .unwrap();
        }
        rx.recv_timeout(Duration::from_secs(20)).unwrap();
        // 200 short jobs one after another need far fewer than 200 threads
        // (other tests may run jobs here at the same time).
        assert!(ids.lock().unwrap().len() < 150, "{} threads", ids.lock().unwrap().len());
        let start = Instant::now();
        while counts().1 < counts().0 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A job that panics leaves the pool working.
    #[test]
    fn a_panicking_job_does_not_break_the_pool() {
        spawn(|| panic!("job panics on purpose")).unwrap();
        let (tx, rx) = mpsc::channel();
        spawn(move || tx.send(7).unwrap()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 7);
    }
}
