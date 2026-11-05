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

use crate::value::{Closure, Key, Map, Object, Value};
use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::{Rc, Weak};

/// Hashes the addresses the collector indexes values by. They are not
/// chosen by scripts, so a fast multiplicative hash is enough.
#[derive(Default)]
struct AddrHasher(u64);

impl Hasher for AddrHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(b as u64);
        }
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    fn finish(&self) -> u64 {
        // The low bits of an address are always zero (alignment); fold the
        // high bits, which the multiplication mixed, into them.
        self.0 ^ (self.0 >> 29)
    }
}

type AddrMap = HashMap<usize, usize, BuildHasherDefault<AddrHasher>>;

#[derive(Clone, Copy)]
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

/// The values inside a node that can take part in a cycle.
enum Children<'a> {
    Slice(std::slice::Iter<'a, Value>),
    Map(indexmap::map::Values<'a, Key, Value>),
}

impl<'a> Iterator for Children<'a> {
    type Item = &'a Value;

    #[inline]
    fn next(&mut self) -> Option<&'a Value> {
        match self {
            Children::Slice(i) => i.next(),
            Children::Map(i) => i.next(),
        }
    }
}

fn children<'a>(node: &Node<'a>) -> Children<'a> {
    match *node {
        Node::Object(o) => Children::Slice(o.fields.iter()),
        Node::List(l) => Children::Slice(l.iter()),
        Node::Map(m) => Children::Map(m.values()),
        Node::Closure(c) => Children::Slice(c.caps.iter()),
        Node::Opaque => Children::Slice([].iter()),
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
        let mut index: AddrMap = AddrMap::with_capacity_and_hasher(live.len(), Default::default());
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
