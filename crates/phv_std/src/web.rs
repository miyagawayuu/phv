//! JSON decoding, cookies, sessions, CSRF tokens and uploaded files.

use crate::{int, s};
use indexmap::IndexMap;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use phv_vm::{BuiltinFn, Key, Value, Vm, VmError};
use std::rc::Rc;

type R = Result<Value, VmError>;

/// Largest total size of one session's values.
const MAX_SESSION_BYTES: usize = 64 * 1024;

// ----- JSON ---------------------------------------------------------------

struct Json<'a> {
    s: &'a [u8],
    i: usize,
    /// The run whose memory and time limits apply while values are built
    /// (a few megabytes of `[[],[],...]` make hundreds of megabytes of values).
    vm: Option<&'a Vm>,
    values: usize,
    /// The limit that stopped the parse, if one did.
    stopped: Option<VmError>,
}

/// Values built between two checks of the run's limits.
const JSON_CHECK_EVERY: usize = 4096;

impl Json<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn err<T>(&self, msg: &str) -> Result<T, String> {
        Err(format!("{msg} at offset {}", self.i))
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > 128 {
            return self.err("JSON is nested too deeply");
        }
        self.values += 1;
        if self.values.is_multiple_of(JSON_CHECK_EVERY) {
            if let Some(Err(e)) = self.vm.map(Vm::check_limits) {
                self.stopped = Some(e);
                return Err(String::new());
            }
        }
        self.ws();
        match self.s.get(self.i) {
            None => self.err("unexpected end of JSON"),
            Some(b'{') => {
                self.i += 1;
                let mut m = IndexMap::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Value::map(m));
                }
                loop {
                    self.ws();
                    if self.s.get(self.i) != Some(&b'"') {
                        return self.err("expected a string key");
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.s.get(self.i) != Some(&b':') {
                        return self.err("expected `:`");
                    }
                    self.i += 1;
                    let v = self.value(depth + 1)?;
                    m.insert(Key::Str(Rc::new(k)), v);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Value::map(m));
                        }
                        _ => return self.err("expected `,` or `}`"),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Value::list(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::list(items));
                        }
                        _ => return self.err("expected `,` or `]`"),
                    }
                }
            }
            Some(b'"') => Ok(Value::str(self.string()?)),
            Some(b't') if self.s[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ok(Value::Bool(true))
            }
            Some(b'f') if self.s[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ok(Value::Bool(false))
            }
            Some(b'n') if self.s[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ok(Value::Null)
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => self.err("unexpected character"),
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        if self.s[self.i] == b'-' {
            self.i += 1;
        }
        let mut float = false;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'0'..=b'9' => {}
                b'.' | b'e' | b'E' | b'+' | b'-' => float = true,
                _ => break,
            }
            self.i += 1;
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
        if !float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Int(i));
            }
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Value::Float(f)),
            _ => self.err("invalid number"),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.s.get(self.i..self.i + 4).filter(|b| b.iter().all(u8::is_ascii_hexdigit));
        let v = h.and_then(|b| std::str::from_utf8(b).ok()).and_then(|h| u32::from_str_radix(h, 16).ok());
        match v {
            Some(v) => {
                self.i += 4;
                Ok(v)
            }
            None => self.err("invalid \\u escape"),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.s.len() && !matches!(self.s[self.i], b'"' | b'\\') && self.s[self.i] >= 0x20 {
                self.i += 1;
            }
            match std::str::from_utf8(&self.s[start..self.i]) {
                Ok(t) => out.push_str(t),
                Err(_) => return self.err("invalid UTF-8 in string"),
            }
            match self.s.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = self.s.get(self.i).copied();
                    self.i += 1;
                    match c {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{8}'),
                        Some(b'f') => out.push('\u{c}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            let hi = self.hex4()?;
                            let mut cp = hi;
                            if (0xD800..0xDC00).contains(&hi) && self.s[self.i..].starts_with(b"\\u") {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    self.i = save; // not a pair: decode it separately
                                }
                            }
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        _ => return self.err("invalid escape"),
                    }
                }
                Some(_) => return self.err("control character in string"),
                None => return self.err("unterminated string"),
            }
        }
    }
}

pub fn json_decode(text: &str) -> Result<Value, String> {
    let mut p = Json { s: text.as_bytes(), i: 0, vm: None, values: 0, stopped: None };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return p.err("unexpected text after JSON value");
    }
    Ok(v)
}

/// `json_decode` for a script: the run's memory and time limits are checked as
/// the values are built, not only once they all are. The outer error is the
/// limit; the inner one, text that is not JSON (for a `ValueError`).
pub fn json_decode_in(vm: &Vm, text: &str) -> Result<Result<Value, String>, VmError> {
    let mut p = Json { s: text.as_bytes(), i: 0, vm: Some(vm), values: 0, stopped: None };
    let v = p.value(0);
    if let Some(e) = p.stopped.take() {
        return Err(e);
    }
    let v = match v {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    p.ws();
    if p.i != p.s.len() {
        return Ok(p.err("unexpected text after JSON value"));
    }
    Ok(Ok(v))
}

// ----- cookies --------------------------------------------------------------

/// Characters not allowed raw in a cookie value (RFC 6265 cookie-octet).
const COOKIE_VALUE: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b',').add(b';').add(b'\\').add(b'%');

fn is_token(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$&'*+-.^_`|~".contains(&b))
}

fn cookies(vm: &Vm) -> IndexMap<Key, Value> {
    let mut m = IndexMap::new();
    for (k, v) in &vm.http.headers {
        if k != "cookie" {
            continue;
        }
        for part in v.split(';') {
            if let Some((name, value)) = part.trim().split_once('=') {
                let value = percent_decode_str(value.trim().trim_matches('"')).decode_utf8_lossy().into_owned();
                m.entry(Key::Str(Rc::new(name.trim().to_string()))).or_insert(Value::str(value));
            }
        }
    }
    m
}

fn set_cookie(vm: &mut Vm, a: &[Value]) -> R {
    unsent(vm, "cookies")?;
    let (name, value, max_age, path) = (s(&a[0]), s(&a[1]), int(&a[2]), s(&a[3]));
    let (secure, http_only, same_site) =
        (matches!(a[4], Value::Bool(true)), matches!(a[5], Value::Bool(true)), s(&a[6]));
    if !is_token(name) {
        return Err(vm.throw("ValueError", format!("invalid cookie name \"{name}\"")));
    }
    if path.bytes().any(|b| b < 0x20 || b == b';' || b == 0x7f) {
        return Err(vm.throw("ValueError", "invalid cookie path"));
    }
    let same_site = match same_site.to_ascii_lowercase().as_str() {
        "lax" => "Lax",
        "strict" => "Strict",
        "none" => {
            if !secure {
                return Err(vm.throw("ValueError", "SameSite=None cookies must also be secure"));
            }
            "None"
        }
        _ => return Err(vm.throw("ValueError", "sameSite must be \"Lax\", \"Strict\" or \"None\"")),
    };
    let mut header = format!("{name}={}; Path={path}; SameSite={same_site}", utf8_percent_encode(value, COOKIE_VALUE));
    if max_age != 0 {
        header.push_str(&format!("; Max-Age={}", max_age.max(0)));
    }
    if secure {
        header.push_str("; Secure");
    }
    if http_only {
        header.push_str("; HttpOnly");
    }
    vm.http.resp_headers.push(("Set-Cookie".into(), header));
    Ok(Value::Null)
}

// ----- sessions and CSRF ------------------------------------------------------

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("operating system random number generator failed");
    hex_of(&buf)
}

/// `bytes` as lowercase hex (every page makes a few ids this way).
pub fn hex_of(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

/// Refuses a change to what flush() has already sent: the status, headers,
/// cookies and the session (whose cookie went with them).
pub(crate) fn unsent(vm: &Vm, what: &str) -> Result<(), VmError> {
    if vm.http.flushed {
        return Err(vm.throw("ValueError", format!("{what} cannot change after response()->flush() sent the response")));
    }
    Ok(())
}

pub(crate) fn session_set(vm: &mut Vm, k: &str, v: &str) -> R {
    unsent(vm, "the session")?;
    let sess = &mut vm.http.session;
    let current: usize = sess.values.iter().filter(|(key, _)| key.as_str() != k).map(|(a, b)| a.len() + b.len()).sum();
    if current + k.len() + v.len() > MAX_SESSION_BYTES {
        return Err(vm.throw("ValueError", "session data is limited to 64 KB"));
    }
    let sess = &mut vm.http.session;
    sess.values.insert(k.to_string(), v.to_string());
    sess.changed = true;
    sess.destroyed = false;
    Ok(Value::Null)
}

/// The CSRF token for a browser's CSRF cookie: HMAC-SHA256(secret, cookie).
/// Tokens need no server-side state, so issuing them cannot fill memory.
pub fn csrf_expected(secret: &[u8], cookie: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut m = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    m.update(b"phv-csrf:");
    m.update(cookie.as_bytes());
    m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A CSRF token as written into a page: a fresh random pad, then the token
/// XORed with it. The text differs on every response, so a page compressed by
/// any proxy or CDN on the way does not give the token away through its size
/// (BREACH), while the server can still recover the token (`csrf_matches`).
pub fn mask_csrf(token: &str) -> String {
    let t = from_hex(token).unwrap_or_default();
    let mut pad = vec![0u8; t.len()];
    getrandom::fill(&mut pad).expect("operating system random number generator failed");
    let masked: Vec<u8> = t.iter().zip(&pad).map(|(a, b)| a ^ b).collect();
    format!("{}{}", to_hex(&pad), to_hex(&masked))
}

/// Whether a submitted token is `expected`: masked (`mask_csrf`), or written
/// as it is (pages served before tokens were masked).
pub fn csrf_matches(expected: &str, submitted: &str) -> bool {
    let plain = if submitted.len() == expected.len() * 2 {
        // Decoded whole first: a value with other characters is refused, never split mid-character.
        let Some(bytes) = from_hex(submitted) else { return false };
        let (pad, masked) = bytes.split_at(bytes.len() / 2);
        to_hex(&pad.iter().zip(masked).map(|(a, b)| a ^ b).collect::<Vec<u8>>())
    } else {
        submitted.to_string()
    };
    crate::constant_time_eq(expected.as_bytes(), plain.as_bytes())
}

pub fn csrf_token(vm: &mut Vm) -> Result<String, VmError> {
    // A token ties the page to the visitor: one whose head flush() sent without
    // `Cache-Control: private, no-store` could be kept by a shared cache (found by
    // the 1.2 audit). The server marks a streamed head personal when the request
    // carries a cookie, which a token after flush() needs anyway.
    if vm.http.flushed && !vm.http.personalized {
        return Err(vm.throw(
            "ValueError",
            "call csrf_token() or csrf_field() before response()->flush(): the response was sent as one that shared caches may keep",
        ));
    }
    vm.http.personalized = true;
    let cookie = match (&vm.http.new_csrf_cookie, &vm.http.csrf_cookie) {
        (Some(c), _) | (None, Some(c)) => c.clone(),
        (None, None) => {
            // The token needs a cookie, which can no longer be sent.
            if vm.http.flushed {
                return Err(vm.throw("ValueError", "call csrf_token() or csrf_field() before response()->flush()"));
            }
            let c = random_hex(32);
            vm.http.new_csrf_cookie = Some(c.clone());
            c
        }
    };
    Ok(mask_csrf(&csrf_expected(&vm.http.csrf_secret, &cookie)))
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "json_decode" => {
            |vm, a| json_decode_in(vm, s(&a[0]))?.map_err(|e| vm.throw("ValueError", format!("json_decode(): {e}")))
        }
        "__req_cookies" => |vm, _| Ok(Value::map(cookies(vm))),
        "__resp_cookie" => set_cookie,
        "__sess_get" => {
            |vm, a| Ok(vm.http.session.values.get(s(&a[0])).map(|v| Value::str(v.as_str())).unwrap_or(Value::Null))
        }
        "__sess_set" => |vm, a| {
            let (k, v) = (s(&a[0]).to_string(), s(&a[1]).to_string());
            session_set(vm, &k, &v)
        },
        "__sess_remove" => |vm, a| {
            unsent(vm, "the session")?;
            let sess = &mut vm.http.session;
            if sess.values.shift_remove(s(&a[0])).is_some() {
                sess.changed = true;
            }
            Ok(Value::Null)
        },
        "__sess_all" => |vm, _| {
            let mut m = IndexMap::new();
            for (k, v) in &vm.http.session.values {
                m.insert(Key::Str(Rc::new(k.clone())), Value::str(v.as_str()));
            }
            Ok(Value::map(m))
        },
        "__sess_regenerate" => |vm, _| {
            unsent(vm, "the session")?;
            vm.http.session.regenerate = true;
            vm.http.session.changed = true;
            // Tokens issued before login stop working after it.
            vm.http.new_csrf_cookie = Some(random_hex(32));
            Ok(Value::Null)
        },
        "__sess_destroy" => |vm, _| {
            unsent(vm, "the session")?;
            let sess = &mut vm.http.session;
            sess.values.clear();
            sess.destroyed = true;
            sess.changed = true;
            Ok(Value::Null)
        },
        "csrf_token" => |vm, _| csrf_token(vm).map(Value::str),
        "csrf_field" => |vm, _| {
            let t = csrf_token(vm)?;
            Ok(Value::html(format!("<input type=\"hidden\" name=\"_csrf\" value=\"{t}\">")))
        },
        "__req_files" => |vm, _| {
            let mut out = Vec::new();
            for (i, f) in vm.http.files.iter().enumerate() {
                let mut m = IndexMap::new();
                let mut put = |k: &str, v: String| {
                    m.insert(Key::Str(Rc::new(k.to_string())), Value::str(v));
                };
                put("index", i.to_string());
                put("field", f.field.clone());
                put("name", f.filename.clone());
                put("type", f.content_type.clone());
                put("size", f.data.len().to_string());
                out.push(Value::map(m));
            }
            Ok(Value::list(out))
        },
        "__upload_text" => |vm, a| {
            let i = int(&a[0]) as usize;
            Ok(Value::str(
                vm.http.files.get(i).map(|f| String::from_utf8_lossy(&f.data).into_owned()).unwrap_or_default(),
            ))
        },
        "__upload_save" => |vm, a| {
            let i = int(&a[0]) as usize;
            let rel = s(&a[1]).to_string();
            let (dir, name) = crate::sys::resolve_path(vm, &rel, true)?;
            let Some(f) = vm.http.files.get(i) else { return Ok(Value::Null) };
            let mut out = dir.open(&name, true).map_err(|e| crate::sys::fs_error(vm, &rel, "", e))?;
            std::io::Write::write_all(&mut out, &f.data)
                .map_err(|e| vm.throw("RuntimeException", format!("cannot save upload: {}", e.kind())))?;
            Ok(Value::Null)
        },
        _ => return None,
    };
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_json() {
        let v = json_decode(r#"{"a": [1, 2.5, "xé😀", true, null], "b": {}}"#).unwrap();
        let Value::Map(m) = &v else { panic!() };
        assert_eq!(m.len(), 2);
        assert!(json_decode("[1,]").is_err());
        assert!(json_decode("{\"a\":1} x").is_err());
        assert!(json_decode(&"[".repeat(1000)).is_err());
        assert!(matches!(json_decode("12345678901234567890").unwrap(), Value::Float(_)));
    }

    /// The hex table gives what formatting each byte gave.
    #[test]
    fn hex_matches_formatting() {
        let bytes: Vec<u8> = (0..=255).collect();
        let formatted: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex_of(&bytes), formatted);
        assert_eq!(hex_of(&[]), "");
        assert_eq!(random_hex(16).len(), 32);
        assert!(random_hex(16).bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }
}

#[cfg(test)]
mod csrf_tests {
    use super::*;

    #[test]
    fn masked_tokens_differ_and_verify() {
        let token = csrf_expected(b"secret", "cookie");
        let (a, b) = (mask_csrf(&token), mask_csrf(&token));
        assert_ne!(a, b, "a fresh pad on every page");
        assert_eq!(a.len(), 128);
        assert!(!a.contains(&token));
        assert!(csrf_matches(&token, &a) && csrf_matches(&token, &b));
        assert!(csrf_matches(&token, &token), "plain tokens from older pages still work");
        let other = mask_csrf(&csrf_expected(b"secret", "other"));
        assert!(!csrf_matches(&token, &other));
        let mut flipped = a.clone().into_bytes();
        flipped[100] = if flipped[100] == b'0' { b'1' } else { b'0' };
        assert!(!csrf_matches(&token, &String::from_utf8(flipped).unwrap()));
        // 128 bytes with multi-byte characters: refused, not split mid-character.
        assert!(!csrf_matches(&token, &"é".repeat(64)));
        assert!(!csrf_matches(&token, ""));
    }
}
