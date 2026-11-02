//! Starts the real server on a random port and talks HTTP to it.

use phx_server::{serve_on, ServeOptions};
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
    let dir = std::env::temp_dir().join(format!("phx-http-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phx.toml", "[permissions]\nwrite = [\"data/\"]\n");
    write(&dir, "public/index.phx", "<?phx\nimport std.http.request;\n$name = request()->query(\"name\") ?? \"world\";\n?>\n<h1>Hello, <?= $name ?></h1>\n");
    write(
        &dir,
        "public/form.phx",
        "<?phx\nimport std.http.{request, session};\nif (request()->isPost()) {\n    session()->set(\"saved\", request()->form(\"v\") ?? \"\");\n    echo \"saved \", session()->get(\"saved\") ?? \"\";\n    return;\n}\necho csrf_field();\n",
    );
    write(&dir, "public/loop.phx", "<?phx\nwhile (true) {\n}\n");
    write(&dir, "public/style.css", "body { color: red; }");
    write(&dir, "public/.secret", "nope");
    write(&dir, "secret.phx", "<?phx echo \"outside\";");
    write(&dir, "data/.keep", "");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.clone(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: true,
        time_limit: Duration::from_secs(1),
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());

    let r = get(addr, "/?name=%3Cscript%3Ealert(1)%3C/script%3E", "");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("Hello, &lt;script&gt;alert(1)&lt;/script&gt;"), "{}", r.body);
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("x-frame-options"), Some("SAMEORIGIN"));
    assert!(r.header("server").is_none());

    assert_eq!(get(addr, "/style.css", "").header("content-type"), Some("text/css; charset=utf-8"));
    for path in ["/.secret", "/../secret", "/%2e%2e/secret", "/..%2fsecret.phx", "/missing"] {
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
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    addr
}

#[test]
fn csp_nonces_and_rate_limits() {
    let dir = std::env::temp_dir().join(format!("phx-http-csp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phx.toml", "[security]\nrequests_per_minute = 6\n");
    write(
        &dir,
        "public/index.phx",
        "<?phx\n?>\n<script src=\"/app.js\"></script>\n<script>var ok = 1;</script>\n<?= raw(\"<script>injected()</script>\") ?>\n",
    );
    write(&dir, "public/login.phx", "<?phx\necho rate_limit(\"login\", 2, 60) ? \"ok\" : \"limited\";\n");
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
    let dir = std::env::temp_dir().join(format!("phx-http-sess-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phx.toml", "[security]\ncsrf = false\nsession_store = \"sessions/\"\n");
    write(
        &dir,
        "public/index.phx",
        "<?phx\nimport std.http.{request, session};\n$v = request()->query(\"set\");\nif ($v != null) {\n    session()->set(\"v\", $v);\n}\necho session()->get(\"v\") ?? \"none\";\n",
    );
    let first = start(&dir);
    let r = get(first, "/?set=kept", "");
    let cookie = r.header("set-cookie").unwrap().split(';').next().unwrap().to_string();
    assert_eq!(r.body, "kept");
    // Another server process on the same directory sees the session.
    let second = start(&dir);
    assert_eq!(get(second, "/", &cookie).body, "kept");
    assert_eq!(
        get(second, "/", "PHXSESSID=0000000000000000000000000000000000000000000000000000000000000000").body,
        "none"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scripts_cannot_write_source_files() {
    let dir = std::env::temp_dir().join(format!("phx-http-src-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phx.toml", "[security]\ncsrf = false\n[permissions]\nwrite = [\"views/\"]\n");
    write(&dir, "views/page.phx.html", "original");
    write(
        &dir,
        "public/index.phx",
        "<?phx\nimport std.http.request;\n$p = request()->query(\"p\") ?? \"\";\ntry {\n    file_put_contents($p, \"<?= getenv(\\\"PATH\\\") ?>\");\n    echo \"wrote\";\n} catch (PermissionError $e) {\n    echo \"refused\";\n}\n",
    );
    let addr = start(&dir);
    for p in [
        "views/page.phx.html",
        "views/page.phx.html/",
        "views/page.phx.html/.",
        "views/x.phx/",
        "views/X.PHX",
        "views/a.phx::$DATA",
        "views/b.phx.",
    ] {
        let url = format!("/?p={}", p.replace('/', "%2F").replace(':', "%3A").replace('$', "%24"));
        assert_eq!(get(addr, &url, "").body, "refused", "{p}");
    }
    assert_eq!(std::fs::read_to_string(dir.join("views/page.phx.html")).unwrap(), "original");
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
    let dir = std::env::temp_dir().join(format!("phx-http-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phx.toml", "[permissions]\nwrite = [\"data/\"]\n");
    // Not valid UTF-8, so it could not go through echo.
    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00, 0xfe];
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::write(dir.join("data/a.png"), &png).unwrap();
    write(&dir, "data/page.svg", "<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>");
    write(&dir, "secret/key.txt", "secret");
    write(
        &dir,
        "public/file.phx",
        "<?phx\nimport std.http.{request, response};\necho \"<p>ignored</p>\";\n$p = request()->query(\"p\") ?? \"\";\n$t = request()->query(\"t\") ?? \"\";\ntry {\n    response()->file($p, $t);\n} catch (PermissionError $e) {\n    response()->status(403);\n    echo \"refused\";\n}\n",
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

    for p in ["secret%2Fkey.txt", "data%2F..%2Fsecret%2Fkey.txt", "phx.toml", "public%2Ffile.phx"] {
        let r = get(addr, &format!("/file?p={p}"), "");
        assert!(r.status == 403 && r.body.ends_with("refused"), "{p}: {} {}", r.status, r.body);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
