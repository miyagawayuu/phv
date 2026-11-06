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
    let limits = Limits {
        max_memory: Some(max_memory),
        max_time: Some(std::time::Duration::from_secs(60)),
        ..Limits::default()
    };
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
