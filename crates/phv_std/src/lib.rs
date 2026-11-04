//! Built-in function implementations and the standard library sources.
//!
//! The type signatures for these functions live in `phv_types::builtins`.

use indexmap::IndexMap;
use phv_vm::value::{compare, format_float, html_escape, values_equal};
use phv_vm::{BuiltinFn, Key, Map, Value, Vm, VmError};
use std::collections::HashSet;
use std::fmt::Write;
use std::rc::Rc;

mod asyncio;
mod db;
mod httpclient;
mod mail;
pub mod migrate;
mod netdb;
mod netio;
mod pgmeter;
pub mod ratelimit;
mod redisclient;
pub mod safefs;
mod sys;
mod twofactor;
mod web;
mod websocket;

pub use httpclient::{check_header_name, check_header_value, check_http_url};
pub use netdb::{
    session_insert, session_read, session_remove, session_sweep, session_update, SessionLimits, SessionRow,
};
pub use sys::{constant_time_eq, sha256_hex};
pub use web::{csrf_expected, csrf_matches, json_decode, mask_csrf, random_hex};

pub const PRELUDE: &str = include_str!("phv/prelude.phv");

/// Source of a `std.*` module.
/// Whether `Name <address>` or `address` names a plain email address (for phv.toml).
pub fn mail_address(s: &str) -> Option<&str> {
    mail::address_of(s)
}

pub fn std_module(name: &str) -> Option<&'static str> {
    match name {
        "std.http" => Some(include_str!("phv/http.phv")),
        "std.db" => Some(include_str!("phv/db.phv")),
        "std.net" => Some(include_str!("phv/net.phv")),
        "std.async" => Some(include_str!("phv/async.phv")),
        "std.validate" => Some(include_str!("phv/validate.phv")),
        "std.testing" => Some(include_str!("phv/testing.phv")),
        "std.mail" => Some(include_str!("phv/mail.phv")),
        _ => None,
    }
}

/// Resolves the builtin names a program uses to function pointers.
pub fn link(names: &[String]) -> Result<Vec<BuiltinFn>, String> {
    names.iter().map(|n| lookup(n).ok_or_else(|| format!("missing builtin `{n}`"))).collect()
}

type R = Result<Value, VmError>;

fn s(v: &Value) -> &str {
    match v {
        Value::Str(s) | Value::Html(s) => s,
        v => panic!("builtin expected string, got {}", v.type_name()),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        v => panic!("builtin expected int, got {}", v.type_name()),
    }
}

/// How long a shared rate limit waits for its store: at most 2 seconds, and
/// never past the script's deadline.
fn shared_until(vm: &Vm) -> std::time::Instant {
    let cap = std::time::Instant::now() + std::time::Duration::from_secs(2);
    vm.deadline().map_or(cap, |d| d.min(cap))
}

fn float(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        Value::Int(i) => *i as f64,
        v => panic!("builtin expected float, got {}", v.type_name()),
    }
}

fn list(v: &Value) -> &Rc<Vec<Value>> {
    match v {
        Value::List(l) => l,
        v => panic!("builtin expected list, got {}", v.type_name()),
    }
}

fn map(v: &Value) -> &Rc<Map> {
    match v {
        Value::Map(m) => m,
        v => panic!("builtin expected map, got {}", v.type_name()),
    }
}

fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> Value) -> Value {
    v.map(f).unwrap_or(Value::Null)
}

fn string_map(pairs: &[(String, String)]) -> Value {
    let mut m = IndexMap::new();
    for (k, v) in pairs {
        m.insert(Key::Str(Rc::new(k.clone())), Value::str(v.as_str()));
    }
    Value::map(m)
}

/// Every value sent under `name`, in order (checkboxes and multiple selects
/// send one pair per checked item).
fn values_named(pairs: &[(String, String)], name: &str) -> Value {
    Value::list(pairs.iter().filter(|(k, _)| k == name).map(|(_, v)| Value::str(v.as_str())).collect())
}

/// Character-based slice with PHP `substr` semantics for negative values.
fn char_slice(text: &str, start: i64, len: Option<i64>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len() as i64;
    let from = if start < 0 { n.saturating_add(start).max(0) } else { start.min(n) };
    let to = match len {
        None => n,
        Some(l) if l < 0 => n.saturating_add(l).max(from),
        Some(l) => from.saturating_add(l).min(n),
    };
    chars[from as usize..to as usize].iter().collect()
}

fn number_format(f: f64, decimals: usize) -> String {
    // Round half away from zero like PHP (Rust's formatting rounds half to even).
    let p = 10f64.powi(decimals as i32);
    let rounded = (f.abs() * p).round() / p;
    let text = format!("{:.*}", decimals, rounded);
    let (int_part, frac) = match text.split_once('.') {
        Some((i, fr)) => (i.to_string(), Some(fr.to_string())),
        None => (text.clone(), None),
    };
    let mut grouped = String::new();
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let negative = f < 0.0 && text.chars().any(|c| c.is_ascii_digit() && c != '0');
    let mut out = if negative { format!("-{grouped}") } else { grouped };
    if let Some(fr) = frac {
        out.push('.');
        out.push_str(&fr);
    }
    out
}

fn pad(vm: &Vm, text: &str, len: i64, with: &str, start: bool) -> R {
    if with.is_empty() {
        return Err(vm.throw("ValueError", "padding string must not be empty"));
    }
    let have = text.chars().count() as i64;
    if len <= have {
        return Ok(Value::str(text));
    }
    vm.reserve(((len - have) as usize).saturating_mul(with.len().div_ceil(with.chars().count().max(1)).max(1)))?;
    let padding: String = with.chars().cycle().take((len - have) as usize).collect();
    Ok(Value::str(if start { padding + text } else { text.to_string() + &padding }))
}

fn json_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json(vm: &Vm, v: &Value, out: &mut String, depth: usize) -> Result<(), VmError> {
    // Shared lists make the output exponential in the input (`$a = [$a, $a]`).
    if out.len() > 1 << 16 {
        vm.check_limits()?;
    }
    if depth > 128 {
        return Err(vm.throw("ValueError", "json_encode(): value is nested too deeply (or recursive)"));
    }
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(vm.throw("ValueError", "json_encode(): INF and NAN cannot be encoded"));
            }
            let _ = write!(out, "{f:?}");
        }
        Value::Str(s) | Value::Html(s) => json_string(s, out),
        Value::List(l) => {
            out.push('[');
            for (i, x) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json(vm, x, out, depth + 1)?;
            }
            out.push(']');
        }
        Value::Map(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                match k {
                    Key::Str(s) => json_string(s, out),
                    Key::Int(n) => json_string(&n.to_string(), out),
                }
                out.push(':');
                json(vm, x, out, depth + 1)?;
            }
            out.push('}');
        }
        Value::Object(o) => {
            let o = o.borrow();
            let class = &vm.prog.classes[o.class as usize];
            out.push('{');
            let mut first = true;
            for (f, x) in class.fields.iter().zip(o.fields.iter()) {
                if !f.public {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                json_string(&f.name, out);
                out.push(':');
                json(vm, x, out, depth + 1)?;
            }
            out.push('}');
        }
        Value::Closure(_) => return Err(vm.throw("ValueError", "json_encode(): closures cannot be encoded")),
    }
    Ok(())
}

fn dump_value(vm: &Vm, v: &Value, out: &mut String, indent: usize) -> Result<(), VmError> {
    if out.len() > 1 << 16 {
        vm.check_limits()?;
    }
    let pad = "  ".repeat(indent);
    if indent > 32 {
        out.push_str("*RECURSION*\n");
        return Ok(());
    }
    match v {
        Value::Null => out.push_str("NULL\n"),
        Value::Bool(b) => {
            let _ = writeln!(out, "bool({b})");
        }
        Value::Int(i) => {
            let _ = writeln!(out, "int({i})");
        }
        Value::Float(f) => {
            let _ = writeln!(out, "float({})", format_float(*f));
        }
        Value::Str(s) => {
            let _ = writeln!(out, "string({}) \"{s}\"", s.len());
        }
        Value::Html(s) => {
            let _ = writeln!(out, "Html({}) \"{s}\"", s.len());
        }
        Value::List(l) => {
            let _ = writeln!(out, "list({}) {{", l.len());
            for (i, x) in l.iter().enumerate() {
                let _ = writeln!(out, "{pad}  [{i}]=>");
                out.push_str(&pad);
                out.push_str("  ");
                dump_value(vm, x, out, indent + 1)?;
            }
            let _ = writeln!(out, "{pad}}}");
        }
        Value::Map(m) => {
            let _ = writeln!(out, "map({}) {{", m.len());
            for (k, x) in m.iter() {
                match k {
                    Key::Int(n) => {
                        let _ = writeln!(out, "{pad}  [{n}]=>");
                    }
                    Key::Str(s) => {
                        let _ = writeln!(out, "{pad}  [\"{s}\"]=>");
                    }
                }
                out.push_str(&pad);
                out.push_str("  ");
                dump_value(vm, x, out, indent + 1)?;
            }
            let _ = writeln!(out, "{pad}}}");
        }
        Value::Object(o) => {
            let o = o.borrow();
            let class = &vm.prog.classes[o.class as usize];
            let _ = writeln!(out, "object({}) {{", class.name);
            for (f, x) in class.fields.iter().zip(o.fields.iter()) {
                let _ = writeln!(out, "{pad}  [\"{}\"]=>", f.name);
                out.push_str(&pad);
                out.push_str("  ");
                dump_value(vm, x, out, indent + 1)?;
            }
            let _ = writeln!(out, "{pad}}}");
        }
        Value::Closure(_) => out.push_str("closure\n"),
    }
    Ok(())
}

fn assert_impl(vm: &Vm, ok: bool, msg: String) -> R {
    if ok {
        Ok(Value::Null)
    } else {
        Err(vm.throw("AssertionError", msg))
    }
}

/// Short, readable form of a value for assertion messages.
fn repr(vm: &Vm, v: &Value) -> String {
    let mut out = String::new();
    match v {
        Value::Object(o) => {
            let o = o.borrow();
            out.push_str(&vm.prog.classes[o.class as usize].name);
            out.push_str(" object");
        }
        Value::Closure(_) => out.push_str("closure"),
        other => {
            if json(vm, other, &mut out, 0).is_err() {
                out = other.type_name().to_string();
            }
        }
    }
    if out.chars().count() > 200 {
        out = out.chars().take(200).collect::<String>() + "…";
    }
    out
}

/// Stable merge sort with a comparator that may fail.
fn merge_sort(
    items: Vec<Value>,
    cmp: &mut dyn FnMut(&Value, &Value) -> Result<i64, VmError>,
) -> Result<Vec<Value>, VmError> {
    if items.len() <= 1 {
        return Ok(items);
    }
    let mut items = items;
    let right = items.split_off(items.len() / 2);
    let left = merge_sort(items, cmp)?;
    let right = merge_sort(right, cmp)?;
    let mut out = Vec::with_capacity(left.len() + right.len());
    let (mut l, mut r) = (left.into_iter().peekable(), right.into_iter().peekable());
    while let (Some(a), Some(b)) = (l.peek(), r.peek()) {
        if cmp(a, b)? <= 0 {
            out.push(l.next().unwrap());
        } else {
            out.push(r.next().unwrap());
        }
    }
    out.extend(l);
    out.extend(r);
    Ok(out)
}

fn truthy(vm: &Vm, v: Value) -> Result<bool, VmError> {
    match v {
        Value::Bool(b) => Ok(b),
        other => Err(VmError::Fatal(format!(
            "callback returned {} instead of bool (in {})",
            other.type_name(),
            vm.prog.funcs[vm.prog.main as usize].name
        ))),
    }
}

/// Headers the server controls; a script setting them could desynchronize
/// the HTTP connection (response splitting).
const FRAMING_HEADERS: &[&str] =
    &["content-length", "transfer-encoding", "connection", "keep-alive", "upgrade", "te", "trailer"];

fn check_header(vm: &Vm, name: &str, value: &str) -> Result<(), VmError> {
    let lower = name.to_ascii_lowercase();
    if FRAMING_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-") {
        return Err(vm.throw("ValueError", format!("the \"{name}\" header is set by the server")));
    }
    let name_ok =
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !name_ok {
        return Err(vm.throw("ValueError", format!("invalid header name \"{name}\"")));
    }
    if value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
        return Err(vm.throw("ValueError", format!("header \"{name}\" contains a line break (header injection)")));
    }
    Ok(())
}

/// Media types whose output is not HTML-escaped. Everything else (HTML, XML,
/// SVG, unknown types) keeps escaping on, because a browser may render it as markup.
const PLAIN_TYPES: &[&str] = &[
    "text/plain",
    "text/csv",
    "text/css",
    "text/javascript",
    "text/calendar",
    "text/markdown",
    "application/json",
    "application/javascript",
    "application/octet-stream",
    "application/pdf",
];

fn set_type(vm: &mut Vm, ct: &str) -> Result<(), VmError> {
    if ct.contains(',') {
        return Err(vm.throw("ValueError", "Content-Type must name a single media type"));
    }
    let mime = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if mime.is_empty() || !mime.contains('/') || mime.contains(char::is_whitespace) {
        return Err(vm.throw("ValueError", format!("invalid Content-Type \"{ct}\"")));
    }
    let media = ["image/", "audio/", "video/", "font/"].iter().any(|p| mime.starts_with(p)) && mime != "image/svg+xml";
    vm.http.content_type = Some(ct.to_string());
    vm.out[0].html = !(PLAIN_TYPES.contains(&mime.as_str()) || media);
    Ok(())
}

/// `str_replace` with a size check (and no change for an empty search).
fn replace(vm: &Vm, subject: &str, search: &str, with: &str) -> R {
    if search.is_empty() {
        return Ok(Value::str(subject));
    }
    if with.len() > search.len() {
        let grow = subject.matches(search).count().saturating_mul(with.len() - search.len());
        vm.reserve(subject.len().saturating_add(grow))?;
    }
    Ok(Value::str(subject.replace(search, with)))
}

/// JSON safe to place inside `<script>`: `<`, `>`, `&`, quotes and the
/// JavaScript line separators are written as `\uXXXX`.
fn json_html(vm: &Vm, v: &Value) -> Result<String, VmError> {
    let mut out = String::new();
    json(vm, v, &mut out, 0)?;
    let mut safe = String::with_capacity(out.len());
    for c in out.chars() {
        match c {
            '<' | '>' | '&' | '\'' | '\u{2028}' | '\u{2029}' => safe.push_str(&format!("\\u{:04x}", c as u32)),
            c => safe.push(c),
        }
    }
    Ok(safe)
}

/// A URL that cannot run script: http(s), mailto, tel or a relative URL.
/// Anything else (javascript:, data:, vbscript:, ...) becomes "#".
fn safe_url(url: &str) -> String {
    let cleaned: String = url.trim().chars().filter(|c| !c.is_control() && !c.is_whitespace()).collect();
    let scheme_end = cleaned.find([':', '/', '?', '#']);
    let ok = match scheme_end {
        Some(i) if cleaned.as_bytes()[i] == b':' => {
            let scheme = cleaned[..i].to_ascii_lowercase();
            matches!(scheme.as_str(), "http" | "https" | "mailto" | "tel")
        }
        // Relative; `//host` would be protocol-relative (another site).
        _ => !cleaned.starts_with("//"),
    };
    // Browsers read `\` as `/` here, so `\\host`, `/\host` and `\/host` are `//host` too.
    let b = cleaned.as_bytes();
    let slash = |c: u8| c == b'/' || c == b'\\';
    let ok = ok && !(b.len() >= 2 && slash(b[0]) && slash(b[1]));
    if ok {
        cleaned
    } else {
        "#".into()
    }
}

/// A path on this site, safe for `Location`. Browsers drop tabs and newlines
/// from URLs and treat `\\` like `/`, so `/<TAB>/evil.com` or `/\\evil.com`
/// would become the protocol-relative `//evil.com`.
pub fn is_local_path(p: &str) -> bool {
    p.starts_with('/') && !p.starts_with("//") && !p.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\')
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    core_lookup(name)
        .or_else(|| web::lookup(name))
        .or_else(|| sys::lookup(name))
        .or_else(|| db::lookup(name))
        .or_else(|| twofactor::lookup(name))
        .or_else(|| websocket::lookup(name))
        .or_else(|| httpclient::lookup(name))
        .or_else(|| asyncio::lookup(name))
}

fn core_lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "assert" => |vm, a| assert_impl(vm, matches!(a[0], Value::Bool(true)), "assertion failed".into()),
        "assert_msg" => |vm, a| assert_impl(vm, matches!(a[0], Value::Bool(true)), s(&a[1]).to_string()),
        "assert_eq" | "assert_eq_msg" => |vm, a| {
            if values_equal(&a[0], &a[1]) {
                return Ok(Value::Null);
            }
            let mut msg = format!("expected {}, got {}", repr(vm, &a[0]), repr(vm, &a[1]));
            if let Some(m) = a.get(2) {
                msg = format!("{}: {msg}", s(m));
            }
            Err(vm.throw("AssertionError", msg))
        },
        "list_len" => |_, a| Ok(Value::Int(list(&a[0]).len() as i64)),
        "map_len" => |_, a| Ok(Value::Int(map(&a[0]).len() as i64)),
        "strlen" => |_, a| Ok(Value::Int(s(&a[0]).len() as i64)),
        "implode" => |vm, a| {
            let parts: Vec<&str> = list(&a[1]).iter().map(s).collect();
            vm.reserve(parts.iter().map(|p| p.len()).sum::<usize>() + parts.len().saturating_mul(s(&a[0]).len()))?;
            Ok(Value::str(parts.join(s(&a[0]))))
        },
        "explode" => |vm, a| {
            let sep = s(&a[0]);
            if sep.is_empty() {
                return Err(vm.throw("ValueError", "explode(): separator must not be empty"));
            }
            vm.reserve(s(&a[1]).matches(sep).count().saturating_mul(64) + s(&a[1]).len())?;
            Ok(Value::list(s(&a[1]).split(sep).map(Value::str).collect()))
        },
        "str_split" => |vm, a| {
            let sep = s(&a[1]);
            if sep.is_empty() {
                return Err(vm.throw("ValueError", "split(): separator must not be empty; use ->chars()"));
            }
            vm.reserve(s(&a[0]).matches(sep).count().saturating_mul(64) + s(&a[0]).len())?;
            Ok(Value::list(s(&a[0]).split(sep).map(Value::str).collect()))
        },
        "str_repeat" => |vm, a| {
            let n = int(&a[1]);
            if n < 0 {
                return Err(vm.throw("ValueError", "repeat count must not be negative"));
            }
            vm.reserve(s(&a[0]).len().saturating_mul(n as usize))?;
            Ok(Value::str(s(&a[0]).repeat(n as usize)))
        },
        "str_contains" => |_, a| Ok(Value::Bool(s(&a[0]).contains(s(&a[1])))),
        "str_starts_with" => |_, a| Ok(Value::Bool(s(&a[0]).starts_with(s(&a[1])))),
        "str_ends_with" => |_, a| Ok(Value::Bool(s(&a[0]).ends_with(s(&a[1])))),
        "str_replace" => |vm, a| replace(vm, s(&a[2]), s(&a[0]), s(&a[1])),
        "str_replace_m" => |vm, a| replace(vm, s(&a[0]), s(&a[1]), s(&a[2])),
        "str_upper" => |_, a| Ok(Value::str(s(&a[0]).to_uppercase())),
        "str_lower" => |_, a| Ok(Value::str(s(&a[0]).to_lowercase())),
        "str_ucfirst" => |_, a| {
            let mut c = s(&a[0]).chars();
            Ok(Value::str(match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }))
        },
        "str_trim" => |_, a| Ok(Value::str(s(&a[0]).trim())),
        "str_ltrim" => |_, a| Ok(Value::str(s(&a[0]).trim_start())),
        "str_rtrim" => |_, a| Ok(Value::str(s(&a[0]).trim_end())),
        "str_len" => |_, a| Ok(Value::Int(s(&a[0]).chars().count() as i64)),
        "str_is_empty" => |_, a| Ok(Value::Bool(s(&a[0]).is_empty())),
        "str_slice1" => |_, a| Ok(Value::str(char_slice(s(&a[0]), int(&a[1]), None))),
        "str_slice" => |_, a| Ok(Value::str(char_slice(s(&a[0]), int(&a[1]), Some(int(&a[2]))))),
        "str_index_of" => |_, a| {
            let hay = s(&a[0]);
            Ok(opt(hay.find(s(&a[1])), |b| Value::Int(hay[..b].chars().count() as i64)))
        },
        "str_pad_start" => |vm, a| pad(vm, s(&a[0]), int(&a[1]), s(&a[2]), true),
        "str_pad_end" => |vm, a| pad(vm, s(&a[0]), int(&a[1]), s(&a[2]), false),
        "str_to_int" => |_, a| Ok(opt(s(&a[0]).trim().parse::<i64>().ok(), Value::Int)),
        "str_to_float" => |_, a| {
            let t = s(&a[0]).trim();
            let ok = !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || "+-.eE".contains(c));
            Ok(opt(t.parse::<f64>().ok().filter(|f| ok && f.is_finite()), Value::Float))
        },
        "str_chars" => |vm, a| {
            vm.reserve(s(&a[0]).chars().count().saturating_mul(64))?;
            Ok(Value::list(s(&a[0]).chars().map(|c| Value::str(c.to_string())).collect()))
        },
        "nl2br" => |_, a| Ok(Value::html(html_escape(s(&a[0])).replace('\n', "<br>\n"))),
        "raw" => |_, a| Ok(Value::html(s(&a[0]))),
        "escape" => |_, a| Ok(Value::html(html_escape(s(&a[0])))),
        "json_html" => |vm, a| json_html(vm, &a[0]).map(Value::html),
        "safe_url" => |_, a| Ok(Value::html(html_escape(&safe_url(s(&a[0]))))),
        "json_encode" => |vm, a| {
            let mut out = String::new();
            json(vm, &a[0], &mut out, 0)?;
            Ok(Value::str(out))
        },
        "dump" => |vm, a| {
            let mut out = String::new();
            dump_value(vm, &a[0], &mut out, 0)?;
            vm.echo(&Value::str(out));
            Ok(Value::Null)
        },
        "intdiv" => |vm, a| {
            let (x, y) = (int(&a[0]), int(&a[1]));
            if y == 0 {
                return Err(vm.throw("DivisionByZeroError", "division by zero"));
            }
            x.checked_div(y).map(Value::Int).ok_or_else(|| vm.throw("ArithmeticError", "integer overflow"))
        },
        "abs_int" => |vm, a| {
            int(&a[0]).checked_abs().map(Value::Int).ok_or_else(|| vm.throw("ArithmeticError", "integer overflow"))
        },
        "abs_float" => |_, a| Ok(Value::Float(float(&a[0]).abs())),
        "min_int" => |_, a| Ok(Value::Int(int(&a[0]).min(int(&a[1])))),
        "max_int" => |_, a| Ok(Value::Int(int(&a[0]).max(int(&a[1])))),
        "min_float" => |_, a| Ok(Value::Float(float(&a[0]).min(float(&a[1])))),
        "max_float" => |_, a| Ok(Value::Float(float(&a[0]).max(float(&a[1])))),
        "floor" => |_, a| Ok(Value::Float(float(&a[0]).floor())),
        "ceil" => |_, a| Ok(Value::Float(float(&a[0]).ceil())),
        "round" => |_, a| Ok(Value::Float(float(&a[0]).round())),
        "round_prec" => |_, a| {
            let p = 10f64.powi(int(&a[1]).clamp(-15, 15) as i32);
            Ok(Value::Float((float(&a[0]) * p).round() / p))
        },
        "sqrt" => |_, a| Ok(Value::Float(float(&a[0]).sqrt())),
        "fmod" => |_, a| Ok(Value::Float(float(&a[0]) % float(&a[1]))),
        "number_format0" => |_, a| Ok(Value::str(number_format(float(&a[0]), 0))),
        "number_format" => |_, a| Ok(Value::str(number_format(float(&a[0]), int(&a[1]).clamp(0, 20) as usize))),
        "range" => |vm, a| {
            let (from, to) = (int(&a[0]), int(&a[1]));
            let count = (to as i128 - from as i128).abs() + 1;
            if count > 10_000_000 {
                return Err(vm.throw("ValueError", "range is too large (more than 10,000,000 items)"));
            }
            vm.reserve(count as usize * std::mem::size_of::<Value>())?;
            let items: Vec<Value> = if from <= to {
                (from..=to).map(Value::Int).collect()
            } else {
                (to..=from).rev().map(Value::Int).collect()
            };
            Ok(Value::list(items))
        },
        "time" => |_, _| {
            let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            Ok(Value::Int(d.as_secs() as i64))
        },
        "microtime" => |_, _| {
            let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            Ok(Value::Float(d.as_secs_f64()))
        },
        "rand" => |vm, a| {
            let (lo, hi) = (int(&a[0]), int(&a[1]));
            if lo > hi {
                return Err(vm.throw("ValueError", "rand(): min must not be greater than max"));
            }
            let span = (hi as i128 - lo as i128 + 1) as u128;
            let r = (vm.next_random() as u128) % span;
            Ok(Value::Int((lo as i128 + r as i128) as i64))
        },
        "list_is_empty" => |_, a| Ok(Value::Bool(list(&a[0]).is_empty())),
        "list_contains" => |_, a| Ok(Value::Bool(list(&a[0]).iter().any(|x| values_equal(x, &a[1])))),
        "list_index_of" => {
            |_, a| Ok(opt(list(&a[0]).iter().position(|x| values_equal(x, &a[1])), |i| Value::Int(i as i64)))
        }
        "list_first" => |_, a| Ok(list(&a[0]).first().cloned().unwrap_or(Value::Null)),
        "list_last" => |_, a| Ok(list(&a[0]).last().cloned().unwrap_or(Value::Null)),
        "list_slice1" | "list_slice" => |_, a| {
            let l = list(&a[0]);
            let n = l.len() as i64;
            let start = int(&a[1]);
            let from = if start < 0 { n.saturating_add(start).max(0) } else { start.min(n) };
            let to = match a.get(2).map(int) {
                None => n,
                Some(len) if len < 0 => n.saturating_add(len).max(from),
                Some(len) => from.saturating_add(len).min(n),
            };
            Ok(Value::list(l[from as usize..to as usize].to_vec()))
        },
        "list_reverse" => |_, a| Ok(Value::list(list(&a[0]).iter().rev().cloned().collect())),
        "list_concat" => |vm, a| {
            vm.reserve((list(&a[0]).len() + list(&a[1]).len()).saturating_mul(std::mem::size_of::<Value>()))?;
            let mut v = list(&a[0]).as_ref().clone();
            v.extend(list(&a[1]).iter().cloned());
            Ok(Value::list(v))
        },
        "list_unique" => |vm, a| {
            let mut seen = HashSet::new();
            let mut out: Vec<Value> = Vec::new();
            for (i, x) in list(&a[0]).iter().enumerate() {
                // Lists and maps are compared pairwise (quadratic): watch the time limit.
                if i % 1024 == 1023 {
                    vm.check_limits()?;
                }
                let fresh = match x {
                    Value::Int(_) | Value::Str(_) => seen.insert(x.to_key()),
                    _ => !out.iter().any(|y| values_equal(x, y)),
                };
                if fresh {
                    out.push(x.clone());
                }
            }
            Ok(Value::list(out))
        },
        "list_sorted" => |_, a| {
            let mut v = list(&a[0]).as_ref().clone();
            v.sort_by(compare);
            Ok(Value::list(v))
        },
        "list_sort_by" => |vm, a| {
            let f = a[1].clone();
            let items = list(&a[0]).as_ref().clone();
            let sorted = merge_sort(items, &mut |x, y| match vm.call_value(&f, &[x.clone(), y.clone()])? {
                Value::Int(i) => Ok(i),
                other => Err(VmError::Fatal(format!("sortBy callback returned {}", other.type_name()))),
            })?;
            Ok(Value::list(sorted))
        },
        "list_map" => |vm, a| {
            let f = a[1].clone();
            let mut out = Vec::with_capacity(list(&a[0]).len());
            for x in list(&a[0]).iter() {
                out.push(vm.call_value(&f, std::slice::from_ref(x))?);
            }
            Ok(Value::list(out))
        },
        "list_filter" => |vm, a| {
            let f = a[1].clone();
            let mut out = Vec::new();
            for x in list(&a[0]).iter() {
                let keep = vm.call_value(&f, std::slice::from_ref(x))?;
                if truthy(vm, keep)? {
                    out.push(x.clone());
                }
            }
            Ok(Value::list(out))
        },
        "list_find" => |vm, a| {
            let f = a[1].clone();
            for x in list(&a[0]).iter() {
                let hit = vm.call_value(&f, std::slice::from_ref(x))?;
                if truthy(vm, hit)? {
                    return Ok(x.clone());
                }
            }
            Ok(Value::Null)
        },
        "list_any" => |vm, a| {
            let f = a[1].clone();
            for x in list(&a[0]).iter() {
                let hit = vm.call_value(&f, std::slice::from_ref(x))?;
                if truthy(vm, hit)? {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        },
        "list_all" => |vm, a| {
            let f = a[1].clone();
            for x in list(&a[0]).iter() {
                let hit = vm.call_value(&f, std::slice::from_ref(x))?;
                if !truthy(vm, hit)? {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        },
        "list_join" => |vm, a| {
            let sep = s(&a[1]);
            let items = list(&a[0]);
            let total = items
                .iter()
                .map(|x| match x {
                    Value::Str(t) | Value::Html(t) => t.len(),
                    _ => 24,
                })
                .fold(sep.len().saturating_mul(items.len()), usize::saturating_add);
            vm.reserve(total)?;
            let mut out = String::with_capacity(total);
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                x.append_to(&mut out);
            }
            Ok(Value::str(out))
        },
        "list_sum_int" => |vm, a| {
            let mut total: i64 = 0;
            for x in list(&a[0]).iter() {
                total = total.checked_add(int(x)).ok_or_else(|| vm.throw("ArithmeticError", "integer overflow"))?;
            }
            Ok(Value::Int(total))
        },
        "list_sum_float" => |_, a| Ok(Value::Float(list(&a[0]).iter().map(float).sum())),
        "map_is_empty" => |_, a| Ok(Value::Bool(map(&a[0]).is_empty())),
        "map_has" => |_, a| Ok(Value::Bool(map(&a[0]).contains_key(&a[1].to_key()))),
        "map_get" => |_, a| Ok(map(&a[0]).get(&a[1].to_key()).cloned().unwrap_or(Value::Null)),
        "map_keys" => |_, a| Ok(Value::list(map(&a[0]).keys().map(|k| k.to_value()).collect())),
        "map_values" => |_, a| Ok(Value::list(map(&a[0]).values().cloned().collect())),
        "map_merge" => |_, a| {
            let mut m = map(&a[0]).as_ref().clone();
            for (k, v) in map(&a[1]).iter() {
                m.insert(k.clone(), v.clone());
            }
            Ok(Value::map(m))
        },
        "__req_method" => |vm, _| Ok(Value::str(vm.http.method.as_str())),
        "__req_ip" => |vm, _| Ok(Value::str(vm.http.remote_ip.as_str())),
        "__is_local_path" => |_, a| Ok(Value::Bool(is_local_path(s(&a[0])))),
        "__is_http_url" => |_, a| {
            let u = s(&a[0]);
            let lower = u.get(..8).unwrap_or(u).to_ascii_lowercase();
            let scheme = lower.starts_with("https://") || lower.starts_with("http://");
            Ok(Value::Bool(scheme && !u.chars().any(|c| c.is_control() || c.is_whitespace())))
        },
        "csp_nonce" => |vm, _| Ok(Value::str(vm.http.csp_nonce.clone().unwrap_or_default())),
        // An IPv6 address stands for its /64 (one network hands out many), like the
        // server's own per-client limit; IPv4 addresses and other text are kept.
        "ip_group" => |_, a| {
            let ip = s(&a[0]);
            Ok(Value::str(match ip.parse::<std::net::IpAddr>() {
                Ok(addr) => ratelimit::client_key(addr),
                Err(_) => ip.to_string(),
            }))
        },
        "rate_limit" => |vm, a| {
            let (max, secs) = (int(&a[1]), int(&a[2]));
            if !(1..=1_000_000).contains(&max) || !(1..=86_400).contains(&secs) {
                return Err(vm.throw(
                    "ValueError",
                    "rate_limit(): the limit must be 1..1000000 and the window 1..86400 seconds",
                ));
            }
            let key = format!("app:{}", s(&a[0]));
            let window = std::time::Duration::from_secs(secs as u64);
            match ratelimit::hit_shared(&vm.host, &key, max as u32, window, shared_until(vm)) {
                Ok((allowed, _)) => Ok(Value::Bool(allowed)),
                // A limit that cannot be counted refuses: it usually guards logins and the like.
                Err(e) => {
                    eprintln!("rate_limit(): the shared rate limit store cannot be used: {e}");
                    Ok(Value::Bool(false))
                }
            }
        },
        "rate_limit_reset" => |vm, a| {
            let key = format!("app:{}", s(&a[0]));
            if let Err(e) = ratelimit::reset_shared(&vm.host, &key, shared_until(vm)) {
                return Err(vm.throw("DatabaseError", format!("rate_limit_reset(): {e}")));
            }
            Ok(Value::Null)
        },
        "__req_path" => |vm, _| Ok(Value::str(vm.http.path.as_str())),
        "__req_params" => |vm, _| Ok(string_map(&vm.http.params)),
        // `layout()`: what the page writes after it is kept apart, and given to the layout as its body.
        // std.testing: [status, body, name, value, name, value, ...]
        "__site_request" => |vm, a| {
            let Some(site) = vm.host.site.clone() else {
                return Err(vm.throw(
                    "RuntimeException",
                    "there is no site to send requests to: run the test with `phv test` in a project that has public/",
                ));
            };
            let headers: Vec<(String, String)> =
                map(&a[2]).iter().map(|(k, v)| (k.to_value().to_display_string(), v.to_display_string())).collect();
            let req = phv_vm::SiteRequest {
                method: s(&a[0]).to_string(),
                path: s(&a[1]).to_string(),
                headers,
                body: s(&a[3]).as_bytes().to_vec(),
            };
            let resp = site.request(req).map_err(|e| vm.throw("RuntimeException", format!("request failed: {e}")))?;
            let mut out =
                vec![Value::str(resp.status.to_string()), Value::str(String::from_utf8_lossy(&resp.body).into_owned())];
            for (k, v) in resp.headers {
                out.push(Value::str(k));
                out.push(Value::str(v));
            }
            Ok(Value::list(out))
        },
        "__mail_send" => mail::send,
        "__capture_begin" => |vm, _| {
            let html = vm.out.last().is_none_or(|o| o.html);
            vm.out.push(phv_vm::OutBuf { buf: String::new(), html });
            Ok(Value::Null)
        },
        "__capture_end" => |vm, _| {
            let body = if vm.out.len() > 1 { vm.out.pop().map(|o| o.buf).unwrap_or_default() } else { String::new() };
            Ok(Value::html(body))
        },
        "__locals_get" => |vm, a| {
            let key = s(&a[0]);
            Ok(vm
                .http
                .locals
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| Value::str(v.as_str()))
                .unwrap_or(Value::Null))
        },
        "__locals_set" => |vm, a| {
            let (key, value) = (s(&a[0]).to_string(), s(&a[1]).to_string());
            match vm.http.locals.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                None => vm.http.locals.push((key, value)),
            }
            Ok(Value::Null)
        },
        "__locals_all" => |vm, _| Ok(string_map(&vm.http.locals)),
        "__req_query" => |vm, _| Ok(string_map(&vm.http.query)),
        "__req_form" => |vm, _| Ok(string_map(&vm.http.form)),
        "__req_query_all" => |vm, a| Ok(values_named(&vm.http.query, s(&a[0]))),
        "__req_form_all" => |vm, a| Ok(values_named(&vm.http.form, s(&a[0]))),
        "__req_headers" => |vm, _| Ok(string_map(&vm.http.headers)),
        "__req_body" => |vm, _| Ok(Value::str(vm.http.body.as_str())),
        "__resp_status" => |vm, a| {
            let code = int(&a[0]);
            if !(200..=599).contains(&code) {
                return Err(vm.throw("ValueError", format!("invalid HTTP status code {code}")));
            }
            vm.http.status = code as u16;
            Ok(Value::Null)
        },
        "__resp_header" => |vm, a| {
            let (name, value) = (s(&a[0]), s(&a[1]));
            check_header(vm, name, value)?;
            if name.eq_ignore_ascii_case("content-type") {
                let v = value.to_string();
                set_type(vm, &v)?;
            } else {
                vm.http.resp_headers.push((name.to_string(), value.to_string()));
            }
            Ok(Value::Null)
        },
        "__resp_type" => |vm, a| {
            let ct = s(&a[0]).to_string();
            check_header(vm, "Content-Type", &ct)?;
            set_type(vm, &ct)?;
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
    fn every_signature_has_an_implementation() {
        for sig in phv_types_sigs() {
            assert!(lookup(sig).is_some(), "builtin `{sig}` has a signature but no implementation");
        }
    }

    // The signature table lives in phv_types; keep this list in sync by
    // reading it through the dev-dependency.
    fn phv_types_sigs() -> Vec<&'static str> {
        phv_types::builtins::BUILTINS.iter().map(|b| b.imp).collect()
    }

    #[test]
    fn urls_cannot_run_script() {
        for bad in [
            "javascript:alert(1)",
            " JaVaScRiPt:alert(1)",
            "java\nscript:x",
            "data:text/html,x",
            "vbscript:x",
            "//evil.com",
            "\\\\evil\\x",
            "/\\evil.com",
            "\\/evil.com",
        ] {
            assert_eq!(safe_url(bad), "#", "{bad:?}");
        }
        for good in
            ["https://example.com/a?b=1", "http://x", "mailto:a@b.c", "/posts?id=2", "post?id=2", "#top", "?page=2"]
        {
            assert_eq!(safe_url(good), good);
        }
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(number_format(1234567.891, 2), "1,234,567.89");
        assert_eq!(number_format(-1234.5, 0), "-1,235");
        assert_eq!(number_format(999.0, 0), "999");
        assert_eq!(char_slice("こんにちは", 1, Some(2)), "んに");
        assert_eq!(char_slice("hello", -3, None), "llo");
    }
}
