//! Runs background jobs (`[jobs]`): `[jobs] workers` threads take jobs from
//! the store and run their scripts, outside any request; a scheduler thread
//! adds the jobs of `[[schedule]]` and cleans the store up once a minute.
//!
//! A job runs with `[jobs] time_limit` and `[security] memory_limit_mb`, and
//! its threads are not the page workers: jobs never take a worker from a
//! request. A job that throws (or hits a limit) runs again after 30 s, 60 s,
//! 2 min, ... (at most an hour), until `max_attempts` runs; then it is given up
//! and kept a week for `phv jobs list --failed` and `phv jobs retry`.

use crate::metrics::StoreKind;
use crate::State;
use phv_driver::{run, RunOptions};
use phv_std::jobs::{Row, Store};
use phv_syntax::diag::log_escape;
use phv_vm::{HttpCtx, JobCtx, Limits};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The longest error text kept with a job.
const MAX_ERROR: usize = 2000;
/// The longest wait before running a failed job again.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(3600);
/// A runner looks at the store at least this often (jobs added by other
/// processes; those added in this one wake it at once).
const POLL: Duration = Duration::from_secs(1);

pub(crate) struct Runner {
    stop: Arc<AtomicBool>,
    /// Jobs running now.
    running: Arc<AtomicUsize>,
}

impl Runner {
    /// Asks the threads to stop once their job ends.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Waits until no job runs, for at most `wait`; whether they all ended.
    pub fn wait(&self, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        while self.running.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

/// The wait before run `attempts + 1` of a failed job: `first`, doubled each time.
pub(crate) fn retry_wait(first: Duration, attempts: u32) -> Duration {
    first.saturating_mul(1 << attempts.saturating_sub(1).min(16)).min(MAX_RETRY_WAIT)
}

/// Compiles every job (an error stops the server, as a route error does) and
/// starts the threads.
pub(crate) fn start(state: &Arc<State>, serving: bool) -> Result<Option<Runner>, String> {
    let Some(cfg) = state.config.jobs.clone() else { return Ok(None) };
    for name in state.job_names() {
        state.job_compiled(&name).map_err(|e| format!("job {}/{name}.phv: {e}", cfg.dir))?;
    }
    // `run = false` keeps a serving process from running jobs; `phv jobs run`
    // and `--jobs-only` run them all the same (found by the 1.1 audit).
    if !cfg.run && serving {
        return Ok(None);
    }
    let Some(store) = Store::of(&state.config.host) else {
        return Err("[jobs] store: the database is not in [databases]".into());
    };
    let runner = Runner { stop: Arc::new(AtomicBool::new(false)), running: Arc::new(AtomicUsize::new(0)) };
    for i in 0..cfg.workers {
        let (state, store, stop, running) = (state.clone(), store.clone(), runner.stop.clone(), runner.running.clone());
        std::thread::Builder::new()
            .name(format!("phv-job-{i}"))
            .stack_size(16 << 20)
            .spawn(move || work(&state, &store, &stop, &running))
            .map_err(|e| format!("cannot start a job thread: {e}"))?;
    }
    let (state, stop) = (state.clone(), runner.stop.clone());
    std::thread::Builder::new()
        .name("phv-schedule".into())
        .spawn(move || schedule(&state, &store, &stop))
        .map_err(|e| format!("cannot start the scheduler: {e}"))?;
    Ok(Some(runner))
}

fn work(state: &State, store: &Store, stop: &AtomicBool, running: &AtomicUsize) {
    let cfg = state.config.jobs.as_ref().expect("checked by start");
    let lease = Duration::from_secs(cfg.time_limit + 30);
    let mut backoff = Duration::from_secs(1);
    let mut warned = false;
    while !stop.load(Ordering::SeqCst) {
        running.fetch_add(1, Ordering::SeqCst);
        let taken = store.take(lease);
        match taken {
            Ok(Some(job)) => {
                state.metrics.health.ok(StoreKind::Jobs);
                warned = false;
                backoff = Duration::from_secs(1);
                run_one(state, store, &job, cfg.max_attempts, Duration::from_secs(cfg.retry_delay));
                running.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(None) => {
                running.fetch_sub(1, Ordering::SeqCst);
                state.metrics.health.ok(StoreKind::Jobs);
                store.wait_for_jobs(POLL);
            }
            Err(e) => {
                running.fetch_sub(1, Ordering::SeqCst);
                state.metrics.health.failed(StoreKind::Jobs);
                if !warned {
                    eprintln!("warning: the job store cannot be used ({e}); trying again");
                    warned = true;
                }
                let until = Instant::now() + backoff;
                while !stop.load(Ordering::SeqCst) && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(100));
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Runs one job taken from the store and records how it ended.
fn run_one(state: &State, store: &Store, job: &Row, max_attempts: u32, retry_delay: Duration) {
    let m = &state.metrics;
    // Taken again after its lease ran out once too often (its runs kept stopping the process).
    let outcome = if job.attempts > max_attempts {
        Err(format!("given up after {} runs that did not end", job.attempts - 1))
    } else {
        let started = Instant::now();
        let r = state.run_job(job);
        m.job_duration.observe(started.elapsed());
        r
    };
    let end = match &outcome {
        Ok(()) => {
            m.jobs_done.fetch_add(1, Ordering::Relaxed);
            phv_std::jobs::End::Done { keyed: true }
        }
        Err(error) if job.attempts < max_attempts => {
            m.jobs_failed.fetch_add(1, Ordering::Relaxed);
            let at = (SystemTime::now() + retry_wait(retry_delay, job.attempts))
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            phv_std::jobs::End::Retry { at, error }
        }
        Err(error) => {
            m.jobs_dead.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "job {} ({}) was given up after {} runs",
                log_escape(&job.name),
                log_escape(&job.id),
                job.attempts
            );
            phv_std::jobs::End::Dead { error }
        }
    };
    match store.end(job, end) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "warning: job {} ({}) ran past its lease; another run may have taken it",
                log_escape(&job.name),
                log_escape(&job.id)
            )
        }
        Err(e) => {
            m.health.failed(StoreKind::Jobs);
            eprintln!(
                "warning: job {} ({}): its end could not be recorded ({})",
                log_escape(&job.name),
                log_escape(&job.id),
                log_escape(&e.to_string())
            );
        }
    }
}

impl State {
    /// The jobs of the project (the scripts of `[jobs] dir`), sorted.
    pub(crate) fn job_names(&self) -> Vec<String> {
        if let Some(e) = &self.embedded {
            let mut names: Vec<String> = e.jobs.keys().cloned().collect();
            names.sort();
            return names;
        }
        let dir = self.config.jobs.as_ref().map_or("jobs", |j| j.dir.as_str());
        phv_driver::config::scan_jobs(&self.root, dir).unwrap_or_default()
    }

    pub(crate) fn job_compiled(&self, name: &str) -> Result<Arc<phv_driver::Compiled>, String> {
        if !phv_std::jobs::job_name(name) {
            return Err(format!("there is no job \"{}\"", name.escape_debug()));
        }
        if let Some(e) = &self.embedded {
            return e.jobs.get(name).cloned().ok_or_else(|| format!("there is no job \"{name}\""));
        }
        let dir = self.config.jobs.as_ref().map_or("jobs", |j| j.dir.as_str());
        if phv_driver::config::jobs_dir_is_link(&self.root, dir) {
            return Err(format!("{dir} is a link; the jobs must be in the project"));
        }
        let script = self.root.join(dir).join(format!("{name}.phv"));
        // Only a plain file of the directory: no link leads elsewhere.
        match std::fs::symlink_metadata(&script) {
            Ok(m) if m.is_file() => {}
            _ => return Err(format!("there is no job {dir}/{name}.phv")),
        }
        self.compiled(&script)
    }

    /// Runs a job's script; the error text if it failed.
    fn run_job(&self, job: &Row) -> Result<(), String> {
        let cfg = self.config.jobs.as_ref().expect("jobs are configured");
        let compiled = self.job_compiled(&job.name)?;
        let limits = Limits {
            max_time: Some(Duration::from_secs(cfg.time_limit)),
            max_memory: Some(self.config.memory_limit_mb as usize * 1024 * 1024),
            ..Limits::default()
        };
        // [telemetry]: each run is a trace of its own.
        let trace = self.telemetry.as_ref().map(|e| phv_vm::TraceCtx {
            trace_id: phv_std::random_hex(16),
            span_id: phv_std::random_hex(8),
            sampled: e.sample(),
            spans: Default::default(),
        });
        let ctx = HttpCtx {
            can_publish: true,
            trace: trace.clone(),
            job: Some(JobCtx {
                name: job.name.clone(),
                id: job.id.clone(),
                payload: job.payload.clone(),
                attempt: job.attempts,
            }),
            ..HttpCtx::default()
        };
        let host = Some(self.config.host.clone());
        let started = std::time::SystemTime::now();
        let mut outcome = run(&compiled, RunOptions { html: false, limits, http: Some(ctx), host });
        if let (Some(exporter), Some(t)) = (&self.telemetry, &trace) {
            let mut spans = std::mem::take(&mut *t.spans.lock().unwrap_or_else(|e| e.into_inner()));
            if t.sampled {
                let mut attributes = vec![
                    ("phv.job.name", phv_vm::SpanValue::Str(job.name.clone())),
                    ("phv.job.id", phv_vm::SpanValue::Str(job.id.clone())),
                    ("phv.job.attempt", phv_vm::SpanValue::Int(job.attempts as i64)),
                ];
                if outcome.error.is_some() {
                    attributes.push(("error.type", phv_vm::SpanValue::Str("exception".into())));
                }
                spans.push(phv_vm::Span {
                    trace_id: t.trace_id.clone(),
                    span_id: t.span_id.clone(),
                    parent_id: None,
                    name: format!("job {}", job.name),
                    kind: phv_vm::SpanKind::Consumer,
                    start: started,
                    end: std::time::SystemTime::now(),
                    attributes,
                    error: outcome.error.is_some(),
                });
            }
            exporter.send(spans);
        }
        if let Some(err) = &outcome.error {
            self.metrics.failure(crate::metrics::Failure::of(&err.message));
            let text = err.render();
            eprintln!(
                "job {} ({}) failed (run {}):\n{}",
                log_escape(&job.name),
                log_escape(&job.id),
                job.attempts,
                err.render_for_log()
            );
            let mut cut = text.len().min(MAX_ERROR);
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            return Err(text[..cut].to_string());
        }
        // What the job published (only when it succeeded, as for pages).
        let publishes = std::mem::take(&mut outcome.http.publishes);
        self.deliver(&publishes);
        Ok(())
    }
}

/// A minute of a schedule whose job could not be added is tried again this
/// often, for this long at most.
const SCHEDULE_RETRY: Duration = Duration::from_secs(5);
const SCHEDULE_RETRY_FOR: i64 = 3600;

/// Adds the jobs of `[[schedule]]` when their minute comes, and cleans the
/// store up once a minute. Every process does: on a shared store, the key of
/// each minute adds the job once.
///
/// A minute whose job could not be added (the store did not answer in time)
/// is tried again a few seconds later, for an hour at most, with the minutes
/// after it (found by the 1.2 audit: such a minute was lost).
fn schedule(state: &State, store: &Store, stop: &AtomicBool) {
    let mut last = now_secs();
    let mut swept = Instant::now();
    // The first minute that failed, while it is tried again, and when.
    let mut failing: Option<i64> = None;
    let mut retry_at = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));
        let now = now_secs();
        if now == last || Instant::now() < retry_at {
            continue;
        }
        let failed = add_scheduled(&state.config.schedules, store, last, now);
        match failed.iter().map(|(minute, _)| *minute).min() {
            Some(first) => {
                state.metrics.health.failed(StoreKind::Jobs);
                // Once for each minute that fails, not at every try.
                if failing != Some(first) {
                    for (_, e) in &failed {
                        eprintln!("warning: {e}; trying again");
                    }
                }
                failing = Some(first);
                retry_at = Instant::now() + SCHEDULE_RETRY;
            }
            None => failing = None,
        }
        last = looked_until(last, now, failing);
        if swept.elapsed() >= Duration::from_secs(60) {
            swept = Instant::now();
            if let Err(e) = store.sweep() {
                state.metrics.health.failed(StoreKind::Jobs);
                eprintln!("warning: the job store could not be cleaned up ({e})");
            }
        }
    }
}

/// Where the next look at the schedule starts after one from `last` to `now`
/// in which the minute `failed` (the first) could not be added: just before
/// it, for an hour at most. A clock set back is not run again; one set forward
/// runs the minutes it skipped (at most 60).
fn looked_until(last: i64, now: i64, failed: Option<i64>) -> i64 {
    match failed {
        Some(first) => (first - 1).max(now - SCHEDULE_RETRY_FOR).max(last),
        None => now.max(last),
    }
}

/// Adds the jobs of `schedules` whose minute is after `from` and at most `to`
/// (Unix seconds; at most 60 minutes of each), keyed by the minute: on a shared
/// store, every process adding them adds each job once. The minutes whose
/// job could not be added, with the errors.
pub(crate) fn add_scheduled(
    schedules: &[phv_driver::config::Schedule],
    store: &Store,
    from: i64,
    to: i64,
) -> Vec<(i64, String)> {
    let mut errors = Vec::new();
    for s in schedules {
        for minute in s.cron.due(from, to, 60) {
            let key = phv_std::jobs::schedule_key(&s.job, minute);
            if let Err(e) = store.add_scheduled(&s.job, &key) {
                errors.push((minute, format!("the scheduled job {} could not be added ({e})", s.job)));
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_wait_longer_each_time() {
        let first = Duration::from_secs(30);
        assert_eq!(retry_wait(first, 1), Duration::from_secs(30));
        assert_eq!(retry_wait(first, 2), Duration::from_secs(60));
        assert_eq!(retry_wait(first, 4), Duration::from_secs(240));
        assert_eq!(retry_wait(first, 10), MAX_RETRY_WAIT);
        assert_eq!(retry_wait(first, 100), MAX_RETRY_WAIT);
    }

    /// A minute whose job could not be added is looked at again, with those
    /// after it, for an hour at most (found by the 1.2 audit: it was lost).
    #[test]
    fn a_minute_that_failed_is_tried_again() {
        let minute = 1_793_000_040;
        assert_eq!(looked_until(minute - 30, minute + 5, None), minute + 5);
        // Failed at the minute: the next look starts just before it.
        assert_eq!(looked_until(minute - 30, minute + 5, Some(minute)), minute - 1);
        assert!(looked_until(minute - 1, minute + 600, Some(minute)) < minute);
        // An hour later it is given up.
        assert_eq!(looked_until(minute - 1, minute + 4000, Some(minute)), minute + 4000 - 3600);
        // A clock set back is not run again.
        assert_eq!(looked_until(minute + 100, minute, None), minute + 100);
    }

    /// Three processes look at the schedule each second for a day: each
    /// minute of the schedule adds its job once.
    #[test]
    fn a_schedule_adds_each_minute_once_however_many_processes_look() {
        let store = Store::Memory(format!("schedule-test-{}", phv_std::random_hex(8)));
        let schedules = vec![phv_driver::config::Schedule {
            job: "tick".into(),
            cron: phv_std::cron::Cron::parse("0 * * * *", Some("Asia/Tokyo")).unwrap(),
        }];
        let start = 1_793_000_000 - 1_793_000_000 % 86_400;
        let mut lasts = [start, start + 3, start + 7];
        for second in (start..=start + 86_400).step_by(15) {
            for last in lasts.iter_mut() {
                assert!(add_scheduled(&schedules, &store, *last, second).is_empty());
                *last = second;
            }
        }
        let jobs = store.list(false).unwrap();
        assert_eq!(jobs.len(), 24, "one job an hour");
        assert!(jobs.iter().all(|j| j.name == "tick" && j.state == "waiting"));
    }
}
