//! Built-in function implementations and the standard library sources.
//!
//! The type signatures for these functions live in `phv_types::builtins`.

use indexmap::IndexMap;
use phv_vm::value::{compare, format_float, html_escape};
use phv_vm::{BuiltinFn, Key, Map, Value, Vm, VmError};
use std::collections::HashSet;
use std::fmt::Write;
use std::rc::Rc;

mod asyncio;
mod collections;
pub mod cron;
mod crypt;
mod db;
pub(crate) mod deadline;
mod httpclient;
pub mod i18n;
mod imaging;
pub mod jobs;
mod jsonas;
mod jwt;
mod mail;
pub mod migrate;
mod netdb;
mod netio;
pub use netio::reaches_listener;
mod numbers;
mod oauth;
mod orm;
mod pgmeter;
pub mod ratelimit;
mod re;
mod redisclient;
pub mod safefs;
mod sets;
mod sys;
#[doc(hidden)]
pub mod testing;
mod textio;
mod timez;
mod twofactor;
mod web;
mod websocket;
mod workers;

/// SQLite connections kept between scripts: idle ones, opens served by one
/// so far, runs of scripts' statements that compiled something so far, and
/// the memory the idle ones hold. For tests.
#[doc(hidden)]
pub fn sqlite_kept_stats() -> (usize, u64, u64, usize) {
    db::kept_stats()
}

/// SQLite statements weighed (`EXPLAIN` runs) so far, and the weights kept
/// per process. For tests.
#[doc(hidden)]
pub fn sqlite_weight_stats() -> (u64, usize) {
    db::weight_stats()
}

/// How long idle SQLite connections are kept (`None`: 30 seconds). For tests.
#[doc(hidden)]
pub fn sqlite_set_kept_idle(idle: Option<std::time::Duration>) {
    db::set_kept_idle(idle)
}

/// How much memory the idle SQLite connections may hold together (`None`:
/// 64 MB). For tests.
#[doc(hidden)]
pub fn sqlite_set_kept_bytes(bytes: Option<usize>) {
    db::set_kept_bytes(bytes)
}
pub mod wsserver;

pub use asyncio::running as async_running;
pub use crypt::{hmac_sha256, server_key, store_namespace};
pub use httpclient::{check_header_name, check_header_value, check_http_url, check_oauth_url, post_telemetry};
pub use netdb::{
    redis_publish, redis_subscribe, session_claim_oauth, session_insert, session_read, session_remove, session_sweep,
    session_update, OAuthClaims, Pushed, RedisSubscriber, SessionLimits, SessionRow, SESSIONS_OF_1_1,
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
        "std.regex" => Some(include_str!("phv/regex.phv")),
        "std.time" => Some(include_str!("phv/time.phv")),
        "std.ws" => Some(include_str!("phv/ws.phv")),
        "std.jobs" => Some(include_str!("phv/jobs.phv")),
        "std.jwt" => Some(include_str!("phv/jwt.phv")),
        "std.oauth" => Some(include_str!("phv/oauth.phv")),
        "std.orm" => Some(include_str!("phv/orm.phv")),
        "std.i18n" => Some(include_str!("phv/i18n.phv")),
        "std.image" => Some(include_str!("phv/image.phv")),
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

/// HTML elements whose text browsers read as raw text (in HTML, not in SVG or MathML).
const RAW_TEXT_TAGS: &[&str] =
    &["script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "plaintext"];

/// The raw-text elements whose start tag older browsers ignore inside
/// `<select>`, reading their text as markup (`IGNORED_IN_SELECT` in the
/// HTML context tracker, R19-26; not `<script>` and `<textarea>`, which they
/// read there as everywhere).
const IGNORED_IN_SELECT: &[&str] = &["title", "style", "xmp", "iframe", "noembed", "noframes", "plaintext"];

/// Whether the tag name `name` is at `at` in `b` (in any ASCII case), ended
/// as browsers end one: by whitespace, `/`, `>`, or the end of the text
/// (what follows it in the page may end it).
fn tag_name_at(b: &[u8], at: usize, name: &str) -> bool {
    b.get(at..at + name.len()).is_some_and(|s| s.eq_ignore_ascii_case(name.as_bytes()))
        && b.get(at + name.len()).is_none_or(|c| matches!(c, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b'/' | b'>'))
}

/// Why `Html` cannot be inserted at `place` (see `__html_inside`), if it cannot.
///
/// In a `comment` or `bogus comment`: what ends it, and in a comment what
/// would end it right after a value (`>`, `->`... at the start).
///
/// In the text of the raw-text element `place` (`textarea`...): its end tag,
/// which would end the element there.
///
/// In both: `</noscript`, and with `watched ` before `place` (inside
/// `<noscript>`, `<svg>` or `<math>`) the end tag of any raw-text element.
///
/// Inside SVG or MathML (`foreign`), browsers read markup as foreign content,
/// while the template that made the `Html` was checked as HTML. The two read
/// it alike but for raw-text elements (`<title>`, `<style>`...), whose text
/// is markup there, and CDATA sections. So such an element may hold no `<`,
/// and there may be no CDATA section, and no end tag of a raw-text element
/// that the `Html` did not open (one may be open around it, in HTML that this
/// SVG or MathML holds, such as a `<foreignObject>`). And read from the place
/// (`foreign svg`, `foreign html`...: what the innermost open element is), it
/// must close every element it opens and no other, with no breakout element
/// such as `<p>` right inside SVG (`htmlctx::foreign_insert_problem`).
///
/// Inside `<select>` (`select`), older browsers ignore the start tag of a
/// `<title>`, `<style>`, `<xmp>`, `<iframe>`, `<noembed>`, `<noframes>` or
/// `<plaintext>` and read its text as markup, where the template was checked
/// as its text (the check a template gets for its own `<select>`, R19-26;
/// found by the 1.2.1 audit). Such a start tag anywhere in the `Html` is
/// refused (in a comment or a `<script>` too: simpler, and rare there).
fn html_inside(html: &str, place: &str) -> Option<String> {
    let b = html.as_bytes();
    // `watched `: inside `<noscript>`, `<svg>` or `<math>` (see below).
    let (watched, place) = match place.strip_prefix("watched ") {
        Some(p) => (true, p),
        None => (false, place),
    };
    if place == "select" {
        let tag =
            html.match_indices('<').find_map(|(i, _)| IGNORED_IN_SELECT.iter().find(|t| tag_name_at(b, i + 1, t)));
        return tag.map(|t| {
            format!(
                "`<{t}>` inside `<select>`: `Html` inserted into a `<select>` holds it, and older browsers ignore it \
                 there and read its text as markup, where the template that made the `Html` is checked as its text; \
                 close the `<select>` first, or insert a string (it is escaped) instead"
            )
        });
    }
    // An end tag that ends an element a browser may read around the `Html` as
    // raw text, where the template was checked as a comment or raw text: any
    // template may be written inside a `<noscript>` (raw text with scripting
    // on), and inside one or inside SVG or MathML, HTML that this tracker
    // does not follow there may hold a `<title>`, `<textarea>`... (found by
    // the 1.2 audit).
    let hidden = || {
        html.match_indices("</").find_map(|(i, _)| {
            std::iter::once("noscript")
                .chain(RAW_TEXT_TAGS.iter().copied().filter(|_| watched))
                .find(|t| tag_name_at(b, i + 2, t))
        })
    };
    let hides = |into: &str, t: &str| {
        Some(format!(
            "`Html` inserted into {into} holds the end tag `</{t}`, which would end a `<{t}>` that browsers may \
             read around it as raw text, and turn the rest into markup; insert a string (it is escaped) instead"
        ))
    };
    // In a comment: what ends it (`-->`, `--!>`; a bogus comment such as
    // `<!x ...>` ends at the first `>`). A string next to it cannot add a `>`
    // (strings are escaped), and the text after it is checked when compiled.
    if place == "comment" || place == "bogus comment" {
        let ends = if place == "comment" { html.contains("-->") || html.contains("--!>") } else { html.contains('>') };
        if ends {
            return Some(format!(
                "`Html` inserted into a {place} holds what ends it (`{}`), which would turn the rest into \
                 markup; insert a string (it is escaped) instead",
                if place == "comment" { "-->" } else { ">" }
            ));
        }
        // Right after a value ending with `--` (or `-`, `--!`), these end it
        // (found by the 1.2 audit: `"-"` then `Html` `-> next`).
        if place == "comment" && [">", "!>", "->", "-!>"].iter().any(|s| html.starts_with(s)) {
            return Some(
                "`Html` inserted into a comment starts with `>`, `!>`, `->` or `-!>`, which would end it after \
                 a value ending with `--` (or `-`, `--!`) and turn the rest into markup; insert a string (it is \
                 escaped) instead"
                    .into(),
            );
        }
        return hidden().and_then(|t| hides(&format!("a {place}"), t));
    }
    // `foreign svg`, `foreign html`... (`foreign` alone from an older build).
    let foreign = place == "foreign" || place.starts_with("foreign ");
    if !foreign {
        let at = html.match_indices("</").find(|&(i, _)| tag_name_at(b, i + 2, place));
        if at.is_some() {
            return Some(format!(
                "`Html` inserted into a `<{place}>` holds its end tag `</{place}`, which would end the element \
                 there; insert a string (it is escaped) instead"
            ));
        }
        return hidden().and_then(|t| hides(&format!("a `<{place}>`"), t));
    }
    let refused = |what: &str| {
        Some(format!(
            "`Html` inserted inside `<svg>` or `<math>` holds {what}, which browsers read as markup there (it \
             was checked as HTML); insert it outside the SVG or MathML, or write the element's text without `<`"
        ))
    };
    let mut i = 0;
    while let Some(off) = html[i..].find('<') {
        i += off;
        if b[i + 1..].starts_with(b"![CDATA[") {
            return refused("a CDATA section");
        }
        if b.get(i + 1) == Some(&b'/') {
            if let Some(t) = RAW_TEXT_TAGS.iter().find(|t| tag_name_at(b, i + 2, t)) {
                return refused(&format!("the end tag `</{t}` of an element it did not open"));
            }
        } else if let Some(t) = RAW_TEXT_TAGS.iter().find(|t| tag_name_at(b, i + 1, t)) {
            // Its text, up to its end tag, may hold no `<`.
            let text = i + 1 + t.len();
            let end = html[text..].find('<').map(|o| text + o);
            match end {
                Some(e) if b.get(e + 1) == Some(&b'/') && tag_name_at(b, e + 2, t) => i = e + 2 + t.len(),
                _ => return refused(&format!("a `<{t}>` whose text holds `<` (or does not end)")),
            }
            continue;
        }
        i += 1;
    }
    // The elements it opens and closes, read as browsers read them where it
    // lands: the page after it was checked as if it changed nothing there
    // (found by the 19th assessment: `<p>icon</p>`, `</svg>` or an open
    // `<foreignObject>` turned the page's self-closed `<script/>` into a
    // script with the nonce).
    phv_syntax::htmlctx::foreign_insert_problem(html, place).map(|what| {
        format!(
            "`Html` inserted inside `<svg>` or `<math>` holds {what}: browsers would build the SVG or MathML \
             around it in another way than the page after it was checked; close every element it opens there and \
             no other, write HTML elements only inside a `<foreignObject>` (or `<desc>`, `<title>`, a MathML text \
             element) it opens, or insert it outside the SVG or MathML"
        )
    })
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        v => panic!("builtin expected int, got {}", v.type_name()),
    }
}

/// How long a shared rate limit waits for its store at most (and never past
/// the script's deadline: `deadline::until`).
const SHARED_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

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
/// Where `x` is in list `l` (`contains`, `indexOf`), watching the time limit.
fn position(vm: &Vm, l: &Value, x: &Value) -> Result<Option<usize>, VmError> {
    for (i, y) in list(l).iter().enumerate() {
        if i % 1024 == 1023 {
            vm.check_limits()?;
        }
        if vm.equal(y, x)? {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

fn char_slice(text: &str, start: i64, len: Option<i64>) -> String {
    // Counted in place: a copy as characters would take four bytes each.
    let n = text.chars().count() as i64;
    let from = if start < 0 { n.saturating_add(start).max(0) } else { start.min(n) };
    let to = match len {
        None => n,
        Some(l) if l < 0 => n.saturating_add(l).max(from),
        Some(l) => from.saturating_add(l).min(n),
    };
    let at = |c: i64| text.char_indices().nth(c as usize).map_or(text.len(), |(b, _)| b);
    let start = at(from);
    let end = if to == n {
        text.len()
    } else {
        start + text[start..].char_indices().nth((to - from) as usize).map_or(text.len() - start, |(b, _)| b)
    };
    text[start..end].to_string()
}

fn number_format(f: f64, decimals: usize) -> String {
    // The sign of an infinity is kept (it has no digits to tell it by below).
    if f.is_infinite() {
        return if f < 0.0 { "-inf" } else { "inf" }.to_string();
    }
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
    json_to(vm, v, out, depth, false)
}

/// A line break and the indentation of `depth` (two spaces a level).
fn json_newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// JSON of `v`, compact or (`pretty`) on lines indented by two spaces. Past
/// 64 KB of output the limits are checked before each value, so neither
/// shared lists (`$a = [$a, $a]`) nor indentation can outgrow them unseen.
fn json_to(vm: &Vm, v: &Value, out: &mut String, depth: usize, pretty: bool) -> Result<(), VmError> {
    if out.len() > 1 << 16 {
        vm.check_limits()?;
    }
    if depth > 128 {
        return Err(vm.throw("ValueError", "json_encode(): value is nested too deeply (or recursive)"));
    }
    // Before each item of a list or an object: `,` and, when pretty, a new line.
    let sep = |out: &mut String, i: usize| {
        if i > 0 {
            out.push(',');
        }
        if pretty {
            json_newline(out, depth + 1);
        }
    };
    let close = |out: &mut String, n: usize, c: char| {
        if pretty && n > 0 {
            json_newline(out, depth);
        }
        out.push(c);
    };
    let colon = if pretty { ": " } else { ":" };
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
        Value::Str(s) | Value::Html(s) => {
            // One long string can grow six times (`\u0001`).
            if s.len() > 4096 {
                vm.reserve(s.len().saturating_mul(6))?;
            }
            json_string(s, out)
        }
        Value::List(l) => {
            out.push('[');
            for (i, x) in l.iter().enumerate() {
                sep(out, i);
                json_to(vm, x, out, depth + 1, pretty)?;
            }
            close(out, l.len(), ']');
        }
        // A set is a JSON array of its elements.
        Value::Set(s) => {
            out.push('[');
            for (i, k) in s.iter().enumerate() {
                sep(out, i);
                match k {
                    // As a string value: one may grow six times (found by the 1.0 audit).
                    Key::Str(s) => {
                        if out.len() > 1 << 16 && i % 256 == 0 {
                            vm.check_limits()?;
                        }
                        if s.len() > 4096 {
                            vm.reserve(s.len().saturating_mul(6))?;
                        }
                        json_string(s, out)
                    }
                    Key::Int(n) => {
                        let _ = write!(out, "{n}");
                    }
                }
            }
            close(out, s.len(), ']');
        }
        Value::Map(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                sep(out, i);
                match k {
                    Key::Str(s) => json_string(s, out),
                    Key::Int(n) => json_string(&n.to_string(), out),
                }
                out.push_str(colon);
                json_to(vm, x, out, depth + 1, pretty)?;
            }
            close(out, m.len(), '}');
        }
        Value::Object(o) => {
            let o = o.borrow();
            let class = &vm.prog.classes[o.class as usize];
            // An enum case is its value (`"admin"`), or its name for a pure enum,
            // as `json_decode_as()` reads it back.
            if !class.cases.is_empty() {
                let field = ["value", "name"].iter().find_map(|n| class.fields.iter().position(|f| f.name == *n));
                if let Some(x) = field.and_then(|i| o.fields.get(i)) {
                    return json_to(vm, x, out, depth + 1, pretty);
                }
            }
            out.push('{');
            let mut n = 0;
            for (f, x) in class.fields.iter().zip(o.fields.iter()) {
                if !f.public {
                    continue;
                }
                sep(out, n);
                n += 1;
                json_string(&f.name, out);
                out.push_str(colon);
                json_to(vm, x, out, depth + 1, pretty)?;
            }
            close(out, n, '}');
        }
        Value::Closure(_) => return Err(vm.throw("ValueError", "json_encode(): closures cannot be encoded")),
    }
    Ok(())
}

/// The order `sorted()` and the map sorts use: `compare`, with NaN after every
/// number so that the order is total.
fn sort_order(a: &Value, b: &Value) -> std::cmp::Ordering {
    let nan = |v: &Value| matches!(v, Value::Float(f) if f.is_nan());
    match (nan(a), nan(b)) {
        (false, false) => compare(a, b),
        (x, y) => x.cmp(&y),
    }
}

/// A stable merge sort that, unlike the standard library's, cannot panic when
/// the order is not consistent (ints and floats near 2^53 compare as floats).
pub(crate) fn stable_sort<T>(items: Vec<T>, le: &dyn Fn(&T, &T) -> bool) -> Vec<T> {
    if items.len() <= 1 {
        return items;
    }
    let mut items = items;
    let right = items.split_off(items.len() / 2);
    let (left, right) = (stable_sort(items, le), stable_sort(right, le));
    let mut out = Vec::with_capacity(left.len() + right.len());
    let (mut l, mut r) = (left.into_iter().peekable(), right.into_iter().peekable());
    while let (Some(a), Some(b)) = (l.peek(), r.peek()) {
        let take_left = le(a, b);
        out.extend(if take_left { l.next() } else { r.next() });
    }
    out.extend(l);
    out.extend(r);
    out
}

/// Sorts `items` by the values `key` gives, watching the time limit: comparing
/// long strings can take long, so every 16 MB compared it is checked, and once
/// it has passed the rest of the sort compares nothing.
pub(crate) fn sort_metered<T>(vm: &Vm, items: Vec<T>, key: impl Fn(&T) -> &Value) -> Result<Vec<T>, VmError> {
    let deadline = vm.deadline();
    let (late, bytes) = (std::cell::Cell::new(false), std::cell::Cell::new(0usize));
    let le = |x: &T, y: &T| -> bool {
        if late.get() {
            return true;
        }
        let (a, b) = (key(x), key(y));
        if let (Value::Str(p), Value::Str(q)) = (a, b) {
            let n = bytes.get() + p.len().min(q.len());
            if n >= 16 << 20 {
                bytes.set(0);
                if deadline.is_some_and(|d| std::time::Instant::now() > d) {
                    late.set(true);
                    return true;
                }
            } else {
                bytes.set(n);
            }
        }
        sort_le(a, b)
    };
    let sorted = stable_sort(items, &le);
    if late.get() {
        vm.check_limits()?;
    }
    Ok(sorted)
}

pub(crate) fn sort_le(a: &Value, b: &Value) -> bool {
    sort_order(a, b) != std::cmp::Ordering::Greater
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
        Value::Set(s) => {
            let _ = writeln!(out, "set({}) {{", s.len());
            for k in s.iter() {
                if out.len() > 1 << 16 {
                    vm.check_limits()?;
                }
                match k {
                    Key::Int(n) => {
                        let _ = writeln!(out, "{pad}  int({n})");
                    }
                    Key::Str(s) => {
                        let _ = writeln!(out, "{pad}  string({}) \"{s}\"", s.len());
                    }
                }
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
    set_type_with(vm, ct, false)
}

/// The Content-Type of a body that replaces any earlier one (`response()->file()`):
/// bytes sent before it are not sent, so they do not count as written output.
fn set_body_type(vm: &mut Vm, ct: &str) -> Result<(), VmError> {
    set_type_with(vm, ct, true)
}

fn set_type_with(vm: &mut Vm, ct: &str, replaces_body: bool) -> Result<(), VmError> {
    if ct.contains(',') {
        return Err(vm.throw("ValueError", "Content-Type must name a single media type"));
    }
    let mime = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if mime.is_empty() || !mime.contains('/') || mime.contains(char::is_whitespace) {
        return Err(vm.throw("ValueError", format!("invalid Content-Type \"{ct}\"")));
    }
    let media = ["image/", "audio/", "video/", "font/"].iter().any(|p| mime.starts_with(p)) && mime != "image/svg+xml";
    let html = !(PLAIN_TYPES.contains(&mime.as_str()) || media);
    // Bytes of `response()->file()` or an image's `send()` are output too, and
    // never escaped or checked: retyping them as HTML or JavaScript would serve
    // a file or an image as markup or as a script (R22-30).
    let body = !replaces_body && (vm.http.body_file.is_some() || vm.http.body_bytes.is_some());
    if html && body {
        return Err(vm.throw(
            "ValueError",
            format!("set the Content-Type \"{ct}\" before response()->file() or an image's send(): their bytes are not escaped and would be read as markup"),
        ));
    }
    if html && !vm.out[0].html && !vm.out[0].buf.trim().is_empty() {
        // What was written so far was not escaped: a browser would read it as markup
        // (found by the 1.2 audit).
        return Err(vm.throw(
            "ValueError",
            format!("set the Content-Type \"{ct}\" before writing any output: what was written under a type that is not escaped (such as text/plain) would be read as markup"),
        ));
    }
    vm.http.content_type = Some(ct.to_string());
    vm.out[0].html = html;
    // Every type browsers run as a script (the WHATWG list).
    let js = matches!(
        mime.as_str(),
        "application/ecmascript"
            | "application/javascript"
            | "application/x-ecmascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "text/javascript"
            | "text/javascript1.0"
            | "text/javascript1.1"
            | "text/javascript1.2"
            | "text/javascript1.3"
            | "text/javascript1.4"
            | "text/javascript1.5"
            | "text/jscript"
            | "text/livescript"
            | "text/x-ecmascript"
            | "text/x-javascript"
    );
    if js && (body || !vm.out[0].buf.trim().is_empty()) {
        // What was written so far was not checked as JavaScript.
        return Err(vm.throw("ValueError", "set a JavaScript Content-Type before writing any output"));
    }
    vm.js_output = js;
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
    // Each character may become `\uXXXX` (six bytes).
    vm.reserve(out.len().saturating_mul(6))?;
    let mut safe = String::with_capacity(out.len());
    let escape = |safe: &mut String, c: char| {
        let mut buf = [0u16; 2];
        for u in c.encode_utf16(&mut buf) {
            safe.push_str(&format!("\\u{u:04x}"));
        }
    };
    // Inside its strings, every character but letters and digits is written as
    // `\uXXXX` (and `\"`, `\\` too). Outside them there are only brackets,
    // commas, colons, numbers, `true`, `false` and `null`. So wherever the value
    // lands in a script, it cannot end what holds it: in a '...' string,
    // template literal, comment or regular expression there is no `'`, `` ` ``,
    // `$`, `/` or line break; and if a "..." string written around it (by
    // mistake) turns its strings' contents into code, they are names and
    // numbers next to string literals, a syntax error rather than a call.
    let (mut in_string, mut after_backslash) = (false, false);
    for c in out.chars() {
        if after_backslash {
            after_backslash = false;
            match c {
                '"' => safe.push_str("u0022"),
                '\\' => safe.push_str("u005c"),
                '/' => safe.push_str("u002f"),
                c => safe.push(c),
            }
        } else if !in_string {
            in_string = c == '"';
            safe.push(c);
        } else {
            match c {
                '"' => {
                    in_string = false;
                    safe.push(c);
                }
                '\\' => {
                    after_backslash = true;
                    safe.push(c);
                }
                c if c.is_ascii_alphanumeric() => safe.push(c),
                c if c.is_ascii() || c.is_whitespace() || c == '\u{feff}' => escape(&mut safe, c),
                c => safe.push(c),
            }
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
    if !ok {
        "#".into()
    } else if !cleaned.is_empty() && !cleaned.contains([':', '/', '?', '#']) {
        // `java` is a relative URL, but text after it could make it
        // `javascript:`; `./java` is the same URL with its start decided.
        format!("./{cleaned}")
    } else {
        cleaned
    }
}

/// A path on this site, safe for `Location`. Browsers drop tabs and newlines
/// from URLs and treat `\\` like `/`, so `/<TAB>/evil.com` or `/\\evil.com`
/// would become the protocol-relative `//evil.com`.
pub fn is_local_path(p: &str) -> bool {
    p.starts_with('/') && !p.starts_with("//") && !p.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\')
}

/// The modules that implement builtins, each looked up by name.
const PROVIDERS: &[fn(&str) -> Option<BuiltinFn>] = &[
    core_lookup,
    web::lookup,
    sys::lookup,
    db::lookup,
    twofactor::lookup,
    websocket::lookup,
    httpclient::lookup,
    asyncio::lookup,
    collections::lookup,
    sets::lookup,
    wsserver::lookup,
    numbers::lookup,
    re::lookup,
    textio::lookup,
    timez::lookup,
    jsonas::lookup,
    crypt::lookup,
    jobs::lookup,
    jwt::lookup,
    oauth::lookup,
    orm::lookup,
    i18n::lookup,
    imaging::lookup,
];

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    PROVIDERS.iter().find_map(|p| p(name))
}

fn core_lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "assert" => |vm, a| assert_impl(vm, matches!(a[0], Value::Bool(true)), "assertion failed".into()),
        "assert_msg" => |vm, a| assert_impl(vm, matches!(a[0], Value::Bool(true)), s(&a[1]).to_string()),
        "assert_eq" | "assert_eq_msg" => |vm, a| {
            // Timed and accounted as `==` (found by the 1.0.0 audit).
            if vm.equal(&a[0], &a[1])? {
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
        "str_upper" => |vm, a| {
            vm.reserve(s(&a[0]).len().saturating_mul(3))?;
            Ok(Value::str(s(&a[0]).to_uppercase()))
        },
        "str_lower" => |vm, a| {
            vm.reserve(s(&a[0]).len().saturating_mul(3))?;
            Ok(Value::str(s(&a[0]).to_lowercase()))
        },
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
        "nl2br" => |vm, a| {
            vm.reserve(s(&a[0]).len().saturating_mul(12))?;
            Ok(Value::html(html_escape(s(&a[0])).replace('\n', "<br>\n")))
        },
        "raw" => |_, a| Ok(Value::html(s(&a[0]))),
        "escape" => |vm, a| {
            vm.reserve(s(&a[0]).len().saturating_mul(6))?;
            Ok(Value::html(html_escape(s(&a[0]))))
        },
        "json_html" => |vm, a| json_html(vm, &a[0]).map(Value::html),
        "safe_url" => |_, a| Ok(Value::html(html_escape(&safe_url(s(&a[0]))))),
        "json_encode" => |vm, a| {
            let mut out = String::new();
            json(vm, &a[0], &mut out, 0)?;
            Ok(Value::str(out))
        },
        "json_encode_pretty" => |vm, a| {
            let mut out = String::new();
            json_to(vm, &a[0], &mut out, 0, true)?;
            Ok(Value::str(out))
        },
        "dump" => |vm, a| {
            vm.check_output()?;
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
            let count = phv_vm::value::range_len(from, to);
            if count > phv_vm::value::MAX_RANGE {
                return Err(vm.throw("ValueError", "range is too large (more than 10,000,000 items)"));
            }
            vm.reserve(count as usize * std::mem::size_of::<Value>())?;
            Ok(Value::list(phv_vm::value::range_items(from, to)))
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
        "list_contains" => |vm, a| Ok(Value::Bool(position(vm, &a[0], &a[1])?.is_some())),
        "list_index_of" => |vm, a| Ok(opt(position(vm, &a[0], &a[1])?, |i| Value::Int(i as i64))),
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
            // The values kept that are not hashed: only those are compared
            // pairwise, and each comparison counts (found by the 1.1 audit:
            // nulls were compared with every int and string kept).
            let mut others: Vec<Value> = Vec::new();
            let mut compared = 0usize;
            let mut hashed = 0usize;
            for (i, x) in list(&a[0]).iter().enumerate() {
                if i % 1024 == 1023 {
                    vm.check_limits()?;
                }
                if let Value::Str(t) = x {
                    hashed += t.len();
                    if hashed >= 16 << 20 {
                        hashed = 0;
                        vm.check_limits()?;
                    }
                }
                let fresh = match x {
                    Value::Int(_) | Value::Str(_) => seen.insert(x.to_key()),
                    _ => {
                        let mut found = false;
                        for y in &others {
                            compared += 1;
                            if compared.is_multiple_of(65536) {
                                vm.check_limits()?;
                            }
                            if vm.equal(x, y)? {
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            others.push(x.clone());
                        }
                        !found
                    }
                };
                if fresh {
                    out.push(x.clone());
                }
            }
            Ok(Value::list(out))
        },
        "list_sorted" => |vm, a| {
            let v = list(&a[0]).as_ref().clone();
            sort_metered(vm, v, |x| x).map(Value::list)
        },
        "list_sort_by" => |vm, a| {
            let f = vm.callback(&a[1]);
            let items = list(&a[0]).as_ref().clone();
            let sorted = merge_sort(items, &mut |x, y| match f.call(vm, &[x.clone(), y.clone()])? {
                Value::Int(i) => Ok(i),
                other => Err(VmError::Fatal(format!("sortBy callback returned {}", other.type_name()))),
            })?;
            Ok(Value::list(sorted))
        },
        "list_map" => |vm, a| {
            let f = vm.callback(&a[1]);
            let mut out = Vec::with_capacity(list(&a[0]).len());
            for x in list(&a[0]).iter() {
                out.push(f.call(vm, std::slice::from_ref(x))?);
            }
            Ok(Value::list(out))
        },
        "list_filter" => |vm, a| {
            let f = vm.callback(&a[1]);
            let mut out = Vec::new();
            for x in list(&a[0]).iter() {
                let keep = f.call(vm, std::slice::from_ref(x))?;
                if truthy(vm, keep)? {
                    out.push(x.clone());
                }
            }
            Ok(Value::list(out))
        },
        "list_find" => |vm, a| {
            let f = vm.callback(&a[1]);
            for x in list(&a[0]).iter() {
                let hit = f.call(vm, std::slice::from_ref(x))?;
                if truthy(vm, hit)? {
                    return Ok(x.clone());
                }
            }
            Ok(Value::Null)
        },
        "list_any" => |vm, a| {
            let f = vm.callback(&a[1]);
            for x in list(&a[0]).iter() {
                let hit = f.call(vm, std::slice::from_ref(x))?;
                if truthy(vm, hit)? {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        },
        "list_all" => |vm, a| {
            let f = vm.callback(&a[1]);
            for x in list(&a[0]).iter() {
                let hit = f.call(vm, std::slice::from_ref(x))?;
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
            match ratelimit::hit_shared(&vm.host, &key, max as u32, window, deadline::until(vm, SHARED_WAIT)) {
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
            if let Err(e) = ratelimit::reset_shared(&vm.host, &key, deadline::until(vm, SHARED_WAIT)) {
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
        "__html_inside" => |_, a| {
            if let Value::Html(h) = &a[0] {
                if let Some(msg) = html_inside(h, s(&a[1])) {
                    return Err(VmError::Fatal(msg));
                }
            }
            Ok(a[0].clone())
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
        "__exc_trace" => |vm, a| {
            Ok(Value::str(match &a[0] {
                Value::Object(o) => vm.exception_trace(o),
                _ => String::new(),
            }))
        },
        "__request_id" => |vm, _| Ok(vm.http.request_id.as_deref().map_or(Value::Null, Value::str)),
        "__req_query" => |vm, _| Ok(string_map(&vm.http.query)),
        "__req_form" => |vm, _| Ok(string_map(&vm.http.form)),
        "__req_query_all" => |vm, a| Ok(values_named(&vm.http.query, s(&a[0]))),
        "__req_form_all" => |vm, a| Ok(values_named(&vm.http.form, s(&a[0]))),
        "__req_headers" => |vm, _| Ok(string_map(&vm.http.headers)),
        "__req_body" => |vm, _| Ok(Value::str(vm.http.body.as_str())),
        "__resp_status" => |vm, a| {
            web::unsent(vm, "the status")?;
            let code = int(&a[0]);
            if !(200..=599).contains(&code) {
                return Err(vm.throw("ValueError", format!("invalid HTTP status code {code}")));
            }
            vm.http.status = code as u16;
            Ok(Value::Null)
        },
        "__resp_header" => |vm, a| {
            web::unsent(vm, "headers")?;
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
            web::unsent(vm, "the Content-Type")?;
            let ct = s(&a[0]).to_string();
            check_header(vm, "Content-Type", &ct)?;
            set_type(vm, &ct)?;
            Ok(Value::Null)
        },
        // __resp_flush(): sends the response so far (see `phv_vm::FlushSink`).
        "__resp_flush" => |vm, _| {
            if vm.out.len() != 1 {
                return Err(vm.throw(
                    "ValueError",
                    "response()->flush() cannot be called while output is captured (in a layout or a component)",
                ));
            }
            if vm.http.ws.is_some() {
                return Err(vm.throw("ValueError", "response()->flush() is for pages, not WebSocket scripts"));
            }
            if vm.http.body_file.is_some() {
                return Err(vm.throw("ValueError", "response()->flush() cannot send output after response()->file()"));
            }
            if vm.http.body_bytes.is_some() {
                return Err(vm.throw("ValueError", "response()->flush() cannot send output after an image's send()"));
            }
            let Some(mut sink) = vm.http.stream.take() else { return Ok(Value::Bool(true)) };
            let mut chunk = std::mem::take(&mut vm.out[0].buf).into_bytes();
            vm.output_flushed(chunk.len());
            if chunk.is_empty() {
                // Room without bytes is freed here.
                chunk = Vec::new();
            }
            let room = chunk.capacity();
            // Output the sink took leaves this script: it is counted in the server's
            // output total instead. Output it gave back (the client is gone) is freed
            // here, and stays counted until then (found by the 1.2 audit: it was let
            // go of either way, so each flush to a closed connection added room).
            let open = match sink.flush(&mut vm.http, chunk) {
                Ok(()) => {
                    phv_vm::alloc::uncharge_thread(room);
                    true
                }
                Err(unsent) => {
                    drop(unsent);
                    false
                }
            };
            vm.http.flushed = true;
            vm.http.stream = Some(sink);
            vm.check_limits()?;
            Ok(Value::Bool(open))
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

    /// Two modules implementing one name: the first would shadow the other.
    #[test]
    fn every_builtin_has_one_implementation() {
        for sig in phv_types_sigs() {
            let n = PROVIDERS.iter().filter(|p| p(sig).is_some()).count();
            assert!(n <= 1, "builtin `{sig}` is implemented by {n} modules");
        }
    }

    /// SQLite is opened only where `db::mem::init` has run first: it switches
    /// SQLite's allocator and starts SQLite in one step, and a database
    /// opened past it by another thread while SQLite started left memory of
    /// the default allocator for ours to free (found by CI on macOS: an
    /// abort in the tests).
    #[test]
    fn sqlite_is_opened_through_init() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src];
        let mut opened = Vec::new();
        // Joined so that this test does not find itself.
        let needle = ["Connection::", "open"].concat();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && std::fs::read_to_string(&path).unwrap().contains(&needle)
                {
                    opened.push(
                        path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap().to_string_lossy().replace('\\', "/"),
                    );
                }
            }
        }
        opened.sort();
        // db.rs opens after `vfs::register` or `mem::init`; the VFS tests
        // after `register`; the cost and mem tests after `mem::init`.
        assert_eq!(opened, ["src/db.rs", "src/db/cost.rs", "src/db/mem.rs", "src/db/vfs.rs"]);
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
        assert_eq!(number_format(f64::NEG_INFINITY, 2), "-inf");
        assert_eq!(number_format(f64::INFINITY, 0), "inf");
        assert_eq!(number_format(999.0, 0), "999");
        assert_eq!(char_slice("こんにちは", 1, Some(2)), "んに");
        assert_eq!(char_slice("hello", -3, None), "llo");
    }
}
