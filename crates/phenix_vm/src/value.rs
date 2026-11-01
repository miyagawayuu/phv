use indexmap::IndexMap;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt::Write;
use std::rc::Rc;

pub type Map = IndexMap<Key, Value>;

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
            Value::Object(_) => "object",
            Value::Closure(_) => "closure",
        }
    }

    /// Converts a map key value (int or string) to a `Key`.
    pub fn to_key(&self) -> Key {
        match self {
            Value::Int(i) => Key::Int(*i),
            Value::Str(s) => Key::Str(s.clone()),
            other => panic!("phenix vm: invalid map key of type {}", other.type_name()),
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

/// Strict equality: values of different types are never equal, except that
/// `int` and `float` compare numerically. Objects compare by identity.
pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => (*x as f64) == *y,
        (Value::Str(x), Value::Str(y)) | (Value::Html(x), Value::Html(y)) => x == y,
        (Value::List(x), Value::List(y)) => {
            Rc::ptr_eq(x, y) || (x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| values_equal(a, b)))
        }
        (Value::Map(x), Value::Map(y)) => {
            Rc::ptr_eq(x, y)
                || (x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| values_equal(v, w))))
        }
        (Value::Object(x), Value::Object(y)) => Rc::ptr_eq(x, y),
        (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

/// Ordering for `<`, `<=`, `<=>` and sorting. The type checker only allows
/// numbers with numbers and strings with strings.
pub fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Ordering::Equal),
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

pub fn html_escape_into(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#039;"),
            c => out.push(c),
        }
    }
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
    fn escapes_html() {
        assert_eq!(html_escape("<a href=\"x\">'&'</a>"), "&lt;a href=&quot;x&quot;&gt;&#039;&amp;&#039;&lt;/a&gt;");
    }
}
