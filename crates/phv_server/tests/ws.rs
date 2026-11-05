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
