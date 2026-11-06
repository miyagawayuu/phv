//! Streaming responses: `response()->flush()` sends the head and the output so
//! far while the page goes on running.

use phv_server::{serve_on, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
