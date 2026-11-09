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
