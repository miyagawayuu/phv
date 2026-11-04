//! More list and map methods: `reduce`, `flatMap`, `groupBy`, `chunk`, `zip`,
//! `partition`, `count`, `min` / `max`, `removeAt` / `insertAt`, `flatten`,
//! and for maps `remove`, `filter`, `map`, `sortedByKey` / `sortedByValue`.
//! Like the other methods, they return new values and leave the receiver as it is.

use super::{int, list, map, truthy};
use indexmap::IndexMap;
use phv_vm::value::compare;
use phv_vm::{BuiltinFn, Value, Vm};

/// The time limit is checked every this many callback calls (a callback may be cheap).
const CHECK_EVERY: usize = 1024;

fn tick(vm: &mut Vm, i: usize) -> Result<(), phv_vm::VmError> {
    if i % CHECK_EVERY == CHECK_EVERY - 1 {
        vm.check_limits()?;
    }
    Ok(())
}

fn index_error(vm: &mut Vm, i: i64, len: usize) -> phv_vm::VmError {
    vm.throw("IndexError", format!("list index {i} is out of range (length {len})"))
}

/// Memory of one item of a list.
const VALUE: usize = std::mem::size_of::<Value>();
/// Memory of a new list (its shared header) or a map entry, beyond its items.
const GROUP: usize = 64;

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "list_reduce" => |vm, a| {
            let f = a[2].clone();
            let mut acc = a[1].clone();
            for (i, x) in list(&a[0]).iter().enumerate() {
                tick(vm, i)?;
                acc = vm.call_value(&f, &[acc, x.clone()])?;
            }
            Ok(acc)
        },
        "list_flat_map" => |vm, a| {
            let f = a[1].clone();
            let mut out = Vec::new();
            for x in list(&a[0]).iter() {
                let part = vm.call_value(&f, std::slice::from_ref(x))?;
                let part = list(&part);
                // The parts may all be one shared list: count what copying adds.
                vm.reserve(part.len().saturating_mul(VALUE))?;
                out.reserve(part.len());
                out.extend(part.iter().cloned());
            }
            Ok(Value::list(out))
        },
        "list_flatten" => |vm, a| {
            let xs = list(&a[0]);
            let total = xs.iter().fold(0usize, |n, x| n.saturating_add(list(x).len()));
            vm.reserve(total.saturating_mul(VALUE))?;
            let mut out = Vec::with_capacity(total);
            for x in xs.iter() {
                out.extend(list(x).iter().cloned());
            }
            Ok(Value::list(out))
        },
        "list_group_by" => |vm, a| {
            let f = a[1].clone();
            let mut groups: IndexMap<phv_vm::Key, Vec<Value>> = IndexMap::new();
            let xs = list(&a[0]);
            // Each item may start a group of its own.
            vm.reserve(xs.len().saturating_mul(VALUE + GROUP))?;
            for (i, x) in xs.iter().enumerate() {
                tick(vm, i)?;
                let key = vm.call_value(&f, std::slice::from_ref(x))?;
                groups.entry(key.to_key()).or_default().push(x.clone());
            }
            Ok(Value::map(groups.into_iter().map(|(k, v)| (k, Value::list(v))).collect()))
        },
        "list_chunk" => |vm, a| {
            let n = int(&a[1]);
            if n < 1 {
                return Err(vm.throw("ValueError", "chunk(): the size must be at least 1"));
            }
            let size = usize::try_from(n).unwrap_or(usize::MAX);
            let xs = list(&a[0]);
            let chunks = xs.len().div_ceil(size);
            vm.reserve(xs.len().saturating_mul(VALUE).saturating_add(chunks.saturating_mul(VALUE + GROUP)))?;
            Ok(Value::list(xs.chunks(size).map(|c| Value::list(c.to_vec())).collect()))
        },
        "list_zip" => |vm, a| {
            let f = a[2].clone();
            let (xs, ys) = (list(&a[0]).clone(), list(&a[1]).clone());
            let mut out = Vec::with_capacity(xs.len().min(ys.len()));
            for (x, y) in xs.iter().zip(ys.iter()) {
                out.push(vm.call_value(&f, &[x.clone(), y.clone()])?);
            }
            Ok(Value::list(out))
        },
        "list_partition" => |vm, a| {
            let f = a[1].clone();
            let (mut yes, mut no) = (Vec::new(), Vec::new());
            for x in list(&a[0]).iter() {
                let keep = vm.call_value(&f, std::slice::from_ref(x))?;
                if truthy(vm, keep)? {
                    yes.push(x.clone());
                } else {
                    no.push(x.clone());
                }
            }
            Ok(Value::list(vec![Value::list(yes), Value::list(no)]))
        },
        "list_count" => |vm, a| {
            let f = a[1].clone();
            let mut n = 0;
            for x in list(&a[0]).iter() {
                let hit = vm.call_value(&f, std::slice::from_ref(x))?;
                if truthy(vm, hit)? {
                    n += 1;
                }
            }
            Ok(Value::Int(n))
        },
        "list_min" => |_, a| Ok(list(&a[0]).iter().min_by(|x, y| compare(x, y)).cloned().unwrap_or(Value::Null)),
        "list_max" => |_, a| Ok(list(&a[0]).iter().max_by(|x, y| compare(x, y)).cloned().unwrap_or(Value::Null)),
        "list_remove_at" => |vm, a| {
            let (items, i) = (list(&a[0]), int(&a[1]));
            if i < 0 || i as usize >= items.len() {
                return Err(index_error(vm, i, items.len()));
            }
            let mut out = items.as_ref().clone();
            out.remove(i as usize);
            Ok(Value::list(out))
        },
        "list_insert_at" => |vm, a| {
            let (items, i) = (list(&a[0]), int(&a[1]));
            if i < 0 || i as usize > items.len() {
                return Err(index_error(vm, i, items.len()));
            }
            let mut out = items.as_ref().clone();
            out.insert(i as usize, a[2].clone());
            Ok(Value::list(out))
        },
        "map_remove" => |_, a| {
            let mut m = map(&a[0]).as_ref().clone();
            m.shift_remove(&a[1].to_key());
            Ok(Value::map(m))
        },
        "map_filter" => |vm, a| {
            let f = a[1].clone();
            let mut out = IndexMap::new();
            for (k, v) in map(&a[0]).iter() {
                let keep = vm.call_value(&f, &[k.to_value(), v.clone()])?;
                if truthy(vm, keep)? {
                    out.insert(k.clone(), v.clone());
                }
            }
            Ok(Value::map(out))
        },
        "map_map_values" => |vm, a| {
            let f = a[1].clone();
            let mut out = IndexMap::new();
            for (k, v) in map(&a[0]).iter() {
                out.insert(k.clone(), vm.call_value(&f, std::slice::from_ref(v))?);
            }
            Ok(Value::map(out))
        },
        "map_sorted_by_key" => |_, a| {
            let entries: Vec<(phv_vm::Key, Value)> = map(&a[0]).iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let entries = super::stable_sort(entries, &|x, y| super::sort_le(&x.0.to_value(), &y.0.to_value()));
            Ok(Value::map(entries.into_iter().collect()))
        },
        "map_sorted_by_value" => |vm, a| {
            let entries: Vec<(phv_vm::Key, Value)> = map(&a[0]).iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            // Stable: equal values keep their order.
            let entries = super::sort_metered(vm, entries, |e| &e.1)?;
            Ok(Value::map(entries.into_iter().collect()))
        },
        _ => return None,
    };
    Some(f)
}
