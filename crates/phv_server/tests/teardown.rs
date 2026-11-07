//! A page's response goes out before its VM is torn down (its database
//! connection rolls back and returns to its pool after the response), and
//! the worker stays held until the teardown is done. Against the real
//! server, in a process of its own: the kept SQLite connections are counted
//! for the whole process.

use phv_server::{serve_on, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn request(addr: SocketAddr, raw: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.lines().next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    (status, body.to_string())
}

fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    request(addr, &format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"))
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// A free port on the loopback address (for `[admin] listen`).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Waits until the admin port answers.
fn admin_at(port: u16) -> SocketAddr {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    for _ in 0..200 {
        if TcpStream::connect(addr).is_ok() {
            return addr;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the admin port does not answer");
}

/// `phv_workers_busy` from `/metrics`: the permits held now.
fn busy(admin: SocketAddr) -> usize {
    let (_, text) = get(admin, "/metrics");
    text.lines()
        .find(|l| l.starts_with("phv_workers_busy "))
        .unwrap_or_else(|| panic!("no phv_workers_busy in\n{text}"))
        .rsplit(' ')
        .next()
        .unwrap()
        .parse::<f64>()
        .unwrap() as usize
}

/// Idle SQLite connections kept for the next script.
fn kept() -> usize {
    phv_std::sqlite_kept_stats().0
}

/// Waits (up to a second) for `n` kept connections.
fn wait_kept(n: usize) {
    let until = Instant::now() + Duration::from_secs(1);
    while kept() != n {
        assert!(Instant::now() < until, "{} connections kept, not {n}", kept());
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A page that writes to a SQLite database, with a heap that takes a while
/// to tear down (the VM drops its objects before the connection returns).
const DB_PAGE: &str = "<?phv
import std.db.Database;
$db = Database::open(\"data/t.db\");
$db->execute(sql\"CREATE TABLE IF NOT EXISTS t (a INTEGER)\");
$db->execute(sql\"INSERT INTO t (a) VALUES (1)\");
class Node {
    public int $a = 0;
}
list<Node> $keep = [];
for ($i = 0; $i < 300000; $i++) {
    $keep[] = new Node();
}
echo \"ok\";
";

fn site(name: &str, workers: usize) -> (PathBuf, SocketAddr, SocketAddr) {
    let dir = std::env::temp_dir().join(format!("phv-teardown-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let port = free_port();
    write(
        &dir,
        "phv.toml",
        &format!(
            "[permissions]\nwrite = [\"data/\"]\n\n[server]\nworkers = {workers}\n\n[admin]\nlisten = \"127.0.0.1:{port}\"\n"
        ),
    );
    std::fs::create_dir_all(dir.join("data")).unwrap();
    write(&dir, "public/db.phv", DB_PAGE);
    write(&dir, "public/index.phv", "<?phv echo \"plain\";");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.clone(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: true,
        time_limit: Duration::from_secs(20),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    let admin = admin_at(port);
    (dir, addr, admin)
}

/// The tests share the process (and its count of kept connections): one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The response arrives while the page's connection is still the page's;
/// the connection returns to the pool after, and a page that needs the only
/// worker runs once the teardown is done.
#[test]
fn responses_are_sent_before_the_vm_is_torn_down() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, addr, admin) = site("one", 1);
    // (Connections of the other test's site may still be kept.)
    let base = kept();

    let started = Instant::now();
    let (status, body) = get(addr, "/db");
    let at_response = kept();
    let held = busy(admin);
    assert_eq!((status, body.as_str()), (200, "ok"));
    assert_eq!(at_response, base, "the response came before the connection returned");
    assert_eq!(held, 1, "the worker is held while the VM is torn down");
    wait_kept(base + 1);

    // The next page waits for the worker: by the time it answers, the first
    // page's teardown is done.
    let (status, body) = get(addr, "/db");
    assert_eq!((status, body.as_str()), (200, "ok"));
    assert_eq!(kept(), base, "the second page took the kept connection");
    let (status, body) = get(addr, "/");
    assert_eq!((status, body.as_str()), (200, "plain"));
    assert_eq!(kept(), base + 1, "the plain page ran after the teardown gave the connection back");
    eprintln!("three requests in {:?}", started.elapsed());

    let _ = std::fs::remove_dir_all(&dir);
}

/// With four workers, four pages being torn down still count as running:
/// no more than four pages are on the way at once.
#[test]
fn the_worker_limit_counts_pages_being_torn_down() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, addr, admin) = site("four", 4);
    // (Connections of the other test's site may still be kept.)
    let base = kept();

    let pages: Vec<_> = (0..4).map(|_| std::thread::spawn(move || get(addr, "/db"))).collect();
    for p in pages {
        assert_eq!(p.join().unwrap(), (200, "ok".to_string()));
    }
    // Every response is in; the four VMs are still being torn down.
    let held = busy(admin);
    let returned = kept() - base;
    assert!(returned < 4, "{returned} connections returned before the fourth response");
    assert!(held >= 4 - returned, "{held} workers held while {} teardowns are running", 4 - returned);
    wait_kept(base + 4);
    assert_eq!(busy(admin), 0);

    let _ = std::fs::remove_dir_all(&dir);
}
