//! How much of a long value a statement may read between two looks at the
//! deadline.
//!
//! SQLite looks at the deadline (its progress handler) only where its program
//! jumps back, and some single steps read a whole value: turning a text into
//! a number (`b + 0`, `CAST`, `NOT b`), comparing two texts, `substr()`,
//! `length()`, `sum()`. Such a step on a value of 16 MB takes up to about 30
//! ms, allocates nothing, and is not stopped. A statement can be written, or
//! made by expanding a view or a `WITH` table many times over, so that one row
//! runs tens of thousands of them with no jump between (found by the 1.2.0
//! audit: minutes under a 1 second limit).
//!
//! So before a script's statement first runs, its program is read (`EXPLAIN`)
//! and its steps are weighed: a step that may turn a whole value into a number
//! or read it slowly (`NOCASE`, most functions) weighs 16, 32 when it does so
//! with two operands (`b * b`), one that copies it (`memcpy`) 2, one that
//! compares it (`memcmp`) 1, and the others nothing. PHV's own functions look
//! at the deadline themselves, and functions that allocate their result look
//! at it through the allocator (`mem`): they weigh nothing. A statement that
//! weighs more than `MAX_WEIGHT` slow reads is refused.
//!
//! Every step counts, also in loops: between two looks at the deadline a loop
//! runs at most `PROGRESS_STEPS` steps, so it reads no more than a statement
//! of that weight would.
//!
//! A weight is kept per process (`cached`, `remember`), by the database file,
//! the schema the program was compiled against and the statement's text, so
//! that the connections of one file (a pool's, or a new one after the kept
//! one was closed) do not weigh the same texts again.

use crate::safefs::FileId;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

/// The SQLite the tables below were written for (the bundled one). The steps
/// are SQLite's opcodes, which a release may add to, rename or change the
/// meaning of, so a build with another SQLite fails the test
/// `weights_are_for_this_sqlite`. On an upgrade: read the opcode list of the
/// new version at <https://sqlite.org/opcode.html>, put every opcode in the
/// class it belongs to (`SQLITE_OPCODES` in the tests lists them all), run the
/// corpus test (`every_opcode_is_classed`), then change this.
#[cfg(test)]
const SQLITE_VERSION: &str = "3.54.1";

/// The most a statement may weigh, in slow reads of a whole value. At 16 MB a
/// value, 128 of them take about 3.5 seconds (measured, the slowest kind);
/// ordinary statements weigh up to about 10, and 30 columns of arithmetic 120.
pub(crate) const MAX_WEIGHT: u32 = 128;

/// Steps that read no value, or only a bounded part of one.
const FREE: &[&str] = &[
    "Abortable",
    "AddImm",
    "AutoCommit",
    "BeginSubrtn",
    "Blob",
    "ClrSubtype",
    "Close",
    "CollSeq",
    "Column",
    "ColumnsUsed",
    "Count",
    "CursorHint",
    "CursorLock",
    "CursorUnlock",
    "DecrJumpZero",
    "DeferredSeek",
    "Delete",
    "ElseEq",
    "EndCoroutine",
    "Expire",
    "Explain",
    "FinishSeek",
    "FkCheck",
    "FkCounter",
    "FkIfZero",
    "GetSubtype",
    "Gosub",
    "Goto",
    "Halt",
    "HaltIfNull",
    "IdxRowid",
    "IfEmpty",
    "IfNotZero",
    "IfNullRow",
    "IfPos",
    "IfSizeBetween",
    "Init",
    "InitCoroutine",
    "Int64",
    "IntCopy",
    "Integer",
    "IsNull",
    "IsType",
    "Jump",
    "Last",
    "MaxPgcnt",
    "MemMax",
    "Move",
    "NewRowid",
    "Next",
    "Noop",
    "NotNull",
    "Null",
    "NullRow",
    "Offset",
    "OffsetLimit",
    "Once",
    "OpenAutoindex",
    "OpenDup",
    "OpenEphemeral",
    "OpenPseudo",
    "OpenRead",
    "OpenWrite",
    "Pagecount",
    "Param",
    "Permutation",
    "Prev",
    "Program",
    "ReadCookie",
    "Real",
    "RealAffinity",
    "ReleaseReg",
    "ReopenIdx",
    "ResetCount",
    "ResultRow",
    "Return",
    "Rewind",
    "RowCell",
    "RowData",
    "RowSetAdd",
    "RowSetRead",
    "RowSetTest",
    "Rowid",
    "SCopy",
    "Savepoint",
    "SeekEnd",
    "SeekHit",
    "Sequence",
    "SequenceTest",
    "SetCookie",
    "SetSubtype",
    "SoftNull",
    "Sort",
    "SorterData",
    "SorterInsert",
    "SorterNext",
    "SorterOpen",
    "SorterSort",
    "String",
    "String8",
    "TableLock",
    "Trace",
    "Transaction",
    "VBegin",
    "VColumn",
    "VFilter",
    "VInitIn",
    "VNext",
    "VOpen",
    "Variable",
    "Yield",
    "ZeroOrNull",
];

/// Steps that compare a value (`memcmp`).
const COMPARE: &[&str] = &[
    "Compare",
    "Filter",
    "FilterAdd",
    "Found",
    "IFindKey",
    "IdxDelete",
    "IdxGE",
    "IdxGT",
    "IdxLE",
    "IdxLT",
    "IfNoHope",
    "NoConflict",
    "NotFound",
    "SeekGE",
    "SeekGT",
    "SeekLE",
    "SeekLT",
    "SeekScan",
    "SorterCompare",
];

/// Steps that copy a value (`memcpy`, twice as long as a comparison).
const COPY: &[&str] = &["Concat", "Copy", "IdxInsert", "Insert", "MakeRecord"];

/// Steps that compare two values, by `P5` with a numeric affinity (both
/// turned into numbers), with `NOCASE` (one read of each), or byte by byte.
const COMPARISONS: &[&str] = &["Eq", "Ge", "Gt", "Le", "Lt", "Ne"];

/// Steps that apply affinities (`P4`) to values: a numeric one turns a text
/// into a number.
const AFFINITIES: &[&str] = &["Affinity", "MakeRecord"];

/// Steps that call a function (`P4` names it): the function's weight.
const FUNCTIONS: &[&str] = &["AggFinal", "AggInverse", "AggStep", "AggStep1", "AggValue", "Function", "PureFunc"];

/// Steps that turn both operands into numbers.
const BINARY: &[&str] =
    &["Add", "And", "BitAnd", "BitOr", "Divide", "Multiply", "Or", "Remainder", "ShiftLeft", "ShiftRight", "Subtract"];

/// The rest of SQLite's steps. Some turn one value into a number (`Cast`,
/// `Not`, `If`, `MustBeInt`...); the others (`VACUUM`, `ANALYZE`, the schema
/// and virtual table steps) are rare and not many in a program. They weigh a
/// slow read, as a step this module does not know does (`weight`).
const SLOW_STEPS: &[&str] = &[
    "BitNot",
    "Cast",
    "Checkpoint",
    "Clear",
    "CreateBtree",
    "Destroy",
    "DropIndex",
    "DropTable",
    "DropTrigger",
    "If",
    "IfNot",
    "IncrVacuum",
    "IntegrityCk",
    "IsTrue",
    "JournalMode",
    "LoadAnalysis",
    "MustBeInt",
    "Not",
    "NotExists",
    "ParseSchema",
    "ResetSorter",
    "SeekRowid",
    "SqlExec",
    "TypeCheck",
    "VCheck",
    "VCreate",
    "VDestroy",
    "VRename",
    "VUpdate",
    "Vacuum",
];

/// Functions that weigh nothing: PHV's own (they look at the deadline), those
/// that allocate their result (the allocator looks at it), and those that
/// read a bounded part of their arguments.
const FREE_FUNCTIONS: &[&str] = &[
    "changes",
    "char",
    "concat",
    "concat_ws",
    "count",
    "cume_dist",
    "dense_rank",
    "format",
    "glob",
    "group_concat",
    "hex",
    "instr",
    "last_insert_rowid",
    "like",
    "likelihood",
    "likely",
    "lower",
    "ntile",
    "octet_length",
    "percent_rank",
    "printf",
    "quote",
    "random",
    "randomblob",
    "rank",
    "replace",
    "row_number",
    "sqlite_source_id",
    "sqlite_version",
    "string_agg",
    "total_changes",
    "typeof",
    "unhex",
    "unicode",
    "unistr",
    "unistr_quote",
    "unlikely",
    "upper",
    "zeroblob",
];

/// Functions that compare their arguments.
const COMPARE_FUNCTIONS: &[&str] =
    &["date", "datetime", "julianday", "nullif", "strftime", "time", "timediff", "unixepoch"];

/// Functions that return a copy of an argument.
const COPY_FUNCTIONS: &[&str] = &["first_value", "lag", "last_value", "lead", "max", "min", "nth_value"];

/// The weights of a step that may turn a whole value into a number (about 2
/// ns a byte), copy it, and compare it.
const SLOW: u32 = 16;
const COPYING: u32 = 2;
const COMPARING: u32 = 1;

/// One step of a program, as `EXPLAIN` lists it.
struct Step {
    op: String,
    p1: i64,
    p2: i64,
    p3: i64,
    p4: String,
    p5: i64,
}

/// The schema a program was compiled against: its `Transaction` step carries
/// the schema cookie (`P3`) of the database it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Schema {
    /// The program reads no table: the same under any schema.
    None,
    /// The main database's, at this schema cookie (`PRAGMA schema_version`).
    Main(i64),
    /// Another database's too (a temporary table): not kept.
    Other,
}

/// The weight of the statement `text` (prepared before on `c`, so it is
/// valid, and not itself an `EXPLAIN`) in slow reads, rounded up, and the
/// schema its program was compiled against. The listing includes the programs
/// of the triggers it fires.
pub(super) fn weigh(c: &Connection, text: &str) -> rusqlite::Result<(u32, Schema)> {
    WEIGHED.fetch_add(1, Ordering::SeqCst);
    let mut st = c.prepare(&format!("EXPLAIN {text}"))?;
    let mut rows = st.raw_query();
    let mut steps = Vec::new();
    while let Some(r) = rows.next()? {
        let int = |i| match r.get_ref(i) {
            Ok(ValueRef::Integer(n)) => n,
            _ => 0,
        };
        let text = |i| match r.get_ref(i) {
            Ok(ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
            _ => String::new(),
        };
        steps.push(Step { op: text(1), p1: int(2), p2: int(3), p3: int(4), p4: text(5), p5: int(6) });
    }
    Ok((weight(&steps).div_ceil(SLOW), schema(&steps)))
}

/// The schema the program `steps` was compiled against (see `Schema`): the
/// `Transaction` step of the main database (`P1` 0) checks its cookie `P3`
/// when the program runs (`P5` set), and one of another database is not kept.
fn schema(steps: &[Step]) -> Schema {
    let mut out = Schema::None;
    for s in steps.iter().filter(|s| s.op == "Transaction") {
        match (s.p1, s.p5 != 0, out) {
            (0, true, Schema::None) => out = Schema::Main(s.p3),
            (0, true, Schema::Main(cookie)) if cookie == s.p3 => {}
            _ => return Schema::Other,
        }
    }
    out
}

/// The weight of a program, in sixteenths of a slow read.
fn weight(steps: &[Step]) -> u32 {
    // How many times each parameter is read.
    let mut reads: HashMap<i64, u32> = HashMap::new();
    for s in steps.iter().filter(|s| s.op == "Variable") {
        *reads.entry(s.p1).or_default() += 1;
    }
    let mut sum = 0u32;
    // The register of the last record made of one such parameter.
    let mut param_record: Option<i64> = None;
    for (i, s) in steps.iter().enumerate() {
        let w = match s.op.as_str() {
            op if FREE.contains(&op) => 0,
            op if COMPARISONS.contains(&op) => {
                // A numeric affinity turns the text operands into numbers; NOCASE
                // compares byte by byte.
                if s.p5 & 0x47 >= 0x43 {
                    2 * SLOW
                } else if s.p4.starts_with("NOCASE") {
                    SLOW
                } else {
                    COMPARING
                }
            }
            op if AFFINITIES.contains(&op) => {
                // The affinities of the values (`C`, `D`, `E` are numeric).
                let w = if s.p4.contains(['C', 'D', 'E']) {
                    SLOW
                } else if s.op == "MakeRecord" {
                    COPYING
                } else {
                    0
                };
                // A parameter read once, just before (the values of `IN (?, ?, ...)`):
                // the reads of all of them are bounded by the bytes bound.
                let once = i > 0 && s.p2 == 1 && {
                    let p = &steps[i - 1];
                    p.op == "Variable" && p.p2 == s.p1 && reads.get(&p.p1) == Some(&1)
                };
                if once {
                    if s.op == "MakeRecord" {
                        param_record = Some(s.p3);
                    }
                    0
                } else {
                    w
                }
            }
            // That record, put in the index of the `IN` values.
            "IdxInsert" if param_record == Some(s.p2) => 0,
            op if FUNCTIONS.contains(&op) => function_weight(&s.p4),
            op if COMPARE.contains(&op) => COMPARING,
            op if COPY.contains(&op) => COPYING,
            // Both operands may be long texts.
            op if BINARY.contains(&op) => 2 * SLOW,
            // The rest may turn a long text into a number; so may a step not
            // known here (a newer SQLite's).
            op if SLOW_STEPS.contains(&op) => SLOW,
            _ => SLOW,
        };
        sum = sum.saturating_add(w);
    }
    sum
}

/// The weight of a call of the function `p4` (`name(arguments)`).
fn function_weight(p4: &str) -> u32 {
    let (name, rest) = p4.split_once('(').unwrap_or((p4, ""));
    let args: u32 = rest.trim_end_matches(')').parse().unwrap_or(0);
    let name = name.to_ascii_lowercase();
    // PHV's `trim(X, Y)`; `trim(X)` is SQLite's.
    if matches!(name.as_str(), "trim" | "ltrim" | "rtrim") {
        return if args == 2 { 0 } else { SLOW };
    }
    let name = name.as_str();
    if FREE_FUNCTIONS.contains(&name) {
        0
    } else if COPY_FUNCTIONS.contains(&name) {
        COPYING
    } else if COMPARE_FUNCTIONS.contains(&name) || name.starts_with("json") || name.starts_with("->") {
        COMPARING
    } else {
        SLOW
    }
}

// ----- Weights kept per process -----------------------------------------------

/// `EXPLAIN` runs so far (for tests).
static WEIGHED: AtomicU64 = AtomicU64::new(0);

/// `ANALYZE` statements compiled so far, by any connection of the process.
/// `ANALYZE` changes the plans of later programs without changing the schema
/// cookie (the first one, which makes `sqlite_stat1`, does), so the weights
/// kept before it are not used after it.
static ANALYZED: AtomicU64 = AtomicU64::new(0);

/// Counts an `ANALYZE` (or `PRAGMA optimize`) being compiled.
pub(super) fn note_analyze() {
    ANALYZED.fetch_add(1, Ordering::SeqCst);
}

/// The `ANALYZE` count now (read before a statement is weighed, see `remember`).
pub(super) fn analyzed() -> u64 {
    ANALYZED.load(Ordering::SeqCst)
}

/// The database file a connection has open, as the key of its weights: its
/// identity, and when it was made, where the file system says (a Unix file
/// system may give a deleted file's inode to a new file; Windows file
/// indexes carry a sequence number). Where the file system does not say
/// (no birth time), the opening of the file stands in: each connection then
/// has weights of its own, which it keeps across the scripts that take it,
/// and two files that had one inode in turn cannot share them (found by
/// the 1.2.1 audit).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Database {
    id: FileId,
    created: Option<SystemTime>,
    /// 0 when `created` is known; else a number no other open in this
    /// process has.
    opened: u64,
}

impl Database {
    /// The key for the file `id`, made at `created` where the file system
    /// says so.
    pub(super) fn new(id: FileId, created: Option<SystemTime>) -> Database {
        static OPENS: AtomicU64 = AtomicU64::new(0);
        let opened = if created.is_some() { 0 } else { OPENS.fetch_add(1, Ordering::Relaxed) + 1 };
        Database { id, created, opened }
    }
}

/// What a weight is kept by: the file, the schema cookie the program was
/// compiled against, the `ANALYZE` count then, and the statement's text.
#[derive(PartialEq, Eq, Hash)]
struct Key {
    db: Database,
    schema: i64,
    analyzed: u64,
    text: String,
}

/// The weights kept: at most `KEPT_WEIGHTS`, the oldest dropped first.
#[derive(Default)]
struct Weights {
    map: HashMap<Arc<Key>, u32>,
    order: VecDeque<Arc<Key>>,
}

/// The most weights kept per process, and the longest text kept (the
/// program of a longer one is weighed on each connection; a text of this
/// size has a few hundred steps, and the kept weights hold at most 16 MB
/// of text).
const KEPT_WEIGHTS: usize = 4096;
const KEPT_TEXT: usize = 4096;

static WEIGHTS: LazyLock<Mutex<Weights>> = LazyLock::new(Default::default);

fn weights() -> std::sync::MutexGuard<'static, Weights> {
    WEIGHTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The weight kept for `text` on `db` at the schema cookie `schema` and the
/// `ANALYZE` count `analyzed` (now), if any.
pub(super) fn cached(db: Database, schema: i64, analyzed: u64, text: &str) -> Option<u32> {
    if text.len() > KEPT_TEXT {
        return None;
    }
    let key = Key { db, schema, analyzed, text: text.to_string() };
    weights().map.get(&key).copied()
}

/// Keeps `weight` for `text` on `db`, weighed against the schema cookie
/// `schema` (`weigh` said so, see `Schema`) with `analyzed` the `ANALYZE`
/// count read before it ran: an `ANALYZE` compiled meanwhile keeps it from
/// being used, as a schema change does.
pub(super) fn remember(db: Database, schema: i64, analyzed: u64, text: &str, weight: u32) {
    if text.len() > KEPT_TEXT {
        return;
    }
    let key = Arc::new(Key { db, schema, analyzed, text: text.to_string() });
    let mut w = weights();
    if w.map.contains_key(&key) {
        return;
    }
    while w.order.len() >= KEPT_WEIGHTS {
        if let Some(old) = w.order.pop_front() {
            w.map.remove(&old);
        }
    }
    w.order.push_back(key.clone());
    w.map.insert(key, weight);
}

/// `EXPLAIN` runs so far, and the weights kept (for tests).
pub(super) fn weight_stats() -> (u64, usize) {
    (WEIGHED.load(Ordering::SeqCst), weights().map.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory database with `setup` run on it.
    fn memory(setup: &str) -> Connection {
        // The allocator is installed before SQLite starts (see `mem::init`).
        super::super::mem::init();
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(setup).unwrap();
        c
    }

    fn weigh_sql(setup: &str, sql: &str) -> u32 {
        weigh(&memory(setup), sql).unwrap().0
    }

    /// Every opcode of SQLite `SQLITE_VERSION`: the `#define OP_*` lines of
    /// the bundled `sqlite3.c` (libsqlite3-sys 0.35.0), as
    /// <https://sqlite.org/opcode.html> lists them for 3.50.2. On an upgrade,
    /// read that page for the new version and put each new opcode here and in
    /// its class (see `SQLITE_VERSION`).
    const SQLITE_OPCODES: &[&str] = &[
        "Abortable",
        "Add",
        "AddImm",
        "Affinity",
        "AggFinal",
        "AggInverse",
        "AggStep",
        "AggStep1",
        "AggValue",
        "And",
        "AutoCommit",
        "BeginSubrtn",
        "BitAnd",
        "BitNot",
        "BitOr",
        "Blob",
        "Cast",
        "Checkpoint",
        "Clear",
        "Close",
        "ClrSubtype",
        "CollSeq",
        "Column",
        "ColumnsUsed",
        "Compare",
        "Concat",
        "Copy",
        "Count",
        "CreateBtree",
        "CursorHint",
        "CursorLock",
        "CursorUnlock",
        "DecrJumpZero",
        "DeferredSeek",
        "Delete",
        "Destroy",
        "Divide",
        "DropIndex",
        "DropTable",
        "DropTrigger",
        "ElseEq",
        "EndCoroutine",
        "Eq",
        "Expire",
        "Explain",
        "Filter",
        "FilterAdd",
        "FinishSeek",
        "FkCheck",
        "FkCounter",
        "FkIfZero",
        "Found",
        "Function",
        "Ge",
        "GetSubtype",
        "Gosub",
        "Goto",
        "Gt",
        "Halt",
        "HaltIfNull",
        "IFindKey",
        "IdxDelete",
        "IdxGE",
        "IdxGT",
        "IdxInsert",
        "IdxLE",
        "IdxLT",
        "IdxRowid",
        "If",
        "IfEmpty",
        "IfNoHope",
        "IfNot",
        "IfNotZero",
        "IfNullRow",
        "IfPos",
        "IfSizeBetween",
        "IncrVacuum",
        "Init",
        "InitCoroutine",
        "Insert",
        "Int64",
        "IntCopy",
        "Integer",
        "IntegrityCk",
        "IsNull",
        "IsTrue",
        "IsType",
        "JournalMode",
        "Jump",
        "Last",
        "Le",
        "LoadAnalysis",
        "Lt",
        "MakeRecord",
        "MaxPgcnt",
        "MemMax",
        "Move",
        "Multiply",
        "MustBeInt",
        "Ne",
        "NewRowid",
        "Next",
        "NoConflict",
        "Noop",
        "Not",
        "NotExists",
        "NotFound",
        "NotNull",
        "Null",
        "NullRow",
        "Offset",
        "OffsetLimit",
        "Once",
        "OpenAutoindex",
        "OpenDup",
        "OpenEphemeral",
        "OpenPseudo",
        "OpenRead",
        "OpenWrite",
        "Or",
        "Pagecount",
        "Param",
        "ParseSchema",
        "Permutation",
        "Prev",
        "Program",
        "PureFunc",
        "ReadCookie",
        "Real",
        "RealAffinity",
        "ReleaseReg",
        "Remainder",
        "ReopenIdx",
        "ResetCount",
        "ResetSorter",
        "ResultRow",
        "Return",
        "Rewind",
        "RowCell",
        "RowData",
        "RowSetAdd",
        "RowSetRead",
        "RowSetTest",
        "Rowid",
        "SCopy",
        "Savepoint",
        "SeekEnd",
        "SeekGE",
        "SeekGT",
        "SeekHit",
        "SeekLE",
        "SeekLT",
        "SeekRowid",
        "SeekScan",
        "Sequence",
        "SequenceTest",
        "SetCookie",
        "SetSubtype",
        "ShiftLeft",
        "ShiftRight",
        "SoftNull",
        "Sort",
        "SorterCompare",
        "SorterData",
        "SorterInsert",
        "SorterNext",
        "SorterOpen",
        "SorterSort",
        "SqlExec",
        "String",
        "String8",
        "Subtract",
        "TableLock",
        "Trace",
        "Transaction",
        "TypeCheck",
        "VBegin",
        "VCheck",
        "VColumn",
        "VCreate",
        "VDestroy",
        "VFilter",
        "VInitIn",
        "VNext",
        "VOpen",
        "VRename",
        "VUpdate",
        "Vacuum",
        "Variable",
        "Yield",
        "ZeroOrNull",
    ];

    /// The classes of steps, by name.
    const CLASSES: &[(&str, &[&str])] = &[
        ("FREE", FREE),
        ("COMPARE", COMPARE),
        ("COPY", COPY),
        ("COMPARISONS", COMPARISONS),
        ("AFFINITIES", AFFINITIES),
        ("FUNCTIONS", FUNCTIONS),
        ("BINARY", BINARY),
        ("SLOW_STEPS", SLOW_STEPS),
    ];

    /// Whether `op` is in a class (`MakeRecord` is in two: `AFFINITIES`
    /// decides it, `COPY` says what it is).
    fn classed(op: &str) -> bool {
        CLASSES.iter().any(|(_, ops)| ops.contains(&op))
    }

    /// The tables were written for the bundled SQLite: another one may have
    /// other opcodes, or other meanings for these (see `SQLITE_VERSION`).
    #[test]
    fn weights_are_for_this_sqlite() {
        assert_eq!(
            rusqlite::version(),
            SQLITE_VERSION,
            "SQLite was upgraded: check the opcodes against https://sqlite.org/opcode.html (see SQLITE_VERSION)"
        );
    }

    /// Every name in the tables is an opcode of this SQLite (a typo would
    /// weigh a free step as slow, or go unnoticed), and every opcode of this
    /// SQLite is in one class.
    #[test]
    fn the_classes_are_the_opcodes() {
        for (class, ops) in CLASSES {
            for op in *ops {
                assert!(SQLITE_OPCODES.contains(op), "{class} names {op}, which SQLite {SQLITE_VERSION} has not");
            }
            let mut sorted = ops.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), ops.len(), "{class} names an opcode twice");
        }
        let unclassed: Vec<&str> = SQLITE_OPCODES.iter().copied().filter(|op| !classed(op)).collect();
        assert!(unclassed.is_empty(), "opcodes of SQLite {SQLITE_VERSION} in no class: {unclassed:?}");
    }

    /// The schema the corpus runs against: tables with every affinity and
    /// collation, indexes, a `WITHOUT ROWID` table, a `STRICT` table,
    /// generated columns, foreign keys, views, triggers and a full-text index.
    const CORPUS_SCHEMA: &str = "
        PRAGMA foreign_keys = ON;
        CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE, email TEXT UNIQUE, age INTEGER,
            score REAL, created TEXT, data BLOB, tags TEXT,
            full_name TEXT GENERATED ALWAYS AS (name || ' <' || email || '>') VIRTUAL,
            upper_name TEXT GENERATED ALWAYS AS (upper(name)) STORED);
        CREATE TABLE posts (id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            title TEXT, body TEXT, views INTEGER DEFAULT 0 CHECK (views >= 0), created TEXT);
        CREATE INDEX posts_user ON posts (user_id, created);
        CREATE INDEX posts_title ON posts (title COLLATE NOCASE);
        CREATE INDEX posts_views ON posts (views) WHERE views > 100;
        CREATE TABLE tags (post_id INTEGER, tag TEXT, PRIMARY KEY (post_id, tag)) WITHOUT ROWID;
        CREATE TABLE strict_t (id INTEGER PRIMARY KEY, n INT, t TEXT, a ANY) STRICT;
        CREATE TABLE log (at TEXT, what TEXT);
        CREATE TABLE log2 (at TEXT, what TEXT);
        CREATE TABLE numbers (n INTEGER);
        CREATE VIEW post_counts AS SELECT user_id, count(*) AS n, max(views) AS top FROM posts GROUP BY user_id;
        CREATE TRIGGER posts_log AFTER INSERT ON posts BEGIN
            INSERT INTO log VALUES (datetime('now'), 'post ' || new.id);
            UPDATE users SET score = score + 1 WHERE id = new.user_id;
        END;
        CREATE TRIGGER posts_del BEFORE DELETE ON posts BEGIN DELETE FROM tags WHERE post_id = old.id; END;
        CREATE TRIGGER posts_upd AFTER UPDATE OF views ON posts WHEN new.views > old.views BEGIN
            UPDATE posts SET title = title WHERE id = new.id; INSERT INTO log VALUES ('up', new.id);
        END;
        CREATE TRIGGER counts_upd INSTEAD OF UPDATE ON post_counts BEGIN SELECT RAISE(ABORT, 'read only'); END;
        CREATE VIRTUAL TABLE docs USING fts5(title, body);
        INSERT INTO users (name, email, age, score, created) VALUES ('ann', 'ann@example.com', 30, 1.5, '2026-01-01');
        INSERT INTO posts (user_id, title, body, views, created) VALUES (1, 'Hello', 'world', 3, '2026-01-02');
        INSERT INTO numbers VALUES (1), (2), (3);
    ";

    /// Statements that together use every class of step the tables know
    /// (`every_opcode_is_classed` prints what they produce).
    const CORPUS: &[&str] = &[
        // Joins, ordering, limits, subqueries, `EXISTS`, `IN`.
        "SELECT p.id, p.title, u.name FROM posts p JOIN users u ON u.id = p.user_id WHERE p.created > ? AND u.name LIKE ? ORDER BY p.created DESC LIMIT 20 OFFSET 5",
        "SELECT * FROM posts p LEFT JOIN users u ON u.id = p.user_id LEFT JOIN tags t ON t.post_id = p.id WHERE t.tag IS NULL",
        "SELECT * FROM users u CROSS JOIN numbers n WHERE n.n = u.age % 3",
        "SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id AND p.views > (SELECT avg(views) FROM posts))",
        "SELECT * FROM posts WHERE user_id IN (SELECT id FROM users WHERE age > ?) AND title NOT IN ('a', 'b', 'c')",
        "SELECT * FROM posts WHERE id IN (?, ?, ?, ?, ?, ?, ?, ?)",
        "SELECT * FROM posts WHERE (user_id, created) IN (SELECT user_id, max(created) FROM posts GROUP BY user_id)",
        "SELECT * FROM posts WHERE title = ? COLLATE NOCASE AND body GLOB ? AND created BETWEEN ? AND ?",
        "SELECT * FROM users WHERE name = ? ORDER BY name COLLATE BINARY, created COLLATE RTRIM",
        "SELECT * FROM posts WHERE views > 100 ORDER BY views",
        "SELECT * FROM posts WHERE rowid = ?",
        "SELECT * FROM tags WHERE post_id = ? AND tag > ? ORDER BY tag DESC",
        "SELECT DISTINCT user_id FROM posts ORDER BY 1",
        "SELECT * FROM posts p, (SELECT user_id, count(*) AS c FROM posts GROUP BY user_id) s WHERE s.user_id = p.user_id",
        "SELECT * FROM users WHERE id = ? AND email = ? AND age = ?",
        "SELECT * FROM users INDEXED BY sqlite_autoindex_users_1 WHERE email = ?",
        "SELECT * FROM users NOT INDEXED WHERE email = ?",
        "SELECT * FROM posts WHERE user_id = ? AND created >= ? AND created < ?",
        "SELECT * FROM posts WHERE user_id = ? AND created > ? ORDER BY created DESC",
        "SELECT * FROM posts WHERE user_id = ? AND created < ? ORDER BY created DESC LIMIT 1",
        "SELECT * FROM posts WHERE user_id IN (1, 2, 3) AND created IN (?, ?) ORDER BY created",
        "SELECT * FROM tags WHERE post_id = ? AND tag IN (?, ?, ?)",
        "SELECT * FROM tags WHERE post_id IN (SELECT id FROM posts) AND tag = ?",
        "SELECT * FROM posts WHERE user_id = ? OR title = ?",
        "SELECT * FROM posts a JOIN posts b ON a.user_id = b.user_id AND a.id < b.id",
        "SELECT * FROM log a JOIN log b ON a.what = b.what",
        "SELECT views IN (1, 2, NULL), title IN (SELECT name FROM users), views IS TRUE, views IS NOT FALSE, 9223372036854775807, -9223372036854775808 FROM posts",
        "SELECT user_id, created, count(*) FROM posts GROUP BY created, user_id ORDER BY 2 DESC, 1",
        "SELECT json_array(json(tags)), (SELECT json(tags) FROM users LIMIT 1), json_group_array(json(tags)) FROM users",
        "SELECT sum(views) OVER (ORDER BY created GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE CURRENT ROW), count(*) OVER (ORDER BY views RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM posts",
        "SELECT * FROM docs WHERE rowid IN (?, ?)",
        // Aggregates, `GROUP BY`, `HAVING`, window functions, `CASE`, `CAST`.
        "SELECT strftime('%Y-%m', created) AS m, count(*), sum(views), avg(views), min(views), max(views), total(views), group_concat(title, ', '), string_agg(title, '|') FROM posts WHERE created >= ? GROUP BY m HAVING count(*) > ? ORDER BY m",
        "SELECT count(DISTINCT user_id), sum(CASE WHEN views > 10 THEN 1 ELSE 0 END), CASE views WHEN 0 THEN 'none' WHEN 1 THEN 'one' ELSE 'many' END FROM posts",
        "SELECT id, row_number() OVER w, rank() OVER w, dense_rank() OVER w, percent_rank() OVER w, cume_dist() OVER w, ntile(4) OVER w, lag(views, 1, 0) OVER w, lead(views) OVER w, first_value(title) OVER w, last_value(title) OVER w, nth_value(title, 2) OVER w, sum(views) OVER (PARTITION BY user_id ORDER BY created ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), avg(views) OVER (ORDER BY created RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), max(views) FILTER (WHERE views > 1) OVER () FROM posts WINDOW w AS (PARTITION BY user_id ORDER BY views DESC)",
        "SELECT CAST(views AS TEXT), CAST(title AS INTEGER), CAST(score AS REAL), CAST(data AS BLOB), typeof(data), views + 1, views - 1, views * 2, views / 2, views % 3, views & 1, views | 2, views << 1, views >> 1, ~views, -views, NOT views, views AND score, views OR score, views IS NOT NULL, title IS NOT ?, views ISNULL, title NOTNULL, (views > 1) = (score > 1) FROM posts, users",
        // (The bundled SQLite has no `SQLITE_ENABLE_MATH_FUNCTIONS`: no `sqrt()`, `floor()`...)
        "SELECT abs(views), round(score, 2), random(), randomblob(8), min(views, 3), max(views, score, 1), coalesce(title, body, 'x'), ifnull(title, ''), nullif(views, 0), iif(views > 1, 'a', 'b'), likely(views), unlikely(views), likelihood(views, 0.5), views + 0.5, score / 3.0, -score FROM posts, users",
        "SELECT length(body), substr(body, 1, 3), substring(body, 2), instr(body, 'o'), replace(body, 'o', '0'), upper(title), lower(title), trim(title), ltrim(title, 'x'), rtrim(title), hex(body), unhex('41'), quote(title), char(65, 66), unicode(title), printf('%d %s', views, title), format('%05.2f', score), concat(title, body), concat_ws('-', title, body), zeroblob(4), octet_length(body), title || body, title LIKE 'a%' ESCAPE '\\', body GLOB '*o*' FROM posts, users",
        "SELECT date('now'), time('now', '+1 hour'), datetime(created), julianday(created), unixepoch(created), strftime('%s', created), timediff(created, '2026-01-01') FROM posts",
        "SELECT json_extract(tags, '$.a'), tags -> 'a', tags ->> '$.b[0]', json_array(1, 2), json_object('k', name), json_valid(tags), json_type(tags), json_quote(name), json_insert(tags, '$.c', 1), json_set(tags, '$.c', 2), json_replace(tags, '$.c', 3), json_remove(tags, '$.c'), json_patch(tags, '{}'), json_array_length(tags), json(tags), jsonb(tags), json_group_array(name), json_group_object(name, age) FROM users",
        "SELECT key, value, type FROM users, json_each(users.tags) WHERE users.id = ?",
        "SELECT fullkey, atom FROM json_tree(?)",
        "SELECT * FROM docs WHERE docs MATCH ? ORDER BY rank",
        "SELECT title, bm25(docs), highlight(docs, 0, '<b>', '</b>'), snippet(docs, 1, '<b>', '</b>', '...', 8) FROM docs WHERE docs MATCH 'hello'",
        // Set operations, `WITH`, recursion, `VALUES`.
        "SELECT title FROM posts UNION SELECT name FROM users UNION ALL SELECT email FROM users INTERSECT SELECT upper_name FROM users EXCEPT SELECT full_name FROM users",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 100) SELECT sum(x) FROM c",
        "WITH RECURSIVE tree(id, parent, depth) AS (SELECT id, user_id, 0 FROM posts WHERE user_id = ? UNION ALL SELECT p.id, p.user_id, depth + 1 FROM posts p JOIN tree ON p.user_id = tree.id WHERE depth < 10) SELECT * FROM tree ORDER BY depth",
        "WITH a(x) AS (SELECT views FROM posts), b AS MATERIALIZED (SELECT x * 2 AS y FROM a), c AS NOT MATERIALIZED (SELECT y FROM b) SELECT * FROM c, a WHERE c.y = a.x",
        "SELECT * FROM (VALUES (1, 'a'), (2, 'b'), (3, 'c')) WHERE column1 > ?",
        "SELECT * FROM post_counts WHERE n > ? ORDER BY top DESC",
        "SELECT name, full_name, upper_name FROM users WHERE upper_name = ?",
        "SELECT n FROM numbers WHERE n IN (SELECT n FROM numbers WHERE n % 2 = 0) LIMIT ?",
        "SELECT * FROM users WHERE id > ? LIMIT -1 OFFSET ?",
        // Writes: inserts, `RETURNING`, upserts, updates, deletes, triggers.
        "INSERT INTO users (name, email, age) VALUES (?, ?, ?) RETURNING id, full_name",
        "INSERT INTO users (id, name, email) VALUES (?, ?, ?) ON CONFLICT(email) DO UPDATE SET name = excluded.name WHERE users.age < excluded.age",
        "INSERT INTO users (name, email) VALUES (?, ?) ON CONFLICT DO NOTHING",
        "INSERT OR REPLACE INTO users (id, name, email) VALUES (?, ?, ?)",
        "INSERT OR IGNORE INTO tags (post_id, tag) VALUES (?, ?), (?, ?)",
        "INSERT INTO posts (user_id, title, body, created) SELECT id, name, email, created FROM users WHERE age > ?",
        "INSERT INTO posts DEFAULT VALUES",
        "INSERT INTO strict_t (n, t, a) VALUES (?, ?, ?)",
        "INSERT INTO docs (title, body) VALUES (?, ?)",
        "INSERT INTO log SELECT ?, ? WHERE NOT EXISTS (SELECT 1 FROM log WHERE what = ?)",
        "REPLACE INTO numbers (n) VALUES (?)",
        "UPDATE posts SET views = views + 1, title = upper(title) WHERE id = ? RETURNING views",
        "UPDATE posts SET views = (SELECT max(n) FROM numbers) WHERE user_id IN (SELECT id FROM users WHERE name = ?)",
        "UPDATE posts SET body = u.email FROM users u WHERE u.id = posts.user_id AND u.age > ?",
        "UPDATE OR IGNORE users SET email = ? WHERE id = ?",
        "UPDATE users SET score = score * 0.9 + ?, data = randomblob(16), tags = json_set(tags, '$.n', ?) WHERE id = ?",
        "UPDATE strict_t SET n = ?, a = ? WHERE id = ?",
        "UPDATE posts SET views = ? WHERE title = ?",
        "UPDATE posts SET views = views + 1 WHERE user_id = ? AND created > ?",
        "INSERT INTO log2 SELECT * FROM log",
        "UPDATE post_counts SET n = 0 WHERE user_id = ?",
        "DELETE FROM posts WHERE id = ?",
        "DELETE FROM posts WHERE created < (SELECT min(created) FROM posts WHERE user_id = ?) RETURNING id",
        "DELETE FROM tags WHERE post_id IN (SELECT id FROM posts WHERE views = 0)",
        "DELETE FROM users WHERE id = ?",
        "DELETE FROM log",
        // Transactions and the schema.
        "BEGIN",
        "BEGIN IMMEDIATE",
        "COMMIT",
        "ROLLBACK",
        "SAVEPOINT phv_2",
        "RELEASE SAVEPOINT phv_2",
        "ROLLBACK TO SAVEPOINT phv_2",
        "CREATE TABLE IF NOT EXISTS entries (id INTEGER PRIMARY KEY, name TEXT NOT NULL, message TEXT NOT NULL, created_at TEXT NOT NULL)",
        "CREATE TABLE copy AS SELECT * FROM users WHERE age > ?",
        "CREATE INDEX IF NOT EXISTS log_at ON log (at)",
        "CREATE UNIQUE INDEX posts_user_title ON posts (user_id, title)",
        "CREATE VIEW v AS SELECT id FROM users",
        "CREATE TRIGGER tr AFTER UPDATE OF views ON posts WHEN new.views > old.views BEGIN INSERT INTO log VALUES ('up', new.id); END",
        "CREATE VIRTUAL TABLE more USING fts5(a)",
        "ALTER TABLE log ADD COLUMN who TEXT DEFAULT ''",
        "ALTER TABLE log RENAME COLUMN at TO when_",
        "ALTER TABLE log RENAME TO journal",
        "ALTER TABLE users DROP COLUMN tags",
        "ALTER TABLE docs RENAME TO documents",
        "DROP INDEX posts_title",
        "DROP VIEW post_counts",
        "DROP TRIGGER posts_log",
        "DROP TABLE strict_t",
        "DROP TABLE docs",
        "REINDEX",
        "ANALYZE",
        "VACUUM",
        "PRAGMA integrity_check",
        "PRAGMA quick_check",
        "PRAGMA table_info(users)",
        "PRAGMA foreign_key_check",
        "PRAGMA journal_mode",
        "PRAGMA incremental_vacuum",
        "PRAGMA optimize",
        "PRAGMA wal_checkpoint(PASSIVE)",
        "PRAGMA page_count",
        "PRAGMA max_page_count",
        "PRAGMA user_version = 3",
        "SELECT sqlite_version(), sqlite_source_id(), changes(), total_changes(), last_insert_rowid()",
        "SELECT ?1, ?1 + 1, :name, @x, $y",
        "SELECT 1 WHERE 0",
    ];

    /// Every step the corpus produces is in a class: an opcode that is in
    /// none (a new SQLite's, say) fails here, as it would weigh a slow read
    /// without anyone deciding so. The corpus must prepare (so the schema and
    /// the statements are right), and the steps seen are printed.
    #[test]
    fn every_opcode_is_classed() {
        let c = memory(CORPUS_SCHEMA);
        let mut seen = std::collections::BTreeSet::new();
        for sql in CORPUS {
            // A schema statement is listed (`EXPLAIN` does not run it): the
            // rest of the corpus still sees the tables above.
            let mut st = c.prepare(&format!("EXPLAIN {sql}")).unwrap_or_else(|e| panic!("{sql}: {e}"));
            // The parameters stay unbound (`EXPLAIN` lists the program).
            let mut rows = st.raw_query();
            let mut ops = Vec::new();
            while let Some(r) = rows.next().unwrap_or_else(|e| panic!("{sql}: {e}")) {
                ops.push(r.get::<_, String>(1).unwrap());
            }
            assert!(!ops.is_empty(), "{sql}: no steps");
            seen.extend(ops);
        }
        let unclassed: Vec<&String> = seen.iter().filter(|op| !classed(op)).collect();
        println!("{} opcodes seen: {:?}", seen.len(), seen);
        let never: Vec<&str> = SQLITE_OPCODES.iter().copied().filter(|op| !seen.contains(*op)).collect();
        println!("{} opcodes the corpus never produced: {never:?}", never.len());
        assert!(unclassed.is_empty(), "opcodes in no class: {unclassed:?}");
        let unknown: Vec<&String> = seen.iter().filter(|op| !SQLITE_OPCODES.contains(&op.as_str())).collect();
        assert!(unknown.is_empty(), "opcodes not in SQLITE_OPCODES: {unknown:?}");
    }

    /// The identities of `n` plain files (made in a directory of their own).
    fn files(tag: &str, n: usize) -> Vec<Database> {
        let dir = std::env::temp_dir().join(format!("phv-cost-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (0..n)
            .map(|i| {
                let name = format!("{i}.db");
                std::fs::write(dir.join(&name), b"").unwrap();
                let (guard, name) = crate::safefs::open_parent(&dir, std::path::Path::new(&name)).unwrap();
                let id = guard.plain_file_id(&name).unwrap().unwrap();
                Database::new(id, std::fs::metadata(dir.join(name)).and_then(|m| m.created()).ok())
            })
            .collect()
    }

    /// A file whose birth time the file system does not record is known by
    /// its opening: two opens of it do not share weights (an inode given to
    /// another file after a delete would otherwise; found by the 1.2.1
    /// audit). With a birth time, they do.
    #[test]
    fn a_file_without_a_birth_time_has_weights_per_open() {
        let id = files("nonce", 1)[0].id;
        let (a, b) = (Database::new(id, None), Database::new(id, None));
        assert_ne!(a, b);
        let n = analyzed();
        remember(a, 3, n, "SELECT 1", 5);
        assert_eq!(cached(a, 3, n, "SELECT 1"), Some(5));
        assert_eq!(cached(b, 3, n, "SELECT 1"), None, "shared by two opens of a file without a birth time");
        let made = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(Database::new(id, Some(made)), Database::new(id, Some(made)));
        assert_ne!(Database::new(id, Some(made)), Database::new(id, Some(made + std::time::Duration::from_secs(1))));
    }

    /// The weights are kept by file, schema cookie, `ANALYZE` count and
    /// text, bounded in count and in the length of the texts. (The cache is
    /// the process's: other tests of this binary add to it meanwhile, so the
    /// bound is checked, not the exact count.)
    #[test]
    fn weights_are_kept_and_bounded() {
        let dbs = files("kept", 2);
        let (a, b) = (dbs[0], dbs[1]);
        assert_ne!(a, b);
        let n = analyzed();
        remember(a, 7, n, "SELECT 1", 3);
        assert_eq!(cached(a, 7, n, "SELECT 1"), Some(3));
        // Another schema, another text, another file: not the same weight.
        assert_eq!(cached(a, 8, n, "SELECT 1"), None);
        assert_eq!(cached(a, 7, n, "SELECT 2"), None);
        assert_eq!(cached(b, 7, n, "SELECT 1"), None);
        // Weighed before an `ANALYZE`: not used after it.
        note_analyze();
        assert_eq!(cached(a, 7, analyzed(), "SELECT 1"), None, "used after an ANALYZE");
        assert_eq!(cached(a, 7, n, "SELECT 1"), Some(3));
        // A long text is not kept.
        let long = format!("SELECT '{}'", "x".repeat(KEPT_TEXT));
        remember(a, 7, n, &long, 1);
        assert_eq!(cached(a, 7, n, &long), None);
        // At most `KEPT_WEIGHTS`, the oldest dropped first.
        remember(b, 1, n, "SELECT 'first'", 1);
        for i in 0..KEPT_WEIGHTS {
            remember(b, 1, n, &format!("SELECT {i}"), 2);
        }
        assert!(weight_stats().1 <= KEPT_WEIGHTS);
        assert_eq!(cached(b, 1, n, "SELECT 'first'"), None, "the oldest weight was kept");
        assert_eq!(cached(b, 1, n, &format!("SELECT {}", KEPT_WEIGHTS - 1)), Some(2));
    }

    /// A program's schema: none for a statement that reads no table, the
    /// main database's cookie for one that does, and another database's (a
    /// temporary table) is not kept.
    #[test]
    fn the_schema_of_a_program_is_its_cookie() {
        let c = memory("CREATE TABLE t (a); CREATE TEMP TABLE tt (b);");
        let version: i64 = c.query_row("PRAGMA schema_version", [], |r| r.get(0)).unwrap();
        assert_eq!(weigh(&c, "SELECT length(?)").unwrap().1, Schema::None);
        assert_eq!(weigh(&c, "SELECT a FROM t").unwrap().1, Schema::Main(version));
        assert_eq!(weigh(&c, "SELECT a FROM t, tt").unwrap().1, Schema::Other);
        assert_eq!(weigh(&c, "SELECT b FROM tt").unwrap().1, Schema::Other);
        c.execute_batch("CREATE TABLE u (x)").unwrap();
        assert_eq!(weigh(&c, "SELECT a FROM t").unwrap().1, Schema::Main(version + 1));
    }

    /// Ordinary statements weigh far less than the limit.
    #[test]
    fn ordinary_statements_are_light() {
        let setup = "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT, score REAL, views INTEGER, created TEXT);
            CREATE INDEX posts_user ON posts (user_id, created);
            CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE, email TEXT UNIQUE);";
        let in_list = format!("SELECT * FROM posts WHERE id IN ({})", vec!["?"; 2000].join(","));
        let values = format!("INSERT INTO posts (user_id, title) VALUES {}", vec!["(?, ?)"; 1000].join(","));
        let report = "SELECT strftime('%Y-%m', created) AS m, count(*), sum(score), avg(score), max(views),
            sum(CASE WHEN views > 10 THEN 1 ELSE 0 END) FROM posts WHERE created >= ? GROUP BY m HAVING count(*) > ? ORDER BY m";
        // 30 columns of arithmetic (60 steps), about the most a page computes in one statement.
        let arith = format!(
            "SELECT {} FROM posts WHERE id = ?",
            (0..30).map(|i| format!("score * {i} + views AS x{i}")).collect::<Vec<_>>().join(", ")
        );
        assert!(weigh_sql(setup, &arith) <= MAX_WEIGHT);
        for sql in [
            "SELECT p.id, p.title, u.name FROM posts p JOIN users u ON u.id = p.user_id WHERE p.created > ? AND u.name LIKE ? ORDER BY p.created DESC LIMIT 20",
            "INSERT INTO users (id, name, email) VALUES (?, ?, ?) ON CONFLICT(email) DO UPDATE SET name = excluded.name",
            "UPDATE posts SET score = score * 0.9 + ?, views = views + 1 WHERE id = ?",
            report,
            &in_list,
            &values,
        ] {
            let w = weigh_sql(setup, sql);
            assert!(w <= MAX_WEIGHT / 4, "{w}: {sql}");
        }
    }

    /// A statement that reads long values many times weighs as much, also
    /// when it is made by expanding a `WITH` table.
    #[test]
    fn repeated_reads_weigh_what_they_read() {
        let setup = "CREATE TABLE t (b TEXT)";
        let chain = |t: &str, n: usize| vec![t; n].join("+");
        let flat = format!("SELECT {} FROM t", chain("b", 100));
        assert!(weigh_sql(setup, &flat) > MAX_WEIGHT);
        let nested = format!("WITH a(x) AS (SELECT {} FROM t) SELECT {} FROM a", chain("b", 10), chain("x", 10));
        assert!(weigh_sql(setup, &nested) > MAX_WEIGHT);
        let lengths = format!("SELECT {} FROM t", vec!["length(b)"; 200].join(", "));
        assert!(weigh_sql(setup, &lengths) > MAX_WEIGHT);
        let same_param = format!("SELECT 1 FROM t WHERE b IN ({})", vec!["?1"; 3000].join(","));
        assert!(weigh_sql(setup, &same_param) > MAX_WEIGHT);
        // PHV's own functions and those that allocate weigh nothing.
        let terms = ["instr(b, 'x')", "upper(b)", "b LIKE '%x'", "trim(b, 'x')"].repeat(100);
        let ours = format!("SELECT {} FROM t", terms.join(", "));
        assert!(weigh_sql(setup, &ours) < 2);
    }
}
