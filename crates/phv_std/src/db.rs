//! SQLite access. Queries are `Sql` values (`sql"..."` literals), which keep
//! interpolated values as bound parameters, so SQL injection cannot happen.
//! At runtime an `Sql` value is the list `[query text, list of parameters]`.

use crate::{int, s};
use indexmap::IndexMap;
use phv_vm::{BuiltinFn, Key, Value, Vm, VmError};
use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags};
use std::rc::Rc;
use std::time::Duration;

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
    conn: Connection,
    _dir: Option<crate::safefs::DirGuard>,
    /// How many `transaction()` calls are open (inner ones are savepoints).
    depth: std::cell::Cell<u32>,
    /// The pages an in-memory database may use (0 for a file).
    pages: usize,
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

fn conn<'v>(vm: &'v Vm, h: &Value) -> Result<&'v Connection, VmError> {
    vm.resources
        .get(int(h) as usize)
        .and_then(|r| r.as_ref())
        .and_then(|b| b.downcast_ref::<Db>())
        .map(|d| &d.conn)
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

fn query(vm: &mut Vm, a: &[Value]) -> R {
    if !is_sqlite(vm, &a[0]) {
        return crate::netdb::query(vm, a);
    }
    let (text, params) = parts(&a[1]);
    let text = render_sql(text, Dialect::Sqlite);
    let c = conn(vm, &a[0])?;
    let mut stmt = c.prepare(&text).map_err(|e| db_error(vm, e))?;
    let names: Vec<Rc<String>> = stmt.column_names().iter().map(|n| Rc::new(n.to_string())).collect();
    let bound: Vec<Param> = params.iter().map(Param).collect();
    let mut rows = stmt.query(rusqlite::params_from_iter(bound.iter())).map_err(|e| db_error(vm, e))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| db_error(vm, e))? {
        let mut m = IndexMap::with_capacity(names.len());
        for (i, n) in names.iter().enumerate() {
            let v = match row.get_ref(i).map_err(|e| db_error(vm, e))? {
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
            m.insert(Key::Str(n.clone()), v);
        }
        out.push(Value::map(m));
        vm.check_memory()?;
    }
    Ok(Value::list(out))
}

fn execute(vm: &mut Vm, a: &[Value]) -> R {
    if !is_sqlite(vm, &a[0]) {
        return crate::netdb::execute(vm, a);
    }
    let (text, params) = parts(&a[1]);
    let text = render_sql(text, Dialect::Sqlite);
    let c = conn(vm, &a[0])?;
    let bound: Vec<Param> = params.iter().map(Param).collect();
    let n = c.execute(&text, rusqlite::params_from_iter(bound.iter())).map_err(|e| db_error(vm, e))?;
    Ok(Value::Int(n as i64))
}

fn open(vm: &mut Vm, a: &[Value]) -> R {
    let rel = s(&a[0]).to_string();
    let (c, dir) = if rel == ":memory:" {
        (Connection::open_in_memory().map_err(|e| db_error(vm, e))?, None)
    } else {
        let (dir, name) = crate::sys::resolve_path(vm, &rel, true)?;
        // SQLite opens the database and its journal files by name, so each must
        // be missing or a plain file (no link, no second hard link) before and
        // after opening. The directory itself cannot change meanwhile. Files
        // SQLite opens later (a journal per transaction) are checked by `vfs`.
        let check = |vm: &Vm| -> Result<(), VmError> {
            for suffix in ["", "-journal", "-wal", "-shm"] {
                let mut n = name.clone();
                n.push(suffix);
                dir.check_plain_or_missing(&n).map_err(|e| crate::sys::fs_error(vm, &rel, "", e))?;
            }
            Ok(())
        };
        check(vm)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        if !vfs::register() {
            return Err(vm.throw("DatabaseError", "SQLite has no file system support"));
        }
        let c = Connection::open_with_flags_and_vfs(dir.path().join(&name), flags, vfs::NAME)
            .map_err(|e| db_error(vm, e))?;
        check(vm)?;
        (c, Some(dir))
    };
    harden(&c).map_err(|e| db_error(vm, e))?;
    // The in-memory databases of a script share its memory limit (found by
    // the 1.0 audit: each one had all of it, and SQLite's memory is not the VM's).
    let mut pages = 0;
    if dir.is_none() {
        let total = vm.memory_limit().unwrap_or(128 << 20) / 4096;
        let used: usize = vm.resources.iter().flatten().filter_map(|r| r.downcast_ref::<Db>()).map(|d| d.pages).sum();
        pages = total.saturating_sub(used);
        if pages < 16 {
            return Err(
                vm.throw("DatabaseError", "the in-memory databases of this script already have its whole memory limit")
            );
        }
        c.execute_batch(&format!("PRAGMA page_size = 4096; PRAGMA max_page_count = {pages}"))
            .map_err(|e| db_error(vm, e))?;
    }
    // Queries stop when the script's time limit is reached.
    if let Some(deadline) = vm.deadline() {
        c.progress_handler(10_000, Some(move || std::time::Instant::now() > deadline));
    }
    // SQLite runs one `LIKE` or `GLOB` as a single step, which the progress
    // handler cannot stop (a long text and pattern take minutes): these do.
    like_functions(&c, vm.deadline()).map_err(|e| db_error(vm, e))?;
    // Last: the authorizer refuses the pragmas set above.
    c.authorizer(Some(sqlite_authorizer));
    vm.resources.push(Some(Box::new(Db { conn: c, _dir: dir, depth: std::cell::Cell::new(0), pages })));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
}

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
    // A process-wide cap on SQLite's own memory, and per connection a small
    // page cache. An in-memory database lives entirely in memory, so it is
    // bounded by the script's memory limit.
    c.execute_batch("PRAGMA hard_heap_limit = 536870912; PRAGMA cache_size = -2000")?;
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
    let check = || -> Result<(), String> {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let mut n = name.clone();
            n.push(suffix);
            dir.check_plain_or_missing(&n)
                .map_err(|e| format!("{}{suffix}: {}", rel.display(), crate::safefs::describe(&e)))?;
        }
        Ok(())
    };
    check()?;
    if !vfs::register() {
        return Err("SQLite has no file system support".into());
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let c = Connection::open_with_flags_and_vfs(dir.path().join(&name), flags, vfs::NAME)
        .map_err(|e| format!("{}: {e}", rel.display()))?;
    check()?;
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
            Ok(Value::Int(conn(vm, &a[0])?.last_insert_rowid()))
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
            let db = vm
                .resources
                .get(int(&a[0]) as usize)
                .and_then(|r| r.as_ref())
                .and_then(|b| b.downcast_ref::<Db>())
                .ok_or_else(|| vm.throw("DatabaseError", "the database connection is closed"))?;
            let (sqls, depth) = transaction_sql(cmd, db.depth.get());
            if cmd == "ROLLBACK" {
                db.depth.set(depth);
            }
            for sql in &sqls {
                db.conn.execute_batch(sql).map_err(|e| db_error(vm, e))?;
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
