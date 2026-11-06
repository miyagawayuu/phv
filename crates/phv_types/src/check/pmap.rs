//! A persistent map with integer keys (a big-endian Patricia trie whose nodes
//! are shared). A copy takes constant time, a change copies only the path to
//! its key, and joining two maps that come from the same one skips the parts
//! they still share: the cost follows what changed, not the map's size.
//!
//! The checker copies its narrowings at each branch and joins them after;
//! with a plain map that took time in proportion to every variable in scope.

use std::rc::Rc;

/// A key: its order as a `u64` is the map's order.
pub(crate) trait Key: Copy {
    fn bits(self) -> u64;
}

impl Key for u32 {
    fn bits(self) -> u64 {
        self as u64
    }
}

impl Key for (u32, u32) {
    fn bits(self) -> u64 {
        ((self.0 as u64) << 32) | self.1 as u64
    }
}

enum Node<V> {
    Leaf(u64, V),
    /// Keys whose bits above `bit` are `prefix`; those with `bit` clear on the left.
    Branch {
        prefix: u64,
        bit: u64,
        left: Rc<Node<V>>,
        right: Rc<Node<V>>,
    },
}

pub(crate) struct PMap<K, V> {
    root: Option<Rc<Node<V>>>,
    _key: std::marker::PhantomData<K>,
}

impl<K, V> Clone for PMap<K, V> {
    fn clone(&self) -> Self {
        PMap { root: self.root.clone(), _key: std::marker::PhantomData }
    }
}

impl<K, V> Default for PMap<K, V> {
    fn default() -> Self {
        PMap { root: None, _key: std::marker::PhantomData }
    }
}

/// The bits of `k` above `bit`.
fn mask(k: u64, bit: u64) -> u64 {
    k & (!(bit - 1) ^ bit)
}

fn link<V>(k1: u64, t1: Rc<Node<V>>, k2: u64, t2: Rc<Node<V>>) -> Rc<Node<V>> {
    let bit = 1u64 << (63 - (k1 ^ k2).leading_zeros());
    let prefix = mask(k1, bit);
    let (left, right) = if k1 & bit == 0 { (t1, t2) } else { (t2, t1) };
    Rc::new(Node::Branch { prefix, bit, left, right })
}

/// A branch of `left` and `right`, either of which may have gone; `orig` when both are unchanged.
fn rebuild<V>(
    orig: &Rc<Node<V>>,
    prefix: u64,
    bit: u64,
    left: Option<Rc<Node<V>>>,
    right: Option<Rc<Node<V>>>,
) -> Option<Rc<Node<V>>> {
    let Node::Branch { left: l0, right: r0, .. } = &**orig else { unreachable!() };
    match (left, right) {
        (None, None) => None,
        (Some(l), None) => Some(l),
        (None, Some(r)) => Some(r),
        (Some(l), Some(r)) if Rc::ptr_eq(&l, l0) && Rc::ptr_eq(&r, r0) => Some(orig.clone()),
        (Some(left), Some(right)) => Some(Rc::new(Node::Branch { prefix, bit, left, right })),
    }
}

fn get<V>(mut t: &Rc<Node<V>>, k: u64) -> Option<&V> {
    loop {
        match &**t {
            Node::Leaf(j, v) => return (*j == k).then_some(v),
            Node::Branch { prefix, bit, left, right } => {
                if mask(k, *bit) != *prefix {
                    return None;
                }
                t = if k & bit == 0 { left } else { right };
            }
        }
    }
}

fn insert<V: PartialEq>(t: &Rc<Node<V>>, k: u64, v: V) -> Rc<Node<V>> {
    match &**t {
        Node::Leaf(j, old) if *j == k => {
            if *old == v {
                t.clone()
            } else {
                Rc::new(Node::Leaf(k, v))
            }
        }
        Node::Leaf(j, _) => link(k, Rc::new(Node::Leaf(k, v)), *j, t.clone()),
        Node::Branch { prefix, bit, left, right } => {
            if mask(k, *bit) != *prefix {
                return link(k, Rc::new(Node::Leaf(k, v)), *prefix, t.clone());
            }
            let (l, r) =
                if k & bit == 0 { (insert(left, k, v), right.clone()) } else { (left.clone(), insert(right, k, v)) };
            if Rc::ptr_eq(&l, left) && Rc::ptr_eq(&r, right) {
                return t.clone();
            }
            Rc::new(Node::Branch { prefix: *prefix, bit: *bit, left: l, right: r })
        }
    }
}

/// `t` without the keys from `lo` to `hi`.
fn remove_range<V>(t: &Rc<Node<V>>, lo: u64, hi: u64) -> Option<Rc<Node<V>>> {
    match &**t {
        Node::Leaf(k, _) => (*k < lo || *k > hi).then(|| t.clone()),
        Node::Branch { prefix, bit, left, right } => {
            let (min, max) = (*prefix, *prefix | (bit - 1) | bit);
            if max < lo || min > hi {
                return Some(t.clone());
            }
            if lo <= min && max <= hi {
                return None;
            }
            let l = remove_range(left, lo, hi);
            let r = remove_range(right, lo, hi);
            rebuild(t, *prefix, *bit, l, r)
        }
    }
}

/// The keys of both maps, with `join` of the two values (kept as in `a` when equal).
fn intersect<V: Clone + PartialEq>(
    a: &Rc<Node<V>>,
    b: &Rc<Node<V>>,
    join: &impl Fn(&V, &V) -> V,
) -> Option<Rc<Node<V>>> {
    if Rc::ptr_eq(a, b) {
        return Some(a.clone());
    }
    let leaf = |k: u64, va: &V, vb: &V, orig: Option<&Rc<Node<V>>>| {
        if va == vb {
            orig.cloned().unwrap_or_else(|| Rc::new(Node::Leaf(k, va.clone())))
        } else {
            Rc::new(Node::Leaf(k, join(va, vb)))
        }
    };
    match (&**a, &**b) {
        (Node::Leaf(k, va), _) => get(b, *k).map(|vb| leaf(*k, va, vb, Some(a))),
        (_, Node::Leaf(k, vb)) => get(a, *k).map(|va| leaf(*k, va, vb, None)),
        (
            Node::Branch { prefix: p, bit: m, left: l, right: r },
            Node::Branch { prefix: q, bit: n, left: s, right: t },
        ) => {
            if m == n {
                if p != q {
                    return None;
                }
                let left = intersect(l, s, join);
                let right = intersect(r, t, join);
                rebuild(a, *p, *m, left, right)
            } else if m > n {
                // `b` lies within one side of `a`, if at all.
                if mask(*q, *m) != *p {
                    return None;
                }
                intersect(if q & m == 0 { l } else { r }, b, join)
            } else {
                if mask(*p, *n) != *q {
                    return None;
                }
                intersect(a, if p & n == 0 { s } else { t }, join)
            }
        }
    }
}

impl<K: Key, V: Clone + PartialEq> PMap<K, V> {
    pub fn get(&self, k: &K) -> Option<&V> {
        self.root.as_ref().and_then(|t| get(t, k.bits()))
    }

    pub fn contains_key(&self, k: &K) -> bool {
        self.get(k).is_some()
    }

    pub fn insert(&mut self, k: K, v: V) {
        let k = k.bits();
        self.root = Some(match &self.root {
            None => Rc::new(Node::Leaf(k, v)),
            Some(t) => insert(t, k, v),
        });
    }

    pub fn remove(&mut self, k: &K) {
        self.remove_range(*k, *k);
    }

    /// Removes the keys from `lo` to `hi`.
    pub fn remove_range(&mut self, lo: K, hi: K) {
        if let Some(t) = &self.root {
            self.root = remove_range(t, lo.bits(), hi.bits());
        }
    }

    pub fn clear(&mut self) {
        self.root = None;
    }

    /// The keys of both maps, each with `join` of its two values.
    pub fn intersect(a: &Self, b: &Self, join: impl Fn(&V, &V) -> V) -> Self {
        let root = match (&a.root, &b.root) {
            (Some(x), Some(y)) => intersect(x, y, &join),
            _ => None,
        };
        PMap { root, _key: std::marker::PhantomData }
    }

    #[cfg(test)]
    fn entries(&self) -> Vec<(u64, V)> {
        fn walk<V: Clone>(t: &Rc<Node<V>>, out: &mut Vec<(u64, V)>) {
            match &**t {
                Node::Leaf(k, v) => out.push((*k, v.clone())),
                Node::Branch { left, right, .. } => {
                    walk(left, out);
                    walk(right, out);
                }
            }
        }
        let mut out = Vec::new();
        if let Some(t) = &self.root {
            walk(t, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Random changes, removals and joins agree with a `BTreeMap`.
    #[test]
    fn agrees_with_a_btreemap() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rand = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for round in 0..200 {
            let span = if round % 2 == 0 { 64 } else { 1 << 40 };
            let mut base = PMap::<(u32, u32), u64>::default();
            let mut model = BTreeMap::new();
            for _ in 0..rand(100) {
                let k = rand(span);
                let v = rand(4);
                base.insert(((k >> 32) as u32, k as u32), v);
                model.insert(k, v);
            }
            let (mut a, mut b) = (base.clone(), base.clone());
            let (mut ma, mut mb) = (model.clone(), model.clone());
            for (m, mm) in [(&mut a, &mut ma), (&mut b, &mut mb)] {
                for _ in 0..rand(20) {
                    let k = rand(span);
                    let key = ((k >> 32) as u32, k as u32);
                    match rand(3) {
                        0 => {
                            m.remove(&key);
                            mm.remove(&k);
                        }
                        1 => {
                            let hi = k + rand(span / 4 + 1);
                            m.remove_range(key, ((hi >> 32) as u32, hi as u32));
                            mm.retain(|x, _| *x < k || *x > hi);
                        }
                        _ => {
                            let v = rand(4);
                            m.insert(key, v);
                            mm.insert(k, v);
                        }
                    }
                }
                assert_eq!(m.entries(), mm.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>());
            }
            let j = PMap::intersect(&a, &b, |x, y| x + y + 10);
            let mj: Vec<(u64, u64)> =
                ma.iter().filter_map(|(k, x)| mb.get(k).map(|y| (*k, if x == y { *x } else { x + y + 10 }))).collect();
            assert_eq!(j.entries(), mj);
            for (k, v) in &mj {
                assert_eq!(j.get(&((k >> 32) as u32, *k as u32)), Some(v));
            }
        }
    }

    /// Joining two copies of a large map that each changed a little is cheap:
    /// the parts they share are not visited.
    #[test]
    fn joins_follow_the_changes() {
        let mut m = PMap::<u32, u32>::default();
        for k in 0..100_000 {
            m.insert(k, 1);
        }
        let start = std::time::Instant::now();
        for i in 0..100_000u32 {
            let mut a = m.clone();
            let b = m.clone();
            a.insert(i, 2);
            m = PMap::intersect(&a, &b, |x, y| x.max(y).to_owned());
        }
        assert_eq!(m.get(&5), Some(&2));
        // Quadratic work (10^10 steps) would take minutes; this is about 10^7.
        assert!(start.elapsed() < std::time::Duration::from_secs(20), "{:?}", start.elapsed());
    }
}
