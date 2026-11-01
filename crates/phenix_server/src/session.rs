//! Server-side session store. Only a random id travels in the cookie; values
//! stay on the server. Unknown ids are never adopted (no session fixation).
//!
//! The store is bounded by count and by bytes; when full, the least recently
//! used sessions are evicted, so a flood of new sessions cannot lock out
//! real visitors or exhaust memory.

use indexmap::IndexMap;
use phenix_vm::SessionData;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_SESSIONS: usize = 50_000;
const MAX_BYTES: usize = 256 * 1024 * 1024;
/// Sessions end after this long even if they stay active.
const MAX_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

struct Entry {
    values: IndexMap<String, String>,
    created: Instant,
    last: Instant,
    bytes: usize,
}

struct Inner {
    map: HashMap<String, Entry>,
    bytes: usize,
    requests: u64,
}

pub struct Store {
    ttl: Duration,
    /// `__Host-PHXSESSID` over HTTPS (bound to this origin), `PHXSESSID` otherwise.
    pub cookie: &'static str,
    secure: bool,
    inner: Mutex<Inner>,
}

fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn size(values: &IndexMap<String, String>) -> usize {
    values.iter().map(|(k, v)| k.len() + v.len() + 64).sum::<usize>() + 160
}

/// The first value of cookie `name` in a Cookie header.
pub fn cookie_value<'a>(header: Option<&'a str>, name: &str) -> Option<&'a str> {
    header?.split(';').find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

impl Store {
    pub fn new(ttl: Duration, secure: bool) -> Store {
        Store {
            ttl,
            cookie: if secure { "__Host-PHXSESSID" } else { "PHXSESSID" },
            secure,
            inner: Mutex::new(Inner { map: HashMap::new(), bytes: 0, requests: 0 }),
        }
    }

    fn alive(&self, e: &Entry) -> bool {
        e.last.elapsed() < self.ttl && e.created.elapsed() < MAX_LIFETIME
    }

    /// The session named by the request's cookie, if it exists and has not expired.
    pub fn load(&self, cookie_header: Option<&str>) -> SessionData {
        let id = cookie_value(cookie_header, self.cookie).filter(|v| valid_id(v)).map(str::to_string);
        let mut inner = self.inner.lock().unwrap();
        if let Some(id) = id {
            let alive = inner.map.get(&id).map(|e| self.alive(e));
            match alive {
                Some(true) => {
                    let e = inner.map.get_mut(&id).unwrap();
                    e.last = Instant::now();
                    return SessionData { id: Some(id), values: e.values.clone(), ..SessionData::default() };
                }
                Some(false) => {
                    if let Some(e) = inner.map.remove(&id) {
                        inner.bytes -= e.bytes;
                    }
                }
                None => {}
            }
        }
        SessionData::default()
    }

    fn evict(&self, inner: &mut Inner, room: usize) {
        let ttl = self.ttl;
        let before = inner.map.len();
        inner.map.retain(|_, e| e.last.elapsed() < ttl && e.created.elapsed() < MAX_LIFETIME);
        if inner.map.len() != before {
            inner.bytes = inner.map.values().map(|e| e.bytes).sum();
        }
        if inner.map.len() < MAX_SESSIONS && inner.bytes + room <= MAX_BYTES {
            return;
        }
        // Drop the least recently used tenth.
        let mut by_age: Vec<(Instant, String)> = inner.map.iter().map(|(k, e)| (e.last, k.clone())).collect();
        by_age.sort();
        for (_, k) in by_age.into_iter().take((MAX_SESSIONS / 10).max(1)) {
            if let Some(e) = inner.map.remove(&k) {
                inner.bytes -= e.bytes;
            }
        }
    }

    /// Stores the session after a request. Returns the `Set-Cookie` value to send, if any.
    pub fn save(&self, s: SessionData) -> Option<String> {
        let flags = if self.secure { "; Secure" } else { "" };
        let name = self.cookie;
        let mut inner = self.inner.lock().unwrap();
        inner.requests += 1;
        if inner.requests.is_multiple_of(1024) {
            self.evict(&mut inner, 0);
        }
        if s.destroyed {
            if let Some(id) = &s.id {
                if let Some(e) = inner.map.remove(id) {
                    inner.bytes -= e.bytes;
                }
            }
            return s.id.map(|_| format!("{name}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{flags}"));
        }
        if !s.changed || (s.id.is_none() && s.values.is_empty()) {
            return None;
        }
        let new_id = match (&s.id, s.regenerate) {
            (Some(id), false) => id.clone(),
            _ => phenix_std::random_hex(32),
        };
        let mut created = Instant::now();
        if let Some(old) = &s.id {
            if let Some(e) = inner.map.remove(old) {
                inner.bytes -= e.bytes;
                created = e.created;
            }
        }
        let bytes = size(&s.values);
        if inner.map.len() >= MAX_SESSIONS || inner.bytes + bytes > MAX_BYTES {
            self.evict(&mut inner, bytes);
        }
        let issue_cookie = s.id.as_deref() != Some(new_id.as_str());
        inner.bytes += bytes;
        inner.map.insert(new_id.clone(), Entry { values: s.values, created, last: Instant::now(), bytes });
        issue_cookie.then(|| format!("{name}={new_id}; Path=/; HttpOnly; SameSite=Lax{flags}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_and_reuses_ids() {
        let store = Store::new(Duration::from_secs(60), true);
        assert_eq!(store.cookie, "__Host-PHXSESSID");
        // A request without a session that sets nothing gets no cookie.
        assert!(store.save(store.load(None)).is_none());
        let mut s = store.load(None);
        s.values.insert("user".into(), "ann".into());
        s.changed = true;
        let cookie = store.save(s).unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("Secure") && cookie.contains("SameSite=Lax"));
        let id = cookie.split(['=', ';']).nth(1).unwrap().to_string();
        let header = format!("other=1; __Host-PHXSESSID={id}");
        let loaded = store.load(Some(&header));
        assert_eq!(loaded.values.get("user").map(String::as_str), Some("ann"));
        // An attacker-chosen id is ignored.
        let forged = store.load(Some(&format!("__Host-PHXSESSID={}", "a".repeat(64))));
        assert!(forged.id.is_none());
        // regenerate() moves the values to a new id and drops the old one.
        let mut s = store.load(Some(&header));
        s.regenerate = true;
        s.changed = true;
        let cookie2 = store.save(s).unwrap();
        assert!(!cookie2.contains(&id));
        assert!(store.load(Some(&header)).id.is_none());
    }

    #[test]
    fn a_flood_of_sessions_evicts_old_ones() {
        let store = Store::new(Duration::from_secs(600), false);
        let mut first = None;
        for i in 0..MAX_SESSIONS + 10 {
            let mut s = SessionData::default();
            s.values.insert("n".into(), i.to_string());
            s.changed = true;
            let c = store.save(s).expect("a new visitor always gets a session");
            if i == 0 {
                first = Some(c);
            }
        }
        let inner = store.inner.lock().unwrap();
        assert!(inner.map.len() <= MAX_SESSIONS);
        drop(inner);
        let id = first.unwrap().split(['=', ';']).nth(1).unwrap().to_string();
        assert!(store.load(Some(&format!("PHXSESSID={id}"))).id.is_none(), "oldest session was evicted");
    }
}
