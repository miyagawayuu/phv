//! HTTP services named in phv.toml (`[http.<name>]`). A script names a
//! service and gives a path (`HttpClient::connect("weather")->get("/today")`);
//! the base URL and the headers taken from environment variables (API keys)
//! stay in the configuration, so they never appear in code or messages, and
//! scripts cannot send requests to any other host.
//!
//! Requests use HTTP/1.1 connections that are kept open between requests to
//! the same host (see `Pool`). Redirects are not followed: a 3xx response is
//! returned to the script.

use crate::netio::{check_plain, describe, dial, until, Stream};
use crate::{map, s};
use indexmap::IndexMap;
use phv_vm::{Key, Value, Vm, VmError};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

type R = Result<Value, VmError>;

/// Longest wait for a response when the script has no time limit.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest request body a script may send, and largest response body accepted.
const MAX_BODY: usize = 16 << 20;
/// Largest response head (status line and headers).
const MAX_HEAD: usize = 64 << 10;
const MAX_HEADERS: usize = 100;
/// Interim (1xx) responses skipped before the final one.
const MAX_INTERIM: usize = 10;

const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// Idle connections kept per host, in all, and for how long.
const KEEP_PER_HOST: usize = 8;
const KEEP_TOTAL: usize = 256;
const KEEP_IDLE: Duration = Duration::from_secs(30);

/// Connections whose last response ended exactly where its framing said
/// (`Content-Length`, or a chunked body with nothing after it), kept for the
/// next request to the same host, port and scheme: the TCP and TLS handshakes
/// are not made again. The connection goes to the address the host name was
/// looked up to when it was made, as any connection to that host would.
/// Only requests without side effects (`GET`, `HEAD`, `OPTIONS`) take one: a
/// kept connection the server has closed meanwhile is retried once on a new
/// one, which only such a request may be.
struct Pool {
    idle: HashMap<(String, u16, bool), Vec<(Stream, Instant)>>,
    count: usize,
}

static POOL: LazyLock<Mutex<Pool>> = LazyLock::new(|| Mutex::new(Pool { idle: HashMap::new(), count: 0 }));

fn pool_key(base: &Base) -> (String, u16, bool) {
    (base.host.to_ascii_lowercase(), base.port, base.tls)
}

/// An idle connection to `base`, if one was kept and is not too old.
fn take(base: &Base) -> Option<Stream> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let Pool { idle, count } = &mut *pool;
    let list = idle.get_mut(&pool_key(base))?;
    while let Some((stream, since)) = list.pop() {
        *count -= 1;
        if since.elapsed() < KEEP_IDLE {
            return Some(stream);
        }
    }
    None
}

/// Keeps a connection whose last response ended cleanly, unless enough are kept.
fn give(base: &Base, stream: Stream) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let Pool { idle, count } = &mut *pool;
    if *count >= KEEP_TOTAL {
        // Old connections of every host are dropped first.
        for list in idle.values_mut() {
            let before = list.len();
            list.retain(|(_, since)| since.elapsed() < KEEP_IDLE);
            *count -= before - list.len();
        }
        if *count >= KEEP_TOTAL {
            return;
        }
    }
    let list = idle.entry(pool_key(base)).or_default();
    if list.len() < KEEP_PER_HOST {
        list.push((stream, Instant::now()));
        *count += 1;
    }
}

/// Headers that describe the connection or the message framing: set by PHV only.
const RESERVED: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "expect",
    "proxy-authorization",
    "proxy-connection",
];

/// A parsed base URL: `https://api.example.com:8443/v1?key=x`.
struct Base {
    tls: bool,
    host: String,
    port: u16,
    /// Host header value (with the port when it is not the default).
    authority: String,
    /// Path prefix without a trailing slash (`/v1`, or empty).
    path: String,
    /// Query string of the base URL, without `?` (may be empty).
    query: String,
}

fn parse_base(url: &str) -> Result<Base, String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err("the URL must start with http:// or https://".into());
    };
    if url.chars().any(|c| c.is_control() || c == ' ') {
        return Err("the URL contains spaces or control characters".into());
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return Err("the URL cannot hold a user name or password; send credentials with headers_env".into());
    }
    if tail.contains('#') {
        return Err("the URL cannot have a fragment (#)".into());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && (!h.contains(':') || h.ends_with(']')) => {
            (h, p.parse::<u16>().map_err(|_| "the URL has an invalid port")?)
        }
        _ => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() || (host.contains(':') && !(host.starts_with('[') && host.ends_with(']'))) {
        return Err("the URL has no valid host".into());
    }
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    Ok(Base {
        tls,
        host: host.to_string(),
        port,
        authority: authority.to_string(),
        path: path.trim_end_matches('/').to_string(),
        query: query.to_string(),
    })
}

/// Checks a base URL from phv.toml (used when the configuration is loaded).
pub fn check_http_url(url: &str) -> Result<(), String> {
    parse_base(url).map(|_| ())
}

fn is_token(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Checks a header name that phv.toml or a script wants to send.
pub fn check_header_name(name: &str) -> Result<(), String> {
    if !is_token(name) {
        return Err(format!("invalid header name \"{}\"", name.escape_debug()));
    }
    if RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(format!("the {name} header is set by PHV"));
    }
    Ok(())
}

/// Checks a header value: no line breaks or other control characters (tab is allowed).
pub fn check_header_value(name: &str, value: &str) -> Result<(), String> {
    if value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
        return Err(format!("the value of the {name} header contains a line break or a control character"));
    }
    Ok(())
}

/// Whether two header names are the same header. `_` counts as `-`: servers
/// that pass headers on as variables (CGI: `HTTP_X_API_KEY`) cannot tell them apart.
fn same_header(a: &str, b: &str) -> bool {
    let norm = |c: u8| if c == b'_' { b'-' } else { c.to_ascii_lowercase() };
    a.len() == b.len() && a.bytes().zip(b.bytes()).all(|(x, y)| norm(x) == norm(y))
}

/// The keys of a query string (`a=1&b=2`), decoded.
fn query_keys(query: &str) -> Vec<String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let key = p.split('=').next().unwrap_or("").replace('+', " ");
            percent_encoding::percent_decode_str(&key).decode_utf8_lossy().into_owned()
        })
        .collect()
}

/// Checks the path a script asks for and returns it ready to send (characters
/// outside ASCII percent-encoded). It is added to the base URL's path, so it
/// must start with `/` and cannot leave that path (`..`) or name another host
/// (`//host`). Servers differ in how they read paths, so the spellings that
/// some of them take as `..` are refused too: `..;` (path parameters) and
/// `%252e%252e` (decoded twice).
fn check_path(path: &str) -> Result<String, String> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err("the path must start with a single `/`".into());
    }
    if path.contains(['?', '#']) {
        return Err("put the query in the `query` argument, not in the path".into());
    }
    if path.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') {
        return Err("the path contains spaces, backslashes or control characters".into());
    }
    let bytes = path.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        let hex = |j: usize| bytes.get(j).is_some_and(u8::is_ascii_hexdigit);
        if *b == b'%' && !(hex(i + 1) && hex(i + 2)) {
            return Err("the path has a `%` that is not followed by two hex digits".into());
        }
        let allowed = b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/%".contains(b) || *b >= 0x80;
        if !allowed {
            return Err(format!("the path cannot contain `{}`; percent-encode it", *b as char));
        }
    }
    for segment in path.split('/') {
        let once = percent_encoding::percent_decode_str(segment).decode_utf8_lossy().into_owned();
        let twice = percent_encoding::percent_decode_str(&once).decode_utf8_lossy().into_owned();
        for decoded in [&once, &twice] {
            let name = decoded.split(';').next().unwrap_or("").trim_end();
            if name == "." || name == ".." || decoded.contains(['/', '\\']) {
                return Err("the path cannot contain `.` or `..` segments or encoded slashes".into());
            }
        }
    }
    const NON_ASCII: &percent_encoding::AsciiSet = percent_encoding::CONTROLS;
    Ok(percent_encoding::utf8_percent_encode(path, NON_ASCII).to_string())
}

/// A request a script asked for, checked against its service.
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    /// The bytes it holds on the heap.
    pub(crate) fn heap_size(&self) -> usize {
        self.body.capacity() + self.headers.iter().map(|(k, v)| k.capacity() + v.capacity() + 48).sum::<usize>()
    }
}

fn encode_query(pairs: &[(String, String)]) -> String {
    const SET: &percent_encoding::AsciiSet =
        &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                percent_encoding::utf8_percent_encode(k, SET),
                percent_encoding::utf8_percent_encode(v, SET)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn str_pairs(v: &Value) -> Vec<(String, String)> {
    map(v)
        .iter()
        .map(|(k, v)| {
            let k = match k {
                Key::Str(s) => s.to_string(),
                Key::Int(i) => i.to_string(),
            };
            (k, s(v).to_string())
        })
        .collect()
}

/// A decimal Content-Length (digits only: `+3` and ` 3` are refused).
fn parse_length(v: &str) -> Option<usize> {
    if v.is_empty() || v.len() > 20 || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse().ok()
}

/// Reads a response whose body is at most `limit` bytes, and whether the
/// connection may carry another request: an HTTP/1.1 response the server did
/// not ask to close, whose end was where its framing said, with no byte
/// after it.
fn read_response(stream: &mut impl Read, head_only: bool, limit: usize) -> Result<(Response, bool), String> {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 16 << 10];
    let mut interim = 0;
    loop {
        // Interim (1xx) responses are skipped.
        let (status, headers, used, http11) = loop {
            let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
            let mut parsed = httparse::Response::new(&mut slots);
            match parsed.parse(&buf) {
                Ok(httparse::Status::Complete(used)) => {
                    let status = parsed.code.unwrap_or(0);
                    let headers: Vec<(String, String)> = parsed
                        .headers
                        .iter()
                        .map(|h| (h.name.to_ascii_lowercase(), String::from_utf8_lossy(h.value).trim().to_string()))
                        .collect();
                    break (status, headers, used, parsed.version == Some(1));
                }
                Ok(httparse::Status::Partial) => {
                    if buf.len() > MAX_HEAD {
                        return Err("the response headers are too large".into());
                    }
                    let n = read_some(stream, &mut chunk)?;
                    if n == 0 {
                        return Err("the server closed the connection before answering".into());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(httparse::Error::TooManyHeaders) => return Err("the response has too many headers".into()),
                Err(_) => return Err("the server sent an invalid HTTP response".into()),
            }
        };
        buf.drain(..used);
        if (100..200).contains(&status) {
            interim += 1;
            if interim > MAX_INTERIM {
                return Err("the server sent too many interim (1xx) responses".into());
            }
            continue;
        }
        let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        let no_body = head_only || status == 204 || status == 304;
        let keep = http11
            && !headers
                .iter()
                .any(|(n, v)| n == "connection" && v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")));
        let mut clean = false;
        let body = if no_body {
            clean = buf.is_empty();
            Vec::new()
        } else if header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
            let (body, ended) = read_chunked(stream, buf, &mut chunk, limit)?;
            clean = ended;
            body
        } else if let Some(len) = header("content-length") {
            // Repeated Content-Length headers must agree.
            let lengths: Vec<&str> =
                headers.iter().filter(|(n, _)| n == "content-length").map(|(_, v)| v.as_str()).collect();
            let invalid = "the response has an invalid Content-Length";
            let len = parse_length(len).ok_or(invalid)?;
            if lengths.iter().any(|l| parse_length(l) != Some(len)) {
                return Err(invalid.into());
            }
            if len > limit {
                return Err(too_large(limit));
            }
            while buf.len() < len {
                let n = read_some(stream, &mut chunk)?;
                if n == 0 {
                    return Err("the server closed the connection in the middle of the response".into());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            clean = buf.len() == len;
            buf.truncate(len);
            buf
        } else {
            loop {
                if buf.len() > limit {
                    return Err(too_large(limit));
                }
                let n = read_some(stream, &mut chunk)?;
                if n == 0 {
                    break buf;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        };
        return Ok((Response { status, headers, body }, keep && clean));
    }
}

fn too_large(limit: usize) -> String {
    if limit >= 1 << 20 {
        format!("the response is larger than {} MB", limit >> 20)
    } else {
        format!("the response is larger than {} KB", limit >> 10)
    }
}

fn read_some(stream: &mut impl Read, chunk: &mut [u8]) -> Result<usize, String> {
    loop {
        match stream.read(chunk) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            // Servers that close a TLS connection without close_notify: the data so far counts.
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(0),
            Err(e) => return Err(describe(&e)),
        }
    }
}

/// A chunked body, and whether it ended cleanly: the last chunk, no trailer
/// and nothing after it (already received; the connection is not waited on).
fn read_chunked(
    stream: &mut impl Read,
    mut buf: Vec<u8>,
    chunk: &mut [u8],
    limit: usize,
) -> Result<(Vec<u8>, bool), String> {
    let mut body = Vec::new();
    let mut fill = |buf: &mut Vec<u8>| -> Result<(), String> {
        let n = read_some(stream, chunk)?;
        if n == 0 {
            return Err("the server closed the connection in the middle of the response".into());
        }
        buf.extend_from_slice(&chunk[..n]);
        Ok(())
    };
    loop {
        let line_end = loop {
            if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
                break i;
            }
            if buf.len() > 1024 {
                return Err("the server sent an invalid chunk".into());
            }
            fill(&mut buf)?;
        };
        let invalid = "the server sent an invalid chunk";
        let line = String::from_utf8_lossy(&buf[..line_end]).to_string();
        let size_text = line.split(';').next().unwrap_or("").trim();
        // Hex digits only (no sign), at most 16 of them.
        if size_text.is_empty() || size_text.len() > 16 || !size_text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid.into());
        }
        let size = usize::from_str_radix(size_text, 16).map_err(|_| invalid)?;
        buf.drain(..line_end + 2);
        if size == 0 {
            // Trailers are ignored (the connection is then not kept).
            return Ok((body, buf.as_slice() == b"\r\n"));
        }
        if size > limit - body.len() {
            return Err(too_large(limit));
        }
        while buf.len() < size + 2 {
            fill(&mut buf)?;
        }
        if &buf[size..size + 2] != b"\r\n" {
            return Err(invalid.into());
        }
        body.extend_from_slice(&buf[..size]);
        buf.drain(..size + 2);
    }
}

/// Sends `req` to `base` and reads the answer, on a kept connection when the
/// request has no side effects and one is idle (see `Pool`).
fn exchange(base: &Base, req: &Request, until: Instant, limit: usize) -> Result<Response, String> {
    let safe = matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS");
    if safe {
        if let Some(mut stream) = take(base) {
            stream.set_deadline(until);
            // A kept connection may have been closed by the server: the request
            // is sent again on a new one.
            if let Ok((resp, keep)) = send(&mut stream, base, req, limit) {
                if keep {
                    give(base, stream);
                }
                return Ok(resp);
            }
        }
    }
    let mut stream = dial(&base.host, base.port, base.tls, until)?;
    let (resp, keep) = send(&mut stream, base, req, limit)?;
    if keep {
        give(base, stream);
    }
    Ok(resp)
}

/// Writes `req` on `stream` and reads the response (see `read_response`).
fn send(stream: &mut Stream, base: &Base, req: &Request, limit: usize) -> Result<(Response, bool), String> {
    let mut head = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", req.method, req.target, base.authority);
    for (name, value) in &req.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let sends_body = !req.body.is_empty() || matches!(req.method.as_str(), "POST" | "PUT" | "PATCH");
    if sends_body {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    head.push_str("\r\n");
    let io = |e: std::io::Error| describe(&e);
    stream.write_all(head.as_bytes()).map_err(io)?;
    stream.write_all(&req.body).map_err(io)?;
    stream.flush().map_err(io)?;
    read_response(stream, req.method == "HEAD", limit)
}

/// `__http_request(name, method, path, query, headers, body)`: sends a request
/// to `[http.<name>]` and returns `["status" => int, "headers" => map, "body" => string]`.
pub fn request(vm: &mut Vm, a: &[Value]) -> R {
    let (base, req, fail) = prepare(vm, a)?;
    let result = exchange(&base, &req, until(vm, REQUEST_TIMEOUT), MAX_BODY);
    vm.check_limits()?;
    match result {
        Ok(r) => Ok(response_value(r)),
        Err(e) => Err(vm.throw("NetworkError", format!("{fail}{e}"))),
    }
}

/// `__http_start(...)`: `__http_request` in the background; `await()` gives
/// the same map. The response may take a sixteenth of the memory left to the
/// script (other work may run at the same time), and at most 16 MB.
pub fn start(vm: &mut Vm, a: &[Value]) -> R {
    let (base, req, fail) = prepare(vm, a)?;
    let limit = (vm.memory_left() / 16).clamp(64 << 10, MAX_BODY);
    let until = until(vm, REQUEST_TIMEOUT);
    crate::asyncio::start(vm, until, None, move || {
        crate::asyncio::Outcome::Http(exchange(&base, &req, until, limit).map_err(|e| format!("{fail}{e}")))
    })
}

/// A response as the map the std.net module turns into an `HttpResponse`.
pub(crate) fn response_value(resp: Response) -> Value {
    let mut hs: IndexMap<Key, Value> = IndexMap::new();
    for (n, v) in resp.headers {
        let key = Key::Str(Rc::new(n));
        let joined = match hs.get(&key) {
            Some(Value::Str(old)) => format!("{old}, {v}"),
            _ => v,
        };
        hs.insert(key, Value::str(joined));
    }
    let mut out: IndexMap<Key, Value> = IndexMap::new();
    out.insert(Key::Str(Rc::new("status".into())), Value::Int(resp.status as i64));
    out.insert(Key::Str(Rc::new("headers".into())), Value::map(hs));
    out.insert(Key::Str(Rc::new("body".into())), Value::str(String::from_utf8_lossy(&resp.body).into_owned()));
    Value::map(out)
}

/// Checks the arguments of `__http_request` / `__http_start` and builds the
/// request, with the prefix of its error messages.
fn prepare(vm: &mut Vm, a: &[Value]) -> Result<(Base, Request, String), VmError> {
    let name = s(&a[0]).to_string();
    let fail = |vm: &Vm, msg: String| vm.throw("NetworkError", format!("HTTP service \"{name}\": {msg}"));
    let Some(spec) = vm.host.http.get(&name).cloned() else {
        return Err(vm.throw("NetworkError", format!("no HTTP service named \"{name}\" in [http] of phv.toml")));
    };
    let Some(url) = spec.url.clone() else {
        let var = spec.url_env.as_deref().unwrap_or("?");
        return Err(fail(vm, format!("the environment variable {var} is not set")));
    };
    let method = s(&a[1]).to_ascii_uppercase();
    if !METHODS.contains(&method.as_str()) {
        return Err(vm.throw("ValueError", format!("unsupported HTTP method \"{}\"", method.escape_debug())));
    }
    let path = match check_path(s(&a[2])) {
        Ok(p) => p,
        Err(e) => return Err(vm.throw("ValueError", e)),
    };
    let body = s(&a[5]).as_bytes().to_vec();
    if body.len() > MAX_BODY {
        return Err(vm.throw("ValueError", format!("the request body is larger than {} MB", MAX_BODY >> 20)));
    }
    // Headers: those of phv.toml first; a script may replace the fixed ones but
    // not those taken from environment variables (credentials).
    let mut headers: Vec<(String, String)> = Vec::new();
    for (h, value) in &spec.headers {
        let Some(value) = value else {
            let var = spec.headers_env.iter().find(|(n, _)| n == h).map_or("?", |(_, v)| v.as_str());
            return Err(fail(vm, format!("the environment variable {var} (for the {h} header) is not set")));
        };
        headers.push((h.clone(), value.clone()));
    }
    for (h, value) in str_pairs(&a[4]) {
        let checked = check_header_name(&h).and_then(|_| check_header_value(&h, &value));
        if let Err(e) = checked {
            return Err(vm.throw("ValueError", e));
        }
        if spec.headers_env.iter().any(|(n, _)| same_header(n, &h)) {
            return Err(vm.throw("ValueError", format!("the {h} header comes from phv.toml and cannot be replaced")));
        }
        headers.retain(|(n, _)| !same_header(n, &h));
        headers.push((h, value));
    }
    let has = |headers: &[(String, String)], n: &str| headers.iter().any(|(h, _)| h.eq_ignore_ascii_case(n));
    if !has(&headers, "user-agent") {
        headers.push(("User-Agent".into(), concat!("phv/", env!("CARGO_PKG_VERSION")).into()));
    }
    if !has(&headers, "accept-encoding") {
        headers.push(("Accept-Encoding".into(), "identity".into()));
    }
    let base = match parse_base(&url) {
        Ok(b) => b,
        Err(e) => return Err(fail(vm, e)),
    };
    if let Err(e) = check_plain(&base.host, base.tls, spec.tls, "https://") {
        return Err(fail(vm, e));
    }
    let pairs = str_pairs(&a[3]);
    // The base URL's query may hold a key; the script cannot add a second value for it.
    let fixed = query_keys(&base.query);
    if let Some((k, _)) = pairs.iter().find(|(k, _)| fixed.contains(k)) {
        let msg = format!("the query parameter \"{}\" comes from phv.toml and cannot be replaced", k.escape_debug());
        return Err(vm.throw("ValueError", msg));
    }
    let query = encode_query(&pairs);
    let mut target = format!("{}{path}", base.path);
    match (base.query.is_empty(), query.is_empty()) {
        (true, true) => {}
        (false, true) => target.push_str(&format!("?{}", base.query)),
        (true, false) => target.push_str(&format!("?{query}")),
        (false, false) => target.push_str(&format!("?{}&{query}", base.query)),
    }
    let fail = format!("HTTP service \"{name}\": {method} {path}: ");
    let req = Request { method, target, headers, body };
    Ok((base, req, fail))
}

pub fn lookup(name: &str) -> Option<phv_vm::BuiltinFn> {
    let f: phv_vm::BuiltinFn = match name {
        "__http_request" => request,
        "__http_start" => start,
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_base_urls() {
        let b = parse_base("https://api.example.com/v1/?key=x").unwrap();
        assert_eq!(
            (b.tls, b.host.as_str(), b.port, b.path.as_str(), b.query.as_str()),
            (true, "api.example.com", 443, "/v1", "key=x")
        );
        let b = parse_base("http://[::1]:8080").unwrap();
        assert_eq!((b.host.as_str(), b.port, b.authority.as_str(), b.path.as_str()), ("[::1]", 8080, "[::1]:8080", ""));
        for bad in ["ftp://a/", "https://user:pw@a/", "https://a/#x", "https://:80/", "https://a:x/", "https://a b/"] {
            assert!(parse_base(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn paths_stay_under_the_base() {
        assert_eq!(check_path("/v1/items/42").unwrap(), "/v1/items/42");
        assert_eq!(check_path("/search%20results;v=1").unwrap(), "/search%20results;v=1");
        assert_eq!(check_path("/café").unwrap(), "/caf%C3%A9");
        for bad in [
            "items",
            "//evil.example/x",
            "/a/../b",
            "/a/%2e%2E/b",
            "/a/%2F/b",
            "/a/..;/b",
            "/a/%2e%2e;x/b",
            "/a/%252e%252e/b",
            "/a/%252F/b",
            "/a/.;/b",
            "/a?x=1",
            "/a#b",
            "/a b",
            "/a\\b",
            "/a\r\nX: y",
            "/a%",
            "/a%zz",
            "/a<b>",
            "/a\"b",
            "/a\u{3000}b",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn headers_are_checked() {
        assert!(check_header_name("X-Api-Key").is_ok());
        for bad in ["Host", "content-length", "Transfer-Encoding", "Bad Name", "X:Y", ""] {
            assert!(check_header_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_header_value("X", "a\r\nInjected: 1").is_err());
        assert!(check_header_value("X", "a\x0bb").is_err());
        assert!(check_header_value("X", "a\tb").is_ok());
        assert!(same_header("X-Api-Key", "x_api_KEY") && !same_header("X-Api-Key", "X-Api-Keys"));
        assert_eq!(query_keys("key=x&a+b=1&c"), ["key", "a b", "c"]);
    }

    #[test]
    fn reads_chunked_and_sized_bodies() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX-A: 1\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        let (r, keep) = read_response(&mut &raw[..], false, MAX_BODY).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hello world"[..]));
        assert!(r.headers.contains(&("x-a".into(), "1".into())));
        assert!(keep, "a chunked body that ended cleanly");
        // Bytes past the stated length: the connection is not kept.
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\n\r\nabcdef";
        let (r, keep) = read_response(&mut &raw[..], false, MAX_BODY).unwrap();
        assert_eq!((r.status, r.body.as_slice(), keep), (404, &b"abc"[..], false));
        let raw = b"HTTP/1.1 200 OK\r\n\r\nuntil the end";
        let (r, keep) = read_response(&mut &raw[..], false, MAX_BODY).unwrap();
        assert_eq!((r.body.as_slice(), keep), (&b"until the end"[..], false));
        // Kept: an exact length; not kept: HTTP/1.0, `Connection: close`, a
        // trailer, and bytes after a response without a body.
        for (raw, kept) in [
            (&b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"[..], true),
            (b"HTTP/1.1 204 No Content\r\n\r\n", true),
            (b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok", false),
            (b"HTTP/1.1 200 OK\r\nConnection: keep-alive, Close\r\nContent-Length: 2\r\n\r\nok", false),
            (b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\nX-T: 1\r\n\r\n", false),
            (b"HTTP/1.1 204 No Content\r\n\r\nHTTP/1.1 200 OK\r\n", false),
        ] {
            let got = read_response(&mut &raw[..], false, MAX_BODY).unwrap().1;
            assert_eq!(got, kept, "{}", String::from_utf8_lossy(raw));
        }
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort";
        assert!(read_response(&mut &raw[..], false, MAX_BODY).is_err());
        let raw = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert!(read_response(&mut raw.as_bytes(), false, MAX_BODY).is_err());
        // A chunk size near usize::MAX after a first chunk must not overflow.
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\nffffffffffffffff\r\n";
        assert!(read_response(&mut &raw[..], false, MAX_BODY).unwrap_err().contains("larger than"));
        for bad in [
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n+5\r\nhello\r\n0\r\n\r\n"[..],
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10000000000000000\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhelloXX0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: +3\r\n\r\nabc",
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\nabcd",
        ] {
            assert!(read_response(&mut &bad[..], false, MAX_BODY).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let endless = b"HTTP/1.1 100 Continue\r\n\r\n".repeat(MAX_INTERIM + 1);
        assert!(read_response(&mut &endless[..], false, MAX_BODY).unwrap_err().contains("interim"));
    }
}
