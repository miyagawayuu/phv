//! Request counters shared by every request in the process, for
//! `rate_limit()` and the server's per-client limit. Each (key, window) pair
//! counts hits in a fixed window. The table is bounded; when it is full,
//! expired windows go first, then windows that are not at their limit, and
//! only then the oldest limited ones, so flooding the table with new keys
//! does not lift an active limit.
//!
//! With `[security] rate_limit_store`, the counters live on a Redis server
//! instead, shared by every server process (`hit_shared`, `reset_shared`).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const MAX_KEYS: usize = 100_000;

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

static TABLE: LazyLock<Mutex<HashMap<String, Window>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The table key: the caller's key and the window length (so a per-minute and
/// a per-day limit on the same key count separately). Long keys are hashed
/// so the table's memory stays bounded.
fn key_of(key: &str, window: Duration) -> String {
    format!("{}{}", base_of(key), window.as_millis())
}

/// Everything in a table key before the window length.
fn base_of(key: &str) -> String {
    if key.len() <= 128 {
        format!("{key}\u{0}")
    } else {
        use sha2::{Digest, Sha256};
        format!("#{:x}\u{0}", Sha256::digest(key.as_bytes()))
    }
}

/// Makes room for one more key, in amortized O(1): each pass frees a tenth.
fn make_room(t: &mut HashMap<String, Window>) {
    t.retain(|_, w| !w.expired());
    let target = MAX_KEYS * 9 / 10;
    if t.len() <= target {
        return;
    }
    let mut order: Vec<(bool, Instant, String)> =
        t.iter().map(|(k, w)| (w.hits >= w.max, w.start, k.clone())).collect();
    // Not-limited before limited, then oldest first.
    order.sort();
    let excess = t.len() - target;
    for (_, _, k) in order.into_iter().take(excess) {
        t.remove(&k);
    }
}

/// Counts a hit for `key`. Returns false (and does not count) when `max`
/// hits have already happened in the current window of `window`.
pub fn hit(key: &str, max: u32, window: Duration) -> bool {
    let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let key = key_of(key, window);
    if !t.contains_key(&key) && t.len() >= MAX_KEYS {
        make_room(&mut t);
    }
    let now = Instant::now();
    let w = t.entry(key).or_insert(Window { start: now, length: window, hits: 0, max });
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

/// Seconds until the window of `key` ends (0 if it has none).
pub fn retry_after(key: &str, window: Duration) -> u64 {
    let t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    t.get(&key_of(key, window)).map(|w| w.length.saturating_sub(w.start.elapsed()).as_secs() + 1).unwrap_or(0)
}

/// Forgets the hits of `key` in every window (e.g. after a successful login).
pub fn reset(key: &str) {
    let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let base = base_of(key);
    t.retain(|k, _| !k.starts_with(&base));
}

/// Counts a hit in the store phv.toml names: the Redis database of
/// `[security] rate_limit_store`, or this process. Returns whether the hit is
/// allowed and the seconds until the window ends. When the Redis server cannot
/// be reached, the error says why.
pub fn hit_shared(
    host: &phv_vm::Host,
    key: &str,
    max: u32,
    window: Duration,
    until: Instant,
) -> Result<(bool, u64), String> {
    match store(host)? {
        Some(spec) => crate::netdb::redis_hit(spec, key, max, window, until),
        None => Ok((hit(key, max, window), retry_after(key, window))),
    }
}

/// Forgets the hits of `key` in the store phv.toml names (see `hit_shared`).
pub fn reset_shared(host: &phv_vm::Host, key: &str, until: Instant) -> Result<(), String> {
    match store(host)? {
        Some(spec) => crate::netdb::redis_reset(spec, key, until),
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

    #[test]
    fn a_flood_of_keys_does_not_lift_a_limit() {
        let day = Duration::from_secs(86_400);
        assert!(hit("test:flood-victim", 1, day));
        assert!(!hit("test:flood-victim", 1, day));
        for i in 0..MAX_KEYS + 10 {
            hit(&format!("test:flood-{i}"), 5, day);
        }
        assert!(!hit("test:flood-victim", 1, day), "still limited after the flood");
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
