//! WebSocket servers: each `tests/ws/<name>.phv` talks to a small server run
//! by the test, and its output is compared with the `.out` file next to it.
//! `PHV_BLESS=1` rewrites the expected files.

use phv_driver::{compile_with_overlay, run, RunOptions};
use phv_vm::{Host, Limits, WebSocketSpec};
use std::collections::{BTreeMap, HashMap};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::Message;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ws")
}

fn host(url: &str, tls: Option<bool>) -> Arc<Host> {
    let spec = WebSocketSpec { url: Some(url.to_string()), url_env: None, tls };
    Arc::new(Host { root: root(), websockets: BTreeMap::from([("main".to_string(), spec)]), ..Host::default() })
}

/// Echoes text messages, with a few commands: `bye` closes, `binary` answers
/// with a binary message, and `ping-then-echo` sends a ping first.
fn server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || {
                let Ok(mut ws) = tungstenite::accept(stream) else { return };
                while let Ok(msg) = ws.read() {
                    let Message::Text(t) = msg else { continue };
                    let reply = match t.as_str() {
                        "bye" => {
                            let _ = ws.close(None);
                            let _ = ws.flush();
                            return;
                        }
                        "binary" => Message::binary(vec![0u8, 1, 2]),
                        "ping-then-echo" => {
                            let _ = ws.send(Message::Ping(Vec::new().into()));
                            Message::text(format!("echo: {t}"))
                        }
                        _ => Message::text(format!("echo: {t}")),
                    };
                    if ws.send(reply).is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("ws://{addr}/secret-pw")
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

#[test]
fn echo() {
    let host = host(&server(), None);
    let actual = run_script(&host, "echo", Some(Duration::from_secs(30)));
    let expected_path = root().join("echo.out");
    if std::env::var_os("PHV_BLESS").is_some() {
        std::fs::write(&expected_path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&expected_path).unwrap_or_default().replace("\r\n", "\n");
    assert_eq!(actual, expected, "output of tests/ws/echo.phv");
}

/// Waiting for a message stops at the script's time limit.
#[test]
fn receive_stops_at_the_time_limit() {
    let host = host(&server(), None);
    let start = Instant::now();
    let out = run_script(&host, "slow", Some(Duration::from_secs(1)));
    assert!(out.contains("maximum execution time"), "{out}");
    assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
}

/// Remote servers need TLS unless the configuration says otherwise, and the
/// URL (which may hold a password) never shows up in messages.
#[test]
fn remote_servers_need_tls() {
    let out = run_script(&host("ws://game.invalid:28016/secret-pw", None), "connect", None);
    assert!(out.contains("use a wss:// URL"), "{out}");
    assert!(!out.contains("secret-pw"), "{out}");
    let out = run_script(&host("ws://game.invalid:28016/secret-pw", Some(false)), "connect", None);
    assert!(out.contains("cannot connect"), "{out}");
    assert!(!out.contains("secret-pw"), "{out}");
}

/// A server that is not a WebSocket server is reported with its HTTP status.
#[test]
fn http_answer_is_reported() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    let out = run_script(&host(&format!("ws://{addr}/secret-pw"), None), "connect", None);
    assert!(out.contains("HTTP 404"), "{out}");
    assert!(!out.contains("secret-pw"), "{out}");
}
