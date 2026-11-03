//! Server-side session store. Only a random id travels in the cookie; values
//! stay on the server. Unknown ids are never adopted (no session fixation).
//!
//! The store is bounded by count and by bytes; when full, the least recently
//! used sessions are evicted, so a flood of new sessions cannot lock out
//! real visitors or exhaust memory or disk. Sessions whose visitor never came
//! back go first: a flood of requests without a cookie (each making a new
//! session) then evicts its own sessions, not those of returning visitors.
//!
//! Sessions live in memory unless `[security] session_store` names a
//! directory: then each session is a file (named by a hash of its id, so a
//! directory listing reveals no ids), and sessions survive restarts and are
//! shared by every server process using the directory. A new session's file
//! is `n-<hash>.json` until its cookie comes back, then `<hash>.json`, so the
//! sweep can tell the two kinds apart from the listing alone.
//!
//! With `session_store = "db:<name>"` sessions live in that database of
//! `[databases]` instead (Redis, PostgreSQL or MySQL), keyed by the same hash,
//! with the same rules; see `DbStore`.

use indexmap::IndexMap;
use phv_std::safefs;
use phv_vm::SessionData;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_SESSIONS: usize = 50_000;
const MAX_BYTES: usize = 256 * 1024 * 1024;
/// Sessions end after this long even if they stay active.
const MAX_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

struct Entry {
    values: IndexMap<String, String>,
    created: Instant,
    last: Instant,
    bytes: usize,
    /// The visitor sent the session's cookie back at least once.
    returned: bool,
}

struct Inner {
    map: HashMap<String, Entry>,
    bytes: usize,
    requests: u64,
}

pub struct Store {
    ttl: Duration,
    /// `__Host-PHVSESSID` over HTTPS (bound to this origin), `PHVSESSID` otherwise.
    pub cookie: &'static str,
    secure: bool,
    inner: Mutex<Inner>,
    files: Option<Box<dyn Backend>>,
}

/// Where sessions are kept when not in memory: files, or a database.
trait Backend: Send + Sync {
    /// The session and whether its visitor came back.
    fn read(&self, id: &str) -> Option<(Record, bool)>;
    fn write(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<()>;
    fn remove(&self, id: &str);
    /// Marks the session as one whose visitor came back; false if that failed.
    fn promote(&self, id: &str) -> bool;
    /// Removes expired sessions, and the excess when there are too many.
    fn sweep(&self, ttl: Duration);
    /// The file store, for tests that lower its limits.
    #[cfg(test)]
    fn file_store(&mut self) -> Option<&mut FileStore> {
        None
    }
}

/// The key a session is stored under: a hash of its id, so a listing of the
/// store reveals no ids.
fn session_key(id: &str) -> String {
    phv_std::sha256_hex(format!("phv-session:{id}").as_bytes())
}

fn values_json(values: &IndexMap<String, String>) -> String {
    serde_json::json!(values).to_string()
}

fn values_from_json(text: &str) -> Option<IndexMap<String, String>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v.as_object()?.iter().map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()
}

/// Sessions in a database of `[databases]`: Redis keeps them under
/// `phv:session:<hash>` and expires them itself (a session whose visitor has
/// not come back after an hour); PostgreSQL and MySQL in a `phv_sessions`
/// table, which the sweep keeps within `MAX_SESSIONS`, dropping those whose
/// visitor never came back first. A database that cannot be reached loses the
/// session (a warning is logged) instead of failing the request.
struct DbStore {
    spec: phv_vm::DatabaseSpec,
    ttl: Duration,
    /// Unix time of the last sweep.
    swept: AtomicU64,
}

impl DbStore {
    fn warn(what: &str, e: String) {
        eprintln!("warning: cannot {what} the session in the database: {e}");
    }
}

impl Backend for DbStore {
    fn read(&self, id: &str) -> Option<(Record, bool)> {
        let row = match phv_std::session_read(&self.spec, &session_key(id)) {
            Ok(r) => r?,
            Err(e) => {
                Self::warn("read", e);
                return None;
            }
        };
        let values = values_from_json(&row.data)?;
        Some((Record { created: row.created, last: row.last, values }, row.returned))
    }

    fn write(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<()> {
        let row = phv_std::SessionRow { data: values_json(&r.values), created: r.created, last: r.last, returned };
        phv_std::session_write(&self.spec, &session_key(id), &row, self.ttl).map_err(std::io::Error::other)
    }

    fn remove(&self, id: &str) {
        if let Err(e) = phv_std::session_remove(&self.spec, &session_key(id)) {
            Self::warn("remove", e);
        }
    }

    fn promote(&self, id: &str) -> bool {
        let Some((r, _)) = self.read(id) else { return false };
        self.write(id, &r, true).is_ok()
    }

    fn sweep(&self, ttl: Duration) {
        let now = now_secs();
        let last = self.swept.load(Ordering::Relaxed);
        if now < last + 30 || self.swept.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return;
        }
        if let Err(e) = phv_std::session_sweep(&self.spec, ttl, MAX_LIFETIME, MAX_SESSIONS) {
            Self::warn("clean up", e);
        }
    }
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

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// One session file: `{"created": t, "last": t, "values": {...}}` (Unix times).
struct Record {
    created: u64,
    last: u64,
    values: IndexMap<String, String>,
}

struct FileStore {
    dir: PathBuf,
    /// Unix time of the last sweep for expired and excess files.
    swept: AtomicU64,
    /// Bytes written since the last sweep; a burst triggers an early sweep.
    written: AtomicU64,
    /// `MAX_SESSIONS` and `MAX_BYTES` (smaller in tests).
    max_sessions: usize,
    max_bytes: u64,
}

/// Session files are `<64 hex>.json` (`n-<64 hex>.json` while new); temporary
/// ones `.<that name>.<16 hex>`. The sweep never touches anything else in the directory.
fn is_session_file(name: &str) -> bool {
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    let hash = |s: &str| hex(s.strip_prefix("n-").unwrap_or(s), 64);
    if let Some(rest) = name.strip_prefix('.') {
        return rest.split_once(".json.").is_some_and(|(id, r)| hash(id) && hex(r, 16));
    }
    name.strip_suffix(".json").is_some_and(hash)
}

impl Backend for FileStore {
    fn read(&self, id: &str) -> Option<(Record, bool)> {
        FileStore::read(self, id)
    }

    fn write(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<()> {
        FileStore::write(self, id, r, returned)
    }

    fn remove(&self, id: &str) {
        FileStore::remove(self, id)
    }

    fn promote(&self, id: &str) -> bool {
        FileStore::promote(self, id)
    }

    fn sweep(&self, ttl: Duration) {
        FileStore::sweep(self, ttl)
    }

    #[cfg(test)]
    fn file_store(&mut self) -> Option<&mut FileStore> {
        Some(self)
    }
}

impl FileStore {
    /// `<hash>.json` once the visitor came back (and for files from older
    /// versions), `n-<hash>.json` before.
    fn name(id: &str, returned: bool) -> String {
        let hash = session_key(id);
        if returned {
            format!("{hash}.json")
        } else {
            format!("n-{hash}.json")
        }
    }

    /// The session and whether its visitor came back. The returned name is
    /// tried again last: another request may rename the file in between.
    fn read(&self, id: &str) -> Option<(Record, bool)> {
        let (back, new) = (Self::name(id, true), Self::name(id, false));
        self.read_file(&back)
            .map(|r| (r, true))
            .or_else(|| self.read_file(&new).map(|r| (r, false)))
            .or_else(|| self.read_file(&back).map(|r| (r, true)))
    }

    fn read_file(&self, name: &str) -> Option<Record> {
        let (dir, name) = safefs::open_parent(&self.dir, Path::new(name)).ok()?;
        let mut text = String::new();
        dir.open(&name, false).ok()?.take(1 << 20).read_to_string(&mut text).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let values = v["values"]
            .as_object()?
            .iter()
            .map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect::<Option<IndexMap<_, _>>>()?;
        Some(Record { created: v["created"].as_u64()?, last: v["last"].as_u64()?, values })
    }

    /// Writes the file atomically: a temporary file renamed over the old one.
    fn write(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<()> {
        let text = serde_json::json!({"created": r.created, "last": r.last, "values": r.values}).to_string();
        let name = Self::name(id, returned);
        let tmp = format!(".{name}.{}", phv_std::random_hex(8));
        let fail = |e: safefs::FsError| std::io::Error::other(format!("{e:?}"));
        let (dir, tmp_name) = safefs::open_parent(&self.dir, Path::new(&tmp)).map_err(fail)?;
        let mut f = dir.open(&tmp_name, true).map_err(fail)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(text.as_bytes())?;
        self.written.fetch_add(text.len() as u64, Ordering::Relaxed);
        drop(f);
        let result = std::fs::rename(dir.path().join(&tmp_name), dir.path().join(&name));
        if result.is_err() {
            let _ = std::fs::remove_file(dir.path().join(&tmp_name));
        }
        result
    }

    fn remove(&self, id: &str) {
        for returned in [false, true] {
            if let Ok((dir, name)) = safefs::open_parent(&self.dir, Path::new(&Self::name(id, returned))) {
                let _ = dir.remove(&name);
            }
        }
    }

    /// Renames `n-<hash>.json` to `<hash>.json` when the visitor comes back.
    /// A link or hard link is left alone, never followed or moved.
    fn promote(&self, id: &str) -> bool {
        let Ok((dir, new)) = safefs::open_parent(&self.dir, Path::new(&Self::name(id, false))) else {
            return false;
        };
        if !dir.exists(&new).unwrap_or(false) || dir.check_plain_or_missing(&new).is_err() {
            return false;
        }
        std::fs::rename(dir.path().join(&new), dir.path().join(Self::name(id, true))).is_ok()
    }

    /// Every 30 seconds, or sooner after many writes: deletes expired session
    /// files, then while there are too many or they take too much space, new
    /// sessions whose visitor never came back, then the oldest of the rest.
    fn sweep(&self, ttl: Duration) {
        let now = now_secs();
        let last = self.swept.load(Ordering::Relaxed);
        let burst = self.written.load(Ordering::Relaxed) > self.max_bytes / 16;
        if (now < last + 30 && !burst)
            || self.swept.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err()
        {
            return;
        }
        self.written.store(0, Ordering::Relaxed);
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        let limit = SystemTime::now() - ttl.min(MAX_LIFETIME);
        let mut live: Vec<(bool, SystemTime, u64, PathBuf)> = Vec::new();
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            let modified = meta.modified().unwrap_or(UNIX_EPOCH);
            if !meta.is_file() || !is_session_file(&name) {
                continue;
            }
            if name.starts_with('.') {
                // Temporary files left by a crash are removed once old.
                if modified < limit {
                    let _ = std::fs::remove_file(e.path());
                }
                continue;
            }
            if modified < limit {
                let _ = std::fs::remove_file(e.path());
            } else {
                live.push((!name.starts_with("n-"), modified, meta.len(), e.path()));
            }
        }
        let mut bytes: u64 = live.iter().map(|l| l.2).sum();
        if live.len() <= self.max_sessions && bytes <= self.max_bytes {
            return;
        }
        live.sort();
        let mut count = live.len();
        for (_, _, len, path) in live {
            if count <= self.max_sessions * 9 / 10 && bytes <= self.max_bytes * 9 / 10 {
                break;
            }
            let _ = std::fs::remove_file(path);
            count -= 1;
            bytes -= len;
        }
    }
}

impl Store {
    pub fn new(ttl: Duration, secure: bool, dir: Option<PathBuf>) -> Store {
        let files = dir.map(|dir| {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                eprintln!("warning: cannot create session directory {}: {e}", dir.display());
            }
            if !dir.join(".gitignore").exists() {
                let _ = std::fs::write(dir.join(".gitignore"), "*\n");
            }
            Box::new(FileStore {
                dir,
                swept: AtomicU64::new(0),
                written: AtomicU64::new(0),
                max_sessions: MAX_SESSIONS,
                max_bytes: MAX_BYTES as u64,
            }) as Box<dyn Backend>
        });
        Store {
            ttl,
            cookie: if secure { "__Host-PHVSESSID" } else { "PHVSESSID" },
            secure,
            inner: Mutex::new(Inner { map: HashMap::new(), bytes: 0, requests: 0 }),
            files,
        }
    }

    /// A store that keeps sessions in a database of `[databases]` (see `DbStore`).
    pub fn with_database(ttl: Duration, secure: bool, spec: phv_vm::DatabaseSpec) -> Store {
        let mut store = Store::new(ttl, secure, None);
        store.files = Some(Box::new(DbStore { spec, ttl, swept: AtomicU64::new(0) }));
        store
    }

    fn alive(&self, e: &Entry) -> bool {
        e.last.elapsed() < self.ttl && e.created.elapsed() < MAX_LIFETIME
    }

    /// The session named by the request's cookie, if it exists and has not expired.
    pub fn load(&self, cookie_header: Option<&str>) -> SessionData {
        let id = cookie_value(cookie_header, self.cookie).filter(|v| valid_id(v)).map(str::to_string);
        if let Some(files) = &self.files {
            let Some(id) = id else { return SessionData::default() };
            let Some((mut r, mut returned)) = files.read(&id) else { return SessionData::default() };
            let now = now_secs();
            if now >= r.last.saturating_add(self.ttl.as_secs())
                || now >= r.created.saturating_add(MAX_LIFETIME.as_secs())
            {
                files.remove(&id);
                return SessionData::default();
            }
            // The cookie came back: the sweep no longer drops this one first.
            if !returned {
                returned = files.promote(&id);
            }
            // Refresh the expiry, at most once a minute per session.
            if now >= r.last + 60 {
                r.last = now;
                let _ = files.write(&id, &r, returned);
            }
            return SessionData { id: Some(id), values: r.values, ..SessionData::default() };
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(id) = id {
            let alive = inner.map.get(&id).map(|e| self.alive(e));
            match alive {
                Some(true) => {
                    let e = inner.map.get_mut(&id).unwrap();
                    e.last = Instant::now();
                    e.returned = true;
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
        // Drop a tenth: first sessions that were never used again, then the least recently used.
        let mut by_age: Vec<(bool, Instant, String)> =
            inner.map.iter().map(|(k, e)| (e.returned, e.last, k.clone())).collect();
        by_age.sort();
        for (_, _, k) in by_age.into_iter().take((MAX_SESSIONS / 10).max(1)) {
            if let Some(e) = inner.map.remove(&k) {
                inner.bytes -= e.bytes;
            }
        }
    }

    fn cookie_for(&self, id: Option<&str>) -> String {
        let flags = if self.secure { "; Secure" } else { "" };
        match id {
            Some(id) => format!("{}={id}; Path=/; HttpOnly; SameSite=Lax{flags}", self.cookie),
            None => format!("{}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{flags}", self.cookie),
        }
    }

    /// Stores the session after a request. Returns the `Set-Cookie` value to send, if any.
    pub fn save(&self, s: SessionData) -> Option<String> {
        if let Some(files) = &self.files {
            return self.save_file(files.as_ref(), s);
        }
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
            return s.id.map(|_| self.cookie_for(None));
        }
        if !s.changed || (s.id.is_none() && s.values.is_empty()) {
            return None;
        }
        let new_id = match (&s.id, s.regenerate) {
            (Some(id), false) => id.clone(),
            _ => phv_std::random_hex(32),
        };
        let mut created = Instant::now();
        let mut returned = false;
        if let Some(old) = &s.id {
            // Gone meanwhile (logged out, regenerated or expired by another
            // request): saving would bring it back for whoever holds the old id.
            let e = inner.map.remove(old)?;
            inner.bytes -= e.bytes;
            created = e.created;
            returned = e.returned;
        }
        let bytes = size(&s.values);
        if inner.map.len() >= MAX_SESSIONS || inner.bytes + bytes > MAX_BYTES {
            self.evict(&mut inner, bytes);
        }
        let issue_cookie = s.id.as_deref() != Some(new_id.as_str());
        inner.bytes += bytes;
        inner.map.insert(new_id.clone(), Entry { values: s.values, created, last: Instant::now(), bytes, returned });
        issue_cookie.then(|| self.cookie_for(Some(&new_id)))
    }

    fn save_file(&self, files: &dyn Backend, s: SessionData) -> Option<String> {
        files.sweep(self.ttl);
        if s.destroyed {
            if let Some(id) = &s.id {
                files.remove(id);
            }
            return s.id.map(|_| self.cookie_for(None));
        }
        if !s.changed || (s.id.is_none() && s.values.is_empty()) {
            return None;
        }
        let new_id = match (&s.id, s.regenerate) {
            (Some(id), false) => id.clone(),
            _ => phv_std::random_hex(32),
        };
        let now = now_secs();
        let mut created = now;
        let mut returned = false;
        if let Some(old) = &s.id {
            // As in the memory store: a session removed meanwhile stays removed.
            let (r, back) = files.read(old)?;
            created = r.created;
            returned = back;
            if old != &new_id {
                files.remove(old);
            }
        }
        if let Err(e) = files.write(&new_id, &Record { created, last: now, values: s.values }, returned) {
            eprintln!("warning: cannot save session: {e}");
            return None;
        }
        let issue_cookie = s.id.as_deref() != Some(new_id.as_str());
        issue_cookie.then(|| self.cookie_for(Some(&new_id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_and_reuses_ids() {
        let store = Store::new(Duration::from_secs(60), true, None);
        assert_eq!(store.cookie, "__Host-PHVSESSID");
        // A request without a session that sets nothing gets no cookie.
        assert!(store.save(store.load(None)).is_none());
        let mut s = store.load(None);
        s.values.insert("user".into(), "ann".into());
        s.changed = true;
        let cookie = store.save(s).unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("Secure") && cookie.contains("SameSite=Lax"));
        let id = cookie.split(['=', ';']).nth(1).unwrap().to_string();
        let header = format!("other=1; __Host-PHVSESSID={id}");
        let loaded = store.load(Some(&header));
        assert_eq!(loaded.values.get("user").map(String::as_str), Some("ann"));
        // An attacker-chosen id is ignored.
        let forged = store.load(Some(&format!("__Host-PHVSESSID={}", "a".repeat(64))));
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
    fn a_logged_out_session_stays_gone() {
        for dir in [None, Some(std::env::temp_dir().join(format!("phv-sess-gone-{}", std::process::id())))] {
            let store = Store::new(Duration::from_secs(60), false, dir.clone());
            let mut s = SessionData::default();
            s.values.insert("user".into(), "alice".into());
            s.changed = true;
            let cookie = store.save(s).unwrap();
            let header = format!("PHVSESSID={}", cookie.split(['=', ';']).nth(1).unwrap());
            // A slow request loads the session, then the user logs out.
            let mut slow = store.load(Some(&header));
            let mut logout = store.load(Some(&header));
            logout.destroyed = true;
            store.save(logout);
            // The slow request finishes and saves: the session must not come back.
            slow.values.insert("seen".into(), "1".into());
            slow.changed = true;
            assert!(store.save(slow).is_none());
            assert!(store.load(Some(&header)).id.is_none());
            if let Some(d) = dir {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }

    /// The database stores keep the rules of the others: values come back,
    /// a logged-out session stays gone, and the sweep removes expired
    /// sessions and then the excess, those whose visitor never came back first.
    /// Runs against the servers named by `PHV_TEST_REDIS_URL`,
    /// `PHV_TEST_POSTGRES_URL` and `PHV_TEST_MYSQL_URL` (skipped without them).
    #[test]
    fn database_stores_keep_the_same_rules() {
        for var in ["PHV_TEST_REDIS_URL", "PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
            let Some(url) = std::env::var(var).ok().filter(|u| !u.is_empty()) else {
                eprintln!("skipped: {var} is not set");
                continue;
            };
            let spec = phv_vm::DatabaseSpec { url: Some(url), url_env: None, tls: None, pool_size: 2 };
            let store = Store::with_database(Duration::from_secs(60), false, spec.clone());
            let mut s = SessionData::default();
            s.values.insert("user".into(), "alice".into());
            s.changed = true;
            let cookie = store.save(s).unwrap();
            let header = format!("PHVSESSID={}", cookie.split(['=', ';']).nth(1).unwrap());
            assert_eq!(store.load(Some(&header)).values.get("user").map(String::as_str), Some("alice"), "{var}");
            // Logged out while a slow request still holds it: it stays gone.
            let mut slow = store.load(Some(&header));
            let mut logout = store.load(Some(&header));
            logout.destroyed = true;
            store.save(logout);
            slow.values.insert("seen".into(), "1".into());
            slow.changed = true;
            assert!(store.save(slow).is_none(), "{var}");
            assert!(store.load(Some(&header)).id.is_none(), "{var}");

            if var == "PHV_TEST_REDIS_URL" {
                continue;
            }
            // The sweep: one expired session, two returning and two new ones, room for three.
            let now = now_secs();
            let row =
                |last: u64, returned: bool| phv_std::SessionRow { data: "{}".into(), created: last, last, returned };
            let ttl = Duration::from_secs(60);
            let keys = ["sweep-expired", "sweep-back-1", "sweep-back-2", "sweep-new-1", "sweep-new-2"];
            let rows = [
                row(now - 3600, true),
                row(now - 5, true),
                row(now - 4, true),
                row(now - 3, false),
                row(now - 2, false),
            ];
            for (k, r) in keys.iter().zip(&rows) {
                phv_std::session_write(&spec, k, r, ttl).unwrap();
            }
            phv_std::session_sweep(&spec, ttl, MAX_LIFETIME, 3).unwrap();
            let left: Vec<&str> =
                keys.iter().copied().filter(|k| phv_std::session_read(&spec, k).unwrap().is_some()).collect();
            // 90% of 3 is 2: the new ones go first.
            assert_eq!(left, ["sweep-back-1", "sweep-back-2"], "{var}");
            for k in keys {
                phv_std::session_remove(&spec, k).unwrap();
            }
        }
    }

    #[test]
    fn the_sweep_only_touches_session_files() {
        assert!(is_session_file(&format!("{}.json", "a".repeat(64))));
        assert!(is_session_file(&format!(".{}.json.{}", "b".repeat(64), "c".repeat(16))));
        assert!(is_session_file(&format!("n-{}.json", "a".repeat(64))));
        assert!(is_session_file(&format!(".n-{}.json.{}", "b".repeat(64), "c".repeat(16))));
        let x = format!("x-{}.json", "a".repeat(64));
        for other in ["phv.toml", "app.db", ".gitignore", "x.json", "index.phv", "n-x.json", &x] {
            assert!(!is_session_file(other), "{other}");
        }
    }

    fn file_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn a_new_session_file_is_renamed_when_its_cookie_comes_back() {
        let dir = std::env::temp_dir().join(format!("phv-sess-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(Duration::from_secs(60), false, Some(dir.clone()));
        let mut s = SessionData::default();
        s.values.insert("user".into(), "ann".into());
        s.changed = true;
        let cookie = store.save(s).unwrap();
        let id = cookie.split(['=', ';']).nth(1).unwrap().to_string();
        let header = format!("PHVSESSID={id}");
        let (back, new) = (FileStore::name(&id, true), FileStore::name(&id, false));
        assert!(file_names(&dir).contains(&new) && !file_names(&dir).contains(&back));
        for _ in 0..2 {
            assert_eq!(store.load(Some(&header)).values.get("user").map(String::as_str), Some("ann"));
            assert!(file_names(&dir).contains(&back) && !file_names(&dir).contains(&new));
        }
        // Saving it again keeps the returned name.
        let mut s = store.load(Some(&header));
        s.values.insert("seen".into(), "1".into());
        s.changed = true;
        store.save(s);
        assert!(file_names(&dir).contains(&back) && !file_names(&dir).contains(&new));
        assert_eq!(store.load(Some(&header)).values.get("seen").map(String::as_str), Some("1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// As with the memory store: a flood of new sessions past the limits does
    /// not remove the file of a visitor who came back, even when it is the oldest.
    #[test]
    fn a_flood_does_not_sweep_returning_visitors_files() {
        let dir = std::env::temp_dir().join(format!("phv-sess-flood-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::new(Duration::from_secs(600), false, Some(dir.clone()));
        let files = store.files.as_mut().unwrap().file_store().unwrap();
        files.max_sessions = 20;
        files.max_bytes = 20 * 200;
        let mut s = SessionData::default();
        s.values.insert("user".into(), "ann".into());
        s.changed = true;
        let cookie = store.save(s).unwrap();
        let id = cookie.split(['=', ';']).nth(1).unwrap().to_string();
        let header = format!("PHVSESSID={id}");
        assert!(store.load(Some(&header)).id.is_some());
        // Ann's file is the oldest of all.
        let old = SystemTime::now() - Duration::from_secs(300);
        let path = dir.join(FileStore::name(&id, true));
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        for i in 0..50 {
            let mut s = SessionData::default();
            s.values.insert("n".into(), i.to_string());
            s.changed = true;
            assert!(store.save(s).is_some());
        }
        let ttl = store.ttl;
        let files = store.files.as_mut().unwrap().file_store().unwrap();
        files.swept.store(0, Ordering::Relaxed);
        files.sweep(ttl);
        let names = file_names(&dir);
        assert!(names.iter().filter(|n| is_session_file(n)).count() <= 20);
        assert!(names.iter().any(|n| n.starts_with("n-")), "only the excess is swept");
        assert_eq!(store.load(Some(&header)).values.get("user").map(String::as_str), Some("ann"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flood_of_sessions_evicts_old_ones() {
        let store = Store::new(Duration::from_secs(600), false, None);
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
        assert!(store.load(Some(&format!("PHVSESSID={id}"))).id.is_none(), "oldest session was evicted");
    }

    /// A visitor who came back keeps their session through a flood of new ones.
    #[test]
    fn a_flood_does_not_evict_returning_visitors() {
        let store = Store::new(Duration::from_secs(600), false, None);
        let mut s = SessionData::default();
        s.values.insert("user".into(), "ann".into());
        s.changed = true;
        let cookie = store.save(s).unwrap();
        let header = format!("PHVSESSID={}", cookie.split(['=', ';']).nth(1).unwrap());
        assert!(store.load(Some(&header)).id.is_some());
        for i in 0..MAX_SESSIONS + 10 {
            let mut s = SessionData::default();
            s.values.insert("n".into(), i.to_string());
            s.changed = true;
            store.save(s);
        }
        assert_eq!(store.load(Some(&header)).values.get("user").map(String::as_str), Some("ann"));
    }

    #[test]
    fn file_store_survives_restarts() {
        let dir = std::env::temp_dir().join(format!("phv-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(Duration::from_secs(60), false, Some(dir.clone()));
        let mut s = SessionData::default();
        s.values.insert("user".into(), "ann".into());
        s.changed = true;
        let cookie = store.save(s).unwrap();
        let id = cookie.split(['=', ';']).nth(1).unwrap().to_string();
        let header = format!("PHVSESSID={id}");
        // Files are named by a hash, never by the id itself.
        let names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.contains(&id)));
        // A second store (another process, or after a restart) sees the session.
        let again = Store::new(Duration::from_secs(60), false, Some(dir.clone()));
        let loaded = again.load(Some(&header));
        assert_eq!(loaded.values.get("user").map(String::as_str), Some("ann"));
        assert!(again.load(Some(&format!("PHVSESSID={}", "b".repeat(64)))).id.is_none());
        // regenerate() moves it to a new id; destroying removes it.
        let mut s = again.load(Some(&header));
        s.regenerate = true;
        s.changed = true;
        let cookie2 = again.save(s).unwrap();
        assert!(again.load(Some(&header)).id.is_none());
        let header2 = format!("PHVSESSID={}", cookie2.split(['=', ';']).nth(1).unwrap());
        let mut s = again.load(Some(&header2));
        assert!(s.id.is_some());
        s.destroyed = true;
        assert!(again.save(s).unwrap().contains("Max-Age=0"));
        assert!(again.load(Some(&header2)).id.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
