//! `Redis::pipeline()`: several commands in one round trip, against the Redis
//! server of `PHV_TEST_REDIS_URL` (skipped when it is not set) and a server
//! that stalls. The limits of a single command hold for the whole exchange:
//! the script's memory and time.

use phv_driver::{compile_with_overlay, run, RunOptions};
use phv_std::testing;
use phv_vm::alloc::CountingAlloc;
use phv_vm::{DatabaseSpec, Host, Limits};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/db")
}

fn host(url: &str) -> Arc<Host> {
    let spec = DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 1 };
    Arc::new(Host { root: root(), databases: BTreeMap::from([("main".to_string(), spec)]), ..Host::default() })
}

fn run_limited(host: &Arc<Host>, name: &str, max_time: Option<Duration>, max_memory: Option<usize>) -> String {
    let path = root().join(format!("{name}.phv"));
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

/// Every method of the pipeline, the order of the replies, an error in the
/// middle, PHV's keys, the caps on the commands and their bytes, and a closed
/// connection; the replies over the memory limit stop the script at the limit.
#[test]
fn pipelines_send_several_commands_in_one_round_trip() {
    let Some(url) = std::env::var("PHV_TEST_REDIS_URL").ok().filter(|u| !u.is_empty()) else {
        eprintln!("skipped: PHV_TEST_REDIS_URL is not set");
        return;
    };
    let host = host(&url);
    let actual = run_limited(&host, "redis_pipeline", Some(Duration::from_secs(30)), None);
    let expected = std::fs::read_to_string(root().join("redis_pipeline.out")).unwrap().replace("\r\n", "\n");
    assert_eq!(actual, expected, "output of tests/db/redis_pipeline.phv");

    phv_vm::alloc::reset_thread_peak();
    let before = phv_vm::alloc::thread_allocated();
    let out = run_limited(&host, "redis_pipeline_big", Some(Duration::from_secs(30)), Some(16 << 20));
    assert!(out.contains("pushed"), "{out}");
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{out}");
    assert!(!out.contains("not reached"), "{out}");
    let peak = phv_vm::alloc::thread_peak() - before;
    assert!(peak < 64 << 20, "{} MB allocated at the peak", peak >> 20);
}

/// A server that answers the first command of a pipeline and then nothing:
/// the script ends at its time limit, as with a single command.
#[test]
fn a_stalled_pipeline_ends_at_the_time_limit() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        s.write_all(b"+OK\r\n").unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_secs(3));
        request
    });
    let start = Instant::now();
    let out = run_limited(
        &host(&format!("redis://127.0.0.1:{port}")),
        "redis_pipeline_slow",
        Some(Duration::from_secs(1)),
        None,
    );
    assert!(out.contains("maximum execution time"), "{out}");
    assert!(!out.contains("not reached"), "{out}");
    assert!(start.elapsed() < Duration::from_secs(3), "took {:?}", start.elapsed());
    let request = server.join().unwrap();
    // Both commands were sent together, before any reply.
    assert!(request.contains("SET") && request.contains("GET"), "{request}");
}
