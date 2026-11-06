//! Background jobs (`[jobs]`, `std.jobs`) run by the real server, with the
//! memory store. The database stores are tested in phv_driver/tests/databases.rs.

use phv_server::{serve_on, serve_until, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    (head.split(' ').nth(1).unwrap().parse().unwrap(), body.to_string())
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn site(name: &str, toml: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-jobs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", &format!("[permissions]\nwrite = [\"data/\"]\n{toml}"));
    write(&dir, "data/.keep", "");
    // A page that adds the job named in `?job=`, with `?p=` as its payload.
    write(
        &dir,
        "public/add.phv",
        "<?phv\nimport std.http.request;\nimport std.jobs.enqueue;\n\
         $k = request()->query(\"key\");\n\
         $id = enqueue(request()->query(\"job\") ?? \"\", request()->query(\"p\") ?? \"\", key: $k);\n\
         echo $id ?? \"duplicate\";\n",
    );
    // A job that appends what it got to data/log.txt.
    write(
        &dir,
        "jobs/note.phv",
        "<?phv\nimport std.jobs.job;\n$j = job();\nfile_append(\"data/log.txt\", $j->name . \" \" . $j->payload . \" \" . $j->attempt . \"\\n\");\n",
    );
    dir
}

fn options(dir: &Path) -> ServeOptions {
    ServeOptions {
        root: dir.to_path_buf(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: false,
        time_limit: Duration::from_secs(5),
        watch: false,
    }
}

fn start(dir: &Path) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = options(dir);
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    addr
}

/// Waits until `data/log.txt` has `n` lines; its lines.
fn log_lines(dir: &Path, n: usize, within: Duration) -> Vec<String> {
    let until = Instant::now() + within;
    loop {
        let text = std::fs::read_to_string(dir.join("data/log.txt")).unwrap_or_default();
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        if lines.len() >= n || Instant::now() >= until {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn pages_add_jobs_that_run_once() {
    let dir = site("once", "[jobs]\n");
    let addr = start(&dir);
    let (status, id) = get(addr, "/add?job=note&p=hello");
    assert_eq!(status, 200, "{id}");
    assert_eq!(id.len(), 32, "{id}");
    assert_eq!(log_lines(&dir, 1, Duration::from_secs(5)), ["note hello 1"]);
    // A key adds a job once.
    assert_ne!(get(addr, "/add?job=note&p=k1&key=welcome:1").1, "duplicate");
    assert_eq!(get(addr, "/add?job=note&p=k2&key=welcome:1").1, "duplicate");
    assert_eq!(log_lines(&dir, 2, Duration::from_secs(5)).len(), 2);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(log_lines(&dir, 3, Duration::ZERO).len(), 2, "nothing ran twice");
    // Unknown jobs and bad arguments are errors of the page.
    let (status, body) = get(addr, "/add?job=nothing");
    assert_eq!(status, 500, "{body}");
    let (status, _) = get(addr, "/add?job=..%2Fpublic%2Fadd");
    assert_eq!(status, 500);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failing_jobs_run_again_then_are_given_up() {
    let dir = site("retry", "[jobs]\nmax_attempts = 3\nretry_delay = 1\ntime_limit = 1\n");
    write(
        &dir,
        "jobs/flaky.phv",
        "<?phv\nimport std.jobs.job;\n$j = job();\nfile_append(\"data/log.txt\", \"run \" . $j->attempt . \"\\n\");\n\
         if ($j->attempt < 2) {\n    throw new RuntimeException(\"not yet\");\n}\n",
    );
    write(&dir, "jobs/forever.phv", "<?phv\nwhile (true) {\n}\n");
    let addr = start(&dir);
    get(addr, "/add?job=flaky");
    // Run 1 fails; run 2, a second later, succeeds.
    assert_eq!(log_lines(&dir, 2, Duration::from_secs(10)), ["run 1", "run 2"]);
    // A job past its time limit is stopped and runs again (1 s, then 2 s later);
    // after max_attempts runs it is given up and kept with its error.
    get(addr, "/add?job=forever");
    let store = phv_std::jobs::Store::Memory(dir.canonicalize().unwrap().to_string_lossy().into_owned());
    let until = Instant::now() + Duration::from_secs(20);
    let failed = loop {
        let f = store.list(true).unwrap();
        if !f.is_empty() || Instant::now() > until {
            break f;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!((failed[0].name.as_str(), failed[0].attempts), ("forever", 3));
    assert!(failed[0].error.contains("maximum execution time"), "{}", failed[0].error);
    // And can be put back in the queue.
    assert!(store.requeue(&failed[0].id).unwrap());
    assert!(!store.requeue(&failed[0].id).unwrap(), "it is no longer a failed job");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Jobs have threads of their own: a long job does not take the worker a page needs.
#[test]
fn jobs_do_not_take_the_workers_of_pages() {
    let dir = site("workers", "[server]\nworkers = 1\n[jobs]\nworkers = 2\ntime_limit = 10\n");
    write(
        &dir,
        "jobs/slow.phv",
        "<?phv\n$t = microtime();\nwhile (microtime() - $t < 3.0) {\n}\nfile_append(\"data/log.txt\", \"slow\\n\");\n",
    );
    write(&dir, "public/index.phv", "<?phv\necho \"page\";\n");
    let addr = start(&dir);
    get(addr, "/add?job=slow");
    get(addr, "/add?job=slow");
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    assert_eq!(get(addr, "/").1, "page");
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    assert_eq!(log_lines(&dir, 2, Duration::from_secs(10)), ["slow", "slow"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A job's errors stop the server from starting, as a route error does.
#[test]
fn a_job_that_does_not_compile_stops_the_server() {
    let dir = site("broken", "[jobs]\n");
    write(&dir, "jobs/broken.phv", "<?phv\n$x = ;\n");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let err = serve_on(listener, options(&dir)).unwrap_err();
    assert!(err.contains("jobs/broken.phv"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pages can use job() only in a job, and a site without [jobs] cannot add one.
#[test]
fn std_jobs_outside_a_job() {
    let dir = site("outside", "");
    write(&dir, "public/who.phv", "<?phv\nimport std.jobs.job;\necho job()->name;\n");
    let addr = start(&dir);
    assert_eq!(get(addr, "/add?job=note").0, 500, "no [jobs]");
    assert_eq!(get(addr, "/who").0, 500, "not a job");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The server waits for a running job when it stops.
#[test]
fn stopping_waits_for_running_jobs() {
    let dir = site("stop", "[jobs]\ntime_limit = 10\n");
    write(
        &dir,
        "jobs/slow.phv",
        "<?phv\n$t = microtime();\nwhile (microtime() - $t < 1.5) {\n}\nfile_append(\"data/log.txt\", \"done\\n\");\n",
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let opts = options(&dir);
    let server = std::thread::spawn(move || {
        serve_until(listener, opts, async {
            let _ = rx.await;
        })
    });
    std::thread::sleep(Duration::from_millis(200));
    get(addr, "/add?job=slow");
    std::thread::sleep(Duration::from_millis(300));
    let _ = tx.send(());
    server.join().unwrap().unwrap();
    assert_eq!(log_lines(&dir, 1, Duration::ZERO), ["done"]);
    let _ = std::fs::remove_dir_all(&dir);
}
