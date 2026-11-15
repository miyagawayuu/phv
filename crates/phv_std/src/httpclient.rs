//! HTTP services named in phv.toml (`[http.<name>]`). A script names a
//! service and gives a path (`HttpClient::connect("weather")->get("/today")`);
//! the base URL and the headers taken from environment variables (API keys)
//! stay in the configuration, so they never appear in code or messages, and
//! scripts cannot send requests to any other host.
//!
//! Requests use HTTP/1.1 connections that are kept open between requests to
//! the same host (see `Pool`). Redirects are not followed: a 3xx response is
//! returned to the script.

use crate::deadline::until;
use crate::netio::{check_plain, describe, dial_avoiding, Stream};
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

/// A request with side effects (`POST`, `PUT`, `PATCH`, `DELETE`) takes a kept
/// connection only when it was idle for less than this: servers close idle
/// connections after a few seconds (Apache, Node: 5; Gunicorn: 2), and such a
/// request cannot be sent again when the connection turns out closed (see
/// `exchange`). A connection idle for longer waits for a request without
/// side effects.
const KEEP_IDLE_UNSAFE: Duration = Duration::from_secs(1);

/// Connections whose last response ended exactly where its framing said
/// (`Content-Length`, or a chunked body with nothing after it), kept for the
/// next request to the same host, port and scheme: the TCP and TLS handshakes
/// are not made again. The connection goes to the address the host name was
/// looked up to when it was made, as any connection to that host would.
/// Every request takes one; a kept connection the server has closed meanwhile
/// is retried once on a new one when the request has no side effects (`GET`,
/// `HEAD`, `OPTIONS`); a request with side effects is never sent twice (see
/// `exchange`).
struct Pool {
    idle: HashMap<(String, u16, bool), Vec<(Stream, Instant)>>,
    count: usize,
}

static POOL: LazyLock<Mutex<Pool>> = LazyLock::new(|| Mutex::new(Pool { idle: HashMap::new(), count: 0 }));

fn pool_key(base: &Base) -> (String, u16, bool) {
    (base.host.to_ascii_lowercase(), base.port, base.tls)
}

/// An idle connection to `base`, if one was kept, was idle for less than
/// `max_idle` and has received nothing since its last response (found by the
/// 1.0.0 audit: a response the server sent later was read by the next
/// request, another visitor's, as its own). The most recently kept is taken
/// first; one idle for longer than `max_idle` (but not yet `KEEP_IDLE`) stays
/// for a request that may take it.
fn take(base: &Base, max_idle: Duration) -> Option<Stream> {
    loop {
        let (mut stream, since) = {
            let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            let Pool { idle, count } = &mut *pool;
            let list = idle.get_mut(&pool_key(base))?;
            let idle_for = list.last()?.1.elapsed();
            if idle_for >= max_idle && idle_for < KEEP_IDLE {
                return None;
            }
            let kept = list.pop()?;
            *count -= 1;
            kept
        };
        if since.elapsed() < KEEP_IDLE && stream.quiet() {
            return Some(stream);
        }
    }
}

/// Keeps a connection whose last response ended cleanly, unless enough are
/// kept or something more already came on it (a stray answer sent right
/// after the real one would otherwise be read by the next request).
fn give(base: &Base, mut stream: Stream) {
    if !stream.quiet() {
        return;
    }
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
    /// The address `phv serve` listens on, when a page of it sends the
    /// request: a connection that would reach it is refused (R22-19).
    own: Option<std::net::SocketAddr>,
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
        own: None,
    })
}

/// Checks a base URL from phv.toml (used when the configuration is loaded).
pub fn check_http_url(url: &str) -> Result<(), String> {
    parse_base(url).map(|_| ())
}

/// Checks a URL of `[oauth]` in phv.toml: https, unless the host is this
/// machine or `tls` is false.
pub fn check_oauth_url(url: &str, tls: Option<bool>) -> Result<(), String> {
    let base = parse_base(url)?;
    check_plain(&base.host, base.tls, tls, "https://")
}

/// Checks an endpoint that an OpenID Connect provider's discovery document
/// names: as `check_oauth_url`, and it uses TLS when the issuer does, and is
/// on this machine only when the issuer is. (TLS is not needed on this
/// machine, so a remote provider could otherwise have the code and the client
/// secret sent in plain text to a port of its choosing here.)
pub(crate) fn check_discovered_url(issuer: &str, url: &str, tls: Option<bool>) -> Result<(), String> {
    check_oauth_url(url, tls)?;
    let (issuer, endpoint) = (parse_base(issuer)?, parse_base(url)?);
    if issuer.tls && !endpoint.tls {
        return Err("the issuer uses https://, so its endpoints must too".into());
    }
    // Both read the same way: an issuer on this machine may name its own
    // endpoints. A name is on this machine when every address it has now is
    // (the ones its discovery document was just fetched from): a name of the
    // hosts file for a provider run here, say.
    if is_this_machine(&issuer.host) || resolves_here(&issuer) {
        return Ok(());
    }
    if is_this_machine(&endpoint.host) {
        return Err(LOCAL_ENDPOINT.into());
    }
    // A name says nothing of where it leads: it is looked at again, by the
    // addresses it has, when a request is sent to it (`send_url`).
    let mut remote = remote_endpoints();
    if remote.len() < MAX_REMOTE_ENDPOINTS {
        remote.insert((endpoint.host.to_ascii_lowercase(), endpoint.port));
    }
    Ok(())
}

const LOCAL_ENDPOINT: &str = "the endpoint is on this machine but the issuer is not";

/// The hosts and ports of the endpoints that providers on other machines
/// named (`check_discovered_url`): a request to one of them is refused when
/// its name has an address of this machine at that time (found by the
/// re-attack of the 21st assessment: a name that resolves to 127.0.0.1 got
/// the code and the client secret posted to a port of this machine).
static REMOTE_ENDPOINTS: LazyLock<Mutex<std::collections::HashSet<(String, u16)>>> = LazyLock::new(Default::default);
/// Far more than the endpoints of the providers of one configuration.
const MAX_REMOTE_ENDPOINTS: usize = 4096;

fn remote_endpoints() -> std::sync::MutexGuard<'static, std::collections::HashSet<(String, u16)>> {
    REMOTE_ENDPOINTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Refuses a request to an endpoint that a provider on another machine named
/// when the endpoint's name leads to this machine now: the addresses are the
/// ones the connection made right after is given (they are kept for 30
/// seconds, `netio::resolve`).
fn check_remote_endpoint(base: &Base, until: Instant) -> Result<(), String> {
    if !remote_endpoints().contains(&(base.host.to_ascii_lowercase(), base.port)) {
        return Ok(());
    }
    let addrs = crate::netio::resolve(&base.host, base.port, until)?;
    if addrs.iter().any(|a| local_address(a.ip())) {
        return Err(LOCAL_ENDPOINT.into());
    }
    Ok(())
}

/// Whether the name of `base` has addresses now, all of this machine
/// (`local_address`). Looked up within two seconds (the addresses of a name
/// just connected to are kept, `netio::resolve`); a name that cannot be
/// looked up is not this machine.
fn resolves_here(base: &Base) -> bool {
    let until = Instant::now() + Duration::from_secs(2);
    crate::netio::resolve(&base.host, base.port, until)
        .is_ok_and(|addrs| !addrs.is_empty() && addrs.iter().all(|a| local_address(a.ip())))
}

/// Whether a connection to `ip` reaches this machine whatever its network:
/// a loopback address (also as an IPv4-mapped IPv6 address) or an
/// unspecified one (`0.0.0.0`, `::`), which a connection takes as this machine.
fn local_address(ip: std::net::IpAddr) -> bool {
    let ip = ip.to_canonical();
    ip.is_loopback() || ip.is_unspecified()
}

/// An IPv4 address in the older forms name lookups still take (`inet_aton`):
/// one to four parts, each decimal, octal (`0177`) or hexadecimal (`0x7f`),
/// the last filling the bytes that are left: `127.1`, `2130706433`,
/// `0x7f.0.0.1`.
fn old_ipv4(host: &str) -> Option<std::net::Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    let mut numbers = Vec::with_capacity(4);
    for part in &parts {
        let lower = part.to_ascii_lowercase();
        let n = if let Some(hex) = lower.strip_prefix("0x") {
            u32::from_str_radix(hex, 16).ok()?
        } else if lower.len() > 1 && lower.starts_with('0') {
            u32::from_str_radix(&lower[1..], 8).ok()?
        } else if !lower.is_empty() && lower.bytes().all(|b| b.is_ascii_digit()) {
            lower.parse::<u32>().ok()?
        } else {
            return None;
        };
        numbers.push(n);
    }
    let (last, first) = numbers.split_last()?;
    let mut value: u32 = 0;
    for n in first {
        if *n > 255 {
            return None;
        }
        value = (value << 8) | n;
    }
    let room = 8 * (4 - first.len()) as u32;
    if room < 32 && *last >= 1 << room {
        return None;
    }
    let value = if first.is_empty() { *last } else { (value << room) | last };
    Some(std::net::Ipv4Addr::from(value))
}

/// Whether `host` may be this machine, read more widely than
/// `is_local_host`: `localhost` also with a trailing dot, a loopback address
/// also written as an IPv4-mapped IPv6 address (`[::ffff:127.0.0.1]`), and
/// the unspecified addresses (`0.0.0.0`, `::`), which a connection takes as
/// this machine; an IPv6 address also with a zone (`[::1%1]`, `[::1%25lo]`),
/// and an IPv4 address also in its older forms (`127.1`, `2130706433`,
/// `0x7f.0.0.1`; `old_ipv4`). Only for telling whether an issuer and the
/// endpoints it names are on this machine (`check_discovered_url`; found by
/// the 21st assessment: a remote provider could name such an endpoint):
/// where being on this machine allows something (a connection without TLS),
/// `is_local_host` keeps deciding, so nothing more is allowed.
fn is_this_machine(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    let h = h.strip_suffix('.').unwrap_or(h);
    // The zone of an IPv6 address (`%25` in a URL, or as it is).
    let address = h.split_once('%').map_or(h, |(address, _)| address);
    h.eq_ignore_ascii_case("localhost")
        || address.parse::<std::net::IpAddr>().is_ok_and(local_address)
        || old_ipv4(h).is_some_and(|ip| local_address(ip.into()))
}

fn is_token(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Checks a header name that phv.toml or a script wants to send.
pub fn check_header_name(name: &str) -> Result<(), String> {
    if !is_token(name) {
        return Err(format!("invalid header name \"{}\"", brief(name)));
    }
    // `_` counts as `-` here too: servers that pass headers on as variables
    // read `Content_Length` as `Content-Length` (21st assessment).
    if RESERVED.iter().any(|r| same_header(r, name)) {
        return Err(format!("the {} header is set by PHV", brief(name)));
    }
    Ok(())
}

/// Checks a header value: no line breaks or other control characters (tab is allowed).
pub fn check_header_value(name: &str, value: &str) -> Result<(), String> {
    if value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
        return Err(format!("the value of the {} header contains a line break or a control character", brief(name)));
    }
    Ok(())
}

/// Whether two header names are the same header. `_` counts as `-`: servers
/// that pass headers on as variables (CGI: `HTTP_X_API_KEY`) cannot tell them
/// apart. So do `.` and a run of them (`Content.Length`, `Content__Length`):
/// gateways that make variable names of headers turn every character a name
/// cannot hold into `_`, and some read several as one.
fn same_header(a: &str, b: &str) -> bool {
    header_key(a) == header_key(b)
}

/// A header name as `same_header` compares it.
fn header_key(name: &str) -> String {
    let mut key = String::with_capacity(name.len());
    for c in name.chars() {
        let c = if matches!(c, '_' | '.') { '-' } else { c.to_ascii_lowercase() };
        if !(c == '-' && key.ends_with('-')) {
            key.push(c);
        }
    }
    key
}

/// A value a script gave, for a message: its first 64 characters, escaped.
/// A message never holds a whole value (found by the re-attack of the 21st
/// assessment: a 40 MB address made a 41 MB exception message).
pub(crate) fn brief(value: &str) -> String {
    let short: String = value.chars().take(64).collect();
    let more = if short.len() < value.len() { "…" } else { "" };
    format!("{}{more}", short.escape_debug())
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

/// The names a server may read the query parameter `key` as, lower-cased
/// (ASP.NET compares names without case). PHP drops leading spaces and what
/// follows a NUL, and reads `.` and a space as `_`; it reads `name[...]` as
/// `name` (an array that replaces a plain value of that name, as Rack does),
/// and a `[` with no `]` after it as `_`, since PHP 8 with the `.`, spaces and
/// `[` after it too (found by the 1.2.0 audit: `account.id` and `account id`
/// replaced a fixed `account_id`).
fn query_names(key: &str) -> Vec<String> {
    let mangle = |s: &str| s.replace([' ', '.'], "_");
    let mut out = vec![key.to_string()];
    let php = key.split('\0').next().unwrap_or("").trim_start_matches(' ');
    match php.find('[') {
        None => out.push(mangle(php)),
        Some(i) if php[i + 1..].contains(']') => out.push(mangle(&php[..i])),
        Some(i) => {
            out.push(format!("{}_{}", mangle(&php[..i]), php[i + 1..].replace([' ', '.', '['], "_")));
            out.push(format!("{}_{}", mangle(&php[..i]), &php[i + 1..]));
        }
    }
    out.iter_mut().for_each(|n| n.make_ascii_lowercase());
    out
}

/// The fixed query parameter of the base URL (keys `fixed`, see `query_keys`)
/// that the script's parameter `key` would replace at the server, if any. It
/// is compared as the server may read it (`query_names`), also once more
/// percent-decoded, for servers that decode twice.
fn replaces_fixed<'f>(key: &str, fixed: &'f [String]) -> Option<&'f String> {
    let decoded = percent_encoding::percent_decode_str(key).decode_utf8_lossy();
    let mut names = query_names(key);
    names.extend(query_names(&decoded));
    fixed.iter().find(|f| query_names(f).iter().any(|n| names.contains(n)))
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
    pub(crate) fn status(&self) -> u16 {
        self.status
    }

    /// A header's value (the first, any case).
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

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
            // Exactly the body: growing by doubling could hold twice as much.
            buf.reserve_exact(len.saturating_sub(buf.len()));
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
                    settle(&mut buf);
                    break buf;
                }
                make_room(&mut buf, n, limit + chunk.len());
                buf.extend_from_slice(&chunk[..n]);
            }
        };
        return Ok((Response { status, headers, body }, keep && clean));
    }
}

/// A body of unknown size grows by doubling up to this, and then gets all
/// the room it may ever take at once (`make_room`).
const GROW_AT_ONCE: usize = 256 << 10;

/// Makes room for `more` bytes in a body that may hold `most` in all. A
/// vector left to grow by itself doubles, and holds the old and the new
/// buffer while it does: up to three times the body, whatever the limit
/// (found by the re-attack of the 21st assessment: a response without a
/// length took 40 MB under a 16 MB limit). Here it doubles only while it
/// is small, and then takes `most` once, so no more than `GROW_AT_ONCE`
/// past `most` is ever held.
fn make_room(body: &mut Vec<u8>, more: usize, most: usize) {
    let need = body.len() + more;
    if need <= body.capacity() {
        return;
    }
    let room = if need <= GROW_AT_ONCE { need.next_power_of_two().max(8192) } else { need.max(most) };
    body.reserve_exact(room - body.len());
}

/// Gives back the room a finished body did not take, when that is much.
fn settle(body: &mut Vec<u8>) {
    if body.capacity() - body.len() > GROW_AT_ONCE {
        body.shrink_to_fit();
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
            settle(&mut body);
            // Trailers are ignored (the connection is then not kept).
            return Ok((body, buf.as_slice() == b"\r\n"));
        }
        if size > limit - body.len() {
            return Err(too_large(limit));
        }
        // The chunk's bytes go to the body as they come (found by the 21st
        // assessment: a whole chunk was held in the buffer and then copied).
        make_room(&mut body, size, limit);
        let mut left = size;
        while left > 0 {
            if buf.is_empty() {
                fill(&mut buf)?;
            }
            let n = left.min(buf.len());
            body.extend_from_slice(&buf[..n]);
            buf.drain(..n);
            left -= n;
        }
        while buf.len() < 2 {
            fill(&mut buf)?;
        }
        if &buf[..2] != b"\r\n" {
            return Err(invalid.into());
        }
        buf.drain(..2);
    }
}

/// Sends `req` to `base` and reads the answer, on a kept connection when one
/// is idle (see `Pool`).
fn exchange(base: &Base, req: &Request, until: Instant, limit: usize) -> Result<Response, String> {
    let safe = matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS");
    if let Some(mut stream) = take(base, if safe { KEEP_IDLE } else { KEEP_IDLE_UNSAFE }) {
        stream.set_deadline(until);
        // A kept connection may have been closed by the server: a request
        // without side effects is sent again on a new one, but only when no
        // byte of an answer came (found by the 1.0.0 audit: an answer refused
        // as too large was asked for again). A request with side effects is
        // never sent twice: the server may have acted on what reached it, and
        // a failed write does not say how much did (found by the 1.2.1 audit:
        // a POST whose write failed after its head and 64 KB of its body had
        // reached the server was sent whole again on a new connection).
        let mut received = 0;
        match send(&mut stream, base, req, limit, &mut received) {
            Ok((resp, keep)) => {
                if keep {
                    give(base, stream);
                }
                return Ok(resp);
            }
            Err(e) if !safe || received > 0 => return Err(e),
            Err(_) => {}
        }
    }
    let mut stream = dial_avoiding(&base.host, base.port, base.tls, until, base.own)?;
    let (resp, keep) = send(&mut stream, base, req, limit, &mut 0)?;
    if keep {
        give(base, stream);
    }
    Ok(resp)
}

/// Counts the bytes read through it.
struct Counting<'a> {
    inner: &'a mut Stream,
    read: &'a mut usize,
}

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        *self.read += n;
        Ok(n)
    }
}

/// Writes `req` on `stream` and reads the response (see `read_response`),
/// counting in `received` the bytes of the response that came.
fn send(
    stream: &mut Stream,
    base: &Base,
    req: &Request,
    limit: usize,
    received: &mut usize,
) -> Result<(Response, bool), String> {
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
    read_response(&mut Counting { inner: stream, read: received }, req.method == "HEAD", limit)
}

/// `__http_request(name, method, path, query, headers, body)`: sends a request
/// to `[http.<name>]` and returns `["status" => int, "headers" => map, "body" => string]`.
pub fn request(vm: &mut Vm, a: &[Value]) -> R {
    let (base, mut req, fail) = prepare(vm, a)?;
    let span = ClientSpan::open(vm, &base, &mut req, Some(s(&a[0])));
    let limit = body_limit(vm);
    let result = exchange(&base, &req, until(vm, REQUEST_TIMEOUT), limit).and_then(|r| r.into_text(limit));
    ClientSpan::close(span, &result);
    vm.check_limits()?;
    match result {
        Ok(r) => Ok(response_value(r)),
        Err(e) => Err(vm.throw("NetworkError", format!("{fail}{e}"))),
    }
}

/// The largest response a script's request reads, as bytes and as the text
/// they become (`Response::into_text`): `MAX_BODY`, and at most a third of
/// the memory left to the script, since the bytes and the text are held
/// together while one is made from the other (found by the 21st assessment:
/// 16 MB were read whatever was left, 84 MB in all under a 16 MB limit).
fn body_limit(vm: &Vm) -> usize {
    (vm.memory_left() / 3).clamp(64 << 10, MAX_BODY)
}

/// The same for a request sent in the background: a script may have 16 of
/// them running at once, each holding its bytes and its text for a moment
/// on a thread whose memory is not the script's, so each gets a fortieth of
/// what is left (found by the re-attack of the 21st assessment: with a
/// sixteenth each, and the text made unreserved when awaited, 16 responses
/// took 523 MB under a 128 MB limit).
fn background_limit(vm: &Vm) -> usize {
    (vm.memory_left() / 40).clamp(64 << 10, MAX_BODY)
}

impl Response {
    /// The response with its body as the UTF-8 text a script gets (each
    /// sequence that is not UTF-8 becomes U+FFFD), refused when the text is
    /// longer than `limit`: text of bytes that are not UTF-8 is up to three
    /// times as long as they are. Afterwards `response_value` takes the body
    /// as it is, so nothing grows where the response is awaited.
    fn into_text(mut self, limit: usize) -> Result<Response, String> {
        if std::str::from_utf8(&self.body).is_ok() {
            return Ok(self);
        }
        if text_len(&self.body) > limit {
            return Err(too_large(limit));
        }
        self.body = body_text(std::mem::take(&mut self.body)).into_bytes();
        Ok(self)
    }
}

/// The length of `bytes` as text (see `body_text`).
fn text_len(bytes: &[u8]) -> usize {
    let replaced = |c: &std::str::Utf8Chunk<'_>| if c.invalid().is_empty() { 0 } else { '\u{FFFD}'.len_utf8() };
    bytes.utf8_chunks().map(|c| c.valid().len() + replaced(&c)).sum()
}

/// `__http_start(...)`: `__http_request` in the background; `await()` gives
/// the same map. The response may take a fortieth of the memory left to the
/// script (see `background_limit`), and at most 16 MB.
pub fn start(vm: &mut Vm, a: &[Value]) -> R {
    let (base, mut req, fail) = prepare(vm, a)?;
    let span = ClientSpan::open(vm, &base, &mut req, Some(s(&a[0])));
    let limit = background_limit(vm);
    let until = until(vm, REQUEST_TIMEOUT);
    crate::asyncio::start(vm, until, None, move || {
        let result = exchange(&base, &req, until, limit).and_then(|r| r.into_text(limit));
        ClientSpan::close(span, &result);
        crate::asyncio::Outcome::Http(result.map_err(|e| format!("{fail}{e}")))
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
    out.insert(Key::Str(Rc::new("body".into())), Value::str(body_text(resp.body)));
    Value::map(out)
}

/// A body as text: as it is when it is UTF-8 (no copy); otherwise each
/// sequence that is not becomes U+FFFD, in a string of exactly the size that
/// takes (one grown by doubling could hold twice as much).
fn body_text(body: Vec<u8>) -> String {
    let bytes = match String::from_utf8(body) {
        Ok(text) => return text,
        Err(e) => e.into_bytes(),
    };
    let mut text = String::with_capacity(text_len(&bytes));
    for c in bytes.utf8_chunks() {
        text.push_str(c.valid());
        if !c.invalid().is_empty() {
            text.push('\u{FFFD}');
        }
    }
    text
}

/// Checks the arguments of `__http_request` / `__http_start` and builds the
/// request, with the prefix of its error messages.
fn prepare(vm: &mut Vm, a: &[Value]) -> Result<(Base, Request, String), VmError> {
    let name = s(&a[0]).to_string();
    let fail = |vm: &Vm, msg: String| vm.throw("NetworkError", format!("HTTP service \"{name}\": {msg}"));
    let Some(spec) = vm.host.http.get(&name).cloned() else {
        let name = brief(&name);
        return Err(vm.throw("NetworkError", format!("no HTTP service named \"{name}\" in [http] of phv.toml")));
    };
    let Some(url) = spec.url.clone() else {
        let var = spec.url_env.as_deref().unwrap_or("?");
        return Err(fail(vm, format!("the environment variable {var} is not set")));
    };
    let method = s(&a[1]).to_ascii_uppercase();
    if !METHODS.contains(&method.as_str()) {
        return Err(vm.throw("ValueError", format!("unsupported HTTP method \"{}\"", brief(&method))));
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
    // `[server] request_ids`: the request's trace goes on to the services it calls.
    if let Some(tp) = &vm.http.traceparent {
        if !has(&headers, "traceparent") {
            headers.push(("traceparent".into(), tp.clone()));
        }
    }
    let mut base = match parse_base(&url) {
        Ok(b) => b,
        Err(e) => return Err(fail(vm, e)),
    };
    base.own = vm.host.listen;
    if let Err(e) = check_plain(&base.host, base.tls, spec.tls, "https://") {
        return Err(fail(vm, e));
    }
    let pairs = str_pairs(&a[3]);
    // The base URL's query may hold a key; the script cannot add a second value
    // for it, also under a name the server reads as the same (`query_names`).
    let fixed = query_keys(&base.query);
    if let Some((k, f)) = pairs.iter().find_map(|(k, _)| replaces_fixed(k, &fixed).map(|f| (k, f))) {
        let msg = if k == f {
            format!("the query parameter \"{}\" comes from phv.toml and cannot be replaced", brief(k))
        } else {
            format!(
                "the query parameter \"{}\" would be read as \"{}\", which comes from phv.toml and cannot be replaced",
                brief(k),
                brief(f)
            )
        };
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
    let fail = format!("HTTP service \"{name}\": {method} {}: ", brief(&path));
    let req = Request { method, target, headers, body };
    Ok((base, req, fail))
}

/// A GET of `path` from `[http.<name>]`, as a script's request would be
/// prepared (its configured headers included).
fn service_get(vm: &mut Vm, name: &str, path: &str) -> Result<(Base, Request, String), VmError> {
    let empty = Value::map(IndexMap::new());
    prepare(vm, &[Value::str(name), Value::str("GET"), Value::str(path), empty.clone(), empty, Value::str("")])
}

fn url_of(base: &Base, req: &Request) -> String {
    format!("{}://{}{}", if base.tls { "https" } else { "http" }, base.authority, req.target)
}

/// The URL a GET of `path` from `[http.<name>]` goes to (to name what it
/// fetched in a cache; never shown).
pub(crate) fn service_url(vm: &mut Vm, name: &str, path: &str) -> Result<String, VmError> {
    let (base, req, _) = service_get(vm, name, path)?;
    Ok(url_of(&base, &req))
}

/// Sends a GET of `path` to `[http.<name>]`; the outer error is one of the
/// script (an unknown service), the inner one a network error.
pub(crate) fn get_service(
    vm: &mut Vm,
    name: &str,
    path: &str,
    limit: usize,
) -> Result<Result<Response, String>, VmError> {
    let (base, mut req, _) = service_get(vm, name, path)?;
    let span = ClientSpan::open(vm, &base, &mut req, Some(name));
    let result = exchange(&base, &req, until(vm, REQUEST_TIMEOUT), limit);
    ClientSpan::close(span, &result);
    Ok(result)
}

/// Sends a request to a URL that PHV itself was given (an OAuth provider's
/// endpoints), not one a script wrote: TLS unless the host is this machine
/// or `tls` is false, the URL's path and query sent as they are.
pub(crate) fn send_url(
    vm: &Vm,
    method: &str,
    url: &str,
    tls: Option<bool>,
    mut headers: Vec<(String, String)>,
    body: Vec<u8>,
    limit: usize,
) -> Result<Response, String> {
    let mut base = parse_base(url)?;
    base.own = vm.host.listen;
    check_plain(&base.host, base.tls, tls, "https://")?;
    check_remote_endpoint(&base, until(vm, REQUEST_TIMEOUT))?;
    let scheme_len = if base.tls { 8 } else { 7 };
    let tail = &url[scheme_len + base.authority.len()..];
    let target = if tail.is_empty() || tail.starts_with('?') { format!("/{tail}") } else { tail.to_string() };
    let has = |headers: &[(String, String)], n: &str| headers.iter().any(|(h, _)| h.eq_ignore_ascii_case(n));
    if !has(&headers, "user-agent") {
        headers.push(("User-Agent".into(), concat!("phv/", env!("CARGO_PKG_VERSION")).into()));
    }
    headers.push(("Accept-Encoding".into(), "identity".into()));
    if let Some(tp) = &vm.http.traceparent {
        headers.push(("traceparent".into(), tp.clone()));
    }
    let mut req = Request { method: method.to_string(), target, headers, body };
    let span = ClientSpan::open(vm, &base, &mut req, None);
    let result = exchange(&base, &req, until(vm, REQUEST_TIMEOUT), limit);
    ClientSpan::close(span, &result);
    result
}

/// Spans a script may add to its trace; more are not recorded.
const MAX_SPANS: usize = 1000;

/// A request being sent within a recorded trace (`[telemetry]`): it carries a
/// `traceparent` naming its own span, recorded when the answer comes.
pub(crate) struct ClientSpan {
    span: phv_vm::Span,
    spans: std::sync::Arc<Mutex<Vec<phv_vm::Span>>>,
}

impl ClientSpan {
    fn open(vm: &Vm, base: &Base, req: &mut Request, service: Option<&str>) -> Option<ClientSpan> {
        let trace = vm.http.trace.as_ref()?;
        let span_id = crate::random_hex(8);
        let flags = if trace.sampled { "01" } else { "00" };
        if !req.headers.iter().any(|(h, _)| h.eq_ignore_ascii_case("traceparent")) {
            req.headers.push(("traceparent".into(), format!("00-{}-{span_id}-{flags}", trace.trace_id)));
        }
        if !trace.sampled {
            return None;
        }
        let method = phv_vm::Span::method(&req.method);
        let mut attributes = vec![
            ("http.request.method", phv_vm::SpanValue::Str(method.into())),
            (
                "server.address",
                phv_vm::SpanValue::Str(base.host.trim_start_matches('[').trim_end_matches(']').to_string()),
            ),
            ("server.port", phv_vm::SpanValue::Int(base.port as i64)),
        ];
        if let Some(name) = service {
            attributes.push(("phv.http.service", phv_vm::SpanValue::Str(name.to_string())));
        }
        let now = std::time::SystemTime::now();
        Some(ClientSpan {
            span: phv_vm::Span {
                trace_id: trace.trace_id.clone(),
                span_id,
                parent_id: Some(trace.span_id.clone()),
                name: if method == "_OTHER" { "HTTP".into() } else { method.into() },
                kind: phv_vm::SpanKind::Client,
                start: now,
                end: now,
                attributes,
                error: false,
            },
            spans: trace.spans.clone(),
        })
    }

    fn close(this: Option<ClientSpan>, result: &Result<Response, String>) {
        let Some(ClientSpan { mut span, spans }) = this else { return };
        span.end = std::time::SystemTime::now();
        match result {
            Ok(r) => {
                span.attributes.push(("http.response.status_code", phv_vm::SpanValue::Int(r.status as i64)));
                span.error = r.status >= 400;
                if span.error {
                    span.attributes.push(("error.type", phv_vm::SpanValue::Str(r.status.to_string())));
                }
            }
            Err(_) => {
                span.error = true;
                span.attributes.push(("error.type", phv_vm::SpanValue::Str("network".into())));
            }
        }
        let mut list = spans.lock().unwrap_or_else(|e| e.into_inner());
        if list.len() < MAX_SPANS {
            list.push(span);
        }
    }
}

/// Sends spans to an OpenTelemetry collector (`[telemetry]`): a POST outside
/// any script, at most `timeout`. The status, or a network error.
pub fn post_telemetry(
    url: &str,
    tls: Option<bool>,
    mut headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<u16, String> {
    let base = parse_base(url)?;
    check_plain(&base.host, base.tls, tls, "https://")?;
    let scheme_len = if base.tls { 8 } else { 7 };
    let tail = &url[scheme_len + base.authority.len()..];
    let target = if tail.is_empty() || tail.starts_with('?') { format!("/{tail}") } else { tail.to_string() };
    headers.push(("User-Agent".into(), concat!("phv/", env!("CARGO_PKG_VERSION")).into()));
    headers.push(("Accept-Encoding".into(), "identity".into()));
    let req = Request { method: "POST".into(), target, headers, body };
    exchange(&base, &req, Instant::now() + timeout, 64 << 10).map(|r| r.status)
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
        // `_` counts as `-` in the names PHV sets itself (21st assessment).
        for bad in ["Content_Length", "transfer_encoding", "Proxy_Authorization", "Keep_Alive", "PROXY_CONNECTION"] {
            assert!(check_header_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_header_name("X_Content_Length").is_ok());
        // `.` and runs of `_`, `.` and `-` read as one `-` (re-attack of the 21st assessment).
        for bad in
            ["Content.Length", "Content__Length", "content-.length", "Transfer._Encoding", "Proxy..Authorization"]
        {
            assert!(check_header_name(bad).is_err(), "{bad:?}");
        }
        assert!(same_header("X-Api-Key", "x.api__key") && !same_header("X-Api-Key", "XApiKey"));
        assert_eq!(header_key("X__Api.-Key"), "x-api-key");
        // A message holds the start of a long value, never all of it.
        let long = "A".repeat(1 << 20);
        assert!(check_header_name(&format!("{long} ")).unwrap_err().len() < 200);
        assert!(check_header_value(&long, "a\nb").unwrap_err().len() < 200);
        assert_eq!(brief("short \"x\""), "short \\\"x\\\"");
        assert_eq!(brief(&long).chars().count(), 65);
        for bad in ["Host", "content-length", "Transfer-Encoding", "Bad Name", "X:Y", ""] {
            assert!(check_header_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_header_value("X", "a\r\nInjected: 1").is_err());
        assert!(check_header_value("X", "a\x0bb").is_err());
        assert!(check_header_value("X", "a\tb").is_ok());
        assert!(same_header("X-Api-Key", "x_api_KEY") && !same_header("X-Api-Key", "X-Api-Keys"));
        assert_eq!(query_keys("key=x&a+b=1&c"), ["key", "a b", "c"]);
    }

    /// A fixed query parameter cannot be replaced under a name the server
    /// reads as the same (found by the 1.2.0 audit: PHP read `account.id`
    /// and `account id` as `account_id`).
    #[test]
    fn fixed_query_parameters_cannot_be_replaced_under_another_spelling() {
        let fixed = query_keys("account_id=site1&v=2");
        for same in [
            "account_id",
            "account.id",
            "account id",
            "account[id",
            " account_id",
            "account_id\0x",
            "account_id[]",
            "account_id[a]",
            "account.id[x]",
            "Account_ID",
            "account%5Fid",
            "account%2eid",
            "account%20id",
        ] {
            assert_eq!(replaces_fixed(same, &fixed).map(String::as_str), Some("account_id"), "{same:?}");
        }
        assert_eq!(replaces_fixed("V", &fixed).map(String::as_str), Some("v"));
        // PHP 8 reads what follows an unclosed `[` with `.` and `[` as `_`.
        let fixed = query_keys("a_b_c=1");
        for same in ["a[b.c", "a.b[c", "a[b[c", "a b.c"] {
            assert!(replaces_fixed(same, &fixed).is_some(), "{same:?}");
        }
        // A fixed key written with a dot is read by PHP with `_`.
        assert!(replaces_fixed("a_b", &query_keys("a.b=1")).is_some());
        let fixed = query_keys("account_id=site1");
        for other in ["account_ids", "accountid", "account-id", "id", "x[account_id]", "account", "account_id2[x]"] {
            assert_eq!(replaces_fixed(other, &fixed), None, "{other:?}");
        }
    }

    /// An endpoint a provider names is taken as this machine in every
    /// spelling a connection would reach it by (21st assessment).
    #[test]
    fn endpoints_on_this_machine_are_seen_in_every_spelling() {
        let local = [
            "localhost",
            "LOCALHOST.",
            "127.0.0.1",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "[::ffff:7f00:1]",
            "0.0.0.0",
            "[::]",
        ];
        for h in local {
            assert!(is_this_machine(h), "{h}");
            let url = format!("https://{h}:9000/token");
            assert!(check_discovered_url("https://id.example.com", &url, None).is_err(), "{url}");
        }
        // With a zone, and in the older forms of IPv4 addresses (re-attack of the 21st assessment).
        let more = [
            "[::1%1]",
            "[::1%25]",
            "[::1%25lo]",
            "[::%eth0]",
            "127.1",
            "2130706433",
            "0x7f.0.0.1",
            "0177.0.0.1",
            "0",
            "127.0.1",
        ];
        for h in more {
            assert!(is_this_machine(h), "{h}");
            let url = format!("https://{h}:9000/token");
            assert!(check_discovered_url("https://id.example.com", &url, None).is_err(), "{url}");
        }
        let other = ["id.example.com", "localhost.example.com", "10.0.0.5", "localhost..", "[::ffff:10.0.0.5]"];
        for h in other.into_iter().chain([
            "[fe80::1%1]",
            "10.1",
            "167772161",
            "0x0a.0.0.1",
            "1.2.3.4.5",
            "256.0.0.1",
            "127.0.0.0x",
        ]) {
            assert!(!is_this_machine(h), "{h}");
        }
        assert_eq!(old_ipv4("127.1"), Some([127, 0, 0, 1].into()));
        assert_eq!(old_ipv4("0x7f.1"), Some([127, 0, 0, 1].into()));
        assert_eq!(old_ipv4("10.2.515"), Some([10, 2, 2, 3].into()));
        assert_eq!(old_ipv4("example.com"), None);
        assert!(check_discovered_url("https://id.example.com", "https://id.example.com/token", None).is_ok());
        assert!(check_discovered_url("http://localhost:9000", "http://localhost:9000/token", None).is_ok());
        // An issuer on this machine, however it is written, may name its own endpoints.
        assert!(check_discovered_url("http://localhost.:9000", "http://localhost.:9000/token", Some(false)).is_ok());
        assert!(check_discovered_url("http://127.0.0.1:9000", "http://localhost.:9000/token", Some(false)).is_ok());
        // An endpoint a remote issuer named is looked at again when a request goes to it.
        let named = |host: &str, port: u16| remote_endpoints().contains(&(host.to_string(), port));
        assert!(check_discovered_url("https://id.example.com", "https://Token.Example.com:8443/t", None).is_ok());
        assert!(named("token.example.com", 8443) && !named("localhost", 9000) && !named("localhost.", 9000));
        assert!(local_address("127.0.0.9".parse().unwrap()) && local_address("::ffff:127.0.0.1".parse().unwrap()));
        assert!(local_address("::".parse().unwrap()) && !local_address("192.168.1.5".parse().unwrap()));
    }

    /// A chunk's bytes are moved to the body as they come, also when they
    /// arrive in pieces: the buffer never holds a whole chunk (21st
    /// assessment: a 16 MB chunk was held twice).
    #[test]
    fn a_chunk_goes_to_the_body_as_it_comes() {
        /// Gives the response in pieces of at most 1,000 bytes.
        struct Pieces<'a>(&'a [u8]);
        impl Read for Pieces<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(buf.len()).min(1000);
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let size = 300_000;
        let mut raw = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{size:x}\r\n").into_bytes();
        raw.extend(std::iter::repeat_n(0xffu8, size));
        raw.extend_from_slice(b"\r\n3\r\nabc\r\n0\r\n\r\n");
        let (r, keep) = read_response(&mut Pieces(&raw), false, MAX_BODY).unwrap();
        assert!(keep && r.body.len() == size + 3 && r.body.ends_with(b"\xffabc"));
        // Past the limit: refused at the chunk's size, before its bytes are read.
        assert!(read_response(&mut Pieces(&raw), false, size - 1).unwrap_err().contains("larger than"));
        // A chunk cut in the middle, and one that does not end with a line break.
        let cut = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel";
        assert!(read_response(&mut &cut[..], false, MAX_BODY).is_err());
        let bad = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhelloXX0\r\n\r\n";
        assert!(read_response(&mut &bad[..], false, MAX_BODY).is_err());
        // A body that is not UTF-8 becomes text without failing; one that is, as it is.
        let text =
            |body: &[u8]| match response_value(Response { status: 200, headers: Vec::new(), body: body.to_vec() }) {
                Value::Map(ref m) => m.get(&Key::Str(Rc::new("body".into()))).map(|v| s(v).to_string()),
                _ => None,
            };
        assert_eq!(text(b"caf\xc3\xa9").as_deref(), Some("caf\u{e9}"));
        assert_eq!(text(b"a\xffb").as_deref(), Some("a\u{fffd}b"));
        // The text is made where the body is, within the limit, and taken as it is afterwards.
        let resp = |body: &[u8]| Response { status: 200, headers: Vec::new(), body: body.to_vec() };
        assert_eq!(resp(b"a\xffb").into_text(5).unwrap().body, "a\u{fffd}b".as_bytes());
        assert!(resp(b"a\xffb").into_text(4).unwrap_err().contains("larger than"));
        assert_eq!(
            resp(b"plain").into_text(1).unwrap().body,
            b"plain",
            "UTF-8 is what was read, within the read's limit"
        );
        // A body of unknown size never holds much more than it may take in all.
        let mut body = Vec::new();
        let mut most = 0;
        for _ in 0..3000 {
            make_room(&mut body, 1000, 3_000_000);
            body.extend_from_slice(&[0u8; 1000]);
            most = most.max(body.capacity());
        }
        assert!(body.len() == 3_000_000 && most == 3_000_000, "{most}");
        let mut small = Vec::new();
        make_room(&mut small, 300_000, 16 << 20);
        small.extend_from_slice(&[0u8; 300_000]);
        assert_eq!(small.capacity(), 16 << 20);
        settle(&mut small);
        assert!(small.capacity() < 400_000);
        // As `String::from_utf8_lossy` reads it, in a string of exactly that size.
        for body in [&b"\xff\xfe"[..], b"a\xe3\x81", b"\xf0\x9f\x98x\xc3", b"ok", b"", b"\xed\xa0\x80z"] {
            let text = body_text(body.to_vec());
            assert_eq!(text, String::from_utf8_lossy(body));
            assert_eq!(text.capacity(), text.len(), "{body:?}");
        }
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
