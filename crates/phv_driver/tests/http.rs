//! HTTP services: each `tests/http/<name>.phv` sends requests to a small
//! server run by the test, and its output is compared with the `.out` file
//! next to it. `PHV_BLESS=1` rewrites the expected files.

use phv_driver::{compile_with_overlay, run, RunOptions};
use phv_vm::{Host, HttpSpec, Limits};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/http")
}

fn spec(url: &str, tls: Option<bool>) -> HttpSpec {
    HttpSpec {
        url: Some(url.to_string()),
        url_env: None,
        tls,
        headers: vec![
            ("Authorization".to_string(), Some("Bearer test-key".to_string())),
            ("X-Site".to_string(), Some("phv-test".to_string())),
            ("X-Unset".to_string(), None),
        ],
        headers_env: vec![
            ("Authorization".to_string(), "API_KEY".to_string()),
            ("X-Unset".to_string(), "UNSET_VAR".to_string()),
        ],
    }
}

fn host(url: &str, tls: Option<bool>) -> Arc<Host> {
    let mut api = spec(url, tls);
    // The service used by most scripts has no header with an unset variable.
    api.headers.retain(|(n, _)| n != "X-Unset");
    api.headers_env.retain(|(n, _)| n != "X-Unset");
    let http = BTreeMap::from([("api".to_string(), api), ("broken".to_string(), spec(url, tls))]);
    Arc::new(Host { root: root(), http, ..Host::default() })
}

/// A request as the test server received it.
struct Received {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// Reads one request: the request line and headers, and a body of Content-Length bytes.
fn read_request(s: &mut TcpStream) -> Option<Received> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (method, target, headers, used) = loop {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut slots = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut slots);
        if let Ok(httparse::Status::Complete(used)) = req.parse(&buf) {
            let headers: Vec<(String, String)> = req
                .headers
                .iter()
                .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).to_string()))
                .collect();
            break (req.method?.to_string(), req.path?.to_string(), headers, used);
        }
    };
    let len: usize = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[used..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Received { method, target, headers, body: String::from_utf8_lossy(&body).to_string() })
}

/// Answers by path: `/echo...` describes the request as JSON, `/chunked` uses
/// chunked encoding, `/redirect` answers 302, `/missing` 404, `/slow` never answers.
fn server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let Some(Received { method, target, headers, body }) = read_request(&mut s) else { return };
                let path = target.split('?').next().unwrap_or("").to_string();
                let reply = match path.as_str() {
                    "/v1/chunked" => {
                        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\n\
                         6\r\nchunks\r\n9\r\n together\r\n0\r\n\r\n"
                            .to_string()
                    }
                    "/v1/redirect" => {
                        "HTTP/1.1 302 Found\r\nLocation: http://elsewhere.invalid/\r\nContent-Length: 0\r\n\r\n".into()
                    }
                    "/v1/missing" => "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found".into(),
                    "/v1/wait" => {
                        std::thread::sleep(Duration::from_millis(800));
                        "HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nwaited".into()
                    }
                    "/v1/slow" => {
                        std::thread::sleep(Duration::from_secs(60));
                        return;
                    }
                    // One byte of the headers every 300 ms: each read finishes in time,
                    // the response never does.
                    "/v1/drip" => {
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\n");
                        for _ in 0..200 {
                            if s.write_all(b"X").is_err() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(300));
                        }
                        return;
                    }
                    "/v1/chunk-overflow" => {
                        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\nffffffffffffffff\r\n".into()
                    }
                    _ => {
                        // Headers the script cannot see otherwise, sorted for a stable output.
                        let mut hs: Vec<String> = headers
                            .iter()
                            .filter(|(n, _)| !n.eq_ignore_ascii_case("content-length"))
                            .map(|(n, v)| format!("{}: {v}", n.to_ascii_lowercase()))
                            .collect();
                        hs.sort();
                        let json =
                            serde_json::json!({ "method": method, "target": target, "headers": hs, "body": body });
                        let text = json.to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Multi: a\r\nX-Multi: b\r\nContent-Length: {}\r\n\r\n{text}",
                            text.len()
                        )
                    }
                };
                // The connection is closed after the answer, so the answer says so
                // (a POST on a kept connection the server closed is not sent again).
                let reply = reply.replacen("\r\n", "\r\nConnection: close\r\n", 1);
                let _ = s.write_all(reply.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
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

fn golden(name: &str, actual: &str) {
    let expected_path = root().join(format!("{name}.out"));
    if std::env::var_os("PHV_BLESS").is_some() {
        std::fs::write(&expected_path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&expected_path).unwrap_or_default().replace("\r\n", "\n");
    assert_eq!(actual, expected, "output of tests/http/{name}.phv");
}

#[test]
fn requests() {
    let url = server();
    let actual = run_script(&host(&url, None), "requests", Some(Duration::from_secs(30)));
    // The server's port changes from run to run, the version with each release.
    let actual = actual
        .replace(url.trim_start_matches("http://").trim_end_matches("/v1"), "HOST")
        .replace(concat!("phv/", env!("CARGO_PKG_VERSION")), "phv/VERSION");
    golden("requests", &actual);
}

/// Connections are kept between requests to one host, with or without a
/// body: a server that keeps them open sees one connection for the GETs, the
/// POST and the PUT; the most recent one kept is used first; one whose
/// response asked to close, or did not end where its framing said (bytes
/// after the body), is not used again; and one idle for more than a second
/// is taken by a GET but not by a POST.
#[test]
fn connections_are_kept_between_requests() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let conn = count.fetch_add(1, Ordering::SeqCst) + 1;
            std::thread::spawn(move || {
                while let Some(r) = read_request(&mut s) {
                    let close = r.target.ends_with("/close");
                    let body = format!("{} {} on connection {conn}", r.method, r.target);
                    let extra = if close { "Connection: close\r\n" } else { "" };
                    let after = if r.target.ends_with("/extra") { "xyz" } else { "" };
                    let reply =
                        format!("HTTP/1.1 200 OK\r\n{extra}Content-Length: {}\r\n\r\n{body}{after}", body.len());
                    if s.write_all(reply.as_bytes()).is_err() || close {
                        return;
                    }
                }
            });
        }
    });
    let url = format!("http://{addr}/v1");
    let out = run_script(&host(&url, None), "keepalive", Some(Duration::from_secs(30)));
    assert_eq!(
        out,
        "GET /v1/n/0 on connection 1\nGET /v1/n/1 on connection 1\nGET /v1/n/2 on connection 1\n\
         GET /v1/n/3 on connection 1\nGET /v1/n/4 on connection 1\nPOST /v1/p on connection 1\n\
         PUT /v1/q on connection 1\nGET /v1/n/9 on connection 1\nGET /v1/close on connection 1\n\
         POST /v1/extra on connection 2\nPOST /v1/n/10 on connection 3\nGET /v1/n/11 on connection 3\n\
         POST /v1/n/12 on connection 4\nPOST /v1/n/13 on connection 4\n"
    );
    assert_eq!(accepted.load(Ordering::SeqCst), 4);
}

/// A kept connection the server closed meanwhile: a GET is sent again on a
/// new connection (`late_answers` above), a POST only when it was not
/// written whole. A server that got the whole POST and closed without
/// answering may have acted on it: the script gets the error, and the POST
/// is not sent again.
#[test]
fn requests_with_side_effects_are_not_repeated_on_a_closed_connection() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    let seen = posts.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let seen = seen.clone();
            std::thread::spawn(move || {
                while let Some(r) = read_request(&mut s) {
                    if r.method == "POST" {
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                    if r.target.ends_with("/drop") {
                        // Closed without an answer, after the whole request came.
                        return;
                    }
                    let body = format!("{} {}", r.method, r.target);
                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                    if s.write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    let url = format!("http://{addr}/v1");
    let out = run_script(&host(&url, None), "post_closed", Some(Duration::from_secs(30)));
    assert_eq!(out, "GET /v1/warm\nNetworkError\nGET /v1/after\n", "{out}");
    assert_eq!(posts.load(Ordering::SeqCst), 1, "the POST was sent again");
}

/// Found by the 1.0.0 audit: a kept connection on which bytes arrived while
/// it was idle (a second, late response) is not used again, so no request
/// reads another's answer; and a request whose answer came (and was refused
/// as too large) is not sent again on a new connection.
#[test]
fn kept_connections_carry_no_late_answer_and_answered_requests_are_not_repeated() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let big_seen = Arc::new(AtomicUsize::new(0));
    let seen = big_seen.clone();
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let Ok(mut s) = stream else { continue };
            let seen = seen.clone();
            std::thread::spawn(move || {
                while let Some(r) = read_request(&mut s) {
                    let path = r.target.clone();
                    if path.ends_with("/late") {
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nlate");
                        std::thread::sleep(Duration::from_millis(100));
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nFORGED");
                        continue;
                    }
                    if path.ends_with("/big") {
                        seen.fetch_add(1, Ordering::SeqCst);
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 17000000\r\n\r\n");
                        return;
                    }
                    let body = format!("{path} on {n}");
                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                    if s.write_all(reply.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    let url = format!("http://{addr}/v1");
    let out = run_script(&host(&url, None), "late_answers", Some(Duration::from_secs(30)));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "late");
    assert!(lines[1].starts_with("/v1/next on "), "{out}");
    assert!(lines[2].starts_with("/v1/warm on "), "{out}");
    assert!(lines[3].contains("larger than 16 MB"), "{out}");
    assert_eq!(big_seen.load(Ordering::SeqCst), 1, "the request was sent again");
}

/// Waiting for an answer stops at the script's time limit, also when the
/// server keeps sending a byte now and then.
#[test]
fn requests_stop_at_the_time_limit() {
    for script in ["slow", "drip"] {
        let start = Instant::now();
        let out = run_script(&host(&server(), None), script, Some(Duration::from_secs(1)));
        assert!(out.contains("maximum execution time"), "{script}: {out}");
        assert!(start.elapsed() < Duration::from_secs(5), "{script} took {:?}", start.elapsed());
    }
}

/// A chunk size near the largest integer is an error, not a crash.
#[test]
fn huge_chunk_sizes_are_errors() {
    let out = run_script(&host(&server(), None), "chunk_overflow", Some(Duration::from_secs(10)));
    assert!(out.contains("NetworkError") && out.contains("larger than 16 MB"), "{out}");
    assert!(!out.contains("with overflow") && !out.contains("panicked"), "{out}");
}

/// Remote services need TLS unless the configuration says otherwise, and
/// neither the URL nor the API key shows up in messages.
#[test]
fn remote_services_need_tls() {
    let out = run_script(&host("http://api.invalid/secret-path", None), "connect", None);
    assert!(out.contains("use a https:// URL"), "{out}");
    assert!(!out.contains("secret-path") && !out.contains("test-key"), "{out}");
    let out = run_script(&host("http://api.invalid/secret-path", Some(false)), "connect", None);
    assert!(out.contains("HTTP service \"api\""), "{out}");
    assert!(!out.contains("secret-path") && !out.contains("test-key"), "{out}");
}

/// Requests started with `*Async` run at the same time: three answers that
/// each take 800 ms arrive in about 800 ms, not 2.4 s.
#[test]
fn async_requests_run_at_the_same_time() {
    let url = server();
    let start = Instant::now();
    let out = run_script(&host(&url, None), "async", Some(Duration::from_secs(10)));
    let took = start.elapsed();
    golden("async", &out);
    assert!(took < Duration::from_millis(2000), "took {took:?}");
}

/// Waiting for a background request stops at the time limit, and a script
/// cannot start more than 16 at once.
#[test]
fn async_requests_are_limited() {
    let start = Instant::now();
    let out = run_script(&host(&server(), None), "async_slow", Some(Duration::from_secs(1)));
    assert!(out.contains("maximum execution time"), "{out}");
    assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
    let out = run_script(&host(&server(), None), "async_many", Some(Duration::from_secs(10)));
    assert!(out.contains("started 16") && out.contains("at most 16 operations"), "{out}");
}

/// Work a script started and never awaited is waited for when the script
/// ends, so a request cannot leave it running (holding a thread and a
/// connection) after its worker is free again.
#[test]
fn unawaited_requests_are_waited_for() {
    let start = Instant::now();
    let out = run_script(&host(&server(), None), "async_abandoned", Some(Duration::from_secs(10)));
    assert_eq!(out, "ended");
    assert!(start.elapsed() >= Duration::from_millis(700), "took {:?}", start.elapsed());
}
