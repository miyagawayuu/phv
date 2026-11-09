//! SQLite connections to a database file are kept between scripts. A kept
//! connection carries nothing from one script to the next: no transaction, no
//! temporary table, no session setting, no deadline, no counters; and it is
//! used only for the very file the name holds.

use phv_driver::{compile, run, RunOptions};
use phv_vm::{Host, Limits, Permissions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The tests read process-wide counters: one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Site {
    root: PathBuf,
    host: Arc<Host>,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl Site {
    fn new(tag: &str) -> Site {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("phv-sqlite-pool-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        let host = Host {
            root: root.clone(),
            permissions: Permissions { write: vec!["data/".into()], ..Permissions::default() },
            ..Host::default()
        };
        Site { root, host: Arc::new(host), _serial: serial }
    }

    /// Runs `src` with `max_time`; its output, and the first line of its error.
    fn run(&self, src: &str, max_time: Option<Duration>) -> String {
        let entry = self.root.join("main.phv");
        std::fs::write(&entry, format!("import std.db.Database;\n{src}")).unwrap();
        let c = compile(&self.root, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
        let limits = Limits { max_time, ..Limits::default() };
        let outcome = run(&c, RunOptions { html: false, limits, http: None, host: Some(self.host.clone()) });
        let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
        format!("{}{}", outcome.output, err.unwrap_or_default())
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn reused() -> u64 {
    phv_std::sqlite_kept_stats().1
}

/// Runs of scripts' statements for which SQLite compiled something so far.
fn compiled() -> u64 {
    phv_std::sqlite_kept_stats().2
}

/// The memory the idle connections hold.
fn held() -> usize {
    phv_std::sqlite_kept_stats().3
}

/// Statements weighed (`EXPLAIN` runs) so far.
fn weighed() -> u64 {
    phv_std::sqlite_weight_stats().0
}

/// Runs `src` and checks a kept connection served its open.
fn run_reused(site: &Site, src: &str, max_time: Option<Duration>) -> String {
    let before = reused();
    let out = site.run(src, max_time);
    assert!(reused() > before, "a kept connection was not used: {out}");
    out
}

const OPEN: &str = "$db = Database::open(\"data/t.db\");\n";

#[test]
fn kept_connections_carry_nothing_between_scripts() {
    let site = Site::new("leaks");
    assert_eq!(site.run(&format!("{OPEN}$db->execute(sql\"CREATE TABLE t (a INTEGER)\"); echo \"ok\";"), None), "ok");

    // An open transaction is rolled back; the connection is kept.
    let out = run_reused(
        &site,
        &format!(
            "{OPEN}$db->execute(sql\"BEGIN\"); $db->execute(sql\"INSERT INTO t (a) VALUES (1)\"); echo \"left open\";"
        ),
        None,
    );
    assert_eq!(out, "left open");
    let out = run_reused(&site, &format!("{OPEN}echo count($db->query(sql\"SELECT a FROM t\"));"), None);
    assert_eq!(out, "0", "the uncommitted row was rolled back");

    // The counters start from zero.
    run_reused(
        &site,
        &format!("{OPEN}$db->execute(sql\"INSERT INTO t (a) VALUES (5)\"); echo $db->lastInsertId();"),
        None,
    );
    let out = run_reused(
        &site,
        &format!(
            "{OPEN}echo $db->lastInsertId(), \" \", json_encode($db->query(sql\"SELECT last_insert_rowid() AS r, changes() AS c, total_changes() AS t\")[0]->toMap());"
        ),
        None,
    );
    assert_eq!(out, "0 {\"r\":0,\"c\":0,\"t\":0}");
    let out = run_reused(
        &site,
        &format!(
            "{OPEN}$db->execute(sql\"INSERT INTO t (a) VALUES (6)\"); $db->execute(sql\"INSERT INTO t (a) VALUES (7)\");
             echo json_encode($db->query(sql\"SELECT changes() AS c, total_changes() AS t\")[0]->toMap());"
        ),
        None,
    );
    assert_eq!(out, "{\"c\":1,\"t\":2}");
    // `execute()` of a statement that changes no row: 0, not the count of the
    // previous script's last change (found by the 1.0.0 audit).
    run_reused(&site, &format!("{OPEN}echo $db->execute(sql\"UPDATE t SET a = a + 1\");"), None);
    let out = run_reused(
        &site,
        &format!("{OPEN}echo $db->execute(sql\"CREATE TABLE IF NOT EXISTS other (x INTEGER)\");"),
        None,
    );
    assert_eq!(out, "0");

    // A temporary table: the connection is not kept, so the next script cannot see it.
    run_reused(
        &site,
        &format!("{OPEN}$db->execute(sql\"CREATE TEMP TABLE scratch (x INTEGER)\"); echo \"temp\";"),
        None,
    );
    let out = site.run(&format!("{OPEN}echo count($db->query(sql\"SELECT name FROM sqlite_temp_master\"));"), None);
    assert_eq!(out, "0");

    // A pragma (a session setting): not kept either.
    run_reused(&site, &format!("{OPEN}$db->execute(sql\"PRAGMA foreign_keys = OFF\"); echo \"pragma\";"), None);
    let before = reused();
    let out =
        site.run(&format!("{OPEN}echo $db->query(sql\"PRAGMA foreign_keys\")[0]->int(\"foreign_keys\") ?? -1;"), None);
    assert_eq!(out, "1", "the default (on), not the setting of the previous script");
    assert_eq!(reused(), before, "the connection that ran a pragma was kept");

    // A failed statement: not kept.
    site.run(&format!("{OPEN}echo \"plain\";"), None);
    let out = run_reused(
        &site,
        &format!("{OPEN}try {{ $db->execute(sql\"INSERT INTO missing VALUES (1)\"); }} catch (DatabaseError $e) {{ echo \"failed\"; }}"),
        None,
    );
    assert_eq!(out, "failed");
    let before = reused();
    site.run(&format!("{OPEN}echo \"next\";"), None);
    assert_eq!(reused(), before, "the connection of a failed statement was kept");
}

/// An idle connection is closed when it has been idle for the kept time (30
/// seconds; 4 here), not up to half as long again (found by the 1.1.1 audit:
/// the sweep woke every 15 seconds, so a connection held its file for up to
/// 45). Only the time from a connection's return to its close is measured, so
/// a slow machine running the scripts slowly does not change the result.
#[test]
fn idle_connections_are_closed_on_time() {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            phv_std::sqlite_set_kept_idle(None);
        }
    }
    let site = Site::new("idle");
    let idle = Duration::from_secs(4);
    phv_std::sqlite_set_kept_idle(Some(idle));
    let _restore = Restore;
    let kept = || phv_std::sqlite_kept_stats().0;
    let wait_for_none = |what: &str| -> Instant {
        let until = Instant::now() + Duration::from_secs(120);
        while kept() > 0 {
            assert!(Instant::now() < until, "{what}: {} connections still kept", kept());
            std::thread::sleep(Duration::from_millis(5));
        }
        Instant::now()
    };
    // The connections other tests left go first (they may take 30 seconds).
    wait_for_none("other tests' connections");
    // One connection starts the sweep; a second one is returned while the
    // first is still kept (the old sweep then closed it at 1.4 times the time).
    assert_eq!(site.run("$a = Database::open(\"data/a.db\"); echo \"a\";", None), "a");
    std::thread::sleep(idle * 6 / 10);
    assert_eq!(site.run("$b = Database::open(\"data/b.db\"); echo \"b\";", None), "b");
    let returned = Instant::now();
    assert_eq!(kept(), 2);
    let closed = wait_for_none("the idle connections");
    let held = closed - returned;
    assert!(held < idle + Duration::from_secs(1), "kept {held:?} after it was returned");
}

/// A recursive query that runs for a while.
const SLOW: &str = "$n = $db->query(sql\"WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3000000) SELECT count(*) AS n FROM c\")[0]->int(\"n\") ?? 0; echo $n;";

#[test]
fn a_kept_connection_stops_at_its_own_scripts_deadline() {
    let site = Site::new("deadline");
    // A script with a short limit leaves the connection; the next one, without
    // a limit and after that deadline, runs a long query to the end.
    assert_eq!(site.run(&format!("{OPEN}echo \"short\";"), Some(Duration::from_millis(300))), "short");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(run_reused(&site, &format!("{OPEN}{SLOW}"), None), "3000000");
    // And a script with a limit is stopped at its own deadline.
    let start = Instant::now();
    let out = run_reused(&site, &format!("{OPEN}{SLOW}"), Some(Duration::from_millis(100)));
    assert!(out.contains("time limit"), "{out}");
    assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
}

#[test]
fn a_kept_connection_follows_its_file() {
    let site = Site::new("file");
    let db = site.root.join("data").join("t.db");
    assert_eq!(site.run(&format!("{OPEN}$db->execute(sql\"CREATE TABLE old (a INTEGER)\"); echo \"ok\";"), None), "ok");

    // A second hard link to the file: refused, as when it is opened anew.
    let link = site.root.join("t-link.db");
    std::fs::hard_link(&db, &link).unwrap();
    let out = site.run(&format!("{OPEN}echo \"opened\";"), None);
    assert!(out.contains("hard link"), "{out}");
    std::fs::remove_file(&link).unwrap();
    assert_eq!(run_reused(&site, &format!("{OPEN}echo \"opened\";"), None), "opened");

    // A script deletes the file (idle connections let go of it), and a new one
    // with the same name is a new database.
    assert_eq!(site.run("unlink(\"data/t.db\"); echo \"deleted\";", None), "deleted");
    assert!(!Path::new(&db).exists());
    let out = site
        .run(&format!("{OPEN}echo count($db->query(sql\"SELECT name FROM sqlite_master WHERE name = 'old'\"));"), None);
    assert_eq!(out, "0");
}

/// A kept connection keeps the statements compiled on it: the next script
/// runs the same texts without compiling (or weighing) them again, with its
/// own values. SQLite would compile them all again if the authorizer or a
/// function were registered again at the open, so those stay with the
/// connection and only what they read about the script changes.
#[test]
fn kept_connections_keep_their_prepared_statements() {
    let site = Site::new("statements");
    let script = |x: i64, s: &str| {
        format!(
            "{OPEN}$x = {x}; $s = \"{s}\";
             $db->execute(sql\"INSERT INTO t (a, b) VALUES ({{$x}}, {{$s}})\");
             echo json_encode($db->query(sql\"SELECT a, b, {{$s}} || '!' AS c FROM t WHERE a = {{$x}}\")[0]->toMap());"
        )
    };
    let create = format!("{OPEN}$db->execute(sql\"CREATE TABLE t (a INTEGER, b TEXT)\"); echo \"ok\";");
    assert_eq!(site.run(&create, None), "ok");
    assert_eq!(run_reused(&site, &script(1, "one"), None), "{\"a\":1,\"b\":\"one\",\"c\":\"one!\"}");
    // The same two texts again: nothing is compiled, neither at the prepare
    // nor at the first step (an expired statement is compiled again there).
    let before = compiled();
    assert_eq!(run_reused(&site, &script(2, "two"), None), "{\"a\":2,\"b\":\"two\",\"c\":\"two!\"}");
    assert_eq!(compiled(), before, "a kept statement was compiled again");
    // Still with the next script's own deadline, counters and taint: the
    // functions read them from the connection's session.
    let out = run_reused(
        &site,
        &format!(
            "{OPEN}echo json_encode($db->query(sql\"SELECT changes() AS c, total_changes() AS t, 'ab' LIKE 'a%' AS l\")[0]->toMap());"
        ),
        None,
    );
    assert_eq!(out, "{\"c\":0,\"t\":0,\"l\":1}");
    let out = run_reused(&site, &format!("{OPEN}{SLOW}"), Some(Duration::from_millis(100)));
    assert!(out.contains("time limit"), "{out}");
}

/// A statement of 80 comparisons of a parameter with `b`: with `b` a `TEXT`
/// column it weighs 5; with `b` an `INTEGER` column (the texts are turned into
/// numbers), 240, over the limit of 128. Its program has more than 200 steps.
const COMPARISONS: &str = "$x = \"7\"; $q = sql\"SELECT count(*) AS c FROM t WHERE 1\"; for ($i = 0; $i < 80; $i++) { $q = $q . sql\" AND b = {$x}\"; }";
const COLUMN_TYPE: &str =
    "$db->execute(sql\"ALTER TABLE t DROP COLUMN b\"); $db->execute(sql\"ALTER TABLE t RENAME COLUMN n TO b\");";

/// A kept statement was weighed when it was compiled. When the schema changed
/// since, by another connection or by this one in an earlier script, SQLite
/// compiles it again at its first step, with at most 200 steps: a larger one
/// is prepared and weighed again, and refused when it is now over the limit.
#[test]
fn kept_statements_are_weighed_again_after_a_schema_change() {
    let site = Site::new("schema");
    let setup = |file: &str| {
        format!(
            "$db = Database::open(\"{file}\"); $db->execute(sql\"CREATE TABLE t (b TEXT, n INTEGER)\");
             $db->execute(sql\"INSERT INTO t VALUES ('7', 7)\"); {COMPARISONS}
             echo $db->query($q)[0]->int(\"c\") ?? -1, $db->query(sql\"SELECT count(*) AS c FROM t\")[0]->int(\"c\") ?? -1;"
        )
    };
    assert_eq!(site.run(&setup("data/t.db"), None), "11");
    // Another connection changes the column's type; the kept connection's
    // statement, compiled against the old schema, is refused.
    let out = run_reused(
        &site,
        &format!(
            "{OPEN}{COMPARISONS} $other = Database::open(\"data/t.db\"); {}
             try {{ $db->query($q); echo \"ran\"; }} catch (DatabaseError $e) {{ echo $e->getMessage(); }}",
            COLUMN_TYPE.replace("$db", "$other")
        ),
        None,
    );
    assert!(out.contains("could read long values 240 times") && out.contains("split it"), "{out}");
    // The same connection changes it in a script of its own; the statement in
    // its cache is refused by the next script, and a light one runs.
    assert_eq!(site.run(&setup("data/s.db"), None), "11");
    assert_eq!(
        run_reused(&site, &format!("$db = Database::open(\"data/s.db\"); {COLUMN_TYPE} echo \"altered\";"), None),
        "altered"
    );
    let out = run_reused(
        &site,
        &format!(
            "$db = Database::open(\"data/s.db\"); {COMPARISONS}
             try {{ $db->query($q); echo \"ran\"; }} catch (DatabaseError $e) {{ echo $e->getMessage(); }}
             echo \" \", $db->query(sql\"SELECT count(*) AS c FROM t\")[0]->int(\"c\") ?? -1;"
        ),
        None,
    );
    assert!(out.contains("could read long values 240 times") && out.ends_with(" 1"), "{out}");
}

/// A weight is kept per process by file, schema and text: the second
/// connection to a file compiles a text the first one weighed, but does not
/// weigh it again. After a schema change the statement is weighed again (the
/// schema version is part of the key), once; another file at the same schema
/// version, with another schema, has weights of its own; a long text is
/// weighed on every connection; `:memory:` databases share nothing.
#[test]
fn weights_are_shared_by_the_connections_of_a_file() {
    let site = Site::new("weights");
    let create = |file: &str, columns: &str| {
        format!(
            "$db = Database::open(\"{file}\"); $db->execute(sql\"CREATE TABLE t ({columns})\");
             $db->execute(sql\"INSERT INTO t VALUES ('7', 7)\"); echo \"ok\";"
        )
    };
    // The statement on two connections held at once (the second is new).
    let two = |file: &str| {
        format!(
            "$a = Database::open(\"{file}\"); $b = Database::open(\"{file}\"); {COMPARISONS}
             foreach ([$a, $b] as $db) {{ try {{ echo $db->query($q)[0]->int(\"c\") ?? -1; }} catch (DatabaseError $e) {{ echo \"refused \"; }} }}"
        )
    };
    assert_eq!(site.run(&create("data/w.db", "b TEXT, n INTEGER"), None), "ok");
    let (before_compiled, before_weighed) = (compiled(), weighed());
    assert_eq!(site.run(&two("data/w.db"), None), "11");
    assert_eq!(compiled(), before_compiled + 2, "each connection compiles the text");
    assert_eq!(weighed(), before_weighed + 1, "the second connection weighed the text again");
    // Both kept: neither compiles nor weighs.
    let (before_compiled, before_weighed) = (compiled(), weighed());
    assert_eq!(run_reused(&site, &two("data/w.db"), None), "11");
    assert_eq!((compiled(), weighed()), (before_compiled, before_weighed));
    // The schema changes (two statements, weighed): the kept statement is
    // compiled again at its first step, then prepared and weighed again (the
    // schema version is part of the key), and refused at 240.
    let before_weighed = weighed();
    let out = run_reused(
        &site,
        &format!(
            "$db = Database::open(\"data/w.db\"); {COLUMN_TYPE} {COMPARISONS}
             try {{ $db->query($q); echo \"ran\"; }} catch (DatabaseError $e) {{ echo $e->getMessage(); }}"
        ),
        None,
    );
    assert!(out.contains("could read long values 240 times"), "{out}");
    assert_eq!(weighed(), before_weighed + 3);
    // Both kept connections compile the text again (one dropped the refused
    // statement, the other's expired), and find the weight at the new schema
    // version: refused without weighing.
    let (before_compiled, before_weighed) = (compiled(), weighed());
    assert_eq!(run_reused(&site, &two("data/w.db"), None), "refused refused ");
    assert_eq!(compiled(), before_compiled + 2);
    assert_eq!(weighed(), before_weighed, "the text was weighed again at a schema version it was weighed at");
    // Another file whose schema version is the same (one `CREATE TABLE`) but
    // whose column type makes the statement heavy: weighed, and refused.
    assert_eq!(site.run(&create("data/x.db", "b INTEGER, n TEXT"), None), "ok");
    let before_weighed = weighed();
    assert_eq!(site.run(&two("data/x.db"), None), "refused refused ");
    assert_eq!(weighed(), before_weighed + 1, "the weight of another file was used");
    // A text longer than 4 KB is weighed on every connection.
    assert_eq!(site.run(&create("data/l.db", "a INTEGER, n TEXT"), None), "ok");
    let before_weighed = weighed();
    let long = format!("{} {}", in_list("a", "data/l.db", 2000), in_list("b", "data/l.db", 2000));
    assert_eq!(site.run(&long, None), "00");
    assert_eq!(weighed(), before_weighed + 2);
    // In a transaction a connection may see a schema of its own: weighed,
    // and not kept (the next script, outside one, weighs the text again).
    let before_weighed = weighed();
    let count = |var: &str| {
        format!("echo ${var}->query(sql\"SELECT count(*) AS c FROM t WHERE n = 'x'\")[0]->int(\"c\") ?? -1;")
    };
    let open = |var: &str| format!("${var} = Database::open(\"data/l.db\");");
    let script = format!(
        "{} {} $a->execute(sql\"BEGIN\"); $b->execute(sql\"BEGIN\"); {} {}",
        open("a"),
        open("b"),
        count("a"),
        count("b")
    );
    assert_eq!(site.run(&script, None), "00");
    // `BEGIN` is weighed once (kept), the count on both connections.
    assert_eq!(weighed(), before_weighed + 3);
    // A third connection, outside a transaction, weighs it (and keeps it).
    let script = format!("{} {} {} {}", open("a"), open("b"), open("c"), count("c"));
    assert_eq!(site.run(&script, None), "0");
    assert_eq!(weighed(), before_weighed + 4);
    let script = format!("{} {} {} {} {}", open("a"), open("b"), open("c"), open("d"), count("d"));
    assert_eq!(site.run(&script, None), "0");
    assert_eq!(weighed(), before_weighed + 4);
    // `:memory:` databases: nothing to share.
    let before_weighed = weighed();
    let memory = "$a = Database::open(\":memory:\"); $b = Database::open(\":memory:\");
        foreach ([$a, $b] as $db) { echo $db->query(sql\"SELECT length('x') AS n\")[0]->int(\"n\") ?? -1; }";
    assert_eq!(site.run(memory, None), "11");
    assert_eq!(weighed(), before_weighed + 2);
    assert!(phv_std::sqlite_weight_stats().1 <= 4096, "the kept weights are not bounded");
}

/// A statement's bound values are cleared when it goes back to the cache
/// (rusqlite's `StatementCache` resets it and clears its bindings): none stays
/// in the pool, and the next script's run of the text binds its own.
#[test]
fn kept_statements_hold_no_bound_values() {
    let site = Site::new("bindings");
    let script =
        |v: &str| format!("{OPEN}$v = {v}; echo $db->query(sql\"SELECT length({{$v}}) AS n\")[0]->int(\"n\") ?? -1;");
    let before = held();
    assert_eq!(site.run(&script("str_repeat(\"x\", 4194304)"), None), "4194304");
    let more = held() as i64 - before as i64;
    assert!(more < 1 << 20, "the kept connection holds {more} more bytes");
    let before = compiled();
    assert_eq!(run_reused(&site, &script("\"ab\""), None), "2");
    assert_eq!(compiled(), before, "the kept statement was compiled again");
}

/// A connection goes back to the pool when the run's teardown is dropped,
/// not when the run ends: a server hands the response over first.
#[test]
fn the_teardown_returns_the_connection() {
    let site = Site::new("teardown");
    let entry = site.root.join("main.phv");
    std::fs::write(
        &entry,
        format!(
            "import std.db.Database;
{OPEN}echo \"ok\";"
        ),
    )
    .unwrap();
    let c = compile(&site.root, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let kept = || phv_std::sqlite_kept_stats().0;
    let before = kept();
    let opts = RunOptions { html: false, limits: Limits::default(), http: None, host: Some(site.host.clone()) };
    let mut outcome = run(&c, opts);
    assert_eq!(outcome.output, "ok");
    assert!(outcome.error.is_none());
    assert_eq!(kept(), before, "the teardown still holds the connection");
    drop(outcome.teardown.take());
    assert_eq!(kept(), before + 1, "dropping the teardown gave the connection back");
}

/// A script that makes the table of `file`.
fn create(file: &str) -> String {
    format!("$db = Database::open(\"{file}\"); $db->execute(sql\"CREATE TABLE t (a INTEGER)\"); echo \"ok\";")
}

/// A script that opens `file` as `$var` and runs a statement whose program
/// has `n` values in an `IN` list (about 150 bytes each: 2,000 take 300 KB,
/// 8,000 1.2 MB).
fn in_list(var: &str, file: &str, n: usize) -> String {
    format!(
        "${var} = Database::open(\"{file}\"); $x = 1;
         $q = sql\"SELECT count(*) AS c FROM t WHERE a IN ({{$x}}\"; for ($i = 0; $i < {n}; $i++) {{ $q = $q . sql\", {{$x}}\"; }}
         echo ${var}->query($q . sql\")\")[0]->int(\"c\") ?? -1;"
    )
}

/// A statement whose program takes more than 1 MB runs but is not kept (found
/// by the 17th assessment: the memory of the kept statements was bounded by
/// their count alone): the idle connection holds no more than before, and the
/// next script compiles the text again. A statement of 300 KB is kept.
#[test]
fn large_statements_are_not_kept() {
    let site = Site::new("large");
    assert_eq!(site.run(&create("data/big.db"), None), "ok");
    let before = held();
    assert_eq!(run_reused(&site, &in_list("db", "data/big.db", 8000), None), "0");
    let more = held() as i64 - before as i64;
    assert!(more < 256 << 10, "the kept connection holds {more} more bytes: the large statement was kept");
    let before = compiled();
    assert_eq!(run_reused(&site, &in_list("db", "data/big.db", 8000), None), "0");
    assert!(compiled() > before, "the large statement was kept");
    assert_eq!(site.run(&create("data/small.db"), None), "ok");
    assert_eq!(run_reused(&site, &in_list("db", "data/small.db", 2000), None), "0");
    let before = compiled();
    assert_eq!(run_reused(&site, &in_list("db", "data/small.db", 2000), None), "0");
    assert_eq!(compiled(), before, "the kept statement was compiled again");
}

/// The idle connections hold at most 64 MB together (1 MB here; found by the
/// 17th assessment: the pool was bounded by count alone, up to 256 connections
/// of 32 large statements each, off every script's memory limit). A connection
/// going back makes room by closing the idle ones that hold the most; one
/// that would not fit alone is closed.
#[test]
fn idle_connections_hold_a_bounded_memory() {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            phv_std::sqlite_set_kept_bytes(None);
        }
    }
    let site = Site::new("bytes");
    let kept = || phv_std::sqlite_kept_stats().0;
    let cap = 1 << 20;
    // A small connection to one file is kept first.
    assert_eq!(site.run(&create("data/b.db"), None), "ok");
    assert_eq!(site.run(&create("data/c.db"), None), "ok");
    assert_eq!(site.run("$a = Database::open(\"data/a.db\"); echo \"a\";", None), "a");
    phv_std::sqlite_set_kept_bytes(Some(cap));
    let _restore = Restore;
    // Eight connections to another file, each with a statement of 300 KB.
    let before = kept();
    let eight: String = (0..8).map(|i| in_list(&format!("d{i}"), "data/b.db", 2000)).collect();
    assert_eq!(site.run(&eight, None), "00000000");
    assert!(held() <= cap, "the idle connections hold {} bytes", held());
    assert!(kept() < before + 8, "all eight connections were kept");
    // The small one is still there: the large ones went first.
    run_reused(&site, "$a = Database::open(\"data/a.db\"); echo \"a\";", None);
    // A connection that alone holds more than the cap is closed.
    phv_std::sqlite_set_kept_bytes(Some(128 << 10));
    assert_eq!(site.run(&in_list("db", "data/c.db", 2000), None), "0");
    let before = reused();
    assert_eq!(site.run(&in_list("db", "data/c.db", 2000), None), "0");
    assert_eq!(reused(), before, "a connection over the cap was kept");
}

/// A `COMMIT` that fails (`database is locked`: another connection reads the
/// file) rolls the transaction back and ends it: the script's later writes
/// and transactions are committed (found by the 19th audit: the transaction
/// stayed open at its depth, the script saw three rows, and the file held
/// none once the connection was given back).
#[test]
fn a_commit_that_fails_ends_the_transaction() {
    let site = Site::new("busy-commit");
    let script = "$r = Database::open(\"data/t.db\");
$w = Database::open(\"data/t.db\");
$w->execute(sql\"CREATE TABLE t (name TEXT)\");
$r->transaction(function () use ($r, $w): void {
    $r->query(sql\"SELECT * FROM t\");
    try {
        $w->transaction(function () use ($w): void {
            $w->execute(sql\"INSERT INTO t VALUES ('mk1')\");
        });
        echo \"committed\\n\";
    } catch (DatabaseError $e) {
        echo \"transaction() threw: \", $e->getMessage(), \"\\n\";
    }
});
echo \"insert mk2: \", $w->execute(sql\"INSERT INTO t VALUES ('mk2')\"), \"\\n\";
$w->transaction(function () use ($w): void {
    $w->execute(sql\"INSERT INTO t VALUES ('mk3')\");
});
echo \"rows seen: \", $w->first(sql\"SELECT count(*) AS n FROM t\")?->int(\"n\") ?? -1, \"\\n\";";
    assert_eq!(
        site.run(script, Some(Duration::from_secs(30))),
        "transaction() threw: database is locked\ninsert mk2: 1\nrows seen: 2\n"
    );
    let check = "$db = Database::open(\"data/t.db\");
foreach ($db->query(sql\"SELECT name FROM t ORDER BY name\") as $row) { echo $row->string(\"name\") ?? \"\", \" \"; }";
    assert_eq!(site.run(check, None), "mk2 mk3 ");
}
