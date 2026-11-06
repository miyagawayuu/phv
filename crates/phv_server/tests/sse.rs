//! Server-Sent Events: `GET` with `Accept: text/event-stream` to the path of a
//! `*.ws.phv` script streams what is sent and published to the connection.

use phv_server::{serve_on, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CHAT: &str = r#"<?phv
import std.ws.{socket, publish, WsEvent};
import std.http.{request, response};

$ws = socket();
if ($ws->event == WsEvent::Open) {
    $name = request()->query("name") ?? "";
    if ($name == "") {
        response()->status(403);
    } else {
        $ws->set("name", $name);
        $ws->join("lobby");
        $ws->send("welcome " . $name);
    }
} elseif ($ws->event == WsEvent::Close) {
    publish("lobby", ($ws->get("name") ?? "?") . " left");
}
"#;

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn site(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-sse-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "");
    write(&dir, "public/chat.ws.phv", CHAT);
    write(&dir, "public/both.ws.phv", CHAT);
    write(&dir, "public/both.phv", "<?phv\necho \"the page\";\n");
    write(
        &dir,
        "public/notify.phv",
        "<?phv\nimport std.ws.publish;\nimport std.http.request;\npublish(\"lobby\", request()->query(\"m\") ?? \"notice\");\necho \"sent\";\n",
    );
    dir
}

fn start(dir: &Path) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.to_path_buf(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: false,
        time_limit: Duration::from_secs(5),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    addr
}

/// An open event stream: the bytes read so far.
struct Events {
    s: TcpStream,
    seen: String,
}

impl Events {
    fn open(addr: SocketAddr, path: &str, extra: &str) -> Events {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\nAccept: text/event-stream\r\n{extra}\r\n").unwrap();
        Events { s, seen: String::new() }
    }

    /// Reads until `text` has arrived (within 10 s); everything read so far.
    fn wait_for(&mut self, text: &str) -> &str {
        let until = Instant::now() + Duration::from_secs(10);
        let mut buf = [0u8; 4096];
        while !self.seen.contains(text) {
            assert!(Instant::now() < until, "{text:?} did not arrive; got {:?}", self.seen);
            match self.s.read(&mut buf) {
                Ok(0) => panic!("closed before {text:?}; got {:?}", self.seen),
                Ok(n) => self.seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(_) => {}
            }
        }
        &self.seen
    }
}

fn get(addr: SocketAddr, path: &str, extra: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\n{extra}Connection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

/// A free port on the loopback address (for `[admin] listen`).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The admin port, once it answers.
fn admin_at(port: u16) -> SocketAddr {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let until = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < until, "the admin port does not answer");
        std::thread::sleep(Duration::from_millis(20));
    }
    addr
}

/// A gauge of `/metrics`.
fn gauge(admin: SocketAddr, name: &str) -> f64 {
    let text = get(admin, "/metrics", "");
    text.lines()
        .find(|l| l.starts_with(name) && l[name.len()..].starts_with(' '))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no {name} in\n{text}"))
}

/// Bytes of event streams waiting to be sent.
fn events_waiting(admin: SocketAddr) -> f64 {
    gauge(admin, "phv_events_waiting_bytes")
}

/// Polls `f` until it is true; false if it never was within `limit`.
fn eventually(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

/// An event stream over HTTP/2 whose client takes 1 KB at a time (it reads
/// nothing unless `body.frame()` is called), once its welcome has arrived.
async fn h2_stream(
    addr: SocketAddr,
    name: &str,
) -> (hyper::body::Incoming, hyper::client::conn::http2::SendRequest<http_body_util::Empty<bytes::Bytes>>) {
    use http_body_util::{BodyExt, Empty};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut send, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .initial_stream_window_size(1024)
        .initial_connection_window_size(1024)
        .handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = hyper::Request::builder()
        .uri(format!("http://localhost:{}/chat?name={name}", addr.port()))
        .header("accept", "text/event-stream")
        .body(Empty::<bytes::Bytes>::new())
        .unwrap();
    let mut body = send.send_request(req).await.unwrap().into_body();
    let mut seen = String::new();
    while !seen.contains(&format!("data: welcome {name}\n\n")) {
        let frame = tokio::time::timeout(Duration::from_secs(30), body.frame()).await.expect("an event").unwrap();
        if let Ok(d) = frame.unwrap().into_data() {
            seen.push_str(&String::from_utf8_lossy(&d));
        }
    }
    (body, send)
}

/// Tests that fill the server's budgets for event streams run one at a time:
/// on a slow machine, run together they could slow a client that reads for
/// longer than a stream may hold output while the budget is full.
fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ONE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A page that publishes message `m<i>:` followed by `n` bytes.
const FLOOD: &str = "<?phv\nimport std.ws.publish;\nimport std.http.request;\n$n = (request()->query(\"n\") ?? \"1000\")->toInt() ?? 1000;\npublish(\"lobby\", \"m\" . (request()->query(\"i\") ?? \"\") . \":\" . str_repeat(\"x\", $n));\necho \"sent\";\n";

#[test]
fn events_stream_what_is_sent_and_published() {
    let dir = site("stream");
    let addr = start(&dir);
    let mut ann = Events::open(addr, "/chat?name=ann", "");
    let head = ann.wait_for("data: welcome ann\n\n").to_string();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.to_ascii_lowercase().contains("content-type: text/event-stream"), "{head}");
    assert!(head.to_ascii_lowercase().contains("cache-control: no-store"), "{head}");
    assert!(!head.to_ascii_lowercase().contains("content-encoding"), "{head}");
    let mut bob = Events::open(addr, "/chat?name=bob", "Accept-Encoding: gzip\r\n");
    bob.wait_for("data: welcome bob\n\n");

    assert!(get(addr, "/notify", "").ends_with("sent"));
    ann.wait_for("data: notice\n\n");
    bob.wait_for("data: notice\n\n");
    // A text of several lines is one event of several data lines.
    assert!(get(addr, "/notify?m=one%0Atwo%0D%0Athree", "").ends_with("sent"));
    ann.wait_for("data: one\ndata: two\ndata: three\n\n");

    // When a client goes away, the Close event runs.
    drop(ann);
    bob.wait_for("data: ann left\n\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_requests_for_events_get_a_stream() {
    let dir = site("only");
    let addr = start(&dir);
    // Without the Accept header, a *.ws.phv answers nothing (as before).
    assert!(get(addr, "/chat?name=x", "").starts_with("HTTP/1.1 404"));
    // A page at the same path wins.
    assert!(get(addr, "/both?name=x", "Accept: text/event-stream\r\n").ends_with("the page"));
    // The script may refuse, with its status.
    assert!(get(addr, "/chat", "Accept: text/event-stream\r\n").starts_with("HTTP/1.1 403"));
    // Other sites may not open a stream with the visitor's cookies.
    let r = get(addr, "/chat?name=x", "Accept: text/event-stream\r\nOrigin: https://evil.example\r\n");
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
    // HEAD gets the headers only (and no connection is kept open for it).
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "HEAD /chat?name=x HTTP/1.1\r\nHost: test\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut r = String::new();
    s.read_to_string(&mut r).unwrap();
    assert!(r.starts_with("HTTP/1.1 200") && r.contains("text/event-stream"), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Events count until they are sent: a client that does not read holds at
/// most 2 MB, and cannot slow pages or the streams of clients that read
/// (found by the 1.1 audit: 30 such clients held 3.9 GB, then made pages get
/// 503, then made readers miss events).
#[test]
fn events_count_in_the_output_cap() {
    let _one = one_at_a_time();
    use http_body_util::{BodyExt, Empty};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let dir = site("cap");
    // The cap is workers × memory_limit_mb: 64 MB here, 16 MB of it for event
    // streams. Over HTTP/2 the client's small window keeps what it does not
    // read in the server (over TCP the system's buffers would take it).
    write(&dir, "phv.toml", "[server]\nworkers = 1\nh2c = true\n[security]\nmemory_limit_mb = 64\n");
    // A message of 1 MB: an event of 1,000,008 bytes. And one of a million line
    // breaks, whose event (7 MB) is too large to be sent.
    write(
        &dir,
        "public/flood.phv",
        "<?phv\nimport std.ws.publish;\npublish(\"lobby\", str_repeat(\"x\", 1000000));\necho \"sent\";\n",
    );
    write(
        &dir,
        "public/lines.phv",
        "<?phv\nimport std.ws.publish;\npublish(\"lobby\", str_repeat(\"\\n\", 1000000));\necho \"sent\";\n",
    );
    let addr = start(&dir);
    std::thread::sleep(Duration::from_millis(200));
    // A client that reads, over HTTP/1.1.
    let mut reader = Events::open(addr, "/chat?name=reader", "");
    reader.wait_for("data: welcome reader\n\n");
    let reading = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(30);
        let mut buf = vec![0u8; 1 << 16];
        let mut got = 0usize;
        while got < 4 * 1_000_008 && Instant::now() < until {
            match reader.s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(_) => {}
            }
        }
        got
    });
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let got = rt.block_on(async {
        // Three clients that stop reading, each on a connection of its own.
        let mut bodies = Vec::new();
        for name in ["slow", "slow2", "slow3"] {
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (mut send, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                .initial_stream_window_size(1024)
                .initial_connection_window_size(1024)
                .handshake(TokioIo::new(tcp))
                .await
                .unwrap();
            tokio::spawn(conn);
            let req = hyper::Request::builder()
                .uri(format!("http://localhost:{}/chat?name={name}", addr.port()))
                .header("accept", "text/event-stream")
                .body(Empty::<bytes::Bytes>::new())
                .unwrap();
            let mut body = send.send_request(req).await.unwrap().into_body();
            let mut seen = String::new();
            while !seen.contains(&format!("data: welcome {name}\n\n")) {
                let frame =
                    tokio::time::timeout(Duration::from_secs(10), body.frame()).await.expect("an event").unwrap();
                if let Ok(d) = frame.unwrap().into_data() {
                    seen.push_str(&String::from_utf8_lossy(&d));
                }
            }
            bodies.push((body, send));
        }
        // They stop reading while four messages are published, one at a time,
        // so each leaves the connection's queue (1 MB) before the next.
        assert!(tokio::task::spawn_blocking(move || get(addr, "/lines", "")).await.unwrap().ends_with("sent"));
        for _ in 0..4 {
            assert!(tokio::task::spawn_blocking(move || get(addr, "/flood", "")).await.unwrap().ends_with("sent"));
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // Pages are still answered while the streams hold their events: the
        // streams have a budget of their own (found by the 1.1 audit: they
        // pushed the pages' output past its cap, and every page got 503).
        let page = tokio::task::spawn_blocking(move || get(addr, "/notify", "")).await.unwrap();
        assert!(page.ends_with("sent"), "{page}");
        let (mut body, _send) = bodies.remove(0);
        // Then one of them reads: fewer than the four events arrive (its stream
        // ends, or the events it could not hold were skipped).
        let until = Instant::now() + Duration::from_secs(60);
        let mut got = 0usize;
        loop {
            assert!(Instant::now() < until, "still reading after 60 s ({got} bytes)");
            match tokio::time::timeout(Duration::from_secs(5), body.frame()).await {
                Ok(Some(Ok(f))) => got += f.into_data().map(|d| d.len()).unwrap_or(0),
                Ok(_) | Err(_) => break,
            }
        }
        got
    });
    // At most two events of 1 MB were held for the client that did not read.
    assert!(got < 3 * 1_000_008, "{got} bytes: more than 2 MB was held");
    // The client that read got all four.
    let read = reading.join().unwrap();
    assert!(read >= 4 * 1_000_008, "the reader got {read} bytes");
    // Pages are answered once the stream is gone.
    assert!(get(addr, "/notify", "").ends_with("sent"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Event streams have a quarter of the output cap, so pages and streams
/// together stay within it, and pages are still answered (found by the 1.1.1
/// review: each had all of it, so the output could reach twice the cap).
#[test]
fn events_have_a_quarter_of_the_output_cap() {
    let _one = one_at_a_time();
    let dir = site("quarter");
    let port = free_port();
    // 16 MB of output: 4 MB for event streams.
    write(
        &dir,
        "phv.toml",
        &format!("[server]\nworkers = 1\nh2c = true\n[security]\nmemory_limit_mb = 16\n[admin]\nlisten = \"127.0.0.1:{port}\"\n"),
    );
    write(&dir, "public/flood.phv", FLOOD);
    let addr = start(&dir);
    let admin = admin_at(port);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    // Ten clients that do not read, and an event of 1 MB for each: 10 MB.
    let idle = rt.block_on(async {
        let mut all = Vec::new();
        for i in 0..10 {
            all.push(h2_stream(addr, &format!("idle{i}")).await);
        }
        all
    });
    assert!(get(addr, "/flood?i=0&n=1000000", "").ends_with("sent"));
    // The budget fills, and stays within its quarter.
    assert!(eventually(Duration::from_secs(30), || events_waiting(admin) >= 3_000_000.0));
    for _ in 0..20 {
        let w = events_waiting(admin);
        assert!(w <= (4 << 20) as f64, "events waiting: {w}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Pages are answered meanwhile.
    assert!(get(addr, "/notify", "").ends_with("sent"));
    drop(idle);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Streams that do not read fill the events' budget; a client that reads
/// still gets every event: the streams that have held output for a while are
/// dropped to make room for it (found by the 1.1 audit, R17-28: with 80 such
/// streams a reader got 3 of 8 events, until they were dropped 30 s later).
#[test]
fn streams_that_do_not_read_cannot_make_readers_miss_events() {
    let _one = one_at_a_time();
    let dir = site("evict");
    let port = free_port();
    // 16 MB of output, 4 MB of it for event streams: 16 events of 250 KB.
    write(
        &dir,
        "phv.toml",
        &format!("[server]\nworkers = 1\nh2c = true\n[security]\nmemory_limit_mb = 16\n[admin]\nlisten = \"127.0.0.1:{port}\"\n"),
    );
    write(&dir, "public/flood.phv", FLOOD);
    let addr = start(&dir);
    let admin = admin_at(port);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    // Twenty clients that never read, each on a connection of its own.
    let idle = rt.block_on(async {
        let mut all = Vec::new();
        for i in 0..20 {
            all.push(h2_stream(addr, &format!("idle{i}")).await);
        }
        all
    });
    // They fill the budget with an event each, and hold it.
    assert!(get(addr, "/flood?i=w&n=250000", "").ends_with("sent"));
    assert!(eventually(Duration::from_secs(30), || events_waiting(admin) >= (3 << 20) as f64));
    std::thread::sleep(Duration::from_millis(1200));

    // A client that reads, over HTTP/1.1, until the last message.
    let mut reader = Events::open(addr, "/chat?name=reader", "");
    reader.wait_for("data: welcome reader\n\n");
    let reading = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(120);
        let mut buf = vec![0u8; 1 << 16];
        let mut seen = String::new();
        while !seen.contains("data: last\n\n") && Instant::now() < until {
            match reader.s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(_) => {}
            }
        }
        seen
    });
    // Eight events, each after the idle streams have held theirs for a while.
    for k in 0..8 {
        assert!(get(addr, &format!("/flood?i={k}&n=250000"), "").ends_with("sent"));
        std::thread::sleep(Duration::from_millis(1200));
    }
    assert!(get(addr, "/notify?m=last", "").ends_with("sent"));
    let seen = reading.join().unwrap();
    let got: Vec<usize> = (0..8).filter(|k| seen.contains(&format!("data: m{k}:"))).collect();
    assert_eq!(got, (0..8).collect::<Vec<_>>(), "the reader missed events");
    assert!(seen.contains("data: last\n\n"));
    drop(idle);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stream ended while it holds output its client has not taken has its
/// connection dropped, so what it held is freed at once, over HTTP/1.1 and
/// HTTP/2 (found by the 1.1.1 review: it stayed counted until the connection
/// had stalled for 30 s, and over HTTP/2 the end of the stream hid the stall).
#[test]
fn an_ended_stream_frees_its_output_at_once() {
    let _one = one_at_a_time();
    use http_body_util::BodyExt;
    let dir = site("ended");
    let port = free_port();
    write(&dir, "phv.toml", &format!("[server]\nh2c = true\n[admin]\nlisten = \"127.0.0.1:{port}\"\n"));
    write(&dir, "public/flood.phv", FLOOD);
    let addr = start(&dir);
    let admin = admin_at(port);
    let streams = || gauge(admin, "phv_websockets");
    // Well under the 30 s after which a stalled connection is dropped anyway.
    let freed = || eventually(Duration::from_secs(20), || events_waiting(admin) == 0.0);

    // HTTP/1.1: a client (with a small receive buffer) that stops reading after
    // the welcome. Messages are sent until its stream ends (the system's
    // buffers take some first).
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let mut s = rt.block_on(async {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.set_recv_buffer_size(4096).unwrap();
        let s = sock.connect(addr).await.unwrap().into_std().unwrap();
        s.set_nonblocking(false).unwrap();
        s
    });
    write!(s, "GET /chat?name=h1 HTTP/1.1\r\nHost: test\r\nAccept: text/event-stream\r\n\r\n").unwrap();
    let mut buf = [0u8; 4096];
    let mut seen = String::new();
    while !seen.contains("welcome h1") {
        let n = s.read(&mut buf).unwrap();
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert_eq!(streams(), 1.0);
    let mut most = 0f64;
    for k in 0..100 {
        assert!(get(addr, &format!("/flood?i={k}&n=1000000"), "").ends_with("sent"));
        std::thread::sleep(Duration::from_millis(100));
        most = most.max(events_waiting(admin));
        if streams() == 0.0 {
            break;
        }
    }
    assert!(eventually(Duration::from_secs(30), || streams() == 0.0), "the stream did not end");
    assert!(most >= 1_000_000.0, "the stream held {most} bytes");
    assert!(freed(), "{} bytes are still held", events_waiting(admin));
    drop(s);

    // HTTP/2: its stream ends when a third event of 1 MB does not fit in 2 MB.
    let (body, send) = rt.block_on(h2_stream(addr, "h2"));
    for k in 0..2 {
        assert!(get(addr, &format!("/flood?i={k}&n=1000000"), "").ends_with("sent"));
    }
    assert!(eventually(Duration::from_secs(30), || events_waiting(admin) >= 2_000_000.0));
    assert!(get(addr, "/flood?i=2&n=1000000", "").ends_with("sent"));
    assert!(freed(), "{} bytes are still held", events_waiting(admin));
    // The connection is gone: the client gets what was on the way, then an error.
    rt.block_on(async {
        let mut body = body;
        let end =
            tokio::time::timeout(Duration::from_secs(30), async { while let Some(Ok(_)) = body.frame().await {} });
        assert!(end.await.is_ok(), "the stream did not end");
    });
    drop(send);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Event streams over HTTP/2 (here without TLS, `[server] h2c`).
#[test]
fn events_stream_over_http2() {
    use http_body_util::{BodyExt, Empty};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let dir = site("h2");
    write(&dir, "phv.toml", "[server]\nh2c = true\n");
    let addr = start(&dir);
    std::thread::sleep(Duration::from_millis(200));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut send, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp)).await.unwrap();
        tokio::spawn(conn);
        let req = hyper::Request::builder()
            .uri(format!("http://localhost:{}/chat?name=h2", addr.port()))
            .header("accept", "text/event-stream")
            .body(Empty::<bytes::Bytes>::new())
            .unwrap();
        let resp = send.send_request(req).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
        let mut body = resp.into_body();
        let mut seen = String::new();
        let mut published = false;
        while !seen.contains("data: notice\n\n") {
            let frame = tokio::time::timeout(Duration::from_secs(10), body.frame()).await.expect("an event").unwrap();
            if let Ok(data) = frame.unwrap().into_data() {
                seen.push_str(&String::from_utf8_lossy(&data));
            }
            if seen.contains("data: welcome h2\n\n") && !published {
                published = true;
                tokio::task::spawn_blocking(move || get(addr, "/notify", "")).await.unwrap();
            }
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// An idle event stream over HTTP/2 stays open past the 30 s write stall:
/// its comment lines keep it moving (`--ignored`: it takes 40 s).
#[test]
#[ignore]
fn idle_http2_streams_stay_open() {
    use http_body_util::{BodyExt, Empty};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    let dir = site("h2idle");
    write(&dir, "phv.toml", "[server]\nh2c = true\n");
    let addr = start(&dir);
    std::thread::sleep(Duration::from_millis(200));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut send, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp)).await.unwrap();
        tokio::spawn(conn);
        let req = hyper::Request::builder()
            .uri(format!("http://localhost:{}/chat?name=idle", addr.port()))
            .header("accept", "text/event-stream")
            .body(Empty::<bytes::Bytes>::new())
            .unwrap();
        let mut body = send.send_request(req).await.unwrap().into_body();
        let start = Instant::now();
        let mut seen = String::new();
        let mut published = false;
        while !seen.contains("data: notice\n\n") {
            if start.elapsed() > Duration::from_secs(40) && !published {
                published = true;
                tokio::task::spawn_blocking(move || get(addr, "/notify", "")).await.unwrap();
            }
            match tokio::time::timeout(Duration::from_secs(1), body.frame()).await {
                Ok(Some(Ok(f))) => {
                    if let Ok(d) = f.into_data() {
                        seen.push_str(&String::from_utf8_lossy(&d));
                    }
                }
                Ok(other) => panic!("the stream ended after {:?}: {:?}", start.elapsed(), other.map(|r| r.is_ok())),
                Err(_) => {}
            }
        }
        assert!(seen.contains(": ping"), "{seen}");
    });
    let _ = std::fs::remove_dir_all(&dir);
}
