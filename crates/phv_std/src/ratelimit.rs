//! Request counters shared by every request in the process, for
//! `rate_limit()` and the server's per-client limit. Each (key, window) pair
//! counts hits in a fixed window. The table is bounded; when it is full,
//! expired windows go first, then windows that are not at their limit, and
//! only then the oldest limited ones, so flooding the table with new keys
//! does not lift an active limit.
//!
//! The table is split by key into `SHARDS` maps, each under a lock of its
//! own, so requests with different keys do not wait for one another. The
//! bound and the eviction are those of the whole table: the number of windows
//! is counted across the shards, and making room locks them all and chooses
//! what goes over all of them together, so the table refuses exactly what one
//! map would.
//!
//! With `[security] rate_limit_store`, the counters live on a Redis server
//! instead, shared by every server process (`hit_shared`, `reset_shared`).

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const MAX_KEYS: usize = 100_000;

/// The maps the table is split into.
const SHARDS: usize = 32;

struct Window {
    start: Instant,
    length: Duration,
    hits: u32,
    max: u32,
}

impl Window {
    fn expired(&self) -> bool {
        self.start.elapsed() >= self.length
    }
}

/// A table key: the caller's key (`base_of`) and the window length in
/// milliseconds, so a per-minute and a per-day limit on the same key count
/// separately. The two are kept apart, so `reset` forgets the windows of its
/// own key only (found by the 1.2.0 audit: a key and its window were one
/// string, and resetting `k` also reset every key that starts with `k\0`).
type TableKey = (String, u128);

type Map = HashMap<TableKey, Window>;

/// The table: its shards, and the number of windows in all of them.
struct Table {
    shards: Vec<Mutex<Map>>,
    /// Exact: a window is counted under its shard's lock, before it is
    /// inserted (`reserve`), and the count is set again under every lock when
    /// room is made.
    len: AtomicUsize,
    /// Chooses a key's shard (keyed at random, like the maps, so a flood of
    /// keys cannot be aimed at one shard).
    hasher: RandomState,
}

static TABLE: LazyLock<Table> = LazyLock::new(|| Table {
    shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
    len: AtomicUsize::new(0),
    hasher: RandomState::new(),
});

impl Table {
    /// The shard of the windows of a key (every window of one key is in the
    /// same shard, so `reset` looks in one).
    fn shard_of(&self, base: &str) -> usize {
        (self.hasher.hash_one(base) % SHARDS as u64) as usize
    }

    fn lock(&self, shard: usize) -> MutexGuard<'_, Map> {
        self.shards[shard].lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Counts one more window, unless the table is full.
    fn reserve(&self) -> bool {
        reserve(&self.len, MAX_KEYS)
    }

    /// Makes room for more keys, in amortized O(1): each pass frees a tenth.
    /// Expired windows go first, then windows that are not at their limit,
    /// then the oldest limited ones, chosen over every shard together.
    fn make_room(&self) {
        let mut shards: Vec<MutexGuard<'_, Map>> = (0..SHARDS).map(|i| self.lock(i)).collect();
        for t in shards.iter_mut() {
            t.retain(|_, w| !w.expired());
        }
        let len: usize = shards.iter().map(|t| t.len()).sum();
        let target = MAX_KEYS * 9 / 10;
        if len > target {
            let mut order: Vec<(bool, Instant, TableKey)> =
                shards.iter().flat_map(|t| t.iter().map(|(k, w)| (w.hits >= w.max, w.start, k.clone()))).collect();
            // Not-limited before limited, then oldest first.
            order.sort();
            for (_, _, k) in order.into_iter().take(len - target) {
                shards[self.shard_of(&k.0)].remove(&k);
            }
        }
        self.len.store(shards.iter().map(|t| t.len()).sum(), Ordering::SeqCst);
    }
}

/// Counts one more in `n`, unless it is at `max` already.
fn reserve(n: &AtomicUsize, max: usize) -> bool {
    let mut current = n.load(Ordering::SeqCst);
    loop {
        if current >= max {
            return false;
        }
        match n.compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(seen) => current = seen,
        }
    }
}

fn key_of(key: &str, window: Duration) -> TableKey {
    (base_of(key), window.as_millis())
}

/// The caller's key as the table holds it. Long keys are hashed so the
/// table's memory stays bounded; the first character tells the two kinds
/// apart, so a short key cannot be written as the hash of a long one.
fn base_of(key: &str) -> String {
    if key.len() <= 128 {
        format!("={key}")
    } else {
        use sha2::{Digest, Sha256};
        format!("#{:x}", Sha256::digest(key.as_bytes()))
    }
}

/// Counts a hit in `w` (a window of `window`, at `now`), unless `max` hits
/// have already happened in it.
fn count(w: &mut Window, now: Instant, max: u32, window: Duration) -> bool {
    if w.expired() {
        *w = Window { start: now, length: window, hits: 0, max };
    }
    w.max = max;
    if w.hits >= max {
        return false;
    }
    w.hits += 1;
    true
}

/// Counts a hit for `key`. Returns false (and does not count) when `max`
/// hits have already happened in the current window of `window`.
pub fn hit(key: &str, max: u32, window: Duration) -> bool {
    let key = key_of(key, window);
    let shard = TABLE.shard_of(&key.0);
    loop {
        let mut t = TABLE.lock(shard);
        let now = Instant::now();
        if let Some(w) = t.get_mut(&key) {
            return count(w, now, max, window);
        }
        if TABLE.reserve() {
            let w = t.entry(key).or_insert(Window { start: now, length: window, hits: 0, max });
            return count(w, now, max, window);
        }
        // The table is full: room is made under every shard's lock, so this
        // one is let go first.
        drop(t);
        TABLE.make_room();
    }
}

/// Seconds until the window of `key` ends (0 if it has none).
pub fn retry_after(key: &str, window: Duration) -> u64 {
    let key = key_of(key, window);
    let t = TABLE.lock(TABLE.shard_of(&key.0));
    t.get(&key).map(|w| w.length.saturating_sub(w.start.elapsed()).as_secs() + 1).unwrap_or(0)
}

/// Forgets the hits of `key` in every window (e.g. after a successful login).
pub fn reset(key: &str) {
    let base = base_of(key);
    let mut t = TABLE.lock(TABLE.shard_of(&base));
    let before = t.len();
    t.retain(|(k, _), _| *k != base);
    TABLE.len.fetch_sub(before - t.len(), Ordering::SeqCst);
}

/// For the tests: the number of windows in the table, checked against the
/// count the shards keep (under every lock, so nothing changes meanwhile).
#[cfg(test)]
fn exact_len() -> usize {
    let shards: Vec<MutexGuard<'_, Map>> = (0..SHARDS).map(|i| TABLE.lock(i)).collect();
    let len: usize = shards.iter().map(|t| t.len()).sum();
    assert_eq!(len, TABLE.len.load(Ordering::SeqCst), "the count of windows is exact");
    len
}

/// Counts a hit in the store phv.toml names: the Redis database of
/// `[security] rate_limit_store`, or this process. Returns whether the hit is
/// allowed and the seconds until the window ends. When the Redis server cannot
/// be reached, the error says why.
/// A key in the shared store: in this site's namespace when it has one.
/// A namespace (32 hex digits) is never empty and has no NUL, so a key in one
/// starts with a character other than NUL and has one. A key without a
/// namespace stays as it is unless it has a NUL: then it starts with one, so
/// it cannot be any site's key (found by the 1.1.1 audit: `"{ns}\0login:x"`
/// from a site without a namespace counted as that site's `login:x`).
fn shared_key(host: &phv_vm::Host, key: &str) -> String {
    match &host.store_namespace {
        Some(ns) => format!("{ns}\0{key}"),
        None if key.contains('\0') => format!("\0{key}"),
        None => key.to_string(),
    }
}

pub fn hit_shared(
    host: &phv_vm::Host,
    key: &str,
    max: u32,
    window: Duration,
    until: Instant,
) -> Result<(bool, u64), String> {
    match store(host)? {
        Some(spec) => crate::netdb::redis_hit(spec, &shared_key(host, key), max, window, until),
        None => Ok((hit(key, max, window), retry_after(key, window))),
    }
}

/// Forgets the hits of `key` in the store phv.toml names (see `hit_shared`).
pub fn reset_shared(host: &phv_vm::Host, key: &str, until: Instant) -> Result<(), String> {
    match store(host)? {
        Some(spec) => crate::netdb::redis_reset(spec, &shared_key(host, key), until),
        None => {
            reset(key);
            Ok(())
        }
    }
}

fn store(host: &phv_vm::Host) -> Result<Option<&phv_vm::DatabaseSpec>, String> {
    let Some(name) = &host.rate_limit_store else { return Ok(None) };
    host.databases.get(name).map(Some).ok_or_else(|| format!("no database named \"{name}\""))
}

/// The address a client is limited by: IPv6 clients usually control a whole
/// /64, so they are grouped by it.
pub fn client_key(ip: std::net::IpAddr) -> String {
    match ip.to_canonical() {
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        v4 => v4.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_resets() {
        let k = "test:limits_and_resets";
        let minute = Duration::from_secs(60);
        for _ in 0..3 {
            assert!(hit(k, 3, minute));
        }
        assert!(!hit(k, 3, minute));
        assert!(retry_after(k, minute) > 0);
        // Another window on the same key counts separately and does not reset this one.
        assert!(hit(k, 100, Duration::from_secs(3600)));
        assert!(!hit(k, 3, minute));
        reset(k);
        assert!(hit(k, 3, minute));
        let short = "test:window_ends";
        assert!(hit(short, 1, Duration::from_millis(20)));
        assert!(!hit(short, 1, Duration::from_millis(20)));
        std::thread::sleep(Duration::from_millis(30));
        assert!(hit(short, 1, Duration::from_millis(20)));
    }

    /// Resetting a key leaves the keys that start with it alone (found by the
    /// 1.2.0 audit: `reset("login:a")` also reset `"login:a\0b"`).
    #[test]
    fn reset_clears_only_its_own_key() {
        let minute = Duration::from_secs(60);
        let long = format!("test:reset-own:{}", "x".repeat(200));
        let others =
            ["test:reset-own\0b", "test:reset-own\0", "test:reset-own\u{0}60000", "test:reset-ownb", long.as_str()];
        for k in others {
            assert!(hit(k, 1, minute));
            assert!(!hit(k, 1, minute));
        }
        assert!(hit("test:reset-own", 1, minute));
        reset("test:reset-own");
        assert!(hit("test:reset-own", 1, minute), "its own window is forgotten");
        for k in others {
            assert!(!hit(k, 1, minute), "{k:?} is still limited");
        }
        // A short key cannot name the hashed form of a long one.
        use sha2::{Digest, Sha256};
        let hashed = format!("#{:x}", Sha256::digest(long.as_bytes()));
        assert!(hit(&hashed, 1, minute));
        reset(&hashed);
        assert!(!hit(&long, 1, minute));
    }

    #[test]
    fn a_flood_of_keys_does_not_lift_a_limit() {
        let day = Duration::from_secs(86_400);
        assert!(hit("test:flood-victim", 1, day));
        assert!(!hit("test:flood-victim", 1, day));
        for i in 0..MAX_KEYS + 10 {
            hit(&format!("test:flood-{i}"), 5, day);
        }
        assert!(!hit("test:flood-victim", 1, day), "still limited after the flood");
        // The table never holds more than its bound, counted over every shard.
        assert!(exact_len() <= MAX_KEYS);
    }

    /// The bound and the eviction are those of the whole table, not of a
    /// shard: a flood past the bound keeps every limited window of the
    /// victims (there is room for them among the not-limited ones that go
    /// first), and a reset takes its windows out of the count.
    #[test]
    fn the_bound_is_counted_over_every_shard() {
        let day = Duration::from_secs(86_400);
        let victims: Vec<String> = (0..500).map(|i| format!("test:shard-victim-{i}")).collect();
        for v in &victims {
            assert!(hit(v, 1, day));
            assert!(!hit(v, 1, day));
        }
        for i in 0..MAX_KEYS + 10 {
            hit(&format!("test:shard-flood-{i}"), 5, day);
        }
        assert!(exact_len() <= MAX_KEYS);
        for v in &victims {
            assert!(!hit(v, 1, day), "{v} is still limited after the flood");
        }
        // A reset takes the key's windows out of the table (and the count stays
        // exact; other tests add keys meanwhile, so only that is checked).
        assert!(retry_after(&victims[0], day) > 0);
        reset(&victims[0]);
        assert_eq!(retry_after(&victims[0], day), 0);
        assert!(exact_len() <= MAX_KEYS);
        assert!(hit(&victims[0], 1, day));
        assert!(exact_len() <= MAX_KEYS);
    }

    /// Hits on one key from many threads at once: exactly `max` of them count.
    #[test]
    fn concurrent_hits_on_one_key_count_exactly() {
        let minute = Duration::from_secs(60);
        let allowed = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..1000 {
                        if hit("test:concurrent", 2500, minute) {
                            allowed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(allowed.load(Ordering::Relaxed), 2500);
        assert!(!hit("test:concurrent", 2500, minute));
        assert!(retry_after("test:concurrent", minute) > 0);
    }

    /// Sites with a namespace count apart in a shared store (found by the 1.1
    /// audit: one site's login tries limited another's).
    #[test]
    fn namespaces_keep_sites_apart() {
        let plain = phv_vm::Host::default();
        let a = phv_vm::Host { store_namespace: Some("a".into()), ..phv_vm::Host::default() };
        let b = phv_vm::Host { store_namespace: Some("b".into()), ..phv_vm::Host::default() };
        assert_eq!(shared_key(&plain, "login:x"), "login:x");
        assert_ne!(shared_key(&a, "login:x"), shared_key(&b, "login:x"));
        assert_ne!(shared_key(&a, "login:x"), "login:x");
        // A site without a namespace cannot name another site's key (1.1.1 audit).
        assert_ne!(shared_key(&plain, "a\0login:x"), shared_key(&a, "login:x"));
        let ns_a = a.store_namespace.clone().unwrap();
        assert_ne!(shared_key(&plain, &format!("{ns_a}\0login:x")), shared_key(&a, "login:x"));
        // Keys without a NUL are unchanged.
        assert_eq!(shared_key(&plain, "login:a\u{1}b"), "login:a\u{1}b");
    }

    /// A store that cannot be reached is not asked again for a few seconds:
    /// the next hits fail at once, with the same error.
    #[test]
    fn an_unreachable_store_is_not_waited_for_again() {
        let spec =
            phv_vm::DatabaseSpec { url: Some("redis://127.0.0.1:1".into()), url_env: None, tls: None, pool_size: 2 };
        let host = phv_vm::Host {
            databases: std::collections::BTreeMap::from([("limits".to_string(), spec)]),
            rate_limit_store: Some("limits".into()),
            ..phv_vm::Host::default()
        };
        let until = || Instant::now() + Duration::from_secs(5);
        let first = hit_shared(&host, "test:down", 3, Duration::from_secs(60), until()).unwrap_err();
        let start = Instant::now();
        for _ in 0..20 {
            assert_eq!(hit_shared(&host, "test:down", 3, Duration::from_secs(60), until()).unwrap_err(), first);
        }
        assert_eq!(reset_shared(&host, "test:down", until()).unwrap_err(), first);
        assert!(start.elapsed() < Duration::from_millis(500), "took {:?}", start.elapsed());
    }

    #[test]
    fn groups_ipv6_by_prefix() {
        let a: std::net::IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: std::net::IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        assert_eq!(client_key(a), client_key(b));
        assert_eq!(client_key("::ffff:10.0.0.1".parse().unwrap()), "10.0.0.1");
    }
}
