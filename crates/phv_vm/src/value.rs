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
            Value::Int(i) => write_int(out, *i),
            Value::Float(f) => write_float(out, *f),
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

/// What `values_equal_metered` counts for each pair of values it queues (or
/// records as seen): their room in its lists, generously.
pub const PAIR_COST: usize = 32;

/// What `values_equal_metered` tells its meter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charge {
    /// Bytes of work: each string compared, each pair of elements and map
    /// key looked at (for the time limit).
    Work(usize),
    /// Bytes about to be allocated at once: a list of pairs or the table of
    /// pairs seen growing, while the old one is still held.
    Alloc(usize),
}

/// The bytes of the table a `HashSet<(usize, usize)>` of `cap` items has:
/// a power of two of buckets of 16 bytes and a control byte each, and a group
/// of control bytes more (as hashbrown lays it out, rounded up).
fn pair_table_bytes(cap: usize) -> usize {
    let buckets = if cap < 4 {
        4
    } else if cap < 8 {
        8
    } else {
        (cap.saturating_mul(8) / 7).checked_next_power_of_two().unwrap_or(usize::MAX)
    };
    buckets.saturating_mul(17).saturating_add(16)
}

/// `values_equal`, telling `meter` the work it does in bytes: each string
/// compared, each pair of elements and map key looked at, and what its lists
/// of pairs to compare will hold, before they grow (found by the 1.0.0 audit:
/// only strings were counted, so `==` on large or differently shared lists ran
/// past the time and memory limits). Growing a list is told as `Alloc` before
/// it happens (found by the 1.0.0 audit again: the table of pairs seen grew in
/// one step, to about twice the memory limit). `None` when `meter` returns
/// false (stop: a limit was passed).
pub fn values_equal_metered(a: &Value, b: &Value, meter: &mut dyn FnMut(Charge) -> bool) -> Option<bool> {
    /// Records the pair `(x, y)`; whether it was new.
    fn first_visit(
        seen: &mut std::collections::HashSet<(usize, usize)>,
        x: usize,
        y: usize,
        meter: &mut dyn FnMut(Charge) -> bool,
    ) -> Option<bool> {
        if seen.len() == seen.capacity() && !seen.contains(&(x, y)) {
            // The new table is allocated while the old one is still held.
            if !meter(Charge::Alloc(pair_table_bytes(seen.capacity().saturating_add(1)))) {
                return None;
            }
            seen.reserve(1);
        }
        Some(seen.insert((x, y)))
    }
    /// Room in `work` for `n` more pairs, told before the list grows.
    fn room<T>(work: &mut Vec<T>, n: usize, meter: &mut dyn FnMut(Charge) -> bool) -> bool {
        if work.capacity() - work.len() < n {
            let cap = work.capacity().saturating_mul(2).max(work.len().saturating_add(n)).max(4);
            if !meter(Charge::Alloc(cap.saturating_mul(std::mem::size_of::<T>()))) {
                return false;
            }
            work.reserve(n);
        }
        true
    }
    let mut work: Vec<(&Value, &Value)> = vec![(a, b)];
    // Pairs of lists/maps already queued. Shared structure (`$a = [$a, $a]`)
    // would otherwise be compared exponentially often; each pair is compared once.
    let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
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
                    if !meter(Charge::Work(x.len().min(y.len()) + 1)) {
                        return None;
                    }
                    x == y
                }
            }
            (Value::List(x), Value::List(y)) => {
                if !Rc::ptr_eq(x, y)
                    && first_visit(&mut seen, Rc::as_ptr(x) as usize, Rc::as_ptr(y) as usize, &mut *meter)?
                {
                    if x.len() != y.len() {
                        return Some(false);
                    }
                    // The pairs queued, and the pair recorded as seen.
                    if !meter(Charge::Work(x.len().saturating_mul(PAIR_COST).saturating_add(PAIR_COST)))
                        || !room(&mut work, x.len(), &mut *meter)
                    {
                        return None;
                    }
                    work.extend(x.iter().zip(y.iter()));
                }
                true
            }
            (Value::Map(x), Value::Map(y)) => {
                if !Rc::ptr_eq(x, y)
                    && first_visit(&mut seen, Rc::as_ptr(x) as usize, Rc::as_ptr(y) as usize, &mut *meter)?
                {
                    if x.len() != y.len() {
                        return Some(false);
                    }
                    if !meter(Charge::Work(x.len().saturating_mul(PAIR_COST).saturating_add(PAIR_COST)))
                        || !room(&mut work, x.len(), &mut *meter)
                    {
                        return None;
                    }
                    for (k, v) in x.iter() {
                        // Looking a key up hashes it.
                        if let Key::Str(s) = k {
                            if s.len() >= 64 && !meter(Charge::Work(s.len())) {
                                return None;
                            }
                        }
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
                        if !meter(Charge::Work(bytes)) {
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
    let mut out = String::new();
    write_float(&mut out, f);
    out
}

/// A buffer on the stack for `write!` (the text of one number).
struct StackText {
    buf: [u8; 64],
    len: usize,
}

impl std::fmt::Write for StackText {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        if end > self.buf.len() {
            return Err(std::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Appends `f` as `format_float` writes it, without allocating: the 14
/// significant digits of `{:.13e}` are placed around the decimal point
/// directly (rounding at the same digit as a second, fixed-point format would).
pub fn write_float(out: &mut String, f: f64) {
    if f.is_nan() {
        return out.push_str("NAN");
    }
    if f.is_infinite() {
        return out.push_str(if f > 0.0 { "INF" } else { "-INF" });
    }
    if f == 0.0 {
        return out.push_str(if f.is_sign_negative() { "-0" } else { "0" });
    }
    let mut sci = StackText { buf: [0; 64], len: 0 };
    let _ = write!(sci, "{:.13e}", f);
    let text = std::str::from_utf8(&sci.buf[..sci.len]).unwrap_or("0e0");
    let (mantissa, exp) = text.split_once('e').unwrap_or((text, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mantissa),
    };
    // The 14 digits, and how many stay after dropping trailing zeros.
    let mut digits = [0u8; 14];
    let mut n = 0;
    for b in mantissa.bytes().filter(u8::is_ascii_digit) {
        if n < digits.len() {
            digits[n] = b;
            n += 1;
        }
    }
    while n > 1 && digits[n - 1] == b'0' {
        n -= 1;
    }
    let digits = &digits[..n];
    if negative {
        out.push('-');
    }
    let push = |out: &mut String, d: &[u8]| out.push_str(std::str::from_utf8(d).unwrap_or(""));
    if !(-4..14).contains(&exp) {
        // `1.5E+20`, `1.0E-5`.
        push(out, &digits[..1]);
        out.push('.');
        if digits.len() > 1 {
            push(out, &digits[1..]);
        } else {
            out.push('0');
        }
        out.push('E');
        out.push(if exp < 0 { '-' } else { '+' });
        let _ = write!(out, "{}", exp.abs());
    } else if exp < 0 {
        // `0.0001`: zeros before the digits.
        out.push_str("0.");
        for _ in 0..(-exp - 1) {
            out.push('0');
        }
        push(out, digits);
    } else {
        let int_len = exp as usize + 1;
        if digits.len() <= int_len {
            push(out, digits);
            for _ in digits.len()..int_len {
                out.push('0');
            }
        } else {
            push(out, &digits[..int_len]);
            out.push('.');
            push(out, &digits[int_len..]);
        }
    }
}

/// Appends an integer without the formatting machinery.
pub fn write_int(out: &mut String, i: i64) {
    let mut buf = [0u8; 20];
    let mut pos = buf.len();
    let mut n = i.unsigned_abs();
    loop {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    if i < 0 {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(&buf[pos..]).unwrap_or(""));
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
/// So are `<` and `>`, which give the same value: a `</noscript>` or
/// `</title>` in it cannot end an element that a browser reads around the
/// attribute as raw text (found by the 1.2 audit).
pub fn attr_quotes_into(s: &str, out: &mut String) {
    replace_bytes_into(s, out, |b| match b {
        b'"' => Some("&quot;"),
        b'\'' => Some("&#039;"),
        b'<' => Some("&lt;"),
        b'>' => Some("&gt;"),
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

    /// `format_float` before 1.1 (two formatting passes), to compare with.
    fn format_float_before(f: f64) -> String {
        fn trim_zeros(s: &str) -> &str {
            if s.contains('.') {
                s.trim_end_matches('0').trim_end_matches('.')
            } else {
                s
            }
        }
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

    /// The text of every float is what it was: random bit patterns (every
    /// exponent), values with few decimals, values near the rounding and
    /// exponent boundaries, and the special values. `PHV_FLOAT_CHECKS` sets
    /// how many random values (default 2,000,000).
    #[test]
    fn floats_print_as_before() {
        let n: u64 = std::env::var("PHV_FLOAT_CHECKS").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000_000);
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let check = |f: f64| assert_eq!(format_float(f), format_float_before(f), "{f:e} ({:#x})", f.to_bits());
        for _ in 0..n {
            check(f64::from_bits(next()));
            let r = next();
            // Numbers people print: a few decimals, at every magnitude near the boundaries.
            let k = (r >> 40) as i64 % 2_000_000 - 1_000_000;
            let scale = 10f64.powi(((r >> 8) % 40) as i32 - 20);
            check(k as f64 * scale);
            check(k as f64 / 1000.0);
            check((r % 100_000) as f64 * 0.5);
        }
        for e in -330..330 {
            let p = 10f64.powi(e);
            for f in
                [p, -p, p * 0.999999999999995, p * 9.99999999999995, p * 1.00000000000005, p.next_up(), p.next_down()]
            {
                check(f);
            }
        }
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.0, f64::MAX, f64::MIN, f64::MIN_POSITIVE, 5e-324]
        {
            check(f);
        }
    }

    #[test]
    fn integers_print_as_before() {
        let mut s = String::new();
        for i in [0, 1, -1, 9, 10, -10, 99, 100, 12345, i64::MAX, i64::MIN, i64::MIN + 1, 1_000_000_000_000] {
            s.clear();
            write_int(&mut s, i);
            assert_eq!(s, i.to_string());
        }
    }

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
        assert_eq!(format_float(9.999999999999999), "10");
        assert_eq!(format_float(-99999999999999.5), "-1.0E+14");
        assert_eq!(format_float(f64::MIN_POSITIVE), "2.2250738585072E-308");
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
