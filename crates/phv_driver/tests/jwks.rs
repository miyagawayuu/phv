//! Key sets fetched over HTTP (`KeySet::fetch` of std.jwt): kept between
//! scripts, fetched again for a key id they lack (a key rotation), but not
//! more often than every 30 seconds however many unknown ids tokens name.

use phv_driver::{compile_with_overlay, run, RunOptions};
use phv_vm::{Host, HttpSpec};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Test keys made for this file.
const KEY_A: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg5QolW45chCm9++y+
TfYGzKMO0aDejBcDv5tdEEI4aqOhRANCAATQ2Qv3/CLby0lGuu3gysrspRipof5T
3D5yQajI5Dm4fCz+kMNuwOQaa+MAJvYaiIdiB/8SO6nyS8jpaQiVb+n0
-----END PRIVATE KEY-----";
const KEY_B: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgxIx+5jr9oEkfxCmK
MoySIsr0uZAi651zNO15w5Q6uo+hRANCAATCmPMx+TIZQ9hZ39ltEKpZeRVrWW1V
3CzFyd6SUxWSwZq5X/SKFP4Jot9cOpBlRhtZdAqC7z5zOl98KTVoa3cv
-----END PRIVATE KEY-----";

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-jwks-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_source(root: &Path, host: Option<Arc<Host>>, src: &str) -> String {
    let path = root.join("main.phv");
    std::fs::write(&path, src).unwrap();
    let c = match compile_with_overlay(root, &path, HashMap::new()) {
        Ok(c) => c,
        Err(e) => return e.render(false),
    };
    let outcome = run(&c, RunOptions { html: false, limits: Default::default(), http: None, host });
    let mut out = outcome.output;
    if let Some(err) = outcome.error {
        out.push_str("\n--- runtime error ---\n");
        out.push_str(&err.render());
    }
    out
}

/// Serves the current JWKS at /jwks.json, counting the requests.
fn server(jwks: Arc<Mutex<String>>, hits: Arc<AtomicUsize>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            hits.fetch_add(1, Ordering::SeqCst);
            let body = jwks.lock().unwrap().clone();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: max-age=600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(reply.as_bytes());
        }
    });
    format!("http://{addr}")
}

#[test]
fn key_sets_are_kept_and_fetched_again_for_new_key_ids() {
    let root = temp_root("rotate");
    // The public keys and a token of each key, from PHV itself.
    let made = run_source(
        &root,
        None,
        &format!(
            "import std.jwt.JwtKey;\n\
             $a = JwtKey::fromPem(\"{KEY_A}\", \"a\");\n\
             $b = JwtKey::fromPem(\"{KEY_B}\", \"b\");\n\
             echo $a->publicJwk(), \"\\n\", $b->publicJwk(), \"\\n\";\n\
             echo $a->sign([\"sub\" => \"from a\"]), \"\\n\", $b->sign([\"sub\" => \"from b\"]), \"\\n\";\n\
             echo JwtKey::fromPem(\"{KEY_B}\", \"c\")->sign([\"sub\" => \"from c\"]), \"\\n\";\n"
        ),
    );
    let lines: Vec<&str> = made.lines().collect();
    assert_eq!(lines.len(), 5, "{made}");
    let (jwk_a, jwk_b, token_a, token_b, token_c) = (lines[0], lines[1], lines[2], lines[3], lines[4]);

    let jwks = Arc::new(Mutex::new(format!("{{\"keys\":[{jwk_a}]}}")));
    let hits = Arc::new(AtomicUsize::new(0));
    let url = server(jwks.clone(), hits.clone());
    let spec = HttpSpec { url: Some(url), url_env: None, tls: None, headers: vec![], headers_env: vec![] };
    let host =
        Arc::new(Host { root: root.clone(), http: BTreeMap::from([("idp".to_string(), spec)]), ..Host::default() });
    let verify = |token: &str| {
        run_source(
            &root,
            Some(host.clone()),
            &format!(
                "import std.jwt.KeySet;\n\
                 $c = KeySet::fetch(\"idp\", \"/jwks.json\")->verify(\"{token}\");\n\
                 $sub = $c == null ? \"refused\" : $c[\"sub\"] ?? null;\n\
                 echo $sub is string ? $sub : \"?\";\n"
            ),
        )
    };
    assert_eq!(verify(token_a), "from a");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // Kept: a second script does not fetch it again.
    assert_eq!(verify(token_a), "from a");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // The provider adds key b: a token naming b makes the set be fetched again.
    *jwks.lock().unwrap() = format!("{{\"keys\":[{jwk_a},{jwk_b}]}}");
    assert_eq!(verify(token_b), "from b");
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    // Tokens naming unknown ids do not make it fetch again within 30 seconds.
    for _ in 0..5 {
        assert_eq!(verify(token_c), "refused");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let _ = std::fs::remove_dir_all(&root);
}
