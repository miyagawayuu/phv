//! SQLite access. Queries are `Sql` values (`sql"..."` literals), which keep
//! interpolated values as bound parameters, so SQL injection cannot happen.
//! At runtime an `Sql` value is the list `[query text, list of parameters]`.

use crate::safefs::{DirGuard, FileId};
use crate::{int, s};
use indexmap::IndexMap;
use phv_vm::{BuiltinFn, Key, Value, Vm, VmError};
use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

mod cost;
mod mem;
mod vfs;

type R = Result<Value, VmError>;

/// SQLite databases a script may have open at once (each holds files and memory).
const MAX_OPEN: usize = 64;

/// Marks a bound parameter in `Sql` text (see the type checker).
pub(crate) const PARAM: char = '\u{E000}';
/// Start and end of an identifier from `sql_ident()`.
pub(crate) const IDENT_START: char = '\u{E001}';
pub(crate) const IDENT_END: char = '\u{E002}';

/// How a database writes placeholders and quoted identifiers.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Dialect {
    /// `?` and `"name"`.
    Sqlite,
    /// `$1, $2, ...` and `"name"`.
    Postgres,
    /// `?` and `` `name` ``.
    MySql,
}

/// The SQL text a database receives for an `Sql` value.
pub(crate) fn render_sql(text: &str, dialect: Dialect) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut n = 0;
    for c in text.chars() {
        match c {
            PARAM => {
                n += 1;
                match dialect {
                    Dialect::Postgres => {
                        out.push('$');
                        out.push_str(&n.to_string());
                    }
                    _ => out.push('?'),
                }
            }
            IDENT_START | IDENT_END => out.push(if matches!(dialect, Dialect::MySql) { '`' } else { '"' }),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn parts(v: &Value) -> (&str, &[Value]) {
    let Value::List(l) = v else { panic!("Sql value expected") };
    let Value::List(params) = &l[1] else { panic!("Sql parameters expected") };
    (s(&l[0]), params)
}

fn to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Int(i) => SqlValue::Integer(*i),
        Value::Float(f) => SqlValue::Real(*f),
        other => SqlValue::Text(other.to_display_string()),
    }
}

/// An open database. The directory guard keeps the database's directory
/// pinned (see `safefs`) while SQLite uses files in it by name.
struct Db {
    /// Always there while the script holds it (taken when it is dropped).
    conn: Option<Connection>,
    _dir: Option<crate::safefs::DirGuard>,
    /// How many `transaction()` calls are open (inner ones are savepoints).
    depth: std::cell::Cell<u32>,
    /// Why SQLite ended the transaction of the open `transaction()` calls
    /// before they did (see `Ended`).
    ended: std::cell::Cell<Option<Ended>>,
    /// The pages an in-memory database may use (0 for a file).
    pages: usize,
    /// For a database file: the pool the connection goes back to.
    pooled: Option<Lease>,
    /// What the connection's authorizer and functions know of this script.
    session: Arc<Session>,
}

impl Db {
    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("an open database has its connection")
    }

    /// The script's statement `text`, prepared (or kept from before), and
    /// weighed when it was compiled (`cost`): one that could read long values
    /// too many times between two looks at the deadline is refused.
    ///
    /// Statements are compiled with up to `PREPARE_OPS` steps and weighed;
    /// while one runs, SQLite may compile only programs of `RUN_OPS` steps: a
    /// statement whose schema changed meanwhile (another connection changed a
    /// view it reads, say) is compiled again by `sqlite3_step` without being
    /// weighed, so it may not be large (`query` and `execute` then prepare and
    /// weigh it again).
    ///
    /// A kept connection keeps its statements between scripts (`reusable`),
    /// and the weighing holds across them as within one script: a statement
    /// comes out of the cache only if SQLite compiled it on this connection
    /// (`prepare_cached` compiles a text it does not hold), and it was weighed
    /// then, whichever script ran it. When the schema changed since, by
    /// another connection or by this one (a `CREATE` or `ALTER` of this or an
    /// earlier script; those do not taint the connection), the statement's
    /// `OP_Transaction` step finds the schema cookie changed, or the statement
    /// expired, and `sqlite3_step` compiles it again under `RUN_OPS`: a larger
    /// program fails as out of memory, `refused_recompile` tells, and the text
    /// is prepared and weighed again here, after `discard` dropped the kept
    /// one. So no program that was never weighed runs from the cache with more
    /// than `RUN_OPS` steps. A statement that reads no table carries no schema
    /// and is not compiled again, and need not be. The memory the kept
    /// statements take moves with the connection (`held`), and the values a
    /// run bound are cleared when the statement goes back to the cache (see
    /// `reusable`). A program of more than `KEPT_STATEMENT_BYTES` runs but is
    /// not kept (`Prepared::Once`).
    ///
    /// A weight is kept per process by file, schema and text (`weight`), so
    /// the other connections of the file do not run `EXPLAIN` for a text one
    /// of them weighed; each still checks the weight against `MAX_WEIGHT`.
    fn statement(&self, vm: &Vm, text: &str) -> Result<Prepared<'_>, VmError> {
        use rusqlite::limits::Limit;
        let c = self.conn();
        let _ = c.set_limit(Limit::SQLITE_LIMIT_VDBE_OP, PREPARE_OPS);
        let before = self.session.compiles.load(Ordering::SeqCst);
        let prepared = c.prepare_cached(text).and_then(|st| {
            if self.session.compiles.load(Ordering::SeqCst) == before {
                // Kept from an earlier run, and weighed then (see above).
                return Ok(Ok(Prepared::Kept(st)));
            }
            COMPILED.fetch_add(1, Ordering::SeqCst);
            // Not kept unless it passes: the next prepare of the text compiles
            // (and weighs) it again.
            if st.is_explain() == 0 {
                match self.weight(c, text) {
                    Ok(weight) if weight <= cost::MAX_WEIGHT => {}
                    Ok(weight) => {
                        st.discard();
                        return Ok(Err(weight));
                    }
                    Err(e) => {
                        st.discard();
                        return Err(e);
                    }
                }
            }
            if statement_bytes(&st) <= KEPT_STATEMENT_BYTES {
                return Ok(Ok(Prepared::Kept(st)));
            }
            // Too large to keep: compiled once more, outside the cache (the
            // weighing holds, the text is the same).
            st.discard();
            c.prepare(text).map(|st| Ok(Prepared::Once(st)))
        });
        let _ = c.set_limit(Limit::SQLITE_LIMIT_VDBE_OP, RUN_OPS);
        let prepared = prepared.map_err(|e| {
            // SQLite reports a program over `PREPARE_OPS` as out of memory.
            let too_long = matches!(&e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::OutOfMemory)
                && !mem::Budget::refused()
                && !mem::Budget::late();
            if too_long {
                let msg = "this statement has more than 43,008 steps (out of memory): split it into smaller statements";
                return vm.throw("DatabaseError", msg);
            }
            self.fail(vm, e)
        })?;
        match prepared {
            Ok(st) => Ok(st),
            Err(weight) => Err(vm.throw(
                "DatabaseError",
                format!(
                    "this statement could read long values {weight} times over without looking at the time limit (at most {}): split it into smaller statements",
                    cost::MAX_WEIGHT
                ),
            )),
        }
    }

    /// The weight of `text`, which SQLite compiled just now on `c`: the one
    /// kept per process for this file at its schema (`cost::cached`), or else
    /// weighed now (`cost::weigh`) and kept. The schema is the cookie in the
    /// file's header (`schema_cookie`; every schema change moves it), read
    /// before the program is listed: a weight is kept only when the program
    /// was compiled against that very cookie, so a schema change in between
    /// keeps nothing. A `:memory:` database has no other connections, and a
    /// connection in a transaction may see a schema of its own: not kept.
    fn weight(&self, c: &Connection, text: &str) -> rusqlite::Result<u32> {
        let Some(file) = self.session.file else { return cost::weigh(c, text).map(|(w, _)| w) };
        let Some(schema) = self.schema_cookie(c) else { return cost::weigh(c, text).map(|(w, _)| w) };
        let analyzed = cost::analyzed();
        if let Some(weight) = cost::cached(file, schema, analyzed, text) {
            return Ok(weight);
        }
        let (weight, compiled) = cost::weigh(c, text)?;
        match compiled {
            cost::Schema::None => cost::remember(file, schema, analyzed, text, weight),
            cost::Schema::Main(cookie) if cookie == schema => cost::remember(file, schema, analyzed, text, weight),
            _ => {}
        }
        Ok(weight)
    }

    /// The schema cookie of the database file (bytes 40–43 of its header,
    /// what `PRAGMA schema_version` reads): read through SQLite's open file,
    /// without a transaction (the pragma would take a read lock, which costs
    /// as much as weighing a short statement). The file holds the committed
    /// schema, so this is nothing when the connection is in a transaction (it
    /// may have changed the schema itself), or when the file is in WAL mode
    /// (the log may hold a newer schema); a value read while another
    /// process writes the header is not the cookie SQLite compiles against,
    /// so nothing is kept for it (see `weight`). An empty file is at 0.
    fn schema_cookie(&self, c: &Connection) -> Option<i64> {
        use rusqlite::ffi;
        if !c.is_autocommit() {
            return None;
        }
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        // SAFETY: the handle is the open connection's; the file control
        // writes the main database's file pointer, owned by the connection.
        let rc = unsafe {
            ffi::sqlite3_file_control(
                c.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_FILE_POINTER,
                (&mut file as *mut *mut ffi::sqlite3_file).cast(),
            )
        };
        if rc != ffi::SQLITE_OK || file.is_null() {
            return None;
        }
        let mut header = [0u8; 44];
        // SAFETY: `file` is the open file; `xRead` is its method, and the
        // buffer is as long as asked (a short read zero-fills the rest).
        let rc = unsafe {
            let methods = (*file).pMethods;
            if methods.is_null() {
                return None;
            }
            let read = (*methods).xRead?;
            read(file, header.as_mut_ptr().cast(), header.len() as std::ffi::c_int, 0)
        };
        if rc != ffi::SQLITE_OK && rc != ffi::SQLITE_IOERR_SHORT_READ {
            return None;
        }
        // Bytes 18 and 19 are the file format versions: 2 means WAL.
        if header[18] == 2 || header[19] == 2 {
            return None;
        }
        // SQLite holds the cookie as an `int`.
        Some(i64::from(u32::from_be_bytes([header[40], header[41], header[42], header[43]]) as i32))
    }

    /// Whether `e`, from running a statement during which SQLite compiled
    /// something (the authorizer count moved from `before`), is SQLite
    /// refusing to compile it again with more than `RUN_OPS` steps.
    fn refused_recompile(&self, e: &rusqlite::Error, before: u64) -> bool {
        matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::OutOfMemory)
            && !mem::Budget::refused()
            && !mem::Budget::late()
            && self.session.compiles.load(Ordering::SeqCst) != before
    }

    /// Counts a run of a statement during which SQLite compiled something
    /// (the authorizer count moved from `before`): the statement itself,
    /// again, after a schema change. For tests.
    fn ran(&self, before: u64) {
        if self.session.compiles.load(Ordering::SeqCst) != before {
            COMPILED.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The error of a failed statement; the connection is then not reused.
    fn fail(&self, vm: &Vm, e: rusqlite::Error) -> VmError {
        self.session.tainted.store(true, Ordering::SeqCst);
        db_error(vm, e)
    }
}

/// The most memory a kept statement takes (`SQLITE_STMTSTATUS_MEMUSED`: its
/// program, with the texts in it). A larger one runs but is not kept, so the
/// statements a connection keeps hold at most 32 times this (found by the
/// 1.2.0 audit: the kept statements were bounded by count alone). The
/// programs of real statements take a few KB; `IN` with 2,000 values about
/// 300 KB, and with 7,000 or more over this.
const KEPT_STATEMENT_BYTES: usize = 1 << 20;

/// The memory statement `st` takes.
fn statement_bytes(st: &rusqlite::Statement<'_>) -> usize {
    st.get_status(rusqlite::StatementStatus::MemUsed).max(0) as usize
}

/// A script's statement: one kept in the connection's cache for the next run
/// of its text, or one run once (a program over `KEPT_STATEMENT_BYTES`).
enum Prepared<'c> {
    Kept(rusqlite::CachedStatement<'c>),
    Once(rusqlite::Statement<'c>),
}

impl<'c> std::ops::Deref for Prepared<'c> {
    type Target = rusqlite::Statement<'c>;
    fn deref(&self) -> &rusqlite::Statement<'c> {
        match self {
            Prepared::Kept(st) => st,
            Prepared::Once(st) => st,
        }
    }
}

impl<'c> std::ops::DerefMut for Prepared<'c> {
    fn deref_mut(&mut self) -> &mut rusqlite::Statement<'c> {
        match self {
            Prepared::Kept(st) => st,
            Prepared::Once(st) => st,
        }
    }
}

impl Prepared<'_> {
    /// Finalizes the statement instead of keeping it.
    fn discard(self) {
        if let Prepared::Kept(st) = self {
            st.discard();
        }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let (Some(c), Some(lease)) = (self.conn.take(), self.pooled.take()) {
            give_back(c, lease);
        }
    }
}

// ----- Connections to database files, kept between scripts --------------------
//
// Opening a database file costs several opens of the file (the link checks,
// then SQLite's own), which the OS serializes when many requests open the same
// file. So a connection to a file goes back to a pool when its script is done
// with it, and the next script that opens the same file takes it again:
//
// - At every open, `resolve_path` checks and pins the directories, the journal
//   names (`-journal`, `-wal`, `-shm`) must be missing or plain files, and the
//   database's name must still be a plain file with one link and the very
//   file the kept connection has open (its identity). Otherwise the
//   connection is not used. Files SQLite opens later are checked by `vfs`.
// - The authorizer and PHV's SQL functions (`LIKE`/`GLOB`, `changes()`...)
//   are registered once, when the connection is opened: registering a
//   function or the authorizer again expires every prepared statement
//   (SQLite's `sqlite3ExpirePreparedStatements`), which `sqlite3_step` then
//   compiles again. What they know of the script (its deadline, the counters'
//   base, whether a statement tainted the connection) is a `Session` set at
//   every open, with the progress handler and its deadline (`for_script`).
// - When the script is done, an open transaction is rolled back. The
//   connection is closed instead of kept after a failed statement, any
//   `PRAGMA` (the session settings it may change), or when a temporary table,
//   view, index or trigger exists; also when the rollback fails. Its page
//   cache is let go of; its prepared statements are kept, with their bound
//   values cleared (see `Db::statement` and `reusable`).
// - An idle connection is closed after `KEPT_IDLE`, and when a script deletes
//   or renames its file. `:memory:` databases are never kept. The idle
//   connections are bounded by count (`KEPT_PER_FILE`, `KEPT_ALL`) and by
//   the memory they hold together (`KEPT_BYTES`); a statement's program by
//   `KEPT_STATEMENT_BYTES`.
// - SQLite's memory is counted as the thread's (`mem`). The memory a kept
//   connection holds is taken off the thread that gives it back and counted
//   again on the thread that takes it (or closes it), so a script is charged
//   for the connections it holds, kept or new.

/// Idle connections kept per database file, and in all.
const KEPT_PER_FILE: usize = 64;
const KEPT_ALL: usize = 256;
/// The most memory the idle connections hold together (`held`): it counts
/// against no script while they are idle (found by the 1.2.0 audit: the pool
/// was bounded by count alone, 256 connections of 32 statements each). When a
/// connection going back would pass it, the idle ones that hold the most are
/// closed until it fits; one that would not fit alone is closed instead.
const KEPT_BYTES: usize = 64 << 20;
/// An idle connection is closed after this long.
const KEPT_IDLE: Duration = Duration::from_secs(30);
/// `KEPT_IDLE`, in milliseconds; tests make it shorter (`set_kept_idle`).
static KEPT_IDLE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(KEPT_IDLE.as_millis() as u64);
/// `KEPT_BYTES`; tests make it smaller (`set_kept_bytes`).
static KEPT_BYTES_NOW: AtomicUsize = AtomicUsize::new(KEPT_BYTES);

fn kept_idle() -> Duration {
    Duration::from_millis(KEPT_IDLE_MS.load(Ordering::SeqCst))
}

/// Sets how long an idle connection is kept (`None`: `KEPT_IDLE`). For tests.
pub fn set_kept_idle(idle: Option<Duration>) {
    KEPT_IDLE_MS.store(idle.unwrap_or(KEPT_IDLE).as_millis() as u64, Ordering::SeqCst);
}

fn kept_bytes() -> usize {
    KEPT_BYTES_NOW.load(Ordering::SeqCst)
}

/// Sets how much memory the idle connections may hold together (`None`:
/// `KEPT_BYTES`). For tests.
pub fn set_kept_bytes(bytes: Option<usize>) {
    KEPT_BYTES_NOW.store(bytes.unwrap_or(KEPT_BYTES), Ordering::SeqCst);
}

/// A connection a script holds: the file it is for, and its session (whose
/// `tainted` says whether it may be kept).
struct Lease {
    key: PathBuf,
    id: FileId,
    session: Arc<Session>,
    /// The memory the connection took when it was opened (see `held`).
    base: usize,
}

struct Idle {
    /// Always there until it is taken (`into_conn`).
    conn: Option<Connection>,
    id: FileId,
    since: Instant,
    base: usize,
    /// The memory it holds, taken off the thread that gave it back.
    held: usize,
    session: Arc<Session>,
}

impl Idle {
    /// A connection going into the pool: the memory it holds is no longer
    /// this thread's.
    fn new(conn: Connection, lease: Lease) -> Idle {
        let held = held(&conn, lease.base);
        phv_vm::alloc::uncharge_thread(held);
        Idle { conn: Some(conn), id: lease.id, since: Instant::now(), base: lease.base, held, session: lease.session }
    }

    fn into_conn(mut self) -> (Connection, usize, Arc<Session>) {
        let conn = self.conn.take().expect("an idle connection has its connection");
        (conn, self.base, self.session.clone())
    }
}

impl Drop for Idle {
    /// Taken or closed: the memory is the current thread's again (closing it
    /// frees it from this thread's count).
    fn drop(&mut self) {
        phv_vm::alloc::charge_thread(self.held);
    }
}

/// The memory connection `c` holds: what opening it took (`base`), and its
/// page cache, schema and prepared statements (with the values bound to
/// them, `SQLITE_DBSTATUS_STMT_USED` counts what finalizing them would free).
fn held(c: &Connection, base: usize) -> usize {
    // SAFETY: the handle is the open connection's.
    let db = unsafe { c.handle() };
    let mut sum = base;
    for op in [
        rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED,
        rusqlite::ffi::SQLITE_DBSTATUS_SCHEMA_USED,
        rusqlite::ffi::SQLITE_DBSTATUS_STMT_USED,
    ] {
        let (mut now, mut high) = (0, 0);
        // SAFETY: as above; the out pointers are valid.
        if unsafe { rusqlite::ffi::sqlite3_db_status(db, op, &mut now, &mut high, 0) } == rusqlite::ffi::SQLITE_OK {
            sum = sum.saturating_add(now.max(0) as usize);
        }
    }
    sum
}

static KEPT: LazyLock<Mutex<HashMap<PathBuf, Vec<Idle>>>> = LazyLock::new(Default::default);
/// Opens served by a kept connection (for tests).
static REUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Runs of scripts' statements for which SQLite compiled something: at the
/// prepare, or at the first step after a schema change (for tests).
static COMPILED: AtomicU64 = AtomicU64::new(0);

/// Idle connections kept, opens served by one so far, runs of scripts'
/// statements that compiled something so far, and the memory the idle
/// connections hold (for tests).
pub fn kept_stats() -> (usize, u64, u64, usize) {
    let map = kept();
    let idle: Vec<&Idle> = map.values().flatten().collect();
    (idle.len(), REUSED.load(Ordering::SeqCst), COMPILED.load(Ordering::SeqCst), idle.iter().map(|i| i.held).sum())
}

/// Statements weighed (`EXPLAIN` runs) so far, and the weights kept per
/// process (for tests).
pub fn weight_stats() -> (u64, usize) {
    cost::weight_stats()
}
static SWEEPING: AtomicBool = AtomicBool::new(false);

fn kept() -> MutexGuard<'static, HashMap<PathBuf, Vec<Idle>>> {
    KEPT.lock().unwrap_or_else(|e| e.into_inner())
}

/// An idle connection to the file `key` whose identity is `id`. Connections
/// to another file of that name, or idle for too long, are closed.
fn take(key: &Path, id: FileId) -> Option<(Connection, usize, Arc<Session>)> {
    let mut closing = Vec::new();
    let found = {
        let mut map = kept();
        let list = map.get_mut(key)?;
        let mut found = None;
        while let Some(idle) = list.pop() {
            if idle.id == id && idle.since.elapsed() < kept_idle() {
                REUSED.fetch_add(1, Ordering::SeqCst);
                found = Some(idle.into_conn());
                break;
            }
            closing.push(idle);
        }
        if list.is_empty() {
            map.remove(key);
        }
        found
    };
    // Closed outside the lock: closing a file can take a while.
    drop(closing);
    found
}

/// Takes back a connection a script is done with, if it can be reused: when
/// the pool has room for it by count (`KEPT_PER_FILE`, `KEPT_ALL`) and by
/// memory (`KEPT_BYTES`, making room by closing the idle connections that
/// hold the most). Otherwise it is closed.
fn give_back(c: Connection, lease: Lease) {
    let Some(c) = reusable(c, &lease.session) else { return };
    let key = lease.key.clone();
    let idle = Idle::new(c, lease);
    let closing = {
        let mut map = kept();
        // One pass over the idle connections (at most `KEPT_ALL`).
        let (all, mut held) = map.values().flatten().fold((0, 0usize), |(n, b), i| (n + 1, b.saturating_add(i.held)));
        let cap = kept_bytes();
        let of_file = map.get(&key).map_or(0, Vec::len);
        let mut closing = Vec::new();
        if of_file >= KEPT_PER_FILE || all >= KEPT_ALL || idle.held > cap {
            closing.push(idle);
        } else {
            while held.saturating_add(idle.held) > cap {
                let Some(big) = take_largest(&mut map) else { break };
                held -= big.held;
                closing.push(big);
            }
            map.entry(key).or_default().push(idle);
        }
        closing
    };
    // Closed outside the lock: closing a file can take a while.
    drop(closing);
    sweep_later();
}

/// Removes the idle connection that holds the most memory.
fn take_largest(map: &mut HashMap<PathBuf, Vec<Idle>>) -> Option<Idle> {
    let (key, at) = map
        .iter()
        .flat_map(|(k, l)| l.iter().enumerate().map(move |(i, idle)| (k, i, idle.held)))
        .max_by_key(|&(_, _, held)| held)
        .map(|(k, i, _)| (k.clone(), i))?;
    let list = map.get_mut(&key)?;
    let idle = list.remove(at);
    if list.is_empty() {
        map.remove(&key);
    }
    Some(idle)
}

/// Rolls back what the script left and checks the connection can be kept.
fn reusable(mut c: Connection, session: &Session) -> Option<Connection> {
    // Statements run here must not stop at the script's deadline.
    c.progress_handler(0, None::<fn() -> bool>).ok()?;
    session.set_deadline(None);
    if session.tainted.load(Ordering::SeqCst) {
        return None;
    }
    if !c.is_autocommit() && c.execute_batch("ROLLBACK").is_err() {
        return None;
    }
    let temp: i64 = c.query_row("SELECT count(*) FROM sqlite_temp_master", [], |r| r.get(0)).ok()?;
    if temp != 0 || !c.is_autocommit() || session.tainted.load(Ordering::SeqCst) {
        return None;
    }
    // The page cache is let go of (it is SQLite's memory; `release_memory`
    // leaves the statements alone). The prepared statements stay, so the next
    // script runs the same texts without compiling or weighing them again
    // (see `Db::statement`); their memory goes with the connection (`held`).
    // None of this script's values is in them: rusqlite's `StatementCache`
    // (0.37, `cache.rs`) takes a `CachedStatement` back at its drop, which
    // comes after `Rows` or `execute` reset it (`sqlite3_reset`), and calls
    // `sqlite3_clear_bindings` on it before it goes into the cache.
    c.release_memory().ok()?;
    Some(c)
}

/// Closes the idle connections of the file `name` in `dir` (before a script
/// deletes or renames it: on Windows an open file cannot be).
pub(crate) fn release(dir: &DirGuard, name: &std::ffi::OsStr) {
    let key = dir.path().join(name);
    let closing = kept().remove(&key);
    drop(closing);
}

/// Closes idle connections once they are idle for `KEPT_IDLE`, on a thread
/// that runs while any are kept. It sleeps until the oldest one's time is up
/// (found by the 1.1.1 audit: woken every 15 seconds, it kept a connection,
/// and its file open, for up to 45).
fn sweep_later() {
    if SWEEPING.swap(true, Ordering::SeqCst) {
        return;
    }
    let started = std::thread::Builder::new().name("phv-sqlite-idle".into()).spawn(|| loop {
        let idle = kept_idle();
        let (closing, next): (Vec<Idle>, Option<Instant>) = {
            let mut map = kept();
            let mut out = Vec::new();
            for list in map.values_mut() {
                let (old, fresh): (Vec<Idle>, Vec<Idle>) =
                    std::mem::take(list).into_iter().partition(|i| i.since.elapsed() >= idle);
                *list = fresh;
                out.extend(old);
            }
            map.retain(|_, l| !l.is_empty());
            let next = map.values().flatten().map(|i| i.since + idle).min();
            if next.is_none() {
                SWEEPING.store(false, Ordering::SeqCst);
            }
            (out, next)
        };
        // Closed outside the lock: closing a file can take a while.
        drop(closing);
        let Some(next) = next else { return };
        // At most a second at a time: the connection it slept for may have
        // been taken since, and the kept time changed (by tests), so the
        // next one is looked for again.
        std::thread::sleep(next.saturating_duration_since(Instant::now()).min(Duration::from_secs(1)));
    });
    if started.is_err() {
        SWEEPING.store(false, Ordering::SeqCst);
    }
}

/// The VDBE steps between two looks at the deadline (SQLite's progress
/// handler). One step may be a function call on a value of many MB, so the
/// steps between looks must be few (found by the 1.2.0 audit: with 10,000, a
/// query calling `trim()` on 30 MB values ran 468 s under a 1 s limit). The
/// work in a step is bounded in bytes too: SQLite's allocations look at the
/// deadline every MB (`mem`), and PHV's own functions look at it on each
/// call (`like_functions`).
const PROGRESS_STEPS: std::ffi::c_int = 100;

/// The most steps of a script's statement (`SQLITE_LIMIT_VDBE_OP`; SQLite
/// grows a program by doubling, so the largest is 43,008). Expanding a view
/// or `WITH` table many times over makes a long program from a short text
/// (found by the 1.2.0 audit); the statements of real programs have tens or
/// hundreds of steps, and `IN` with 10,000 values 30,000.
const PREPARE_OPS: std::ffi::c_int = 65_536;

/// The most steps of a program SQLite compiles while a statement runs (the
/// statement itself when its schema changed, and statements of SQLite's own,
/// as for `VACUUM` or a full-text index); the largest is 168 steps.
const RUN_OPS: std::ffi::c_int = 200;

/// What a connection's authorizer and PHV's SQL functions know of the
/// script that holds the connection. They are registered once, when the
/// connection is opened (`install`), and read this at each call; `for_script`
/// sets it for each script. Registering them again for each script would
/// expire every prepared statement (SQLite compiles an expired statement
/// again at its next step), and a kept connection keeps its statements
/// between scripts.
struct Session {
    /// Set after a statement that keeps the connection from being kept (a
    /// failed one, a `PRAGMA`, a temporary object).
    tainted: AtomicBool,
    /// Counts the calls of the authorizer, which SQLite makes while it
    /// compiles a statement (see `Db::statement`).
    compiles: AtomicU64,
    /// The script's deadline, as nanoseconds after `EPOCH` plus one (0:
    /// none); an atomic, as the functions read it at every call.
    deadline: AtomicU64,
    /// SQLite's count of changes when the script took the connection
    /// (`changes()` and `total_changes()` count from there).
    changes_base: AtomicU64,
    /// The database file the connection has open (none for `:memory:`): the
    /// key of its statements' weights (`cost`).
    file: Option<cost::Database>,
    /// When the current wait for a lock began, as nanoseconds after `EPOCH`
    /// (see `wait_for_lock`).
    waiting_since: AtomicU64,
}

/// The longest a statement waits for a lock another connection holds (a
/// write while another connection writes), unless the script's time limit
/// comes first; it then fails with "database is locked".
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// The instant the sessions' deadlines count from.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

impl Session {
    /// Registers the authorizer and PHV's SQL functions on a new connection
    /// to `file` (none for `:memory:`), after the caller's own pragmas (the
    /// authorizer refuses them).
    fn install(c: &mut Connection, file: Option<cost::Database>) -> rusqlite::Result<Arc<Session>> {
        let session = Arc::new(Session {
            tainted: AtomicBool::new(false),
            compiles: AtomicU64::new(0),
            deadline: AtomicU64::new(0),
            changes_base: AtomicU64::new(0),
            file,
            waiting_since: AtomicU64::new(0),
        });
        // SQLite runs one `LIKE` or `GLOB` as a single step, which the progress
        // handler cannot stop (a long text and pattern take minutes): these do.
        like_functions(c, session.clone())?;
        counters(c, session.clone())?;
        let auth = session.clone();
        c.authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
            use rusqlite::hooks::AuthAction;
            auth.compiles.fetch_add(1, Ordering::SeqCst);
            match &ctx.action {
                // The plans of later programs change: the weights kept before
                // are not used after (see `cost`).
                AuthAction::Analyze { .. } => cost::note_analyze(),
                AuthAction::Pragma { pragma_name, .. } if pragma_name.eq_ignore_ascii_case("optimize") => {
                    cost::note_analyze()
                }
                _ => {}
            }
            // A pragma may change the session; temporary objects stay in it.
            if matches!(
                ctx.action,
                AuthAction::Pragma { .. }
                    | AuthAction::CreateTempIndex { .. }
                    | AuthAction::CreateTempTable { .. }
                    | AuthAction::CreateTempTrigger { .. }
                    | AuthAction::CreateTempView { .. }
            ) {
                auth.tainted.store(true, Ordering::SeqCst);
            }
            sqlite_authorizer(ctx)
        }))?;
        // Waits for another connection's lock end at the script's deadline
        // (it replaces `harden`'s `busy_timeout`; see `wait_for_lock`).
        let busy = session.clone();
        c.busy_handler(Some(move |count: i32| busy.wait_for_lock(count)))?;
        Ok(session)
    }

    /// SQLite's busy handler: whether to try again for a lock another
    /// connection holds, after sleeping a little (as SQLite's own handler
    /// does, 1 ms at first and at most 100 ms at a time). It gives up after
    /// `LOCK_WAIT` in all, and at the script's deadline. SQLite sleeps here
    /// outside its program's steps, so the progress handler cannot stop the
    /// wait (found by the 20th audit: with `PRAGMA busy_timeout`, which
    /// scripts could set, a write waited up to 24 days past the time limit,
    /// and the 5 seconds of `busy_timeout` ran past a shorter limit).
    fn wait_for_lock(&self, count: i32) -> bool {
        const DELAYS_MS: [u64; 12] = [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100];
        let now = Instant::now();
        let since = if count == 0 {
            self.waiting_since.store(now.saturating_duration_since(*EPOCH).as_nanos() as u64, Ordering::SeqCst);
            now
        } else {
            *EPOCH + Duration::from_nanos(self.waiting_since.load(Ordering::SeqCst))
        };
        let mut left = LOCK_WAIT.saturating_sub(now.saturating_duration_since(since));
        if let Some(deadline) = self.deadline() {
            left = left.min(deadline.saturating_duration_since(now));
        }
        let step = Duration::from_millis(DELAYS_MS[(count.max(0) as usize).min(DELAYS_MS.len() - 1)]);
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(step.min(left));
        true
    }

    fn set_deadline(&self, deadline: Option<Instant>) {
        let n = match deadline {
            None => 0,
            Some(d) => d.saturating_duration_since(*EPOCH).as_nanos().min(u64::MAX as u128 - 1) as u64 + 1,
        };
        self.deadline.store(n, Ordering::SeqCst);
    }

    fn deadline(&self) -> Option<Instant> {
        match self.deadline.load(Ordering::SeqCst) {
            0 => None,
            n => Some(*EPOCH + Duration::from_nanos(n - 1)),
        }
    }
}

/// Sets what belongs to the script on a connection, new or kept: the
/// progress handler with its deadline, the session's deadline and taint,
/// counters that start from zero, and the size of programs. Nothing here
/// expires the connection's prepared statements.
fn for_script(c: &mut Connection, vm: &Vm, session: &Session) -> rusqlite::Result<()> {
    // Queries stop when the script's time limit is reached.
    match vm.deadline() {
        Some(deadline) => c.progress_handler(PROGRESS_STEPS, Some(move || Instant::now() > deadline))?,
        None => c.progress_handler(0, None::<fn() -> bool>)?,
    }
    session.set_deadline(vm.deadline());
    session.tainted.store(false, Ordering::SeqCst);
    // `last_insert_rowid()`, `changes()` and `total_changes()` count this
    // script's statements only, as on a new connection.
    // SAFETY: the handle is the open connection's.
    unsafe { rusqlite::ffi::sqlite3_set_last_insert_rowid(c.handle(), 0) };
    session.changes_base.store(c.total_changes(), Ordering::SeqCst);
    c.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_VDBE_OP, RUN_OPS)?;
    Ok(())
}

/// `changes()` and `total_changes()` as the script's (from the session's
/// base, see `for_script`).
fn counters(c: &Connection, session: Arc<Session>) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    // SAFETY: the handle is the open connection's; the functions below are
    // owned by the connection, so it is open whenever they run.
    let db = unsafe { c.handle() } as usize;
    let flags = FunctionFlags::SQLITE_UTF8;
    let total_changes = session.clone();
    c.create_scalar_function("total_changes", 0, flags, move |_| {
        // SAFETY: see above.
        let total = unsafe { rusqlite::ffi::sqlite3_total_changes64(db as *mut rusqlite::ffi::sqlite3) } as u64;
        Ok(total.saturating_sub(total_changes.changes_base.load(Ordering::SeqCst)) as i64)
    })?;
    c.create_scalar_function("changes", 0, flags, move |_| {
        // SAFETY: see above.
        let db = db as *mut rusqlite::ffi::sqlite3;
        // Until this script changes a row, the last change was another script's.
        if (unsafe { rusqlite::ffi::sqlite3_total_changes64(db) } as u64) == session.changes_base.load(Ordering::SeqCst)
        {
            return Ok(0);
        }
        Ok(unsafe { rusqlite::ffi::sqlite3_changes64(db) })
    })?;
    Ok(())
}

/// The statements for `BEGIN` / `COMMIT` / `ROLLBACK` of `Database::transaction()`
/// with `depth` transactions already open, and the depth after them. Inner
/// levels are savepoints: an inner transaction neither commits the outer one
/// (PostgreSQL) nor ends it silently (MySQL `BEGIN`).
pub(crate) fn transaction_sql(cmd: &str, depth: u32) -> (Vec<String>, u32) {
    match (cmd, depth) {
        ("BEGIN", 0) => (vec!["BEGIN".into()], 1),
        ("BEGIN", d) => (vec![format!("SAVEPOINT phv_{}", d + 1)], d + 1),
        (_, 0 | 1) => (vec![cmd.to_string()], 0),
        ("COMMIT", d) => (vec![format!("RELEASE SAVEPOINT phv_{d}")], d - 1),
        (_, d) => (vec![format!("ROLLBACK TO SAVEPOINT phv_{d}"), format!("RELEASE SAVEPOINT phv_{d}")], d - 1),
    }
}

/// Why the database ended the transaction of the open `transaction()` calls
/// before the outermost one did (found by the 19th audit: on PostgreSQL a
/// `COMMIT` after a caught statement error rolled back without an error, and
/// `transaction()` returned as if it had committed).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Ended {
    /// PostgreSQL: a statement in it failed (PostgreSQL ends a transaction at
    /// its first error; its `COMMIT` then rolls back).
    Aborted,
    /// A statement in it ended it: `COMMIT`, `ROLLBACK` or `BEGIN` written as
    /// SQL, a statement that commits implicitly (MySQL), or an error that
    /// rolls the whole transaction back (SQLite).
    ByStatement,
    /// MySQL rolled it back after a deadlock (error 1213).
    Deadlock,
    /// MySQL rolled it back after a lock wait timeout (error 1205, with
    /// `innodb_rollback_on_timeout` on).
    LockWait,
}

impl Ended {
    fn why(self) -> &'static str {
        match self {
            Ended::Aborted => {
                "it was rolled back: a statement in it failed, and PostgreSQL ends a transaction at its first \
                 error (an inner transaction() around the statement lets the rest go on)"
            }
            Ended::ByStatement => {
                "a statement in it ended it (COMMIT, ROLLBACK or BEGIN written as SQL, a statement that commits \
                 implicitly, or an error that rolls the whole transaction back)"
            }
            Ended::Deadlock => "it was rolled back after a deadlock",
            Ended::LockWait => "it was rolled back after a lock wait timeout",
        }
    }

    /// The error of the `COMMIT` of `transaction()`.
    pub(crate) fn not_committed(self) -> String {
        format!("transaction() did not commit: {}", self.why())
    }

    /// The error of a statement run in it afterwards (it would run outside
    /// any transaction, and stay when `transaction()` throws).
    pub(crate) fn refused(self) -> String {
        format!("the transaction of transaction() has ended, and nothing more runs in it: {}", self.why())
    }
}

/// A connection `Database::transaction()` works on (SQLite, PostgreSQL, MySQL).
pub(crate) trait TxConn {
    fn depth(&self) -> u32;
    fn set_depth(&mut self, depth: u32);
    fn ended(&self) -> Option<Ended>;
    fn set_ended(&mut self, ended: Option<Ended>);
    /// Runs one statement of `transaction_sql`.
    fn run(&mut self, vm: &Vm, sql: &str) -> Result<(), VmError>;
    /// The outermost `COMMIT` (PostgreSQL checks the transaction was not aborted).
    fn commit(&mut self, vm: &Vm) -> Result<(), VmError> {
        self.run(vm, "COMMIT")
    }
    /// The outermost level has ended; `rolled_back` is false when its
    /// `ROLLBACK` failed, so the connection may still be in a transaction.
    fn finished(&mut self, vm: &mut Vm, rolled_back: bool);
}

/// `BEGIN`, `COMMIT` or `ROLLBACK` of `Database::transaction()` on `c`.
///
/// A `COMMIT` or `ROLLBACK` ends its level whatever happens (found by the 19th
/// audit: a `COMMIT` that failed left the transaction open and the depth as it
/// was, so later statements of the script, and later transactions that seemed
/// to commit, were rolled back at its end). A `COMMIT` that fails is followed
/// by the rollback of its level, and raises. When the database ended the
/// transaction itself (`Ended`), its savepoints went with it: the `COMMIT`
/// of every level raises, and the outermost level makes sure it is over.
pub(crate) fn transaction_step(vm: &mut Vm, c: &mut dyn TxConn, cmd: &str) -> Result<(), VmError> {
    let depth = c.depth();
    let (sqls, after) = transaction_sql(cmd, depth);
    if cmd == "BEGIN" {
        if depth == 0 {
            c.set_ended(None);
        }
        for sql in &sqls {
            c.run(vm, sql)?;
        }
        c.set_depth(after);
        return Ok(());
    }
    c.set_depth(after);
    let (undo, _) = transaction_sql("ROLLBACK", depth);
    let undo_all = |c: &mut dyn TxConn, vm: &Vm| undo.iter().try_for_each(|sql| c.run(vm, sql));
    let mut rolled_back = true;
    let r = match c.ended() {
        Some(ended) => {
            if after == 0 {
                rolled_back = c.run(vm, "ROLLBACK").is_ok();
            }
            if cmd == "COMMIT" {
                Err(vm.throw("DatabaseError", ended.not_committed()))
            } else {
                Ok(())
            }
        }
        None if cmd == "COMMIT" => {
            let r = if after == 0 { c.commit(vm) } else { sqls.iter().try_for_each(|sql| c.run(vm, sql)) };
            match r {
                Ok(()) => Ok(()),
                Err(e) => {
                    let undone = undo_all(c, vm);
                    rolled_back = undone.is_ok();
                    match e {
                        VmError::Fatal(_) => Err(e),
                        // The savepoint is gone: the transaction ended before (MySQL
                        // commits implicitly, and says only "SAVEPOINT does not exist").
                        _ if after > 0 && undone.is_err() => {
                            c.set_ended(Some(Ended::ByStatement));
                            Err(vm.throw("DatabaseError", Ended::ByStatement.not_committed()))
                        }
                        _ => Err(e),
                    }
                }
            }
        }
        None => match undo_all(c, vm) {
            // The savepoint is gone with the transaction: the levels around
            // it cannot commit (and the exception that rolls this one back
            // goes on, not this error).
            Err(VmError::Exception(_)) if after > 0 => {
                c.set_ended(Some(Ended::ByStatement));
                Ok(())
            }
            r => {
                rolled_back = r.is_ok();
                r
            }
        },
    };
    if after == 0 {
        c.set_ended(None);
        c.finished(vm, rolled_back);
    }
    r
}

/// Whether handle `h` is an SQLite database (otherwise a server connection, see `netdb`).
fn is_sqlite(vm: &Vm, h: &Value) -> bool {
    vm.resources.get(int(h) as usize).and_then(|r| r.as_ref()).is_some_and(|b| b.is::<Db>())
}

/// The SQL dialect of an open `Database` handle (std.orm), or None when it is closed.
pub(crate) fn dialect_of(vm: &Vm, h: &Value) -> Option<Dialect> {
    if is_sqlite(vm, h) {
        return Some(Dialect::Sqlite);
    }
    crate::netdb::dialect_of(vm, h)
}

fn db_of<'v>(vm: &'v Vm, h: &Value) -> Result<&'v Db, VmError> {
    vm.resources
        .get(int(h) as usize)
        .and_then(|r| r.as_ref())
        .and_then(|b| b.downcast_ref::<Db>())
        .ok_or_else(|| vm.throw("DatabaseError", "the database connection is closed"))
}

fn db_error(vm: &Vm, e: rusqlite::Error) -> VmError {
    // SQLite was refused memory past the script's limit (see `mem`).
    let no_memory = matches!(&e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::OutOfMemory);
    // SQLite was refused memory because the time limit had passed.
    if no_memory && mem::Budget::late() {
        return VmError::Fatal("database query stopped: the time limit was reached".into());
    }
    if no_memory && mem::Budget::refused() {
        return vm.reserve(usize::MAX).err().unwrap_or_else(|| vm.throw("DatabaseError", e.to_string()));
    }
    // `LIKE` and `GLOB` stop at the time limit with an error of their own.
    if vm.deadline().is_some_and(|d| std::time::Instant::now() > d) {
        return VmError::Fatal("database query stopped: the time limit was reached".into());
    }
    if let rusqlite::Error::SqliteFailure(f, _) = &e {
        if f.code == rusqlite::ErrorCode::OperationInterrupted {
            let secs = vm.deadline().map(|_| "the time limit").unwrap_or("a limit");
            return VmError::Fatal(format!("database query stopped: {secs} was reached"));
        }
    }
    vm.throw("DatabaseError", e.to_string())
}

struct Param<'a>(&'a Value);

impl rusqlite::ToSql for Param<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Owned(to_sql(self.0)))
    }
}

/// The rows of a query result as values, built the same way by every SQL
/// client: the column names made once, a map per row, and the script's memory
/// limit checked after each row (the next rows are not read once it is
/// passed). What each client bounds before a row gets here differs with
/// where the data is: SQLite's values are reserved before they are copied out
/// of SQLite's memory; PostgreSQL's and MySQL's are bounded as they are read
/// from the network (a byte budget and `max_allowed_packet`).
pub(crate) struct ResultRows {
    names: Vec<Rc<String>>,
    rows: Vec<Value>,
}

impl ResultRows {
    pub(crate) fn new(names: impl IntoIterator<Item = String>) -> ResultRows {
        ResultRows { names: names.into_iter().map(Rc::new).collect(), rows: Vec::new() }
    }

    /// Adds a row (its values in the order of the columns); an error once the
    /// memory limit is passed.
    pub(crate) fn push(&mut self, vm: &Vm, values: impl IntoIterator<Item = Value>) -> Result<(), VmError> {
        let mut m = IndexMap::with_capacity(self.names.len());
        for (n, v) in self.names.iter().zip(values) {
            m.insert(Key::Str(n.clone()), v);
        }
        self.rows.push(Value::map(m));
        vm.check_memory()
    }

    pub(crate) fn into_value(self) -> Value {
        Value::list(self.rows)
    }
}

fn query(vm: &mut Vm, a: &[Value]) -> R {
    if !is_sqlite(vm, &a[0]) {
        return crate::netdb::query(vm, a);
    }
    let (text, params) = parts(&a[1]);
    let text = render_sql(text, Dialect::Sqlite);
    let _budget = mem::Budget::enter(vm);
    let db = db_of(vm, &a[0])?;
    let bound: Vec<Param> = params.iter().map(Param).collect();
    let mut again = true;
    'run: loop {
        // Kept prepared for the next run of the same text (`prepare_cached`).
        let mut stmt = db.statement(vm, &text)?;
        let before = db.session.compiles.load(Ordering::SeqCst);
        let mut rows = stmt.query(rusqlite::params_from_iter(bound.iter())).map_err(|e| db.fail(vm, e))?;
        // Made once the first row is there: a kept statement is prepared again
        // by its first step when the schema changed, and its columns with it.
        let mut out: Option<ResultRows> = None;
        loop {
            let row = match rows.next() {
                Ok(Some(row)) => row,
                Ok(None) => {
                    db.ran(before);
                    break;
                }
                // Compiled again too large by its first step (see `Db::statement`):
                // prepared and weighed once more.
                Err(e) if again && out.is_none() && db.refused_recompile(&e, before) => {
                    again = false;
                    drop(rows);
                    stmt.discard();
                    continue 'run;
                }
                Err(e) => return Err(db.fail(vm, e)),
            };
            let out = out.get_or_insert_with(|| ResultRows::new(row.as_ref().column_names().map(|n| n.to_string())));
            let mut values = Vec::with_capacity(out.names.len());
            for i in 0..out.names.len() {
                let v = match row.get_ref(i).map_err(|e| db.fail(vm, e))? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(i) => Value::Int(i),
                    ValueRef::Real(f) => Value::Float(f),
                    // A value is accounted before it is copied: a row may hold 16 MB, and
                    // invalid UTF-8 in a blob grows up to three times.
                    ValueRef::Text(t) => {
                        vm.reserve(t.len())?;
                        Value::str(String::from_utf8_lossy(t).into_owned())
                    }
                    ValueRef::Blob(b) => {
                        vm.reserve(b.len().saturating_mul(3))?;
                        Value::str(String::from_utf8_lossy(b).into_owned())
                    }
                };
                values.push(v);
            }
            out.push(vm, values)?;
            // A row ends a run of SQLite's program: the deadline is looked at.
            if vm.deadline().is_some_and(|d| Instant::now() > d) {
                return Err(VmError::Fatal("database query stopped: the time limit was reached".into()));
            }
        }
        return Ok(out.map_or_else(|| Value::list(Vec::new()), ResultRows::into_value));
    }
}

fn execute(vm: &mut Vm, a: &[Value]) -> R {
    if !is_sqlite(vm, &a[0]) {
        return crate::netdb::execute(vm, a);
    }
    let (text, params) = parts(&a[1]);
    let text = render_sql(text, Dialect::Sqlite);
    let _budget = mem::Budget::enter(vm);
    let db = db_of(vm, &a[0])?;
    let bound: Vec<Param> = params.iter().map(Param).collect();
    let before = db.conn().total_changes();
    let mut again = true;
    let n = loop {
        let mut stmt = db.statement(vm, &text)?;
        let compiles = db.session.compiles.load(Ordering::SeqCst);
        match stmt.execute(rusqlite::params_from_iter(bound.iter())) {
            Ok(n) => {
                db.ran(compiles);
                break n;
            }
            // Compiled again too large by its first step (see `Db::statement`):
            // prepared and weighed once more.
            Err(e) if again && db.refused_recompile(&e, compiles) => {
                again = false;
                stmt.discard();
            }
            Err(e) => return Err(db.fail(vm, e)),
        }
    };
    // A statement that changes no row (`CREATE TABLE`, `BEGIN`...) leaves
    // SQLite's count of the last change as it was, which on a kept connection
    // was another script's (found by the 1.0.0 audit): it changed 0 rows.
    let n = if db.conn().total_changes() == before { 0 } else { n };
    Ok(Value::Int(n as i64))
}

fn open(vm: &mut Vm, a: &[Value]) -> R {
    // Each connection holds files and memory: a script that opens a database
    // in a loop without `close()` is stopped (found by the 1.1.1 audit: 5,000
    // connections to one file held 935 MB under a 16 MB limit).
    let open = vm.resources.iter().flatten().filter(|r| r.is::<Db>()).count();
    if open >= MAX_OPEN {
        return Err(vm.throw(
            "DatabaseError",
            format!("this script already has {MAX_OPEN} SQLite databases open: close() the ones it no longer uses"),
        ));
    }
    let rel = s(&a[0]).to_string();
    let budget = mem::Budget::enter(vm);
    if rel == ":memory:" {
        return open_memory(vm, budget);
    }
    let (dir, name) = crate::sys::resolve_path(vm, &rel, true)?;
    let file_error = |vm: &Vm, e| crate::sys::fs_error(vm, &rel, "", e);
    // A kept connection is used only for the very file the name holds now,
    // checked as before an open (see "Connections to database files").
    check_journals(&dir, &name).map_err(|e| file_error(vm, e))?;
    let now = dir.plain_file_id(&name).map_err(|e| file_error(vm, e))?;
    let key = dir.path().join(&name);
    let (mut c, id, base, session) = match now.and_then(|id| take(&key, id).map(|(c, base, s)| (c, id, base, s))) {
        Some(kept) => kept,
        None => {
            // SQLite starts (once, for good) before the connection is measured.
            vfs::register();
            let before = phv_vm::alloc::thread_allocated();
            let (mut c, id) = open_in(&dir, &name).map_err(|e| match e {
                OpenError::File(_, e) => file_error(vm, e),
                OpenError::NoFileSystem => vm.throw("DatabaseError", NO_FILE_SYSTEM),
                OpenError::Sqlite(e) => db_error(vm, e),
            })?;
            // When the file was made, where the file system says (see `cost::Database`).
            let created = std::fs::metadata(dir.path().join(&name)).and_then(|m| m.created()).ok();
            let session = harden(&c)
                .and_then(|()| script_settings(&c))
                .and_then(|()| Session::install(&mut c, Some(cost::Database::new(id, created))))
                .map_err(|e| db_error(vm, e))?;
            let base = (phv_vm::alloc::thread_allocated() - before).max(0) as usize;
            (c, id, base, session)
        }
    };
    // A kept connection's memory counts from here (its `Idle` was dropped):
    // a failure below closes it on this thread.
    for_script(&mut c, vm, &session).map_err(|e| db_error(vm, e))?;
    drop(budget);
    let lease = Lease { key, id, session: session.clone(), base };
    let db = Db {
        conn: Some(c),
        _dir: Some(dir),
        depth: Cell::new(0),
        ended: Cell::new(None),
        pages: 0,
        pooled: Some(lease),
        session,
    };
    vm.resources.push(Some(Box::new(db)));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
}

/// The prepared statements a connection keeps (the texts run last; rusqlite
/// keeps 16 by default): a page's statements, with room for a few more.
const KEPT_STATEMENTS: usize = 32;

/// The settings of a script's connection beyond `harden`: temporary tables,
/// indexes and sorts are kept in memory, so they count against the script's
/// memory limit (found by the 1.1.1 audit: they went to temporary files,
/// which no limit bounded, also for `:memory:` databases without permissions);
/// and the statements it keeps.
fn script_settings(c: &Connection) -> rusqlite::Result<()> {
    c.set_prepared_statement_cache_capacity(KEPT_STATEMENTS);
    c.execute_batch("PRAGMA temp_store = MEMORY")
}

/// `Database::open(":memory:")`: a database of the script's own, never kept.
fn open_memory(vm: &mut Vm, _budget: mem::Budget) -> R {
    let counted = mem::init();
    let mut c = Connection::open_in_memory().map_err(|e| db_error(vm, e))?;
    harden(&c).and_then(|()| script_settings(&c)).map_err(|e| db_error(vm, e))?;
    // An in-memory database has at most the script's memory limit in pages
    // (found by the 1.0 audit: it had all of SQLite's). Its pages count as
    // the script's memory (`mem`), so several share the limit; only if
    // SQLite's memory were not counted do they split it.
    let total = vm.memory_limit().unwrap_or(128 << 20) / 4096;
    let used: usize = if counted {
        0
    } else {
        vm.resources.iter().flatten().filter_map(|r| r.downcast_ref::<Db>()).map(|d| d.pages).sum()
    };
    let pages = total.saturating_sub(used);
    if pages < 16 {
        return Err(
            vm.throw("DatabaseError", "the in-memory databases of this script already have its whole memory limit")
        );
    }
    c.execute_batch(&format!("PRAGMA page_size = 4096; PRAGMA max_page_count = {pages}"))
        .map_err(|e| db_error(vm, e))?;
    let session = Session::install(&mut c, None).map_err(|e| db_error(vm, e))?;
    for_script(&mut c, vm, &session).map_err(|e| db_error(vm, e))?;
    let db =
        Db { conn: Some(c), _dir: None, depth: Cell::new(0), ended: Cell::new(None), pages, pooled: None, session };
    vm.resources.push(Some(Box::new(db)));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
}

/// The journal files of the database `name` must be missing or plain files.
fn check_journals(dir: &DirGuard, name: &std::ffi::OsStr) -> Result<(), crate::safefs::FsError> {
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut n = name.to_os_string();
        n.push(suffix);
        dir.check_plain_or_missing(&n)?;
    }
    Ok(())
}

const NO_FILE_SYSTEM: &str = "SQLite has no file system support";

/// Why `open_in` failed.
enum OpenError {
    /// The database or one of its journals (the suffix) is not a plain file.
    File(&'static str, crate::safefs::FsError),
    NoFileSystem,
    Sqlite(rusqlite::Error),
}

/// Opens the SQLite file `name` in `dir`, for a script (`open`) or a
/// migration (`open_file`). SQLite opens the database and its journal files
/// by name, so each must be missing or a plain file (no link, no second hard
/// link) before and after opening. The directory itself cannot change
/// meanwhile. Files SQLite opens later (a journal per transaction) are
/// checked by `vfs`.
fn open_in(dir: &DirGuard, name: &std::ffi::OsStr) -> Result<(Connection, FileId), OpenError> {
    let check = || -> Result<(), OpenError> {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let mut n = name.to_os_string();
            n.push(suffix);
            dir.check_plain_or_missing(&n).map_err(|e| OpenError::File(suffix, e))?;
        }
        Ok(())
    };
    check()?;
    let before = dir.plain_file_id(name).map_err(|e| OpenError::File("", e))?;
    if !vfs::register() {
        return Err(OpenError::NoFileSystem);
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let c = Connection::open_with_flags_and_vfs(dir.path().join(name), flags, vfs::NAME).map_err(OpenError::Sqlite)?;
    check()?;
    // The file SQLite opened is the one checked (the name was not replaced meanwhile).
    let gone = || OpenError::File("", crate::safefs::FsError::Io(std::io::ErrorKind::NotFound.into()));
    let id = dir.plain_file_id(name).map_err(|e| OpenError::File("", e))?.ok_or_else(gone)?;
    if before.is_some_and(|b| b != id) {
        return Err(OpenError::File("", crate::safefs::FsError::BadName));
    }
    Ok((c, id))
}

/// Whether the process-wide cap on SQLite's memory is set (see `harden`).
static HEAP_LIMIT_SET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The settings of every SQLite connection, a script's or a migration's (the
/// authorizer, `sqlite_authorizer`, is set after the caller's own pragmas).
pub(crate) fn harden(c: &Connection) -> rusqlite::Result<()> {
    use rusqlite::limits::Limit;
    // How long a migration waits for a lock; a script's connection also stops
    // waiting at its deadline (`Session::wait_for_lock` replaces this).
    c.busy_timeout(LOCK_WAIT)?;
    // Defensive mode: no writes to the schema or the shadow tables of virtual
    // tables, which could corrupt the database file.
    c.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    // Prevent `ATTACH 'other.db'` from reaching files outside the permissions.
    c.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    // Single strings/blobs and statements are bounded (e.g. `zeroblob(1e9)`).
    c.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_TEXT as i32)?;
    c.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 1024 * 1024)?;
    // A process-wide cap on SQLite's own memory (set by the first connection;
    // no connection can change it, see `sqlite_authorizer`), and per
    // connection a small page cache. A script's share is bounded by its
    // memory limit (`mem`). Past the soft limit, page caches stop growing
    // (they reuse their pages) before the hard limit fails other requests.
    if !HEAP_LIMIT_SET.load(std::sync::atomic::Ordering::Acquire) {
        c.execute_batch("PRAGMA hard_heap_limit = 536870912; PRAGMA soft_heap_limit = 402653184")?;
        HEAP_LIMIT_SET.store(true, std::sync::atomic::Ordering::Release);
    }
    c.execute_batch("PRAGMA cache_size = -2000")?;
    Ok(())
}

/// Opens the SQLite file `rel` beneath `base` as a script does: no link or
/// second hard link on the way or for the database and its journal files,
/// before and after opening, and SQLite's later journals checked by `vfs`.
/// The directory guard keeps the directories from being swapped meanwhile.
pub(crate) fn open_file(
    base: &std::path::Path,
    rel: &std::path::Path,
) -> Result<(Connection, crate::safefs::DirGuard), String> {
    let (dir, name) = crate::safefs::open_parent(base, rel)
        .map_err(|e| format!("{}: {}", rel.display(), crate::safefs::describe(&e)))?;
    let (mut c, _) = open_in(&dir, &name).map_err(|e| match e {
        OpenError::File(suffix, e) => format!("{}{suffix}: {}", rel.display(), crate::safefs::describe(&e)),
        OpenError::NoFileSystem => NO_FILE_SYSTEM.to_string(),
        OpenError::Sqlite(e) => format!("{}: {e}", rel.display()),
    })?;
    harden(&c).map_err(|e| e.to_string())?;
    // Last: the authorizer refuses the pragmas set above.
    c.authorizer(Some(sqlite_authorizer)).map_err(|e| e.to_string())?;
    Ok((c, dir))
}

/// Refuses `ATTACH`/`DETACH` and pragmas that change process-wide state, touch
/// other files or lift the memory caps (scripts and migrations alike).
pub(crate) fn sqlite_authorizer(ctx: rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization {
    {
        use rusqlite::hooks::{AuthAction, Authorization};
        match ctx.action {
            AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
            AuthAction::Pragma { pragma_name, .. } => {
                let name = pragma_name.to_ascii_lowercase();
                let blocked = [
                    "temp_store_directory",
                    "data_store_directory",
                    "hard_heap_limit",
                    "soft_heap_limit",
                    "writable_schema",
                    "mmap_size",
                    "threads",
                    // These would lift the memory caps set above.
                    "cache_size",
                    "max_page_count",
                    "temp_store",
                    // It registers SQLite's own `like()` again, which the time limit cannot stop.
                    "case_sensitive_like",
                    // These can corrupt a database or make SQLite trust what is in it.
                    "schema_version",
                    "cell_size_check",
                    "trusted_schema",
                    // It replaces the busy handler, which gives up at the time
                    // limit, with a wait the time limit cannot stop (found by
                    // the 20th audit; see `Session::wait_for_lock`).
                    "busy_timeout",
                ];
                if blocked.contains(&name.as_str()) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }
            _ => Authorization::Allow,
        }
    }
}

/// One piece of a `LIKE` or `GLOB` pattern.
enum Pat {
    /// `%` or `*`.
    Any,
    /// `_` or `?`.
    One,
    Char(char),
    /// `[...]` in `GLOB`: ranges, and whether it is negated (`[^...]`).
    Class(Vec<(char, char)>, bool),
}

/// The longest pattern accepted (SQLite's default `SQLITE_LIMIT_LIKE_PATTERN_LENGTH`).
const MAX_PATTERN: usize = 50_000;

fn like_pattern(p: &str, escape: Option<char>) -> Vec<Pat> {
    let mut out = Vec::new();
    let mut it = p.chars();
    while let Some(c) = it.next() {
        out.push(match c {
            c if Some(c) == escape => match it.next() {
                Some(n) => Pat::Char(n),
                // A pattern ending in its escape character matches nothing, as in SQLite.
                None => Pat::Class(Vec::new(), false),
            },
            '%' => Pat::Any,
            '_' => Pat::One,
            c => Pat::Char(c),
        });
    }
    out
}

fn glob_pattern(p: &str) -> Vec<Pat> {
    let chars: Vec<char> = p.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        out.push(match c {
            '*' => Pat::Any,
            '?' => Pat::One,
            '[' => {
                let negated = chars.get(i) == Some(&'^');
                if negated {
                    i += 1;
                }
                let mut ranges = Vec::new();
                if chars.get(i) == Some(&']') {
                    ranges.push((']', ']'));
                    i += 1;
                }
                let mut prior: Option<char> = None;
                while i < chars.len() && chars[i] != ']' {
                    let c = chars[i];
                    match (c, prior, chars.get(i + 1)) {
                        ('-', Some(lo), Some(&hi)) if hi != ']' => {
                            ranges.push((lo, hi));
                            prior = None;
                            i += 2;
                        }
                        _ => {
                            ranges.push((c, c));
                            prior = Some(c);
                            i += 1;
                        }
                    }
                }
                if i >= chars.len() {
                    // An unclosed `[` matches nothing, as in SQLite.
                    return vec![Pat::Class(Vec::new(), false)];
                }
                i += 1;
                Pat::Class(ranges, negated)
            }
            c => Pat::Char(c),
        });
    }
    out
}

/// Whether `text` matches `pat`, or `None` when `deadline` passed first. The
/// usual single-backtrack wildcard match: at most text × pattern steps.
fn wild_match(text: &str, pat: &[Pat], fold: bool, deadline: Option<std::time::Instant>) -> Option<bool> {
    let same = |a: char, b: char| if fold { a.eq_ignore_ascii_case(&b) } else { a == b };
    let one = |p: &Pat, c: char| match p {
        Pat::One => true,
        Pat::Char(x) => same(*x, c),
        Pat::Class(ranges, negated) => ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi) != *negated,
        Pat::Any => false,
    };
    let (mut t, mut p) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    let mut steps = 0u32;
    while t < text.len() {
        steps = steps.wrapping_add(1);
        if steps.is_multiple_of(1 << 16) && deadline.is_some_and(|d| std::time::Instant::now() > d) {
            return None;
        }
        let c = text[t..].chars().next().unwrap_or('\0');
        if p < pat.len() && one(&pat[p], c) {
            t += c.len_utf8();
            p += 1;
        } else if p < pat.len() && matches!(pat[p], Pat::Any) {
            star = Some((p, t));
            p += 1;
        } else if let Some((sp, st)) = star {
            let next = st + text[st..].chars().next().map_or(1, char::len_utf8);
            star = Some((sp, next));
            p = sp + 1;
            t = next;
        } else {
            return Some(false);
        }
    }
    while p < pat.len() && matches!(pat[p], Pat::Any) {
        p += 1;
    }
    Some(p == pat.len())
}

/// The error of a function of ours when the script's time limit has passed
/// (`db_error` reports it as the time limit).
fn too_late(deadline: Option<Instant>) -> rusqlite::Result<()> {
    match deadline {
        Some(d) if Instant::now() > d => Err(rusqlite::Error::UserFunctionError("the time limit was reached".into())),
        _ => Ok(()),
    }
}

/// Refuses `bytes` that a function of ours is about to allocate when they
/// would take the script past its memory limit, as SQLite's own allocations
/// are (`mem`; `db_error` reports it as the memory limit).
fn room(bytes: usize) -> rusqlite::Result<()> {
    if mem::room(bytes) {
        return Ok(());
    }
    Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOMEM), None))
}

/// Replaces SQLite's `like()` (two and three arguments) and `glob()` with
/// versions that stop when the script's time limit passes.
///
/// SQLite does not call its progress handler while a function runs, nor
/// between the functions of one row, so each of these looks at the deadline
/// (the session's: the script that holds the connection now) when it is
/// called and, where its work can be long, as it goes. Their arguments are
/// read in place, and what they allocate is counted against the script's
/// memory limit before it is (found by the 1.2.0 audit: copies of 32 MB
/// values passed the limit).
fn like_functions(c: &Connection, session: Arc<Session>) -> rusqlite::Result<()> {
    use rusqlite::functions::{Context, FunctionFlags};
    use std::borrow::Cow;
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    fn text<'c>(ctx: &'c Context, i: usize) -> rusqlite::Result<Option<Cow<'c, str>>> {
        Ok(match ctx.get_raw(i) {
            ValueRef::Null => None,
            ValueRef::Text(t) | ValueRef::Blob(t) => match std::str::from_utf8(t) {
                Ok(t) => Some(Cow::Borrowed(t)),
                Err(_) => {
                    // An invalid byte becomes U+FFFD (three bytes).
                    room(t.len().saturating_mul(3))?;
                    Some(Cow::Owned(String::from_utf8_lossy(t).into_owned()))
                }
            },
            ValueRef::Integer(i) => Some(Cow::Owned(i.to_string())),
            ValueRef::Real(f) => Some(Cow::Owned(real_text(f))),
        })
    }
    let run = move |session: &Session, ctx: &Context, glob: bool| -> rusqlite::Result<Option<bool>> {
        let deadline = session.deadline();
        too_late(deadline)?;
        let (Some(p), Some(t)) = (text(ctx, 0)?, text(ctx, 1)?) else { return Ok(None) };
        if p.len() > MAX_PATTERN {
            return Err(rusqlite::Error::UserFunctionError("LIKE or GLOB pattern too complex".into()));
        }
        let pat = if glob {
            glob_pattern(&p)
        } else {
            let escape = if ctx.len() > 2 {
                match text(ctx, 2)? {
                    Some(e) if e.chars().count() == 1 => e.chars().next(),
                    None => return Ok(None),
                    _ => {
                        return Err(rusqlite::Error::UserFunctionError(
                            "ESCAPE expression must be a single character".into(),
                        ))
                    }
                }
            } else {
                None
            };
            like_pattern(&p, escape)
        };
        match wild_match(&t, &pat, !glob, deadline) {
            Some(m) => Ok(Some(m)),
            None => Err(rusqlite::Error::UserFunctionError("the time limit was reached".into())),
        }
    };
    let like = session.clone();
    c.create_scalar_function("like", 2, flags, move |ctx| run(&like, ctx, false))?;
    let like = session.clone();
    c.create_scalar_function("like", 3, flags, move |ctx| run(&like, ctx, false))?;
    let glob = session.clone();
    c.create_scalar_function("glob", 2, flags, move |ctx| run(&glob, ctx, true))?;

    // SQLite's `instr()`, `replace()` and `trim(X, Y)` search naively (text × pattern
    // in one step, minutes on long values): these search in linear time.
    let instr = session.clone();
    c.create_scalar_function("instr", 2, flags, move |ctx| {
        too_late(instr.deadline())?;
        let (Some(h), Some(n)) = (text(ctx, 0)?, text(ctx, 1)?) else { return Ok(None) };
        Ok(Some(h.find(&*n).map_or(0, |b| h[..b].chars().count() as i64 + 1)))
    })?;
    let replace = session.clone();
    c.create_scalar_function("replace", 3, flags, move |ctx| {
        too_late(replace.deadline())?;
        let (Some(x), Some(y), Some(z)) = (text(ctx, 0)?, text(ctx, 1)?, text(ctx, 2)?) else {
            return Ok(None);
        };
        if y.is_empty() {
            room(x.len())?;
            return Ok(Some(x.into_owned()));
        }
        let found = x.matches(&*y).count();
        let len = (x.len() - found * y.len()).saturating_add(found.saturating_mul(z.len()));
        if len > MAX_TEXT {
            return Err(rusqlite::Error::UserFunctionError("string or blob too big".into()));
        }
        room(len)?;
        Ok(Some(x.replace(&*y, &z)))
    })?;
    for (name, left, right) in [("trim", true, true), ("ltrim", true, false), ("rtrim", false, true)] {
        let trim = session.clone();
        c.create_scalar_function(name, 2, flags, move |ctx| {
            let deadline = trim.deadline();
            too_late(deadline)?;
            let (Some(x), Some(y)) = (text(ctx, 0)?, text(ctx, 1)?) else { return Ok(None) };
            let set = CharSet::of(&y, deadline)?;
            let mut t = &*x;
            if left {
                t = t.trim_start_matches(|c| set.has(c));
            }
            if right {
                t = t.trim_end_matches(|c| set.has(c));
            }
            room(t.len())?;
            Ok(Some(t.to_string()))
        })?;
    }
    Ok(())
}

/// The characters of `trim(X, Y)`'s `Y`: a bit for each character, so
/// making it and looking a character up take the same short time whatever
/// the characters are.
struct CharSet {
    ascii: [bool; 128],
    /// The others, made only when `Y` has one: a bit for every code point
    /// (136 KB).
    other: Vec<u64>,
}

impl CharSet {
    fn of(y: &str, deadline: Option<Instant>) -> rusqlite::Result<CharSet> {
        let mut set = CharSet { ascii: [false; 128], other: Vec::new() };
        for (i, c) in y.chars().enumerate() {
            if i % (1 << 20) == (1 << 20) - 1 {
                too_late(deadline)?;
            }
            let c = c as usize;
            if c < 128 {
                set.ascii[c] = true;
                continue;
            }
            if set.other.is_empty() {
                let words = (char::MAX as usize >> 6) + 1;
                room(words * 8)?;
                set.other = vec![0; words];
            }
            set.other[c >> 6] |= 1 << (c & 63);
        }
        Ok(set)
    }

    fn has(&self, c: char) -> bool {
        let c = c as usize;
        if c < 128 {
            return self.ascii[c];
        }
        self.other.get(c >> 6).is_some_and(|w| w & (1 << (c & 63)) != 0)
    }
}

/// The longest text or blob SQLite allows here (`SQLITE_LIMIT_LENGTH`): the
/// time some steps take grows with it (see `cost`). Twice the largest request
/// body (8 MB).
const MAX_TEXT: usize = 16 * 1024 * 1024;

/// A real number as SQLite writes it as text (`%!.15g`: `1.0`, `0.3`, `1.0e+20`).
fn real_text(f: f64) -> String {
    if !f.is_finite() {
        return if f.is_nan() {
            "NaN".into()
        } else if f > 0.0 {
            "Inf".into()
        } else {
            "-Inf".into()
        };
    }
    let sci = format!("{f:.14e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    // Trailing zeros go, but one digit stays after the point.
    let trim = |t: &str| -> String {
        let t = if t.contains('.') { t.trim_end_matches('0') } else { t };
        if t.ends_with('.') {
            format!("{t}0")
        } else if t.contains('.') {
            t.to_string()
        } else {
            format!("{t}.0")
        }
    };
    if !(-4..15).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa), exp.abs())
    } else {
        trim(&format!("{:.*}", (14 - exp).max(0) as usize, f))
    }
}

/// `transaction_step` on an SQLite database: its depth and `ended` are read
/// from the `Db` and written back after the step.
struct SqliteTx {
    h: Value,
    depth: u32,
    ended: Option<Ended>,
}

impl TxConn for SqliteTx {
    fn depth(&self) -> u32 {
        self.depth
    }

    fn set_depth(&mut self, depth: u32) {
        self.depth = depth;
    }

    fn ended(&self) -> Option<Ended> {
        self.ended
    }

    fn set_ended(&mut self, ended: Option<Ended>) {
        self.ended = ended;
    }

    fn run(&mut self, vm: &Vm, sql: &str) -> Result<(), VmError> {
        let db = db_of(vm, &self.h)?;
        // Nothing to roll back once SQLite ended the transaction (an error
        // would only keep the connection from being used again).
        if sql == "ROLLBACK" && db.conn().is_autocommit() {
            return Ok(());
        }
        db.conn().execute_batch(sql).map_err(|e| db.fail(vm, e))
    }

    /// A transaction SQLite still holds open (its `ROLLBACK` failed) would
    /// take the script's later statements with it: the handle is closed
    /// (its connection is rolled back, or not kept, when it goes back).
    fn finished(&mut self, vm: &mut Vm, _: bool) {
        let open = db_of(vm, &self.h).is_ok_and(|db| !db.conn().is_autocommit());
        if open {
            if let Some(slot) = vm.resources.get_mut(int(&self.h) as usize) {
                *slot = None;
            }
        }
    }
}

/// `BEGIN`, `COMMIT` or `ROLLBACK` of `Database::transaction()` (inner ones are savepoints).
fn transaction_step_of(vm: &mut Vm, a: &[Value], cmd: &str) -> R {
    if !is_sqlite(vm, &a[0]) {
        return crate::netdb::raw(vm, a);
    }
    let _budget = mem::Budget::enter(vm);
    let db = db_of(vm, &a[0])?;
    let mut tx = SqliteTx { h: a[0].clone(), depth: db.depth.get(), ended: db.ended.get() };
    let r = transaction_step(vm, &mut tx, cmd);
    if let Ok(db) = db_of(vm, &a[0]) {
        db.depth.set(tx.depth);
        db.ended.set(tx.ended);
    }
    r.map(|()| Value::Null)
}

/// Runs a script's statement (`query` or `execute`) on any database. A
/// statement that may roll back writes or ends a transaction (`ROLLBACK`,
/// `COMMIT` written as SQL) makes std.orm forget what it kept (found by the
/// 19th audit: only `transaction()` did). On SQLite, a statement after which
/// the connection is back in autocommit mode ended the open transaction,
/// perhaps by rolling it back (`INSERT OR ROLLBACK`, `RAISE(ROLLBACK)` in a
/// trigger), whether `transaction()` or the script's own `BEGIN` opened it:
/// std.orm forgets (found by the 20th audit: only a transaction of
/// `transaction()` was looked at). Inside `transaction()`, a statement is
/// refused once SQLite ended the transaction (`Ended`).
fn statement(vm: &mut Vm, a: &[Value], run: fn(&mut Vm, &[Value]) -> R) -> R {
    // A `COMMIT` too: on PostgreSQL it rolls back a transaction that a failed
    // statement aborted, without an error.
    let text = parts(&a[1]).0;
    let rolls_back = dialect_of(vm, &a[0])
        .is_some_and(|d| crate::netdb::may_roll_back(text, d) || crate::netdb::ends_transaction(text, d));
    if rolls_back {
        crate::orm::rolled_back();
    }
    if !is_sqlite(vm, &a[0]) {
        return run(vm, a);
    }
    let was_open = {
        let db = db_of(vm, &a[0])?;
        if let Some(ended) = db.ended.get().filter(|_| db.depth.get() > 0) {
            return Err(vm.throw("DatabaseError", ended.refused()));
        }
        !db.conn().is_autocommit()
    };
    let r = run(vm, a);
    if let Ok(db) = db_of(vm, &a[0]) {
        if was_open && db.conn().is_autocommit() && !rolls_back {
            crate::orm::rolled_back();
        }
        if db.depth.get() > 0 && db.ended.get().is_none() && db.conn().is_autocommit() {
            db.ended.set(Some(Ended::ByStatement));
            crate::orm::rolled_back();
        }
    }
    r
}

fn is_identifier(t: &str) -> bool {
    let mut chars = t.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__db_open" => open,
        "__db_query" => |vm, a| statement(vm, a, query),
        "__db_execute" => |vm, a| statement(vm, a, execute),
        "__db_connect" => crate::netdb::connect,
        "__redis_connect" => crate::netdb::connect_redis,
        "__redis" => crate::netdb::redis_command,
        "__redis_start" => crate::netdb::start_redis,
        "__redis_pipeline" => crate::netdb::redis_pipeline,
        "__db_start" => |vm, a| {
            if !is_sqlite(vm, &a[0]) {
                return crate::netdb::start_query(vm, a);
            }
            // SQLite runs in the script's thread: the statement is done when it returns.
            let r = statement(vm, a, if matches!(a[2], Value::Bool(true)) { execute } else { query });
            crate::asyncio::ready(vm, r)
        },
        "__db_last_id" => |vm, a| {
            if !is_sqlite(vm, &a[0]) {
                return crate::netdb::last_insert_id(vm, a);
            }
            Ok(Value::Int(db_of(vm, &a[0])?.conn().last_insert_rowid()))
        },
        "__db_raw" => |vm, a| {
            // Only BEGIN / COMMIT / ROLLBACK, used by Database::transaction().
            let cmd = s(&a[1]);
            if !matches!(cmd, "BEGIN" | "COMMIT" | "ROLLBACK") {
                return Err(VmError::Fatal(format!("__db_raw: unexpected statement {cmd}")));
            }
            // What std.orm remembers of rows may be undone (see `orm::rolled_back`).
            if cmd == "ROLLBACK" {
                crate::orm::rolled_back();
            }
            let r = transaction_step_of(vm, a, cmd);
            if r.is_err() {
                crate::orm::rolled_back();
            }
            r
        },
        "__db_close" => |vm, a| {
            if let Some(slot) = vm.resources.get_mut(int(&a[0]) as usize) {
                *slot = None;
            }
            Ok(Value::Null)
        },
        "sql_concat" => |_, a| {
            let (q1, p1) = parts(&a[0]);
            let (q2, p2) = parts(&a[1]);
            let params: Vec<Value> = p1.iter().chain(p2.iter()).cloned().collect();
            Ok(Value::list(vec![Value::str(format!("{q1}{q2}")), Value::list(params)]))
        },
        "sql_ident" => |vm, a| {
            let t = s(&a[0]);
            if !is_identifier(t) {
                return Err(vm.throw("ValueError", format!("\"{t}\" is not a valid SQL identifier")));
            }
            // PHV's own tables (`phv_sessions`, `phv_migrations`) are not reached
            // through a name from a request, like `phv:` keys in Redis.
            if t.len() >= 4 && t[..4].eq_ignore_ascii_case("phv_") {
                return Err(vm.throw("ValueError", format!("\"{t}\": names starting with `phv_` are PHV's own")));
            }
            Ok(Value::list(vec![Value::str(format!("{IDENT_START}{t}{IDENT_END}")), Value::list(Vec::new())]))
        },
        _ => return None,
    };
    Some(f)
}
