//! Scripts that open one SQLite file while others write to it. Before an
//! open, the journal files of the database must be missing or plain files.
//! A journal another connection is deleting at that moment is neither on
//! Windows: it cannot be opened until its last handle is closed, and a handle
//! opened just before has no link. Found by the 21st assessment: 54 of
//! 12,000 `Database::open()` failed with "permission denied" or "several
//! hard links" under 16 workers. Such a journal now counts as missing; a
//! journal or database that really has a second hard link is still refused.

use phv_driver::{compile, run, RunOptions};
use phv_vm::{Host, Limits, Permissions};
use std::path::PathBuf;
use std::sync::Arc;

fn site(tag: &str) -> (PathBuf, Arc<Host>) {
    let root = std::env::temp_dir().join(format!("phv-sqlite-race-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    let host = Host {
        root: root.clone(),
        permissions: Permissions { write: vec!["data/".into()], ..Permissions::default() },
        ..Host::default()
    };
    (root, Arc::new(host))
}

/// Runs the script `name` of `root`; its output and the first line of its error.
fn run_script(root: &std::path::Path, host: &Arc<Host>, name: &str, src: &str) -> String {
    let entry = root.join(name);
    std::fs::write(&entry, format!("import std.db.Database;\n{src}")).unwrap();
    let c = compile(root, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
    let outcome = run(&c, RunOptions { html: false, limits: Limits::default(), http: None, host: Some(host.clone()) });
    let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
    format!("{}{}", outcome.output, err.unwrap_or_default())
}

#[test]
fn opening_a_database_others_write_to_does_not_fail() {
    let (root, host) = site("writers");
    let made = run_script(
        &root,
        &host,
        "make.phv",
        "$db = Database::open(\"data/t.db\"); $db->execute(sql\"CREATE TABLE t (v INTEGER)\"); echo \"ok\";",
    );
    assert_eq!(made, "ok");
    // Two threads open the file and commit a transaction each time (a
    // journal is made and deleted); six only open it, which looks at the
    // journal and takes no lock.
    const OPEN: &str = "$db = Database::open(\"data/t.db\"); echo \"ok\";";
    const WRITE: &str = "$db = Database::open(\"data/t.db\");\n\
        $db->transaction(function () use ($db): void { $db->execute(sql\"INSERT INTO t (v) VALUES (1)\"); });\n\
        echo \"ok\";";
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let (root, host) = (root.clone(), host.clone());
            std::thread::spawn(move || {
                let (src, runs) = if t < 2 { (WRITE, 100) } else { (OPEN, 200) };
                let entry = root.join(format!("w{t}.phv"));
                std::fs::write(&entry, format!("import std.db.Database;\n{src}")).unwrap();
                let c = compile(&root, &entry).unwrap_or_else(|e| panic!("{}", e.render(false)));
                let mut bad = Vec::new();
                for _ in 0..runs {
                    let options =
                        RunOptions { html: false, limits: Limits::default(), http: None, host: Some(host.clone()) };
                    let outcome = run(&c, options);
                    let err = outcome.error.map(|e| e.render().lines().next().unwrap_or_default().to_string());
                    let out = format!("{}{}", outcome.output, err.unwrap_or_default());
                    // A writer that waited 5 seconds for the other's lock
                    // is another matter (a slow disk), not this check.
                    if out != "ok" && !out.contains("database is locked") {
                        bad.push(out);
                    }
                }
                bad
            })
        })
        .collect();
    let bad: Vec<String> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
    let _ = std::fs::remove_dir_all(&root);
    assert!(bad.is_empty(), "{} of 1400 runs failed, the first: {}", bad.len(), bad[0]);
}

/// What the check is for stays refused: a journal, or the database, that is
/// another file's hard link.
#[test]
fn a_hard_linked_journal_or_database_is_still_refused() {
    let (root, host) = site("links");
    const OPEN: &str = "$db = Database::open(\"data/t.db\"); echo \"opened\";";
    assert_eq!(run_script(&root, &host, "a.phv", OPEN), "opened");
    let other = root.join("other.txt");
    std::fs::write(&other, "not a journal").unwrap();
    for suffix in ["-journal", "-wal", "-shm"] {
        let journal = root.join("data").join(format!("t.db{suffix}"));
        if std::fs::hard_link(&other, &journal).is_err() {
            // A file system without hard links.
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let out = run_script(&root, &host, "a.phv", OPEN);
        assert!(out.contains("hard link"), "{suffix}: {out}");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "not a journal");
        std::fs::remove_file(&journal).unwrap();
    }
    assert_eq!(run_script(&root, &host, "a.phv", OPEN), "opened");
    let link = root.join("t-link.db");
    std::fs::hard_link(root.join("data").join("t.db"), &link).unwrap();
    let out = run_script(&root, &host, "a.phv", OPEN);
    assert!(out.contains("hard link"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}
