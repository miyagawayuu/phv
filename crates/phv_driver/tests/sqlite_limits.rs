//! SQLite's memory counts against the script's memory limit: the page caches
//! of its connections, temporary tables and sorts. Regression tests for the
//! 1.1.1 audit (security-audit.md §25).

use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::{reset_thread_peak, thread_allocated, thread_peak, CountingAlloc};
use phv_vm::{Host, Limits, Permissions};
use std::sync::Arc;

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const LIMIT: usize = 16 << 20;

/// Runs `src` in a site that may write `data/`; returns its output and first
/// error line, and the most memory the run held at once.
fn run_site(name: &str, src: &str, max_memory: usize) -> (String, usize) {
    run_site_for(name, src, max_memory, std::time::Duration::from_secs(60))
}

/// `run_site` with the time limit `max_time`.
fn run_site_for(name: &str, src: &str, max_memory: usize, max_time: std::time::Duration) -> (String, usize) {
    let dir = std::env::temp_dir().join(format!("phv-sqlite-limits-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("data")).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, format!("<?phv\nimport std.db.Database;\n{src}")).unwrap();
    let c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let host = Host {
        root: dir.clone(),
        permissions: Permissions { write: vec!["data/".into()], ..Permissions::default() },
        ..Host::default()
    };
    let limits = Limits { max_memory: Some(max_memory), max_time: Some(max_time), ..Limits::default() };
    let base = thread_allocated();
    reset_thread_peak();
    let outcome = run(&c, RunOptions { html: false, limits, http: None, host: Some(Arc::new(host)) });
    let peak = (thread_peak() - base).max(0) as usize;
    let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
    // The connections the run gave back are closed with the directory.
    let _ = std::fs::remove_dir_all(&dir);
    (format!("{}{}", outcome.output, err.unwrap_or_default()), peak)
}

/// A table of about 4 MB, more than a connection's page cache.
const TABLE: &str = r#"$db = Database::open("data/rows.db");
$db->execute(sql"CREATE TABLE t (x TEXT)");
$db->execute(sql"INSERT INTO t WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 40000) SELECT hex(randomblob(50)) FROM c");
"#;

/// Found by the 1.1.1 audit: each connection to a file had a 2 MB page cache
/// that no limit counted (400 connections held 572 MB under 16 MB).
#[test]
fn page_caches_count_against_the_memory_limit() {
    let src = format!(
        r#"{TABLE}list<Database> $keep = [];
for ($i = 0; $i < 60; $i++) {{
    $d = Database::open("data/rows.db");
    $d->first(sql"SELECT sum(length(x)) AS s FROM t");
    $keep[] = $d;
}}
echo "opened ", count($keep);
"#
    );
    let (out, peak) = run_site("caches", &src, LIMIT);
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{out}");
    assert!(peak < LIMIT + LIMIT / 2, "held {} MB", peak >> 20);
}

/// A script may hold 64 databases at once; closed ones do not count, and a
/// connection kept between opens is charged only while it is held.
#[test]
fn a_script_holds_at_most_64_databases() {
    let src = r#"$db = Database::open("data/small.db");
$db->execute(sql"CREATE TABLE t (x INTEGER)");
$db->close();
list<Database> $keep = [];
try {
    for ($i = 0; $i < 100; $i++) { $keep[] = Database::open($i % 2 == 0 ? "data/small.db" : ":memory:"); }
} catch (DatabaseError $e) {
    echo count($keep), ": ", $e->getMessage(), "\n";
}
foreach ($keep as $d) { $d->close(); }
// Opened and closed many times over: the kept connection is charged only while
// held. (Reads only: a write per open waits for the disk, slow on some machines.)
$n = 0;
for ($i = 0; $i < 3000; $i++) {
    $d = Database::open("data/small.db");
    $n += $d->first(sql"SELECT count(*) + 1 AS n FROM t")?->int("n") ?? 0;
    $d->close();
}
echo $n;
"#;
    let (out, _) = run_site("cap", src, LIMIT);
    assert_eq!(
        out, "64: this script already has 64 SQLite databases open: close() the ones it no longer uses\n3000",
        "{out}"
    );
}

/// Found by the 1.1.1 audit: temporary tables went to temporary files that no
/// limit bounded (2.4 GB of disk in 8 seconds under 16 MB, without permissions).
#[test]
fn temporary_tables_count_against_the_memory_limit() {
    let src = r#"$db = Database::open(":memory:");
$db->execute(sql"CREATE TEMP TABLE tt (b BLOB)");
$m = 0;
while ($m < 200) {
    $db->execute(sql"INSERT INTO tt SELECT randomblob(1000000) FROM (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4)");
    $m += 4;
}
echo "temp table holds ", $m, " MB";
"#;
    let (out, peak) = run_site("temp", src, LIMIT);
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{out}");
    assert!(peak < LIMIT + LIMIT / 2, "held {} MB", peak >> 20);
    // A temporary table of a database file too.
    let (out, _) = run_site("temp_file", &src.replace(":memory:", "data/t.db"), LIMIT);
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{out}");
}

/// Sorts larger than the page cache went to temporary files as well: they
/// are now bounded by the memory limit, and smaller ones still work.
#[test]
fn sorts_count_against_the_memory_limit() {
    let fill = |rows: usize, then: &str| {
        format!(
            r#"$db = Database::open("data/s.db");
$db->execute(sql"CREATE TABLE s (b BLOB)");
$db->execute(sql"INSERT INTO s WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < {rows}) SELECT randomblob(500000) FROM c");
{then}"#
        )
    };
    // Every row goes through the sorter.
    let group =
        r#"echo "groups: ", $db->first(sql"SELECT count(*) AS n FROM (SELECT b FROM s GROUP BY b)")?->int("n") ?? 0;"#;
    let index = r#"$db->execute(sql"CREATE INDEX s_b ON s (b)"); echo "indexed";"#;
    // 32 MB to sort.
    for (name, then) in [("group_big", group), ("index_big", index)] {
        let (out, peak) = run_site(name, &fill(64, then), LIMIT);
        assert!(out.contains("allowed memory size of 16 MB exhausted"), "{name}: {out}");
        assert!(peak < LIMIT + LIMIT / 2, "{name} held {} MB", peak >> 20);
    }
    // 4 MB.
    assert_eq!(run_site("group_small", &fill(8, group), LIMIT).0, "groups: 8");
    assert_eq!(run_site("index_small", &fill(8, index), LIMIT).0, "indexed");
}

/// Found by the 1.2.0 audit: SQLite looked at the deadline every 10,000
/// steps, and one step may be a function call on a value of many MB: these
/// queries ran 20 to 470 seconds under a 1 second limit. Each case does about
/// a thousand calls on 15 MB values between two of the old looks (tens of
/// seconds or more on a fast machine); now it stops soon after the limit.
#[test]
fn sqlite_functions_on_large_values_stop_at_the_time_limit() {
    let cases = [
        ("trim", "length(trim('mk1', substr($y, a % 2 + 1)))"),
        ("trim_x", "length(trim(substr($y, a % 2 + 1), 'mk1'))"),
        ("instr", "instr(substr($y, a % 2 + 1), 'mk1')"),
        ("replace", "length(replace(substr($y, a % 2 + 1), 'mk1', 'x'))"),
        ("like", "substr($y, a % 2 + 1) LIKE '%mk1'"),
        ("upper", "length(upper(substr($y, a % 2 + 1)))"),
    ];
    for (name, call) in cases {
        let call = call.replace("$y", "{$y}");
        let src = format!(
            r#"$db = Database::open(":memory:");
$db->execute(sql"CREATE TABLE t (a INTEGER)");
$db->execute(sql"INSERT INTO t WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 3000) SELECT i FROM c");
$y = str_repeat("abcdefghijklmnopqrstuvwxyz0123", 500000);
echo $db->first(sql"SELECT sum({call}) AS s FROM t")?->int("s") ?? 0;
"#
        );
        let start = std::time::Instant::now();
        let (out, _) = run_site_for(name, &src, 1 << 30, std::time::Duration::from_secs(1));
        assert!(out.contains("time limit was reached") || out.contains("maximum execution time"), "{name}: {out}");
        assert!(start.elapsed() < std::time::Duration::from_secs(8), "{name} took {:?}", start.elapsed());
    }
}

/// Found by the 20th audit: SQLite sleeps for a lock outside its program's
/// steps, so the time limit did not stop the wait. A script set
/// `PRAGMA busy_timeout` to up to 24 days and held its worker that long, and
/// the default 5 seconds ran past a 1 second limit. The pragma is refused,
/// and a write waiting for another connection's lock ends at the time limit.
#[test]
fn a_write_waiting_for_a_lock_stops_at_the_time_limit() {
    let hold = r#"$a = Database::open("data/locked.db");
$a->execute(sql"CREATE TABLE IF NOT EXISTS t (n INTEGER)");
$b = Database::open("data/locked.db");
$a->execute(sql"BEGIN IMMEDIATE");
$a->execute(sql"INSERT INTO t VALUES (1)");
"#;
    for (name, pragma) in [
        ("busy_set", "PRAGMA busy_timeout = 60000"),
        ("busy_read", "PRAGMA busy_timeout"),
        ("busy_table", "SELECT * FROM pragma_busy_timeout"),
    ] {
        let src = format!(
            r#"{hold}try {{ $b->query(sql"{pragma}"); echo "ran"; }} catch (DatabaseError $e) {{ echo $e->getMessage(); }}"#
        );
        let (out, _) = run_site_for(name, &src, 1 << 30, std::time::Duration::from_secs(1));
        assert!(out.contains("not authorized"), "{pragma}: {out}");
    }
    let src = format!("{hold}$b->execute(sql\"INSERT INTO t VALUES (2)\");\necho \"wrote\";");
    let start = std::time::Instant::now();
    let (out, _) = run_site_for("busy_wait", &src, 1 << 30, std::time::Duration::from_secs(1));
    let took = start.elapsed();
    assert!(out.contains("time limit was reached"), "{out}");
    assert!(took < std::time::Duration::from_millis(1500), "took {took:?}");
    // Without a time limit nearer than that, a write waits 5 seconds for the lock.
    let src = format!(
        "{hold}try {{ $b->execute(sql\"INSERT INTO t VALUES (2)\"); }} catch (DatabaseError $e) {{ echo $e->getMessage(); }}"
    );
    let start = std::time::Instant::now();
    let (out, _) = run_site_for("busy_long", &src, 1 << 30, std::time::Duration::from_secs(30));
    let took = start.elapsed();
    assert!(out.contains("database is locked"), "{out}");
    assert!(took >= std::time::Duration::from_secs(5) && took < std::time::Duration::from_secs(8), "took {took:?}");
}

/// Found by the 1.2.0 audit: PHV's own SQL functions (`replace()`, `trim()`,
/// `instr()`, `LIKE`) copied their arguments and built their results without
/// asking the memory limit, passing it by up to four values of the longest.
#[test]
fn sqlite_functions_count_against_the_memory_limit() {
    // 1.9 MB of `0`, each replaced by 8 characters: a 15.2 MB result.
    let src = r#"$db = Database::open(":memory:");
echo $db->first(sql"SELECT length(replace(hex(zeroblob(950000)), '0', 'abcdefgh')) AS n")?->int("n") ?? 0;
"#;
    let (out, peak) = run_site("replace_mem", src, LIMIT);
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{out}");
    assert!(peak < LIMIT, "held {} MB", peak >> 20);
    // Smaller values still work, also blobs that are not UTF-8.
    let src = r#"$db = Database::open(":memory:");
echo $db->first(sql"SELECT length(replace(hex(zeroblob(500000)), '0', 'ab')) AS n")?->int("n") ?? 0, " ";
echo $db->first(sql"SELECT length(trim(x'ff' || hex(zeroblob(1000)) || x'ff', x'ff')) AS n")?->int("n") ?? 0, " ";
echo $db->first(sql"SELECT instr(x'ff41ff42', 'B') AS n")?->int("n") ?? 0;
"#;
    assert_eq!(run_site("small_mem", src, LIMIT).0, "2000000 2000 4");
}

/// The script's statements in the following tests: a table `t` holding one
/// value of 16 MB of digits (each `b * b` turns it into a number twice,
/// about 60 ms; nothing in SQLite looks at the deadline meanwhile), and `n`
/// of 100,000 rows.
const LONG_DIGITS: &str = r#"$db = Database::open(":memory:");
$db->execute(sql"CREATE TABLE t (b TEXT)");
$v = str_repeat("1111111111", 1600000);
$db->execute(sql"INSERT INTO t VALUES ({$v})");
$v = "";
$db->execute(sql"CREATE TABLE n (i INTEGER)");
$db->execute(sql"INSERT INTO n WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 100000) SELECT i FROM c");
"#;

/// Found by the 1.2.0 audit (after the fix above): SQLite looks at the
/// deadline only where its program jumps back, so one row computed by
/// thousands of steps that each read a long value ran for minutes, and a
/// short text makes such a program by expanding a `WITH` table, a view or a
/// trigger. Such statements are refused before they run, and the heaviest
/// statement allowed reads a long value at most 128 times. The bound is
/// counted in reads, timed on this machine: 250 of them (the old code ran
/// 1,000 and more).
#[test]
fn sqlite_statements_that_reread_long_values_are_refused() {
    let x = |n: usize, term: &str, sep: &str| vec![term; n].join(sep);
    // The time of one `b * b`.
    let src = format!(
        "{LONG_DIGITS}$t = microtime();\n$db->query(sql\"SELECT {} FROM t\");\necho round((microtime() - $t) / 32 * 1000000);\n",
        x(32, "b * b", ", ")
    );
    let (out, _) = run_site_for("unit", &src, 256 << 20, std::time::Duration::from_secs(120));
    let micros: f64 = out.trim().parse().unwrap_or_else(|_| panic!("{out}"));
    let unit = std::time::Duration::from_secs_f64(micros / 1e6);
    let flat = format!("SELECT {} FROM t", x(1000, "b * b", ", "));
    let cte = format!(
        "WITH a(x) AS (SELECT {} FROM t), a2(y) AS (SELECT {} FROM a) SELECT {} FROM a2",
        x(10, "b * b", " + "),
        x(10, "x", " + "),
        x(10, "y", " + ")
    );
    let view = format!(
        "{LONG_DIGITS}$db->execute(sql\"CREATE VIEW v AS SELECT {} AS x FROM t\");\n$db->query(sql\"SELECT {} FROM v\");\n",
        x(20, "b * b", " + "),
        x(20, "x", " + ")
    );
    let trigger = format!(
        "{LONG_DIGITS}$db->execute(sql\"CREATE TABLE log (x)\");\n$db->execute(sql\"CREATE TRIGGER tr AFTER UPDATE ON t BEGIN INSERT INTO log SELECT {}; END\");\n$db->execute(sql\"UPDATE t SET b = b\");\n",
        x(400, "new.b * new.b", " + ")
    );
    for (name, src) in [
        ("flat", format!("{LONG_DIGITS}$db->query(sql\"{flat}\");\n")),
        ("cte", format!("{LONG_DIGITS}$db->query(sql\"{cte}\");\n")),
        ("view", view),
        ("trigger", trigger),
        // A loop: SQLite's progress handler looks at the deadline.
        ("loop", format!("{LONG_DIGITS}$db->query(sql\"SELECT sum(b * b) FROM t, n\");\n")),
    ] {
        let start = std::time::Instant::now();
        let (out, _) = run_site_for(name, &src, 256 << 20, std::time::Duration::from_secs(1));
        let took = start.elapsed();
        assert!(
            out.contains("split it into smaller statements")
                || out.contains("time limit was reached")
                || out.contains("maximum execution time"),
            "{name}: {out}"
        );
        assert!(took < std::time::Duration::from_secs(3) + unit * 250, "{name} took {took:?} (one read: {unit:?})");
    }
    // The statements of real programs are far lighter.
    let src = format!(
        "{LONG_DIGITS}echo count($db->query(sql\"SELECT {} FROM n WHERE i < 3\"));\n",
        (0..30).map(|i| format!("i * {i} + i AS x{i}")).collect::<Vec<_>>().join(", ")
    );
    assert_eq!(run_site_for("ordinary", &src, 256 << 20, std::time::Duration::from_secs(60)).0, "2");
    // `IN` with 10,000 values (30,000 steps).
    let src = format!(
        "{LONG_DIGITS}$q = sql\"SELECT i FROM n WHERE i IN (0\";\nfor ($i = 1; $i <= 10000; $i++) {{ $q = $q . sql\", {{$i}}\"; }}\necho count($db->query($q . sql\")\"));\n"
    );
    assert_eq!(run_site_for("in_list", &src, 256 << 20, std::time::Duration::from_secs(60)).0, "10000");
    // A program of more than 43,008 steps.
    let src = format!(
        "{LONG_DIGITS}$db->query(sql\"WITH a(x) AS (SELECT {} FROM n), a2(y) AS (SELECT {} FROM a) SELECT {} FROM a2\");\n",
        x(40, "i", " + "),
        x(40, "x", " + "),
        x(40, "y", " + ")
    );
    let (out, _) = run_site_for("steps", &src, 256 << 20, std::time::Duration::from_secs(60));
    assert!(out.contains("more than 43,008 steps"), "{out}");
}
