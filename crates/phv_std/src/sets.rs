//! `set<T>`: distinct `int` or `string` values in the order they were added.
//! Like list and map methods, set methods return new values and leave the
//! receiver as it is; `$s[] = $x` adds in place.

use super::{list, truthy};
use phv_vm::{BuiltinFn, Key, KeySet, Value, Vm, VmError};
use std::rc::Rc;

/// Memory of one element: the key and its slot in the hash table.
const ITEM: usize = std::mem::size_of::<Key>() + 16;

/// The time limit is checked every this many elements, and after this many
/// bytes of strings hashed.
const CHECK_EVERY: usize = 1024;
const CHECK_BYTES: usize = 16 << 20;

fn set(v: &Value) -> &Rc<KeySet> {
    match v {
        Value::Set(s) => s,
        v => panic!("builtin expected set, got {}", v.type_name()),
    }
}

/// Counts the work of hashing elements and checks the limits now and then.
struct Meter {
    items: usize,
    bytes: usize,
}

impl Meter {
    fn new() -> Meter {
        Meter { items: 0, bytes: 0 }
    }

    fn step(&mut self, vm: &mut Vm, k: &Key) -> Result<(), VmError> {
        self.items += 1;
        if let Key::Str(s) = k {
            self.bytes += s.len();
        }
        if self.items.is_multiple_of(CHECK_EVERY) || self.bytes >= CHECK_BYTES {
            self.bytes = 0;
            vm.check_limits()?;
        }
        Ok(())
    }
}

/// A new set with room for `n` elements, accounted before it is made.
fn with_room(vm: &mut Vm, n: usize) -> Result<KeySet, VmError> {
    vm.reserve(n.saturating_mul(ITEM))?;
    Ok(KeySet::with_capacity(n))
}

/// The elements of `a` for which `keep` holds.
fn retain(vm: &mut Vm, a: &KeySet, mut keep: impl FnMut(&Key) -> bool) -> Result<Value, VmError> {
    let mut out = with_room(vm, a.len())?;
    let mut m = Meter::new();
    for k in a.iter() {
        m.step(vm, k)?;
        if keep(k) {
            out.insert(k.clone());
        }
    }
    Ok(Value::set(out))
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "set_from_list" => |vm, a| {
            let items = list(&a[0]);
            let mut out = with_room(vm, items.len())?;
            let mut m = Meter::new();
            for x in items.iter() {
                let k = x.to_key();
                m.step(vm, &k)?;
                out.insert(k);
            }
            Ok(Value::set(out))
        },
        "set_len" => |_, a| Ok(Value::Int(set(&a[0]).len() as i64)),
        "set_is_empty" => |_, a| Ok(Value::Bool(set(&a[0]).is_empty())),
        "set_has" => |_, a| Ok(Value::Bool(set(&a[0]).contains(&a[1].to_key()))),
        "set_add" => |vm, a| {
            let s = set(&a[0]);
            let k = a[1].to_key();
            if s.contains(&k) {
                return Ok(a[0].clone());
            }
            let mut out = with_room(vm, s.len() + 1)?;
            out.extend(s.iter().cloned());
            out.insert(k);
            Ok(Value::set(out))
        },
        "set_remove" => |vm, a| {
            let k = a[1].to_key();
            if !set(&a[0]).contains(&k) {
                return Ok(a[0].clone());
            }
            retain(vm, set(&a[0]), |x| *x != k)
        },
        "set_union" => |vm, a| {
            let (x, y) = (set(&a[0]), set(&a[1]));
            let mut out = with_room(vm, x.len().saturating_add(y.len()))?;
            let mut m = Meter::new();
            for k in x.iter().chain(y.iter()) {
                m.step(vm, k)?;
                out.insert(k.clone());
            }
            Ok(Value::set(out))
        },
        "set_intersect" => |vm, a| {
            let other = Rc::clone(set(&a[1]));
            retain(vm, set(&a[0]), |k| other.contains(k))
        },
        "set_diff" => |vm, a| {
            let other = Rc::clone(set(&a[1]));
            retain(vm, set(&a[0]), |k| !other.contains(k))
        },
        "set_is_subset" => |vm, a| {
            let (x, y) = (set(&a[0]), set(&a[1]));
            let mut m = Meter::new();
            for k in x.iter() {
                m.step(vm, k)?;
                if !y.contains(k) {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        },
        "set_to_list" => |vm, a| {
            let s = set(&a[0]);
            vm.reserve(s.len().saturating_mul(std::mem::size_of::<Value>()))?;
            Ok(Value::list(s.iter().map(Key::to_value).collect()))
        },
        "set_filter" => |vm, a| {
            let f = vm.callback(&a[1]);
            let s = Rc::clone(set(&a[0]));
            let mut out = with_room(vm, s.len())?;
            for k in s.iter() {
                let hit = f.call(vm, &[k.to_value()])?;
                if truthy(vm, hit)? {
                    out.insert(k.clone());
                }
            }
            Ok(Value::set(out))
        },
        "set_any" => |vm, a| {
            let f = vm.callback(&a[1]);
            let s = Rc::clone(set(&a[0]));
            for k in s.iter() {
                let hit = f.call(vm, &[k.to_value()])?;
                if truthy(vm, hit)? {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        },
        "set_all" => |vm, a| {
            let f = vm.callback(&a[1]);
            let s = Rc::clone(set(&a[0]));
            for k in s.iter() {
                let hit = f.call(vm, &[k.to_value()])?;
                if !truthy(vm, hit)? {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        },
        _ => return None,
    };
    Some(f)
}
