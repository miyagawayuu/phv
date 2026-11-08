//! Database servers: each `tests/db/<server>.phv` runs against a real server
//! and its output is compared with the `.out` file next to it. The tests are
//! skipped unless the server's URL is set:
//!
//!   PHV_TEST_POSTGRES_URL=postgres://postgres:phv@localhost/postgres
//!   PHV_TEST_MYSQL_URL=mysql://root:phv@localhost/phv
//!   PHV_TEST_REDIS_URL=redis://localhost
//!
//! `PHV_BLESS=1` rewrites the expected files.
//!
//! Several test processes may run against the servers at once: each run owns
//! its tables and namespaces (`phv_std::testing`): the scripts' fixed names
//! are rewritten to this run's as they are compiled.

use phv_driver::{compile_with_overlay, run, RunOptions};
use phv_std::testing;
use phv_vm::alloc::CountingAlloc;
use phv_vm::{DatabaseSpec, Host, Limits};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The tests that fill a job store or time it run one at a time: the 97,000
/// jobs of one would slow the server under the timing of another.
static HEAVY_JOB_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn heavy() -> std::sync::MutexGuard<'static, ()> {
    HEAVY_JOB_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Once per process, before its first test on a server: removes what runs
/// older than two hours left on the servers (never this run's names). Called
/// before `heavy()`, which it takes itself.
fn sweep_once() {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| {
        let _one_at_a_time = heavy();
        for var in ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL", "PHV_TEST_REDIS_URL"] {
            if let Some(url) = std::env::var(var).ok().filter(|u| !u.is_empty()) {
                testing::sweep(&url);
            }
        }
    });
}

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

/// Reads one RESP command (`*N`, then N bulk strings) from a client; None
/// at the end of the stream.
fn read_resp_command(r: &mut impl std::io::BufRead) -> Option<Vec<String>> {
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let n: usize = line.trim().strip_prefix('*')?.parse().ok()?;
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        line.clear();
        r.read_line(&mut line).ok()?;
        line.clear();
        r.read_line(&mut line).ok()?;
        args.push(line.trim_end().to_string());
    }
    Some(args)
}

/// Found by the 1.2.1 audit: a reply the Redis server sent late, after a
/// pipeline's own replies, waited in the socket and was read as the next
/// command's reply, by the same script or by the next script that took the
/// connection from the pool. A connection with bytes waiting is out of step
/// before anything more is written on it: another takes its place, and the
/// script gets the server's real reply. (No real Redis server is needed: a
/// test server answers here.)
#[test]
fn a_late_redis_reply_is_not_read_as_the_next_commands() {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let seen = connections.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(s) = stream else { continue };
            seen.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let mut r = std::io::BufReader::new(s.try_clone().unwrap());
                let mut s = s;
                while let Some(command) = read_resp_command(&mut r) {
                    let reply: &[u8] = match command.first().map(String::as_str) {
                        // The pipeline (SET, INCRBY, INCRBY, GET): its four
                        // replies, then one too many 100 ms later.
                        Some("SET") => {
                            for _ in 0..3 {
                                read_resp_command(&mut r);
                            }
                            if s.write_all(b"+OK\r\n:7\r\n-WRONGTYPE not a counter\r\n$1\r\nx\r\n").is_err() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                            b"+EXTRA\r\n"
                        }
                        Some("PING") => b"+PONG\r\n",
                        _ => b"$5\r\nfresh\r\n",
                    };
                    if s.write_all(reply).is_err() {
                        return;
                    }
                }
            });
        }
    });
    let host = host(&format!("redis://127.0.0.1:{port}"));
    // Within one script: the pipeline, other work while the late reply
    // arrives, then a GET on the same handle.
    let out = run_script(&host, "redis_late_reply", Some(Duration::from_secs(30)));
    assert_eq!(
        out,
        "pipeline replies: 4\n  [0] true\n  [1] 7\n  [2] DatabaseError: WRONGTYPE: not a counter\n  [3] \"x\"\n\
         next get: \"fresh\"\nsecond get: \"fresh\"\n",
        "{out}"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 2, "the GET went on the connection out of step");
    // Across scripts: the late reply arrives after the connection went back
    // to the pool, before the next script takes it.
    let out = run_script(&host, "redis_late_reply_first", Some(Duration::from_secs(30)));
    assert_eq!(out, "pipeline replies: 4\n", "{out}");
    std::thread::sleep(Duration::from_millis(400));
    let out = run_script(&host, "redis_late_reply_second", Some(Duration::from_secs(30)));
    assert_eq!(out, "get: \"fresh\"\n", "{out}");
    assert_eq!(connections.load(Ordering::SeqCst), 3, "the pooled connection out of step was used again");
}

fn run_limited(host: &Arc<Host>, name: &str, max_time: Option<Duration>, max_memory: Option<usize>) -> String {
    let path = root().join(format!("{name}.phv"));
    // The script with this run's table names (the expected output has the fixed ones).
    let script = testing::rewrite(&std::fs::read_to_string(&path).unwrap());
    let overlay = HashMap::from([(path.canonicalize().unwrap(), script)]);
    let c = match compile_with_overlay(&root(), &path, overlay) {
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
    testing::restore(&out.replace("\r\n", "\n"))
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
    sweep_once();
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
    check(&host, "postgres_prepared");
    check_migrations(&host, "SELECT nope FROM missing_table");
}

/// A script that deallocates every statement of its connection (here in a
/// function, where PHV cannot see it) also removes those the client prepared
/// to look up types: the connection is not given to the next script, which
/// would fail on them ("prepared statement ... does not exist"; found by the
/// 1.1.1 tests, after jobs_kept.phv ran on the connection of postgres_types).
#[test]
fn a_connection_whose_statements_a_script_deallocated_is_not_reused() {
    sweep_once();
    let Some(url) = server("PHV_TEST_POSTGRES_URL") else { return };
    // A pool of its own (pool_size 3): one connection, used by the two scripts in turn.
    let spec = DatabaseSpec { url: Some(url), url_env: None, tls: None, pool_size: 3 };
    let host =
        Arc::new(Host { root: root(), databases: BTreeMap::from([("main".to_string(), spec)]), ..Host::default() });
    assert_eq!(run_script(&host, "postgres_dealloc", Some(Duration::from_secs(30))), "x\ndeallocated: 1\n");
    assert_eq!(run_script(&host, "postgres_dealloc_after", Some(Duration::from_secs(30))), "y\ndone\n");
}

/// A script that deallocates only the statements the client prepared to look
/// types up (here in a function, where PHV cannot see it) does not leave a
/// connection that fails for later scripts: the reset finds them gone by
/// their names and closes it; when they were prepared in the same run, so
/// the reset never saw them, it is closed too; and when only some of them
/// were, the first statement that fails on one closes it (found by the 1.2
/// audit: such a connection failed for its whole life).
#[test]
fn a_connection_whose_type_lookups_a_script_deallocated_is_not_reused() {
    sweep_once();
    let Some(url) = server("PHV_TEST_POSTGRES_URL") else { return };
    // Pools of their own (pool_size 5 to 7): one connection each, used by the scripts in turn.
    let host = |pool_size: usize| {
        let spec = DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size };
        Arc::new(Host { root: root(), databases: BTreeMap::from([("main".to_string(), spec)]), ..Host::default() })
    };
    let limit = Some(Duration::from_secs(30));
    let (seen, same_run, partly) = (host(5), host(6), host(7));
    assert_eq!(run_script(&seen, "postgres_typeinfo", limit), "x\n");
    assert_eq!(run_script(&seen, "postgres_typeinfo_poison", limit), "deallocated\n");
    assert_eq!(run_script(&seen, "postgres_typeinfo_after", limit), "y\n", "deallocated after the reset saw them");
    assert_eq!(run_script(&same_run, "postgres_typeinfo_both", limit), "x\ndeallocated\n");
    assert_eq!(run_script(&same_run, "postgres_typeinfo_after", limit), "y\n", "deallocated in the run that made them");
    assert_eq!(run_script(&partly, "postgres_typeinfo_partial", limit), "x\ndeallocated\n");
    let first = run_script(&partly, "postgres_typeinfo_after", limit);
    assert!(first == "y\n" || first.contains("does not exist"), "{first}");
    assert_eq!(run_script(&partly, "postgres_typeinfo_after", limit), "y\n", "some of them deallocated");
    assert_eq!(run_script(&seen, "postgres_typeinfo_drop", limit), "done\n");
}

#[test]
fn mysql() {
    sweep_once();
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
    check(&host, "mysql_xa");
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
    let table = format!("phv_{}_mig", testing::run_id());
    conn.apply(&first, &format!("CREATE TABLE {table} (n integer); DROP TABLE {table};")).unwrap();
    assert!(conn.applied().unwrap().iter().any(|(n, _)| *n == first));
    let bad = format!("9002_{tag}.sql");
    assert!(conn.apply(&bad, failing).is_err());
    assert!(!conn.applied().unwrap().iter().any(|(n, _)| *n == bad));
}

#[test]
fn redis() {
    sweep_once();
    let Some(url) = server("PHV_TEST_REDIS_URL") else { return };
    let host = host(&url);
    check(&host, "redis");
    check_memory_limit(&host, "redis_big");
    check_async(&url, "redis_async");

    // Rate limits counted on the server: a second process (here, the counters
    // of this process) does not see them, the server does. The keys are this
    // run's: in a namespace of its own.
    let shared = Arc::new(Host {
        rate_limit_store: Some("main".into()),
        store_namespace: Some(testing::namespace(0x20)),
        ..(*host).clone()
    });
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
    // A namespace of this run (the stores of sites without one are covered
    // by the unnamed-site tests of phv_std and phv_server).
    let ns = testing::namespace(0x10);
    let store = Store::Db(spec.clone(), Some(ns.clone()));
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
    // Due in a few seconds, and taken at the end: a job left waiting (once an
    // hour) came due in the middle of a later run of these tests and was taken
    // there as one of its own.
    let later_due = Instant::now() + Duration::from_secs(LATER_SECS as u64);
    let later = store.add("later", "", LATER_SECS, None).unwrap().unwrap();
    // Another site on the same database (another namespace) sees none of
    // them, and its own jobs stay its own (found by the 1.1 audit).
    let other = Store::Db(spec.clone(), Some(testing::namespace(0x11)));
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
                    if j.id == later {
                        // On a slow machine the checks above took longer than
                        // its delay: back to waiting until it is looked for.
                        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
                            as i64
                            + 5_000;
                        store.end(&j, End::Retry { at, error: "not yet" }).unwrap();
                        continue;
                    }
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
    std::thread::sleep(later_due.saturating_duration_since(Instant::now()) + Duration::from_millis(200));
    // Due now, or within the few seconds it was put back for above.
    let mut j = store.take(lease).unwrap();
    for _ in 0..100 {
        if j.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        j = store.take(lease).unwrap();
    }
    let j = j.expect("the later job is due");
    assert_eq!(j.id, later);
    assert!(store.end(&j, End::Done { keyed: false }).unwrap());
    clear(&store);

    // The outbox: enqueue() inside a transaction() on PostgreSQL and MySQL
    // (Redis has no transaction() in std.db).
    if url.starts_with("redis") {
        return;
    }
    let mut h = (*host(url)).clone();
    h.store_namespace = Some(ns);
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
        // The scripts name the table phv_jobs_cafe0123cafe0123cafe0123cafe0123,
        // rewritten to this run's namespace of that tag.
        store_namespace: Some(testing::namespace(0xca)),
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
    sweep_once();
    let _one_at_a_time = heavy();
    for var in ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
        if let Some(url) = server(var) {
            check_job_cap(&url);
        }
    }
}

/// A host whose job store is the table `phv_jobs_<ns>` of `url`.
fn jobs_host(url: &str, ns: &str, names: &[&str]) -> Arc<Host> {
    let spec = DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 4 };
    Arc::new(Host {
        root: root(),
        databases: BTreeMap::from([("main".to_string(), spec)]),
        store_namespace: Some(ns.into()),
        jobs: Some(phv_vm::JobsSpec {
            store: Some("main".into()),
            names: Some(names.iter().map(|n| n.to_string()).collect()),
            memory_key: String::new(),
        }),
        ..Host::default()
    })
}

/// A job added with a key that another open transaction has added waits for
/// that transaction no longer than the page's time limit, on the server too
/// (nothing is left waiting there once the page gave up), and such waits do
/// not hold the connections other jobs are added on (found by the 1.1.1 audit:
/// MySQL answered after 40 s under a 2 s limit, PostgreSQL left the statements
/// waiting on the server, and four waits took every connection of the store).
fn check_keyed_wait(url: &str) {
    // The scripts name phv_jobs_f1f1f1f1f1f1f1f1, rewritten to this namespace.
    let host = jobs_host(url, &testing::namespace(0xf1), &["k"]);
    let limit = Some(Duration::from_secs(2));
    std::thread::scope(|s| {
        // Holds the key ten seconds.
        let holder = s.spawn(|| run_script(&host, "jobs_key_hold", Some(Duration::from_secs(60))));
        std::thread::sleep(Duration::from_secs(2));
        let waiting: Vec<_> = (0..6)
            .map(|_| {
                s.spawn(|| {
                    let start = Instant::now();
                    let out = run_script(&host, "jobs_key_add", limit);
                    (start.elapsed(), out)
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(500));
        let plain: Vec<_> = (0..4).map(|_| s.spawn(|| run_script(&host, "jobs_plain_add", limit))).collect();
        for p in plain {
            assert_eq!(p.join().unwrap(), "yes", "{url}: a job without a key");
        }
        for w in waiting {
            let (took, out) = w.join().unwrap();
            assert!(took < Duration::from_millis(3500), "{url}: took {took:?}: {out}");
        }
        assert!(!holder.is_finished(), "{url}: the key is no longer held");
        assert_eq!(run_script(&host, "jobs_key_waits", limit), "0", "{url}: statements left waiting on the server");
        assert_eq!(holder.join().unwrap(), "rolled back", "{url}");
    });
}

/// While an open transaction holds a key and more scripts than the store has
/// connections wait for it, the scheduler and a script adding another key add
/// their jobs at once; the waits past `JOB_KEY_WAITERS` fail at once, and the
/// others end when the transaction does (found by the 1.2 audit: the waits
/// took every connection those used, a scheduled run was lost, and the other
/// key failed after 10 s).
fn check_held_key_waits(url: &str) {
    // The scripts name phv_jobs_f4f4f4f4f4f4f4f4, rewritten to this namespace.
    let ns = testing::namespace(0xf4);
    let host = jobs_host(url, &ns, &["k"]);
    let pool_size = host.databases["main"].pool_size;
    let waiters = phv_std::jobs::JOB_KEY_WAITERS + pool_size;
    let store = phv_std::jobs::Store::Db(host.databases["main"].clone(), Some(ns));
    // Far within the waits' time limit (8 s): no wait for a connection.
    let at_once = Duration::from_secs(2);
    std::thread::scope(|s| {
        // Holds the key five seconds.
        let holder = s.spawn(|| run_script(&host, "jobs_held_hold", Some(Duration::from_secs(60))));
        std::thread::sleep(Duration::from_secs(2));
        let waiting: Vec<_> = (0..waiters)
            .map(|_| {
                s.spawn(|| {
                    let start = Instant::now();
                    let out = run_script(&host, "jobs_held_add", Some(Duration::from_secs(8)));
                    (start.elapsed(), out)
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(500));
        for minute in [60, 120] {
            let start = Instant::now();
            let key = phv_std::jobs::schedule_key("k", 1_800_000_000 + minute);
            assert!(store.add_scheduled("k", &key).is_ok_and(|r| r.is_some()), "{url}: the scheduled job");
            assert!(start.elapsed() < at_once, "{url}: the scheduled job took {:?}", start.elapsed());
            let start = Instant::now();
            assert_eq!(run_script(&host, "jobs_other_key_add", Some(Duration::from_secs(8))), "added", "{url}");
            assert!(start.elapsed() < at_once, "{url}: another key took {:?}", start.elapsed());
        }
        assert!(!holder.is_finished(), "{url}: the key is no longer held");
        let outs: Vec<(Duration, String)> = waiting.into_iter().map(|w| w.join().unwrap()).collect();
        let refused = outs.iter().filter(|(took, out)| out.contains("other adds wait") && *took < at_once).count();
        assert_eq!(refused, waiters - phv_std::jobs::JOB_KEY_WAITERS, "{url}: {outs:?}");
        let ended = outs.iter().filter(|(_, out)| out == "added" || out == "taken").count();
        assert_eq!(ended, phv_std::jobs::JOB_KEY_WAITERS, "{url}: {outs:?}");
        assert_eq!(outs.iter().filter(|(_, out)| out == "added").count(), 1, "{url}: {outs:?}");
        assert_eq!(holder.join().unwrap(), "rolled back", "{url}");
    });
}

#[test]
fn waits_for_held_keys_leave_the_scheduler_its_connections() {
    sweep_once();
    let _one_at_a_time = heavy();
    for var in ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
        if let Some(url) = server(var) {
            check_held_key_waits(&url);
        }
    }
}

/// A job added with a key inside a `transaction()` on the store's database,
/// while another open transaction holds that key, waits no longer than
/// outside a transaction (10 seconds), then fails with the same error, and
/// the transaction can go on (found by the 1.2 audit: it waited until the
/// other transaction ended or its own time limit, holding the store's lock).
fn check_tx_keyed_wait(url: &str) {
    let host = jobs_host(url, &testing::namespace(0xf5), &["k"]);
    std::thread::scope(|s| {
        // Holds the key fourteen seconds.
        let holder = s.spawn(|| run_script(&host, "jobs_tx_hold", Some(Duration::from_secs(60))));
        // Until the holder sleeps inside its transaction: a fixed pause was
        // too short on a loaded machine, and the add then found no key.
        assert_eq!(run_script(&host, "jobs_tx_wait_held", Some(Duration::from_secs(30))), "held", "{url}");
        let start = Instant::now();
        let out = run_script(&host, "jobs_tx_add", Some(Duration::from_secs(30)));
        let took = start.elapsed();
        assert!(!holder.is_finished(), "{url}: the key is no longer held");
        assert_eq!(out, "error: enqueue(): the key is held by a transaction that has not ended then 1", "{url}");
        assert!(took < Duration::from_secs(12), "{url}: took {took:?}");
        assert!(took >= Duration::from_secs(5), "{url}: gave up after {took:?}");
        assert_eq!(holder.join().unwrap(), "rolled back", "{url}");
    });
}

#[test]
fn keyed_jobs_in_transactions_wait_no_longer_than_outside_them() {
    sweep_once();
    let _one_at_a_time = heavy();
    let urls: Vec<String> = ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"].iter().filter_map(|v| server(v)).collect();
    // Both servers at once: each takes fifteen seconds.
    std::thread::scope(|s| {
        let runs: Vec<_> = urls.iter().map(|url| s.spawn(move || check_tx_keyed_wait(url))).collect();
        for r in runs {
            r.join().unwrap();
        }
    });
}

#[test]
fn keyed_jobs_wait_no_longer_than_the_time_limit() {
    sweep_once();
    let _one_at_a_time = heavy();
    for var in ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
        if let Some(url) = server(var) {
            check_keyed_wait(&url);
        }
    }
}

/// Near the cap, adding jobs costs a few times what it costs far below it,
/// outside a transaction and inside one (found by the 1.1.1 audit: each job
/// counted the table twice, and each `enqueue()` of a transaction once). The
/// two stores are used in turn, so a busy server slows both alike.
fn check_near_cap(url: &str) {
    let limit = Some(Duration::from_secs(300));
    // The scripts name phv_jobs_f2f2f2f2f2f2f2f2 and ..f3, rewritten to these namespaces.
    let near = jobs_host(url, &testing::namespace(0xf2), &["burst", "k"]);
    let far = jobs_host(url, &testing::namespace(0xf3), &["burst", "k"]);
    assert_eq!(run_script(&far, "jobs_plain_add", limit), "yes", "{url}");
    assert_eq!(run_script(&near, "jobs_near_fill", limit), "filled", "{url}");
    // The counts of this process are a second old at most.
    std::thread::sleep(Duration::from_millis(1100));
    let times = |host: &Arc<Host>| -> [u64; 2] {
        let out = run_script(host, "jobs_near_cap", limit);
        let ms: Vec<u64> = out.split(' ').map(|n| n.trim().parse().unwrap_or(u64::MAX / 100)).collect();
        [ms[0], *ms.get(1).unwrap_or(&(u64::MAX / 100))]
    };
    let (mut far_ms, mut near_ms) = ([0, 0], [0, 0]);
    for _ in 0..10 {
        let [a, b] = times(&far);
        let [c, d] = times(&near);
        far_ms = [far_ms[0] + a, far_ms[1] + b];
        near_ms = [near_ms[0] + c, near_ms[1] + d];
    }
    assert_eq!(run_script(&near, "jobs_near_clear", limit), "cleared", "{url}");
    eprintln!(
        "{url}: 200 jobs far below the cap {far_ms:?} ms, near it {near_ms:?} ms (outside, inside a transaction)"
    );
    // Outside a transaction an add near the cap takes the store's lock: a few
    // more round trips and a commit of their own, which cost more on a slow
    // server (5.7 times on CI's MySQL, 2.3 here). Inside a transaction it costs
    // no more (the old code counted the table there: 5 to 6.3 times).
    for (i, times) in [10, 4].into_iter().enumerate() {
        assert!(near_ms[i] <= far_ms[i].max(100) * times, "{url}: far {far_ms:?} ms, near {near_ms:?} ms");
    }
}

#[test]
fn adding_jobs_near_the_cap_stays_cheap() {
    sweep_once();
    let _one_at_a_time = heavy();
    for var in ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
        if let Some(url) = server(var) {
            check_near_cap(&url);
        }
    }
}

/// How long the delayed job of `check_jobs` waits: past the steps that must
/// not take it.
const LATER_SECS: i64 = 15;

#[test]
fn job_stores() {
    sweep_once();
    // The servers at once: each waits for its delayed job.
    std::thread::scope(|s| {
        let runs: Vec<_> = ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL", "PHV_TEST_REDIS_URL"]
            .into_iter()
            .filter_map(server)
            .map(|url| s.spawn(move || check_jobs(&url)))
            .collect();
        for r in runs {
            r.join().unwrap();
        }
    });
}

/// Ends every job of `store`, the given-up ones too.
fn drain(store: &phv_std::jobs::Store) {
    use phv_std::jobs::End;
    while let Some(j) = store.take(Duration::from_secs(60)).unwrap() {
        store.end(&j, End::Done { keyed: false }).unwrap();
    }
    for j in store.list(true).unwrap() {
        store.requeue(&j.id).unwrap();
        let t = store.take(Duration::from_secs(60)).unwrap().unwrap();
        store.end(&t, End::Done { keyed: false }).unwrap();
    }
}

/// The jobs two sites left in the store every site shared before 1.1 gave
/// sites namespaces (`phv_jobs`, `phv:jobs:`): `Store::adopt` as one site
/// moves the waiting and given-up jobs named as that site's scripts into its
/// own store, and none of the other site's; a dry run moves nothing; a
/// running job and a job whose key the site's store has are left; the site's
/// caps hold; `left_in_shared` counts what is left. The lock of the unnamed
/// stores is held by the caller; the names and keys are this run's, and the
/// counts are taken against what the shared store held before, so what a run
/// that died left there does not matter.
fn check_adopt(url: &str) {
    use phv_std::jobs::{End, JobCap, Outcome, Store};
    let spec = DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 4 };
    let shared = Store::Db(spec.clone(), None);
    let a = Store::Db(spec.clone(), Some(testing::namespace(0x20)));
    let b = Store::Db(spec.clone(), Some(testing::namespace(0x21)));
    for s in [&shared, &a, &b] {
        drain(s);
    }
    let run_id = testing::run_id();
    let name = |n: &str| format!("{n}_{run_id}");
    let (key, key_b) = (format!("k_{run_id}"), format!("kb_{run_id}"));
    let base = a.left_in_shared().unwrap();
    // A site without a namespace has nothing to adopt: the shared store is its own.
    assert!(shared.adopt(&[name("a_one")], true).unwrap().is_none());
    assert!(a.adopt(&[], true).unwrap().unwrap().is_empty(), "no names, nothing");
    // Site A's jobs: one given up, one running, two waiting, one with a key;
    // site B's: one waiting, one with the same key.
    let a_dead = shared.add(&name("a_two"), "gone", 0, None).unwrap().unwrap();
    let t = shared.take(Duration::from_secs(60)).unwrap().unwrap();
    assert_eq!(t.id, a_dead);
    shared.end(&t, End::Dead { error: "gone wrong" }).unwrap();
    let a_run = shared.add(&name("a_run"), "", 0, None).unwrap().unwrap();
    let running = shared.take(Duration::from_secs(60)).unwrap().unwrap();
    assert_eq!(running.id, a_run);
    let a_one = shared.add(&name("a_one"), "first", 0, None).unwrap().unwrap();
    let a_other = shared.add(&name("a_one"), "second", 1800, None).unwrap().unwrap();
    // Due in half an hour: a job due now would tie with `a_one` (the same
    // millisecond on a fast machine, where the store takes them by id).
    let a_key = shared.add(&name("a_key"), "", 1800, Some(&key)).unwrap().unwrap();
    let b_one = shared.add(&name("b_one"), "", 0, None).unwrap().unwrap();
    assert!(shared.add(&name("b_key"), "", 0, Some(&key)).unwrap().is_none(), "the key is A's in the shared store");
    let b_key = shared.add(&name("b_key"), "", 0, Some(&key_b)).unwrap().unwrap();
    let names_a: Vec<String> = ["a_one", "a_two", "a_key", "a_run"].iter().map(|n| name(n)).collect();
    let names_b: Vec<String> = ["b_one", "b_key"].iter().map(|n| name(n)).collect();
    // The ids of this run's jobs in a store.
    let ids = |store: &Store| {
        let mut ids: Vec<String> =
            store.list(false).unwrap().into_iter().filter(|j| j.name.ends_with(run_id)).map(|j| j.id).collect();
        ids.sort();
        ids
    };
    let sorted = |mut v: Vec<&String>| {
        v.sort();
        v.into_iter().cloned().collect::<Vec<String>>()
    };
    let outcomes = |jobs: &[phv_std::jobs::Adopted]| {
        let mut o: Vec<(String, &str, Outcome)> = jobs.iter().map(|j| (j.id.clone(), j.state, j.outcome)).collect();
        o.sort();
        o
    };
    assert_eq!(a.left_in_shared().unwrap(), base + 7);
    // The dry run: A's jobs and their outcomes, B's not mentioned, nothing moved.
    let dry = a.adopt(&names_a, false).unwrap().unwrap();
    assert_eq!(
        outcomes(&dry),
        outcomes(&[
            adopted(&a_dead, "failed", Outcome::Moved),
            adopted(&a_run, "running", Outcome::Running),
            adopted(&a_one, "waiting", Outcome::Moved),
            adopted(&a_other, "waiting", Outcome::Moved),
            adopted(&a_key, "waiting", Outcome::Moved),
        ])
    );
    assert_eq!(ids(&shared), sorted(vec![&a_dead, &a_run, &a_one, &a_other, &a_key, &b_one, &b_key]));
    assert!(a.list(false).unwrap().is_empty());
    assert_eq!(a.left_in_shared().unwrap(), base + 7);
    // Under a cap of two jobs, one waiting job does not fit (the given-up one
    // is not counted); the dry run says so, and the move stops there.
    let small = JobCap { jobs: 2, bytes: 1 << 30 };
    let dry = a.adopt_within(&names_a, false, small).unwrap().unwrap();
    let count = |jobs: &[phv_std::jobs::Adopted], o: Outcome| jobs.iter().filter(|j| j.outcome == o).count();
    assert_eq!((count(&dry, Outcome::Moved), count(&dry, Outcome::NoRoom), count(&dry, Outcome::Running)), (3, 1, 1));
    assert!(dry.iter().all(|j| j.outcome != Outcome::NoRoom || j.state == "waiting"));
    assert_eq!(ids(&shared).len(), 7, "a dry run moves nothing");
    let moved = a.adopt_within(&names_a, true, small).unwrap().unwrap();
    assert_eq!(outcomes(&moved), outcomes(&dry), "the move does what the dry run said");
    let mine = a.list(false).unwrap();
    assert_eq!(mine.len(), 3, "{mine:?}");
    assert_eq!(mine.iter().filter(|j| j.state == "waiting").count(), 2);
    let dead = mine.iter().find(|j| j.id == a_dead).unwrap();
    assert_eq!((dead.state, dead.error.as_str(), dead.attempts), ("failed", "gone wrong", 1));
    assert_eq!(ids(&shared).len(), 4);
    assert_eq!(a.left_in_shared().unwrap(), base + 4);
    // With the store's own caps the rest moves; the running job stays.
    let rest = a.adopt(&names_a, true).unwrap().unwrap();
    assert_eq!((count(&rest, Outcome::Moved), count(&rest, Outcome::Running), rest.len()), (1, 1, 2));
    assert_eq!(ids(&shared), sorted(vec![&a_run, &b_one, &b_key]));
    assert_eq!(ids(&a), sorted(vec![&a_dead, &a_one, &a_other, &a_key]));
    assert_eq!(a.left_in_shared().unwrap(), base + 3);
    // A job whose key A's store has now (the moved one) is left in the shared store.
    let a_again = shared.add(&name("a_key"), "", 0, Some(&key)).unwrap().unwrap();
    let again = a.adopt(&names_a, true).unwrap().unwrap();
    assert_eq!(
        outcomes(&again),
        outcomes(&[adopted(&a_run, "running", Outcome::Running), adopted(&a_again, "waiting", Outcome::KeyTaken)])
    );
    assert_eq!(ids(&shared), sorted(vec![&a_run, &a_again, &b_one, &b_key]));
    // B adopts its own: A's are not touched.
    let theirs = b.adopt(&names_b, true).unwrap().unwrap();
    assert_eq!(count(&theirs, Outcome::Moved), 2);
    assert_eq!(ids(&b), sorted(vec![&b_one, &b_key]));
    assert_eq!(ids(&shared), sorted(vec![&a_run, &a_again]));
    assert_eq!(b.left_in_shared().unwrap(), base + 2);
    // The moved jobs run as the site's own, the one due now first, with its
    // payload; the key moved with its job (it is taken in A's store).
    let t = a.take(Duration::from_secs(60)).unwrap().unwrap();
    assert_eq!((t.id.as_str(), t.payload.as_str(), t.attempts), (a_one.as_str(), "first", 1));
    a.end(&t, End::Done { keyed: false }).unwrap();
    assert!(a.add(&name("a_key"), "", 0, Some(&key)).unwrap().is_none());
    // The running job is moved once it has ended and waits again.
    shared.end(&running, End::Retry { at: 0, error: "once more" }).unwrap();
    let last = a.adopt(&names_a, true).unwrap().unwrap();
    assert_eq!(
        outcomes(&last),
        outcomes(&[adopted(&a_run, "waiting", Outcome::Moved), adopted(&a_again, "waiting", Outcome::KeyTaken)])
    );
    assert_eq!(a.left_in_shared().unwrap(), base + 1);
    for s in [&shared, &a, &b] {
        drain(s);
    }
}

fn adopted(id: &str, state: &'static str, outcome: phv_std::jobs::Outcome) -> phv_std::jobs::Adopted {
    phv_std::jobs::Adopted { id: id.to_string(), name: String::new(), state, outcome }
}

#[test]
fn jobs_left_in_the_shared_store_are_adopted() {
    sweep_once();
    // The shared store is one for every test process: one at a time.
    let _unnamed = testing::unnamed();
    for url in ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL", "PHV_TEST_REDIS_URL"].into_iter().filter_map(server) {
        check_adopt(&url);
    }
}
