//! Caching and partial requests for static files, and gzip for text responses.
//!
//! Static files carry an `ETag` and `Last-Modified`, answer `If-None-Match` /
//! `If-Modified-Since` with 304, and serve one `Range` of bytes (206) for media
//! players and resumed downloads. Text responses are compressed with gzip
//! when the client accepts it, except personalized ones (see `compress`).

use crate::body::RespBody;
use bytes::Bytes;
use hyper::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use hyper::{Response, StatusCode};
use std::io::Write;
use std::time::SystemTime;

type Resp = Response<RespBody>;

/// Bodies smaller than this are sent as they are; gzip would gain little.
const MIN_COMPRESS: usize = 1024;
/// Bodies larger than this are sent as they are (compressing them would hold
/// them in memory and take a worker's time).
pub const MAX_COMPRESS: usize = 8 << 20;

/// What identifies a version of a file.
pub struct Validators {
    pub etag: String,
    pub modified: Option<SystemTime>,
}

impl Validators {
    /// From a file's size and modification time.
    pub fn of_file(len: u64, modified: Option<SystemTime>) -> Validators {
        let stamp = modified.and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        Validators { etag: format!("\"{len:x}-{stamp:x}\""), modified }
    }

    /// From the contents (files of a built site have no useful time).
    pub fn of_bytes(data: &[u8]) -> Validators {
        use std::hash::{Hash, Hasher};
        // `DefaultHasher::new()` uses fixed keys, so the tag is the same in every process.
        let mut h = std::hash::DefaultHasher::new();
        data.hash(&mut h);
        Validators { etag: format!("\"{:x}-{:x}\"", data.len(), h.finish()), modified: None }
    }

    /// Whether the client's copy is current (`If-None-Match`, else `If-Modified-Since`).
    pub fn not_modified(&self, req: &HeaderMap) -> bool {
        if let Some(inm) = req.get("if-none-match").and_then(|v| v.to_str().ok()) {
            return inm
                .split(',')
                .map(str::trim)
                .any(|t| t == "*" || weak(t) == weak(&self.etag) || weak(t) == weak(&gzip_etag(&self.etag)));
        }
        match (req.get("if-modified-since").and_then(|v| v.to_str().ok()), self.modified) {
            (Some(ims), Some(m)) => httpdate::parse_http_date(ims).is_ok_and(|since| {
                // HTTP dates have whole seconds.
                let m = m.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let s = since.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                m <= s
            }),
            _ => false,
        }
    }

    /// Adds `ETag`, `Last-Modified` and `Cache-Control`.
    pub fn headers(&self, h: &mut HeaderMap, max_age: Option<u64>) {
        if let Ok(v) = HeaderValue::from_str(&self.etag) {
            h.insert("etag", v);
        }
        if let Some(m) = self.modified {
            if let Ok(v) = HeaderValue::from_str(&httpdate::fmt_http_date(m)) {
                h.insert("last-modified", v);
            }
        }
        let cc = match max_age {
            Some(a) => format!("public, max-age={a}"),
            None => "no-cache".into(),
        };
        if let Ok(v) = HeaderValue::from_str(&cc) {
            h.insert("cache-control", v);
        }
    }

    /// Whether `If-Range` (when sent) names this version, so a range may be served.
    pub fn range_applies(&self, req: &HeaderMap) -> bool {
        match req.get("if-range").and_then(|v| v.to_str().ok()) {
            None => true,
            Some(v) if v.starts_with('"') => v == self.etag,
            Some(v) => match (httpdate::parse_http_date(v), self.modified) {
                (Ok(d), Some(m)) => {
                    let secs = |t: SystemTime| t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                    secs(d) == secs(m)
                }
                _ => false,
            },
        }
    }
}

fn weak(tag: &str) -> &str {
    tag.strip_prefix("W/").unwrap_or(tag)
}

/// The tag of the gzip-compressed version of a file.
pub fn gzip_etag(etag: &str) -> String {
    match etag.strip_suffix('"') {
        Some(t) => format!("{t}-gzip\""),
        None => etag.to_string(),
    }
}

/// The byte range a `Range` header asks for, inclusive: `None` when there is
/// no usable header (the whole file is sent), `Some(Err(()))` when it cannot be
/// satisfied (416). Only one range is served; a list of them gets the whole file.
pub fn range(req: &HeaderMap, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = req.get("range")?.to_str().ok()?.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let r = if a.is_empty() {
        // The last N bytes.
        let n: u64 = b.parse().ok()?;
        if n == 0 || len == 0 {
            return Some(Err(()));
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() { len.saturating_sub(1) } else { b.parse::<u64>().ok()?.min(len.saturating_sub(1)) };
        if start >= len || end < start {
            return Some(Err(()));
        }
        (start, end)
    };
    Some(Ok(r))
}

/// 416 for a range past the end.
pub fn unsatisfiable(len: u64) -> Resp {
    let mut r = Response::new(RespBody::full(Bytes::new()));
    *r.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    if let Ok(v) = HeaderValue::from_str(&format!("bytes */{len}")) {
        r.headers_mut().insert("content-range", v);
    }
    r
}

/// Whether a content type is worth compressing (text; images and media are compressed already).
pub fn compressible(content_type: &str) -> bool {
    let ct = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    ct.starts_with("text/")
        || matches!(
            ct.as_str(),
            "application/json" | "application/javascript" | "application/xml" | "image/svg+xml" | "application/wasm"
        )
        || ct.ends_with("+json")
        || ct.ends_with("+xml")
}

/// Whether the client accepts gzip (`Accept-Encoding: gzip`, not `gzip;q=0`).
pub fn accepts_gzip(req: &HeaderMap) -> bool {
    req.get_all("accept-encoding").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(|e| {
        let mut parts = e.split(';');
        let name = parts.next().unwrap_or("").trim();
        let refused = parts.any(|p| p.trim().strip_prefix("q=").is_some_and(|q| q.trim().parse::<f32>() == Ok(0.0)));
        (name.eq_ignore_ascii_case("gzip") || name == "*") && !refused
    })
}

/// Whether a response may be compressed. Responses for one visitor (they set
/// cookies, use the session or carry a CSRF token) are not: compressing a
/// secret next to text an attacker chooses would leak it through the size
/// (BREACH).
pub fn may_compress(resp: &Resp) -> bool {
    let h = resp.headers();
    let personal = h.contains_key("set-cookie")
        || h.get("cache-control")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("private") || v.contains("no-store"));
    resp.status() == StatusCode::OK
        && !personal
        && !h.contains_key("content-encoding")
        && !h.contains_key("content-range")
        && h.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(compressible)
}

/// gzip of `data`, or `None` when it is not worth it.
pub fn gzip(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < MIN_COMPRESS || data.len() > MAX_COMPRESS {
        return None;
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(data.len() / 3), flate2::Compression::default());
    enc.write_all(data).ok()?;
    let out = enc.finish().ok()?;
    (out.len() < data.len()).then_some(out)
}

/// Marks a response as compressed (and as varying with `Accept-Encoding`).
pub fn mark_gzip(h: &mut HeaderMap) {
    h.insert("content-encoding", HeaderValue::from_static("gzip"));
    if let Some(tag) = h.get("etag").and_then(|v| v.to_str().ok()).map(gzip_etag) {
        if let Ok(v) = HeaderValue::from_str(&tag) {
            h.insert("etag", v);
        }
    }
    h.remove("accept-ranges");
}

/// Adds `Accept-Encoding` to `Vary`.
pub fn vary(h: &mut HeaderMap, name: &'static str) {
    let current = h.get("vary").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if current.split(',').any(|v| v.trim().eq_ignore_ascii_case(name)) {
        return;
    }
    let value = if current.is_empty() { name.to_string() } else { format!("{current}, {name}") };
    if let Ok(v) = HeaderValue::from_str(&value) {
        h.insert("vary", v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn ranges() {
        let r = |v: &str| range(&headers(&[("range", v)]), 100);
        assert_eq!(r("bytes=0-9"), Some(Ok((0, 9))));
        assert_eq!(r("bytes=90-"), Some(Ok((90, 99))));
        assert_eq!(r("bytes=-10"), Some(Ok((90, 99))));
        assert_eq!(r("bytes=50-1000"), Some(Ok((50, 99))));
        assert_eq!(r("bytes=100-"), Some(Err(())));
        assert_eq!(r("bytes=9-3"), Some(Err(())));
        assert_eq!(r("bytes=0-1,5-6"), None);
        assert_eq!(r("items=0-1"), None);
        assert_eq!(r("bytes=x-1"), None);
    }

    #[test]
    fn gzip_negotiation() {
        assert!(accepts_gzip(&headers(&[("accept-encoding", "gzip, deflate, br")])));
        assert!(accepts_gzip(&headers(&[("accept-encoding", "br;q=1.0, gzip;q=0.8")])));
        assert!(!accepts_gzip(&headers(&[("accept-encoding", "gzip;q=0")])));
        assert!(!accepts_gzip(&headers(&[("accept-encoding", "br")])));
        assert!(compressible("text/html; charset=utf-8") && compressible("application/json"));
        assert!(!compressible("image/png"));
    }

    #[test]
    fn conditional_requests() {
        let v = Validators::of_bytes(b"hello");
        assert!(v.not_modified(&headers(&[("if-none-match", &v.etag)])));
        assert!(v.not_modified(&headers(&[("if-none-match", &format!("W/{}", v.etag))])));
        assert!(v.not_modified(&headers(&[("if-none-match", &gzip_etag(&v.etag))])));
        assert!(!v.not_modified(&headers(&[("if-none-match", "\"other\"")])));
    }
}
