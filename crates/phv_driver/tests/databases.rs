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
use phv_vm::{DatabaseSpec, Host, Limits};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/db")
}

fn host(url: &str) -> Arc<Host> {
    let spec = DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 1 };
    Arc::new(Host { root: root(), databases: BTreeMap::from([("main".to_string(), spec)]), ..Host::default() })
}

fn run_script(host: &Arc<Host>, name: &str, max_time: Option<Duration>) -> String {
    let path = root().join(format!("{name}.phv"));
    let c = match compile_with_overlay(&root(), &path, HashMap::new()) {
        Ok(c) => c,
        Err(e) => return e.render(false),
    };
    let limits = Limits { max_time, ..Limits::default() };
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
}

#[test]
fn mysql() {
    let Some(url) = server("PHV_TEST_MYSQL_URL") else { return };
    let host = host(&url);
    check(&host, "mysql");
    check(&host, "mysql_after");
    check_time_limit(&host, "mysql_slow");
}

#[test]
fn redis() {
    let Some(url) = server("PHV_TEST_REDIS_URL") else { return };
    check(&host(&url), "redis");
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
