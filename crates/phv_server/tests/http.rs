//! Starts the real server on a random port and talks HTTP to it.

use phv_server::{serve_on, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
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

fn get(addr: SocketAddr, path: &str, cookie: &str) -> Reply {
    request(addr, &format!("GET {path} HTTP/1.1\r\nHost: test\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"))
}

fn post(addr: SocketAddr, path: &str, cookie: &str, body: &str) -> Reply {
    request(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: test\r\nCookie: {cookie}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

#[test]
fn serves_pages_safely() {
    let dir = std::env::temp_dir().join(format!("phv-http-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[permissions]\nwrite = [\"data/\"]\n");
    write(&dir, "public/index.phv", "<?phv\nimport std.http.request;\n$name = request()->query(\"name\") ?? \"world\";\n?>\n<h1>Hello, <?= $name ?></h1>\n");
    write(
        &dir,
        "public/form.phv",
        "<?phv\nimport std.http.{request, session};\nif (request()->isPost()) {\n    session()->set(\"saved\", request()->form(\"v\") ?? \"\");\n    echo \"saved \", session()->get(\"saved\") ?? \"\";\n    return;\n}\necho csrf_field();\n",
    );
    write(&dir, "public/loop.phv", "<?phv\nwhile (true) {\n}\n");
    write(&dir, "public/style.css", "body { color: red; }");
    write(&dir, "public/.secret", "nope");
    write(&dir, "secret.phv", "<?phv echo \"outside\";");
    write(&dir, "data/.keep", "");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.clone(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: true,
        time_limit: Duration::from_secs(1),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());

    let r = get(addr, "/?name=%3Cscript%3Ealert(1)%3C/script%3E", "");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("Hello, &lt;script&gt;alert(1)&lt;/script&gt;"), "{}", r.body);
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("x-frame-options"), Some("SAMEORIGIN"));
    assert!(r.header("server").is_none());

    assert_eq!(get(addr, "/style.css", "").header("content-type"), Some("text/css; charset=utf-8"));
    for path in ["/.secret", "/../secret", "/%2e%2e/secret", "/..%2fsecret.phv", "/missing"] {
        assert_eq!(get(addr, path, "").status, 404, "{path}");
    }

    // CSRF: rejected without a token, accepted with the session's token.
    assert_eq!(post(addr, "/form", "", "v=1").status, 403);
    let page = get(addr, "/form", "");
    let cookie = page.header("set-cookie").expect("session cookie").split(';').next().unwrap().to_string();
    assert!(page.header("set-cookie").unwrap().contains("HttpOnly"));
    let token = page.body.split("value=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    assert_eq!(post(addr, "/form", &cookie, "v=1&_csrf=wrong").status, 403);
    let ok = post(addr, "/form", &cookie, &format!("v=hello&_csrf={token}"));
    assert_eq!(ok.status, 200);
    assert!(ok.body.contains("saved hello"), "{}", ok.body);
    // Masked on each page (BREACH): another load of the form shows other text, which works too.
    assert_eq!(token.len(), 128);
    let again = get(addr, "/form", &cookie);
    let token2 = again.body.split("value=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    assert_ne!(token, token2);
    assert_eq!(post(addr, "/form", &cookie, &format!("v=again&_csrf={token2}")).status, 200);
    // A token from another session does not work.
    let other = get(addr, "/form", "");
    let other_cookie = other.header("set-cookie").unwrap().split(';').next().unwrap().to_string();
    assert_eq!(post(addr, "/form", &other_cookie, &format!("v=x&_csrf={token}")).status, 403);

    // Time limit: an endless loop is stopped and reported.
    let r = get(addr, "/loop", "");
    assert_eq!(r.status, 500);
    assert!(r.body.contains("maximum execution time"), "{}", r.body);

    let _ = std::fs::remove_dir_all(&dir);
}

fn start(dir: &Path) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.to_path_buf(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: true,
        time_limit: Duration::from_secs(5),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    addr
}

#[test]
fn csp_nonces_and_rate_limits() {
    let dir = std::env::temp_dir().join(format!("phv-http-csp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[security]\nrequests_per_minute = 6\n");
    write(
        &dir,
        "public/index.phv",
        "<?phv\n?>\n<script src=\"/app.js\"></script>\n<script>var ok = 1;</script>\n<?= raw(\"<script>injected()</script>\") ?>\n",
    );
    write(&dir, "public/login.phv", "<?phv\necho rate_limit(\"login\", 2, 60) ? \"ok\" : \"limited\";\n");
    write(&dir, "public/app.js", "console.log(1);");
    let addr = start(&dir);

    let r = get(addr, "/", "");
    let csp = r.header("content-security-policy").expect("CSP is on by default").to_string();
    let nonce = csp.split("'nonce-").nth(1).unwrap_or_else(|| panic!("{csp}\n{}", r.body));
    let nonce = nonce.split('\'').next().unwrap().to_string();
    assert!(csp.contains("default-src 'self'") && csp.contains("object-src 'none'"), "{csp}");
    assert_eq!(r.body.matches(&format!("<script nonce=\"{nonce}\"")).count(), 2, "{}", r.body);
    assert!(r.body.contains("<script>injected()</script>"), "markup from raw() gets no nonce: {}", r.body);
    let again = get(addr, "/", "");
    assert!(!again.header("content-security-policy").unwrap().contains(&nonce), "a new nonce per response");
    let js = get(addr, "/app.js", "");
    assert!(!js.header("content-security-policy").unwrap().contains("nonce-"));

    assert_eq!(get(addr, "/login", "").body, "ok");
    assert_eq!(get(addr, "/login", "").body, "ok");
    assert_eq!(get(addr, "/login", "").body, "limited");
    // Six requests so far: the next one from this address is refused.
    let r = get(addr, "/", "");
    assert_eq!(r.status, 429);
    assert!(r.header("retry-after").is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_sessions_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("phv-http-sess-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[security]\ncsrf = false\nsession_store = \"sessions/\"\n");
    write(
        &dir,
        "public/index.phv",
        "<?phv\nimport std.http.{request, session};\n$v = request()->query(\"set\");\nif ($v != null) {\n    session()->set(\"v\", $v);\n}\necho session()->get(\"v\") ?? \"none\";\n",
    );
    let first = start(&dir);
    let r = get(first, "/?set=kept", "");
    let cookie = r.header("set-cookie").unwrap().split(';').next().unwrap().to_string();
    assert_eq!(r.body, "kept");
    // Another server process on the same directory sees the session.
    let second = start(&dir);
    assert_eq!(get(second, "/", &cookie).body, "kept");
    assert_eq!(
        get(second, "/", "PHVSESSID=0000000000000000000000000000000000000000000000000000000000000000").body,
        "none"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scripts_cannot_write_source_files() {
    let dir = std::env::temp_dir().join(format!("phv-http-src-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[security]\ncsrf = false\n[permissions]\nwrite = [\"views/\"]\n");
    write(&dir, "views/page.phv.html", "original");
    write(
        &dir,
        "public/index.phv",
        "<?phv\nimport std.http.request;\n$p = request()->query(\"p\") ?? \"\";\ntry {\n    file_put_contents($p, \"<?= getenv(\\\"PATH\\\") ?>\");\n    echo \"wrote\";\n} catch (PermissionError $e) {\n    echo \"refused\";\n}\n",
    );
    let addr = start(&dir);
    for p in [
        "views/page.phv.html",
        "views/page.phv.html/",
        "views/page.phv.html/.",
        "views/x.phv/",
        "views/X.PHV",
        "views/a.phv::$DATA",
        "views/b.phv.",
    ] {
        let url = format!("/?p={}", p.replace('/', "%2F").replace(':', "%3A").replace('$', "%24"));
        assert_eq!(get(addr, &url, "").body, "refused", "{p}");
    }
    assert_eq!(std::fs::read_to_string(dir.join("views/page.phv.html")).unwrap(), "original");
    assert_eq!(get(addr, "/?p=views%2Fnotes.txt", "").body, "wrote");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The raw response: status line and headers as text, the body as bytes.
fn get_bytes(addr: SocketAddr, path: &str) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (String::from_utf8_lossy(&buf[..split]).into_owned(), buf[split + 4..].to_vec())
}

#[test]
fn scripts_send_files() {
    let dir = std::env::temp_dir().join(format!("phv-http-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[permissions]\nwrite = [\"data/\"]\n");
    // Not valid UTF-8, so it could not go through echo.
    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00, 0xfe];
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::write(dir.join("data/a.png"), &png).unwrap();
    write(&dir, "data/page.svg", "<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>");
    write(&dir, "secret/key.txt", "secret");
    write(
        &dir,
        "public/file.phv",
        "<?phv\nimport std.http.{request, response};\necho \"<p>ignored</p>\";\n$p = request()->query(\"p\") ?? \"\";\n$t = request()->query(\"t\") ?? \"\";\ntry {\n    response()->file($p, $t);\n} catch (PermissionError $e) {\n    response()->status(403);\n    echo \"refused\";\n}\n",
    );
    let addr = start(&dir);

    let (head, body) = get_bytes(addr, "/file?p=data%2Fa.png");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.to_ascii_lowercase().contains("content-type: image/png"), "{head}");
    assert_eq!(body, png, "the bytes are sent as they are, without the page output");

    // Types a browser would render as a page are only sent as downloads by default.
    let (head, _) = get_bytes(addr, "/file?p=data%2Fpage.svg");
    assert!(head.to_ascii_lowercase().contains("content-type: application/octet-stream"), "{head}");
    let (head, _) = get_bytes(addr, "/file?p=data%2Fa.png&t=image%2Fwebp");
    assert!(head.to_ascii_lowercase().contains("content-type: image/webp"), "{head}");

    for p in ["secret%2Fkey.txt", "data%2F..%2Fsecret%2Fkey.txt", "phv.toml", "public%2Ffile.phv"] {
        let r = get(addr, &format!("/file?p={p}"), "");
        assert!(r.status == 403 && r.body.ends_with("refused"), "{p}: {} {}", r.status, r.body);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repeated_fields_are_all_read() {
    let dir = std::env::temp_dir().join(format!("phv-http-multi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[security]\ncsrf = false\n");
    write(
        &dir,
        "public/index.phv",
        "<?phv\nimport std.http.request;\n$r = request();\necho implode(\",\", $r->queryAll(\"tag\")), \"|\", implode(\",\", $r->formAll(\"ids\")), \"|\", $r->form(\"ids\") ?? \"-\", \"|\", count($r->formAll(\"none\"));\n",
    );
    let addr = start(&dir);
    let r = post(addr, "/?tag=a&tag=b%20c&other=x", "", "ids=3&name=x&ids=1&ids=2");
    assert_eq!(r.body, "a,b c|3,1,2|2|0");

    let boundary = "XBOUNDARY";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"ids\"\r\n\r\n7\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"ids\"\r\n\r\n8\r\n--{boundary}--\r\n"
    );
    let r = request(
        addr,
        &format!(
            "POST / HTTP/1.1\r\nHost: test\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(r.body.starts_with("|7,8|"), "{}", r.body);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn busy_servers_answer_503() {
    let dir = std::env::temp_dir().join(format!("phv-http-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[server]\nworkers = 1\n");
    write(&dir, "public/loop.phv", "<?phv\nwhile (true) {\n}\n");
    write(&dir, "public/index.phv", "<?phv echo \"ok\";");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.clone(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: true,
        time_limit: Duration::from_secs(14),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    assert_eq!(get(addr, "/", "").status, 200);

    // The only worker is busy for longer than a request waits for one.
    let slow = std::thread::spawn(move || get(addr, "/loop", "").status);
    std::thread::sleep(Duration::from_millis(500));
    let start = std::time::Instant::now();
    let r = get(addr, "/", "");
    assert_eq!(r.status, 503, "{}", r.body);
    assert_eq!(r.header("retry-after"), Some("5"));
    assert!(start.elapsed() >= Duration::from_secs(9), "{:?}", start.elapsed());
    // Static files do not need a worker.
    write(&dir, "public/a.txt", "a");
    assert_eq!(get(addr, "/a.txt", "").status, 200);
    assert_eq!(slow.join().unwrap(), 500);
    assert_eq!(get(addr, "/", "").status, 200);
    let _ = std::fs::remove_dir_all(&dir);
}

fn start_with(dir: &Path, dev: bool) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.to_path_buf(),
        host: "127.0.0.1".into(),
        port: 0,
        dev,
        time_limit: Duration::from_secs(5),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    addr
}

/// A production server remembers compiled pages, what paths lead to and small
/// static files, and looks at the files again at most once a second: changes
/// show within a second, and a static file that became a hard link to a file
/// outside `public/` is no longer served.
#[test]
fn production_servers_see_changes_within_a_second() {
    let dir = std::env::temp_dir().join(format!("phv-prod-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "");
    write(&dir, "public/index.phv", "<?phv\necho \"one\";\n");
    write(&dir, "public/site.css", "a{}");
    write(&dir, "key.pem", "SECRET");
    let addr = start_with(&dir, false);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(get(addr, "/", "").body, "one");
    assert_eq!(get(addr, "/site.css", "").body, "a{}");
    assert_eq!(get(addr, "/new", "").status, 404);
    // Asked again at once: answered from memory.
    assert_eq!(get(addr, "/site.css", "").body, "a{}");

    write(&dir, "public/index.phv", "<?phv\necho \"second\";\n");
    write(&dir, "public/site.css", "b{color:red}");
    write(&dir, "public/new.phv", "<?phv\necho \"new\";\n");
    std::thread::sleep(Duration::from_millis(1300));
    // The first request after a second looks at the files; the next ones see the change.
    for _ in 0..2 {
        let _ = (get(addr, "/", ""), get(addr, "/site.css", ""), get(addr, "/new", ""));
    }
    assert_eq!(get(addr, "/", "").body, "second");
    assert_eq!(get(addr, "/site.css", "").body, "b{color:red}");
    assert_eq!(get(addr, "/new", "").body, "new");

    std::fs::remove_file(dir.join("public/site.css")).unwrap();
    if std::fs::hard_link(dir.join("key.pem"), dir.join("public/site.css")).is_ok() {
        std::thread::sleep(Duration::from_millis(1300));
        for _ in 0..2 {
            let _ = get(addr, "/site.css", "");
        }
        let r = get(addr, "/site.css", "");
        assert_eq!(r.status, 404, "{}", r.body);
        assert!(!r.body.contains("SECRET"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Routes with parameters, middleware that answers or passes values on, and
/// the site's own error pages.
#[test]
fn routes_middleware_and_error_pages() {
    let dir = std::env::temp_dir().join(format!("phv-http-routes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let page = |body: &str| format!("<?phv\nimport std.http.{{request, response, locals}};\n{body}\n");
    write(&dir, "phv.toml", "");
    write(&dir, "public/index.phv", &page("echo \"home\";"));
    write(&dir, "public/posts/[id].phv", &page("echo \"post \", request()->param(\"id\") ?? \"?\";"));
    write(&dir, "public/posts/new.phv", &page("echo \"new post\";"));
    write(&dir, "public/posts/[id]/edit.phv", &page("echo \"edit \", request()->params[\"id\"];"));
    write(&dir, "public/docs/[...path].phv", &page("echo \"doc \", request()->param(\"path\") ?? \"\";"));
    write(&dir, "public/_404.phv", &page("echo \"custom 404 for \", request()->path;"));
    write(&dir, "public/_500.phv", &page("echo \"custom 500\";"));
    write(&dir, "public/boom.phv", &page("throw new RuntimeException(\"secret detail\");"));
    write(
        &dir,
        "public/admin/_middleware.phv",
        &page("if (request()->query(\"key\") != \"ok\") {\n    response()->status(403);\n    echo \"denied\";\n} else {\n    locals()->set(\"user\", \"ann\");\n    response()->header(\"X-Checked\", \"yes\");\n}"),
    );
    write(&dir, "public/admin/index.phv", &page("echo \"admin \", locals()->get(\"user\") ?? \"nobody\";"));
    let addr = start_with(&dir, false);
    std::thread::sleep(Duration::from_millis(300));

    let r = get(addr, "/posts/42", "");
    assert_eq!((r.status, r.body.as_str()), (200, "post 42"));
    assert_eq!(get(addr, "/posts/new", "").body, "new post", "a file named exactly wins");
    assert_eq!(get(addr, "/posts/7/edit", "").body, "edit 7");
    assert_eq!(get(addr, "/docs/a/b%20c", "").body, "doc a/b c");
    let r = get(addr, "/posts/7/nope", "");
    assert_eq!((r.status, r.body.as_str()), (404, "custom 404 for /posts/7/nope"));
    assert_eq!(get(addr, "/posts/_hidden", "").status, 404, "private names are never parameters");

    let r = get(addr, "/admin/", "");
    assert_eq!((r.status, r.body.as_str()), (403, "denied"));
    let r = get(addr, "/admin/?key=ok", "");
    assert_eq!((r.status, r.body.as_str()), (200, "admin ann"));
    assert_eq!(r.header("x-checked"), Some("yes"));
    assert_eq!(get(addr, "/admin/_middleware", "").status, 404);

    let r = get(addr, "/boom", "");
    assert_eq!((r.status, r.body.as_str()), (500, "custom 500"));
    assert!(!r.body.contains("secret detail"));
    let _ = std::fs::remove_dir_all(&dir);
}

fn raw_bytes(addr: SocketAddr, raw: &str) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(raw.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let at = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (String::from_utf8_lossy(&buf[..at]).into_owned(), buf[at + 4..].to_vec())
}

/// Found by the 1.0.0 audit: the host scripts see is the request target's
/// authority (an absolute target in HTTP/1.1), and one with user
/// information is refused; a long path is answered (and not remembered);
/// ranges of a file a client takes gzipped are read from the file.
#[test]
fn hosts_long_paths_and_ranges_in_production() {
    let dir = std::env::temp_dir().join(format!("phv-host-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "");
    write(&dir, "public/host.phv", "<?phv\nimport std.http.request;\necho request()->header(\"host\") ?? \"-\";\n");
    let text: String = (0..60_000).map(|i| format!("{i:09}\n")).collect();
    write(&dir, "public/big.txt", &text);
    let addr = start_with(&dir, false);
    std::thread::sleep(Duration::from_millis(300));

    let r = request(addr, "GET http://bank.example/host HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n");
    assert_eq!((r.status, r.body.as_str()), (200, "bank.example"));
    let r =
        request(addr, "GET http://user@bank.example/host HTTP/1.1\r\nHost: bank.example\r\nConnection: close\r\n\r\n");
    assert_eq!(r.status, 400);

    let long = format!("/{}", "x".repeat(20_000));
    for _ in 0..2 {
        assert_eq!(get(addr, &long, "").status, 404);
    }

    // A range of a 600 KB text file, with gzip accepted: the bytes asked for.
    let (head, body) = raw_bytes(
        addr,
        "GET /big.txt HTTP/1.1\r\nHost: t\r\nRange: bytes=100000-100019\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert_eq!(body, &text.as_bytes()[100_000..100_020]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Static files answer conditional and range requests; text is gzipped
/// unless the response is personal; CORS; the access log; stopping.
#[test]
fn caching_ranges_gzip_cors_and_shutdown() {
    let dir = std::env::temp_dir().join(format!("phv-http-static-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        "phv.toml",
        "[server]\nstatic_max_age = 60\naccess_log = \"logs/access.log\"\naccess_log_format = \"json\"\n\
         [cors]\norigins = [\"https://app.example\"]\nmethods = [\"GET\", \"PUT\"]\nheaders = [\"Content-Type\"]\n",
    );
    let css = "body { color: red; }\n".repeat(200);
    write(&dir, "public/site.css", &css);
    write(&dir, "public/index.phv", &format!("<?phv\necho \"{}\";\n", "hello ".repeat(400)));
    write(&dir, "public/form.phv", &format!("<?phv\necho csrf_field(), \"{}\";\n", "x".repeat(2000)));
    // Uses the session but sets its own Cache-Control: still personal.
    write(
        &dir,
        "public/personal.phv",
        &format!(
            "<?phv\nimport std.http.{{session, response}};\nsession()->set(\"k\", \"v\");\n\
             response()->header(\"Cache-Control\", \"no-cache\");\necho \"{}\";\n",
            "s".repeat(2000)
        ),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.clone(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: true,
        time_limit: Duration::from_secs(5),
        watch: false,
    };
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let stop = async move {
        // Polled until the test asks the server to stop.
        while rx.try_recv().is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let server = std::thread::spawn(move || phv_server::serve_until(listener, opts, stop).unwrap());
    std::thread::sleep(Duration::from_millis(300));

    let r = get(addr, "/site.css", "");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control"), Some("public, max-age=60"));
    let etag = r.header("etag").unwrap().to_string();
    assert!(r.header("last-modified").is_some());
    let r = request(
        addr,
        &format!("GET /site.css HTTP/1.1\r\nHost: t\r\nIf-None-Match: {etag}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!((r.status, r.body.as_str()), (304, ""));
    let r = request(addr, "GET /site.css HTTP/1.1\r\nHost: t\r\nRange: bytes=0-3\r\nConnection: close\r\n\r\n");
    assert_eq!((r.status, r.body.as_str()), (206, "body"));
    assert_eq!(r.header("content-range"), Some(&*format!("bytes 0-3/{}", css.len())));
    let r = request(addr, "GET /site.css HTTP/1.1\r\nHost: t\r\nRange: bytes=99999-\r\nConnection: close\r\n\r\n");
    assert_eq!(r.status, 416);

    let gunzip = |b: &[u8]| {
        let mut s = String::new();
        flate2::read::GzDecoder::new(b).read_to_string(&mut s).unwrap();
        s
    };
    for (path, want) in [("/site.css", css.clone()), ("/", "hello ".repeat(400))] {
        let (head, body) = raw_bytes(
            addr,
            &format!("GET {path} HTTP/1.1\r\nHost: t\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n"),
        );
        assert!(head.to_ascii_lowercase().contains("content-encoding: gzip"), "{path}: {head}");
        assert!(head.contains("Accept-Encoding"), "{path} must vary: {head}");
        assert_eq!(gunzip(&body), want);
    }
    // Personal pages, and pages answering a request with cookies, are never
    // compressed (BREACH); static files still are.
    for (path, cookie) in [("/personal", ""), ("/", "Cookie: a=b\r\n")] {
        let (head, _) = raw_bytes(
            addr,
            &format!("GET {path} HTTP/1.1\r\nHost: t\r\nAccept-Encoding: gzip\r\n{cookie}Connection: close\r\n\r\n"),
        );
        assert!(!head.to_ascii_lowercase().contains("content-encoding"), "{path}: {head}");
    }
    let (head, _) = raw_bytes(
        addr,
        "GET /site.css HTTP/1.1\r\nHost: t\r\nAccept-Encoding: gzip\r\nCookie: a=b\r\nConnection: close\r\n\r\n",
    );
    assert!(head.to_ascii_lowercase().contains("content-encoding: gzip"), "{head}");
    // A page with a CSRF token is personal: never compressed (BREACH).
    let (head, _) =
        raw_bytes(addr, "GET /form HTTP/1.1\r\nHost: t\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n");
    assert!(!head.to_ascii_lowercase().contains("content-encoding"), "{head}");

    let pre = "OPTIONS / HTTP/1.1\r\nHost: t\r\nAccess-Control-Request-Method: PUT\r\nConnection: close\r\n";
    let r = request(addr, &format!("{pre}Origin: https://app.example\r\n\r\n"));
    assert_eq!(r.status, 204);
    assert_eq!(r.header("access-control-allow-origin"), Some("https://app.example"));
    assert_eq!(r.header("access-control-allow-methods"), Some("GET, PUT"));
    let r = request(addr, &format!("{pre}Origin: https://evil.example\r\n\r\n"));
    assert_eq!(r.header("access-control-allow-origin"), None);
    let r = request(addr, "GET / HTTP/1.1\r\nHost: t\r\nOrigin: https://app.example\r\nConnection: close\r\n\r\n");
    assert_eq!(r.header("access-control-allow-origin"), Some("https://app.example"));
    assert!(r.header("vary").is_some_and(|v| v.contains("Origin")));

    tx.send(()).unwrap();
    server.join().unwrap();
    assert!(TcpStream::connect(addr).is_err(), "the server stopped listening");
    let log = std::fs::read_to_string(dir.join("logs/access.log")).unwrap();
    let first: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!(first["path"], "/site.css");
    assert_eq!(first["status"], 200);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `std.testing`: a test script sends requests to the site in-process and
/// keeps its cookies.
#[test]
fn test_client_runs_pages_in_process() {
    let dir = std::env::temp_dir().join(format!("phv-test-site-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "");
    write(
        &dir,
        "public/items/[id].phv",
        "<?phv\nimport std.http.request;\necho \"item \", request()->param(\"id\") ?? \"\";\n",
    );
    write(
        &dir,
        "public/login.phv",
        "<?phv\nimport std.http.{request, response, session};\nsession()->set(\"user\", request()->form(\"name\") ?? \"\");\nresponse()->redirect(\"/me\");\n",
    );
    write(
        &dir,
        "public/me.phv",
        "<?phv\nimport std.http.session;\necho \"hi \", session()->get(\"user\") ?? \"nobody\";\n",
    );
    write(
        &dir,
        "check.phv",
        "<?phv\nimport std.testing.TestClient;\n$c = new TestClient();\necho $c->get(\"/items/7\")->body, \"|\";\n\
         echo $c->get(\"/missing\")->status, \"|\";\n$r = $c->post(\"/login\", [\"name\" => \"ann\"]);\n\
         echo $r->status, \" \", $r->location() ?? \"\", \"|\", $c->get(\"/me\")->body;\n",
    );
    let site = phv_server::test_site(&dir).unwrap();
    let root = dir.canonicalize().unwrap();
    let c = phv_driver::compile(&root, &root.join("check.phv")).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let mut host = (*phv_driver::load_config(&root).unwrap().host).clone();
    host.site = Some(site);
    let opts = phv_driver::RunOptions {
        html: false,
        limits: Default::default(),
        http: None,
        host: Some(std::sync::Arc::new(host)),
    };
    let out = phv_driver::run(&c, opts);
    assert!(out.error.is_none(), "{}", out.error.map(|e| e.render()).unwrap_or_default());
    assert_eq!(out.output, "item 7|404|302 /me|hi ann");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn watch_mode_reloads_pages_on_changes() {
    let dir = std::env::temp_dir().join(format!("phv-http-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[permissions]\nwrite = [\"data/\"]\n");
    write(&dir, "public/index.phv", "<?phv echo raw(\"<html><body><p>hi</p></body></html>\");");
    write(&dir, "public/api.phv", "<?phv\nimport std.http.response;\nresponse()->json([\"a\" => 1]);\n");
    let start = |watch: bool| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let opts = ServeOptions {
            root: dir.clone(),
            host: "127.0.0.1".into(),
            port: 0,
            dev: true,
            time_limit: Duration::from_secs(5),
            watch,
        };
        std::thread::spawn(move || serve_on(listener, opts).unwrap());
        addr
    };
    let addr = start(true);
    let page = get(addr, "/", "");
    assert!(
        page.body.ends_with("<p>hi</p><script src=\"/__phv/live-reload.js\" defer></script></body></html>"),
        "{}",
        page.body
    );
    assert!(!get(addr, "/api", "").body.contains("live-reload"), "only HTML gets the script");
    let js = get(addr, "/__phv/live-reload.js", "");
    assert_eq!(js.status, 200);
    assert!(js.body.contains("location.reload()"));
    let before = get(addr, "/__phv/changes", "").body;
    // The stamp is kept for a second (shared by every tab that asks).
    write(&dir, "public/new.phv", "<?phv echo 1;");
    assert_eq!(get(addr, "/__phv/changes", "").body, before, "within a second, the same stamp");
    std::thread::sleep(Duration::from_millis(1100));
    let after = get(addr, "/__phv/changes", "").body;
    assert_ne!(after, before);
    write(&dir, "data/written-by-the-site.txt", "x");
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(get(addr, "/__phv/changes", "").body, after, "data the site writes is not a change");
    // Without --watch, none of this exists.
    let addr = start(false);
    assert!(!get(addr, "/", "").body.contains("live-reload"));
    assert_eq!(get(addr, "/__phv/changes", "").status, 404);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Request bodies are bounded as they are read (v0.9.1): a body of more than
/// 8 MB is refused at once, a client that has sent a large body keeps the next
/// one waiting for the budget (one worker: 8 MB), clients that only state a
/// large body or start a chunked one hold nothing, and a form of millions of
/// fields is refused while it is parsed.
#[test]
fn request_bodies_are_bounded_as_they_are_read() {
    let dir = std::env::temp_dir().join(format!("phv-http-bodies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[security]\ncsrf = false\n\n[server]\nworkers = 1\n");
    write(&dir, "public/index.phv", "<?phv\nimport std.http.request;\necho count(request()->formParams);\n");
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
    assert_eq!(post(addr, "/", "", "a=1&b=2").body, "2");

    let huge = format!("POST / HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", 9 << 20);
    assert_eq!(request(addr, &huge).status, 413);

    let fields = "a&".repeat(1001);
    assert_eq!(post(addr, "/", "", &fields).status, 400);
    let start = std::time::Instant::now();
    assert_eq!(post(addr, "/", "", &"a&".repeat(4 << 20)).status, 400);
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());

    // Clients that state 8 MB bodies or start chunked ones and stop hold nothing.
    let mut idle = Vec::new();
    for i in 0..8 {
        let mut c = TcpStream::connect(addr).unwrap();
        let head = if i % 2 == 0 {
            format!("POST / HTTP/1.1\r\nHost: t\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\nx", 8 << 20)
        } else {
            "POST / HTTP/1.1\r\nHost: t\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n"
                .into()
        };
        c.write_all(head.as_bytes()).unwrap();
        idle.push(c);
    }
    std::thread::sleep(Duration::from_millis(300));
    let start = std::time::Instant::now();
    assert_eq!(post(addr, "/", "", &format!("a={}", "x".repeat(200_000))).body, "1");
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());

    // A client that has really sent most of an 8 MB body holds most of the budget.
    let mut slow = TcpStream::connect(addr).unwrap();
    let head = format!("POST / HTTP/1.1\r\nHost: t\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n", 8 << 20);
    slow.write_all(head.as_bytes()).unwrap();
    slow.write_all(&vec![b'x'; (8 << 20) - 1]).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let start = std::time::Instant::now();
    let r = post(addr, "/", "", &format!("a={}", "x".repeat(200_000)));
    assert_eq!(r.status, 503, "{}", r.body);
    assert!(start.elapsed() >= Duration::from_secs(9), "{:?}", start.elapsed());
    // Small bodies do not need the budget.
    assert_eq!(post(addr, "/", "", "a=1").body, "1");
    drop(slow);
    drop(idle);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A JavaScript response takes only `json_html()` values and numbers: a string
/// or a template's output would be code (any JavaScript type, also when the
/// type is set after output). Pages run only scripts with their nonce.
#[test]
fn javascript_responses_take_only_inert_values() {
    let dir = std::env::temp_dir().join(format!("phv-http-js-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[project]\nname = \"js\"\n");
    let head = "<?phv\nimport std.http.{request, response};\n";
    write(&dir, "public/ok.phv", &format!("{head}response()->type(\"text/javascript\");\n$q = request()->query(\"q\") ?? \"\";\n?>\nvar c = <?= json_html([\"q\" => $q]) ?>; var n = <?= 4 ?>;\n"));
    write(
        &dir,
        "public/str.phv",
        &format!(
            "{head}response()->type(\"text/javascript\");\n$q = request()->query(\"q\") ?? \"\";\n?>\n<?= $q ?>;\n"
        ),
    );
    write(&dir, "public/xjs.phv", &format!("{head}response()->type(\"application/x-javascript\");\n$q = request()->query(\"q\") ?? \"\";\n?>\n<?= $q ?>;\n"));
    write(
        &dir,
        "public/late.phv",
        &format!("{head}$q = request()->query(\"q\") ?? \"\";\necho $q;\nresponse()->type(\"text/javascript\");\n"),
    );
    write(&dir, "public/page.phv", "<?phv\n?>\n<p>hi</p>\n");
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
    let q = "?q=alert(document.domain)//";
    let ok = get(addr, &format!("/ok{q}"), "");
    assert_eq!(ok.status, 200);
    assert!(
        ok.body.contains(r#"var c = {"q":"alert\u0028document\u002edomain\u0029\u002f\u002f"}; var n = 4;"#),
        "{}",
        ok.body
    );
    for page in ["/str", "/xjs", "/late"] {
        assert_eq!(get(addr, &format!("{page}{q}"), "").status, 500, "{page}");
    }
    let csp = get(addr, "/page", "");
    let policy = csp.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-security-policy")).unwrap();
    assert!(policy.1.contains("'strict-dynamic'") && !policy.1.contains("script-src 'self'"), "{}", policy.1);
    let _ = std::fs::remove_dir_all(&dir);
}
