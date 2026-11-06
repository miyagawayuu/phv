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
