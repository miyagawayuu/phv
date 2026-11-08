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
//! A save writes the whole session: of requests of one visitor that run at
//! once, the one that ends last wins, and what another set meanwhile is lost
//! (documented in the spec; a merge would need a per-key log in every store).
//!
//! A record's values are checked when it is read (a JSON object of strings)
//! but decoded only when the page first reads or writes them, or when the
//! record is written back (`Stored`); the memory store shares its map with
//! the page until the page writes to it. What happens to the record itself
//! (expiry, promotion, the `last` refresh, the save) does not wait for that.

use crate::metrics::{Health, StoreKind};
use indexmap::IndexMap;
use phv_std::safefs;
use phv_vm::{SessionData, SessionValues};
use std::collections::HashMap;
use std::ffi::OsStr;
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
    /// Shared with the requests that loaded it; a request that writes copies it.
    values: Arc<IndexMap<String, String>>,
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
    v.as_object()?.iter().map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()
}

/// Whether `text` would decode (`values_from_json`), without decoding it.
fn valid_values(text: &str) -> bool {
    serde_json::from_str::<phv_vm::SessionValuesShape>(text).is_ok()
}

/// The decoder a `SessionValues` is given: its text passed `valid_values`.
fn decode_values(text: &str) -> IndexMap<String, String> {
    values_from_json(text).unwrap_or_default()
}

/// A record's values: as the store keeps them (checked, not yet decoded), or
/// decoded. A record read from the store holds the text; one being written
/// holds the map. Writing the text back decodes and encodes it again, so the
/// store gets the same bytes it got before values were kept as text.
enum Stored {
    Text(String),
    Map(IndexMap<String, String>),
}

impl Stored {
    fn json(&self) -> String {
        match self {
            Stored::Text(text) => values_json(&decode_values(text)),
            Stored::Map(map) => values_json(map),
        }
    }

    fn into_values(self) -> SessionValues {
        match self {
            Stored::Text(text) => SessionValues::encoded(text, decode_values),
            Stored::Map(map) => SessionValues::from(map),
        }
    }
}

/// Sessions in a database of `[databases]`: Redis keeps them under
/// `phv:session:<hash>`, expires them itself (a session whose visitor has not
/// come back after an hour) and keeps within `MAX_SESSIONS` and `MAX_BYTES` as
/// sessions are written, with bookkeeping keys of the site's own
/// (`phv:sessions:<namespace>:`); PostgreSQL and MySQL in a table of the
/// site's own (`phv_sessions_<namespace>`), which the sweep keeps within the
/// same limits; a site without a namespace uses `phv:sessions:unnamed:` and
/// `phv_sessions_unnamed`. So the `session_ttl` and the limits of one site
/// never remove the sessions of a site with another namespace, nor those
/// every site kept in the shared store of 1.1. Both drop sessions whose
/// visitor never came back first.
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
        phv_std::SessionRow { data: r.values.json(), created: r.created, last: r.last, returned }
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
        if !valid_values(&row.data) {
            return None;
        }
        Some((Record { created: row.created, last: row.last, values: Stored::Text(row.data) }, row.returned))
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
    values: Stored,
}

impl Record {
    fn to_json(&self) -> String {
        format!("{{\"created\":{},\"last\":{},\"values\":{}}}", self.created, self.last, self.values.json())
    }

    /// The times are read and the values checked (see `Stored`); anything
    /// else in the file is ignored, as before.
    fn from_json(text: &str) -> Option<Record> {
        let top: HashMap<String, &serde_json::value::RawValue> = serde_json::from_str(text).ok()?;
        let num = |k: &str| serde_json::from_str::<u64>(top.get(k)?.get()).ok();
        let values = top.get("values")?.get();
        if !valid_values(values) {
            return None;
        }
        Some(Record { created: num("created")?, last: num("last")?, values: Stored::Text(values.to_string()) })
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

/// How many times a read of a session file is tried while a save holds the
/// directory lock (two yields, then a millisecond's sleep each: about 40 ms
/// at most, paid only when a read fails while a save is in progress).
const READ_RETRIES: usize = 40;

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
        Self::open_lock_file(&self.dir, &mut held)?.lock()?;
        Ok(DirLock(held))
    }

    /// `lock_dir` without waiting: None when another thread or process holds
    /// the lock (a save or the sweep is in progress).
    fn try_lock_dir(&self) -> std::io::Result<Option<DirLock<'_>>> {
        let mut held = match self.lock.try_lock() {
            Ok(held) => held,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        };
        match Self::open_lock_file(&self.dir, &mut held)?.try_lock() {
            Ok(()) => Ok(Some(DirLock(held))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }

    /// `.phv-session.lock` in `dir`, opened once.
    fn open_lock_file<'a>(dir: &Path, held: &'a mut Option<File>) -> std::io::Result<&'a File> {
        if held.is_none() {
            let fail = |e: safefs::FsError| std::io::Error::other(format!("{e:?}"));
            let (parent, name) = safefs::open_parent(dir, Path::new(".phv-session.lock")).map_err(fail)?;
            *held = Some(parent.open(&name, true).map_err(fail)?);
        }
        Ok(held.as_ref().expect("opened above"))
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

    /// Reads a session file. A save renames a temporary file over it (and a
    /// visitor's first request back renames `n-<hash>.json`): a reader that
    /// opens the file meanwhile can get the old one (on Windows already moved
    /// away for deletion, so its real name no longer matches, or still
    /// pending deletion), find the name absent for a moment, or land in the
    /// rename itself; under load (Windows Defender scans each new file) the
    /// moment stretches to milliseconds. So a read that fails is not trusted
    /// while a save may be in progress: every save holds the directory lock,
    /// so the read is tried again while the lock is held by someone, and once
    /// nobody holds it, read once more under the lock; that result is final
    /// (found by the 1.2.1 audit). A cookie of a session that is gone costs
    /// one failed open, a lock taken without waiting and one more failed
    /// open; reads that succeed never touch the lock (they would otherwise
    /// wait behind every save and the sweep).
    fn read_file(&self, name: &str) -> Option<Record> {
        let (dir, name) = safefs::open_parent(&self.dir, Path::new(name)).ok()?;
        for attempt in 0..READ_RETRIES {
            if let Ok(text) = Self::read_text(&dir, &name) {
                return Record::from_json(&text);
            }
            match self.try_lock_dir() {
                Ok(Some(_held)) => return Self::read_text(&dir, &name).ok().and_then(|t| Record::from_json(&t)),
                Ok(None) if attempt < 2 => std::thread::yield_now(),
                Ok(None) => std::thread::sleep(Duration::from_millis(1)),
                Err(_) => return None,
            }
        }
        None
    }

    fn read_text(dir: &safefs::DirGuard, name: &OsStr) -> Result<String, safefs::FsError> {
        let mut text = String::new();
        dir.open(name, false)?.take(1 << 20).read_to_string(&mut text)?;
        Ok(text)
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
            // A record whose times lie more than a day ahead of the clock is
            // damaged (something else wrote the store): refused and removed,
            // since nothing would ever expire it and the arithmetic below
            // must not overflow (found by the 1.2.1 audit). Less than a day
            // ahead (a clock set back) is accepted.
            let ahead = now.saturating_add(MAX_LIFETIME.as_secs());
            if r.last > ahead
                || r.created > ahead
                || now >= r.last.saturating_add(self.ttl.as_secs())
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
            if now >= r.last.saturating_add(60) {
                r.last = now;
                if let Ok(false) = files.update(&id, &r, returned) {
                    return SessionData::default();
                }
            }
            return SessionData { id: Some(id), values: r.values.into_values(), ..SessionData::default() };
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(id) = id {
            let alive = inner.map.get(&id).map(|e| self.alive(e));
            match alive {
                Some(true) => {
                    let e = inner.map.get_mut(&id).unwrap();
                    e.last = Instant::now();
                    e.returned = true;
                    let values = SessionValues::shared(e.values.clone());
                    return SessionData { id: Some(id), values, ..SessionData::default() };
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
        let values = s.values.into_shared();
        let bytes = size(&values);
        if inner.map.len() >= MAX_SESSIONS || inner.bytes + bytes > MAX_BYTES {
            self.evict(&mut inner, bytes);
        }
        let issue_cookie = s.id.as_deref() != Some(new_id.as_str());
        inner.bytes += bytes;
        inner.map.insert(new_id.clone(), Entry { values, created, last: Instant::now(), bytes, returned });
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
        let mut record = Record { created: now, last: now, values: Stored::Map(s.values.into_map()) };
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

    /// Requests of one visitor that read the session while others save it
    /// (the save renames a temporary file over the session file; on Windows
    /// the file a reader opened meanwhile is already the old one, moved away
    /// for deletion, or the name is absent for a moment) always see the live
    /// session, never none: the visitor is not signed out by a parallel
    /// request (found by the 1.2.1 audit). 8 writers saving 600 times each
    /// and 16 readers; it must hold on a loaded machine too.
    #[test]
    fn a_read_during_another_requests_save_sees_the_session() {
        use std::sync::atomic::AtomicBool;
        let dir = std::env::temp_dir().join(format!("phv-sess-readrace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(Duration::from_secs(600), false, Some(dir.clone()));
        let mut s = SessionData::default();
        s.values.insert("user".into(), "alice".into());
        s.changed = true;
        let header = format!("PHVSESSID={}", id_of(&store.save(s).unwrap()));
        // The cookie comes back once: the file is renamed to `<hash>.json`.
        assert!(store.load(Some(&header)).id.is_some());
        let writing = AtomicBool::new(true);
        let (saves_lost, reads, empty) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
        std::thread::scope(|scope| {
            let writers: Vec<_> = (0..8)
                .map(|w| {
                    let (store, header, saves_lost) = (&store, &header, &saves_lost);
                    scope.spawn(move || {
                        for i in 0..600 {
                            let mut s = store.load(Some(header));
                            if s.id.is_none() {
                                saves_lost.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            s.values.insert("seen".into(), format!("{w}-{i}"));
                            s.changed = true;
                            // The same id: no cookie.
                            assert!(store.save(s).is_none(), "writer {w} save {i}");
                        }
                    })
                })
                .collect();
            for _ in 0..16 {
                let (store, header, writing, reads, empty) = (&store, &header, &writing, &reads, &empty);
                scope.spawn(move || {
                    let mut n = 0;
                    while n < 600 || writing.load(Ordering::Relaxed) {
                        let s = store.load(Some(header));
                        n += 1;
                        reads.fetch_add(1, Ordering::Relaxed);
                        if s.id.is_none() {
                            empty.fetch_add(1, Ordering::Relaxed);
                        } else {
                            assert_eq!(s.values.get("user").map(String::as_str), Some("alice"));
                        }
                    }
                });
            }
            for w in writers {
                w.join().unwrap();
            }
            writing.store(false, Ordering::Relaxed);
        });
        let (reads, empty, saves_lost) =
            (reads.load(Ordering::Relaxed), empty.load(Ordering::Relaxed), saves_lost.load(Ordering::Relaxed));
        assert!(reads >= 9600, "{reads} reads");
        assert_eq!(empty, 0, "{empty} of {reads} reads saw no session");
        assert_eq!(saves_lost, 0, "{saves_lost} saves found no session");
        let s = store.load(Some(&header));
        assert!(s.values.get("seen").is_some_and(|v| v.ends_with("-599")), "{:?}", s.values.get("seen"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Writes raw bytes under a key in the Redis server of `url`
    /// (`redis://[[user]:password@]host[:port][/db]`), as damage would, with
    /// an expiry of a minute; or reads them back (`value` None).
    fn redis_raw(url: &str, key: &[u8], value: Option<&[u8]>) -> Option<Vec<u8>> {
        use std::io::BufRead;
        let rest = url.split_once("://").map_or(url, |(_, r)| r);
        let (auth, rest) = match rest.rsplit_once('@') {
            Some((a, r)) => (Some(a), r),
            None => (None, rest),
        };
        let (addr, db) = rest.split_once('/').unwrap_or((rest, "0"));
        let addr = if addr.contains(':') { addr.to_string() } else { format!("{addr}:6379") };
        let mut conn = std::net::TcpStream::connect(&addr).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut commands: Vec<Vec<&[u8]>> = Vec::new();
        if let Some(auth) = auth {
            let (user, pass) = auth.split_once(':').unwrap_or(("", auth));
            commands.push(if user.is_empty() {
                vec![b"AUTH", pass.as_bytes()]
            } else {
                vec![b"AUTH", user.as_bytes(), pass.as_bytes()]
            });
        }
        commands.push(vec![b"SELECT", db.as_bytes()]);
        match value {
            Some(v) => commands.push(vec![b"SET", key, v, b"PX", b"60000"]),
            None => commands.push(vec![b"GET", key]),
        }
        let mut out = Vec::new();
        for c in &commands {
            out.extend_from_slice(format!("*{}\r\n", c.len()).as_bytes());
            for a in c {
                out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
                out.extend_from_slice(a);
                out.extend_from_slice(b"\r\n");
            }
        }
        conn.write_all(&out).unwrap();
        let mut r = std::io::BufReader::new(conn);
        let mut result = None;
        for (i, _) in commands.iter().enumerate() {
            let mut line = Vec::new();
            r.read_until(b'\n', &mut line).unwrap();
            let last = i + 1 == commands.len();
            match line.first() {
                Some(b'+') => {}
                Some(b'$') => {
                    let n: i64 = std::str::from_utf8(&line[1..]).unwrap().trim().parse().unwrap();
                    if n >= 0 {
                        let mut buf = vec![0u8; n as usize + 2];
                        r.read_exact(&mut buf).unwrap();
                        buf.truncate(n as usize);
                        if last {
                            result = Some(buf);
                        }
                    }
                }
                _ => panic!("redis answered {}", String::from_utf8_lossy(&line)),
            }
        }
        result
    }

    /// A record whose `created` or `last` lies more than a day ahead of the
    /// clock is damaged: refused and removed in every store, instead of
    /// overflowing the refresh arithmetic (a task panic, 500 on every request
    /// of the visitor, the record never removed; found by the 1.2.1 audit). A
    /// time less than a day ahead (a clock set back) is still accepted.
    #[test]
    fn a_record_with_times_in_the_future_is_damaged() {
        let dir = std::env::temp_dir().join(format!("phv-sess-future-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(Duration::from_secs(600), false, Some(dir.clone()));
        let now = now_secs();
        let day = MAX_LIFETIME.as_secs();
        let file = |id: &str, created: u64, last: u64| {
            let path = dir.join(FileStore::name(id, true));
            std::fs::write(&path, format!("{{\"created\":{created},\"last\":{last},\"values\":{{\"a\":\"1\"}}}}"))
                .unwrap();
            path
        };
        for (created, last) in [(now, u64::MAX), (now, now + day + 60), (u64::MAX, now), (now + day + 60, now)] {
            let id = phv_std::random_hex(32);
            let path = file(&id, created, last);
            let s = store.load(Some(&format!("PHVSESSID={id}")));
            assert!(s.id.is_none() && s.values.is_empty(), "{created} {last}");
            assert!(!path.exists(), "{created} {last}: the damaged record was kept");
            // What the page sets starts a new session.
            let mut s = store.load(Some(&format!("PHVSESSID={id}")));
            s.values.insert("x".into(), "1".into());
            s.changed = true;
            assert_ne!(id_of(&store.save(s).unwrap()), id);
        }
        for (created, last) in [(now, now + 3600), (now - 60, now + day - 60)] {
            let id = phv_std::random_hex(32);
            file(&id, created, last);
            let s = store.load(Some(&format!("PHVSESSID={id}")));
            assert_eq!(s.values.get("a").map(String::as_str), Some("1"), "{created} {last}");
        }
        let _ = std::fs::remove_dir_all(&dir);

        let _one_at_a_time = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        for var in ["PHV_TEST_REDIS_URL", "PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
            let Some(url) = std::env::var(var).ok().filter(|u| !u.is_empty()) else {
                eprintln!("skipped: {var} is not set");
                continue;
            };
            let spec = phv_vm::DatabaseSpec { url: Some(url), url_env: None, tls: None, pool_size: 2 };
            let ns = phv_std::testing::namespace(0x47);
            let store =
                Store::with_database(Duration::from_secs(600), false, spec.clone(), Arc::default(), Some(ns.clone()));
            let limits = phv_std::SessionLimits {
                ttl: Duration::from_secs(600),
                max_lifetime: MAX_LIFETIME,
                max_sessions: MAX_SESSIONS,
                max_bytes: MAX_BYTES,
            };
            // PostgreSQL and MySQL keep the times as signed 64-bit integers:
            // u64::MAX comes back as 0 there (an expired record, also removed).
            for (created, last) in [(now, u64::MAX), (now, now + day + 60), (now + day + 60, now)] {
                let id = phv_std::random_hex(32);
                let key = shared_session_key(Some(&ns), &id);
                let row = phv_std::SessionRow { data: r#"{"a":"1"}"#.into(), created, last, returned: true };
                assert!(phv_std::session_insert(&spec, Some(&ns), &key, &row, &limits).unwrap(), "{var}");
                let s = store.load(Some(&format!("PHVSESSID={id}")));
                assert!(s.id.is_none() && s.values.is_empty(), "{var}: {created} {last}");
                assert!(
                    phv_std::session_read(&spec, Some(&ns), &key).unwrap().is_none(),
                    "{var}: {created} {last}: the damaged record was kept"
                );
            }
            let id = phv_std::random_hex(32);
            let key = shared_session_key(Some(&ns), &id);
            let row =
                phv_std::SessionRow { data: r#"{"a":"1"}"#.into(), created: now, last: now + 3600, returned: true };
            assert!(phv_std::session_insert(&spec, Some(&ns), &key, &row, &limits).unwrap(), "{var}");
            let mut s = store.load(Some(&format!("PHVSESSID={id}")));
            assert_eq!(s.values.get("a").map(String::as_str), Some("1"), "{var}");
            s.destroyed = true;
            store.save(s);
        }
    }

    /// The Redis store refuses a record that is not UTF-8, as the file store
    /// does, instead of reading it with replacement characters (found by the
    /// 1.2.1 audit). Skipped without `PHV_TEST_REDIS_URL`.
    #[test]
    fn a_redis_record_that_is_not_utf8_is_refused() {
        let Some(url) = std::env::var("PHV_TEST_REDIS_URL").ok().filter(|u| !u.is_empty()) else {
            eprintln!("skipped: PHV_TEST_REDIS_URL is not set");
            return;
        };
        let _one_at_a_time = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let spec = phv_vm::DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 2 };
        let ns = phv_std::testing::namespace(0x48);
        let store =
            Store::with_database(Duration::from_secs(600), false, spec.clone(), Arc::default(), Some(ns.clone()));
        let now = now_secs();
        let id = phv_std::random_hex(32);
        let key = format!("phv:session:{}", shared_session_key(Some(&ns), &id));
        let mut raw = format!("{now} {now} 1\n{{\"a\":\"").into_bytes();
        raw.extend_from_slice(b"\xff\xfe\"}");
        redis_raw(&url, key.as_bytes(), Some(&raw));
        let s = store.load(Some(&format!("PHVSESSID={id}")));
        assert!(s.id.is_none() && s.values.is_empty(), "a record that is not UTF-8 was read");
        assert_eq!(redis_raw(&url, key.as_bytes(), None).as_deref(), Some(&raw[..]), "the record was touched");
        // The same bytes in the header.
        let mut raw = format!("{now} ").into_bytes();
        raw.extend_from_slice(b"\xff 1\n{}");
        redis_raw(&url, key.as_bytes(), Some(&raw));
        assert!(store.load(Some(&format!("PHVSESSID={id}"))).id.is_none());
        // A record that is UTF-8 is read as before.
        redis_raw(&url, key.as_bytes(), Some(format!("{now} {now} 1\n{{\"a\":\"\u{3042}\"}}").as_bytes()));
        assert_eq!(store.load(Some(&format!("PHVSESSID={id}"))).values.get("a").map(String::as_str), Some("\u{3042}"));
        redis_raw(&url, key.as_bytes(), Some(b"0 0 0\n{}"));
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
            // Namespaces of this test run: other test processes use the servers too.
            let ns = |tag: u8| Some(phv_std::testing::namespace(tag));
            let store = Store::with_database(Duration::from_secs(60), false, spec.clone(), Arc::default(), ns(0x40));
            let mut s = SessionData::default();
            s.values.insert("user".into(), "alice".into());
            s.changed = true;
            let cookie = store.save(s).unwrap();
            let header = format!("PHVSESSID={}", cookie.split(['=', ';']).nth(1).unwrap());
            assert_eq!(store.load(Some(&header)).values.get("user").map(String::as_str), Some("alice"), "{var}");
            // Another site on the same database does not take the cookie
            // (found by the 1.1 audit).
            let other = Store::with_database(Duration::from_secs(60), false, spec.clone(), Arc::default(), ns(0x41));
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
            let site = ns(0x42);
            let ns = site.as_deref();
            // The session keys of this test run (a session's key is the same
            // in every site's bookkeeping; the server derives it from the id).
            let key = |k: &str| format!("{}-{k}", phv_std::testing::run_id());
            let left_of = |ns: Option<&str>, keys: &[&'static str]| -> Vec<&'static str> {
                keys.iter().copied().filter(|k| phv_std::session_read(&spec, ns, &key(k)).unwrap().is_some()).collect()
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
                    let _ = phv_std::session_remove(&spec, ns, &key(k));
                    assert!(phv_std::session_insert(&spec, ns, &key(k), &row(now, i < 2), &limits).unwrap());
                }
                let new_2 = key("cap-new-2");
                assert!(!phv_std::session_insert(&spec, ns, &new_2, &row(now, false), &limits).unwrap());
                assert_eq!(left_of(ns, &keys), ["cap-back-1", "cap-back-2", "cap-new-2"]);
                // A byte limit evicts the others, returning visitors' too.
                let tiny = phv_std::SessionLimits { max_bytes: 1, ..limits };
                assert!(phv_std::session_update(&spec, ns, &new_2, &row(now, true), &tiny).unwrap());
                assert_eq!(left_of(ns, &keys), ["cap-new-2"]);
                assert!(phv_std::session_remove(&spec, ns, &new_2).unwrap());
                assert!(!phv_std::session_update(&spec, ns, &new_2, &row(now, true), &limits).unwrap());
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
                    let _ = phv_std::session_remove(&spec, ns, &key(k));
                    assert!(phv_std::session_insert(&spec, ns, &key(k), r, &limits).unwrap());
                }
                phv_std::session_sweep(&spec, ns, &limits).unwrap();
                // 90% of 3 is 2: the new ones go first.
                assert_eq!(left_of(ns, &keys), ["sweep-back-1", "sweep-back-2"], "{var}");
                for k in keys {
                    phv_std::session_remove(&spec, ns, &key(k)).unwrap();
                }
            }

            // Two sites on the same database, one with a short session_ttl and
            // room for two sessions: what it writes and sweeps never removes
            // the other site's sessions (found by the 1.1.1 audit: the shortest
            // ttl and the limits applied to every site).
            let (long_site, short_site) = (phv_std::testing::namespace(0x43), phv_std::testing::namespace(0x44));
            let (long_ns, short_ns) = (Some(long_site.as_str()), Some(short_site.as_str()));
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
                let _ = phv_std::session_remove(&spec, long_ns, &key(k));
            }
            for k in mine {
                let _ = phv_std::session_remove(&spec, short_ns, &key(k));
            }
            // Idle for two minutes: past the short ttl, well within the long one.
            let idle = if var == "PHV_TEST_REDIS_URL" { now } else { now - 120 };
            assert!(phv_std::session_insert(&spec, long_ns, &key(theirs[0]), &row(idle, true), &long).unwrap());
            assert!(phv_std::session_insert(&spec, long_ns, &key(theirs[1]), &row(idle, false), &long).unwrap());
            if var == "PHV_TEST_REDIS_URL" {
                // Redis takes the time of the last use from its own writes:
                // wait past the short ttl (its times are in seconds).
                std::thread::sleep(Duration::from_millis(2100));
            }
            for k in mine {
                assert!(phv_std::session_insert(&spec, short_ns, &key(k), &row(now_secs(), false), &short).unwrap());
            }
            phv_std::session_sweep(&spec, short_ns, &short).unwrap();
            assert_eq!(left_of(long_ns, &theirs), theirs, "{var}: the other site's sessions were removed");
            let kept = left_of(short_ns, &mine);
            assert!(kept.len() <= 2, "{var}: {kept:?}");
            for k in theirs {
                phv_std::session_remove(&spec, long_ns, &key(k)).unwrap();
            }
            for k in mine {
                phv_std::session_remove(&spec, short_ns, &key(k)).unwrap();
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
        // The store every site shared in 1.1 is one for every test process.
        let _unnamed = phv_std::testing::unnamed();
        for var in ["PHV_TEST_REDIS_URL", "PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
            let Some(url) = std::env::var(var).ok().filter(|u| !u.is_empty()) else {
                eprintln!("skipped: {var} is not set");
                continue;
            };
            let spec = phv_vm::DatabaseSpec { url: Some(url), url_env: None, tls: None, pool_size: 2 };
            let ns = phv_std::testing::namespace(0x45);
            let limits = phv_std::SessionLimits {
                ttl: Duration::from_secs(3600),
                max_lifetime: MAX_LIFETIME,
                max_sessions: MAX_SESSIONS,
                max_bytes: MAX_BYTES,
            };
            // Written as 1.1 wrote them: the site's key, in the store every
            // site shared then.
            let old = Some(phv_std::SESSIONS_OF_1_1);
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
                assert!(phv_std::session_insert(&spec, old, &key, &row, &limits).unwrap(), "{var}");
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
                assert!(phv_std::session_insert(&spec, old, "legacy-writer", &other, &brief).unwrap());
                phv_std::session_remove(&spec, old, "legacy-writer").unwrap();
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
            assert!(phv_std::session_read(&spec, old, &shared_session_key(Some(&ns), id)).unwrap().is_none());

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
                assert!(phv_std::session_read(&spec, old, &key).unwrap().is_none(), "{var}: in the shared table");
            }
            let header = format!("PHVSESSID={new_id}");
            let mut done = store.load(Some(&header));
            assert_eq!(done.values.get("user").map(String::as_str), Some("bob"), "{var}");
            done.destroyed = true;
            store.save(done);
            assert!(store.load(Some(&header)).id.is_none(), "{var}");
        }
    }

    fn id_of(cookie: &str) -> String {
        cookie.split(['=', ';']).nth(1).unwrap().to_string()
    }

    /// What a file or database store holds for a session: its values as the
    /// store keeps them (not decoded by this), its `created` and `last` times.
    fn stored(store: &Store, id: &str) -> Option<(String, u64, u64)> {
        let (r, _) = store.files.as_ref().unwrap().read(id)?;
        match r.values {
            Stored::Text(text) => Some((text, r.created, r.last)),
            Stored::Map(_) => panic!("a record read from the store holds its text"),
        }
    }

    /// Stores a session whose values are the given text (which does not
    /// decode) as the store keeps them; gives its cookie header and a way to
    /// read the raw record back.
    type Damaged<'a> = dyn Fn(&str) -> (String, Box<dyn Fn() -> Option<String>>) + 'a;

    /// The values of a session are decoded at the page's first read, and not
    /// at all by a page that never reads them; the record is still promoted,
    /// refreshed and written as before. `dir` is a file store's directory.
    fn lazy_values_hold(store: &Store, name: &str, dir: Option<&Path>, damaged: &Damaged<'_>) {
        let read_file =
            |id: &str, returned: bool| std::fs::read_to_string(dir.unwrap().join(FileStore::name(id, returned)));
        // Set in an order that is not the sorted one: stores write the values sorted, as before.
        let mut s = SessionData::default();
        s.values.insert("zebra".into(), "1".into());
        s.values.insert("user".into(), "ann".into());
        s.changed = true;
        let id = id_of(&store.save(s).unwrap());
        let header = format!("PHVSESSID={id}");
        let (text0, created, last0) = stored(store, &id).unwrap();
        assert_eq!(text0, r#"{"user":"ann","zebra":"1"}"#, "{name}");
        let file0 = dir.map(|_| read_file(&id, false).unwrap());

        // (a) A page that neither reads nor writes the session: nothing is
        // decoded, and the record is left as it was (promoted: the same bytes
        // under the returned name).
        let loaded = store.load(Some(&header));
        assert!(loaded.id.is_some() && !loaded.values.is_decoded(), "{name}");
        assert!(store.save(loaded).is_none(), "{name}");
        assert_eq!(stored(store, &id).unwrap(), (text0.clone(), created, last0), "{name}: the record changed");
        if dir.is_some() {
            assert_eq!(read_file(&id, true).unwrap(), file0.unwrap(), "{name}: promoted with other bytes");
        }
        // With the refresh of `last` due: only `last` changes.
        let old = Record { created, last: last0 - 120, values: Stored::Text(text0.clone()) };
        assert!(store.files.as_ref().unwrap().update(&id, &old, true).unwrap(), "{name}");
        let loaded = store.load(Some(&header));
        assert!(!loaded.values.is_decoded(), "{name}");
        assert!(store.save(loaded).is_none(), "{name}");
        let (text1, created1, last1) = stored(store, &id).unwrap();
        assert!(text1 == text0 && created1 == created && last1 >= last0, "{name}: {text1} {created1} {last1}");
        if dir.is_some() {
            let expected = format!("{{\"created\":{created},\"last\":{last1},\"values\":{text0}}}");
            assert_eq!(read_file(&id, true).unwrap(), expected, "{name}: more than `last` changed");
        }

        // (c) Decoded at the first read; what the page writes comes back.
        let mut s = store.load(Some(&header));
        assert!(!s.values.is_decoded());
        assert_eq!(s.values.get("user").map(String::as_str), Some("ann"), "{name}");
        assert!(s.values.is_decoded());
        assert_eq!(s.values.get("zebra").map(String::as_str), Some("1"), "{name}");
        assert_eq!(s.values.get("seen"), None);
        s.values.insert("seen".into(), "2".into());
        s.changed = true;
        assert!(store.save(s).is_none(), "{name}: the same id");
        let s = store.load(Some(&header));
        assert_eq!(s.values.get("seen").map(String::as_str), Some("2"), "{name}");
        assert_eq!(s.values.len(), 3, "{name}");
        let text2 = r#"{"seen":"2","user":"ann","zebra":"1"}"#;
        assert_eq!(stored(store, &id).unwrap().0, text2, "{name}");
        // A page that regenerates without reading moves the values whole.
        let mut s = store.load(Some(&header));
        s.regenerate = true;
        s.changed = true;
        let id2 = id_of(&store.save(s).unwrap());
        assert!(store.load(Some(&header)).id.is_none(), "{name}");
        let header2 = format!("PHVSESSID={id2}");
        assert_eq!(stored(store, &id2).unwrap().0, text2, "{name}");
        let mut s = store.load(Some(&header2));
        assert_eq!(s.values.get("user").map(String::as_str), Some("ann"), "{name}");
        s.destroyed = true;
        store.save(s);

        // (b) A record whose values do not decode is refused when it is read,
        // as before: the page has no session, the record is left alone, and
        // what the page sets starts a new session under a new id.
        for text in [r#"{"a":1}"#, "[]", r#"{"a":"b""#, r#"{"a":null}"#, "\"x\""] {
            let (header, raw) = damaged(text);
            let before = raw();
            assert!(before.is_some(), "{name}: {text}");
            let s = store.load(Some(&header));
            assert!(s.id.is_none() && s.values.is_empty(), "{name}: {text}: read");
            assert_eq!(raw(), before, "{name}: {text}: the record was touched");
            let mut s = store.load(Some(&header));
            s.values.insert("x".into(), "1".into());
            s.changed = true;
            let cookie = store.save(s).unwrap();
            assert_ne!(id_of(&cookie), id_of(&header), "{name}: {text}: the damaged record was taken over");
            assert_eq!(raw(), before, "{name}: {text}: the record was touched by the save");
            let mut s = store.load(Some(&format!("PHVSESSID={}", id_of(&cookie))));
            assert_eq!(s.values.get("x").map(String::as_str), Some("1"), "{name}: {text}");
            s.destroyed = true;
            store.save(s);
        }
    }

    #[test]
    fn lazy_values_hold_in_the_file_store() {
        let dir = std::env::temp_dir().join(format!("phv-sess-lazy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(Duration::from_secs(600), false, Some(dir.clone()));
        let damaged = |text: &str| {
            let id = phv_std::random_hex(32);
            let path = dir.join(FileStore::name(&id, true));
            let now = now_secs();
            std::fs::write(&path, format!("{{\"created\":{now},\"last\":{now},\"values\":{text}}}")).unwrap();
            let new = dir.join(FileStore::name(&id, false));
            let raw = move || {
                assert!(!new.exists(), "a damaged record was renamed");
                std::fs::read_to_string(&path).ok()
            };
            (format!("PHVSESSID={id}"), Box::new(raw) as Box<dyn Fn() -> Option<String>>)
        };
        lazy_values_hold(&store, "files", Some(&dir), &damaged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same against the database stores (skipped without the servers).
    #[test]
    fn lazy_values_hold_in_the_database_stores() {
        let _one_at_a_time = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        for var in ["PHV_TEST_REDIS_URL", "PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL"] {
            let Some(url) = std::env::var(var).ok().filter(|u| !u.is_empty()) else {
                eprintln!("skipped: {var} is not set");
                continue;
            };
            let spec = phv_vm::DatabaseSpec { url: Some(url), url_env: None, tls: None, pool_size: 2 };
            let ns = phv_std::testing::namespace(0x46);
            let store =
                Store::with_database(Duration::from_secs(600), false, spec.clone(), Arc::default(), Some(ns.clone()));
            let limits = phv_std::SessionLimits {
                ttl: Duration::from_secs(600),
                max_lifetime: MAX_LIFETIME,
                max_sessions: MAX_SESSIONS,
                max_bytes: MAX_BYTES,
            };
            let damaged = |text: &str| {
                let id = phv_std::random_hex(32);
                let key = shared_session_key(Some(&ns), &id);
                let now = now_secs();
                let row = phv_std::SessionRow { data: text.to_string(), created: now, last: now, returned: true };
                assert!(phv_std::session_insert(&spec, Some(&ns), &key, &row, &limits).unwrap(), "{var}");
                let (spec, ns) = (spec.clone(), ns.clone());
                let raw = move || {
                    let row = phv_std::session_read(&spec, Some(&ns), &key).unwrap()?;
                    Some(format!("{} {} {} {}", row.data, row.created, row.last, row.returned))
                };
                (format!("PHVSESSID={id}"), Box::new(raw) as Box<dyn Fn() -> Option<String>>)
            };
            lazy_values_hold(&store, var, None, &damaged);
        }
    }

    /// The memory store shares its map with the requests that load it, and a
    /// request that writes gets a copy of its own.
    #[test]
    fn the_memory_store_shares_values_until_written() {
        let store = Store::new(Duration::from_secs(600), false, None);
        let mut s = SessionData::default();
        s.values.insert("user".into(), "ann".into());
        s.changed = true;
        let id = id_of(&store.save(s).unwrap());
        let header = format!("PHVSESSID={id}");
        let kept = store.inner.lock().unwrap().map[&id].values.clone();
        // A page that never reads: the same map, and nothing written.
        let loaded = store.load(Some(&header));
        assert!(loaded.values.is_decoded(), "a shared map needs no decoding");
        assert!(Arc::ptr_eq(&loaded.values.into_shared(), &kept));
        assert!(store.save(store.load(Some(&header))).is_none());
        // A page that reads does not copy; one that writes copies, and the
        // other request keeps seeing the old values until the writer saves.
        let (reader, mut writer) = (store.load(Some(&header)), store.load(Some(&header)));
        assert_eq!(reader.values.get("user").map(String::as_str), Some("ann"));
        assert!(Arc::ptr_eq(&reader.values.into_shared(), &kept));
        writer.values.insert("user".into(), "bob".into());
        writer.changed = true;
        assert_eq!(kept.get("user").map(String::as_str), Some("ann"));
        assert_eq!(store.load(Some(&header)).values.get("user").map(String::as_str), Some("ann"));
        assert!(store.save(writer).is_none());
        assert_eq!(store.load(Some(&header)).values.get("user").map(String::as_str), Some("bob"));
        assert_eq!(kept.get("user").map(String::as_str), Some("ann"));
    }

    /// A record is accepted and refused as before it was kept as text, and
    /// decodes to the same values.
    #[test]
    fn records_are_checked_as_before() {
        let ok = |text: &str| Record::from_json(text).map(|r| (r.created, r.last, r.values.into_values().into_map()));
        let values = |pairs: &[(&str, &str)]| -> IndexMap<String, String> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        // As written, with spaces, with other fields, with escapes, with a repeated key (the last wins).
        let (c, l, v) = ok(r#"{"created":1,"last":2,"values":{"b":"2","a":"1"}}"#).unwrap();
        assert_eq!((c, l), (1, 2));
        assert_eq!(v, values(&[("a", "1"), ("b", "2")]), "sorted, as the store writes them");
        assert_eq!(ok(r#" { "values" : { } , "created" : 1 , "last" : 2 , "x" : [1] } "#).unwrap().2, values(&[]));
        assert_eq!(
            ok(r#"{"created":1,"last":2,"values":{"ké":"v\n\"","a":"x","a":"y"}}"#).unwrap().2,
            values(&[("a", "y"), ("ké", "v\n\"")])
        );
        for bad in [
            r#"{"created":1,"last":2,"values":{"a":1}}"#,
            r#"{"created":1,"last":2,"values":{"a":null}}"#,
            r#"{"created":1,"last":2,"values":{"a":{"b":"c"}}}"#,
            r#"{"created":1,"last":2,"values":[]}"#,
            r#"{"created":1,"last":2,"values":"x"}"#,
            r#"{"created":1,"last":2}"#,
            r#"{"created":1.0,"last":2,"values":{}}"#,
            r#"{"created":-1,"last":2,"values":{}}"#,
            r#"{"created":"1","last":2,"values":{}}"#,
            r#"{"created":1,"values":{}}"#,
            r#"[{"created":1,"last":2,"values":{}}]"#,
            r#"{"created":1,"last":2,"values":{"a":"b"}"#,
            "",
        ] {
            assert!(ok(bad).is_none(), "{bad}");
        }
        // The text is kept until read, and written back as before.
        let r = Record::from_json(r#"{"created":1,"last":2,"values":{"b":"2","a":"1"}}"#).unwrap();
        assert!(matches!(&r.values, Stored::Text(t) if t == r#"{"b":"2","a":"1"}"#));
        assert_eq!(r.to_json(), r#"{"created":1,"last":2,"values":{"a":"1","b":"2"}}"#);
        let v = r.values.into_values();
        assert!(!v.is_decoded());
        assert_eq!(v.get("a").map(String::as_str), Some("1"));
        assert!(v.is_decoded());
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
