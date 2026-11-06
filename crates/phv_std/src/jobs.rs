//! Background jobs (`std.jobs`, `[jobs]` in phv.toml): a queue of named jobs
//! that `phv serve` and `phv jobs run` take and run, one script each, outside
//! any request.
//!
//! The queue is kept in this process's memory (`store = "memory"`, lost on
//! restart) or in a database of `[databases]` (PostgreSQL, MySQL or Redis,
//! shared by every process). A job is run at least once: it is taken with a
//! lease, and a job whose process stopped before it ended is taken again when
//! its lease runs out.

use crate::netdb::{self, JobEnd, JobInfo, JobRow};
use crate::{int, s};
use phv_vm::{BuiltinFn, DatabaseSpec, Host, Value, Vm, VmError};
use std::collections::HashMap;
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use netdb::{JobCap, JobEnd as End, JobInfo as Info, JobRow as Row, JOB_MAX_DEAD, JOB_MAX_KEPT};

/// The largest payload of a job.
pub const MAX_PAYLOAD: usize = 64 * 1024;
/// The longest a job may be put off (`delay`), in seconds: 30 days.
pub const MAX_DELAY: i64 = 30 * 24 * 3600;
/// The longest key.
pub const MAX_KEY: usize = 200;
/// Waiting and running jobs a database store holds at most, and the bytes of
/// their payloads (found by the 1.1.1 audit: only the jobs were counted).
pub const MAX_JOBS: usize = 100_000;
pub const MAX_JOB_BYTES: usize = 1 << 30;
/// The caps of a database store.
pub const DB_CAP: JobCap = JobCap { jobs: MAX_JOBS, bytes: MAX_JOB_BYTES };
/// Waiting and running jobs, and their bytes, the memory store holds at most.
pub const MAX_MEMORY_JOBS: usize = 10_000;
pub const MAX_MEMORY_BYTES: usize = 64 << 20;
/// Payloads of jobs given up that the memory store keeps (for `retry`), and
/// keys of ended jobs it remembers (found by the 1.1 audit: neither was bounded).
const MAX_MEMORY_DEAD_BYTES: usize = 16 << 20;
const MAX_MEMORY_KEYS: usize = 100_000;

type R = Result<Value, VmError>;

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether `name` can name a job: lowercase letters, digits and `_`, at most 64.
pub fn job_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

// ----- Wake-ups ----------------------------------------------------------------

/// Jobs added in this process so far (for `/metrics`).
static ADDED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Per store: jobs added to it in this process, so its runners waiting for
/// work wake up at once (jobs added by other processes are found when they
/// look again). One job wakes one runner of its store (found by the 1.1
/// audit: every runner woke and asked the store for each job added).
type Wake = std::sync::Arc<(Mutex<u64>, Condvar)>;
static WAKE: LazyLock<Mutex<HashMap<String, Wake>>> = LazyLock::new(Default::default);

fn wake_of(store: &Store) -> Wake {
    lock(&WAKE).entry(store.wake_key()).or_default().clone()
}

fn added(store: &Store) {
    ADDED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let w = wake_of(store);
    let (m, c) = &*w;
    *lock(m) += 1;
    c.notify_one();
}

/// Jobs added in this process so far (for `/metrics`).
pub fn added_count() -> u64 {
    ADDED.load(std::sync::atomic::Ordering::Relaxed)
}

// ----- The memory store --------------------------------------------------------

struct MemJob {
    name: String,
    payload: String,
    run_at: i64,
    attempts: u32,
    /// 0 waiting, 1 running, 2 given up.
    state: u8,
    lease: Option<(String, i64)>,
    uniq: Option<String>,
    error: String,
}

#[derive(Default)]
struct MemQueue {
    jobs: HashMap<String, MemJob>,
    /// Keys in use, with the job and until when (ms) they are kept.
    keys: HashMap<String, (String, i64)>,
    /// Bytes of the payloads of waiting and running jobs.
    bytes: usize,
    /// Bytes of the payloads of jobs given up.
    dead_bytes: usize,
    /// When it was last swept (ms): at most once a second.
    swept: i64,
}

/// The memory queues of this process, by project.
static MEMORY: LazyLock<Mutex<HashMap<String, MemQueue>>> = LazyLock::new(Default::default);

impl MemQueue {
    fn live(&self) -> usize {
        self.jobs.values().filter(|j| j.state < 2).count()
    }

    /// Forgets expired keys and old jobs given up: at most once a second,
    /// unless a bound is passed (`force`).
    fn sweep(&mut self, now: i64, force: bool) {
        let over_bounds = self.dead_bytes > MAX_MEMORY_DEAD_BYTES || self.keys.len() > MAX_MEMORY_KEYS;
        if !force && !over_bounds && now - self.swept < 1000 {
            return;
        }
        self.swept = now;
        self.keys.retain(|_, (_, until)| *until > now);
        // Over the bound, the keys that end soonest go first.
        if self.keys.len() > MAX_MEMORY_KEYS {
            let mut ends: Vec<(i64, String)> = self
                .keys
                .iter()
                .filter(|(_, (_, until))| *until != i64::MAX)
                .map(|(k, (_, until))| (*until, k.clone()))
                .collect();
            ends.sort();
            // Down to nine tenths, so the next keys do not sweep again at once.
            let excess = self.keys.len() - MAX_MEMORY_KEYS * 9 / 10;
            for (_, k) in ends.into_iter().take(excess) {
                self.keys.remove(&k);
            }
        }
        let mut dead: Vec<(i64, String)> =
            self.jobs.iter().filter(|(_, j)| j.state == 2).map(|(id, j)| (j.run_at, id.clone())).collect();
        dead.sort();
        let over = dead.len().saturating_sub(JOB_MAX_DEAD);
        for (i, (at, id)) in dead.iter().enumerate() {
            if i < over || *at < now - 7 * 24 * 3600 * 1000 || self.dead_bytes > MAX_MEMORY_DEAD_BYTES {
                if let Some(j) = self.jobs.remove(id) {
                    self.dead_bytes -= j.payload.len();
                    if let Some(u) = j.uniq {
                        self.keys.remove(&u);
                    }
                }
            }
        }
    }
}

// ----- The store of a project ------------------------------------------------------

/// Where a project's jobs are kept.
#[derive(Clone, Debug)]
pub enum Store {
    /// In this process, under the project's key.
    Memory(String),
    /// A database, with the site's namespace in it (`Host::store_namespace`).
    Db(DatabaseSpec, Option<String>),
}

impl Store {
    /// Which store this is, for its runners' wake-ups.
    fn wake_key(&self) -> String {
        match self {
            Store::Memory(key) => format!("memory\0{key}"),
            Store::Db(spec, ns) => format!("{}\0{}", spec.url.as_deref().unwrap_or(""), ns.as_deref().unwrap_or("")),
        }
    }

    /// Waits up to `wait` for a job to be added to this store in this process.
    pub fn wait_for_jobs(&self, wait: Duration) {
        let w = wake_of(self);
        let (m, c) = &*w;
        let g = lock(m);
        let seen = *g;
        let _ = c.wait_timeout_while(g, wait, |n| *n == seen);
    }

    /// The store of `[jobs]`, if the project has one.
    pub fn of(host: &Host) -> Option<Store> {
        let j = host.jobs.as_ref()?;
        Some(match &j.store {
            None => Store::Memory(j.memory_key.clone()),
            Some(name) => Store::Db(host.databases.get(name)?.clone(), host.store_namespace.clone()),
        })
    }

    /// Adds a job: its id, or `None` when a job with the same key exists
    /// (waiting, running, given up, or ended less than a day ago).
    pub fn add(
        &self,
        name: &str,
        payload: &str,
        delay_secs: i64,
        uniq: Option<&str>,
    ) -> Result<Option<String>, String> {
        self.add_before(name, payload, delay_secs, uniq, None)
    }

    /// `add`, waiting for the store until `deadline` at most.
    pub fn add_before(
        &self,
        name: &str,
        payload: &str,
        delay_secs: i64,
        uniq: Option<&str>,
        deadline: Option<std::time::Instant>,
    ) -> Result<Option<String>, String> {
        let id = crate::random_hex(16);
        let now = now_ms();
        let run_at = now + delay_secs * 1000;
        let r = match self {
            Store::Db(spec, ns) => {
                netdb::job_add(spec, ns.as_deref(), (&id, name, payload), run_at, uniq, DB_CAP, deadline)?.map(|()| id)
            }
            Store::Memory(key) => {
                let mut all = lock(&MEMORY);
                let q = all.entry(key.clone()).or_default();
                q.sweep(now, false);
                if let Some(u) = uniq {
                    if q.keys.get(u).is_some_and(|(_, until)| *until > now) {
                        return Ok(None);
                    }
                }
                if q.live() >= MAX_MEMORY_JOBS || q.bytes + payload.len() > MAX_MEMORY_BYTES {
                    return Err(format!(
                        "the job queue holds {MAX_MEMORY_JOBS} jobs or {} MB already",
                        MAX_MEMORY_BYTES >> 20
                    ));
                }
                if let Some(u) = uniq {
                    q.keys.insert(u.to_string(), (id.clone(), i64::MAX));
                }
                q.bytes += payload.len();
                q.jobs.insert(
                    id.clone(),
                    MemJob {
                        name: name.to_string(),
                        payload: payload.to_string(),
                        run_at,
                        attempts: 0,
                        state: 0,
                        lease: None,
                        uniq: uniq.map(str::to_string),
                        error: String::new(),
                    },
                );
                Some(id)
            }
        };
        if r.is_some() {
            added(self);
        }
        Ok(r)
    }

    /// Takes the next job due, or one whose lease ran out, for `lease`.
    pub fn take(&self, lease: Duration) -> Result<Option<JobRow>, String> {
        let lease_ms = lease.as_millis() as i64;
        match self {
            Store::Db(spec, ns) => netdb::job_take(spec, ns.as_deref(), lease_ms),
            Store::Memory(key) => {
                let now = now_ms();
                let mut all = lock(&MEMORY);
                let Some(q) = all.get_mut(key) else { return Ok(None) };
                let next = q
                    .jobs
                    .iter()
                    .filter(|(_, j)| {
                        (j.state == 0 && j.run_at <= now)
                            || (j.state == 1 && j.lease.as_ref().is_some_and(|(_, until)| *until < now))
                    })
                    .min_by_key(|(_, j)| j.run_at)
                    .map(|(id, _)| id.clone());
                let Some(id) = next else { return Ok(None) };
                let j = q.jobs.get_mut(&id).expect("found above");
                let token = crate::random_hex(16);
                j.state = 1;
                j.attempts += 1;
                j.lease = Some((token.clone(), now + lease_ms));
                Ok(Some(JobRow {
                    id,
                    name: j.name.clone(),
                    payload: j.payload.clone(),
                    attempts: j.attempts,
                    lease: token,
                }))
            }
        }
    }

    /// Records how a run ended; `false` when its lease was lost meanwhile.
    pub fn end(&self, job: &JobRow, end: JobEnd) -> Result<bool, String> {
        match self {
            Store::Db(spec, ns) => netdb::job_end(spec, ns.as_deref(), job, end),
            Store::Memory(key) => {
                let now = now_ms();
                let mut all = lock(&MEMORY);
                let Some(q) = all.get_mut(key) else { return Ok(false) };
                let Some(j) = q.jobs.get_mut(&job.id) else { return Ok(false) };
                if j.lease.as_ref().map(|(t, _)| t.as_str()) != Some(job.lease.as_str()) {
                    return Ok(false);
                }
                j.lease = None;
                match end {
                    JobEnd::Done { .. } => {
                        let j = q.jobs.remove(&job.id).expect("found above");
                        q.bytes -= j.payload.len();
                        if let Some(u) = j.uniq {
                            q.keys.insert(u, (job.id.clone(), now + 24 * 3600 * 1000));
                        }
                    }
                    JobEnd::Retry { at, error } => {
                        j.state = 0;
                        j.run_at = at;
                        j.error = error.to_string();
                    }
                    JobEnd::Dead { error } => {
                        j.state = 2;
                        j.run_at = now;
                        j.error = error.to_string();
                        q.bytes -= j.payload.len();
                        q.dead_bytes += j.payload.len();
                        q.sweep(now, true);
                    }
                }
                Ok(true)
            }
        }
    }

    /// Removes old jobs that were given up, and old keys.
    pub fn sweep(&self) -> Result<(), String> {
        match self {
            Store::Db(spec, ns) => netdb::job_sweep(spec, ns.as_deref()),
            Store::Memory(key) => {
                if let Some(q) = lock(&MEMORY).get_mut(key) {
                    q.sweep(now_ms(), true);
                }
                Ok(())
            }
        }
    }

    /// The jobs of the store: all, or those given up.
    pub fn list(&self, failed_only: bool) -> Result<Vec<JobInfo>, String> {
        match self {
            Store::Db(spec, ns) => netdb::job_list(spec, ns.as_deref(), failed_only),
            Store::Memory(key) => {
                let all = lock(&MEMORY);
                let Some(q) = all.get(key) else { return Ok(Vec::new()) };
                let mut out: Vec<JobInfo> = q
                    .jobs
                    .iter()
                    .filter(|(_, j)| !failed_only || j.state == 2)
                    .map(|(id, j)| JobInfo {
                        id: id.clone(),
                        name: j.name.clone(),
                        state: ["waiting", "running", "failed"][j.state.min(2) as usize],
                        attempts: j.attempts,
                        at: j.run_at,
                        error: j.error.clone(),
                    })
                    .collect();
                out.sort_by(|a, b| (a.state, a.at).cmp(&(b.state, b.at)));
                Ok(out)
            }
        }
    }

    /// Puts a job that was given up back in the queue.
    pub fn requeue(&self, id: &str) -> Result<bool, String> {
        let r = match self {
            Store::Db(spec, ns) => netdb::job_requeue(spec, ns.as_deref(), id)?,
            Store::Memory(key) => {
                let mut all = lock(&MEMORY);
                let Some(q) = all.get_mut(key) else { return Ok(false) };
                match q.jobs.get_mut(id) {
                    Some(j) if j.state == 2 => {
                        j.state = 0;
                        j.attempts = 0;
                        j.run_at = now_ms();
                        j.error.clear();
                        q.bytes += j.payload.len();
                        q.dead_bytes -= j.payload.len();
                        true
                    }
                    _ => false,
                }
            }
        };
        if r {
            added(self);
        }
        Ok(r)
    }
}

/// The key a job is stored under for `key`: a hash, scoped by the job's name.
pub fn uniq_key(name: &str, key: &str) -> String {
    crate::sha256_hex(format!("{name}\0{key}").as_bytes())
}

/// The key of a schedule's run in `minute`: apart from every key a page can
/// give (found by the 1.1 audit: `key: "schedule:<minute>"` took a run).
/// Job names never start with `\x01`.
pub fn schedule_key(name: &str, minute: i64) -> String {
    crate::sha256_hex(format!("\x01schedule\0{name}\0{minute}").as_bytes())
}

// ----- std.jobs ------------------------------------------------------------------

/// `enqueue($job, $payload, $delay, $key)`.
fn add(vm: &mut Vm, a: &[Value]) -> R {
    let (name, payload, delay) = (s(&a[0]).to_string(), s(&a[1]).to_string(), int(&a[2]));
    let key = match &a[3] {
        Value::Null => None,
        v => Some(s(v).to_string()),
    };
    let Some(spec) = vm.host.jobs.clone() else {
        return Err(vm.throw("RuntimeException", "enqueue(): phv.toml has no [jobs] section"));
    };
    let known = spec.names.as_ref().is_none_or(|n| n.contains(&name));
    if !job_name(&name) || !known {
        let shown: String = name.chars().take(64).collect();
        return Err(vm.throw("ValueError", format!("enqueue(): there is no job \"{}\"", shown.escape_debug())));
    }
    if payload.len() > MAX_PAYLOAD {
        return Err(vm.throw("ValueError", "enqueue(): a payload is at most 64 KB"));
    }
    if !(0..=MAX_DELAY).contains(&delay) {
        return Err(vm.throw("ValueError", "enqueue(): the delay is 0 to 2,592,000 seconds (30 days)"));
    }
    if key.as_ref().is_some_and(|k| k.is_empty() || k.len() > MAX_KEY) {
        return Err(vm.throw("ValueError", format!("enqueue(): a key is 1 to {MAX_KEY} bytes")));
    }
    let uniq = key.as_deref().map(|k| uniq_key(&name, k));
    let Some(store) = Store::of(&vm.host) else {
        return Err(vm.throw("RuntimeException", "enqueue(): the database of [jobs] store is not in [databases]"));
    };
    // Inside a transaction() on the store's own database, the job is added
    // with the transaction: only if it commits.
    if let (Store::Db(db, ns), Some(db_name)) = (&store, spec.store.as_deref()) {
        let id = crate::random_hex(16);
        let run_at = now_ms() + delay * 1000;
        // The table must exist before the transaction adds to it (MySQL would
        // commit the transaction to create it).
        if let Err(e) = netdb::job_store_ready(db, ns.as_deref(), vm.deadline()) {
            return Err(vm.throw("DatabaseError", format!("enqueue(): {e}")));
        }
        let job = (id.as_str(), name.as_str(), payload.as_str());
        match netdb::job_add_in_transaction(vm, db, db_name, job, run_at, uniq.as_deref(), DB_CAP) {
            Some(Ok(Some(()))) => {
                added(&store);
                return Ok(Value::str(id));
            }
            Some(Ok(None)) => return Ok(Value::Null),
            Some(Err(e)) => return Err(e),
            // No transaction: added on a connection of its own below.
            None => {}
        }
    }
    match store.add_before(&name, &payload, delay, uniq.as_deref(), vm.deadline()) {
        Ok(Some(id)) => Ok(Value::str(id)),
        Ok(None) => Ok(Value::Null),
        Err(e) if e.starts_with("the job queue holds") => Err(vm.throw("RuntimeException", format!("enqueue(): {e}"))),
        Err(e) => Err(vm.throw("DatabaseError", format!("enqueue(): {e}"))),
    }
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__jobs_add" => add,
        "__job_info" => |vm, _| -> R {
            let Some(j) = vm.http.job.clone() else {
                return Err(vm.throw("LogicException", "job() can only be used in a job (a script of [jobs] dir)"));
            };
            Ok(Value::list(vec![
                Value::str(j.name),
                Value::str(j.id),
                Value::str(j.payload),
                Value::str(j.attempt.to_string()),
            ]))
        },
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_store_runs_jobs_once_and_retries() {
        let store = Store::Memory(format!("test-{}", crate::random_hex(8)));
        let lease = Duration::from_secs(60);
        let id = store.add("send", "p1", 0, None).unwrap().unwrap();
        assert!(store.add("send", "later", 3600, None).unwrap().is_some());
        let job = store.take(lease).unwrap().unwrap();
        assert_eq!((job.id.as_str(), job.payload.as_str(), job.attempts), (id.as_str(), "p1", 1));
        assert!(store.take(lease).unwrap().is_none(), "the other one is not due");
        assert!(store.end(&job, JobEnd::Retry { at: now_ms() - 1, error: "boom" }).unwrap());
        let again = store.take(lease).unwrap().unwrap();
        assert_eq!(again.attempts, 2);
        // The first lease is gone: ending the old run does nothing.
        assert!(!store.end(&job, JobEnd::Done { keyed: false }).unwrap());
        assert!(store.end(&again, JobEnd::Dead { error: "gave up" }).unwrap());
        let failed = store.list(true).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!((failed[0].state, failed[0].error.as_str()), ("failed", "gave up"));
        assert!(store.requeue(&id).unwrap());
        let third = store.take(lease).unwrap().unwrap();
        assert_eq!(third.attempts, 1);
        assert!(store.end(&third, JobEnd::Done { keyed: false }).unwrap());
        assert_eq!(store.list(false).unwrap().len(), 1, "the delayed one");
    }

    #[test]
    fn keys_add_a_job_once_and_leases_run_out() {
        let store = Store::Memory(format!("test-{}", crate::random_hex(8)));
        let k = uniq_key("report", "2026-11-06");
        assert!(store.add("report", "", 0, Some(&k)).unwrap().is_some());
        assert!(store.add("report", "", 0, Some(&k)).unwrap().is_none());
        let job = store.take(Duration::from_millis(1)).unwrap().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        // Its process stopped: the job is taken again once the lease ran out.
        let again = store.take(Duration::from_secs(60)).unwrap().unwrap();
        assert_eq!((again.id.as_str(), again.attempts), (job.id.as_str(), 2));
        assert!(store.end(&again, JobEnd::Done { keyed: true }).unwrap());
        assert!(store.add("report", "", 0, Some(&k)).unwrap().is_none(), "the key is kept after the job ended");
        assert_ne!(uniq_key("report", "a"), uniq_key("other", "a"));
    }

    #[test]
    fn the_memory_store_is_bounded() {
        let store = Store::Memory(format!("test-{}", crate::random_hex(8)));
        let big = "x".repeat(MAX_PAYLOAD);
        let mut n = 0;
        let err = loop {
            match store.add("big", &big, 0, None) {
                Ok(_) => n += 1,
                Err(e) => break e,
            }
        };
        assert_eq!(n, MAX_MEMORY_BYTES / MAX_PAYLOAD);
        assert!(err.contains("the job queue holds"), "{err}");
    }

    /// Jobs given up and keys of ended jobs are bounded too, and a page's key
    /// cannot be a schedule's (found by the 1.1 audit).
    #[test]
    fn what_the_memory_store_keeps_is_bounded() {
        let key = format!("test-{}", crate::random_hex(8));
        let store = Store::Memory(key.clone());
        let big = "x".repeat(MAX_PAYLOAD);
        for _ in 0..1000 {
            store.add("dead", &big, 0, None).unwrap().unwrap();
            let j = store.take(Duration::from_secs(60)).unwrap().unwrap();
            store.end(&j, JobEnd::Dead { error: "no" }).unwrap();
        }
        let dead_bytes = lock(&MEMORY).get(&key).unwrap().dead_bytes;
        assert!(dead_bytes <= MAX_MEMORY_DEAD_BYTES, "{} MB of jobs given up", dead_bytes >> 20);
        for i in 0..MAX_MEMORY_KEYS + 1000 {
            let k = uniq_key("keyed", &i.to_string());
            store.add("keyed", "", 0, Some(&k)).unwrap().unwrap();
            let j = store.take(Duration::from_secs(60)).unwrap().unwrap();
            store.end(&j, JobEnd::Done { keyed: true }).unwrap();
        }
        store.add("one", "", 0, None).unwrap().unwrap();
        assert!(lock(&MEMORY).get(&key).unwrap().keys.len() <= MAX_MEMORY_KEYS);
        assert_ne!(schedule_key("report", 29000000), uniq_key("report", "schedule:29000000"));
    }
}
