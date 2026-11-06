//! Streaming responses: `response()->flush()` sends the head and the output so
//! far while the page goes on running.

use phv_server::{serve_on, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: phv_vm::alloc::CountingAlloc = phv_vm::alloc::CountingAlloc;

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// Waits (busily) until the test creates `data/<name>`.
const WAIT: &str = "function wait_for(string $name): void {\n    while (!file_exists(\"data/\" . $name)) {}\n}\n";

fn site(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-stream-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[permissions]\nread = [\"data/\"]\nwrite = [\"data/\"]\n");
    std::fs::create_dir_all(dir.join("data")).unwrap();
    write(
        &dir,
        "public/progress.phv",
        &format!(
            "<?phv\nimport std.http.{{response, session}};\n{WAIT}\
             session()->set(\"seen\", \"yes\");\n\
             response()->type(\"text/plain; charset=utf-8\")->header(\"X-Step\", \"1\");\n\
             echo \"first\\n\";\n\
             response()->flush();\n\
             wait_for(\"go\");\n\
             try {{\n    response()->header(\"X-Late\", \"1\");\n}} catch (ValueError $e) {{\n    echo \"late header: \", $e->getMessage(), \"\\n\";\n}}\n\
             try {{\n    session()->set(\"late\", \"1\");\n}} catch (ValueError $e) {{\n    echo \"late session: refused\\n\";\n}}\n\
             echo \"session still reads: \", session()->get(\"seen\") ?? \"-\", \"\\n\";\n\
             echo \"second\\n\";\n\
             response()->flush();\n\
             echo \"last\\n\";\n"
        ),
    );
    write(
        &dir,
        "public/fails.phv",
        &format!(
            "<?phv\nimport std.http.response;\n{WAIT}\
             echo \"partial\";\nresponse()->flush();\nwait_for(\"fail\");\nthrow new RuntimeException(\"boom\");\n"
        ),
    );
    write(
        &dir,
        "public/gone.phv",
        &format!(
            "<?phv\nimport std.http.response;\n{WAIT}\
             echo \"hello\";\nresponse()->flush();\nwait_for(\"closed\");\n\
             $open = true;\n$n = 0;\n\
             while ($open && $n < 200) {{\n    echo str_repeat(\"x\", 65536);\n    $open = response()->flush();\n    $n++;\n}}\n\
             file_put_contents(\"data/result\", $open ? \"still open\" : \"gone after \" . $n);\n"
        ),
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
        time_limit: Duration::from_secs(20),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    addr
}

struct Reader {
    s: TcpStream,
    seen: String,
}

impl Reader {
    fn get(addr: SocketAddr, path: &str) -> Reader {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").unwrap();
        Reader { s, seen: String::new() }
    }

    fn wait_for(&mut self, text: &str) -> &str {
        let until = Instant::now() + Duration::from_secs(15);
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

    /// Reads until the connection closes.
    fn rest(&mut self) -> &str {
        let until = Instant::now() + Duration::from_secs(15);
        let mut buf = [0u8; 4096];
        loop {
            assert!(Instant::now() < until, "not closed; got {:?}", self.seen);
            match self.s.read(&mut buf) {
                Ok(0) => return &self.seen,
                Ok(n) => self.seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return &self.seen,
                Err(_) => {}
            }
        }
    }
}

#[test]
fn flush_sends_the_response_while_the_page_runs() {
    let dir = site("progress");
    let addr = start(&dir);
    let mut r = Reader::get(addr, "/progress");
    // The head and the first part arrive while the page waits.
    let head = r.wait_for("first\n").to_string();
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("transfer-encoding: chunked"), "{head}");
    assert!(lower.contains("content-type: text/plain; charset=utf-8"), "{head}");
    assert!(lower.contains("x-step: 1"), "{head}");
    assert!(lower.contains("set-cookie: phvsessid="), "the session cookie goes with the head: {head}");
    assert!(lower.contains("cache-control: private, no-store"), "{head}");
    assert!(!head.contains("second"));
    write(&dir, "data/go", "");
    let all = r.rest().to_string();
    assert!(all.contains("late header: headers cannot change after response()->flush() sent the response"), "{all}");
    assert!(all.contains("late session: refused"), "{all}");
    assert!(all.contains("session still reads: yes"), "{all}");
    assert!(all.contains("second\n") && all.contains("last\n"), "{all}");
    assert!(all.ends_with("0\r\n\r\n"), "a complete chunked body: {all:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_page_that_fails_after_flush_is_cut_off() {
    let dir = site("fails");
    let addr = start(&dir);
    let mut r = Reader::get(addr, "/fails");
    r.wait_for("partial");
    write(&dir, "data/fail", "");
    let all = r.rest().to_string();
    assert!(!all.ends_with("0\r\n\r\n"), "the body must not look complete: {all:?}");
    assert!(!all.contains("Something went wrong"), "{all}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn flush_returns_false_once_the_client_is_gone() {
    let dir = site("gone");
    let addr = start(&dir);
    let mut r = Reader::get(addr, "/gone");
    r.wait_for("hello");
    drop(r);
    write(&dir, "data/closed", "");
    let until = Instant::now() + Duration::from_secs(15);
    let result = loop {
        if let Ok(t) = std::fs::read_to_string(dir.join("data/result")) {
            break t;
        }
        assert!(Instant::now() < until, "the page did not notice");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(result.starts_with("gone after "), "{result}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A site of its own: `phv.toml` and pages; it reads and writes `data/`.
fn site_with(name: &str, toml: &str, pages: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-stream-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", &format!("[permissions]\nread = [\"data/\"]\nwrite = [\"data/\"]\n{toml}"));
    std::fs::create_dir_all(dir.join("data")).unwrap();
    for (path, src) in pages {
        write(&dir, &format!("public/{path}"), src);
    }
    dir
}

/// Sends a request and reads the whole answer.
fn answer(addr: SocketAddr, request: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// Output flush() could not send (the request is HEAD, or the client is
/// gone) stays counted in the page's memory until it is freed (found by the
/// 1.2 audit: each flush let it go, so a 32 MB page then held 400–794 MB).
#[test]
fn a_page_that_flushes_to_no_one_keeps_its_memory_limit() {
    let export = "<?phv\nimport std.http.response;\n$chunk = str_repeat(\"x\", 1000000);\n\
                  for ($i = 0; $i < 200; $i++) {\n    echo $chunk;\n    response()->flush();\n}\n\
                  file_put_contents(\"data/flushed\", \"yes\");\nlist<string> $kept = [];\n\
                  for ($j = 0; $j < 16; $j++) {\n    $kept[] = str_repeat(\"y\", 4000000);\n}\n\
                  file_put_contents(\"data/held\", \"held \" . count($kept) * 4 . \" MB\");\n";
    let dir = site_with(
        "noone",
        "[security]\nmemory_limit_mb = 32\n[server]\nworkers = 1\n",
        &[("export.phv", export), ("ping.phv", "<?phv echo \"pong\";\n")],
    );
    let addr = start(&dir);
    for method in ["HEAD", "GET"] {
        let _ = std::fs::remove_file(dir.join("data/flushed"));
        if method == "HEAD" {
            let head = answer(addr, "HEAD /export HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        } else {
            // A client that leaves after the head.
            let mut r = Reader::get(addr, "/export");
            r.wait_for("\r\n\r\n");
        }
        let until = Instant::now() + Duration::from_secs(15);
        while !dir.join("data/flushed").exists() {
            assert!(Instant::now() < until, "{method}: the page did not get through its flushes");
            std::thread::sleep(Duration::from_millis(50));
        }
        // The only worker is free once the page has ended.
        let pong = answer(addr, "GET /ping HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
        assert!(pong.ends_with("pong"), "{pong}");
        let held = std::fs::read_to_string(dir.join("data/held")).unwrap_or_default();
        assert!(held.is_empty(), "{method}: the page went past its memory limit: {held}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A streamed chunk is handed to the connection in counted slices, as a
/// page's body is: clients that stop reading make new requests get 503 once
/// the output cap is reached (found by the 1.2 audit: each chunk went to the
/// system's buffers whole, uncounted; 60 such clients held 470 MB there).
#[test]
fn streamed_output_counts_in_the_output_cap() {
    let big = "<?phv\nimport std.http.response;\n$c = str_repeat(\"a\", 4000000);\necho $c;\nresponse()->flush();\necho $c;\n";
    // The cap is workers × memory_limit_mb: 32 MB.
    let dir = site_with("cap", "[security]\nmemory_limit_mb = 32\n[server]\nworkers = 1\n", &[("big.phv", big)]);
    let addr = start(&dir);
    let mut held = Vec::new();
    let mut admitted = 0;
    let mut refused = None;
    for _ in 0..24 {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let sent = Instant::now();
        s.write_all(b"GET /big HTTP/1.1\r\nHost: test\r\n\r\n").unwrap();
        // The status line, then nothing more is read.
        let mut line = [0u8; 12];
        s.read_exact(&mut line).unwrap();
        let status = String::from_utf8_lossy(&line).into_owned();
        if status.ends_with("503") {
            refused = Some(sent.elapsed());
            break;
        }
        assert!(status.ends_with("200"), "{status}");
        admitted += 1;
        held.push(s);
        // The page sends its second chunk and ends.
        std::thread::sleep(Duration::from_millis(300));
    }
    let refused = refused.unwrap_or_else(|| panic!("{admitted} clients that do not read were all admitted"));
    assert!(admitted >= 1);
    // Turned away at once by the output cap, not after waiting for a worker.
    assert!(refused < Duration::from_secs(5), "{refused:?}");
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}

/// flush() gives up on a client that has stopped reading well before the
/// page's time limit (found by the 1.2 audit: it waited until the limit,
/// so a few such clients held all the workers).
#[test]
fn flush_gives_up_on_a_client_that_stopped_reading() {
    let stall = "<?phv\nimport std.http.response;\n$c = str_repeat(\"z\", 1000000);\n$n = 0;\n\
                 while ($n < 2000) {\n    echo $c;\n    if (!response()->flush()) {\n        break;\n    }\n    $n++;\n}\n\
                 file_put_contents(\"data/stall\", \"gave up after \" . $n);\n";
    let dir = site_with("stall", "", &[("stall.phv", stall)]);
    // The page's time limit is 20 s.
    let addr = start(&dir);
    let mut s = TcpStream::connect(addr).unwrap();
    let sent = Instant::now();
    s.write_all(b"GET /stall HTTP/1.1\r\nHost: test\r\n\r\n").unwrap();
    let mut line = [0u8; 12];
    s.read_exact(&mut line).unwrap();
    let result = loop {
        if let Ok(t) = std::fs::read_to_string(dir.join("data/stall")) {
            break t;
        }
        assert!(sent.elapsed() < Duration::from_secs(25), "flush() did not give up");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(result.starts_with("gave up after "), "{result}");
    assert!(sent.elapsed() < Duration::from_secs(18), "flush() gave up after {:?}", sent.elapsed());
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A streamed head is personal when the request carries a cookie: a CSRF
/// token written after flush() is never in a response shared caches may keep
/// (found by the 1.2 audit).
#[test]
fn a_token_after_flush_is_in_a_private_response() {
    let form = "<?phv\nimport std.http.response;\n?>\n<html><body>\n<?phv response()->flush(); ?>\n\
                <form method=\"post\"><?= csrf_field() ?><button>go</button></form>\n";
    let dir = site_with("token", "", &[("form.phv", form)]);
    let addr = start(&dir);
    let cookie = format!("PHVCSRF={}", "a".repeat(64));
    let r = answer(addr, &format!("GET /form HTTP/1.1\r\nHost: test\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"));
    assert!(r.to_ascii_lowercase().contains("cache-control: private, no-store"), "{r}");
    assert!(r.contains("name=\"_csrf\""), "{r}");
    // Without the cookie the token cannot be made after flush().
    let r = answer(addr, "GET /form HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    assert!(!r.contains("_csrf"), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `catch` after flush() writes where the output was cut, not inside a tag
/// that was sent (found by the 1.2 audit).
#[test]
fn a_catch_after_flush_does_not_write_into_a_tag() {
    let cut = "<?phv\nimport std.http.{request, response};\nfunction title(): string {\n    throw new RuntimeException(\"no title\");\n}\n\
               $m = request()->query(\"m\") ?? \"\";\n?>\n<p>Hi</p>\n<?phv try { ?>\n<?phv response()->flush(); ?>\n\
               <div class=\"box\"><a title=\"<?= title() ?>\">link</a></div>\n<?phv } catch (RuntimeException) { ?>\n\
               <b><?= $m ?></b>\n<?phv } ?>\n<p>end</p>\n";
    let dir = site_with("catch", "", &[("cut.phv", cut)]);
    let addr = start(&dir);
    let r = answer(addr, "GET /cut?m=x%20onfocus%3Dalert(1)%20mk1 HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    assert!(r.contains("<b>x onfocus=alert(1) mk1</b>"), "{r}");
    assert!(!r.contains("<div"), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}
