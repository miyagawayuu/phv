//! A service of `[http]` or `[websockets]`, or an `[oauth]` provider, must
//! not point at the server itself: a page calling its own server holds a
//! worker while it waits for one, and four such pages on four workers stalled
//! every request for 10 seconds (18th audit, H-3). The server refuses to start
//! instead, and refuses a page's connection to its own address when only the
//! page knows it (19th assessment, R22-19).

use phv_server::{serve_on, serve_until, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn site(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-selfcall-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "public/index.phv", "<?phv\necho \"home\";\n");
    dir
}

fn options(dir: &Path) -> ServeOptions {
    ServeOptions {
        root: dir.to_path_buf(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: true,
        time_limit: Duration::from_secs(5),
        watch: false,
    }
}

fn status_of(addr: SocketAddr, path: &str) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    text.lines().next().unwrap().split(' ').nth(1).unwrap().parse().unwrap()
}

/// Starting the site on `listen` with `service` in phv.toml: the error, if any.
fn start_error(listen: &str, service: &str) -> Result<SocketAddr, String> {
    let listener = std::net::TcpListener::bind(listen).unwrap();
    let addr = listener.local_addr().unwrap();
    // A site of its own: the tests run at once.
    let dir = site(&format!("refused-{}", addr.port()));
    write(&dir, "phv.toml", &service.replace("{port}", &addr.port().to_string()));
    let result = serve_on(listener, options(&dir));
    let _ = std::fs::remove_dir_all(&dir);
    result.map(|()| addr)
}

#[test]
fn a_service_pointing_at_this_server_is_refused() {
    let own = [
        (
            "[http.self]\nurl = \"http://127.0.0.1:{port}\"\n",
            "[http.self]: the service points at this server (127.0.0.1:",
        ),
        ("[http.api]\nurl = \"http://localhost:{port}/api/v1\"\n", "[http.api]: the service points at this server"),
        (
            "[websockets.chat]\nurl = \"ws://127.0.0.1:{port}/chat\"\n",
            "[websockets.chat]: the service points at this server",
        ),
    ];
    for (service, expected) in own {
        let e = start_error("127.0.0.1:0", service).err().unwrap_or_else(|| panic!("{service} started"));
        assert!(e.contains(expected), "{service}: {e}");
        assert!(e.contains("a page cannot call its own server"), "{e}");
    }
    // A server listening on every address is reached through the loopback too.
    let e = start_error("0.0.0.0:0", "[http.self]\nurl = \"http://127.0.0.1:{port}\"\n").err().unwrap();
    assert!(e.contains("points at this server"), "{e}");
    // The URL of a service from the environment is not shown.
    std::env::set_var("PHV_SELFCALL_TEST_URL", "http://127.0.0.1:{port}");
    let dir = site("env");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::env::set_var("PHV_SELFCALL_TEST_URL", format!("http://127.0.0.1:{port}"));
    write(&dir, "phv.toml", "[http.self]\nurl_env = \"PHV_SELFCALL_TEST_URL\"\n");
    let e = serve_on(listener, options(&dir)).err().unwrap();
    assert!(e.contains("the URL from $PHV_SELFCALL_TEST_URL") && !e.contains(&port.to_string()), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The unspecified address (`0.0.0.0`, `::`) on the server's port is this
/// server (a connection to it reaches the server through the loopback on
/// Linux), and so are the URLs of `[oauth]` providers: the issuer's discovery
/// and the token endpoint are fetched by the page. Found by the 19th
/// assessment (R22-19): both started, and the pages stalled the server 10 s.
#[test]
fn the_unspecified_address_and_oauth_urls_pointing_at_this_server_are_refused() {
    let provider = "client_id = \"site\"\nredirect_uri = \"http://127.0.0.1:{port}/cb\"\n";
    let own = [
        ("127.0.0.1:0", "[http.self]\nurl = \"http://0.0.0.0:{port}\"\ntls = false\n".to_string(), "[http.self]"),
        ("127.0.0.1:0", "[http.self]\nurl = \"http://[::]:{port}\"\ntls = false\n".to_string(), "[http.self]"),
        ("127.0.0.1:0", "[websockets.w]\nurl = \"ws://0.0.0.0:{port}/\"\ntls = false\n".to_string(), "[websockets.w]"),
        ("127.0.0.1:0", format!("[oauth.self]\nissuer = \"http://127.0.0.1:{{port}}\"\n{provider}"), "[oauth.self] issuer"),
        ("0.0.0.0:0", format!("[oauth.self]\nissuer = \"http://localhost:{{port}}\"\n{provider}"), "[oauth.self] issuer"),
        (
            "127.0.0.1:0",
            format!(
                "[oauth.plain]\nauthorize_url = \"http://127.0.0.1:{{port}}/auth\"\ntoken_url = \"https://idp.example/token\"\n{provider}"
            ),
            "[oauth.plain] authorize_url",
        ),
        (
            "127.0.0.1:0",
            format!(
                "[oauth.plain]\nauthorize_url = \"https://idp.example/auth\"\ntoken_url = \"http://127.0.0.1:{{port}}/token\"\n{provider}"
            ),
            "[oauth.plain] token_url",
        ),
    ];
    for (listen, service, expected) in own {
        let e = start_error(listen, &service).err().unwrap_or_else(|| panic!("{service} started"));
        assert!(e.contains(expected) && e.contains("points at this server"), "{service}: {e}");
    }
}

/// A connection to the server's own address is refused when a page makes
/// it, for addresses only known then: here the token endpoint a discovery
/// document names. With one worker, the callback page held it while its token
/// request waited for it (the 10 s stall of R22-19); now the request is refused
/// at once and the page hears of it.
#[test]
fn a_discovered_endpoint_on_this_server_is_refused_when_the_page_connects() {
    use std::net::TcpListener;
    let dir = site("discovered");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // The provider: its discovery document names this server's token endpoint.
    let idp = TcpListener::bind("127.0.0.1:0").unwrap();
    let issuer = format!("http://{}", idp.local_addr().unwrap());
    let doc = format!(
        r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/auth","token_endpoint":"http://{addr}/token","jwks_uri":"http://{addr}/jwks"}}"#
    );
    std::thread::spawn(move || {
        for s in idp.incoming() {
            let Ok(mut s) = s else { continue };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{doc}",
                doc.len()
            );
        }
    });
    write(
        &dir,
        "phv.toml",
        &format!(
            "[server]\nworkers = 1\n[oauth.p]\nissuer = \"{issuer}\"\nclient_id = \"site\"\nredirect_uri = \"http://{addr}/cb\"\n"
        ),
    );
    write(&dir, "public/login.phv", "<?phv\nimport std.oauth.OAuth;\nOAuth::provider(\"p\")->redirect();\n");
    write(
        &dir,
        "public/cb.phv",
        "<?phv\nimport std.oauth.{OAuth, OAuthError};\ntry {\n    OAuth::provider(\"p\")->callback();\n    echo \"signed in\";\n} catch (NetworkError $e) {\n    echo \"refused: \", $e->getMessage();\n} catch (OAuthError $e) {\n    echo \"error \", $e->error;\n}\n",
    );
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let opts = options(&dir);
    let server = std::thread::spawn(move || {
        serve_until(listener, opts, async {
            let _ = rx.await;
        })
    });
    let fetch = |path: &str, cookie: &str| {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\n{cookie}Connection: close\r\n\r\n").unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    };
    let login = fetch("/login", "");
    let location = login.lines().find_map(|l| l.strip_prefix("location: ").or(l.strip_prefix("Location: "))).unwrap();
    let state = location.split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
    let cookie = login
        .lines()
        .filter_map(|l| l.split_once(": ").filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie")).map(|(_, v)| v))
        .find_map(|v| v.split(';').next().filter(|c| c.starts_with("PHVSESSID=")))
        .unwrap()
        .to_string();
    let start = std::time::Instant::now();
    let answer = fetch(&format!("/cb?code=c&state={state}"), &format!("Cookie: {cookie}\r\n"));
    assert!(start.elapsed() < Duration::from_secs(3), "the callback took {:?}", start.elapsed());
    assert!(answer.contains("token endpoint: the address is this server") && answer.contains("own server"), "{answer}");
    assert_eq!(status_of(addr, "/"), 200);
    let _ = tx.send(());
    server.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A service on another port, or on an address that is not this machine's,
/// is not this server: the site starts and serves.
#[test]
fn a_service_elsewhere_is_fine() {
    let dir = site("fine");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let other = addr.port().wrapping_add(1).max(1024);
    write(
        &dir,
        "phv.toml",
        &format!(
            "[http.api]\nurl = \"http://127.0.0.1:{other}\"\n[http.doc]\nurl = \"http://192.0.2.1:{}\"\ntls = false\n\
             [websockets.game]\nurl = \"ws://localhost:{other}/socket\"\n",
            addr.port()
        ),
    );
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let opts = options(&dir);
    let server = std::thread::spawn(move || {
        serve_until(listener, opts, async {
            let _ = rx.await;
        })
    });
    assert_eq!(status_of(addr, "/"), 200);
    let _ = tx.send(());
    server.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
