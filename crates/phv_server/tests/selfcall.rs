//! A service of `[http]` or `[websockets]` must not point at the server
//! itself: a page calling its own server holds a worker while it waits for
//! one, and four such pages on four workers stalled every request for 10
//! seconds (18th audit, H-3). The server refuses to start instead.

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
    let dir = site("refused");
    let listener = std::net::TcpListener::bind(listen).unwrap();
    let addr = listener.local_addr().unwrap();
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
