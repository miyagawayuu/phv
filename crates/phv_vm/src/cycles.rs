//! Frees reference cycles while a script runs.
//!
//! Values are reference counted, so a cycle (`$node->next = $node`, or an
//! object holding a closure that captured it) keeps itself alive. Objects are
//! the only values that can be changed after they are made (lists, maps and
//! closures are copied on write), so every cycle passes through an object.
//!
//! The collector uses trial deletion. Starting from the objects the VM made,
//! it walks every list, map, closure and object they reach, and counts how
//! many of each one's references come from inside that graph. A value with
//! more references than that is also held from outside (a register, a frame,
//! a builtin): it is live, and so is everything it reaches. The objects left
//! over are reachable only from each other; their fields are cleared, which
//! frees the cycle.
//!
//! The walk is conservative: an object the VM did not record, or one that is
//! borrowed at the moment, is treated as held from outside and is not looked
//! into, so a value can be kept too long but is never freed while in use.

use crate::value::{Closure, Map, Object, Value};
use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

enum Node<'a> {
    Object(&'a Object),
    List(&'a [Value]),
    Map(&'a Map),
    Closure(&'a Closure),
    /// An object that could not be looked into: it is kept, with all it reaches.
    Opaque,
}

struct Entry<'a> {
    node: Node<'a>,
    /// References to this value (`Rc::strong_count`), minus the collector's own.
    strong: usize,
    /// References to it from values in the graph.
    internal: usize,
}

/// The values inside `node` that can take part in a cycle.
fn children<'a>(node: &Node<'a>) -> Box<dyn Iterator<Item = &'a Value> + 'a> {
    match *node {
        Node::Object(o) => Box::new(o.fields.iter()),
        Node::List(l) => Box::new(l.iter()),
        Node::Map(m) => Box::new(m.values()),
        Node::Closure(c) => Box::new(c.caps.iter()),
        Node::Opaque => Box::new(std::iter::empty()),
    }
}

/// The address and the reference count of a value that can hold others.
fn identity(v: &Value) -> Option<(usize, usize)> {
    match v {
        Value::Object(o) => Some((Rc::as_ptr(o) as *const u8 as usize, Rc::strong_count(o))),
        Value::List(l) => Some((Rc::as_ptr(l) as *const u8 as usize, Rc::strong_count(l))),
        Value::Map(m) => Some((Rc::as_ptr(m) as *const u8 as usize, Rc::strong_count(m))),
        Value::Closure(c) => Some((Rc::as_ptr(c) as *const u8 as usize, Rc::strong_count(c))),
        _ => None,
    }
}

fn node_of(v: &Value) -> Node<'_> {
    match v {
        Value::List(l) => Node::List(l),
        Value::Map(m) => Node::Map(m),
        Value::Closure(c) => Node::Closure(c),
        _ => Node::Opaque,
    }
}

/// What one collection did.
pub(crate) struct Collected {
    /// Objects whose fields were cleared.
    pub freed: usize,
    /// Values looked at inside live values: the part of the walk that the
    /// next one repeats (garbage is walked once, then freed).
    pub visited: usize,
}

/// Clears the fields of every recorded object that is reachable only through
/// a cycle. `objects` is the VM's record of the objects it made; entries for
/// objects that are gone are dropped.
pub(crate) fn collect(objects: &mut Vec<Weak<RefCell<Object>>>) -> Collected {
    let mut visited = 0;
    objects.retain(|w| w.strong_count() > 0);
    let live: Vec<Rc<RefCell<Object>>> = objects.iter().filter_map(Weak::upgrade).collect();
    let garbage: Vec<usize> = {
        // Borrow every object for the whole walk; one that is borrowed mutably
        // right now is opaque.
        let guards: Vec<Option<Ref<'_, Object>>> = live.iter().map(|o| o.try_borrow().ok()).collect();
        let mut entries: Vec<Entry<'_>> = Vec::with_capacity(live.len());
        let mut index: HashMap<usize, usize> = HashMap::with_capacity(live.len());
        for (o, g) in live.iter().zip(&guards) {
            index.insert(Rc::as_ptr(o) as *const u8 as usize, entries.len());
            let node = match g {
                Some(g) => Node::Object(g),
                None => Node::Opaque,
            };
            // `live` holds one reference of its own.
            entries.push(Entry { node, strong: Rc::strong_count(o) - 1, internal: 0 });
        }

        // Count the references from inside the graph, adding the lists, maps
        // and closures it reaches as they are found.
        let mut next = 0;
        while next < entries.len() {
            // The values borrowed from the objects, not from `entries`.
            for v in children(&entries[next].node) {
                let Some((ptr, strong)) = identity(v) else { continue };
                let i = match index.get(&ptr) {
                    Some(&i) => i,
                    // An object the VM did not record is held from outside.
                    None if matches!(v, Value::Object(_)) => continue,
                    None => {
                        index.insert(ptr, entries.len());
                        entries.push(Entry { node: node_of(v), strong, internal: 0 });
                        entries.len() - 1
                    }
                };
                entries[i].internal += 1;
            }
            next += 1;
        }

        // Everything reachable from a value held from outside is live.
        let mut live_mark = vec![false; entries.len()];
        let mut stack: Vec<usize> = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            if e.strong > e.internal || matches!(e.node, Node::Opaque) {
                live_mark[i] = true;
                stack.push(i);
            }
        }
        while let Some(i) = stack.pop() {
            for v in children(&entries[i].node) {
                visited += 1;
                let Some((ptr, _)) = identity(v) else { continue };
                if let Some(&j) = index.get(&ptr) {
                    if !live_mark[j] {
                        live_mark[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        // The recorded objects come first in `entries`, in the order of `live`.
        (0..live.len()).filter(|&i| !live_mark[i]).collect()
    };

    // The borrows have ended. Take the fields out first and drop them after,
    // so that no object is borrowed while values are freed.
    let freed: Vec<Vec<Value>> = garbage.iter().map(|&i| std::mem::take(&mut live[i].borrow_mut().fields)).collect();
    let n = freed.len();
    drop(freed);
    drop(live);
    objects.retain(|w| w.strong_count() > 0);
    Collected { freed: n, visited }
}
