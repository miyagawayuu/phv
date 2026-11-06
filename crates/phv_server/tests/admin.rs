//! The admin port (`[admin]`: `/health`, `/ready`, `/metrics`) and request
//! ids (`[server] request_ids`), against the real server.

use phv_server::{serve_on, serve_until, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn request(addr: SocketAddr, raw: &str) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    Reply { status, headers, body: body.to_string() }
}

fn get(addr: SocketAddr, path: &str, extra: &str) -> Reply {
    request(addr, &format!("GET {path} HTTP/1.1\r\nHost: test\r\n{extra}Connection: close\r\n\r\n"))
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

const HOME: &str = "<?phv\necho \"home\";\n";

fn site(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-admin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A free port on the loopback address (for `[admin] listen`).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
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

/// The value of one line of `/metrics` (`name{labels} value`).
fn metric(text: &str, line_start: &str) -> f64 {
    text.lines()
        .find(|l| l.starts_with(line_start) && l[line_start.len()..].starts_with(' '))
        .unwrap_or_else(|| panic!("no {line_start} in\n{text}"))
        .rsplit(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn health_ready_and_metrics() {
    let dir = site("metrics");
    let port = free_port();
    write(&dir, "phv.toml", &format!("[admin]\nlisten = \"127.0.0.1:{port}\"\n"));
    write(&dir, "public/index.phv", HOME);
    write(&dir, "public/posts/[id].phv", "<?phv\nimport std.http.request;\necho request()->param(\"id\") ?? \"\";\n");
    write(&dir, "public/fail.phv", "<?phv\nthrow new ValueError(\"no\");\n");
    write(&dir, "public/style.css", "body {}");
    let addr = start(&dir);
    let admin = admin_at(port);

    let h = get(admin, "/health", "");
    assert_eq!((h.status, h.body.as_str()), (200, "ok\n"));
    assert_eq!(get(admin, "/ready", "").status, 200);
    assert_eq!(get(admin, "/nothing", "").status, 404);
    // The site's own paths are not on the admin port, nor the admin paths on the site.
    assert_eq!(get(admin, "/posts/1", "").status, 404);
    assert_eq!(get(addr, "/metrics", "").status, 404);

    for path in ["/", "/posts/1", "/posts/2", "/fail", "/style.css", "/missing"] {
        get(addr, path, "");
    }
    let m = get(admin, "/metrics", "");
    assert_eq!(m.status, 200);
    assert!(m.header("content-type").unwrap().starts_with("text/plain; version=0.0.4"));
    let text = &m.body;
    assert_eq!(metric(text, "phv_requests_total{kind=\"page\",code=\"2xx\"}"), 3.0, "{text}");
    assert_eq!(metric(text, "phv_requests_total{kind=\"page\",code=\"5xx\"}"), 1.0);
    assert_eq!(metric(text, "phv_requests_total{kind=\"static\",code=\"2xx\"}"), 1.0);
    assert_eq!(metric(text, "phv_requests_total{kind=\"other\",code=\"4xx\"}"), 2.0, "/missing and /metrics");
    // Routes are the files of public/, not the paths asked for.
    assert_eq!(metric(text, "phv_request_duration_seconds_count{route=\"/posts/[id]\"}"), 2.0);
    assert_eq!(metric(text, "phv_request_duration_seconds_count{route=\"/\"}"), 1.0);
    assert!(!text.contains("/posts/1"), "{text}");
    assert_eq!(metric(text, "phv_script_failures_total{kind=\"exception\"}"), 1.0);
    assert_eq!(metric(text, "phv_up"), 1.0);
    assert!(metric(text, "phv_workers") >= 8.0);
    assert_eq!(metric(text, "phv_workers_busy"), 0.0);
    assert!(text.contains(&format!("phv_build_info{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"))));
    // Every line is a comment or `name{labels} number`.
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let (_, value) = line.rsplit_once(' ').unwrap();
        assert!(value.parse::<f64>().is_ok(), "{line}");
    }
    assert_eq!(request(admin, "POST /health HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n").status, 405);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn metrics_need_the_token() {
    std::env::set_var("PHV_TEST_ADMIN_TOKEN", "t0ken-of-sixteen-bytes");
    let dir = site("token");
    let port = free_port();
    write(&dir, "phv.toml", &format!("[admin]\nlisten = \"127.0.0.1:{port}\"\ntoken_env = \"PHV_TEST_ADMIN_TOKEN\"\n"));
    write(&dir, "public/index.phv", HOME);
    start(&dir);
    let admin = admin_at(port);
    let r = get(admin, "/metrics", "");
    assert_eq!(r.status, 401);
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    assert_eq!(get(admin, "/metrics", "Authorization: Bearer wrong\r\n").status, 401);
    assert_eq!(get(admin, "/metrics", "Authorization: Bearer t0ken-of-sixteen-bytes\r\n").status, 200);
    // Probes that cannot send headers still work.
    assert_eq!(get(admin, "/health", "").status, 200);
    assert_eq!(get(admin, "/ready", "").status, 200);

    // An unset or short token stops the server.
    std::env::set_var("PHV_TEST_ADMIN_SHORT", "short");
    for (var, needle) in [("PHV_TEST_ADMIN_UNSET", "is not set"), ("PHV_TEST_ADMIN_SHORT", "at least 16 bytes")] {
        let d = site(var);
        write(&d, "phv.toml", &format!("[admin]\nlisten = \"127.0.0.1:{}\"\ntoken_env = \"{var}\"\n", free_port()));
        write(&d, "public/index.phv", HOME);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let err = serve_on(listener, options(&d)).unwrap_err();
        assert!(err.contains(needle), "{err}");
        assert!(!err.contains("short\""), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/health` answers 503 as soon as the server stops, while running requests finish.
#[test]
fn health_turns_503_when_the_server_stops() {
    let dir = site("stop");
    let port = free_port();
    write(&dir, "phv.toml", &format!("[admin]\nlisten = \"127.0.0.1:{port}\"\n"));
    write(&dir, "public/slow.phv", "<?phv\n$t = microtime();\nwhile (microtime() - $t < 1.5) {\n}\necho \"done\";\n");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let opts = options(&dir);
    let server = std::thread::spawn(move || {
        serve_until(listener, opts, async {
            let _ = rx.await;
        })
    });
    let admin = admin_at(port);
    let slow = std::thread::spawn(move || get(addr, "/slow", "").body);
    std::thread::sleep(Duration::from_millis(300));
    let _ = tx.send(());
    std::thread::sleep(Duration::from_millis(200));
    let h = get(admin, "/health", "");
    assert_eq!(h.status, 503, "{}", h.body);
    assert_eq!(get(admin, "/ready", "").status, 503);
    assert!(get(admin, "/metrics", "").body.contains("phv_up 0\n"));
    assert_eq!(slow.join().unwrap(), "done");
    server.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn requests_get_ids() {
    let page = "<?phv\nimport std.http.request_id;\necho request_id() ?? \"none\";\nlog_info(\"seen\");\n";
    let dir = site("ids");
    write(
        &dir,
        "phv.toml",
        "[server]\nrequest_ids = true\naccess_log = \"logs/access.log\"\naccess_log_format = \"json\"\n",
    );
    write(&dir, "public/index.phv", page);
    let addr = start(&dir);
    let r = get(addr, "/", "X-Request-Id: from-the-client\r\n");
    let id = r.header("x-request-id").unwrap().to_string();
    assert_eq!(r.body, id);
    assert_eq!(id.len(), 32, "{id}");
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(get(addr, "/", "").body, id, "each request has its own id");
    // The id is in the JSON access log.
    std::thread::sleep(Duration::from_millis(300));
    let log = std::fs::read_to_string(dir.join("logs/access.log")).unwrap();
    assert!(log.contains(&format!("\"id\":\"{id}\"")), "{log}");

    // Behind a proxy that sends them, its id and trace are taken, when well formed.
    let trusted = site("trusted");
    write(&trusted, "phv.toml", "[server]\nrequest_ids = true\ntrust_request_id = true\n");
    write(&trusted, "public/index.phv", page);
    let addr = start(&trusted);
    assert_eq!(get(addr, "/", "X-Request-Id: abc-123.x_y\r\n").body, "abc-123.x_y");
    let tp = "traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01\r\n";
    assert_eq!(get(addr, "/", tp).body, "4bf92f3577b34da6a3ce929d0e0e4736");
    for bad in [
        "X-Request-Id: has space\r\n".to_string(),
        "X-Request-Id: <script>\r\n".to_string(),
        format!("X-Request-Id: {}\r\n", "a".repeat(129)),
        "traceparent: 00-00000000000000000000000000000000-00f067aa0ba902b7-01\r\n".to_string(),
        "traceparent: 01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01\r\n".to_string(),
        "traceparent: 00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01\r\n".to_string(),
    ] {
        let body = get(addr, "/", &bad).body;
        assert_eq!(body.len(), 32, "{bad}: {body}");
        assert!(!bad.contains(&body), "{bad}");
    }

    // Without `request_ids`, nothing changes.
    let off = site("off");
    write(&off, "phv.toml", "");
    write(&off, "public/index.phv", page);
    let addr = start(&off);
    let r = get(addr, "/", "X-Request-Id: x\r\n");
    assert_eq!(r.body, "none");
    assert!(r.header("x-request-id").is_none());
    for d in [dir, trusted, off] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// A page's calls to other services carry its trace (`traceparent`), with a
/// span of their own.
#[test]
fn calls_to_other_services_carry_the_trace() {
    let dir = site("trace");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    write(
        &dir,
        "phv.toml",
        &format!("[server]\nrequest_ids = true\ntrust_request_id = true\n[http.self]\nurl = \"http://{addr}\"\n"),
    );
    write(
        &dir,
        "public/call.phv",
        "<?phv\nimport std.net.HttpClient;\necho HttpClient::connect(\"self\")->get(\"/echo\")->body;\n",
    );
    write(
        &dir,
        "public/echo.phv",
        "<?phv\nimport std.http.request;\necho request()->header(\"traceparent\") ?? \"none\";\n",
    );
    let opts = options(&dir);
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    let tp = "traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01\r\n";
    let sent = get(addr, "/call", tp).body;
    let parts: Vec<&str> = sent.split('-').collect();
    assert_eq!(parts.len(), 4, "{sent}");
    assert_eq!((parts[0], parts[1], parts[3]), ("00", "4bf92f3577b34da6a3ce929d0e0e4736", "01"));
    assert_ne!(parts[2], "00f067aa0ba902b7", "a span of its own");
    assert_eq!(parts[2].len(), 16);
    let _ = std::fs::remove_dir_all(&dir);
}
