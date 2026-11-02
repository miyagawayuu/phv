//! HTTP services named in phv.toml (`[http.<name>]`). A script names a
//! service and gives a path (`HttpClient::connect("weather")->get("/today")`);
//! the base URL and the headers taken from environment variables (API keys)
//! stay in the configuration, so they never appear in code or messages, and
//! scripts cannot send requests to any other host.
//!
//! Each request uses its own HTTP/1.1 connection (`Connection: close`).
//! Redirects are not followed: a 3xx response is returned to the script.

use crate::netio::{check_plain, dial, until};
use crate::{map, s};
use indexmap::IndexMap;
use phv_vm::{Key, Value, Vm, VmError};
use std::io::{ErrorKind, Read, Write};
use std::rc::Rc;
use std::time::Duration;

type R = Result<Value, VmError>;

/// Longest wait for a response when the script has no time limit.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest request body a script may send, and largest response body accepted.
const MAX_BODY: usize = 16 << 20;
/// Largest response head (status line and headers).
const MAX_HEAD: usize = 64 << 10;
const MAX_HEADERS: usize = 100;

const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

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

fn check_header_value(name: &str, value: &str) -> Result<(), String> {
    if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return Err(format!("the value of the {name} header contains a line break"));
    }
    Ok(())
}

/// Checks the path a script asks for: it is added to the base URL's path, so
/// it must start with `/` and cannot leave that path (`..`) or name another
/// host (`//host`).
fn check_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err("the path must start with a single `/`".into());
    }
    if path.contains(['?', '#']) {
        return Err("put the query in the `query` argument, not in the path".into());
    }
    if path.chars().any(|c| c.is_control() || c == ' ' || c == '\\') {
        return Err("the path contains spaces, backslashes or control characters".into());
    }
    for segment in path.split('/') {
        let decoded = percent_encoding::percent_decode_str(segment).decode_utf8_lossy();
        if decoded == "." || decoded == ".." || decoded.contains(['/', '\\']) {
            return Err("the path cannot contain `.` or `..` segments or encoded slashes".into());
        }
    }
    Ok(())
}

/// A request a script asked for, checked against its service.
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
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

fn read_response(stream: &mut impl Read, head_only: bool) -> Result<Response, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 16 << 10];
    loop {
        // Interim (1xx) responses are skipped.
        let (status, headers, used) = loop {
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
                    break (status, headers, used);
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
            continue;
        }
        let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        let no_body = head_only || status == 204 || status == 304;
        let body = if no_body {
            Vec::new()
        } else if header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
            read_chunked(stream, buf, &mut chunk)?
        } else if let Some(len) = header("content-length") {
            let len: usize = len.parse().map_err(|_| "the response has an invalid Content-Length")?;
            if len > MAX_BODY {
                return Err(too_large());
            }
            while buf.len() < len {
                let n = read_some(stream, &mut chunk)?;
                if n == 0 {
                    return Err("the server closed the connection in the middle of the response".into());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            buf.truncate(len);
            buf
        } else {
            loop {
                if buf.len() > MAX_BODY {
                    return Err(too_large());
                }
                let n = read_some(stream, &mut chunk)?;
                if n == 0 {
                    break buf;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        };
        return Ok(Response { status, headers, body });
    }
}

fn too_large() -> String {
    format!("the response is larger than {} MB", MAX_BODY >> 20)
}

fn read_some(stream: &mut impl Read, chunk: &mut [u8]) -> Result<usize, String> {
    loop {
        match stream.read(chunk) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Err("timed out".into()),
            // Servers that close a TLS connection without close_notify: the data so far counts.
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(0),
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn read_chunked(stream: &mut impl Read, mut buf: Vec<u8>, chunk: &mut [u8]) -> Result<Vec<u8>, String> {
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
        let line = String::from_utf8_lossy(&buf[..line_end]).to_string();
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| "the server sent an invalid chunk")?;
        buf.drain(..line_end + 2);
        if size == 0 {
            // Trailers are ignored.
            return Ok(body);
        }
        if body.len() + size > MAX_BODY {
            return Err(too_large());
        }
        while buf.len() < size + 2 {
            fill(&mut buf)?;
        }
        body.extend_from_slice(&buf[..size]);
        buf.drain(..size + 2);
    }
}

/// Sends `req` to `base` and reads the answer.
fn exchange(base: &Base, req: &Request, until: std::time::Instant) -> Result<Response, String> {
    let mut stream = dial(&base.host, base.port, base.tls, until)?;
    let mut head = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", req.method, req.target, base.authority);
    for (name, value) in &req.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let sends_body = !req.body.is_empty() || matches!(req.method.as_str(), "POST" | "PUT" | "PATCH");
    if sends_body {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    let io = |e: std::io::Error| {
        if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
            "timed out".to_string()
        } else {
            e.to_string()
        }
    };
    stream.write_all(head.as_bytes()).map_err(io)?;
    stream.write_all(&req.body).map_err(io)?;
    stream.flush().map_err(io)?;
    read_response(&mut stream, req.method == "HEAD")
}

/// `__http_request(name, method, path, query, headers, body)`: sends a request
/// to `[http.<name>]` and returns `["status" => int, "headers" => map, "body" => string]`.
pub fn request(vm: &mut Vm, a: &[Value]) -> R {
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
    let path = s(&a[2]).to_string();
    if let Err(e) = check_path(&path) {
        return Err(vm.throw("ValueError", e));
    }
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
        if spec.headers_env.iter().any(|(n, _)| n.eq_ignore_ascii_case(&h)) {
            return Err(vm.throw("ValueError", format!("the {h} header comes from phv.toml and cannot be replaced")));
        }
        headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&h));
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
    let query = encode_query(&str_pairs(&a[3]));
    let mut target = format!("{}{path}", base.path);
    match (base.query.is_empty(), query.is_empty()) {
        (true, true) => {}
        (false, true) => target.push_str(&format!("?{}", base.query)),
        (true, false) => target.push_str(&format!("?{query}")),
        (false, false) => target.push_str(&format!("?{}&{query}", base.query)),
    }
    let req = Request { method, target, headers, body };
    let result = exchange(&base, &req, until(vm, REQUEST_TIMEOUT));
    vm.check_limits()?;
    let resp = match result {
        Ok(r) => r,
        Err(e) => return Err(fail(vm, format!("{} {path}: {e}", req.method))),
    };
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
    Ok(Value::map(out))
}

pub fn lookup(name: &str) -> Option<phv_vm::BuiltinFn> {
    let f: phv_vm::BuiltinFn = match name {
        "__http_request" => request,
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
        assert!(check_path("/v1/items/42").is_ok());
        assert!(check_path("/search%20results").is_ok());
        for bad in [
            "items",
            "//evil.example/x",
            "/a/../b",
            "/a/%2e%2E/b",
            "/a/%2F/b",
            "/a?x=1",
            "/a#b",
            "/a b",
            "/a\\b",
            "/a\r\nX: y",
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
    }

    #[test]
    fn reads_chunked_and_sized_bodies() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX-A: 1\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n";
        let r = read_response(&mut &raw[..], false).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hello world"[..]));
        assert!(r.headers.contains(&("x-a".into(), "1".into())));
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\n\r\nabcdef";
        let r = read_response(&mut &raw[..], false).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (404, &b"abc"[..]));
        let raw = b"HTTP/1.1 200 OK\r\n\r\nuntil the end";
        assert_eq!(read_response(&mut &raw[..], false).unwrap().body, b"until the end");
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort";
        assert!(read_response(&mut &raw[..], false).is_err());
        let raw = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert!(read_response(&mut raw.as_bytes(), false).is_err());
    }
}
