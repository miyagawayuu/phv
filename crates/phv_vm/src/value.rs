use indexmap::{IndexMap, IndexSet};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt::Write;
use std::rc::Rc;

pub type Map = IndexMap<Key, Value>;

/// The elements of a `set<T>` (`int` or `string`), in the order they were added.
pub type KeySet = IndexSet<Key>;

/// A runtime value. Scalars are stored inline; strings, lists and maps are
/// reference-counted and copied on write, so they behave as values (like PHP
/// arrays). Objects and closures are shared references.
#[derive(Clone, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<String>),
    /// A string that is known to be safe HTML (`raw()`, `escape()`, templates).
    Html(Rc<String>),
    List(Rc<Vec<Value>>),
    Map(Rc<Map>),
    Set(Rc<KeySet>),
    Object(Rc<RefCell<Object>>),
    Closure(Rc<Closure>),
}

/// Map keys. Hashing uses the standard SipHash, which resists hash-flooding
/// attacks through request parameters.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    Int(i64),
    Str(Rc<String>),
}

pub struct Object {
    pub class: u32,
    pub fields: Vec<Value>,
}

pub struct Closure {
    pub func: u32,
    pub caps: Vec<Value>,
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(Rc::new(s.into()))
    }

    pub fn html(s: impl Into<String>) -> Value {
        Value::Html(Rc::new(s.into()))
    }

    pub fn list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(items))
    }

    pub fn map(m: Map) -> Value {
        Value::Map(Rc::new(m))
    }

    pub fn set(s: KeySet) -> Value {
        Value::Set(Rc::new(s))
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Html(_) => "Html",
            Value::List(_) => "list",
            Value::Map(_) => "map",
            Value::Set(_) => "set",
            Value::Object(_) => "object",
            Value::Closure(_) => "closure",
        }
    }

    /// Converts a map key value (int or string) to a `Key`. Other values
    /// (only reachable through `any`) use their string form.
    pub fn to_key(&self) -> Key {
        match self {
            Value::Int(i) => Key::Int(*i),
            Value::Str(s) => Key::Str(s.clone()),
            other => Key::Str(Rc::new(other.to_display_string())),
        }
    }

    /// Appends the string form of a scalar (as used by `.`, interpolation and echo).
    pub fn append_to(&self, out: &mut String) {
        match self {
            Value::Str(s) | Value::Html(s) => out.push_str(s),
            Value::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Value::Float(f) => out.push_str(&format_float(*f)),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Null => {}
            other => out.push_str(other.type_name()),
        }
    }

    pub fn to_display_string(&self) -> String {
        let mut s = String::new();
        self.append_to(&mut s);
        s
    }
}

impl Key {
    pub fn to_value(&self) -> Value {
        match self {
            Key::Int(i) => Value::Int(*i),
            Key::Str(s) => Value::Str(s.clone()),
        }
    }
}

/// A value that may hold other values (and so nest deeply).
fn holds_values(v: &Value) -> bool {
    matches!(v, Value::List(_) | Value::Map(_) | Value::Object(_) | Value::Closure(_))
}

/// Dropping a deeply nested value must not recurse (a list nested a million
/// levels deep would overflow the stack and abort the process). Children of
/// containers we hold the last reference to are moved onto a heap worklist.
impl Drop for Value {
    fn drop(&mut self) {
        fn take_children(v: &mut Value, out: &mut Vec<Value>) {
            match v {
                // Only values that hold others go on the worklist; the rest
                // are dropped here (found by the 1.0 audit: moving every item
                // of a large list onto the worklist doubled its memory).
                Value::List(l) => {
                    if let Some(items) = Rc::get_mut(l) {
                        out.extend(items.drain(..).filter(holds_values));
                    }
                }
                Value::Map(m) => {
                    if let Some(m) = Rc::get_mut(m) {
                        out.extend(m.drain(..).map(|(_, v)| v).filter(holds_values));
                    }
                }
                Value::Object(o) => {
                    if Rc::strong_count(o) == 1 {
                        if let Ok(mut ob) = o.try_borrow_mut() {
                            out.extend(ob.fields.drain(..).filter(holds_values));
                        }
                    }
                }
                Value::Closure(c) => {
                    if let Some(c) = Rc::get_mut(c) {
                        out.extend(c.caps.drain(..).filter(holds_values));
                    }
                }
                _ => {}
            }
        }
        if !matches!(self, Value::List(_) | Value::Map(_) | Value::Object(_) | Value::Closure(_)) {
            return;
        }
        let mut stack = Vec::new();
        take_children(self, &mut stack);
        while let Some(mut v) = stack.pop() {
            take_children(&mut v, &mut stack);
            // `v` is now childless (or shared), so dropping it does not recurse.
        }
    }
}

/// The number of items of `range($from, $to)` (both included, counting down
/// when `from > to`).
pub fn range_len(from: i64, to: i64) -> i128 {
    (to as i128 - from as i128).abs() + 1
}

/// The items of `range($from, $to)`; check `range_len` first.
pub fn range_items(from: i64, to: i64) -> Vec<Value> {
    if from <= to {
        (from..=to).map(Value::Int).collect()
    } else {
        (to..=from).rev().map(Value::Int).collect()
    }
}

/// The most items `range()` makes.
pub const MAX_RANGE: i128 = 10_000_000;

/// Strict equality: values of different types are never equal, except that
/// `int` and `float` compare numerically. Objects compare by identity.
/// Iterative, so deeply nested values cannot overflow the stack.
pub fn values_equal(a: &Value, b: &Value) -> bool {
    values_equal_metered(a, b, &mut |_| true).unwrap_or(false)
}

/// `values_equal` of two values that hold no others (scalars, strings,
/// objects and closures), without any allocation; None for lists, maps and
/// sets, which need the full walk. Strings are compared whole: the caller
/// times a long comparison.
#[inline]
pub fn flat_equal(a: &Value, b: &Value) -> Option<bool> {
    Some(match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => (*x as f64) == *y,
        (Value::Str(x), Value::Str(y)) | (Value::Html(x), Value::Html(y)) => Rc::ptr_eq(x, y) || x == y,
        (Value::Object(x), Value::Object(y)) => Rc::ptr_eq(x, y),
        (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
        (Value::List(_), Value::List(_)) | (Value::Map(_), Value::Map(_)) | (Value::Set(_), Value::Set(_)) => {
            return None
        }
        _ => false,
    })
}

/// `values_equal`, telling `meter` how many bytes each string comparison
/// covers; `None` when `meter` returns false (stop: the time limit passed).
pub fn values_equal_metered(a: &Value, b: &Value, meter: &mut dyn FnMut(usize) -> bool) -> Option<bool> {
    let mut work: Vec<(&Value, &Value)> = vec![(a, b)];
    // Pairs of lists/maps already queued. Shared structure (`$a = [$a, $a]`)
    // would otherwise be compared exponentially often; each pair is compared once.
    let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let mut first_visit = |x: usize, y: usize| seen.insert((x, y));
    while let Some((a, b)) = work.pop() {
        let same = match (a, b) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x == y,
            (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => (*x as f64) == *y,
            (Value::Str(x), Value::Str(y)) | (Value::Html(x), Value::Html(y)) => {
                if Rc::ptr_eq(x, y) {
                    true
                } else {
                    if !meter(x.len().min(y.len()) + 1) {
                        return None;
                    }
                    x == y
                }
            }
            (Value::List(x), Value::List(y)) => {
                if !Rc::ptr_eq(x, y) && first_visit(Rc::as_ptr(x) as usize, Rc::as_ptr(y) as usize) {
                    if x.len() != y.len() {
                        return Some(false);
                    }
                    work.extend(x.iter().zip(y.iter()));
                }
                true
            }
            (Value::Map(x), Value::Map(y)) => {
                if !Rc::ptr_eq(x, y) && first_visit(Rc::as_ptr(x) as usize, Rc::as_ptr(y) as usize) {
                    if x.len() != y.len() {
                        return Some(false);
                    }
                    for (k, v) in x.iter() {
                        match y.get(k) {
                            Some(w) => work.push((v, w)),
                            None => return Some(false),
                        }
                    }
                }
                true
            }
            // Sets are equal when they have the same elements, in any order.
            (Value::Set(x), Value::Set(y)) => {
                if !Rc::ptr_eq(x, y) {
                    if x.len() != y.len() {
                        return Some(false);
                    }
                    for k in x.iter() {
                        let bytes = match k {
                            Key::Int(_) => 8,
                            Key::Str(s) => s.len() + 1,
                        };
                        if !meter(bytes) {
                            return None;
                        }
                        if !y.contains(k) {
                            return Some(false);
                        }
                    }
                }
                true
            }
            (Value::Object(x), Value::Object(y)) => Rc::ptr_eq(x, y),
            (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
            _ => false,
        };
        if !same {
            return Some(false);
        }
    }
    Some(true)
}

/// Ordering for `<`, `<=`, `<=>` and sorting. The type checker only allows
/// numbers with numbers and strings with strings.
pub fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Ordering::Equal),
        (Value::Str(x), Value::Str(y)) if Rc::ptr_eq(x, y) => Ordering::Equal,
        (Value::Str(x), Value::Str(y)) => x.as_str().cmp(y.as_str()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// Formats a float like PHP's `echo` (14 significant digits).
pub fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NAN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "INF".into() } else { "-INF".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let sci = format!("{:.13e}", f);
    let (mantissa, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if !(-4..14).contains(&exp) {
        let m = trim_zeros(mantissa);
        let m = if m.contains('.') { m.to_string() } else { format!("{m}.0") };
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{m}E{sign}{}", exp.abs());
    }
    let decimals = (13 - exp).max(0) as usize;
    trim_zeros(&format!("{:.*}", decimals, f)).to_string()
}

fn trim_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// Appends `s` with each byte `entity` maps to replaced by that entity. The
/// bytes in between are copied in runs; the bytes replaced are all ASCII, so
/// a run never splits a character.
#[inline]
fn replace_bytes_into(s: &str, out: &mut String, entity: impl Fn(u8) -> Option<&'static str>) {
    out.reserve(s.len());
    let mut from = 0;
    for (i, b) in s.bytes().enumerate() {
        if let Some(e) = entity(b) {
            out.push_str(&s[from..i]);
            out.push_str(e);
            from = i + 1;
        }
    }
    out.push_str(&s[from..]);
}

pub fn html_escape_into(s: &str, out: &mut String) {
    replace_bytes_into(s, out, |b| match b {
        b'&' => Some("&amp;"),
        b'<' => Some("&lt;"),
        b'>' => Some("&gt;"),
        b'"' => Some("&quot;"),
        b'\'' => Some("&#039;"),
        _ => None,
    })
}

/// `Html` placed in a quoted attribute value: its quotes are encoded (the
/// browser decodes them back), so it cannot end the attribute. Markup made
/// by `escape()` has none; a template's output or a `json_html()` value may.
pub fn attr_quotes_into(s: &str, out: &mut String) {
    replace_bytes_into(s, out, |b| match b {
        b'"' => Some("&quot;"),
        b'\'' => Some("&#039;"),
        _ => None,
    })
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    html_escape_into(s, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_like_php() {
        assert_eq!(format_float(0.1 + 0.2), "0.3");
        assert_eq!(format_float(2.0), "2");
        assert_eq!(format_float(1.5), "1.5");
        assert_eq!(format_float(-3.25), "-3.25");
        assert_eq!(format_float(1e20), "1.0E+20");
        assert_eq!(format_float(1.5e-7), "1.5E-7");
        assert_eq!(format_float(0.0001), "0.0001");
        assert_eq!(format_float(1.0 / 3.0), "0.33333333333333");
        assert_eq!(format_float(123456.789), "123456.789");
        assert_eq!(format_float(0.00001), "1.0E-5");
    }

    #[test]
    fn deep_values_do_not_overflow_the_stack() {
        let mut v = Value::list(Vec::new());
        let mut w = Value::list(Vec::new());
        for _ in 0..1_000_000 {
            v = Value::list(vec![v]);
            w = Value::list(vec![w]);
        }
        assert!(values_equal(&v, &w));
        drop(v);
        drop(w);
    }

    #[test]
    fn escapes_html() {
        assert_eq!(html_escape("<a href=\"x\">'&'</a>"), "&lt;a href=&quot;x&quot;&gt;&#039;&amp;&#039;&lt;/a&gt;");
    }
}
