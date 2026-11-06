//! Database servers: each `tests/db/<server>.phv` runs against a real server
//! and its output is compared with the `.out` file next to it. The tests are
//! skipped unless the server's URL is set:
//!
//!   PHV_TEST_POSTGRES_URL=postgres://postgres:phv@localhost/postgres
//!   PHV_TEST_MYSQL_URL=mysql://root:phv@localhost/phv
//!   PHV_TEST_REDIS_URL=redis://localhost
//!
//! `PHV_BLESS=1` rewrites the expected files.

use phv_driver::{compile_with_overlay, run, RunOptions};
use phv_vm::alloc::CountingAlloc;
use phv_vm::{DatabaseSpec, Host, Limits};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/db")
}

fn host(url: &str) -> Arc<Host> {
    let spec = DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 1 };
    Arc::new(Host { root: root(), databases: BTreeMap::from([("main".to_string(), spec)]), ..Host::default() })
}

fn run_script(host: &Arc<Host>, name: &str, max_time: Option<Duration>) -> String {
    run_limited(host, name, max_time, None)
}

fn run_limited(host: &Arc<Host>, name: &str, max_time: Option<Duration>, max_memory: Option<usize>) -> String {
    let path = root().join(format!("{name}.phv"));
    let c = match compile_with_overlay(&root(), &path, HashMap::new()) {
        Ok(c) => c,
        Err(e) => return e.render(false),
    };
    let limits = Limits { max_time, max_memory, ..Limits::default() };
    let outcome = run(&c, RunOptions { html: false, limits, http: None, host: Some(host.clone()) });
    let mut out = outcome.output;
    if let Some(err) = outcome.error {
        out.push_str("\n--- runtime error ---\n");
        out.push_str(&err.render());
    }
    out.replace("\r\n", "\n")
}

fn check(host: &Arc<Host>, name: &str) {
    let actual = run_script(host, name, Some(Duration::from_secs(30)));
    let expected_path = root().join(format!("{name}.out"));
    if std::env::var_os("PHV_BLESS").is_some() {
        std::fs::write(&expected_path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&expected_path).unwrap_or_default().replace("\r\n", "\n");
    assert_eq!(actual, expected, "output of tests/db/{name}.phv");
}

/// A query running past the script's time limit is stopped by the server.
fn check_time_limit(host: &Arc<Host>, name: &str) {
    let start = Instant::now();
    let out = run_script(host, name, Some(Duration::from_secs(1)));
    assert!(out.contains("maximum execution time"), "{out}");
    assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
}

/// A result larger than the memory limit stops the script at the limit while it
/// is read: the process does not grow far past it, and the connection is not reused.
fn check_memory_limit(host: &Arc<Host>, name: &str) {
    phv_vm::alloc::reset_thread_peak();
    let before = phv_vm::alloc::thread_allocated();
    let out = run_limited(host, name, Some(Duration::from_secs(30)), Some(16 << 20));
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{name}: {out}");
    let peak = phv_vm::alloc::thread_peak() - before;
    assert!(peak < 64 << 20, "{name}: {} MB allocated at the peak", peak >> 20);
}

/// Statements started with `*Async` take connections of their own, so the
/// pool has room for them; the two three-second statements run at the same time
/// (one after the other they would take 6 seconds; the rest leaves room for a
/// slow runner to set up its connections).
fn check_async(url: &str, name: &str) {
    let spec = DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 8 };
    let host =
        Arc::new(Host { root: root(), databases: BTreeMap::from([("main".to_string(), spec)]), ..Host::default() });
    let start = std::time::Instant::now();
    check(&host, name);
    assert!(start.elapsed() < Duration::from_millis(5000), "{name} took {:?}", start.elapsed());
}

fn server(var: &str) -> Option<String> {
    let url = std::env::var(var).ok().filter(|u| !u.is_empty());
    if url.is_none() {
        eprintln!("skipped: {var} is not set");
    }
    url
}

#[test]
fn postgres() {
    let Some(url) = server("PHV_TEST_POSTGRES_URL") else { return };
    let host = host(&url);
    check(&host, "postgres");
    check(&host, "postgres_after");
    check_time_limit(&host, "postgres_slow");
    check_time_limit(&host, "postgres_timeout_off");
    check_memory_limit(&host, "postgres_big");
    check_memory_limit(&host, "postgres_notify");
    check(&host, "postgres_types");
    check(&host, "postgres_types_after");
    check_async(&url, "postgres_async");
    check_async(&url, "postgres_kept");
    check_migrations(&host, "SELECT nope FROM missing_table");
}

#[test]
fn mysql() {
    let Some(url) = server("PHV_TEST_MYSQL_URL") else { return };
    let host = host(&url);
    check(&host, "mysql");
    check(&host, "mysql_after");
    check_time_limit(&host, "mysql_slow");
    check_time_limit(&host, "mysql_timeout_off");
    check_memory_limit(&host, "mysql_big");
    check(&host, "mysql_session");
    check(&host, "mysql_session_after");
    check_async(&url, "mysql_async");
    check_migrations(&host, "SELECT nope FROM missing_table");
}

/// `phv db migrate` on a server: each file runs once and is recorded; a
/// failing one is reported and stays pending.
fn check_migrations(host: &Arc<Host>, failing: &str) {
    use phv_std::migrate::{Conn, Target};
    let spec = &host.databases["main"];
    let mut conn = Conn::open(&Target::Server(spec)).unwrap();
    let tag = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    );
    let first = format!("9001_{tag}.sql");
    conn.apply(&first, "CREATE TABLE phv_mig_check (n integer); DROP TABLE phv_mig_check;").unwrap();
    assert!(conn.applied().unwrap().iter().any(|(n, _)| *n == first));
    let bad = format!("9002_{tag}.sql");
    assert!(conn.apply(&bad, failing).is_err());
    assert!(!conn.applied().unwrap().iter().any(|(n, _)| *n == bad));
}

#[test]
fn redis() {
    let Some(url) = server("PHV_TEST_REDIS_URL") else { return };
    let host = host(&url);
    check(&host, "redis");
    check_memory_limit(&host, "redis_big");
    check_async(&url, "redis_async");

    // Rate limits counted on the server: a second process (here, the counters
    // of this process) does not see them, the server does.
    let shared = Arc::new(Host { rate_limit_store: Some("main".into()), ..(*host).clone() });
    check(&shared, "redis_ratelimit");
    let until = std::time::Instant::now() + Duration::from_secs(2);
    let minute = Duration::from_secs(60);
    for _ in 0..3 {
        assert!(phv_std::ratelimit::hit_shared(&shared, "app:other", 3, minute, until).unwrap().0);
    }
    assert!(!phv_std::ratelimit::hit_shared(&shared, "app:other", 3, minute, until).unwrap().0);
    assert!(phv_std::ratelimit::hit("app:other", 3, minute), "the process's own counters were used");
    phv_std::ratelimit::reset_shared(&shared, "app:other", until).unwrap();
    assert!(phv_std::ratelimit::hit_shared(&shared, "app:other", 3, minute, until).unwrap().0);

    // Two windows of one key count apart, and one reset (a single key on the
    // server, never a scan of all keys) forgets both.
    let second = Duration::from_secs(1);
    assert!(phv_std::ratelimit::hit_shared(&shared, "app:two", 1, minute, until).unwrap().0);
    assert!(phv_std::ratelimit::hit_shared(&shared, "app:two", 1, second, until).unwrap().0);
    assert!(!phv_std::ratelimit::hit_shared(&shared, "app:two", 1, minute, until).unwrap().0);
    phv_std::ratelimit::reset_shared(&shared, "app:two", until).unwrap();
    assert!(phv_std::ratelimit::hit_shared(&shared, "app:two", 1, minute, until).unwrap().0);
    phv_std::ratelimit::reset_shared(&shared, "app:two", until).unwrap();
    phv_std::ratelimit::reset_shared(&shared, "app:other", until).unwrap();
}

/// A shared rate limit whose store cannot be reached refuses.
#[test]
fn unreachable_rate_limit_store_refuses() {
    let mut host = (*host("redis://127.0.0.1:1")).clone();
    host.rate_limit_store = Some("main".into());
    let out = run_script(&Arc::new(host), "ratelimit_down", Some(Duration::from_secs(5)));
    assert_eq!(
        out,
        "limited
"
    );
}

/// Remote servers need TLS unless the configuration says otherwise.
#[test]
fn remote_servers_need_tls() {
    let out = run_script(&host("redis://db.invalid"), "redis", None);
    assert!(out.contains("use a rediss:// URL"), "{out}");
    let out = run_script(&host("postgres://u:secret-pw@db.invalid/x"), "postgres_slow", None);
    assert!(out.contains("cannot connect"), "{out}");
    assert!(!out.contains("secret-pw"), "{out}");
}

/// The job store of `[jobs] store = "db:..."` on each server: jobs are taken
/// once, with a lease; failed ones run again or are given up; keys add a job
/// once; inside a transaction() a job is added only if it commits.
fn check_jobs(url: &str) {
    use phv_std::jobs::{End, Store};
    let spec = DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 4 };
    let store = Store::Db(spec.clone(), None);
    // Start from an empty store (the tests of each server run one at a time).
    let clear = |store: &Store| {
        while let Some(j) = store.take(Duration::from_secs(60)).unwrap() {
            store.end(&j, End::Done { keyed: false }).unwrap();
        }
        for j in store.list(true).unwrap() {
            store.requeue(&j.id).unwrap();
            let t = store.take(Duration::from_secs(60)).unwrap().unwrap();
            store.end(&t, End::Done { keyed: false }).unwrap();
        }
    };
    clear(&store);
    let lease = Duration::from_secs(60);
    let id = store.add("a", "payload", 0, None).unwrap().unwrap();
    let later = store.add("later", "", 3600, None).unwrap().unwrap();
    // Another site on the same database (another namespace) sees none of
    // them, and its own jobs stay its own (found by the 1.1 audit).
    let other = Store::Db(spec.clone(), Some("ab12cd34".into()));
    assert!(other.take(lease).unwrap().is_none(), "another site took a job");
    let theirs = other.add("theirs", "", 0, None).unwrap().unwrap();
    let got = other.take(lease).unwrap().unwrap();
    assert_eq!(got.id, theirs);
    other.end(&got, End::Done { keyed: false }).unwrap();
    let job = store.take(lease).unwrap().unwrap();
    assert_eq!(
        (job.id.as_str(), job.name.as_str(), job.payload.as_str(), job.attempts),
        (id.as_str(), "a", "payload", 1)
    );
    assert!(store.take(lease).unwrap().is_none(), "the other job is not due");
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert!(store.end(&job, End::Retry { at: now() - 1, error: "first" }).unwrap());
    let again = store.take(lease).unwrap().unwrap();
    assert_eq!(again.attempts, 2);
    assert!(!store.end(&job, End::Done { keyed: false }).unwrap(), "the first lease is gone");
    assert!(store.end(&again, End::Dead { error: "given up" }).unwrap());
    let failed = store.list(true).unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!((failed[0].id.as_str(), failed[0].state, failed[0].error.as_str()), (id.as_str(), "failed", "given up"));
    assert!(store.requeue(&id).unwrap());
    let third = store.take(lease).unwrap().unwrap();
    assert_eq!(third.attempts, 1);
    store.end(&third, End::Done { keyed: false }).unwrap();

    // A lease that runs out: the job is taken again.
    store.add("crash", "", 0, None).unwrap().unwrap();
    let first = store.take(Duration::from_millis(1)).unwrap().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let second = store.take(lease).unwrap().unwrap();
    assert_eq!((second.id.as_str(), second.attempts), (first.id.as_str(), 2));
    assert!(!store.end(&first, End::Done { keyed: false }).unwrap());
    assert!(store.end(&second, End::Done { keyed: false }).unwrap());

    // Keys: once while it waits, and for a day after it ended.
    let k = phv_std::jobs::uniq_key("keyed", &format!("k-{}", now()));
    assert!(store.add("keyed", "", 0, Some(&k)).unwrap().is_some());
    assert!(store.add("keyed", "", 0, Some(&k)).unwrap().is_none());
    let j = store.take(lease).unwrap().unwrap();
    store.end(&j, End::Done { keyed: true }).unwrap();
    assert!(store.add("keyed", "", 0, Some(&k)).unwrap().is_none());

    // Three takers at once: each of 150 jobs is taken exactly once.
    for i in 0..150 {
        store.add("many", &i.to_string(), 0, None).unwrap().unwrap();
    }
    let taken = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..3 {
            s.spawn(|| {
                while let Some(j) = store.take(lease).unwrap() {
                    taken.lock().unwrap().push(j.payload.clone());
                    store.end(&j, End::Done { keyed: false }).unwrap();
                }
            });
        }
    });
    let mut taken = taken.into_inner().unwrap();
    taken.sort();
    taken.dedup();
    assert_eq!(taken.len(), 150);
    store.sweep().unwrap();
    // The later job is still there, and no other job of this run.
    let left: Vec<String> = store.list(false).unwrap().into_iter().map(|j| j.id).collect();
    assert!(left.contains(&later), "{left:?}");
    assert!(!left.contains(&id));
    clear(&store);

    // The outbox: enqueue() inside a transaction() on PostgreSQL and MySQL
    // (Redis has no transaction() in std.db).
    if url.starts_with("redis") {
        return;
    }
    let mut h = (*host(url)).clone();
    h.jobs = Some(phv_vm::JobsSpec {
        store: Some("main".into()),
        names: Some(vec!["outbox".into()]),
        memory_key: String::new(),
    });
    let h = Arc::new(h);
    let out = run_script(&h, "jobs_outbox", Some(Duration::from_secs(30)));
    let expected = std::fs::read_to_string(root().join("jobs_outbox.out")).unwrap().replace("\r\n", "\n");
    assert_eq!(out, expected);
    let mut want = vec!["committed", "plain", "x"];
    if url.starts_with("postgres") {
        // A kept statement made stale does not end the next transaction.
        check(&h, "jobs_kept");
        want.extend(["kept-1", "kept-2"]);
    }
    let mut payloads = Vec::new();
    while let Some(j) = store.take(lease).unwrap() {
        payloads.push(j.payload.clone());
        store.end(&j, End::Done { keyed: false }).unwrap();
    }
    payloads.sort();
    want.sort();
    assert_eq!(payloads, want);
}

/// Transactions that add jobs at the same time keep the cap of 100,000
/// waiting and running jobs between them (found by the 1.1.1 audit: eight
/// transactions at 95,000 reached 111,000). Each adds up to 1,000 jobs to a
/// store 3,000 below the cap: together they add exactly 3,000.
fn check_job_cap(url: &str) {
    let spec = DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 16 };
    let host = Arc::new(Host {
        root: root(),
        databases: BTreeMap::from([("main".to_string(), spec)]),
        // The table the scripts name: phv_jobs_cafe0123cafe0123cafe0123cafe0123.
        store_namespace: Some("cafe0123cafe0123cafe0123cafe0123".into()),
        jobs: Some(phv_vm::JobsSpec {
            store: Some("main".into()),
            names: Some(vec!["burst".into()]),
            memory_key: String::new(),
        }),
        ..Host::default()
    });
    let limit = Some(Duration::from_secs(300));
    assert_eq!(run_script(&host, "jobs_cap_fill", limit), "97000", "{url}");
    let outs: Vec<String> = std::thread::scope(|s| {
        let runs: Vec<_> = (0..8).map(|_| s.spawn(|| run_script(&host, "jobs_cap_burst", limit))).collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    });
    let added: i64 = outs.iter().map(|o| o.trim_start_matches("full ").parse::<i64>().unwrap_or(-100_000)).sum();
    let total = run_script(&host, "jobs_cap_count", limit);
    assert_eq!((total.as_str(), added), ("100000", 3000), "{url}: {outs:?}");
}

#[test]
fn concurrent_transactions_keep_the_job_cap() {
    for var in ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
        if let Some(url) = server(var) {
            check_job_cap(&url);
        }
    }
}

#[test]
fn job_stores() {
    for var in ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL", "PHV_TEST_REDIS_URL"] {
        if let Some(url) = server(var) {
            check_jobs(&url);
        }
    }
}
