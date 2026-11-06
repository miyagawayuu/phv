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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

mod vfs;

type R = Result<Value, VmError>;

/// Marks a bound parameter in `Sql` text (see the type checker).
pub(crate) const PARAM: char = '\u{E000}';
/// Start and end of an identifier from `sql_ident()`.
pub(crate) const IDENT_START: char = '\u{E001}';
pub(crate) const IDENT_END: char = '\u{E002}';

/// How a database writes placeholders and quoted identifiers.
#[derive(Clone, Copy)]
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
    /// The pages an in-memory database may use (0 for a file).
    pages: usize,
    /// For a database file: the pool the connection goes back to.
    pooled: Option<Lease>,
}

impl Db {
    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("an open database has its connection")
    }

    /// The error of a failed statement; the connection is then not reused.
    fn fail(&self, vm: &Vm, e: rusqlite::Error) -> VmError {
        if let Some(lease) = &self.pooled {
            lease.tainted.store(true, Ordering::SeqCst);
        }
        db_error(vm, e)
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
// - The script's own settings are installed every time: the progress handler
//   with its deadline, `LIKE`/`GLOB` with its deadline, the counters
//   (`changes()`, `total_changes()`, `last_insert_rowid()`) and the
//   authorizer.
// - When the script is done, an open transaction is rolled back. The
//   connection is closed instead of kept after a failed statement, any
//   `PRAGMA` (the session settings it may change), or when a temporary table,
//   view, index or trigger exists; also when the rollback fails. Its cached
//   statements and page cache are dropped.
// - An idle connection is closed after `KEPT_IDLE`, and when a script deletes
//   or renames its file. `:memory:` databases are never kept.

/// Idle connections kept per database file, and in all.
const KEPT_PER_FILE: usize = 64;
const KEPT_ALL: usize = 256;
/// An idle connection is closed after this long.
const KEPT_IDLE: Duration = Duration::from_secs(30);
/// `KEPT_IDLE`, in milliseconds; tests make it shorter (`set_kept_idle`).
static KEPT_IDLE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(KEPT_IDLE.as_millis() as u64);

fn kept_idle() -> Duration {
    Duration::from_millis(KEPT_IDLE_MS.load(Ordering::SeqCst))
}

/// Sets how long an idle connection is kept (`None`: `KEPT_IDLE`). For tests.
pub fn set_kept_idle(idle: Option<Duration>) {
    KEPT_IDLE_MS.store(idle.unwrap_or(KEPT_IDLE).as_millis() as u64, Ordering::SeqCst);
}

/// A connection a script holds: the file it is for, and whether it may be kept.
struct Lease {
    key: PathBuf,
    id: FileId,
    tainted: Arc<AtomicBool>,
}

struct Idle {
    conn: Connection,
    id: FileId,
    since: Instant,
}

static KEPT: LazyLock<Mutex<HashMap<PathBuf, Vec<Idle>>>> = LazyLock::new(Default::default);
/// Opens served by a kept connection (for tests).
static REUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Idle connections kept, and opens served by one so far (for tests).
pub fn kept_stats() -> (usize, u64) {
    (kept().values().map(Vec::len).sum(), REUSED.load(Ordering::SeqCst))
}
static SWEEPING: AtomicBool = AtomicBool::new(false);

fn kept() -> MutexGuard<'static, HashMap<PathBuf, Vec<Idle>>> {
    KEPT.lock().unwrap_or_else(|e| e.into_inner())
}

/// An idle connection to the file `key` whose identity is `id`. Connections
/// to another file of that name, or idle for too long, are closed.
fn take(key: &Path, id: FileId) -> Option<Connection> {
    let mut closing = Vec::new();
    let found = {
        let mut map = kept();
        let list = map.get_mut(key)?;
        let mut found = None;
        while let Some(idle) = list.pop() {
            if idle.id == id && idle.since.elapsed() < kept_idle() {
                REUSED.fetch_add(1, Ordering::SeqCst);
                found = Some(idle.conn);
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

/// Takes back a connection a script is done with, if it can be reused.
fn give_back(c: Connection, lease: Lease) {
    let Some(c) = reusable(c, &lease.tainted) else { return };
    let idle = Idle { conn: c, id: lease.id, since: Instant::now() };
    let refused = {
        let mut map = kept();
        let all: usize = map.values().map(Vec::len).sum();
        let list = map.entry(lease.key).or_default();
        if list.len() < KEPT_PER_FILE && all < KEPT_ALL {
            list.push(idle);
            None
        } else {
            Some(idle)
        }
    };
    drop(refused);
    sweep_later();
}

/// Rolls back what the script left and checks the connection can be kept.
fn reusable(c: Connection, tainted: &AtomicBool) -> Option<Connection> {
    // Statements run here must not stop at the script's deadline.
    c.progress_handler(0, None::<fn() -> bool>);
    if tainted.load(Ordering::SeqCst) {
        return None;
    }
    if !c.is_autocommit() && c.execute_batch("ROLLBACK").is_err() {
        return None;
    }
    let temp: i64 = c.query_row("SELECT count(*) FROM sqlite_temp_master", [], |r| r.get(0)).ok()?;
    if temp != 0 || !c.is_autocommit() || tainted.load(Ordering::SeqCst) {
        return None;
    }
    // The cached statements were allocated by the script (and counted
    // against its memory); the page cache is SQLite's memory.
    c.flush_prepared_statement_cache();
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

/// Installs what belongs to the script on a connection, new or kept: the
/// progress handler and `LIKE`/`GLOB` with its deadline, counters that start
/// from zero, and the authorizer (which notes, in `tainted`, statements after
/// which the connection is not kept).
fn for_script(c: &Connection, vm: &Vm, tainted: Option<Arc<AtomicBool>>) -> rusqlite::Result<()> {
    // Queries stop when the script's time limit is reached.
    match vm.deadline() {
        Some(deadline) => c.progress_handler(10_000, Some(move || Instant::now() > deadline)),
        None => c.progress_handler(0, None::<fn() -> bool>),
    }
    // SQLite runs one `LIKE` or `GLOB` as a single step, which the progress
    // handler cannot stop (a long text and pattern take minutes): these do.
    like_functions(c, vm.deadline())?;
    counters(c)?;
    // Last: the authorizer refuses the pragmas set before.
    match tainted {
        None => c.authorizer(Some(sqlite_authorizer)),
        Some(tainted) => c.authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
            use rusqlite::hooks::AuthAction;
            // A pragma may change the session; temporary objects stay in it.
            if matches!(
                ctx.action,
                AuthAction::Pragma { .. }
                    | AuthAction::CreateTempIndex { .. }
                    | AuthAction::CreateTempTable { .. }
                    | AuthAction::CreateTempTrigger { .. }
                    | AuthAction::CreateTempView { .. }
            ) {
                tainted.store(true, Ordering::SeqCst);
            }
            sqlite_authorizer(ctx)
        })),
    }
    Ok(())
}

/// `last_insert_rowid()`, `changes()` and `total_changes()` count this
/// script's statements only, as on a new connection.
fn counters(c: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    // SAFETY: the handle is the open connection's; the functions below are
    // owned by the connection, so it is open whenever they run.
    let db = unsafe { c.handle() };
    unsafe { rusqlite::ffi::sqlite3_set_last_insert_rowid(db, 0) };
    let base = c.total_changes();
    let db = db as usize;
    let flags = FunctionFlags::SQLITE_UTF8;
    c.create_scalar_function("total_changes", 0, flags, move |_| {
        // SAFETY: see above.
        let total = unsafe { rusqlite::ffi::sqlite3_total_changes64(db as *mut rusqlite::ffi::sqlite3) } as u64;
        Ok(total.saturating_sub(base) as i64)
    })?;
    c.create_scalar_function("changes", 0, flags, move |_| {
        // SAFETY: see above.
        let db = db as *mut rusqlite::ffi::sqlite3;
        // Until this script changes a row, the last change was another script's.
        if (unsafe { rusqlite::ffi::sqlite3_total_changes64(db) } as u64) == base {
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

/// Whether handle `h` is an SQLite database (otherwise a server connection, see `netdb`).
fn is_sqlite(vm: &Vm, h: &Value) -> bool {
    vm.resources.get(int(h) as usize).and_then(|r| r.as_ref()).is_some_and(|b| b.is::<Db>())
}

fn db_of<'v>(vm: &'v Vm, h: &Value) -> Result<&'v Db, VmError> {
    vm.resources
        .get(int(h) as usize)
        .and_then(|r| r.as_ref())
        .and_then(|b| b.downcast_ref::<Db>())
        .ok_or_else(|| vm.throw("DatabaseError", "the database connection is closed"))
}

fn db_error(vm: &Vm, e: rusqlite::Error) -> VmError {
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
    let db = db_of(vm, &a[0])?;
    // Kept prepared for the next run of the same text (`prepare_cached`).
    let mut stmt = db.conn().prepare_cached(&text).map_err(|e| db.fail(vm, e))?;
    let bound: Vec<Param> = params.iter().map(Param).collect();
    let mut rows = stmt.query(rusqlite::params_from_iter(bound.iter())).map_err(|e| db.fail(vm, e))?;
    // Made once the first row is there: a kept statement is prepared again
    // by its first step when the schema changed, and its columns with it.
    let mut out: Option<ResultRows> = None;
    while let Some(row) = rows.next().map_err(|e| db.fail(vm, e))? {
        let out = out.get_or_insert_with(|| ResultRows::new(row.as_ref().column_names().iter().map(|n| n.to_string())));
        let mut values = Vec::with_capacity(out.names.len());
        for i in 0..out.names.len() {
            let v = match row.get_ref(i).map_err(|e| db.fail(vm, e))? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(i) => Value::Int(i),
                ValueRef::Real(f) => Value::Float(f),
                // A value is accounted before it is copied: a row may hold 32 MB, and
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
    }
    Ok(out.map_or_else(|| Value::list(Vec::new()), ResultRows::into_value))
}

fn execute(vm: &mut Vm, a: &[Value]) -> R {
    if !is_sqlite(vm, &a[0]) {
        return crate::netdb::execute(vm, a);
    }
    let (text, params) = parts(&a[1]);
    let text = render_sql(text, Dialect::Sqlite);
    let db = db_of(vm, &a[0])?;
    let bound: Vec<Param> = params.iter().map(Param).collect();
    let mut stmt = db.conn().prepare_cached(&text).map_err(|e| db.fail(vm, e))?;
    let before = db.conn().total_changes();
    let n = stmt.execute(rusqlite::params_from_iter(bound.iter())).map_err(|e| db.fail(vm, e))?;
    // A statement that changes no row (`CREATE TABLE`, `BEGIN`...) leaves
    // SQLite's count of the last change as it was, which on a kept connection
    // was another script's (found by the 1.0.0 audit): it changed 0 rows.
    let n = if db.conn().total_changes() == before { 0 } else { n };
    Ok(Value::Int(n as i64))
}

fn open(vm: &mut Vm, a: &[Value]) -> R {
    let rel = s(&a[0]).to_string();
    if rel == ":memory:" {
        return open_memory(vm);
    }
    let (dir, name) = crate::sys::resolve_path(vm, &rel, true)?;
    let file_error = |vm: &Vm, e| crate::sys::fs_error(vm, &rel, "", e);
    // A kept connection is used only for the very file the name holds now,
    // checked as before an open (see "Connections to database files").
    check_journals(&dir, &name).map_err(|e| file_error(vm, e))?;
    let now = dir.plain_file_id(&name).map_err(|e| file_error(vm, e))?;
    let key = dir.path().join(&name);
    let (c, id) = match now.and_then(|id| take(&key, id).map(|c| (c, id))) {
        Some(kept) => kept,
        None => {
            let (c, id) = open_in(&dir, &name).map_err(|e| match e {
                OpenError::File(_, e) => file_error(vm, e),
                OpenError::NoFileSystem => vm.throw("DatabaseError", NO_FILE_SYSTEM),
                OpenError::Sqlite(e) => db_error(vm, e),
            })?;
            harden(&c).map_err(|e| db_error(vm, e))?;
            (c, id)
        }
    };
    let tainted = Arc::new(AtomicBool::new(false));
    for_script(&c, vm, Some(tainted.clone())).map_err(|e| db_error(vm, e))?;
    let lease = Lease { key, id, tainted };
    let db = Db { conn: Some(c), _dir: Some(dir), depth: Cell::new(0), pages: 0, pooled: Some(lease) };
    vm.resources.push(Some(Box::new(db)));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
}

/// `Database::open(":memory:")`: a database of the script's own, never kept.
fn open_memory(vm: &mut Vm) -> R {
    let c = Connection::open_in_memory().map_err(|e| db_error(vm, e))?;
    harden(&c).map_err(|e| db_error(vm, e))?;
    // The in-memory databases of a script share its memory limit (found by
    // the 1.0 audit: each one had all of it, and SQLite's memory is not the VM's).
    let total = vm.memory_limit().unwrap_or(128 << 20) / 4096;
    let used: usize = vm.resources.iter().flatten().filter_map(|r| r.downcast_ref::<Db>()).map(|d| d.pages).sum();
    let pages = total.saturating_sub(used);
    if pages < 16 {
        return Err(
            vm.throw("DatabaseError", "the in-memory databases of this script already have its whole memory limit")
        );
    }
    c.execute_batch(&format!("PRAGMA page_size = 4096; PRAGMA max_page_count = {pages}"))
        .map_err(|e| db_error(vm, e))?;
    for_script(&c, vm, None).map_err(|e| db_error(vm, e))?;
    vm.resources.push(Some(Box::new(Db { conn: Some(c), _dir: None, depth: Cell::new(0), pages, pooled: None })));
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
    c.busy_timeout(Duration::from_secs(5))?;
    // Defensive mode: no writes to the schema or the shadow tables of virtual
    // tables, which could corrupt the database file.
    c.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    // Prevent `ATTACH 'other.db'` from reaching files outside the permissions.
    c.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    // Single strings/blobs and statements are bounded (e.g. `zeroblob(1e9)`).
    c.set_limit(Limit::SQLITE_LIMIT_LENGTH, 32 * 1024 * 1024)?;
    c.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 1024 * 1024)?;
    // A process-wide cap on SQLite's own memory (set by the first connection;
    // no connection can change it, see `sqlite_authorizer`), and per
    // connection a small page cache. An in-memory database lives entirely in
    // memory, so it is bounded by the script's memory limit.
    if !HEAP_LIMIT_SET.load(std::sync::atomic::Ordering::Acquire) {
        c.execute_batch("PRAGMA hard_heap_limit = 536870912")?;
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
    let (c, _) = open_in(&dir, &name).map_err(|e| match e {
        OpenError::File(suffix, e) => format!("{}{suffix}: {}", rel.display(), crate::safefs::describe(&e)),
        OpenError::NoFileSystem => NO_FILE_SYSTEM.to_string(),
        OpenError::Sqlite(e) => format!("{}: {e}", rel.display()),
    })?;
    harden(&c).map_err(|e| e.to_string())?;
    // Last: the authorizer refuses the pragmas set above.
    c.authorizer(Some(sqlite_authorizer));
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

/// Replaces SQLite's `like()` (two and three arguments) and `glob()` with
/// versions that stop when the script's time limit passes.
fn like_functions(c: &Connection, deadline: Option<std::time::Instant>) -> rusqlite::Result<()> {
    use rusqlite::functions::{Context, FunctionFlags};
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    fn text(ctx: &Context, i: usize) -> rusqlite::Result<Option<String>> {
        Ok(match ctx.get_raw(i) {
            ValueRef::Null => None,
            ValueRef::Text(t) | ValueRef::Blob(t) => Some(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Integer(i) => Some(i.to_string()),
            ValueRef::Real(f) => Some(real_text(f)),
        })
    }
    let run = move |ctx: &Context, glob: bool| -> rusqlite::Result<Option<bool>> {
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
    c.create_scalar_function("like", 2, flags, move |ctx| run(ctx, false))?;
    c.create_scalar_function("like", 3, flags, move |ctx| run(ctx, false))?;
    c.create_scalar_function("glob", 2, flags, move |ctx| run(ctx, true))?;

    // SQLite's `instr()`, `replace()` and `trim(X, Y)` search naively (text × pattern
    // in one step, minutes on long values): these search in linear time.
    let text_arg = |ctx: &Context, i: usize| -> rusqlite::Result<Option<String>> { text(ctx, i) };
    c.create_scalar_function("instr", 2, flags, move |ctx| {
        let (Some(h), Some(n)) = (text_arg(ctx, 0)?, text_arg(ctx, 1)?) else { return Ok(None) };
        Ok(Some(h.find(n.as_str()).map_or(0, |b| h[..b].chars().count() as i64 + 1)))
    })?;
    c.create_scalar_function("replace", 3, flags, move |ctx| {
        let (Some(x), Some(y), Some(z)) = (text_arg(ctx, 0)?, text_arg(ctx, 1)?, text_arg(ctx, 2)?) else {
            return Ok(None);
        };
        if y.is_empty() {
            return Ok(Some(x));
        }
        let grows = z.len().saturating_sub(y.len());
        if grows > 0 && x.len().saturating_add(x.matches(y.as_str()).count().saturating_mul(grows)) > MAX_TEXT {
            return Err(rusqlite::Error::UserFunctionError("string or blob too big".into()));
        }
        Ok(Some(x.replace(y.as_str(), &z)))
    })?;
    for (name, left, right) in [("trim", true, true), ("ltrim", true, false), ("rtrim", false, true)] {
        c.create_scalar_function(name, 2, flags, move |ctx| {
            let (Some(x), Some(y)) = (text_arg(ctx, 0)?, text_arg(ctx, 1)?) else { return Ok(None) };
            let set: std::collections::HashSet<char> = y.chars().collect();
            let mut t = x.as_str();
            if left {
                t = t.trim_start_matches(|c| set.contains(&c));
            }
            if right {
                t = t.trim_end_matches(|c| set.contains(&c));
            }
            Ok(Some(t.to_string()))
        })?;
    }
    Ok(())
}

/// The longest text or blob SQLite allows here (`SQLITE_LIMIT_LENGTH`).
const MAX_TEXT: usize = 32 * 1024 * 1024;

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

fn is_identifier(t: &str) -> bool {
    let mut chars = t.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn lookup(name: &str) -> Option<BuiltinFn> {
    let f: BuiltinFn = match name {
        "__db_open" => open,
        "__db_query" => query,
        "__db_execute" => execute,
        "__db_connect" => crate::netdb::connect,
        "__redis_connect" => crate::netdb::connect_redis,
        "__redis" => crate::netdb::redis_command,
        "__redis_start" => crate::netdb::start_redis,
        "__db_start" => |vm, a| {
            if !is_sqlite(vm, &a[0]) {
                return crate::netdb::start_query(vm, a);
            }
            // SQLite runs in the script's thread: the statement is done when it returns.
            let r = if matches!(a[2], Value::Bool(true)) { execute(vm, a) } else { query(vm, a) };
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
            if !is_sqlite(vm, &a[0]) {
                return crate::netdb::raw(vm, a);
            }
            let db = db_of(vm, &a[0])?;
            let (sqls, depth) = transaction_sql(cmd, db.depth.get());
            if cmd == "ROLLBACK" {
                db.depth.set(depth);
            }
            for sql in &sqls {
                db.conn().execute_batch(sql).map_err(|e| db.fail(vm, e))?;
            }
            db.depth.set(depth);
            Ok(Value::Null)
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
