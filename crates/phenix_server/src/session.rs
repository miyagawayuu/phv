//! Server-side session store. Only a random id travels in the cookie; values
//! stay on the server. Unknown ids are never adopted (no session fixation).

use indexmap::IndexMap;
use phenix_vm::SessionData;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const COOKIE: &str = "PHXSESSID";
/// Hard cap on stored sessions, so session creation cannot exhaust memory.
const MAX_SESSIONS: usize = 100_000;

struct Entry {
    values: IndexMap<String, String>,
    last: Instant,
}

pub struct Store {
    ttl: Duration,
    map: Mutex<HashMap<String, Entry>>,
    requests: Mutex<u64>,
}

fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Store {
    pub fn new(ttl: Duration) -> Store {
        Store { ttl, map: Mutex::new(HashMap::new()), requests: Mutex::new(0) }
    }

    /// The session named by the request's cookie, if it exists and has not expired.
    pub fn load(&self, cookie_header: Option<&str>) -> SessionData {
        let id = cookie_header.and_then(|h| {
            h.split(';').find_map(|p| {
                let (k, v) = p.trim().split_once('=')?;
                (k == COOKIE && valid_id(v)).then(|| v.to_string())
            })
        });
        let mut map = self.map.lock().unwrap();
        if let Some(id) = id {
            match map.get_mut(&id) {
                Some(e) if e.last.elapsed() < self.ttl => {
                    e.last = Instant::now();
                    return SessionData { id: Some(id), values: e.values.clone(), ..SessionData::default() };
                }
                Some(_) => {
                    map.remove(&id);
                }
                None => {}
            }
        }
        SessionData::default()
    }

    /// Stores the session after a request. Returns the `Set-Cookie` value to send, if any.
    pub fn save(&self, s: SessionData, secure: bool) -> Option<String> {
        self.sweep();
        let flags = if secure { "; Secure" } else { "" };
        let mut map = self.map.lock().unwrap();
        if s.destroyed {
            if let Some(id) = &s.id {
                map.remove(id);
            }
            return s.id.map(|_| format!("{COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{flags}"));
        }
        if !s.changed {
            return None;
        }
        if s.id.is_none() && s.values.is_empty() {
            return None;
        }
        let new_id = match (&s.id, s.regenerate) {
            (Some(id), false) => id.clone(),
            _ => phenix_std::random_hex(32),
        };
        if let Some(old) = &s.id {
            if *old != new_id {
                map.remove(old);
            }
        }
        if map.len() >= MAX_SESSIONS && !map.contains_key(&new_id) {
            eprintln!("warning: session store is full ({MAX_SESSIONS} sessions); not creating a new one");
            return None;
        }
        let issue_cookie = s.id.as_deref() != Some(new_id.as_str());
        map.insert(new_id.clone(), Entry { values: s.values, last: Instant::now() });
        issue_cookie.then(|| format!("{COOKIE}={new_id}; Path=/; HttpOnly; SameSite=Lax{flags}"))
    }

    fn sweep(&self) {
        let mut n = self.requests.lock().unwrap();
        *n += 1;
        if !n.is_multiple_of(256) {
            return;
        }
        drop(n);
        let ttl = self.ttl;
        self.map.lock().unwrap().retain(|_, e| e.last.elapsed() < ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_and_reuses_ids() {
        let store = Store::new(Duration::from_secs(60));
        // A request without a session that sets nothing gets no cookie.
        assert!(store.save(store.load(None), false).is_none());
        let mut s = store.load(None);
        s.values.insert("user".into(), "ann".into());
        s.changed = true;
        let cookie = store.save(s, true).unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("Secure") && cookie.contains("SameSite=Lax"));
        let id = cookie.split(['=', ';']).nth(1).unwrap().to_string();
        let header = format!("other=1; {COOKIE}={id}");
        let loaded = store.load(Some(&header));
        assert_eq!(loaded.values.get("user").map(String::as_str), Some("ann"));
        // An attacker-chosen id is ignored.
        let forged = store.load(Some(&format!("{COOKIE}={}", "a".repeat(64))));
        assert!(forged.id.is_none());
        // regenerate() moves the values to a new id and drops the old one.
        let mut s = store.load(Some(&header));
        s.regenerate = true;
        s.changed = true;
        let cookie2 = store.save(s, false).unwrap();
        assert!(!cookie2.contains(&id));
        assert!(store.load(Some(&header)).id.is_none());
    }
}
