//! `std.image` in a real server: an uploaded image is read from its bytes,
//! whatever its name and type say, and `send()` answers with the image.

use base64::Engine;
use phv_server::{serve_on, ServeOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

/// 40 × 30: left half red, right half blue.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAACgAAAAeCAYAAABe3VzdAAAAOUlEQVR42u3OQQ0AMAwDsSIJf1Dlsg1EXpNPur9nXpuc1slWr+IAAQEBAQEBAQEBAQEBAQEBAf8CXlC/ykG59FM8AAAAAElFTkSuQmCC";
/// A PNG whose header declares 30000 × 30000 pixels.
const BOMB: &str = "iVBORw0KGgoAAAANSUhEUgAAdTAAAHUwCAYAAABmJ/i6AAAAeElEQVR42u3BMQEAAADCoPVPbQ0PoAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIBXA4avAAEakw3bAAAAAElFTkSuQmCC";

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
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

/// Posts `file` as the field `photo` of a form; the status, head and body.
fn upload(addr: SocketAddr, path: &str, name: &str, ctype: &str, file: &[u8]) -> (u16, String, Vec<u8>) {
    let b = "PHVIMAGEBOUNDARY";
    let mut body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"photo\"; filename=\"{name}\"\r\nContent-Type: {ctype}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: test\r\nContent-Type: multipart/form-data; boundary={b}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(&body);
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(&req).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&buf[..split]).to_ascii_lowercase();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head, buf[split + 4..].to_vec())
}

#[test]
fn uploads_are_read_by_their_bytes_and_sent_as_images() {
    let dir = std::env::temp_dir().join(format!("phv-image-server-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(&dir, "phv.toml", "[security]\ncsrf = false\n");
    write(
        &dir,
        "public/thumb.phv",
        r#"<?phv
import std.http.{request, response};
import std.image.Image;
$file = request()->file("photo");
if ($file == null) {
    response()->status(400);
    return;
}
try {
    $img = Image::fromUpload($file);
} catch (ValueError $e) {
    response()->status(422);
    echo $e->getMessage();
    return;
}
echo "<p>not sent</p>";
$img->cover(8, 8)->send("jpeg", 70);
"#,
    );
    write(
        &dir,
        "public/size.phv",
        r#"<?phv
import std.http.request;
import std.image.Image;
$file = request()->file("photo");
if ($file != null) {
    $img = Image::fromUpload($file);
    echo $img->format, " ", $img->width, "×", $img->height, " ", $img->toDataUrl("png");
}
"#,
    );
    let addr = start(&dir);
    let png = base64::engine::general_purpose::STANDARD.decode(PNG).unwrap();

    // Named and typed as a GIF: read as the PNG it is.
    let (status, head, body) = upload(addr, "/thumb", "cat.gif", "image/gif", &png);
    assert_eq!(status, 200, "{head}");
    assert!(head.contains("content-type: image/jpeg"), "{head}");
    assert!(head.contains("x-content-type-options: nosniff"), "{head}");
    assert!(body.starts_with(&[0xFF, 0xD8, 0xFF]) && body.ends_with(&[0xFF, 0xD9]), "{} bytes", body.len());
    assert!(!body.windows(5).any(|w| w == b"<p>no"));

    // A data: URL is escaped as any value, and holds nothing to escape.
    let (status, _, body) = upload(addr, "/size", "a.png", "image/png", &png);
    let text = String::from_utf8(body).unwrap();
    assert_eq!(status, 200);
    assert!(text.starts_with("png 40×30 data:image/png;base64,iVBOR"), "{text}");

    // Text sent as an image, and a bomb's header: refused before anything is decoded.
    let (status, _, body) = upload(addr, "/thumb", "x.png", "image/png", b"<svg onload=alert(1)>");
    assert_eq!(
        (status, String::from_utf8_lossy(&body).as_ref()),
        (422, "Image::fromUpload(): not a PNG, JPEG, GIF or WebP image")
    );
    let bomb = base64::engine::general_purpose::STANDARD.decode(BOMB).unwrap();
    let (status, _, body) = upload(addr, "/thumb", "x.png", "image/png", &bomb);
    assert_eq!(status, 422);
    assert!(String::from_utf8_lossy(&body).contains("the image is too large"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A GET of `path` from the address `from` (any of 127.0.0.0/8): the status,
/// the body and how long it took.
fn get_from(addr: SocketAddr, from: [u8; 4], path: &str) -> (u16, String, Duration) {
    let start = std::time::Instant::now();
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    socket.bind(&SocketAddr::from((from, 0)).into()).unwrap();
    socket.connect(&addr.into()).unwrap();
    let mut s: TcpStream = socket.into();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body, start.elapsed())
}

/// One client address keeps converting with more requests than there are
/// slots: its conversions wait in turn, at most half of the slots are its,
/// and a conversion from another address gets a slot at once and completes
/// (the 20th audit's R23-8: in 1.3.1 every conversion past the slots threw).
#[test]
fn one_address_cannot_take_every_image_conversion_slot() {
    let dir = std::env::temp_dir().join(format!("phv-image-slots-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // 8 workers: 2 slots, 1 for each address.
    write(&dir, "phv.toml", "[server]\nworkers = 8\n");
    let tiny = PNG;
    write(
        &dir,
        "public/slow.phv",
        &format!(
            r#"<?phv
import std.http.request;
import std.image.Image;
if (request()->query("warm") != null) {{
    return;
}}
echo strlen(Image::fromBase64("{tiny}")->resize(1400, 1400)->toBase64("png")) > 0 ? "slow done" : "";
"#
        ),
    );
    write(
        &dir,
        "public/small.phv",
        &format!(
            r#"<?phv
import std.image.Image;
echo strlen(Image::fromBase64("{tiny}")->fit(16)->toBase64("png")) > 0 ? "small done" : "";
"#
        ),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = ServeOptions {
        root: dir.clone(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: true,
        time_limit: Duration::from_secs(60),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    // The pages are compiled first.
    assert_eq!(get_from(addr, [127, 0, 0, 1], "/small").0, 200);
    assert_eq!(get_from(addr, [127, 0, 0, 1], "/slow?warm=1").0, 200);
    let one_alone = get_from(addr, [127, 0, 0, 1], "/slow");
    assert_eq!((one_alone.0, one_alone.1.as_str()), (200, "slow done"));
    // Four slow conversions at once from one address: one runs at a time.
    let busy: Vec<_> = (0..4).map(|_| std::thread::spawn(move || get_from(addr, [127, 0, 0, 1], "/slow"))).collect();
    std::thread::sleep(one_alone.2 / 4);
    // Another address: the other slot, at once.
    let (status, body, took) = get_from(addr, [127, 0, 0, 2], "/small");
    assert_eq!((status, body.as_str()), (200, "small done"));
    let busy: Vec<_> = busy.into_iter().map(|t| t.join().unwrap()).collect();
    for (status, body, _) in &busy {
        assert_eq!((*status, body.as_str()), (200, "slow done"));
    }
    let longest = busy.iter().map(|b| b.2).max().unwrap();
    // The last waited for three conversions before it, one after the other.
    assert!(longest > one_alone.2 * 2, "{longest:?} for four, {:?} for one", one_alone.2);
    // The other address did not wait for them.
    assert!(took < one_alone.2 / 2, "{took:?} for the other address, {:?} for one slow conversion", one_alone.2);
    let _ = std::fs::remove_dir_all(&dir);
}
