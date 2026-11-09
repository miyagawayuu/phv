//! `json_decode_as()`: JSON text checked against a type the checker describes,
//! and built into that type's values (objects and enum cases included).
//!
//! The description (made by `phv_types`) is a shape on the first line, then one
//! line per class it uses: `cN name:shape,other!:shape,third=K:shape` (`!` =
//! required, `=K` = missing means the K-th of the defaults passed with it).
//! Shapes: `i f s b a`, `?X`, `lX`, `msX` / `miX`, `cN`, `eN` (enum by value),
//! `nN` (pure enum by case name).

use super::{s, web::json_decode_in};
use phv_vm::value::Object;
use phv_vm::vm::const_value;
use phv_vm::{BuiltinFn, Key, Map, Value, Vm, VmError};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub(crate) enum Shape {
    Int,
    Float,
    Str,
    Bool,
    Any,
    Opt(Box<Shape>),
    List(Box<Shape>),
    /// `set<int>` (`int_items`) or `set<string>`: an array of distinct items.
    Set {
        int_items: bool,
    },
    Map {
        int_keys: bool,
        value: Box<Shape>,
    },
    Class(u32),
    Enum {
        class: u32,
        by_name: bool,
    },
}

pub(crate) enum Missing {
    /// Keeps the property's own default.
    Default,
    Required,
    /// The default of the constructor parameter that declares the property.
    Param(usize),
}

pub(crate) struct Prop {
    name: String,
    missing: Missing,
    shape: Shape,
}

pub(crate) fn shape(d: &mut std::str::Chars) -> Option<Shape> {
    Some(match d.next()? {
        'i' => Shape::Int,
        'f' => Shape::Float,
        's' => Shape::Str,
        'b' => Shape::Bool,
        'a' => Shape::Any,
        '?' => Shape::Opt(Box::new(shape(d)?)),
        'l' => Shape::List(Box::new(shape(d)?)),
        't' => Shape::Set { int_items: d.next()? == 'i' },
        'm' => {
            let int_keys = d.next()? == 'i';
            Shape::Map { int_keys, value: Box::new(shape(d)?) }
        }
        c @ ('c' | 'e' | 'n') => {
            let digits: String = d.clone().take_while(char::is_ascii_digit).collect();
            for _ in 0..digits.len() {
                d.next();
            }
            let id = digits.parse().ok()?;
            match c {
                'c' => Shape::Class(id),
                _ => Shape::Enum { class: id, by_name: c == 'n' },
            }
        }
        _ => return None,
    })
}

fn parse_desc(desc: &str) -> Option<(Shape, HashMap<u32, Vec<Prop>>)> {
    let mut lines = desc.split('\n');
    let top = shape(&mut lines.next()?.chars())?;
    Some((top, parse_classes(lines)?))
}

/// The class lines of a description (`cN name:shape,...`).
pub(crate) fn parse_classes<'a>(lines: impl Iterator<Item = &'a str>) -> Option<HashMap<u32, Vec<Prop>>> {
    let mut classes = HashMap::new();
    for line in lines {
        let (id, fields) = line.strip_prefix('c')?.split_once(' ')?;
        let mut props = Vec::new();
        for f in fields.split(',').filter(|f| !f.is_empty()) {
            let (name, sh) = f.split_once(':')?;
            let (name, missing) = match (name.strip_suffix('!'), name.split_once('=')) {
                (Some(n), _) => (n, Missing::Required),
                (None, Some((n, k))) => (n, Missing::Param(k.parse().ok()?)),
                (None, None) => (name, Missing::Default),
            };
            props.push(Prop { name: name.to_string(), missing, shape: shape(&mut sh.chars())? });
        }
        classes.insert(id.parse().ok()?, props);
    }
    Some(classes)
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Int(_) | Value::Float(_) => "a number",
        Value::Str(_) => "a string",
        Value::List(_) => "an array",
        Value::Map(_) => "an object",
        _ => "a value",
    }
}

pub(crate) struct Builder<'a> {
    pub(crate) classes: &'a HashMap<u32, Vec<Prop>>,
    pub(crate) defaults: &'a [Value],
    /// What a mismatch is reported as: the exception class and the start of
    /// its message (`json_decode_as()`).
    pub(crate) error: (&'static str, &'a str),
    /// Where in the document the value being built is: `$.items[2].name`.
    pub(crate) path: String,
    /// Values built so far: the limits are checked every `CHECK_EVERY`.
    pub(crate) built: usize,
}

/// The time and memory limits are checked after this many values (found by
/// the 1.0 audit: the second tree grew to 1.7 times the memory limit).
const CHECK_EVERY: usize = 4096;
/// Memory of one entry of a map or set beyond its values.
const ENTRY: usize = 48;

impl Builder<'_> {
    fn fail(&self, vm: &Vm, what: &str) -> VmError {
        // Keys come from the document: keep a long path from filling the message.
        let path: String = if self.path.chars().count() > 200 {
            let tail: Vec<char> = self.path.chars().rev().take(200).collect();
            format!("...{}", tail.into_iter().rev().collect::<String>())
        } else {
            self.path.clone()
        };
        let (class, start) = self.error;
        vm.throw(class, format!("{start}: {what} at {path}"))
    }

    fn expected(&self, vm: &Vm, want: &str, got: &Value) -> VmError {
        self.fail(vm, &format!("expected {want}, found {}", kind(got)))
    }

    pub(crate) fn build(&mut self, vm: &mut Vm, v: &Value, sh: &Shape) -> Result<Value, VmError> {
        self.built += 1;
        if self.built.is_multiple_of(CHECK_EVERY) {
            vm.check_limits()?;
        }
        match (sh, v) {
            (Shape::Any, v) => Ok(v.clone()),
            (Shape::Opt(_), Value::Null) => Ok(Value::Null),
            (Shape::Opt(inner), v) => self.build(vm, v, inner),
            (Shape::Int, Value::Int(_)) => Ok(v.clone()),
            (Shape::Int, v) => Err(self.expected(vm, "an integer", v)),
            (Shape::Float, Value::Int(i)) => Ok(Value::Float(*i as f64)),
            (Shape::Float, Value::Float(_)) => Ok(v.clone()),
            (Shape::Float, v) => Err(self.expected(vm, "a number", v)),
            (Shape::Str, Value::Str(_)) => Ok(v.clone()),
            (Shape::Str, v) => Err(self.expected(vm, "a string", v)),
            (Shape::Bool, Value::Bool(_)) => Ok(v.clone()),
            (Shape::Bool, v) => Err(self.expected(vm, "true or false", v)),
            (Shape::List(item), Value::List(items)) => {
                let len = self.path.len();
                vm.reserve(items.len().saturating_mul(std::mem::size_of::<Value>()))?;
                let mut out = Vec::with_capacity(items.len());
                for (i, x) in items.iter().enumerate() {
                    self.path.push_str(&format!("[{i}]"));
                    out.push(self.build(vm, x, item)?);
                    self.path.truncate(len);
                }
                Ok(Value::list(out))
            }
            (Shape::List(_), v) => Err(self.expected(vm, "an array", v)),
            (Shape::Set { int_items }, Value::List(items)) => {
                let len = self.path.len();
                vm.reserve(items.len().saturating_mul(ENTRY))?;
                let mut out = phv_vm::KeySet::with_capacity(items.len());
                for (i, x) in items.iter().enumerate() {
                    let key = match (int_items, x) {
                        (true, Value::Int(n)) => Key::Int(*n),
                        (false, Value::Str(s)) => Key::Str(s.clone()),
                        _ => {
                            self.path.push_str(&format!("[{i}]"));
                            let want = if *int_items { "an integer" } else { "a string" };
                            return Err(self.expected(vm, want, x));
                        }
                    };
                    out.insert(key);
                }
                self.path.truncate(len);
                Ok(Value::set(out))
            }
            (Shape::Set { .. }, v) => Err(self.expected(vm, "an array", v)),
            (Shape::Map { int_keys, value }, Value::Map(m)) => {
                let len = self.path.len();
                vm.reserve(m.len().saturating_mul(ENTRY + std::mem::size_of::<Value>()))?;
                let mut out = Map::with_capacity(m.len());
                for (k, x) in m.iter() {
                    let name = match k {
                        Key::Str(s) => s.to_string(),
                        Key::Int(i) => i.to_string(),
                    };
                    self.path.push_str(&format!("[{}]", json_quote(&name)));
                    let key = if *int_keys {
                        match name.parse::<i64>() {
                            Ok(i) if i.to_string() == name => Key::Int(i),
                            _ => return Err(self.fail(vm, "expected an integer key")),
                        }
                    } else {
                        Key::Str(Rc::new(name))
                    };
                    out.insert(key, self.build(vm, x, value)?);
                    self.path.truncate(len);
                }
                Ok(Value::map(out))
            }
            (Shape::Map { .. }, v) => Err(self.expected(vm, "an object", v)),
            (Shape::Class(cid), Value::Map(m)) => self.object(vm, *cid, m),
            (Shape::Class(cid), v) => {
                let want = format!("an object for {}", vm.prog.classes[*cid as usize].name);
                Err(self.expected(vm, &want, v))
            }
            (Shape::Enum { class, by_name }, v) => {
                let prog = vm.prog.clone();
                let cls = &prog.classes[*class as usize];
                let found = cls.cases.iter().position(|(name, value)| match (v, by_name) {
                    (Value::Str(s), true) => s.as_str() == name,
                    (_, true) => false,
                    (v, false) => match (v, &const_value(value)) {
                        (Value::Int(a), Value::Int(b)) => a == b,
                        (Value::Str(a), Value::Str(b)) => a == b,
                        _ => false,
                    },
                });
                match found {
                    Some(i) => Ok(vm.enum_case(*class, i as u32)),
                    None => {
                        let what = if *by_name { "a case name" } else { "a value" };
                        Err(self.fail(vm, &format!("{} is not {what} of {}", short(v), cls.name)))
                    }
                }
            }
        }
    }

    fn object(&mut self, vm: &mut Vm, cid: u32, m: &Map) -> Result<Value, VmError> {
        let prog = vm.prog.clone();
        let cls = &prog.classes[cid as usize];
        let mut fields: Vec<Value> = cls.fields.iter().map(|f| const_value(&f.default)).collect();
        let classes = self.classes;
        let Some(props) = classes.get(&cid) else {
            return Err(vm.throw("RuntimeException", "json_decode_as(): bad type description"));
        };
        let len = self.path.len();
        for p in props {
            let Some(slot) = cls.fields.iter().position(|f| f.name == p.name) else {
                return Err(vm.throw("RuntimeException", "json_decode_as(): bad type description"));
            };
            match m.get(&Key::Str(Rc::new(p.name.clone()))) {
                Some(x) => {
                    self.path.push('.');
                    self.path.push_str(&p.name);
                    fields[slot] = self.build(vm, x, &p.shape)?;
                    self.path.truncate(len);
                }
                None => match p.missing {
                    Missing::Required => {
                        return Err(self.fail(vm, &format!("missing property `{}` of {}", p.name, cls.name)));
                    }
                    Missing::Param(k) => match self.defaults.get(k) {
                        Some(d) => fields[slot] = d.clone(),
                        None => return Err(vm.throw("RuntimeException", "json_decode_as(): bad type description")),
                    },
                    // The property's own default (null when it has none).
                    Missing::Default => {}
                },
            }
        }
        let o = Rc::new(RefCell::new(Object { class: cid, fields }));
        vm.track(&o);
        Ok(Value::Object(o))
    }
}

fn json_quote(s: &str) -> String {
    format!("{s:?}")
}

fn short(v: &Value) -> String {
    match v {
        Value::Str(s) if s.chars().count() <= 40 => json_quote(s),
        Value::Str(_) => "a long string".into(),
        Value::Int(i) => i.to_string(),
        v => kind(v).into(),
    }
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__json_as" => |vm, a| {
            let Some((top, classes)) = parse_desc(s(&a[1])) else {
                return Err(vm.throw("RuntimeException", "json_decode_as(): bad type description"));
            };
            let v =
                json_decode_in(vm, s(&a[0]))?.map_err(|e| vm.throw("ValueError", format!("json_decode_as(): {e}")))?;
            let defaults = super::list(&a[2]).clone();
            let error = ("ValueError", "json_decode_as()");
            Builder { classes: &classes, defaults: &defaults, error, path: "$".into(), built: 0 }.build(vm, &v, &top)
        },
        _ => return None,
    };
    Some(f)
}
