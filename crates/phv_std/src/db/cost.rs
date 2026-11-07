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

use rusqlite::types::ValueRef;
use rusqlite::Connection;
use std::collections::HashMap;

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
    "IfNotOpen",
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

/// The weight of the statement `text` (prepared before on `c`, so it is
/// valid, and not itself an `EXPLAIN`) in slow reads, rounded up. The listing
/// includes the programs of the triggers it fires.
pub(super) fn weigh(c: &Connection, text: &str) -> rusqlite::Result<u32> {
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
    Ok(weight(&steps).div_ceil(SLOW))
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
            "Eq" | "Ne" | "Lt" | "Le" | "Gt" | "Ge" => {
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
            "Affinity" | "MakeRecord" => {
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
            "Function" | "PureFunc" | "AggStep" | "AggStep1" | "AggInverse" | "AggValue" | "AggFinal" => {
                function_weight(&s.p4)
            }
            op if COMPARE.contains(&op) => COMPARING,
            op if COPY.contains(&op) => COPYING,
            // Both operands may be long texts.
            "Add" | "Subtract" | "Multiply" | "Divide" | "Remainder" | "BitAnd" | "BitOr" | "ShiftLeft"
            | "ShiftRight" | "And" | "Or" => 2 * SLOW,
            // The rest (arithmetic, `CAST`, `NOT`, `If`, and steps not known
            // here) may turn a long text into a number.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn weigh_sql(setup: &str, sql: &str) -> u32 {
        // The allocator is installed before SQLite starts (see `mem::init`).
        super::super::mem::init();
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(setup).unwrap();
        weigh(&c, sql).unwrap()
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
