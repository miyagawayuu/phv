//! `phv jobs` (run, list, retry) and the jobs of a site built with `phv build`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn phv(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_phv")).args(args).output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn get(port: u16, path: &str) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

fn base(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("phv-cli-jobs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Waits until `file` exists; its text.
fn wait_for(file: &Path, within: Duration) -> Option<String> {
    let until = Instant::now() + within;
    while Instant::now() < until {
        if let Ok(t) = std::fs::read_to_string(file) {
            if !t.is_empty() {
                return Some(t);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

const JOB: &str = "<?phv\nimport std.jobs.job;\nfile_put_contents(\"data/done.txt\", job()->payload);\n";

#[test]
fn a_built_site_runs_its_jobs() {
    let dir = base("built");
    let site = dir.join("site");
    write(&site, "phv.toml", "[permissions]\nwrite = [\"data/\"]\n[jobs]\n");
    write(
        &site,
        "public/index.phv",
        "<?phv\nimport std.jobs.enqueue;\nenqueue(\"note\", \"from the page\");\necho \"ok\";\n",
    );
    write(&site, "jobs/note.phv", JOB);
    // Not `site`: on Unix that is the directory of the sources.
    let exe = dir.join(if cfg!(windows) { "server.exe" } else { "server" });
    let (ok, out) = phv(&["build", &site.to_string_lossy(), "-o", &exe.to_string_lossy()]);
    assert!(ok, "{out}");
    std::fs::remove_dir_all(&site).unwrap();
    let run = dir.join("run");
    std::fs::create_dir_all(run.join("data")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut child = Command::new(&exe)
        .args(["--port", &port.to_string(), "--prod"])
        .current_dir(&run)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    let page = loop {
        if let Some(r) = get(port, "/") {
            break r;
        }
        assert!(Instant::now() < until, "the site did not start");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(page.ends_with("ok"), "{page}");
    let done = wait_for(&run.join("data/done.txt"), Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(done.as_deref(), Some("from the page"));

    // A job that does not compile stops `phv build`.
    let broken = dir.join("broken");
    write(&broken, "phv.toml", "[jobs]\n");
    write(&broken, "public/index.phv", "<?phv echo 1;");
    write(&broken, "jobs/bad.phv", "<?phv $x = ;");
    let (ok, out) = phv(&["build", &broken.to_string_lossy(), "-o", &dir.join("b.exe").to_string_lossy()]);
    assert!(!ok && out.contains("bad.phv"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_and_retry_need_a_shared_store() {
    let dir = base("nostore");
    write(&dir, "phv.toml", "");
    write(&dir, "public/index.phv", "<?phv echo 1;");
    let (ok, out) = phv(&["jobs", "list", &dir.to_string_lossy()]);
    assert!(!ok && out.contains("no [jobs] section"), "{out}");
    write(&dir, "phv.toml", "[jobs]\n");
    let (ok, out) = phv(&["jobs", "list", &dir.to_string_lossy()]);
    assert!(!ok && out.contains("keeps the jobs inside the server process"), "{out}");
    let (ok, out) = phv(&["jobs", "run", &dir.to_string_lossy(), "--help"]);
    assert!(ok, "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `phv jobs run` takes the jobs other processes add to a shared store;
/// `phv jobs list` and `retry` show and requeue them. Needs `PHV_TEST_REDIS_URL`.
#[test]
fn jobs_run_list_and_retry_with_redis() {
    let Ok(url) = std::env::var("PHV_TEST_REDIS_URL") else {
        eprintln!("PHV_TEST_REDIS_URL is not set: skipped");
        return;
    };
    let dir = base("redis");
    // A store namespace of this test run: other test processes use the server too.
    let site = phv_std::testing::run_id();
    write(
        &dir,
        "phv.toml",
        &format!(
            "[permissions]\nwrite = [\"data/\"]\n[databases.q]\nurl = \"{url}\"\n[security]\nstore_namespace = \"{site}\"\n\
             [jobs]\nstore = \"db:q\"\nmax_attempts = 1\n"
        ),
    );
    write(&dir, "public/index.phv", "<?phv echo 1;");
    write(&dir, "jobs/note.phv", JOB);
    write(&dir, "jobs/fail.phv", "<?phv\nthrow new RuntimeException(\"broken on purpose\");\n");
    std::fs::create_dir_all(dir.join("data")).unwrap();
    let spec = phv_vm::DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 2 };
    let store = phv_std::jobs::Store::Db(spec, Some(phv_std::store_namespace(site)));
    // Clear what earlier runs left.
    while let Some(j) = store.take(Duration::from_secs(60)).unwrap() {
        store.end(&j, phv_std::jobs::End::Done { keyed: false }).unwrap();
    }
    for j in store.list(true).unwrap() {
        store.requeue(&j.id).unwrap();
        let t = store.take(Duration::from_secs(60)).unwrap().unwrap();
        store.end(&t, phv_std::jobs::End::Done { keyed: false }).unwrap();
    }
    store.add("note", "from another process", 0, None).unwrap().unwrap();
    let fail_id = store.add("fail", "", 0, None).unwrap().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_phv"))
        .args(["jobs", "run", &dir.to_string_lossy()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let done = wait_for(&dir.join("data/done.txt"), Duration::from_secs(20));
    let until = Instant::now() + Duration::from_secs(10);
    let failed = loop {
        let f = store.list(true).unwrap();
        if !f.is_empty() || Instant::now() > until {
            break f;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(done.as_deref(), Some("from another process"));
    assert_eq!(failed.len(), 1);
    let (ok, out) = phv(&["jobs", "list", "--failed", &dir.to_string_lossy()]);
    assert!(ok && out.contains(&fail_id) && out.contains("broken on purpose"), "{out}");
    let (ok, out) = phv(&["jobs", "retry", &fail_id, &dir.to_string_lossy()]);
    assert!(ok && out.contains("waiting again"), "{out}");
    let (ok, out) = phv(&["jobs", "retry", &fail_id, &dir.to_string_lossy()]);
    assert!(!ok && out.contains("no failed job"), "{out}");
    let (ok, out) = phv(&["jobs", "list", &dir.to_string_lossy()]);
    assert!(ok && out.contains("waiting"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tag no other run of these tests has.
fn run_tag() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{}_{nanos}", std::process::id())
}

/// `phv jobs list` of a site with a namespace says how many jobs are left in
/// the shared store (`phv_jobs`, `phv:jobs:`) of 1.1, which the site does not
/// run; a site without a namespace lists them as its own, with no note. The
/// shared store of `url` is this test's alone.
fn check_left_in_shared(name: &str, url: &str) {
    use phv_std::jobs::{End, Store};
    let dir = base(name);
    let site = format!("left-{}", run_tag());
    let toml = |project: &str| {
        format!("{project}[databases.q]\nurl = \"{url}\"\n[jobs]\nstore = \"db:q\"\nmax_attempts = 1\n")
    };
    let named = toml(&format!("[project]\nname = \"{site}\"\n"));
    write(&dir, "phv.toml", &named);
    write(&dir, "public/index.phv", "<?phv echo 1;");
    let spec = phv_vm::DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 2 };
    let shared = Store::Db(spec.clone(), None);
    // Clear what earlier runs left.
    while let Some(j) = shared.take(Duration::from_secs(60)).unwrap() {
        shared.end(&j, End::Done { keyed: false }).unwrap();
    }
    for j in shared.list(true).unwrap() {
        shared.requeue(&j.id).unwrap();
        let t = shared.take(Duration::from_secs(60)).unwrap().unwrap();
        shared.end(&t, End::Done { keyed: false }).unwrap();
    }
    // The site's own store is empty, and the shared one too: no note.
    let (ok, out) = phv(&["jobs", "list", &dir.to_string_lossy()]);
    assert!(ok && out.contains("no jobs") && !out.contains("note:"), "{out}");
    // Two jobs in the shared store: waiting ones, as 1.1 left them.
    shared.add("left", "one", 0, None).unwrap().unwrap();
    shared.add("left", "two", 0, None).unwrap().unwrap();
    let note = "note: 2 jobs added before this site had a namespace are still in the shared store and not run";
    let (ok, out) = phv(&["jobs", "list", &dir.to_string_lossy()]);
    assert!(ok && out.contains("no jobs") && out.contains(note), "{out}");
    assert!(out.contains("a later release will add a command to move them"), "{out}");
    // The site's own jobs are listed, and the note counts the shared ones only.
    let own = Store::Db(spec.clone(), Some(phv_std::store_namespace(&site)));
    let own_id = own.add("mine", "", 0, None).unwrap().unwrap();
    let (ok, out) = phv(&["jobs", "list", &dir.to_string_lossy()]);
    assert!(ok && out.contains(&own_id) && out.contains("waiting") && out.contains(note), "{out}");
    // A failed job counts too; one that ended does not (and one is "job ... is").
    let t = shared.take(Duration::from_secs(60)).unwrap().unwrap();
    shared.end(&t, End::Done { keyed: false }).unwrap();
    let t = shared.take(Duration::from_secs(60)).unwrap().unwrap();
    shared.end(&t, End::Dead { error: "left behind" }).unwrap();
    let (ok, out) = phv(&["jobs", "list", &dir.to_string_lossy()]);
    assert!(ok && out.contains("note: 1 job added before this site had a namespace is still"), "{out}");
    // A site without a namespace has the shared store as its own: it lists
    // the job, and nothing is noted.
    write(&dir, "phv.toml", &toml(""));
    let (ok, out) = phv(&["jobs", "list", "--failed", &dir.to_string_lossy()]);
    assert!(ok && out.contains("left behind") && !out.contains("note:"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Needs `PHV_TEST_REDIS_URL`; uses database 11 of the server (the shared
/// keys have one name, so another database keeps this test apart from the others).
#[test]
fn list_counts_the_jobs_left_in_the_shared_store_with_redis() {
    let Ok(url) = std::env::var("PHV_TEST_REDIS_URL") else {
        eprintln!("PHV_TEST_REDIS_URL is not set: skipped");
        return;
    };
    let url = match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((server, _)) => format!("{scheme}://{server}/11"),
            None => format!("{url}/11"),
        },
        None => format!("{url}/11"),
    };
    check_left_in_shared("left-redis", &url);
}

/// Needs `PHV_TEST_POSTGRES_URL`; makes a schema of its own and puts it in
/// the URL's `search_path` (the shared table has one name, so the schema
/// keeps this test apart from the others), and drops it at the end.
#[test]
fn list_counts_the_jobs_left_in_the_shared_store_with_postgres() {
    use phv_std::migrate::{Conn, Target};
    let Ok(url) = std::env::var("PHV_TEST_POSTGRES_URL") else {
        eprintln!("PHV_TEST_POSTGRES_URL is not set: skipped");
        return;
    };
    let tag = run_tag();
    let schema = format!("phv_left_{tag}");
    let spec = phv_vm::DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 1 };
    let mut admin = Conn::open(&Target::Server(&spec)).unwrap();
    admin.apply(&format!("9100_left_{tag}.sql"), &format!("CREATE SCHEMA {schema}")).unwrap();
    let sep = if url.contains('?') { '&' } else { '?' };
    check_left_in_shared("left-pg", &format!("{url}{sep}options=-c%20search_path%3D{schema}"));
    admin.apply(&format!("9101_left_{tag}.sql"), &format!("DROP SCHEMA {schema} CASCADE")).unwrap();
}
