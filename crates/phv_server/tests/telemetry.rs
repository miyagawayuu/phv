//! Traces exported to an OpenTelemetry collector (`[telemetry]`): the
//! request's span, the spans of the requests its page sends (whose
//! `traceparent` names them), and the trace a trusted proxy started.

use phv_server::{serve_until, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// Reads one request: its head (lowercase header names) and body.
fn read_request(s: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            let head = text[..end].to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            let mut body = buf[end + 4..].to_vec();
            while body.len() < len {
                let n = s.read(&mut chunk).ok()?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
            return Some((head, String::from_utf8_lossy(&body).into()));
        }
    }
}

/// The requests a recorder received: head and body.
type Received = Arc<Mutex<Vec<(String, String)>>>;

/// A server that keeps what it receives and answers 200 with `{}`.
fn recorder() -> (SocketAddr, Received) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let got = Arc::new(Mutex::new(Vec::new()));
    let g = got.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            if let Some(r) = read_request(&mut s) {
                g.lock().unwrap().push(r);
            }
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        }
    });
    (addr, got)
}

fn get(addr: SocketAddr, path: &str, extra: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\n{extra}Connection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn site(collector: SocketAddr, api: SocketAddr) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-telemetry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        "phv.toml",
        &format!(
            "[server]\ntrust_request_id = true\n\n[telemetry]\nendpoint = \"http://{collector}\"\nservice_name = \"shop\"\n\
             headers = {{ \"X-Collector-Key\" = \"k1\" }}\n\n[http.api]\nurl = \"http://{api}\"\n"
        ),
    );
    write(
        &dir,
        "public/call.phv",
        "<?phv\nimport std.net.HttpClient;\n$r = HttpClient::connect(\"api\")->get(\"/stock\", [\"secret\" => \"s3\"]);\necho $r->status;\n",
    );
    dir
}

#[test]
fn requests_and_their_calls_are_exported_as_spans() {
    let (collector, exported) = recorder();
    let (api, called) = recorder();
    let dir = site(collector, api);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.clone(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: false,
        time_limit: Duration::from_secs(10),
        watch: false,
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = std::thread::spawn(move || {
        serve_until(listener, opts, async move {
            let _ = stop_rx.await;
        })
    });

    let trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let caller = "00f067aa0ba902b7";
    let resp = get(addr, "/call?q=private", &format!("traceparent: 00-{trace}-{caller}-01\r\n"));
    assert!(resp.ends_with("200"), "{resp}");
    // Not sampled by the caller: propagated, not recorded.
    let other = "0af7651916cd43dd8448eb211c80319c";
    let resp = get(addr, "/call", &format!("traceparent: 00-{other}-b7ad6b7169203331-00\r\n"));
    assert!(resp.ends_with("200"), "{resp}");

    let calls = called.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    let tp = |head: &str| head.lines().find_map(|l| l.strip_prefix("traceparent:")).unwrap().trim().to_string();
    let first = tp(&calls[0].0);
    assert!(first.starts_with(&format!("00-{trace}-")) && first.ends_with("-01"), "{first}");
    let client_span = first.split('-').nth(2).unwrap().to_string();
    assert_ne!(client_span, caller);
    let second = tp(&calls[1].0);
    assert!(second.starts_with(&format!("00-{other}-")) && second.ends_with("-00"), "{second}");

    // Stopping the server sends what is queued.
    stop_tx.send(()).unwrap();
    server.join().unwrap().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    while exported.lock().unwrap().is_empty() {
        assert!(Instant::now() < until, "nothing exported");
        std::thread::sleep(Duration::from_millis(50));
    }
    let posts = exported.lock().unwrap().clone();
    let (head, _) = &posts[0];
    assert!(head.starts_with("post /v1/traces "), "{head}");
    assert!(head.contains("content-type: application/json") && head.contains("x-collector-key: k1"), "{head}");
    let spans: Vec<serde_json::Value> = posts
        .iter()
        .flat_map(|(_, body)| {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(v["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"], "shop");
            v["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap().clone()
        })
        .collect();
    assert_eq!(spans.len(), 2, "the unsampled request is not recorded: {spans:?}");
    let server = spans.iter().find(|s| s["kind"] == 2).expect("a server span");
    let client = spans.iter().find(|s| s["kind"] == 3).expect("a client span");
    assert_eq!(server["traceId"], trace);
    assert_eq!(server["parentSpanId"], caller);
    assert_eq!(server["name"], "GET /call");
    assert_eq!(client["traceId"], trace);
    assert_eq!(client["spanId"], client_span.as_str());
    assert_eq!(client["parentSpanId"], server["spanId"]);
    assert_eq!(client["name"], "GET");
    let all = serde_json::to_string(&spans).unwrap();
    assert!(!all.contains("private") && !all.contains("s3"), "no query strings: {all}");
    assert!(all.contains("\"phv.http.service\"") && all.contains("\"http.route\""), "{all}");
    let _ = std::fs::remove_dir_all(&dir);
}
