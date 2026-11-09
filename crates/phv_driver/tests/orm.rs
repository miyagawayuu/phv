//! std.orm keeps the script's limits: the entities it builds, the values it
//! remembers for `update()` and the JSON it decodes count against the memory
//! limit, and loading stops at the time limit. (The SQL it builds runs through
//! std.db, whose own limits `sqlite_limits.rs` and `databases.rs` test.)

use phv_driver::{compile, run, RunOptions};
use phv_vm::alloc::{reset_thread_peak, thread_allocated, thread_peak, CountingAlloc};
use phv_vm::Limits;
use std::time::{Duration, Instant};

// Memory limits are enforced through the counting allocator.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const LIMIT: usize = 16 << 20;

const ENTITY: &str = r#"<?phv
import std.db.Database;
import std.orm.Repo;

#[Table("items")]
class Item {
    public ?int $id = null;
    public string $name = "";
    public list<int> $numbers = [];
}

$db = Database::open(":memory:");
$db->execute(sql"CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, numbers TEXT)");
$items = new Repo<Item>($db);
"#;

/// Runs `src` after `ENTITY`; returns its output and first error line, the
/// most memory the run held at once, and how long it took.
fn run_limited(name: &str, src: &str, max_memory: usize, max_time: Duration) -> (String, usize, Duration) {
    let dir = std::env::temp_dir().join(format!("phv-orm-limits-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.phv");
    std::fs::write(&entry, format!("{ENTITY}{src}")).unwrap();
    let c = compile(&dir, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let limits = Limits { max_memory: Some(max_memory), max_time: Some(max_time), ..Limits::default() };
    let base = thread_allocated();
    reset_thread_peak();
    let start = Instant::now();
    let outcome = run(&c, RunOptions { html: false, limits, http: None, host: None });
    let took = start.elapsed();
    let peak = (thread_peak() - base).max(0) as usize;
    let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    (format!("{}{}", outcome.output, err.unwrap_or_default()), peak, took)
}

/// The entities of a large table, and what the Repo remembers of them, stop
/// at the memory limit instead of growing the process past it.
#[test]
fn loading_many_entities_stops_at_the_memory_limit() {
    let src = r#"$db->execute(sql"INSERT INTO items (name, numbers) WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 200000) SELECT 'item ' || i, '[1, 2, 3]' FROM c");
echo "loaded ", count($items->findBy());
"#;
    let (out, peak, _) = run_limited("many", src, LIMIT, Duration::from_secs(60));
    assert!(out.contains("allowed memory size of 16 MB exhausted"), "{out}");
    assert!(peak < LIMIT + LIMIT / 2, "held {} MB", peak >> 20);
    // Under a larger limit the same rows load.
    let src = r#"$db->execute(sql"INSERT INTO items (name, numbers) WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 2000) SELECT 'item ' || i, '[1, 2, 3]' FROM c");
echo "loaded ", count($items->findBy());
"#;
    let (out, _, _) = run_limited("some", src, LIMIT, Duration::from_secs(60));
    assert_eq!(out, "loaded 2000");
}

/// The JSON in a column is decoded within the memory limit (a column of a
/// few megabytes can make values many times its size).
#[test]
fn a_large_json_column_stops_at_the_memory_limit() {
    let src = r#"$db->execute(sql"INSERT INTO items (name, numbers) WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 1500000) SELECT 'big', '[' || group_concat(i % 10, ',') || ']' FROM c");
echo "loaded ", count($items->find(1)?->numbers ?? []);
"#;
    let (out, peak, _) = run_limited("json", src, 32 << 20, Duration::from_secs(60));
    assert!(out.contains("allowed memory size of 32 MB exhausted"), "{out}");
    assert!(peak < (32 << 20) + (16 << 20), "held {} MB", peak >> 20);
}

/// Loading entities in a loop stops soon after the time limit.
#[test]
fn loading_entities_stops_at_the_time_limit() {
    let src = r#"$db->execute(sql"INSERT INTO items (name, numbers) WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM c WHERE i < 2000) SELECT 'item ' || i, '[1, 2, 3]' FROM c");
$n = 0;
while (true) {
    $n += count($items->findBy(limit: 1000));
}
"#;
    let (out, _, took) = run_limited("time", src, 256 << 20, Duration::from_secs(1));
    assert!(out.contains("maximum execution time"), "{out}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
}
