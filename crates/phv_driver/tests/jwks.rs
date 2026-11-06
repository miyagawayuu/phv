//! Key sets fetched over HTTP (`KeySet::fetch` of std.jwt): kept between
//! scripts, fetched again for a key id they lack (a key rotation), but not
//! more often than every 30 seconds however many unknown ids tokens name;
//! kept per service and headers; decoded within the script's memory limit;
//! and not asked for again at once after a failure.

use phv_driver::{compile_with_overlay, run, RunOptions};
use phv_vm::alloc::CountingAlloc;
use phv_vm::{Host, HttpSpec, Limits};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

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
    run_limited(root, host, src, Limits::default())
}

fn run_limited(root: &Path, host: Option<Arc<Host>>, src: &str, limits: Limits) -> String {
    let path = root.join("main.phv");
    std::fs::write(&path, src).unwrap();
    let c = match compile_with_overlay(root, &path, HashMap::new()) {
        Ok(c) => c,
        Err(e) => return e.render(false),
    };
    let outcome = run(&c, RunOptions { html: false, limits, http: None, host });
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

/// Serves what `answer` gives for each request (its head, as text):
/// status line and body. Counts the requests.
fn server_with(answer: impl Fn(&str) -> (&'static str, String) + Send + 'static, hits: Arc<AtomicUsize>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            hits.fetch_add(1, Ordering::SeqCst);
            let (status, body) = answer(&String::from_utf8_lossy(&buf[..n]));
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(reply.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn service(url: &str, headers: &[(&str, &str)]) -> HttpSpec {
    let headers = headers.iter().map(|(h, v)| (h.to_string(), Some(v.to_string()))).collect();
    HttpSpec { url: Some(url.to_string()), url_env: None, tls: None, headers, headers_env: vec![] }
}

/// Two services may share a URL and differ in a header that makes the server
/// answer with another key set (a tenant): each has its own set. Found by the
/// 16th assessment: sets were kept by URL only, so tenant a's token was valid
/// through tenant b's service.
#[test]
fn key_sets_are_kept_per_service_and_headers() {
    let root = temp_root("tenants");
    let made = run_source(
        &root,
        None,
        &format!(
            "import std.jwt.JwtKey;\n\
             $a = JwtKey::fromPem(\"{KEY_A}\", \"a\");\n\
             $b = JwtKey::fromPem(\"{KEY_B}\", \"b\");\n\
             echo $a->publicJwk(), \"\\n\", $b->publicJwk(), \"\\n\", $a->sign([\"sub\" => \"tenant a\"]), \"\\n\";\n"
        ),
    );
    let lines: Vec<String> = made.lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 3, "{made}");
    let (jwk_a, jwk_b) = (lines[0].clone(), lines[1].clone());
    let hits = Arc::new(AtomicUsize::new(0));
    let url = server_with(
        move |head| {
            let tenant_a = head.lines().any(|l| l.eq_ignore_ascii_case("x-tenant: a"));
            ("200 OK", format!("{{\"keys\":[{}]}}", if tenant_a { &jwk_a } else { &jwk_b }))
        },
        hits.clone(),
    );
    let host = Arc::new(Host {
        root: root.clone(),
        http: BTreeMap::from([
            ("ta".to_string(), service(&url, &[("X-Tenant", "a")])),
            ("tb".to_string(), service(&url, &[("X-Tenant", "b")])),
        ]),
        ..Host::default()
    });
    let verify = |svc: &str| {
        run_source(
            &root,
            Some(host.clone()),
            &format!(
                "import std.jwt.KeySet;\n\
                 $c = KeySet::fetch(\"{svc}\", \"/jwks.json\")->verify(\"{}\");\n\
                 $sub = $c == null ? \"refused\" : $c[\"sub\"] ?? null;\n\
                 echo $sub is string ? $sub : \"?\";\n",
                lines[2]
            ),
        )
    };
    assert_eq!(verify("ta"), "tenant a");
    assert_eq!(verify("tb"), "refused", "tenant b's set holds only b's key");
    assert_eq!(verify("ta"), "tenant a", "a's set is still kept");
    // ta once; tb once, and once more for the key id a that its set lacks.
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    let _ = std::fs::remove_dir_all(&root);
}

/// A JWKS is decoded within the script's memory limit, whether it is given
/// (`fromJson`) or fetched. Found by the 16th assessment: a 522 KB set of
/// empty objects took 25 MB under a 16 MB limit.
#[test]
fn key_sets_are_decoded_within_the_memory_limit() {
    let root = temp_root("memory");
    let doc = format!("{{\"keys\":[{{\"kty\":\"oct\"}}],\"x\":[{}{{}}]}}", "{},".repeat(174_000));
    assert!(doc.len() < 512 << 10);
    let hits = Arc::new(AtomicUsize::new(0));
    let served = doc.clone();
    let url = server_with(move |_| ("200 OK", served.clone()), hits.clone());
    let host = Arc::new(Host {
        root: root.clone(),
        http: BTreeMap::from([("big".to_string(), service(&url, &[]))]),
        ..Host::default()
    });
    let limits = || Limits { max_memory: Some(16 << 20), ..Limits::default() };
    let given = r#"import std.jwt.KeySet;
$doc = "{\"keys\":[{\"kty\":\"oct\"}],\"x\":[" . str_repeat("{},", 174000) . "{}]}";
echo strlen($doc), "\n";
KeySet::fromJson($doc);
"#;
    let out = run_limited(&root, None, given, limits());
    assert!(out.starts_with(&format!("{}\n", doc.len())), "{out}");
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "fromJson: {out}");
    let fetched = "import std.jwt.KeySet;\nKeySet::fetch(\"big\", \"/jwks.json\");\n";
    let out = run_limited(&root, Some(host), fetched, limits());
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "fetch: {out}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// A key set that cannot be fetched, and of which no copy is kept, is not
/// asked for again by every request (each would wait for the provider up to
/// its timeout): for 10 seconds the same error comes back at once.
#[test]
fn a_failed_first_fetch_is_not_repeated_at_once() {
    let root = temp_root("down");
    let hits = Arc::new(AtomicUsize::new(0));
    let url = server_with(|_| ("503 Service Unavailable", "{}".to_string()), hits.clone());
    let host = Arc::new(Host {
        root: root.clone(),
        http: BTreeMap::from([("down".to_string(), service(&url, &[]))]),
        ..Host::default()
    });
    let src = "import std.jwt.KeySet;\n\
               try { KeySet::fetch(\"down\", \"/jwks.json\"); } catch (NetworkError $e) { echo $e->getMessage(); }\n";
    let first = run_source(&root, Some(host.clone()), src);
    assert!(first.contains("the key set answered 503"), "{first}");
    for _ in 0..5 {
        let again = run_source(&root, Some(host.clone()), src);
        assert!(again.contains("answered 503") && again.contains("at most every 10 seconds"), "{again}");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&root);
}
