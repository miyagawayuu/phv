//! An event stream must get back to having sent everything within the time a
//! write may stall (30 s; 2 s here, for every server of this test process).

use http_body_util::{BodyExt, Empty};
use hyper_util::rt::{TokioExecutor, TokioIo};
use phv_server::{serve_on, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

const CHAT: &str = r#"<?phv
import std.ws.{socket, WsEvent};
import std.http.request;

$ws = socket();
if ($ws->event == WsEvent::Open) {
    $name = request()->query("name") ?? "";
    $ws->join("lobby");
    $ws->send("welcome " . $name);
}
"#;

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn get(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

type Stream = (hyper::body::Incoming, hyper::client::conn::http2::SendRequest<Empty<bytes::Bytes>>);

/// An event stream over HTTP/2 whose client gives flow-control windows of
/// `window` bytes.
async fn open(addr: SocketAddr, name: &str, window: u32) -> Stream {
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut send, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .initial_stream_window_size(window)
        .initial_connection_window_size(window)
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

/// A client that reads, but too little to ever catch up, is ended; one that
/// reads everything stays (found by the 1.1.1 review: a client that took a
/// little now and then kept its stream's output counted for as long as it
/// liked, as the connection never stalled).
#[test]
fn trickling_readers_are_ended() {
    phv_server::set_event_drain_for_tests(Duration::from_secs(2));
    let dir = std::env::temp_dir().join(format!("phv-sse-drain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[server]\nh2c = true\n");
    write(&dir, "public/chat.ws.phv", CHAT);
    write(
        &dir,
        "public/flood.phv",
        "<?phv\nimport std.ws.publish;\nimport std.http.request;\npublish(\"lobby\", (request()->query(\"m\") ?? \"\") . str_repeat(\"x\", 64000));\necho \"sent\";\n",
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.clone(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: false,
        time_limit: Duration::from_secs(5),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let (mut slow, _s1) = open(addr, "slow", 1024).await;
        let (mut fast, _s2) = open(addr, "fast", 4 << 20).await;
        // The fast client reads everything as it comes.
        let fast_reading = tokio::spawn(async move {
            let mut seen = String::new();
            let until = Instant::now() + Duration::from_secs(120);
            while !seen.contains("last") && Instant::now() < until {
                match tokio::time::timeout(Duration::from_secs(1), fast.frame()).await {
                    Ok(Some(Ok(f))) => {
                        if let Ok(d) = f.into_data() {
                            seen.push_str(&String::from_utf8_lossy(&d));
                        }
                    }
                    Ok(_) => break,
                    Err(_) => {}
                }
            }
            seen
        });
        // Four events of 64 KB, 256 KB in all.
        for k in 0..4 {
            let r = tokio::task::spawn_blocking(move || get(addr, &format!("/flood?m=e{k}"))).await.unwrap();
            assert!(r.ends_with("sent"), "{r}");
        }
        // The slow client takes 1 KB every 50 ms: all of it would take 13 s, and
        // it never gets back to having nothing waiting.
        let until = Instant::now() + Duration::from_secs(120);
        let mut read = 0usize;
        loop {
            assert!(Instant::now() < until, "the slow stream was not ended ({read} bytes read)");
            match tokio::time::timeout(Duration::from_secs(10), slow.frame()).await {
                Ok(Some(Ok(f))) => read += f.into_data().map(|d| d.len()).unwrap_or(0),
                // Ended (the connection was dropped), or nothing more came.
                Ok(_) | Err(_) => break,
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(read < 4 * 64_000, "the slow client read all {read} bytes: its stream was not ended");
        // The fast client's stream is still open.
        let r = tokio::task::spawn_blocking(move || get(addr, "/flood?m=last")).await.unwrap();
        assert!(r.ends_with("sent"), "{r}");
        let seen = fast_reading.await.unwrap();
        for k in 0..4 {
            assert!(seen.contains(&format!("data: e{k}x")), "the fast client missed e{k}");
        }
        assert!(seen.contains("data: lastx"), "the fast client's stream ended");
    });
    let _ = std::fs::remove_dir_all(&dir);
}
