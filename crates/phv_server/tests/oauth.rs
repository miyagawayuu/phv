//! Sign-in with OpenID Connect (`std.oauth`) against a provider run by the
//! test: discovery, the authorization request (state, nonce, PKCE), the code
//! exchange with the client secret, and the checks of the ID token.

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use phv_server::{serve_on, ServeOptions};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the provider does with the next token request.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Good,
    WrongNonce,
    WrongAudience,
    OtherKey,
    Expired,
}

struct Provider {
    key: EcdsaKeyPair,
    other: EcdsaKeyPair,
    issuer: String,
    mode: Mode,
    /// The code_challenge and nonce of the last authorization request, by state.
    started: HashMap<String, (String, String)>,
    token_requests: usize,
    /// The token_type of the token responses.
    token_type: &'static str,
    /// The discovery document says every authorization response carries `iss`.
    iss_supported: bool,
    discovery_requests: usize,
}

fn new_key() -> EcdsaKeyPair {
    let rng = SystemRandom::new();
    let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, doc.as_ref(), &rng).unwrap()
}

fn jwk(key: &EcdsaKeyPair, kid: &str) -> String {
    let p = key.public_key().as_ref();
    format!(
        r#"{{"kty":"EC","crv":"P-256","x":"{}","y":"{}","kid":"{kid}","use":"sig"}}"#,
        URL_SAFE_NO_PAD.encode(&p[1..33]),
        URL_SAFE_NO_PAD.encode(&p[33..65])
    )
}

fn id_token(key: &EcdsaKeyPair, kid: &str, claims: &str) -> String {
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(format!(r#"{{"alg":"ES256","kid":"{kid}"}}"#)),
        URL_SAFE_NO_PAD.encode(claims)
    );
    let sig = key.sign(&SystemRandom::new(), signed.as_bytes()).unwrap();
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn form(text: &str) -> HashMap<String, String> {
    form_urlencoded::parse(text.as_bytes()).into_owned().collect()
}

/// A request: method, target, headers (lowercase names) and body.
type Received = (String, String, Vec<(String, String)>, String);

/// Reads a request.
fn read_request(s: &mut TcpStream) -> Option<Received> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            let head = &text[..end];
            let mut lines = head.lines();
            let first: Vec<&str> = lines.next()?.split(' ').collect();
            let headers: Vec<(String, String)> = lines
                .filter_map(|l| l.split_once(':').map(|(a, b)| (a.trim().to_lowercase(), b.trim().to_string())))
                .collect();
            let len: usize =
                headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
            let mut body = buf[end + 4..].to_vec();
            while body.len() < len {
                let n = s.read(&mut chunk).ok()?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
            return Some((first[0].to_string(), first[1].to_string(), headers, String::from_utf8_lossy(&body).into()));
        }
    }
}

fn reply(s: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn provider(state: Arc<Mutex<Provider>>, listener: TcpListener) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let Some((method, target, headers, body)) = read_request(&mut s) else { continue };
            let mut p = state.lock().unwrap();
            let issuer = p.issuer.clone();
            match (method.as_str(), target.as_str()) {
                ("GET", "/.well-known/openid-configuration") => {
                    p.discovery_requests += 1;
                    let iss =
                        if p.iss_supported { r#","authorization_response_iss_parameter_supported":true"# } else { "" };
                    let doc = format!(
                        r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/auth","token_endpoint":"{issuer}/token","jwks_uri":"{issuer}/jwks","token_endpoint_auth_methods_supported":["client_secret_basic"]{iss}}}"#
                    );
                    reply(&mut s, "200 OK", &doc);
                }
                ("GET", "/jwks") => {
                    let doc = format!(r#"{{"keys":[{}]}}"#, jwk(&p.key, "k1"));
                    reply(&mut s, "200 OK", &doc);
                }
                ("POST", "/token") => {
                    p.token_requests += 1;
                    let f = form(&body);
                    let auth = headers.iter().find(|(n, _)| n == "authorization").map(|(_, v)| v.clone());
                    let expected = format!("Basic {}", STANDARD.encode("site:s3cret"));
                    if auth.as_deref() != Some(expected.as_str()) {
                        reply(&mut s, "401 Unauthorized", r#"{"error":"invalid_client"}"#);
                        continue;
                    }
                    let code = f.get("code").cloned().unwrap_or_default();
                    // The code is "code-<state>": the provider knows which request it finishes.
                    let Some((challenge, nonce)) = code.strip_prefix("code-").and_then(|st| p.started.get(st)).cloned()
                    else {
                        reply(
                            &mut s,
                            "400 Bad Request",
                            r#"{"error":"invalid_grant","error_description":"unknown code"}"#,
                        );
                        continue;
                    };
                    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
                    if computed != challenge || f.get("grant_type").map(String::as_str) != Some("authorization_code") {
                        reply(
                            &mut s,
                            "400 Bad Request",
                            r#"{"error":"invalid_grant","error_description":"bad verifier"}"#,
                        );
                        continue;
                    }
                    let t = now();
                    let (nonce, aud, exp) = match p.mode {
                        Mode::WrongNonce => ("other".to_string(), "site", t + 300),
                        Mode::WrongAudience => (nonce, "another-site", t + 300),
                        Mode::Expired => (nonce, "site", t - 600),
                        _ => (nonce, "site", t + 300),
                    };
                    let claims = format!(
                        r#"{{"iss":"{issuer}","aud":"{aud}","sub":"user-1","nonce":"{nonce}","exp":{exp},"iat":{t},"email":"ann@example.com","email_verified":true}}"#
                    );
                    let key = if p.mode == Mode::OtherKey { &p.other } else { &p.key };
                    let token = id_token(key, "k1", &claims);
                    let token_type = p.token_type;
                    let doc = format!(
                        r#"{{"access_token":"at-1","token_type":"{token_type}","expires_in":3600,"id_token":"{token}"}}"#
                    );
                    reply(&mut s, "200 OK", &doc);
                }
                _ => reply(&mut s, "404 Not Found", "{}"),
            }
        }
    });
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

const CALLBACK: &str = r#"<?phv
import std.oauth.{OAuth, OAuthError};
try {
    $s = OAuth::provider("idp")->callback();
    echo "ok sub=", $s->subject() ?? "-", " email=", $s->verifiedEmail() ?? "-", " access=", $s->accessToken;
} catch (OAuthError $e) {
    echo "error ", $e->error, ": ", $e->getMessage();
}
"#;

/// A callback after the response was flushed (its session is not saved).
const CALLBACK_AFTER_FLUSH: &str = r#"<?phv
import std.http.response;
import std.oauth.OAuth;
echo "start\n";
response()->flush();
try {
    $s = OAuth::provider("idp")->callback();
    echo "ok sub=", $s->subject() ?? "-";
} catch (Exception $e) {
    echo "refused: ", $e->getMessage();
}
"#;

/// A provider without OpenID Connect: its ID token is not verified.
const CALLBACK_PLAIN: &str = r#"<?phv
import std.oauth.{OAuth, OAuthError};
try {
    $s = OAuth::provider("plain")->callback();
    echo "ok access=", $s->accessToken, " id=", $s->idToken ?? "null", " claims=", count($s->claims);
} catch (OAuthError $e) {
    echo "error ", $e->error, ": ", $e->getMessage();
}
"#;

/// The site and the provider; returns the site's address and the provider's state.
fn start() -> (SocketAddr, Arc<Mutex<Provider>>, PathBuf) {
    start_with(false, false)
}

/// `start()` with a provider that advertises `iss` in every answer, or a
/// site whose sessions are files (`<dir>/sessions/`).
fn start_with(iss_supported: bool, file_sessions: bool) -> (SocketAddr, Arc<Mutex<Provider>>, PathBuf) {
    let idp = TcpListener::bind("127.0.0.1:0").unwrap();
    let issuer = format!("http://{}", idp.local_addr().unwrap());
    let state = Arc::new(Mutex::new(Provider {
        key: new_key(),
        other: new_key(),
        issuer: issuer.clone(),
        mode: Mode::Good,
        started: HashMap::new(),
        token_requests: 0,
        token_type: "Bearer",
        iss_supported,
        discovery_requests: 0,
    }));
    provider(state.clone(), idp);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Each test has its own site (they run at once).
    let dir = std::env::temp_dir().join(format!("phv-oauth-{}-{}", std::process::id(), addr.port()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("PHV_TEST_OAUTH_SECRET", "s3cret");
    let sessions = if file_sessions { "[security]\nsession_store = \"sessions/\"\n" } else { "" };
    write(
        &dir,
        "phv.toml",
        &format!(
            "{sessions}[oauth.idp]\nissuer = \"{issuer}\"\nclient_id = \"site\"\nclient_secret_env = \"PHV_TEST_OAUTH_SECRET\"\n\
             redirect_uri = \"http://{addr}/cb\"\nscopes = [\"openid\", \"email\"]\n\
             [oauth.plain]\nauthorize_url = \"{issuer}/auth\"\ntoken_url = \"{issuer}/token\"\nclient_id = \"site\"\n\
             client_secret_env = \"PHV_TEST_OAUTH_SECRET\"\nredirect_uri = \"http://{addr}/cbp\"\n"
        ),
    );
    write(
        &dir,
        "public/login.phv",
        "<?phv\nimport std.oauth.OAuth;\nOAuth::provider(\"idp\")->redirect([\"prompt\" => \"login\"]);\n",
    );
    write(&dir, "public/cb.phv", CALLBACK);
    write(&dir, "public/cbflush.phv", CALLBACK_AFTER_FLUSH);
    write(&dir, "public/loginp.phv", "<?phv\nimport std.oauth.OAuth;\nOAuth::provider(\"plain\")->redirect();\n");
    write(&dir, "public/cbp.phv", CALLBACK_PLAIN);
    write(
        &dir,
        "public/regen.phv",
        "<?phv\nimport std.http.session;\nsession()->regenerate();\nsession()->set(\"user\", \"u\");\necho \"ok\";\n",
    );
    let opts = ServeOptions {
        root: dir.clone(),
        host: "127.0.0.1".into(),
        port: 0,
        dev: false,
        time_limit: Duration::from_secs(10),
        watch: false,
    };
    std::thread::spawn(move || serve_on(listener, opts).unwrap());
    (addr, state, dir)
}

/// A GET with an optional cookie: (status, headers, body).
fn get(addr: SocketAddr, path: &str, cookie: Option<&str>) -> (u16, Vec<(String, String)>, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let cookie = cookie.map(|c| format!("Cookie: {c}\r\n")).unwrap_or_default();
    write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\n{cookie}Connection: close\r\n\r\n").unwrap();
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("0").parse().unwrap_or(0);
    let headers =
        lines.filter_map(|l| l.split_once(':').map(|(a, b)| (a.trim().to_lowercase(), b.trim().to_string()))).collect();
    (status, headers, body.to_string())
}

/// Starts a sign-in: the session cookie and the authorization request's parameters.
fn login(addr: SocketAddr, p: &Arc<Mutex<Provider>>, cookie: Option<&str>) -> (String, HashMap<String, String>) {
    login_at(addr, p, cookie, "/login")
}

fn login_at(
    addr: SocketAddr,
    p: &Arc<Mutex<Provider>>,
    cookie: Option<&str>,
    page: &str,
) -> (String, HashMap<String, String>) {
    let (status, headers, _) = get(addr, page, cookie);
    assert_eq!(status, 302);
    let location = headers.iter().find(|(n, _)| n == "location").map(|(_, v)| v.clone()).unwrap();
    let issuer = p.lock().unwrap().issuer.clone();
    let query = location.strip_prefix(&format!("{issuer}/auth?")).expect("sent to the provider");
    let params = form(query);
    let session = headers
        .iter()
        .filter(|(n, _)| n == "set-cookie")
        .find_map(|(_, v)| v.split(';').next().filter(|c| c.starts_with("PHVSESSID=")).map(str::to_string))
        .or_else(|| cookie.map(str::to_string))
        .expect("a session cookie");
    let state = params["state"].clone();
    let nonce = params.get("nonce").cloned().unwrap_or_default();
    p.lock().unwrap().started.insert(state, (params["code_challenge"].clone(), nonce));
    (session, params)
}

#[test]
fn openid_connect_sign_in() {
    let (addr, p, dir) = start();

    let (cookie, params) = login(addr, &p, None);
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "site");
    assert_eq!(params["redirect_uri"], format!("http://{addr}/cb"));
    assert_eq!(params["scope"], "openid email");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["prompt"], "login");
    assert!(params["state"].len() >= 43 && params["nonce"].len() >= 43);
    let state = params["state"].clone();
    let cb = format!("/cb?code=code-{state}&state={state}");

    // The sign-in finishes in the session that started it.
    let (_, _, body) = get(addr, &cb, Some(&cookie));
    assert_eq!(body, "ok sub=user-1 email=ann@example.com access=at-1");
    // Once.
    let (_, _, body) = get(addr, &cb, Some(&cookie));
    assert!(body.starts_with("error invalid_state"), "{body}");

    // Not in another browser (login CSRF): the state is not that session's.
    let (_, params) = login(addr, &p, None);
    let state = params["state"].clone();
    let (_, _, body) = get(addr, &format!("/cb?code=code-{state}&state={state}"), None);
    assert!(body.starts_with("error invalid_state"), "{body}");

    // A state made up, and the visitor refusing.
    let (cookie, params) = login(addr, &p, None);
    let (_, _, body) = get(addr, "/cb?code=x&state=made-up", Some(&cookie));
    assert!(body.starts_with("error invalid_state"), "{body}");
    let state = params["state"].clone();
    let (_, _, body) = get(addr, &format!("/cb?error=access_denied&state={state}"), Some(&cookie));
    assert!(body.starts_with("error access_denied"), "{body}");

    // ID tokens that must be refused.
    for (mode, what) in [
        (Mode::WrongNonce, "nonce"),
        (Mode::WrongAudience, "audience"),
        (Mode::OtherKey, "signature"),
        (Mode::Expired, "expiry"),
    ] {
        p.lock().unwrap().mode = mode;
        let (cookie, params) = login(addr, &p, None);
        let state = params["state"].clone();
        let (_, _, body) = get(addr, &format!("/cb?code=code-{state}&state={state}"), Some(&cookie));
        assert!(body.starts_with("error invalid_id_token"), "{what}: {body}");
    }

    // A code the provider does not know: its error is passed on.
    p.lock().unwrap().mode = Mode::Good;
    let (cookie, params) = login(addr, &p, None);
    let state = params["state"].clone();
    let (_, _, body) = get(addr, &format!("/cb?code=wrong&state={state}"), Some(&cookie));
    assert_eq!(body, "error invalid_grant: unknown code");

    // Several sign-ins started in one session (tabs) each finish once.
    let (cookie, first) = login(addr, &p, None);
    let (_, second) = login(addr, &p, Some(&cookie));
    for params in [second, first] {
        let state = params["state"].clone();
        let (_, _, body) = get(addr, &format!("/cb?code=code-{state}&state={state}"), Some(&cookie));
        assert!(body.starts_with("ok sub=user-1"), "{body}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// After `response()->flush()` the session is not saved, so a callback there
/// could not use up its state: it is refused before the code is exchanged.
/// Found by the 16th assessment: the callback could be replayed for 10 minutes.
#[test]
fn callback_after_flush_is_refused() {
    let (addr, p, dir) = start();
    let (cookie, params) = login(addr, &p, None);
    let state = params["state"].clone();
    let query = format!("?code=code-{state}&state={state}");
    for _ in 0..2 {
        let (_, _, body) = get(addr, &format!("/cbflush{query}"), Some(&cookie));
        assert!(body.contains("refused: OAuth::callback() must come before response()"), "{body}");
    }
    assert_eq!(p.lock().unwrap().token_requests, 0, "no code was exchanged");
    // The sign-in is still there for a callback that can finish it, once.
    let (_, _, body) = get(addr, &format!("/cb{query}"), Some(&cookie));
    assert!(body.starts_with("ok sub=user-1"), "{body}");
    let (_, _, body) = get(addr, &format!("/cb{query}"), Some(&cookie));
    assert!(body.starts_with("error invalid_state"), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The token response and the answer on the redirect URI are checked as they
/// should be: the access token is a Bearer token, `iss` comes before the error
/// it names, and without OpenID Connect no unverified ID token is passed on.
#[test]
fn token_responses_and_answers_are_checked() {
    let (addr, p, dir) = start();
    let finish = |page: &str, login_page: &str| {
        let (cookie, params) = login_at(addr, &p, None, login_page);
        let state = params["state"].clone();
        get(addr, &format!("{page}?code=code-{state}&state={state}"), Some(&cookie)).2
    };

    p.lock().unwrap().token_type = "mac";
    let body = finish("/cb", "/login");
    assert!(body.starts_with("error unsupported_token_type"), "{body}");
    p.lock().unwrap().token_type = "bearer";
    let body = finish("/cb", "/login");
    assert!(body.starts_with("ok sub=user-1"), "case does not matter: {body}");

    // Another provider's error answer is refused as such, not shown as this provider's.
    let (cookie, params) = login(addr, &p, None);
    let state = params["state"].clone();
    let other = "iss=https%3A%2F%2Fother.example";
    let (_, _, body) =
        get(addr, &format!("/cb?error=access_denied&error_description=x&state={state}&{other}"), Some(&cookie));
    assert!(body.starts_with("error invalid_issuer"), "{body}");

    // OAuth 2.0 without OpenID Connect: the ID token in the response is not verified, so it is not passed on.
    let body = finish("/cbp", "/loginp");
    assert_eq!(body, "ok access=at-1 id=null claims=0");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A provider whose discovery document says it sends `iss` with every
/// answer (`authorization_response_iss_parameter_supported`) must (RFC 9207
/// 2.4): an answer without it is refused as another provider's, a code and an
/// error alike; one naming the issuer finishes. The flag is kept with the
/// cached document. Found by the 18th assessment: `iss` was checked only when
/// present, so a provider sharing the callback could leave it out.
#[test]
fn answers_without_iss_are_refused_when_the_provider_always_sends_it() {
    let (addr, p, dir) = start_with(true, false);
    let issuer = p.lock().unwrap().issuer.clone();
    let iss = form_urlencoded::byte_serialize(issuer.as_bytes()).collect::<String>();

    let (cookie, params) = login(addr, &p, None);
    let state = params["state"].clone();
    let (_, _, body) = get(addr, &format!("/cb?code=code-{state}&state={state}"), Some(&cookie));
    assert!(body.starts_with("error invalid_issuer"), "{body}");
    assert_eq!(p.lock().unwrap().token_requests, 0, "the code was not exchanged");
    // The answer used the state up all the same.
    let (_, _, body) = get(addr, &format!("/cb?code=code-{state}&state={state}&iss={iss}"), Some(&cookie));
    assert!(body.starts_with("error invalid_state"), "{body}");

    let (cookie, params) = login(addr, &p, None);
    let state = params["state"].clone();
    let (_, _, body) = get(addr, &format!("/cb?error=access_denied&state={state}"), Some(&cookie));
    assert!(body.starts_with("error invalid_issuer"), "an error answer too: {body}");

    let (cookie, params) = login(addr, &p, None);
    let state = params["state"].clone();
    let (_, _, body) = get(addr, &format!("/cb?code=code-{state}&state={state}&iss={iss}"), Some(&cookie));
    assert_eq!(body, "ok sub=user-1 email=ann@example.com access=at-1");
    assert_eq!(p.lock().unwrap().discovery_requests, 1, "the cached document kept the flag");

    // A provider that does not advertise it: an answer without `iss` is its own (as before).
    let (addr2, p2, dir2) = start();
    let (cookie, params) = login(addr2, &p2, None);
    let state = params["state"].clone();
    let (_, _, body) = get(addr2, &format!("/cb?code=code-{state}&state={state}"), Some(&cookie));
    assert!(body.starts_with("ok sub=user-1"), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// The one session file of `<dir>/sessions/`.
fn session_file(dir: &Path) -> PathBuf {
    let files: Vec<PathBuf> = std::fs::read_dir(dir.join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    files[0].clone()
}

/// The `phv.oauth` value of the session file: the pending list's text.
fn pending_text(dir: &Path) -> String {
    let record: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(session_file(dir)).unwrap()).unwrap();
    record["values"]["phv.oauth"].as_str().unwrap_or("[]").to_string()
}

/// Writes `entries` as the session file's pending list, with the record's times as they are.
fn set_pending(dir: &Path, entries: &[String]) {
    let file = session_file(dir);
    let mut record: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    record["values"]["phv.oauth"] = serde_json::Value::String(format!("[{}]", entries.join(",")));
    std::fs::write(&file, record.to_string()).unwrap();
}

/// A session record holding thousands of started sign-ins (a page cannot
/// make one, only a writer of the store) finishes the callback: not found,
/// `invalid_state`; found, the sign-in; and the list is cut back to what a
/// page could have kept. Found by the 18th assessment: the save of the list
/// after the one used was over the session's 64 KB and the callback was a 500.
#[test]
fn an_oversized_pending_list_in_the_store_finishes_the_callback() {
    let (addr, p, dir) = start_with(false, true);
    let (cookie, params) = login(addr, &p, None);
    let state = params["state"].clone();
    let stored = pending_text(&dir);
    let list: Vec<serde_json::Value> = serde_json::from_str(&stored).unwrap();
    assert_eq!(list.len(), 1);
    let t = list[0]["t"].as_i64().unwrap();
    let fakes: Vec<String> =
        (0..5000).map(|i| format!(r#"{{"p":"idp","s":"x{i}","n":"n","v":"v","t":{t}}}"#)).collect();

    // The real sign-in is not among them.
    set_pending(&dir, &fakes);
    assert!(pending_text(&dir).len() > 200_000, "well over the session's 64 KB");
    let (status, _, body) = get(addr, &format!("/cb?code=code-{state}&state={state}"), Some(&cookie));
    assert_eq!(status, 200);
    assert!(body.starts_with("error invalid_state"), "{body}");
    let after = pending_text(&dir);
    assert!(after.matches(r#""p":"#).count() <= 4 && after.len() < 64 * 1024, "cut back: {} bytes", after.len());

    // The real sign-in is the last of them.
    let (_, params) = login(addr, &p, Some(&cookie));
    let state = params["state"].clone();
    let stored = pending_text(&dir);
    let list: Vec<serde_json::Value> = serde_json::from_str(&stored).unwrap();
    let real = list.last().unwrap().to_string();
    assert!(real.contains(&state));
    let mut entries = fakes.clone();
    entries.push(real);
    set_pending(&dir, &entries);
    let (status, _, body) = get(addr, &format!("/cb?code=code-{state}&state={state}"), Some(&cookie));
    assert_eq!(status, 200);
    assert_eq!(body, "ok sub=user-1 email=ann@example.com access=at-1");
    let after = pending_text(&dir);
    assert!(after.matches(r#""p":"#).count() <= 4 && after.len() < 64 * 1024, "cut back: {} bytes", after.len());
    assert!(!after.contains(&state), "the finished sign-in is gone");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A provider without an issuer (OAuth 2.0) refuses an answer whose `iss`
/// names the issuer of another provider of the site: that answer is the other
/// provider's (RFC 9207 mix-up on a shared callback). Found by the 19th
/// assessment (R22-14): the code and the PKCE verifier went to the plain
/// provider's token endpoint.
#[test]
fn an_iss_of_another_provider_is_refused_by_a_plain_provider() {
    let (addr, p, dir) = start();
    let issuer = p.lock().unwrap().issuer.clone();
    for iss in [issuer.clone(), format!("{issuer}/")] {
        let iss = form_urlencoded::byte_serialize(iss.as_bytes()).collect::<String>();
        let (cookie, params) = login_at(addr, &p, None, "/loginp");
        let state = params["state"].clone();
        let (_, _, body) = get(addr, &format!("/cbp?code=code-{state}&state={state}&iss={iss}"), Some(&cookie));
        assert!(body.starts_with("error invalid_issuer"), "{body}");
    }
    assert_eq!(p.lock().unwrap().token_requests, 0, "no code was exchanged");
    // An `iss` that names no provider of the site is not checked (as documented), nor is a missing one.
    let (cookie, params) = login_at(addr, &p, None, "/loginp");
    let state = params["state"].clone();
    let (_, _, body) =
        get(addr, &format!("/cbp?code=code-{state}&state={state}&iss=https%3A%2F%2Fplain.example"), Some(&cookie));
    assert_eq!(body, "ok access=at-1 id=null claims=0");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A used state written back into the session by another request of the
/// same session (a sign-in started in a second tab that read the session
/// before the callback used it) does not finish a second time. Found by the
/// 19th assessment (R22-25): the callback URL worked once more.
#[test]
fn a_state_written_back_by_another_request_is_not_used_twice() {
    let (addr, p, dir) = start_with(false, true);
    let (cookie, params) = login(addr, &p, None);
    let state = params["state"].clone();
    // What the second tab's request read, and later writes back.
    let before = pending_text(&dir);
    assert!(before.contains(&state));
    let cb = format!("/cb?code=code-{state}&state={state}");
    let (_, _, body) = get(addr, &cb, Some(&cookie));
    assert!(body.starts_with("ok sub=user-1"), "{body}");
    assert_eq!(p.lock().unwrap().token_requests, 1);
    let list: Vec<serde_json::Value> = serde_json::from_str(&before).unwrap();
    set_pending(&dir, &list.iter().map(|v| v.to_string()).collect::<Vec<_>>());
    assert!(pending_text(&dir).contains(&state), "the used state is back in the store");
    let (_, _, body) = get(addr, &cb, Some(&cookie));
    assert!(body.starts_with("error invalid_state"), "{body}");
    assert_eq!(p.lock().unwrap().token_requests, 1, "the code was not exchanged again");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A used state written back under a new session id, by a page that read the
/// session before the callback and then called `session()->regenerate()`,
/// does not finish a second time: the claim names the state, not the
/// session. Found by the 20th assessment (R23-9): the callback URL worked
/// again with the new cookie, and the code was exchanged twice.
#[test]
fn a_used_state_moved_to_a_new_id_by_regenerate_is_not_used_twice() {
    let (addr, p, dir) = start_with(false, true);
    let (cookie, params) = login(addr, &p, None);
    let state = params["state"].clone();
    // What the other page read.
    let before = pending_text(&dir);
    let cb = format!("/cb?code=code-{state}&state={state}");
    let (_, _, body) = get(addr, &cb, Some(&cookie));
    assert!(body.starts_with("ok sub=user-1"), "{body}");
    // The page regenerates the session and writes back the list it read.
    let (_, headers, _) = get(addr, "/regen", Some(&cookie));
    let moved = headers
        .iter()
        .filter(|(n, _)| n == "set-cookie")
        .find_map(|(_, v)| v.split(';').next().filter(|c| c.starts_with("PHVSESSID=") && c.len() > 20))
        .expect("a new session cookie")
        .to_string();
    assert_ne!(moved, cookie);
    let list: Vec<serde_json::Value> = serde_json::from_str(&before).unwrap();
    set_pending(&dir, &list.iter().map(|v| v.to_string()).collect::<Vec<_>>());
    assert!(pending_text(&dir).contains(&state), "the used state is back, under the new id");
    let (_, _, body) = get(addr, &cb, Some(&moved));
    assert!(body.starts_with("error invalid_state"), "{body}");
    assert_eq!(p.lock().unwrap().token_requests, 1, "the code was not exchanged again");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A callback that carries neither a code nor an error uses up the state in
/// the session but claims nothing in the store (a claim costs the store; a
/// page can make none without a code).
#[test]
fn a_callback_without_a_code_or_an_error_claims_nothing() {
    let (addr, p, dir) = start_with(false, true);
    let (cookie, params) = login(addr, &p, None);
    let state = params["state"].clone();
    let (_, _, body) = get(addr, &format!("/cb?state={state}"), Some(&cookie));
    assert!(body.starts_with("error invalid_request"), "{body}");
    let claims = dir.join("sessions").join(".phv-oauth");
    let made = std::fs::read_dir(&claims).map_or(0, |d| d.count());
    assert_eq!(made, 0, "a claim was made");
    // The state is used up in the session.
    let (_, _, body) = get(addr, &format!("/cb?code=code-{state}&state={state}"), Some(&cookie));
    assert!(body.starts_with("error invalid_state"), "{body}");
    assert_eq!(p.lock().unwrap().token_requests, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
