//! Sign-in with OAuth 2.0 and OpenID Connect (`std.oauth`, `[oauth.<name>]`
//! in phv.toml): the authorization code flow, for a site's visitors.
//!
//! - `__oauth_begin` makes the URL that sends the visitor to the provider. It
//!   keeps a random `state`, an OpenID Connect `nonce` and a PKCE verifier
//!   (S256) in the visitor's session, for 10 minutes and one use.
//! - `__oauth_finish` runs on the redirect URI: the `state` must be one this
//!   session started for this provider (so another site cannot make a
//!   visitor sign in to the attacker's account), the code is exchanged at the
//!   token endpoint with the verifier and the client secret, and with OpenID
//!   Connect the ID token must be signed by a key of the provider's JWKS
//!   (RS256, ES256 or EdDSA), name the issuer, this client as its audience,
//!   and the nonce, and be valid now.
//!
//! The endpoints of an OpenID Connect provider come from its discovery
//! document (`<issuer>/.well-known/openid-configuration`), whose `issuer` must
//! be the configured one; it is kept for an hour. Endpoints must use TLS,
//! except on this machine or with `tls = false`; those of a discovery
//! document use TLS when the issuer does, and are on this machine only when
//! the issuer is.
//!
//! Failures the provider or the visitor cause come back as
//! `["error" => code, "message" => text]` (std.oauth throws `OAuthError`);
//! network failures raise `NetworkError`, and mistakes in phv.toml or in the
//! script `ValueError`.

use crate::jwt::{self, Expect, Source};
use crate::{map, s};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use indexmap::IndexMap;
use phv_vm::{Key, OAuthSpec, Value, Vm, VmError};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

type R = Result<Value, VmError>;

/// The session value that holds the sign-ins started and not yet finished.
const PENDING_KEY: &str = "phv.oauth";
/// Sign-ins a session may have started at once (several tabs), and for how long.
const PENDING_MAX: usize = 4;
const PENDING_TTL: i64 = 600;
/// Largest discovery document and token response.
const MAX_DOC: usize = 256 << 10;
/// Seconds of clock difference allowed with the provider.
const LEEWAY: i64 = 60;
/// Parameters of the authorization request that PHV sets.
const RESERVED: &[&str] = &[
    "response_type",
    "client_id",
    "redirect_uri",
    "scope",
    "state",
    "nonce",
    "code_challenge",
    "code_challenge_method",
    "response_mode",
    "request",
    "request_uri",
];

fn str_key(k: &str) -> Key {
    Key::Str(Rc::new(k.to_string()))
}

fn random_b64(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("operating system random number generator failed");
    URL_SAFE_NO_PAD.encode(b)
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
}

/// Text from the provider made safe to show: printable ASCII, at most `max` bytes.
fn clean(text: &str, max: usize) -> String {
    text.chars().filter(|c| (' '..='~').contains(c)).take(max).collect()
}

fn error_map(code: &str, message: &str) -> Value {
    let mut m = IndexMap::new();
    m.insert(str_key("error"), Value::str(code));
    m.insert(str_key("message"), Value::str(message));
    Value::map(m)
}

fn spec(vm: &Vm, name: &str) -> Result<OAuthSpec, VmError> {
    vm.host
        .oauth
        .get(name)
        .cloned()
        .ok_or_else(|| vm.throw("ValueError", format!("no sign-in provider named \"{name}\" in [oauth] of phv.toml")))
}

/// A value of the spec that may come from an unset environment variable.
fn needed(vm: &Vm, name: &str, value: &Option<String>, env: &Option<String>, what: &str) -> Result<String, VmError> {
    value.clone().ok_or_else(|| {
        let var = env.as_deref().unwrap_or("?");
        vm.throw("ValueError", format!("[oauth.{name}]: the environment variable {var} ({what}) is not set"))
    })
}

// --- Discovery ---------------------------------------------------------------

/// What PHV uses of a discovery document.
struct Discovery {
    authorize: String,
    token: String,
    jwks: String,
    /// The token endpoint accepts the secret in the body only (not Basic).
    post_only: bool,
    /// The provider says it sends `iss` with every authorization response
    /// (`authorization_response_iss_parameter_supported`, RFC 9207), so one
    /// without it is not its own.
    iss_always: bool,
}

/// Discovery documents by issuer: fetched again after an hour; one that cannot
/// be fetched again is used for a day, and tried again every 30 seconds.
/// A document, when it was fetched, and when it was last tried.
type Kept = (Arc<Discovery>, Instant, Instant);
static DISCOVERY: LazyLock<Mutex<HashMap<String, Kept>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const DISCOVERY_AGE: Duration = Duration::from_secs(3600);
const DISCOVERY_RETRY: Duration = Duration::from_secs(30);
const DISCOVERY_STALE: Duration = Duration::from_secs(86_400);
/// Issuers whose document could not be read and is not kept (see `jwt::Failures`).
static DISCOVERY_FAILED: jwt::Failures = jwt::Failures::new();

fn doc_str(m: &phv_vm::Map, k: &str) -> Option<String> {
    match m.get(&str_key(k)) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// A discovery document, decoded within the script's limits when there is a
/// `vm` (the outer error); the inner error says why it cannot be used.
fn parse_discovery(
    vm: Option<&Vm>,
    issuer: &str,
    tls: Option<bool>,
    body: &[u8],
) -> Result<Result<Discovery, String>, VmError> {
    let Ok(text) = std::str::from_utf8(body) else { return Ok(Err("the discovery document is not UTF-8".into())) };
    let doc = match vm {
        Some(vm) => crate::web::json_decode_in(vm, text)?,
        None => crate::web::json_decode(text),
    };
    Ok(discovery_of(doc, issuer, tls))
}

fn discovery_of(doc: Result<Value, String>, issuer: &str, tls: Option<bool>) -> Result<Discovery, String> {
    let doc = doc.map_err(|e| format!("the discovery document is not JSON: {e}"))?;
    let Value::Map(m) = &doc else { return Err("the discovery document is not a JSON object".into()) };
    // OpenID Connect Discovery 1.0, 4.3: the issuer must be exactly the one asked for.
    if doc_str(m, "issuer").as_deref() != Some(issuer) {
        return Err("the discovery document names another issuer".into());
    }
    let endpoint = |k: &str| -> Result<String, String> {
        let u = doc_str(m, k).ok_or_else(|| format!("the discovery document has no {k}"))?;
        crate::httpclient::check_discovered_url(issuer, &u, tls).map_err(|e| format!("{k}: {e}"))?;
        if u.contains('#') {
            return Err(format!("{k} has a fragment"));
        }
        Ok(u)
    };
    let methods = match m.get(&str_key("token_endpoint_auth_methods_supported")) {
        Some(Value::List(l)) => {
            l.iter().filter_map(|v| if let Value::Str(s) = v { Some(s.to_string()) } else { None }).collect()
        }
        _ => Vec::new(),
    };
    let basic = methods.is_empty() || methods.iter().any(|m| m == "client_secret_basic");
    let post = methods.iter().any(|m| m == "client_secret_post");
    // RFC 9207 3: a boolean; left out, it is false.
    let iss_always =
        matches!(m.get(&str_key("authorization_response_iss_parameter_supported")), Some(Value::Bool(true)));
    Ok(Discovery {
        authorize: endpoint("authorization_endpoint")?,
        token: endpoint("token_endpoint")?,
        jwks: endpoint("jwks_uri")?,
        post_only: !basic && post,
        iss_always,
    })
}

fn discovery(vm: &Vm, name: &str, issuer: &str, tls: Option<bool>) -> Result<Arc<Discovery>, VmError> {
    {
        let cache = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((d, fetched, tried)) = cache.get(issuer) {
            let fresh = fetched.elapsed() < DISCOVERY_AGE;
            let failing = tried.elapsed() < DISCOVERY_RETRY && fetched.elapsed() < DISCOVERY_STALE;
            if fresh || failing {
                return Ok(d.clone());
            }
        }
    }
    // A provider that could not be read a moment ago is not asked again yet.
    if let Some(e) = DISCOVERY_FAILED.recent(issuer) {
        let secs = jwt::RETRY_FAILED.as_secs();
        let msg = format!("sign-in provider \"{name}\": {e} (it is tried again at most every {secs} seconds)");
        return Err(vm.throw("NetworkError", msg));
    }
    let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
    let accept = vec![("Accept".to_string(), "application/json".to_string())];
    let result = crate::httpclient::send_url(vm, "GET", &url, tls, accept, Vec::new(), MAX_DOC);
    vm.check_limits()?;
    let result = match result {
        Ok(r) if r.status() != 200 => Err(format!("the discovery document answered {}", r.status())),
        Ok(r) => parse_discovery(Some(vm), issuer, tls, r.body())?,
        Err(e) => Err(e),
    };
    let mut cache = DISCOVERY.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    match result {
        Ok(d) => {
            let d = Arc::new(d);
            if cache.len() >= 64 {
                cache.clear();
            }
            cache.insert(issuer.to_string(), (d.clone(), now, now));
            DISCOVERY_FAILED.clear(issuer);
            Ok(d)
        }
        Err(e) => match cache.get_mut(issuer) {
            Some((d, fetched, tried)) if fetched.elapsed() < DISCOVERY_STALE => {
                *tried = now;
                Ok(d.clone())
            }
            _ => {
                drop(cache);
                DISCOVERY_FAILED.note(issuer, &e);
                Err(vm.throw("NetworkError", format!("sign-in provider \"{name}\": {e}")))
            }
        },
    }
}

/// The endpoints of a provider: authorization, token, and the JWKS of OpenID
/// Connect; whether the secret goes in the body; and whether every
/// authorization response of the provider carries `iss` (discovery only).
struct Endpoints {
    authorize: String,
    token: String,
    jwks: Option<String>,
    secret_in_body: bool,
    iss_always: bool,
}

fn endpoints(vm: &Vm, name: &str, spec: &OAuthSpec) -> Result<Endpoints, VmError> {
    match &spec.issuer {
        Some(issuer) => {
            let d = discovery(vm, name, issuer, spec.tls)?;
            Ok(Endpoints {
                authorize: d.authorize.clone(),
                token: d.token.clone(),
                jwks: Some(d.jwks.clone()),
                secret_in_body: spec.secret_in_body.unwrap_or(d.post_only),
                iss_always: d.iss_always,
            })
        }
        None => Ok(Endpoints {
            authorize: spec.authorize_url.clone().unwrap_or_default(),
            token: spec.token_url.clone().unwrap_or_default(),
            jwks: None,
            secret_in_body: spec.secret_in_body.unwrap_or(false),
            iss_always: false,
        }),
    }
}

// --- Sign-ins started, in the session -----------------------------------------

struct Pending {
    provider: String,
    state: String,
    nonce: String,
    verifier: String,
    started: i64,
}

fn now() -> i64 {
    jwt::now() as i64
}

/// The sign-ins started in this session and not yet expired, oldest first.
/// The value is decoded within the script's limits (the record comes from
/// the store, which a page did not necessarily write); text that is not a
/// list of entries is treated as no sign-ins.
fn pending(vm: &Vm) -> Result<Vec<Pending>, VmError> {
    let Some(text) = vm.http.session.values.get(PENDING_KEY) else { return Ok(Vec::new()) };
    let Ok(doc) = crate::web::json_decode_in(vm, text)? else { return Ok(Vec::new()) };
    let Value::List(items) = &doc else { return Ok(Vec::new()) };
    let now = now();
    let list = items
        .iter()
        .filter_map(|v| {
            let Value::Map(m) = v else { return None };
            let started = match m.get(&str_key("t")) {
                Some(Value::Int(t)) => *t,
                _ => return None,
            };
            Some(Pending {
                provider: doc_str(m, "p")?,
                state: doc_str(m, "s")?,
                nonce: doc_str(m, "n")?,
                verifier: doc_str(m, "v")?,
                started,
            })
        })
        // Saturating: the session value is only text, and an odd one must not stop the script.
        .filter(|p| (0..PENDING_TTL).contains(&now.saturating_sub(p.started)))
        .collect();
    Ok(list)
}

fn encode_pending(list: &[Pending]) -> String {
    let mut out = String::from("[");
    for (i, p) in list.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"p\":");
        crate::json_string(&p.provider, &mut out);
        out.push_str(&format!(
            ",\"s\":\"{}\",\"n\":\"{}\",\"v\":\"{}\",\"t\":{}}}",
            p.state, p.nonce, p.verifier, p.started
        ));
    }
    out.push(']');
    out
}

/// Keeps `list` (oldest first) in the session: the newest `PENDING_MAX`
/// entries that fit in the session's 64 KB beside its other values; the
/// oldest are dropped first. Returns how many were kept. A page never makes
/// a list over the caps, but a record written to the store by other means
/// can hold one, and it must not stop the callback.
fn save_pending(vm: &mut Vm, list: &[Pending]) -> Result<usize, VmError> {
    crate::web::unsent(vm, "the session")?;
    let other: usize =
        vm.http.session.values.iter().filter(|(k, _)| k.as_str() != PENDING_KEY).map(|(k, v)| k.len() + v.len()).sum();
    let room = crate::web::MAX_SESSION_BYTES.saturating_sub(other + PENDING_KEY.len());
    let mut skip = list.len().saturating_sub(PENDING_MAX);
    let mut out = encode_pending(&list[skip..]);
    while out.len() > room && skip < list.len() {
        skip += 1;
        out = encode_pending(&list[skip..]);
    }
    let kept = list.len() - skip;
    if kept == 0 {
        if vm.http.session.values.shift_remove(PENDING_KEY).is_some() {
            vm.http.session.changed = true;
        }
        return Ok(0);
    }
    crate::web::session_set(vm, PENDING_KEY, &out)?;
    Ok(kept)
}

// --- The flow ---------------------------------------------------------------------

fn in_page(vm: &Vm, what: &str) -> Result<(), VmError> {
    if vm.host.cli || vm.http.job.is_some() {
        return Err(vm.throw("ValueError", format!("OAuth::{what}() works only in a page")));
    }
    // The sign-in is kept in the session, which is not saved once the response
    // is sent: a callback after flush() could not use up its state.
    if vm.http.flushed {
        return Err(vm.throw(
            "ValueError",
            format!("OAuth::{what}() must come before response()->flush(): the sign-in is kept in the session"),
        ));
    }
    Ok(())
}

/// `__oauth_begin(name, params)`: the provider's URL for a new sign-in.
fn begin(vm: &mut Vm, a: &[Value]) -> R {
    in_page(vm, "redirect")?;
    let name = s(&a[0]).to_string();
    let spec = spec(vm, &name)?;
    let client_id = needed(vm, &name, &spec.client_id, &spec.client_id_env, "client_id")?;
    let redirect = needed(vm, &name, &spec.redirect_uri, &spec.redirect_uri_env, "redirect_uri")?;
    let mut extra: Vec<(String, String)> = Vec::new();
    for (k, v) in map(&a[1]).iter() {
        let (Key::Str(k), Value::Str(v)) = (k, v) else { continue };
        if RESERVED.contains(&k.as_str()) {
            return Err(vm.throw("ValueError", format!("the parameter \"{}\" is set by PHV", k.escape_debug())));
        }
        extra.push((k.to_string(), v.to_string()));
    }
    let ep = endpoints(vm, &name, &spec)?;
    let p = Pending {
        provider: name.clone(),
        state: random_b64(32),
        nonce: random_b64(32),
        verifier: random_b64(32),
        started: now(),
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
    let scope = spec.scopes.join(" ");
    let mut pairs: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", &redirect),
        ("state", &p.state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ];
    if !scope.is_empty() {
        pairs.push(("scope", &scope));
    }
    if spec.issuer.is_some() {
        pairs.push(("nonce", &p.nonce));
    }
    for (k, v) in &extra {
        pairs.push((k, v));
    }
    let query = form_encode(&pairs);
    let sep = if ep.authorize.contains('?') { '&' } else { '?' };
    let url = format!("{}{sep}{query}", ep.authorize);
    // The newest sign-ins are kept; the oldest is dropped past the limit.
    let mut list = pending(vm)?;
    list.push(p);
    if save_pending(vm, &list)? == 0 {
        // Not even the new one fits: the script filled the session (a mistake it should hear of).
        let msg = "the sign-in cannot be kept in the session: its other values fill the 64 KB";
        return Err(vm.throw("ValueError", msg));
    }
    Ok(Value::str(url))
}

/// Whether `iss` is the issuer of a provider in `[oauth]` (with or without
/// a trailing `/`).
fn names_another_issuer(vm: &Vm, iss: &str) -> bool {
    let bare = |s: &str| s.trim_end_matches('/').to_string();
    vm.host.oauth.values().filter_map(|o| o.issuer.as_deref()).any(|issuer| bare(issuer) == bare(iss))
}

fn query(vm: &Vm, k: &str) -> Option<String> {
    vm.http.query.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
}

/// `__oauth_finish(name)`: the tokens of the sign-in this request finishes,
/// or an error map.
fn finish(vm: &mut Vm, a: &[Value]) -> R {
    in_page(vm, "callback")?;
    let name = s(&a[0]).to_string();
    let spec = spec(vm, &name)?;
    let client_id = needed(vm, &name, &spec.client_id, &spec.client_id_env, "client_id")?;
    let redirect = needed(vm, &name, &spec.redirect_uri, &spec.redirect_uri_env, "redirect_uri")?;
    let Some(state) = query(vm, "state") else {
        return Ok(error_map("invalid_request", "the provider sent no state"));
    };
    // The sign-in must have been started by this session for this provider; it is used once.
    let mut list = pending(vm)?;
    let found =
        list.iter().position(|p| p.provider == name && crate::constant_time_eq(p.state.as_bytes(), state.as_bytes()));
    let Some(i) = found else {
        // A list longer than a page makes (a record written by other means) is cut back.
        if list.len() > PENDING_MAX {
            save_pending(vm, &list)?;
        }
        return Ok(error_map(
            "invalid_state",
            "this sign-in was not started in this browser session, was already finished, or took more than 10 minutes",
        ));
    };
    let p = list.remove(i);
    save_pending(vm, &list)?;
    // Whole-session saves can restore a stale pending list. Claims are kept
    // separately and shared by all pages that use the same session store.
    let claimed = vm.http.session.id.as_deref().zip(vm.http.oauth_states.as_deref()).is_some_and(|(id, store)| {
        let site = vm.host.store_namespace.as_deref().unwrap_or("");
        let digest = crate::sha256_hex(format!("phv-oauth:{site}:{id}:{name}:{}", p.state).as_bytes());
        store.claim(id, &digest, now(), p.started.saturating_add(PENDING_TTL))
    });
    if !claimed {
        return Ok(error_map(
            "invalid_state",
            "this sign-in was not started in this browser session, was already finished, or took more than 10 minutes",
        ));
    }
    vm.check_limits()?;
    let ep = endpoints(vm, &name, &spec)?;
    // RFC 9207: an `iss` in the answer names the provider that sent it, an
    // error answer too (so another provider's error is not shown as this
    // one's); and a provider whose discovery document says it always sends
    // `iss` must (2.4): an answer without it may be another provider's.
    // A provider without an issuer (OAuth 2.0) has no `iss` of its own, so an
    // answer naming the issuer of another provider of this site is that
    // provider's (a mix-up on a shared callback page, R22-14).
    match (query(vm, "iss"), &spec.issuer) {
        (Some(iss), Some(issuer)) if &iss != issuer => {
            return Ok(error_map("invalid_issuer", "the answer came from another provider"));
        }
        (Some(iss), None) if names_another_issuer(vm, &iss) => {
            return Ok(error_map("invalid_issuer", "the answer came from another provider"));
        }
        (None, Some(_)) if ep.iss_always => {
            let msg = "the answer does not name the provider (iss), which this provider always does";
            return Ok(error_map("invalid_issuer", msg));
        }
        _ => {}
    }
    if let Some(code) = query(vm, "error") {
        let desc = query(vm, "error_description").map(|d| clean(&d, 256)).unwrap_or_default();
        let code = clean(&code, 64);
        let msg = if desc.is_empty() { format!("the provider refused the sign-in: {code}") } else { desc };
        return Ok(error_map(&code, &msg));
    }
    let Some(code) = query(vm, "code") else {
        return Ok(error_map("invalid_request", "the provider sent no code"));
    };

    // The code, exchanged for tokens.
    let mut fields: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", &redirect),
        ("code_verifier", &p.verifier),
    ];
    let mut headers = vec![
        ("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    match &spec.client_secret {
        Some(secret) if !ep.secret_in_body => {
            // RFC 6749 2.3.1: both are form-encoded before Basic encoding.
            let enc = |v: &str| form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
            let basic = STANDARD.encode(format!("{}:{}", enc(&client_id), enc(secret)));
            headers.push(("Authorization".to_string(), format!("Basic {basic}")));
        }
        Some(secret) => {
            fields.push(("client_id", &client_id));
            fields.push(("client_secret", secret));
        }
        None if spec.client_secret_env.is_some() => {
            let var = spec.client_secret_env.as_deref().unwrap_or("?");
            return Err(vm.throw(
                "ValueError",
                format!("[oauth.{name}]: the environment variable {var} (client secret) is not set"),
            ));
        }
        None => fields.push(("client_id", &client_id)),
    }
    let body = form_encode(&fields).into_bytes();
    let resp = crate::httpclient::send_url(vm, "POST", &ep.token, spec.tls, headers, body, MAX_DOC);
    vm.check_limits()?;
    let resp =
        resp.map_err(|e| vm.throw("NetworkError", format!("sign-in provider \"{name}\": token endpoint: {e}")))?;
    let text = String::from_utf8_lossy(resp.body()).into_owned();
    let doc = crate::web::json_decode_in(vm, &text)?;
    let doc = match doc {
        Ok(v @ Value::Map(_)) => v,
        _ => {
            let msg = format!("the token endpoint answered {} without JSON", resp.status());
            return Ok(error_map("invalid_token_response", &msg));
        }
    };
    let Value::Map(m) = &doc else { unreachable!() };
    if resp.status() != 200 || m.contains_key(&str_key("error")) {
        let code = doc_str(m, "error").map(|c| clean(&c, 64)).unwrap_or_else(|| "token_error".into());
        let desc = doc_str(m, "error_description").map(|d| clean(&d, 256)).unwrap_or_default();
        let msg = if desc.is_empty() { format!("the token endpoint refused the code: {code}") } else { desc };
        return Ok(error_map(&code, &msg));
    }
    let Some(access) = doc_str(m, "access_token").filter(|t| !t.is_empty()) else {
        return Ok(error_map("invalid_token_response", "the token endpoint sent no access token"));
    };
    // RFC 6749 7.1: a token of another type (such as one bound to a key) is
    // not used the way SignIn's accessToken says. Some providers leave
    // token_type out; it is then taken to be Bearer.
    match m.get(&str_key("token_type")) {
        None => {}
        Some(Value::Str(t)) if t.eq_ignore_ascii_case("bearer") => {}
        Some(_) => {
            let msg = "the token endpoint sent an access token that is not a Bearer token";
            return Ok(error_map("unsupported_token_type", msg));
        }
    }
    let expires = match m.get(&str_key("expires_in")) {
        Some(Value::Int(i)) => Value::Int(*i),
        Some(Value::Str(s)) => s.parse::<i64>().map(Value::Int).unwrap_or(Value::Null),
        Some(Value::Float(f)) if f.is_finite() => Value::Int(*f as i64),
        _ => Value::Null,
    };
    let opt = |k: &str| doc_str(m, k).map(Value::str).unwrap_or(Value::Null);
    let mut out = IndexMap::new();
    out.insert(str_key("access_token"), Value::str(access));
    out.insert(str_key("token_type"), opt("token_type"));
    out.insert(str_key("expires_in"), expires);
    out.insert(str_key("refresh_token"), opt("refresh_token"));
    out.insert(str_key("scope"), opt("scope"));

    // OpenID Connect: the ID token says who signed in.
    let claims = match (&spec.issuer, &ep.jwks) {
        (Some(issuer), Some(jwks)) => {
            let Some(id_token) = doc_str(m, "id_token") else {
                return Ok(error_map("invalid_id_token", "the provider sent no ID token"));
            };
            let source = Source::Url { url: jwks.clone(), tls: spec.tls };
            let keys = jwt::remote_keys(vm, &source, false)?;
            let expect = Expect { issuer: Some(issuer), audience: Some(&client_id), leeway: LEEWAY };
            let Some(claims) = jwt::verify_with(vm, &keys, Some(&source), &id_token, &expect)? else {
                return Ok(error_map("invalid_id_token", "the ID token's signature or claims are not valid"));
            };
            if let Err(why) = id_token_ok(&claims, &client_id, &p.nonce) {
                return Ok(error_map("invalid_id_token", why));
            }
            out.insert(str_key("id_token"), Value::str(id_token));
            claims
        }
        // Without OpenID Connect an ID token is not verified, so it is not passed on.
        _ => {
            out.insert(str_key("id_token"), Value::Null);
            Value::map(IndexMap::new())
        }
    };
    out.insert(str_key("claims"), claims);
    Ok(Value::map(out))
}

/// The checks of OpenID Connect Core 3.1.3.7 that `jwt::verify_with` does
/// not make: `sub`, `exp` and `iat` are present, the nonce is this sign-in's,
/// and a token for several audiences was issued to this client (`azp`).
fn id_token_ok(claims: &Value, client_id: &str, nonce: &str) -> Result<(), &'static str> {
    let Value::Map(m) = claims else { return Err("the ID token has no claims") };
    if !matches!(m.get(&str_key("sub")), Some(Value::Str(s)) if !s.is_empty()) {
        return Err("the ID token names no subject");
    }
    for k in ["exp", "iat"] {
        if !matches!(m.get(&str_key(k)), Some(Value::Int(_) | Value::Float(_))) {
            return Err("the ID token lacks exp or iat");
        }
    }
    match m.get(&str_key("nonce")) {
        Some(Value::Str(n)) if crate::constant_time_eq(n.as_bytes(), nonce.as_bytes()) => {}
        _ => return Err("the ID token's nonce is not this sign-in's"),
    }
    let several = matches!(m.get(&str_key("aud")), Some(Value::List(l)) if l.len() > 1);
    match m.get(&str_key("azp")) {
        Some(Value::Str(azp)) if azp.as_str() == client_id => {}
        Some(_) => return Err("the ID token was issued to another client (azp)"),
        None if several => return Err("the ID token is for several audiences and names no azp"),
        None => {}
    }
    Ok(())
}

pub(crate) fn lookup(name: &str) -> Option<phv_vm::BuiltinFn> {
    let f: phv_vm::BuiltinFn = match name {
        "__oauth_begin" => begin,
        "__oauth_finish" => finish,
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(pairs: &[(&str, Value)]) -> Value {
        let mut m = IndexMap::new();
        for (k, v) in pairs {
            m.insert(str_key(k), v.clone());
        }
        Value::map(m)
    }

    #[test]
    fn id_tokens_need_their_nonce_and_client() {
        let base = |extra: &[(&str, Value)]| {
            let mut p = vec![
                ("sub", Value::str("1")),
                ("exp", Value::Int(2)),
                ("iat", Value::Int(1)),
                ("nonce", Value::str("n")),
            ];
            p.extend(extra.iter().cloned());
            claims(&p)
        };
        assert!(id_token_ok(&base(&[]), "c", "n").is_ok());
        assert!(id_token_ok(&base(&[]), "c", "other").is_err());
        assert!(id_token_ok(&base(&[("azp", Value::str("x"))]), "c", "n").is_err());
        let two = Value::list(vec![Value::str("c"), Value::str("d")]);
        assert!(id_token_ok(&base(&[("aud", two.clone())]), "c", "n").is_err());
        assert!(id_token_ok(&base(&[("aud", two), ("azp", Value::str("c"))]), "c", "n").is_ok());
        let no_sub = claims(&[("exp", Value::Int(2)), ("iat", Value::Int(1)), ("nonce", Value::str("n"))]);
        assert!(id_token_ok(&no_sub, "c", "n").is_err());
    }

    #[test]
    fn discovery_must_name_the_issuer() {
        let doc = |iss: &str| {
            format!(
                r#"{{"issuer":"{iss}","authorization_endpoint":"https://id.example/auth","token_endpoint":"https://id.example/token","jwks_uri":"https://id.example/jwks"}}"#
            )
        };
        let parse = |issuer: &str, doc: &str| {
            parse_discovery(None, issuer, None, doc.as_bytes()).unwrap_or_else(|_| panic!("no limits"))
        };
        assert!(parse("https://id.example", &doc("https://id.example")).is_ok());
        assert!(parse("https://id.example", &doc("https://evil.example")).is_err());
        assert!(parse("https://id.example", &doc("https://id.example/")).is_err());
        let plain = r#"{"issuer":"https://id.example","authorization_endpoint":"http://id.example/auth","token_endpoint":"https://id.example/token","jwks_uri":"https://id.example/jwks"}"#;
        assert!(parse("https://id.example", plain).is_err(), "http endpoint");
    }

    /// A remote provider cannot name an endpoint on this machine, where TLS is
    /// not needed (found by the 16th assessment: the code and the client secret
    /// could go in plain text to a local port of the provider's choosing).
    #[test]
    fn discovered_endpoints_stay_with_the_issuer() {
        let doc = |iss: &str, token: &str| {
            format!(
                r#"{{"issuer":"{iss}","authorization_endpoint":"{iss}/auth","token_endpoint":"{token}","jwks_uri":"{iss}/jwks"}}"#
            )
        };
        let parse = |issuer: &str, doc: &str, tls: Option<bool>| {
            parse_discovery(None, issuer, tls, doc.as_bytes()).unwrap_or_else(|_| panic!("no limits"))
        };
        let remote = "https://id.example";
        assert!(parse(remote, &doc(remote, "https://token.id.example/t"), None).is_ok(), "another remote host");
        for local in ["http://127.0.0.1:9/token", "http://localhost/token", "http://[::1]:9/t", "https://127.0.0.1/t"] {
            assert!(parse(remote, &doc(remote, local), None).is_err(), "{local}");
        }
        // With tls = false the issuer may be plain, but still not send the secret here.
        let plain = "http://id.example";
        assert!(parse(plain, &doc(plain, "http://id.example/token"), Some(false)).is_ok());
        assert!(parse(plain, &doc(plain, "http://127.0.0.1:9/token"), Some(false)).is_err());
        // A provider on this machine (tests, development) keeps its endpoints here.
        let local = "http://127.0.0.1:8080";
        assert!(parse(local, &doc(local, "http://127.0.0.1:8081/token"), None).is_ok());
    }

    /// `authorization_response_iss_parameter_supported` is kept (RFC 9207 3:
    /// a boolean, false when left out); `finish()` then requires `iss`.
    #[test]
    fn discovery_keeps_whether_iss_is_always_sent() {
        let doc = |flag: &str| {
            format!(
                r#"{{"issuer":"https://id.example","authorization_endpoint":"https://id.example/auth","token_endpoint":"https://id.example/token","jwks_uri":"https://id.example/jwks"{flag}}}"#
            )
        };
        let parse = |doc: &str| {
            parse_discovery(None, "https://id.example", None, doc.as_bytes()).unwrap_or_else(|_| panic!("no limits"))
        };
        assert!(parse(&doc(r#","authorization_response_iss_parameter_supported":true"#)).unwrap().iss_always);
        for flag in [
            "",
            r#","authorization_response_iss_parameter_supported":false"#,
            r#","authorization_response_iss_parameter_supported":"true""#,
        ] {
            assert!(!parse(&doc(flag)).unwrap().iss_always, "{flag:?}");
        }
    }

    #[test]
    fn provider_text_is_cleaned() {
        assert_eq!(clean("bad\r\nthing<script>", 10), "badthing<s");
    }
}
