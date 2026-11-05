//! HTTPS with `[server] tls_cert` / `tls_key`: HTTP/2 and HTTP/1.1 chosen by
//! ALPN, HTTP/2 without TLS only with `h2c = true`, and the files checked.

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{Request, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use phv_server::{serve_on, ServeOptions};
use rustls_pki_types::{CertificateDer, ServerName};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// A site whose page shows the host and two cookies, with a self-signed
/// certificate for `localhost`; returns its directory and the certificate.
fn site(name: &str, toml: &str) -> (PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("phv-tls-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    write(&dir, "certs/cert.pem", &ck.cert.pem());
    write(&dir, "certs/key.pem", &ck.key_pair.serialize_pem());
    write(&dir, "phv.toml", toml);
    write(
        &dir,
        "public/index.phv",
        "<?phv\nimport std.http.request;\necho \"host=\", request()->header(\"host\") ?? \"-\", \" a=\", \
         request()->cookie(\"a\") ?? \"-\", \" b=\", request()->cookie(\"b\") ?? \"-\";\n",
    );
    (dir, ck.cert.der().to_vec())
}

fn start(dir: &Path) -> Result<SocketAddr, String> {
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
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = serve_on(listener, opts);
        let _ = tx.send(r);
    });
    // A configuration error ends the server at once.
    match rx.recv_timeout(Duration::from_millis(500)) {
        Ok(Err(e)) => Err(e),
        Ok(Ok(())) => Err("the server stopped".into()),
        Err(_) => Ok(addr),
    }
}

const TLS_TOML: &str = "[server]\ntls_cert = \"certs/cert.pem\"\ntls_key = \"certs/key.pem\"\n";

struct Got {
    status: u16,
    version: Version,
    alpn: Option<Vec<u8>>,
    hsts: bool,
    body: String,
}

/// GET `/` over TLS offering `alpn`, with the cookies in two fields.
async fn get_tls(addr: SocketAddr, cert: &[u8], alpn: &[u8]) -> Got {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(cert.to_vec())).unwrap();
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![alpn.to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let tls = connector.connect(ServerName::try_from("localhost").unwrap(), tcp).await.unwrap();
    let negotiated = tls.get_ref().1.alpn_protocol().map(|p| p.to_vec());
    let req = |version| {
        Request::builder()
            .version(version)
            .uri(format!("https://localhost:{}/", addr.port()))
            .header("cookie", "a=1")
            .header("cookie", "b=2")
            .body(Empty::<Bytes>::new())
            .unwrap()
    };
    let resp = if negotiated.as_deref() == Some(b"h2") {
        let (mut send, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls)).await.unwrap();
        tokio::spawn(conn);
        send.send_request(req(Version::HTTP_2)).await.unwrap()
    } else {
        let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await.unwrap();
        tokio::spawn(conn);
        let mut r = req(Version::HTTP_11);
        r.headers_mut().insert("host", format!("localhost:{}", addr.port()).parse().unwrap());
        send.send_request(r).await.unwrap()
    };
    let status = resp.status().as_u16();
    let version = resp.version();
    let hsts = resp.headers().contains_key("strict-transport-security");
    let body = String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).into_owned();
    Got { status, version, alpn: negotiated, hsts, body }
}

/// GET `/` as HTTP/2 without TLS ("prior knowledge").
async fn get_h2c(addr: SocketAddr) -> Result<u16, String> {
    let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let (mut send, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(conn);
    let req = Request::builder()
        .uri(format!("http://localhost:{}/", addr.port()))
        .body(Empty::<Bytes>::new())
        .map_err(|e| e.to_string())?;
    let r = tokio::time::timeout(Duration::from_secs(10), send.send_request(req))
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(|e| e.to_string())?;
    Ok(r.status().as_u16())
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
}

#[test]
fn https_serves_http2_and_http1() {
    let (dir, cert) = site("alpn", TLS_TOML);
    let addr = start(&dir).unwrap();
    let rt = rt();

    let h2 = rt.block_on(get_tls(addr, &cert, b"h2"));
    assert_eq!(h2.alpn.as_deref(), Some(&b"h2"[..]));
    assert_eq!((h2.status, h2.version), (200, Version::HTTP_2));
    assert!(h2.hsts, "HTTPS sends HSTS");
    // The host comes from `:authority`, and the two cookie fields are one.
    assert_eq!(h2.body, format!("host=localhost:{} a=1 b=2", addr.port()));

    let h1 = rt.block_on(get_tls(addr, &cert, b"http/1.1"));
    assert_eq!(h1.alpn.as_deref(), Some(&b"http/1.1"[..]));
    assert_eq!((h1.status, h1.version), (200, Version::HTTP_11));
    assert!(h1.body.starts_with(&format!("host=localhost:{} a=1", addr.port())), "{}", h1.body);

    // Plain HTTP/2 is not accepted on a TLS port.
    assert!(rt.block_on(get_h2c(addr)).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn http2_without_tls_only_when_enabled() {
    let (dir, _) = site("plain", "");
    let addr = start(&dir).unwrap();
    assert!(rt().block_on(get_h2c(addr)).is_err(), "h2c is off by default");
    let _ = std::fs::remove_dir_all(&dir);

    let (dir, _) = site("h2c", "[server]\nh2c = true\n");
    let addr = start(&dir).unwrap();
    assert_eq!(rt().block_on(get_h2c(addr)), Ok(200));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_tls_settings_stop_the_server() {
    // The key must not be served.
    let (dir, _) = site("public-key", "[server]\ntls_cert = \"certs/cert.pem\"\ntls_key = \"public/key.pem\"\n");
    std::fs::copy(dir.join("certs/key.pem"), dir.join("public/key.pem")).unwrap();
    let e = start(&dir).unwrap_err();
    assert!(e.contains("inside public/"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);

    // A key that is not the certificate's.
    let (dir, _) = site("other-key", TLS_TOML);
    let other = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    write(&dir, "certs/key.pem", &other.key_pair.serialize_pem());
    let e = start(&dir).unwrap_err();
    assert!(e.contains("does not belong to the certificate"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);

    // Not PEM: the message names the file, not what is in it.
    let (dir, _) = site("not-pem", TLS_TOML);
    write(&dir, "certs/key.pem", "SECRET-CONTENT");
    let e = start(&dir).unwrap_err();
    assert!(e.contains("is not a PEM private key") && !e.contains("SECRET"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);

    // Both files or neither.
    let (dir, _) = site("half", "[server]\ntls_cert = \"certs/cert.pem\"\n");
    let e = start(&dir).unwrap_err();
    assert!(e.contains("both tls_cert and tls_key"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A client that opens a connection and sends nothing is dropped when the
/// handshake time runs out, so it cannot hold a connection slot.
#[test]
fn a_stalled_handshake_is_dropped() {
    use std::io::Read;
    let (dir, _) = site("stall", TLS_TOML);
    let addr = start(&dir).unwrap();
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let t = std::time::Instant::now();
    let mut buf = [0u8; 16];
    let n = s.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0);
    assert!(t.elapsed() < Duration::from_secs(15), "took {:?}", t.elapsed());
    let _ = std::fs::remove_dir_all(&dir);
}
