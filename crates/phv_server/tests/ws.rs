//! WebSocket connections answered by `public/*.ws.phv`: the script runs for
//! each event, keeps values between them, and publishes to topics.

use phv_server::{serve_on, ServeOptions};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::{Message, WebSocket};

const CHAT: &str = r#"<?phv
import std.ws.{socket, publish, WsEvent};
import std.http.{request, response};

$ws = socket();
if ($ws->event == WsEvent::Open) {
    $name = request()->query("name") ?? "";
    if ($name == "") {
        response()->status(403);
    } else {
        $ws->set("name", $name);
        $ws->join("lobby");
        $ws->send("welcome " . $name);
    }
} elseif ($ws->event == WsEvent::Message) {
    $text = $ws->message();
    if ($text == "count") {
        $n = (($ws->get("n") ?? "0")->toInt() ?? 0) + 1;
        $ws->set("n", (string)$n);
        $ws->send("count " . $n);
    } elseif ($text == "bye") {
        $ws->close(4000, "bye");
    } elseif ($text == "crash") {
        throw new RuntimeException("boom");
    } elseif ($text == "quiet") {
        $ws->leave("lobby");
    } else {
        publish("lobby", ($ws->get("name") ?? "?") . ": " . $text);
    }
} else {
    publish("lobby", ($ws->get("name") ?? "?") . " left");
}
"#;

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn site(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-ws-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "");
    write(&dir, "public/chat.ws.phv", CHAT);
    write(&dir, "public/rooms/[room].ws.phv", "<?phv\nimport std.ws.{socket, WsEvent};\nimport std.http.request;\n$ws = socket();\nif ($ws->event == WsEvent::Open) {\n    $ws->send(\"room \" . (request()->param(\"room\") ?? \"?\"));\n}\n");
    write(
        &dir,
        "public/notify.phv",
        "<?phv\nimport std.ws.publish;\npublish(\"lobby\", \"notice\");\necho \"sent\";\n",
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
        time_limit: Duration::from_secs(5),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    std::thread::sleep(Duration::from_millis(200));
    addr
}

type Ws = WebSocket<TcpStream>;

/// Opens `path`, sending `origin` when given; the HTTP status when refused.
fn open(addr: SocketAddr, path: &str, origin: Option<&str>) -> Result<Ws, u16> {
    let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("origin", o.parse().unwrap());
    }
    let tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    match tungstenite::client::client(req, tcp) {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r))) => Err(r.status().as_u16()),
        Err(e) => panic!("handshake: {e}"),
    }
}

fn text(ws: &mut Ws) -> String {
    loop {
        match ws.read().unwrap() {
            Message::Text(t) => return t.to_string(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected text, got {other:?}"),
        }
    }
}

/// Reads until the server closes; the close code it sent.
fn closed(ws: &mut Ws) -> Option<CloseCode> {
    loop {
        match ws.read() {
            Ok(Message::Close(f)) => return f.map(|f| f.code),
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

fn http_get(addr: SocketAddr, path: &str) -> String {
    use std::io::{Read, Write};
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn events_keep_values_and_publish_to_topics() {
    let dir = site("chat");
    let addr = start(&dir);
    let mut ann = open(addr, "/chat?name=ann", None).unwrap();
    assert_eq!(text(&mut ann), "welcome ann");
    let mut bob = open(addr, "/chat?name=bob", Some(&format!("http://{addr}"))).unwrap();
    assert_eq!(text(&mut bob), "welcome bob");

    ann.send(Message::text("hi")).unwrap();
    assert_eq!(text(&mut ann), "ann: hi");
    assert_eq!(text(&mut bob), "ann: hi");

    // Values kept from one event to the next, per connection.
    for n in 1..=3 {
        ann.send(Message::text("count")).unwrap();
        assert_eq!(text(&mut ann), format!("count {n}"));
    }
    bob.send(Message::text("count")).unwrap();
    assert_eq!(text(&mut bob), "count 1");

    // A page publishes too.
    assert!(http_get(addr, "/notify").ends_with("sent"));
    assert_eq!(text(&mut ann), "notice");
    assert_eq!(text(&mut bob), "notice");

    // A connection that left the topic gets nothing more from it.
    bob.send(Message::text("quiet")).unwrap();
    ann.send(Message::text("still here")).unwrap();
    assert_eq!(text(&mut ann), "ann: still here");

    // The script closes a connection; its Close event still runs.
    ann.send(Message::text("bye")).unwrap();
    assert_eq!(closed(&mut ann), Some(CloseCode::Library(4000)));
    let mut carl = open(addr, "/chat?name=carl", None).unwrap();
    assert_eq!(text(&mut carl), "welcome carl");
    drop(bob);

    // Parameters in the path, as for pages.
    let mut room = open(addr, "/rooms/blue", None).unwrap();
    assert_eq!(text(&mut room), "room blue");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn connections_are_refused_and_closed_safely() {
    let dir = site("refuse");
    let addr = start(&dir);
    // The Open event refuses with a status.
    assert_eq!(open(addr, "/chat", None).err(), Some(403));
    // Another site's page cannot connect with the visitor's cookies.
    assert_eq!(open(addr, "/chat?name=x", Some("https://evil.example")).err(), Some(403));
    // No script for the path, and a script is not a page.
    assert_eq!(open(addr, "/nothing", None).err(), Some(404));
    assert!(http_get(addr, "/chat").starts_with("HTTP/1.1 404"));
    assert!(http_get(addr, "/chat.ws").starts_with("HTTP/1.1 404"));
    assert!(http_get(addr, "/chat.ws.phv").starts_with("HTTP/1.1 404"));

    // A script error closes the connection with 1011; binary messages with 1003.
    let mut a = open(addr, "/chat?name=a", None).unwrap();
    assert_eq!(text(&mut a), "welcome a");
    a.send(Message::text("crash")).unwrap();
    assert_eq!(closed(&mut a), Some(CloseCode::Error));
    let mut b = open(addr, "/chat?name=b", None).unwrap();
    assert_eq!(text(&mut b), "welcome b");
    b.send(Message::binary(vec![1, 2, 3])).unwrap();
    assert_eq!(closed(&mut b), Some(CloseCode::Unsupported));
    // A message over 64 KB.
    let mut c = open(addr, "/chat?name=c", None).unwrap();
    assert_eq!(text(&mut c), "welcome c");
    let _ = c.send(Message::text("x".repeat(100 * 1024)));
    assert!(matches!(closed(&mut c), Some(CloseCode::Size) | None));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Found by the 1.0 audit: `[cors] origins = ["*"]` (no credentials) let any
/// site open a WebSocket with the visitor's cookies, and `Origin: null` too.
/// Only a site listed by name with `credentials = true` may.
#[test]
fn cors_lets_other_sites_open_websockets_only_by_name_with_credentials() {
    let dir = site("cors-any");
    write(&dir, "phv.toml", "[cors]\norigins = [\"*\"]\n");
    let addr = start(&dir);
    assert_eq!(open(addr, "/chat?name=x", Some("https://evil.example")).err(), Some(403));
    assert_eq!(open(addr, "/chat?name=x", Some("null")).err(), Some(403));
    let _ = std::fs::remove_dir_all(&dir);

    let dir = site("cors-named");
    write(&dir, "phv.toml", "[cors]\norigins = [\"https://app.example\"]\ncredentials = true\n");
    let addr = start(&dir);
    let mut ws = open(addr, "/chat?name=x", Some("https://app.example")).unwrap();
    assert_eq!(text(&mut ws), "welcome x");
    assert_eq!(open(addr, "/chat?name=x", Some("https://evil.example")).err(), Some(403));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Found by the 1.0 audit: a request with two `Host` fields was answered (a
/// proxy in front may read the other one).
#[test]
fn two_host_fields_are_refused() {
    use std::io::{Read, Write};
    let dir = site("hosts");
    let addr = start(&dir);
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET /notify HTTP/1.1\r\nHost: a.example\r\nHost: b.example\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sends PUBLISH to the Redis server at `url` (`redis://host[:port]`) as
/// anyone with access to it could.
fn redis_publish(url: &str, channel: &str, payload: &[u8]) {
    use std::io::{Read, Write};
    let rest = url.trim_start_matches("redis://");
    let hostport = rest.split('/').next().unwrap();
    let hostport = if hostport.contains(':') { hostport.to_string() } else { format!("{hostport}:6379") };
    let mut s = TcpStream::connect(hostport).unwrap();
    let mut cmd = Vec::new();
    cmd.extend_from_slice(
        format!("*3\r\n$7\r\nPUBLISH\r\n${}\r\n{channel}\r\n${}\r\n", channel.len(), payload.len()).as_bytes(),
    );
    cmd.extend_from_slice(payload);
    cmd.extend_from_slice(b"\r\n");
    s.write_all(&cmd).unwrap();
    let mut reply = [0u8; 64];
    let _ = s.read(&mut reply).unwrap();
}

/// `[server] publish_store`: a message published in one server process
/// reaches the connections of the others, once; messages that are not signed
/// with the site's secret, or break the rules of `publish()`, are dropped.
/// Needs `PHV_TEST_REDIS_URL`.
#[test]
fn publish_reaches_the_other_server_processes() {
    let Ok(url) = std::env::var("PHV_TEST_REDIS_URL") else {
        eprintln!("PHV_TEST_REDIS_URL is not set: skipped");
        return;
    };
    let secret = "s".repeat(40);
    std::env::set_var("PHV_TEST_WS_SECRET", &secret);
    std::env::set_var("PHV_TEST_WS_OTHER", "o".repeat(40));
    let toml = |var: &str| {
        format!(
            "[databases.bus]\nurl = \"{url}\"\n[security]\nsecret_env = \"{var}\"\n[server]\npublish_store = \"bus\"\n"
        )
    };
    let dir = site("bus");
    write(&dir, "phv.toml", &toml("PHV_TEST_WS_SECRET"));
    let other = site("bus-other");
    write(&other, "phv.toml", &toml("PHV_TEST_WS_OTHER"));
    let (a, b, c) = (start(&dir), start(&dir), start(&other));
    // Each process subscribes in the background.
    std::thread::sleep(Duration::from_millis(800));

    let mut ann = open(a, "/chat?name=ann", None).unwrap();
    assert_eq!(text(&mut ann), "welcome ann");
    let mut bob = open(b, "/chat?name=bob", None).unwrap();
    assert_eq!(text(&mut bob), "welcome bob");
    let mut carl = open(c, "/chat?name=carl", None).unwrap();
    assert_eq!(text(&mut carl), "welcome carl");

    ann.send(Message::text("hi")).unwrap();
    assert_eq!(text(&mut ann), "ann: hi");
    assert_eq!(text(&mut bob), "ann: hi");
    assert!(http_get(b, "/notify").ends_with("sent"));
    assert_eq!(text(&mut bob), "notice");
    assert_eq!(text(&mut ann), "notice");

    // Forged messages: unsigned, signed with another key, huge, or breaking the rules.
    let key = phv_std::server_key(secret.as_bytes(), "phv publish v1");
    let tag: String = phv_std::hmac_sha256(&key, b"channel")[..8].iter().map(|x| format!("{x:02x}")).collect();
    let channel = format!("phv:publish:{tag}");
    let frame = |key: &[u8], topic: &str, text: &str| {
        let mut f = b"PHV1".to_vec();
        f.extend_from_slice(&[9u8; 16]);
        f.extend_from_slice(&(topic.len() as u16).to_be_bytes());
        f.extend_from_slice(topic.as_bytes());
        f.extend_from_slice(text.as_bytes());
        let mac = phv_std::hmac_sha256(key, &f);
        f.extend_from_slice(&mac);
        f
    };
    redis_publish(&url, &channel, b"lobby: forged");
    redis_publish(&url, &channel, &frame(b"wrong key, wrong key, wrong key!", "lobby", "forged"));
    redis_publish(&url, &channel, &frame(&key, "lob\u{7}by", "forged"));
    redis_publish(&url, &channel, &vec![b'x'; 3 << 20]);
    // A message signed with the site's key is delivered (the key is the secret).
    redis_publish(&url, &channel, &frame(&key, "lobby", "signed"));
    assert_eq!(text(&mut ann), "signed");
    assert_eq!(text(&mut bob), "signed");

    // Nothing arrived twice, nothing forged, nothing from the other site.
    for (ws, name) in [(&mut ann, "ann"), (&mut bob, "bob"), (&mut carl, "carl")] {
        ws.send(Message::text("count")).unwrap();
        assert_eq!(text(ws), "count 1", "{name}");
    }
    // The other site is not reached either.
    assert!(http_get(c, "/notify").ends_with("sent"));
    assert_eq!(text(&mut carl), "notice");
    ann.send(Message::text("count")).unwrap();
    assert_eq!(text(&mut ann), "count 2");
    for d in [dir, other] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// While the publish store is down, each process still delivers its own
/// messages; after it comes back, messages cross again. Restarts the Docker
/// container named by `PHV_TEST_REDIS_CONTAINER` (run with `--ignored`).
#[test]
#[ignore]
fn publish_survives_a_restart_of_the_store() {
    let (Ok(url), Ok(container)) = (std::env::var("PHV_TEST_REDIS_URL"), std::env::var("PHV_TEST_REDIS_CONTAINER"))
    else {
        eprintln!("PHV_TEST_REDIS_URL and PHV_TEST_REDIS_CONTAINER are not set: skipped");
        return;
    };
    std::env::set_var("PHV_TEST_WS_SECRET2", "r".repeat(40));
    let dir = site("bus-restart");
    write(
        &dir,
        "phv.toml",
        &format!("[databases.bus]\nurl = \"{url}\"\n[security]\nsecret_env = \"PHV_TEST_WS_SECRET2\"\n[server]\npublish_store = \"bus\"\n"),
    );
    let (a, b) = (start(&dir), start(&dir));
    std::thread::sleep(Duration::from_millis(800));
    let mut ann = open(a, "/chat?name=ann", None).unwrap();
    assert_eq!(text(&mut ann), "welcome ann");
    let mut bob = open(b, "/chat?name=bob", None).unwrap();
    assert_eq!(text(&mut bob), "welcome bob");
    let docker = |args: &[&str]| assert!(std::process::Command::new("docker").args(args).status().unwrap().success());

    docker(&["pause", &container]);
    // Local delivery does not wait for the store.
    let t = std::time::Instant::now();
    ann.send(Message::text("during")).unwrap();
    assert_eq!(text(&mut ann), "ann: during");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    docker(&["unpause", &container]);
    docker(&["restart", &container]);
    // Back within the back-off (30 s at most).
    let t = std::time::Instant::now();
    loop {
        assert!(t.elapsed() < Duration::from_secs(40), "the bus did not come back");
        assert!(http_get(a, "/notify").ends_with("sent"));
        assert_eq!(text(&mut ann), "notice");
        bob.get_mut().set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let got = loop {
            match bob.read() {
                Ok(Message::Text(t)) => break Some(t.to_string()),
                Ok(_) => continue,
                Err(_) => break None,
            }
        };
        match got.as_deref() {
            Some("notice") => break,
            Some("ann: during") => continue,
            Some(other) => panic!("{other}"),
            None => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    eprintln!("the bus came back after {:?}", t.elapsed());
    let _ = std::fs::remove_dir_all(&dir);
}
