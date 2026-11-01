//! SQLite access. Queries are `Sql` values (`sql"..."` literals), which keep
//! interpolated values as bound parameters, so SQL injection cannot happen.
//! At runtime an `Sql` value is the list `[query text, list of parameters]`.

use crate::{int, s};
use indexmap::IndexMap;
use phenix_vm::{BuiltinFn, Key, Value, Vm, VmError};
use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::{Connection, OpenFlags};
use std::rc::Rc;
use std::time::Duration;

type R = Result<Value, VmError>;

fn parts(v: &Value) -> (&str, &[Value]) {
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
    let (text, params) = parts(&a[1]);
    let c = conn(vm, &a[0])?;
    let mut stmt = c.prepare(text).map_err(|e| db_error(vm, e))?;
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
                ValueRef::Text(t) => Value::str(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => Value::str(String::from_utf8_lossy(b).into_owned()),
            };
            m.insert(Key::Str(n.clone()), v);
        }
        out.push(Value::map(m));
        if out.len() % 256 == 0 {
            vm.check_memory()?;
        }
    }
    Ok(Value::list(out))
}

fn execute(vm: &mut Vm, a: &[Value]) -> R {
    let (text, params) = parts(&a[1]);
    let c = conn(vm, &a[0])?;
    let bound: Vec<Param> = params.iter().map(Param).collect();
    let n = c.execute(text, rusqlite::params_from_iter(bound.iter())).map_err(|e| db_error(vm, e))?;
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
        // after opening. The directory itself cannot change meanwhile.
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
        let c = Connection::open_with_flags(dir.path().join(&name), flags).map_err(|e| db_error(vm, e))?;
        check(vm)?;
        (c, Some(dir))
    };
    c.busy_timeout(Duration::from_secs(5)).map_err(|e| db_error(vm, e))?;
    use rusqlite::limits::Limit;
    // Prevent `ATTACH 'other.db'` from reaching files outside the permissions.
    c.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0).map_err(|e| db_error(vm, e))?;
    // Single strings/blobs and statements are bounded (e.g. `zeroblob(1e9)`).
    c.set_limit(Limit::SQLITE_LIMIT_LENGTH, 32 * 1024 * 1024).map_err(|e| db_error(vm, e))?;
    c.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 1024 * 1024).map_err(|e| db_error(vm, e))?;
    // A process-wide cap on SQLite's own memory.
    c.execute_batch("PRAGMA hard_heap_limit = 536870912").map_err(|e| db_error(vm, e))?;
    // Queries stop when the script's time limit is reached.
    if let Some(deadline) = vm.deadline() {
        c.progress_handler(10_000, Some(move || std::time::Instant::now() > deadline));
    }
    // Pragmas that change process-wide state or touch other files are refused.
    c.authorizer(Some(|ctx: rusqlite::hooks::AuthContext<'_>| {
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
                ];
                if blocked.contains(&name.as_str()) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }
            _ => Authorization::Allow,
        }
    }));
    vm.resources.push(Some(Box::new(Db { conn: c, _dir: dir })));
    Ok(Value::Int(vm.resources.len() as i64 - 1))
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
        "__db_last_id" => |vm, a| Ok(Value::Int(conn(vm, &a[0])?.last_insert_rowid())),
        "__db_raw" => |vm, a| {
            // Only BEGIN / COMMIT / ROLLBACK, used by Database::transaction().
            let cmd = s(&a[1]);
            if !matches!(cmd, "BEGIN" | "COMMIT" | "ROLLBACK") {
                return Err(VmError::Fatal(format!("__db_raw: unexpected statement {cmd}")));
            }
            conn(vm, &a[0])?.execute_batch(cmd).map_err(|e| db_error(vm, e))?;
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
            Ok(Value::list(vec![Value::str(format!("\"{t}\"")), Value::list(Vec::new())]))
        },
        _ => return None,
    };
    Some(f)
}
