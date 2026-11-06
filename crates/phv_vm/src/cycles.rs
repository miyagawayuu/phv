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
//!
//! The walk's own memory counts against the script's memory limit (found by
//! the 1.1.1 audit: a list of 1.5 million objects took 190 MB more to walk,
//! 321 MB under a 128 MB limit). It is given a budget: when the walk of every
//! object would not fit, it walks the newest ones that do (those made since
//! the last walk, where new garbage is); an object outside the walk is held
//! from outside, as above. A walk that outgrows its budget (the lists and maps
//! it reaches) stops and frees nothing.

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
    strong: u32,
    /// References to it from values in the graph.
    internal: u32,
}

/// The most the walk allocates for each recorded object it looks at: its
/// strong reference (8 bytes) and borrow (16), its entry (32), its place in
/// the index (about 40 with the table's spare room), its mark (1), and its
/// place on the marking stack (8) and in the list of garbage (8).
pub(crate) const BYTES_PER_OBJECT: usize = 120;

/// The most the walk allocates for each list, map or closure it adds as it
/// goes (the same, without the strong reference and the borrow).
const BYTES_PER_VALUE: usize = 96;

/// A count of references as an entry holds it. More than `u32::MAX` would take
/// 64 GB of values; a count cut to it still reads as held from outside.
fn count(n: usize) -> u32 {
    n.min(u32::MAX as usize) as u32
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
    /// Values looked at inside live values, and the records of the objects:
    /// the part of the walk that the next one repeats (garbage is walked
    /// once, then freed).
    pub visited: usize,
}

/// Clears the fields of every recorded object that is reachable only through
/// a cycle. `objects` is the VM's record of the objects it made; entries for
/// objects that are gone are dropped. The walk allocates at most `budget`
/// bytes (see the module's notes).
pub(crate) fn collect(objects: &mut Vec<Weak<RefCell<Object>>>, budget: usize) -> Collected {
    // The record is looked at whole, however few objects are walked.
    let mut visited = objects.len();
    objects.retain(|w| w.strong_count() > 0);
    // The newest objects the budget allows (the record is in the order they were made).
    let from = objects.len() - objects.len().min(budget / BYTES_PER_OBJECT);
    let start = crate::alloc::thread_allocated();
    // Whether `more` bytes still fit in the budget, with what the walk holds now.
    let fits =
        |more: usize| ((crate::alloc::thread_allocated() - start).max(0) as usize).saturating_add(more) <= budget;
    let mut live: Vec<Rc<RefCell<Object>>> = Vec::with_capacity(objects.len() - from);
    live.extend(objects[from..].iter().filter_map(Weak::upgrade));
    let garbage: Option<Vec<usize>> = 'walk: {
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
            entries.push(Entry { node, strong: count(Rc::strong_count(o) - 1), internal: 0 });
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
                    // An object the VM did not record (or not in this walk) is held from outside.
                    None if matches!(v, Value::Object(_)) => continue,
                    None => {
                        // The entries and the index grow to twice their size.
                        let full = entries.len() == entries.capacity() || index.len() == index.capacity();
                        if full && !fits(entries.len().saturating_mul(2 * BYTES_PER_VALUE)) {
                            break 'walk None;
                        }
                        index.insert(ptr, entries.len());
                        entries.push(Entry { node: node_of(v), strong: count(strong), internal: 0 });
                        entries.len() - 1
                    }
                };
                entries[i].internal = entries[i].internal.saturating_add(1);
            }
            next += 1;
        }

        // The marks, the marking stack and the list of garbage.
        // (Each is allocated at its full size at once: growing one would hold
        // the old and the new allocation.)
        if !fits(entries.len().saturating_mul(1 + 8 + 8)) {
            break 'walk None;
        }
        // Everything reachable from a value held from outside is live.
        let mut live_mark = vec![false; entries.len()];
        let mut stack: Vec<usize> = Vec::with_capacity(entries.len());
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
        let mut garbage = Vec::with_capacity(live_mark[..live.len()].iter().filter(|m| !**m).count());
        garbage.extend((0..live.len()).filter(|&i| !live_mark[i]));
        Some(garbage)
    };
    // A walk that did not fit frees nothing.
    let garbage = garbage.unwrap_or_default();

    // The borrows have ended. Take the fields out first and drop them after,
    // so that no object is borrowed while values are freed.
    let freed: Vec<Vec<Value>> = garbage.iter().map(|&i| std::mem::take(&mut live[i].borrow_mut().fields)).collect();
    let n = freed.len();
    drop(freed);
    drop(live);
    objects.retain(|w| w.strong_count() > 0);
    Collected { freed: n, visited }
}
