//! Starts the real server on a random port and talks HTTP to it.

use phenix_server::{serve_on, ServeOptions};
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
    let dir = std::env::temp_dir().join(format!("phenix-http-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phenix.toml", "[permissions]\nwrite = [\"data/\"]\n");
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
