//! Server-side session store. Only a random id travels in the cookie; values
//! stay on the server. Unknown ids are never adopted (no session fixation).
//!
//! The store is bounded by count and by bytes, so a flood of new sessions
//! cannot exhaust memory or disk. When full, sessions whose visitor never came
//! back go first (the least recently used), then the newest of the rest: a
//! flood of sessions, even ones whose cookie is sent back, evicts its own
//! sessions and never those created before it, however long they have been
//! idle. (Idle sessions still end after the session lifetime.)
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
//!
//! In every store a session is only ever inserted under a new id or updated
//! while it still exists, each as one atomic step, so a request that finishes
//! after a logout (in this process or another) cannot bring the session back.

use crate::metrics::{Health, StoreKind};
use indexmap::IndexMap;
use phv_std::safefs;
use phv_vm::SessionData;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_SESSIONS: usize = 50_000;

/// How much a session that came back is worth keeping when the store is full,
/// from its age and idle time in seconds: the older, the more (a flood cannot
/// evict sessions created before it).
fn keep_score(age: f64, _idle: f64) -> f64 {
    age
}
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
    /// Stores a session under a new id; false if the id is taken.
    fn insert(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<bool>;
    /// Replaces a session that still exists; false (nothing stored) if it is gone.
    fn update(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<bool>;
    /// Removes a session; false if it was not there.
    fn remove(&self, id: &str) -> bool;
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

/// The key in a shared database: in the site's namespace when it has one, so
/// a session of one site is not one of another.
fn shared_session_key(namespace: Option<&str>, id: &str) -> String {
    match namespace {
        Some(ns) => phv_std::sha256_hex(format!("phv-session:{ns}:{id}").as_bytes()),
        None => session_key(id),
    }
}

/// A session's values as every store keeps them: a JSON object of strings.
/// (Where its times are kept differs: in a session file's JSON, in columns of
/// `phv_sessions` that its sweep sorts by, in the header of a Redis value
/// that the store's scripts read.)
fn values_json(values: &IndexMap<String, String>) -> String {
    serde_json::json!(values).to_string()
}

fn values_from_json(text: &str) -> Option<IndexMap<String, String>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    values_of(&v)
}

fn values_of(v: &serde_json::Value) -> Option<IndexMap<String, String>> {
    v.as_object()?.iter().map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()
}

/// Sessions in a database of `[databases]`: Redis keeps them under
/// `phv:session:<hash>`, expires them itself (a session whose visitor has not
/// come back after an hour) and keeps within `MAX_SESSIONS` and `MAX_BYTES` as
/// sessions are written, with bookkeeping keys of the site's own
/// (`phv:sessions:<namespace>:`); PostgreSQL and MySQL in a table of the
/// site's own (`phv_sessions_<namespace>`), which the sweep keeps within the
/// same limits. So the `session_ttl` and the limits of one site never remove
/// another's sessions. Both drop sessions whose visitor never came back first.
/// A database that cannot be reached loses the session (a warning is logged)
/// instead of failing the request.
struct DbStore {
    spec: phv_vm::DatabaseSpec,
    limits: phv_std::SessionLimits,
    /// Unix time of the last sweep.
    swept: AtomicU64,
    /// Sessions written since the last sweep; a burst triggers an early sweep.
    written: AtomicU64,
    /// Whether the database answered (`/ready`, `/metrics`).
    health: Arc<Health>,
    /// The site's namespace in the shared database (`Host::store_namespace`).
    namespace: Option<String>,
}

impl DbStore {
    fn key(&self, id: &str) -> String {
        shared_session_key(self.namespace.as_deref(), id)
    }

    fn ns(&self) -> Option<&str> {
        self.namespace.as_deref()
    }
}

impl DbStore {
    fn warn(what: &str, e: String) {
        eprintln!("warning: cannot {what} the session in the database: {e}");
    }

    /// Notes whether the database answered.
    fn seen<T>(&self, r: Result<T, String>) -> Result<T, String> {
        match &r {
            Ok(_) => self.health.ok(StoreKind::Sessions),
            Err(_) => self.health.failed(StoreKind::Sessions),
        }
        r
    }

    fn row(r: &Record, returned: bool) -> phv_std::SessionRow {
        phv_std::SessionRow { data: values_json(&r.values), created: r.created, last: r.last, returned }
    }
}

impl Backend for DbStore {
    fn read(&self, id: &str) -> Option<(Record, bool)> {
        // Sessions made before the site had a namespace (1.0) are not read: a
        // site that could read them could read those of any site without one.
        let row = match self.seen(phv_std::session_read(&self.spec, self.ns(), &self.key(id))) {
            Ok(r) => r?,
            Err(e) => {
                Self::warn("read", e);
                return None;
            }
        };
        let values = values_from_json(&row.data)?;
        Some((Record { created: row.created, last: row.last, values }, row.returned))
    }

    fn insert(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<bool> {
        self.written.fetch_add(1, Ordering::Relaxed);
        self.seen(phv_std::session_insert(&self.spec, self.ns(), &self.key(id), &Self::row(r, returned), &self.limits))
            .map_err(std::io::Error::other)
    }

    fn update(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<bool> {
        self.seen(phv_std::session_update(&self.spec, self.ns(), &self.key(id), &Self::row(r, returned), &self.limits))
            .map_err(std::io::Error::other)
    }

    fn remove(&self, id: &str) -> bool {
        self.seen(phv_std::session_remove(&self.spec, self.ns(), &self.key(id))).unwrap_or_else(|e| {
            Self::warn("remove", e);
            false
        })
    }

    fn promote(&self, id: &str) -> bool {
        let Some((r, _)) = self.read(id) else { return false };
        self.update(id, &r, true).unwrap_or(false)
    }

    /// Every 30 seconds, or sooner after a burst of new sessions.
    fn sweep(&self, _ttl: Duration) {
        let now = now_secs();
        let last = self.swept.load(Ordering::Relaxed);
        let burst = self.written.load(Ordering::Relaxed) >= 1000;
        if (now < last + 30 && !burst)
            || self.swept.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err()
        {
            return;
        }
        self.written.store(0, Ordering::Relaxed);
        if let Err(e) = phv_std::session_sweep(&self.spec, self.ns(), &self.limits) {
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

impl Record {
    fn to_json(&self) -> String {
        serde_json::json!({"created": self.created, "last": self.last, "values": self.values}).to_string()
    }

    fn from_json(text: &str) -> Option<Record> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        Some(Record { created: v["created"].as_u64()?, last: v["last"].as_u64()?, values: values_of(&v["values"])? })
    }
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
    /// `.phv-session.lock` in the directory, once opened. Every change to the
    /// sessions holds it (and an OS lock on it, for other processes).
    lock: Mutex<Option<File>>,
}

/// Holds the OS lock on the session directory's lock file until dropped.
struct DirLock<'a>(std::sync::MutexGuard<'a, Option<File>>);

impl Drop for DirLock<'_> {
    fn drop(&mut self) {
        if let Some(f) = self.0.as_ref() {
            let _ = f.unlock();
        }
    }
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

    fn insert(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<bool> {
        let _held = self.lock_dir()?;
        if self.exists(id, false) || self.exists(id, true) {
            return Ok(false);
        }
        self.write(id, r, returned).map(|()| true)
    }

    fn update(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<bool> {
        let _held = self.lock_dir()?;
        let (back, new) = (self.exists(id, true), self.exists(id, false));
        if !back && !new {
            return Ok(false);
        }
        // Another request may have promoted it meanwhile.
        let returned = returned || back;
        self.write(id, r, returned)?;
        if returned && new {
            self.remove_file(id, false);
        }
        Ok(true)
    }

    fn remove(&self, id: &str) -> bool {
        let _held = self.lock_dir();
        let removed = [false, true].map(|returned| self.remove_file(id, returned));
        removed.contains(&true)
    }

    fn promote(&self, id: &str) -> bool {
        match self.lock_dir() {
            Ok(_held) => FileStore::promote(self, id),
            Err(_) => false,
        }
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
    fn new(dir: PathBuf) -> FileStore {
        FileStore {
            dir,
            swept: AtomicU64::new(0),
            written: AtomicU64::new(0),
            max_sessions: MAX_SESSIONS,
            max_bytes: MAX_BYTES as u64,
            lock: Mutex::new(None),
        }
    }

    /// Locks the store against changes by other threads and processes.
    fn lock_dir(&self) -> std::io::Result<DirLock<'_>> {
        let mut held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if held.is_none() {
            let fail = |e: safefs::FsError| std::io::Error::other(format!("{e:?}"));
            let (dir, name) = safefs::open_parent(&self.dir, Path::new(".phv-session.lock")).map_err(fail)?;
            *held = Some(dir.open(&name, true).map_err(fail)?);
        }
        held.as_ref().expect("opened above").lock()?;
        Ok(DirLock(held))
    }

    fn exists(&self, id: &str, returned: bool) -> bool {
        safefs::open_parent(&self.dir, Path::new(&Self::name(id, returned)))
            .is_ok_and(|(dir, name)| dir.exists(&name).unwrap_or(false))
    }

    fn remove_file(&self, id: &str, returned: bool) -> bool {
        safefs::open_parent(&self.dir, Path::new(&Self::name(id, returned)))
            .is_ok_and(|(dir, name)| dir.exists(&name).unwrap_or(false) && dir.remove(&name).is_ok())
    }

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
        Record::from_json(&text)
    }

    /// Writes the file atomically: a temporary file renamed over the old one.
    fn write(&self, id: &str, r: &Record, returned: bool) -> std::io::Result<()> {
        let text = r.to_json();
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
        let Ok(_held) = self.lock_dir() else { return };
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
        // Sessions that came back go newest first; the creation time is at the
        // start of the file (read only when evicting).
        let created_of = |path: &Path| -> Option<u64> {
            use std::io::Read;
            let mut head = [0u8; 40];
            let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
            let text = std::str::from_utf8(&head[..n]).ok()?.strip_prefix("{\"created\":")?;
            text.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
        };
        let now_t = SystemTime::now();
        let mut ranked: Vec<(bool, f64, u64, PathBuf)> = live
            .into_iter()
            .map(|(returned, modified, len, path)| {
                let idle = now_t.duration_since(modified).map_or(0.0, |d| d.as_secs_f64());
                let score = if returned {
                    let age = created_of(&path).map_or(idle, |c| now.saturating_sub(c) as f64);
                    keep_score(age, idle)
                } else {
                    -idle
                };
                (returned, score, len, path)
            })
            .collect();
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        let mut count = ranked.len();
        for (_, _, len, path) in ranked {
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
            Box::new(FileStore::new(dir)) as Box<dyn Backend>
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
    pub fn with_database(
        ttl: Duration,
        secure: bool,
        spec: phv_vm::DatabaseSpec,
        health: Arc<Health>,
        namespace: Option<String>,
    ) -> Store {
        let mut store = Store::new(ttl, secure, None);
        let limits = phv_std::SessionLimits {
            ttl,
            max_lifetime: MAX_LIFETIME,
            max_sessions: MAX_SESSIONS,
            max_bytes: MAX_BYTES,
        };
        store.files = Some(Box::new(DbStore {
            spec,
            limits,
            swept: AtomicU64::new(0),
            written: AtomicU64::new(0),
            health,
            namespace,
        }));
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
            // Refresh the expiry, at most once a minute per session (never
            // bringing back one that a logout removed meanwhile).
            if now >= r.last + 60 {
                r.last = now;
                if let Ok(false) = files.update(&id, &r, returned) {
                    return SessionData::default();
                }
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
        // Drop a tenth: first sessions that were never used again (the least
        // recently used), then the newest of those that were.
        let mut by_age: Vec<(bool, f64, String)> = inner
            .map
            .iter()
            .map(|(k, e)| {
                let (idle, age) = (e.last.elapsed().as_secs_f64(), e.created.elapsed().as_secs_f64());
                (e.returned, if e.returned { keep_score(age, idle) } else { -idle }, k.clone())
            })
            .collect();
        by_age.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
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
        let mut record = Record { created: now, last: now, values: s.values };
        // As in the memory store: a session removed meanwhile (logged out,
        // regenerated or expired by another request) stays removed. Each step
        // below is atomic in the store, so this holds across processes too.
        let saved = match &s.id {
            Some(old) => {
                let (r, back) = files.read(old)?;
                record.created = r.created;
                if old == &new_id {
                    files.update(old, &record, back)
                } else if files.remove(old) {
                    // A regenerated id: only one request can take over the old session.
                    files.insert(&new_id, &record, back)
                } else {
                    Ok(false)
                }
            }
            None => files.insert(&new_id, &record, false),
        };
        match saved {
            Ok(true) => {}
            Ok(false) => return None,
            Err(e) => {
                eprintln!("warning: cannot save session: {e}");
                return None;
            }
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

    /// Two server processes share a session directory (two stores here):
    /// requests that save while another process logs the session out, from
    /// many threads at once, never bring it back, and of two requests that
    /// regenerate the same session only one takes it over.
    #[test]
    fn a_logout_in_another_process_wins_the_race() {
        let dir = std::env::temp_dir().join(format!("phv-sess-race-{}", std::process::id()));
        let (a, b) = (
            Store::new(Duration::from_secs(60), false, Some(dir.clone())),
            Store::new(Duration::from_secs(60), false, Some(dir.clone())),
        );
        for round in 0..20 {
            let mut s = SessionData::default();
            s.values.insert("user".into(), "alice".into());
            s.changed = true;
            let cookie = a.save(s).unwrap();
            let header = format!("PHVSESSID={}", cookie.split(['=', ';']).nth(1).unwrap());
            let loaded: Vec<SessionData> = (0..8).map(|_| a.load(Some(&header))).collect();
            std::thread::scope(|scope| {
                for (i, mut slow) in loaded.into_iter().enumerate() {
                    let (a, b) = (&a, &b);
                    scope.spawn(move || {
                        slow.values.insert("seen".into(), i.to_string());
                        slow.changed = true;
                        let _ = if i % 2 == 0 { a.save(slow) } else { b.save(slow) };
                    });
                }
                let mut logout = b.load(Some(&header));
                logout.destroyed = true;
                b.save(logout);
            });
            assert!(a.load(Some(&header)).id.is_none(), "round {round}");
            assert!(b.load(Some(&header)).id.is_none(), "round {round}");
        }
        // Login in two tabs at once: both regenerate the same session.
        let mut s = SessionData::default();
        s.values.insert("cart".into(), "1".into());
        s.changed = true;
        let cookie = a.save(s).unwrap();
        let header = format!("PHVSESSID={}", cookie.split(['=', ';']).nth(1).unwrap());
        let (mut x, mut y) = (a.load(Some(&header)), b.load(Some(&header)));
        x.regenerate = true;
        x.changed = true;
        y.regenerate = true;
        y.changed = true;
        assert!(a.save(x).is_some());
        assert!(b.save(y).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The tests on database servers share the stores of sites without a
    /// namespace: one at a time.
    static DB_TESTS: Mutex<()> = Mutex::new(());

    /// The database stores keep the rules of the others: values come back,
    /// a logged-out session stays gone, and the sweep removes expired
    /// sessions and then the excess, those whose visitor never came back first.
    /// Runs against the servers named by `PHV_TEST_REDIS_URL`,
    /// `PHV_TEST_POSTGRES_URL` and `PHV_TEST_MYSQL_URL` (skipped without them).
    #[test]
    fn database_stores_keep_the_same_rules() {
        let _one_at_a_time = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        for var in ["PHV_TEST_REDIS_URL", "PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
            let Some(url) = std::env::var(var).ok().filter(|u| !u.is_empty()) else {
                eprintln!("skipped: {var} is not set");
                continue;
            };
            let spec = phv_vm::DatabaseSpec { url: Some(url), url_env: None, tls: None, pool_size: 2 };
            let store = Store::with_database(Duration::from_secs(60), false, spec.clone(), Arc::default(), None);
            let mut s = SessionData::default();
            s.values.insert("user".into(), "alice".into());
            s.changed = true;
            let cookie = store.save(s).unwrap();
            let header = format!("PHVSESSID={}", cookie.split(['=', ';']).nth(1).unwrap());
            assert_eq!(store.load(Some(&header)).values.get("user").map(String::as_str), Some("alice"), "{var}");
            // Another site on the same database does not take the cookie
            // (found by the 1.1 audit).
            let other =
                Store::with_database(Duration::from_secs(60), false, spec.clone(), Arc::default(), Some("b".into()));
            assert!(other.load(Some(&header)).id.is_none(), "{var}: another site took the session");
            // Logged out while a slow request still holds it: it stays gone.
            let mut slow = store.load(Some(&header));
            let mut logout = store.load(Some(&header));
            logout.destroyed = true;
            store.save(logout);
            slow.values.insert("seen".into(), "1".into());
            slow.changed = true;
            assert!(store.save(slow).is_none(), "{var}");
            assert!(store.load(Some(&header)).id.is_none(), "{var}");

            // Two requests regenerate the same session: only one takes it over.
            let mut s = SessionData::default();
            s.values.insert("cart".into(), "1".into());
            s.changed = true;
            let cookie = store.save(s).unwrap();
            let header = format!("PHVSESSID={}", cookie.split(['=', ';']).nth(1).unwrap());
            let (mut x, mut y) = (store.load(Some(&header)), store.load(Some(&header)));
            for r in [&mut x, &mut y] {
                r.regenerate = true;
                r.changed = true;
            }
            let taken = store.save(x).unwrap();
            assert!(store.save(y).is_none(), "{var}");
            let header = format!("PHVSESSID={}", taken.split(['=', ';']).nth(1).unwrap());
            let mut done = store.load(Some(&header));
            assert_eq!(done.values.get("cart").map(String::as_str), Some("1"), "{var}");
            done.destroyed = true;
            store.save(done);

            let now = now_secs();
            let row =
                |last: u64, returned: bool| phv_std::SessionRow { data: "{}".into(), created: last, last, returned };
            // The limits below are a site's own (a namespace of the tests).
            let ns = Some("5e55a1");
            let left_of = |ns: Option<&str>, keys: &[&'static str]| -> Vec<&'static str> {
                keys.iter().copied().filter(|k| phv_std::session_read(&spec, ns, k).unwrap().is_some()).collect()
            };
            if var == "PHV_TEST_REDIS_URL" {
                // Redis keeps within the limits as sessions are written: new
                // sessions go first, never the one being written.
                let limits = phv_std::SessionLimits {
                    ttl: Duration::from_secs(60),
                    max_lifetime: MAX_LIFETIME,
                    max_sessions: 3,
                    max_bytes: MAX_BYTES,
                };
                let keys = ["cap-back-1", "cap-back-2", "cap-new-1", "cap-new-2"];
                for (i, k) in keys.iter().enumerate() {
                    let _ = phv_std::session_remove(&spec, ns, k);
                    assert!(phv_std::session_insert(&spec, ns, k, &row(now, i < 2), &limits).unwrap());
                }
                assert!(!phv_std::session_insert(&spec, ns, "cap-new-2", &row(now, false), &limits).unwrap());
                assert_eq!(left_of(ns, &keys), ["cap-back-1", "cap-back-2", "cap-new-2"]);
                // A byte limit evicts the others, returning visitors' too.
                let tiny = phv_std::SessionLimits { max_bytes: 1, ..limits };
                assert!(phv_std::session_update(&spec, ns, "cap-new-2", &row(now, true), &tiny).unwrap());
                assert_eq!(left_of(ns, &keys), ["cap-new-2"]);
                assert!(phv_std::session_remove(&spec, ns, "cap-new-2").unwrap());
                assert!(!phv_std::session_update(&spec, ns, "cap-new-2", &row(now, true), &limits).unwrap());
            } else {
                // The sweep: one expired session, two returning and two new ones, room for three.
                let ttl = Duration::from_secs(60);
                let keys = ["sweep-expired", "sweep-back-1", "sweep-back-2", "sweep-new-1", "sweep-new-2"];
                let rows = [
                    row(now - 3600, true),
                    row(now - 5, true),
                    row(now - 4, true),
                    row(now - 3, false),
                    row(now - 2, false),
                ];
                let limits =
                    phv_std::SessionLimits { ttl, max_lifetime: MAX_LIFETIME, max_sessions: 3, max_bytes: MAX_BYTES };
                for (k, r) in keys.iter().zip(&rows) {
                    let _ = phv_std::session_remove(&spec, ns, k);
                    assert!(phv_std::session_insert(&spec, ns, k, r, &limits).unwrap());
                }
                phv_std::session_sweep(&spec, ns, &limits).unwrap();
                // 90% of 3 is 2: the new ones go first.
                assert_eq!(left_of(ns, &keys), ["sweep-back-1", "sweep-back-2"], "{var}");
                for k in keys {
                    phv_std::session_remove(&spec, ns, k).unwrap();
                }
            }

            // Two sites on the same database, one with a short session_ttl and
            // room for two sessions: what it writes and sweeps never removes
            // the other site's sessions (found by the 1.1.1 audit: the shortest
            // ttl and the limits applied to every site).
            let (long_ns, short_ns) = (Some("5e55b2"), Some("5e55c3"));
            let long = phv_std::SessionLimits {
                ttl: Duration::from_secs(7200),
                max_lifetime: MAX_LIFETIME,
                max_sessions: 100,
                max_bytes: MAX_BYTES,
            };
            let short = phv_std::SessionLimits {
                ttl: Duration::from_secs(1),
                max_lifetime: MAX_LIFETIME,
                max_sessions: 2,
                max_bytes: MAX_BYTES,
            };
            let theirs = ["site-long-back", "site-long-new"];
            let mine = ["site-short-1", "site-short-2", "site-short-3"];
            for k in theirs {
                let _ = phv_std::session_remove(&spec, long_ns, k);
            }
            for k in mine {
                let _ = phv_std::session_remove(&spec, short_ns, k);
            }
            // Idle for two minutes: past the short ttl, well within the long one.
            let idle = if var == "PHV_TEST_REDIS_URL" { now } else { now - 120 };
            assert!(phv_std::session_insert(&spec, long_ns, theirs[0], &row(idle, true), &long).unwrap());
            assert!(phv_std::session_insert(&spec, long_ns, theirs[1], &row(idle, false), &long).unwrap());
            if var == "PHV_TEST_REDIS_URL" {
                // Redis takes the time of the last use from its own writes:
                // wait past the short ttl (its times are in seconds).
                std::thread::sleep(Duration::from_millis(2100));
            }
            for k in mine {
                assert!(phv_std::session_insert(&spec, short_ns, k, &row(now_secs(), false), &short).unwrap());
            }
            phv_std::session_sweep(&spec, short_ns, &short).unwrap();
            assert_eq!(left_of(long_ns, &theirs), theirs, "{var}: the other site's sessions were removed");
            let kept = left_of(short_ns, &mine);
            assert!(kept.len() <= 2, "{var}: {kept:?}");
            for k in theirs {
                phv_std::session_remove(&spec, long_ns, k).unwrap();
            }
            for k in mine {
                phv_std::session_remove(&spec, short_ns, k).unwrap();
            }
        }
    }

    /// Sessions that 1.1 stored for a site with a namespace (in the shared
    /// `phv_sessions` table, or with the shared bookkeeping of Redis) are
    /// still read, updated and logged out after the upgrade, as atomically as
    /// before: no one is signed out by it, and a request that finishes after a
    /// logout does not bring the session back. Needs the servers of
    /// `database_stores_keep_the_same_rules`.
    #[test]
    fn sessions_written_by_1_1_are_still_read() {
        let _one_at_a_time = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        for var in ["PHV_TEST_REDIS_URL", "PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
            let Some(url) = std::env::var(var).ok().filter(|u| !u.is_empty()) else {
                eprintln!("skipped: {var} is not set");
                continue;
            };
            let spec = phv_vm::DatabaseSpec { url: Some(url), url_env: None, tls: None, pool_size: 2 };
            let ns = "5e55d4".to_string();
            let limits = phv_std::SessionLimits {
                ttl: Duration::from_secs(3600),
                max_lifetime: MAX_LIFETIME,
                max_sessions: MAX_SESSIONS,
                max_bytes: MAX_BYTES,
            };
            // Written as 1.1 wrote them: the site's key, in the shared store
            // (the store of a site without a namespace).
            let now = now_secs();
            let old_session = |user: &str| -> String {
                let id = phv_std::random_hex(32);
                let row = phv_std::SessionRow {
                    data: format!("{{\"user\":\"{user}\"}}"),
                    created: now - 100,
                    last: now - 100,
                    returned: true,
                };
                let key = shared_session_key(Some(&ns), &id);
                assert!(phv_std::session_insert(&spec, None, &key, &row, &limits).unwrap(), "{var}");
                format!("PHVSESSID={id}")
            };
            let store =
                Store::with_database(Duration::from_secs(3600), false, spec.clone(), Arc::default(), Some(ns.clone()));
            let user = |h: &str| store.load(Some(h)).values.get("user").cloned();

            // Read, updated where it is, and read again.
            let header = old_session("ann");
            assert_eq!(user(&header).as_deref(), Some("ann"), "{var}: a session of 1.1 was not read");
            let mut s = store.load(Some(&header));
            s.values.insert("seen".into(), "1".into());
            s.changed = true;
            assert!(store.save(s).is_none(), "{var}: the same id");
            assert_eq!(store.load(Some(&header)).values.get("seen").map(String::as_str), Some("1"), "{var}");
            if var == "PHV_TEST_REDIS_URL" {
                // Written once since, it left the shared bookkeeping: a site of
                // 1.1 with a short ttl writing there does not remove it.
                std::thread::sleep(Duration::from_millis(2100));
                let brief = phv_std::SessionLimits { ttl: Duration::from_secs(1), ..limits };
                let other = phv_std::SessionRow { data: "{}".into(), created: now, last: now, returned: false };
                assert!(phv_std::session_insert(&spec, None, "legacy-writer", &other, &brief).unwrap());
                phv_std::session_remove(&spec, None, "legacy-writer").unwrap();
                assert_eq!(user(&header).as_deref(), Some("ann"), "{var}: removed by the shared bookkeeping");
            }
            // Logged out while a slow request holds it: gone from both stores.
            let mut slow = store.load(Some(&header));
            let mut logout = store.load(Some(&header));
            logout.destroyed = true;
            store.save(logout);
            slow.values.insert("late".into(), "1".into());
            slow.changed = true;
            assert!(store.save(slow).is_none(), "{var}");
            assert!(store.load(Some(&header)).id.is_none(), "{var}: the logged-out session came back");
            let id = header.trim_start_matches("PHVSESSID=");
            assert!(phv_std::session_read(&spec, None, &shared_session_key(Some(&ns), id)).unwrap().is_none());

            // Regenerated (at login) by two requests at once: one takes it
            // over, under a new id in the site's own store.
            let header = old_session("bob");
            let (mut x, mut y) = (store.load(Some(&header)), store.load(Some(&header)));
            for r in [&mut x, &mut y] {
                r.regenerate = true;
                r.changed = true;
            }
            let taken = store.save(x).unwrap();
            assert!(store.save(y).is_none(), "{var}");
            assert!(store.load(Some(&header)).id.is_none(), "{var}");
            let new_id = taken.split(['=', ';']).nth(1).unwrap().to_string();
            if var != "PHV_TEST_REDIS_URL" {
                let key = shared_session_key(Some(&ns), &new_id);
                assert!(phv_std::session_read(&spec, None, &key).unwrap().is_none(), "{var}: in the shared table");
            }
            let header = format!("PHVSESSID={new_id}");
            let mut done = store.load(Some(&header));
            assert_eq!(done.values.get("user").map(String::as_str), Some("bob"), "{var}");
            done.destroyed = true;
            store.save(done);
            assert!(store.load(Some(&header)).id.is_none(), "{var}");
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
