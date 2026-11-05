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
