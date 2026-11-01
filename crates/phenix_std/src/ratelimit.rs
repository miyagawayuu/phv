//! Request counters shared by every request in the process, for
//! `rate_limit()` and the server's per-client limit. Each key counts hits in
//! a fixed window; the table is bounded, and the oldest windows are dropped
//! first when it is full.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const MAX_KEYS: usize = 100_000;

struct Window {
    start: Instant,
    length: Duration,
    hits: u32,
}

static TABLE: LazyLock<Mutex<HashMap<String, Window>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Long keys are stored by their hash so the table's memory stays bounded.
fn key_of(key: &str) -> String {
    if key.len() <= 128 {
        key.to_string()
    } else {
        use sha2::{Digest, Sha256};
        format!("#{:x}", Sha256::digest(key.as_bytes()))
    }
}

/// Counts a hit for `key`. Returns false (and does not count) when `max`
/// hits have already happened in the current window of `window`.
pub fn hit(key: &str, max: u32, window: Duration) -> bool {
    let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let key = key_of(key);
    if !t.contains_key(&key) && t.len() >= MAX_KEYS {
        t.retain(|_, w| w.start.elapsed() < w.length);
        if t.len() >= MAX_KEYS {
            let mut by_age: Vec<(Instant, String)> = t.iter().map(|(k, w)| (w.start, k.clone())).collect();
            by_age.sort();
            for (_, k) in by_age.into_iter().take(MAX_KEYS / 10) {
                t.remove(&k);
            }
        }
    }
    let now = Instant::now();
    let w = t.entry(key).or_insert(Window { start: now, length: window, hits: 0 });
    if w.start.elapsed() >= w.length || w.length != window {
        *w = Window { start: now, length: window, hits: 0 };
    }
    if w.hits >= max {
        return false;
    }
    w.hits += 1;
    true
}

/// Seconds until the window of `key` ends (0 if it has none).
pub fn retry_after(key: &str) -> u64 {
    let t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    t.get(&key_of(key)).map(|w| w.length.saturating_sub(w.start.elapsed()).as_secs() + 1).unwrap_or(0)
}

/// Forgets the hits of `key` (e.g. after a successful login).
pub fn reset(key: &str) {
    TABLE.lock().unwrap_or_else(|e| e.into_inner()).remove(&key_of(key));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_resets() {
        let k = "test:limits_and_resets";
        for _ in 0..3 {
            assert!(hit(k, 3, Duration::from_secs(60)));
        }
        assert!(!hit(k, 3, Duration::from_secs(60)));
        assert!(retry_after(k) > 0);
        reset(k);
        assert!(hit(k, 3, Duration::from_secs(60)));
        let short = "test:window_ends";
        assert!(hit(short, 1, Duration::from_millis(20)));
        assert!(!hit(short, 1, Duration::from_millis(20)));
        std::thread::sleep(Duration::from_millis(30));
        assert!(hit(short, 1, Duration::from_millis(20)));
    }
}
