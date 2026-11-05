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
