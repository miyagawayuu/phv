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
    assert!(out.contains("`phv jobs adopt` moves those of this site's jobs"), "{out}");
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

/// Needs `PHV_TEST_MYSQL_URL`; the shared table has one name in the test
/// database, so the lock of the unnamed stores is held meanwhile.
#[test]
fn list_counts_the_jobs_left_in_the_shared_store_with_mysql() {
    let Ok(url) = std::env::var("PHV_TEST_MYSQL_URL") else {
        eprintln!("PHV_TEST_MYSQL_URL is not set: skipped");
        return;
    };
    let _unnamed = phv_std::testing::unnamed();
    check_left_in_shared("left-mysql", &url);
}

/// Ends every job of `store`, the given-up ones too.
fn drain(store: &phv_std::jobs::Store) {
    use phv_std::jobs::End;
    while let Some(j) = store.take(Duration::from_secs(60)).unwrap() {
        store.end(&j, End::Done { keyed: false }).unwrap();
    }
    for j in store.list(true).unwrap() {
        store.requeue(&j.id).unwrap();
        let t = store.take(Duration::from_secs(60)).unwrap().unwrap();
        store.end(&t, End::Done { keyed: false }).unwrap();
    }
}

/// `phv jobs adopt` moves the jobs a site with a namespace left in the shared
/// store (`phv_jobs`, `phv:jobs:`) into the site's own store, only those named
/// as the site's job scripts, and a dry run first; a running job, a job whose
/// key the site's store has, and another site's jobs stay. The names and keys
/// are this run's, and the counts are taken against what the shared store of
/// `url` held before, so what a run that died left there does not matter.
fn check_adopt(name: &str, url: &str) {
    use phv_std::jobs::{End, Store};
    let dir = base(name);
    let tag = run_tag();
    let site = format!("adopt-{tag}");
    let (left, keyed_name, other, key) =
        (format!("left_{tag}"), format!("keyed_{tag}"), format!("other_{tag}"), format!("k_{tag}"));
    let toml = |project: &str| {
        format!("{project}[databases.q]\nurl = \"{url}\"\n[jobs]\nstore = \"db:q\"\nmax_attempts = 1\n")
    };
    write(&dir, "phv.toml", &toml(&format!("[project]\nname = \"{site}\"\n")));
    write(&dir, "public/index.phv", "<?phv echo 1;");
    // Without job scripts there is nothing a job could be matched to.
    let (ok, out) = phv(&["jobs", "adopt", &dir.to_string_lossy()]);
    assert!(ok && out.contains("nothing to adopt") && out.contains("no job scripts"), "{out}");
    write(&dir, &format!("jobs/{left}.phv"), JOB);
    write(&dir, &format!("jobs/{keyed_name}.phv"), JOB);
    let spec = phv_vm::DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 2 };
    let shared = Store::Db(spec.clone(), None);
    let own = Store::Db(spec.clone(), Some(phv_std::store_namespace(&site)));
    drain(&shared);
    drain(&own);
    // What the shared store holds besides this run's jobs (jobs of other
    // names that a run left, not yet due): counted with them below.
    let base = own.left_in_shared().unwrap() as usize;
    // Nothing of this site in the shared store yet.
    let (ok, out) = phv(&["jobs", "adopt", &dir.to_string_lossy()]);
    assert!(ok && out.contains("nothing to adopt: the shared store holds no job of this site"), "{out}");
    // What 1.1 left: a job given up, one running, two waiting (one in an
    // hour), one whose key the site's store has already, and another site's.
    let dead = shared.add(&left, "three", 0, None).unwrap().unwrap();
    let t = shared.take(Duration::from_secs(60)).unwrap().unwrap();
    assert_eq!(t.id, dead);
    shared.end(&t, End::Dead { error: "left behind" }).unwrap();
    let running = shared.add(&left, "four", 0, None).unwrap().unwrap();
    let run = shared.take(Duration::from_secs(60)).unwrap().unwrap();
    assert_eq!(run.id, running);
    let one = shared.add(&left, "one", 0, None).unwrap().unwrap();
    let two = shared.add(&left, "two", 3600, None).unwrap().unwrap();
    own.add(&keyed_name, "", 0, Some(&key)).unwrap().unwrap();
    let keyed = shared.add(&keyed_name, "", 0, Some(&key)).unwrap().unwrap();
    let theirs = shared.add(&other, "theirs", 0, None).unwrap().unwrap();
    // The ids of this run's jobs in a store.
    let ids = |store: &Store| {
        let mut ids: Vec<String> =
            store.list(false).unwrap().into_iter().filter(|j| j.name.ends_with(&tag)).map(|j| j.id).collect();
        ids.sort();
        ids
    };
    let sorted = |mut v: Vec<&String>| {
        v.sort();
        v.into_iter().cloned().collect::<Vec<String>>()
    };
    // The dry run says what would move and what is skipped, and moves nothing.
    let (ok, out) = phv(&["jobs", "adopt", &dir.to_string_lossy()]);
    assert!(ok, "{out}");
    assert!(out.contains("would move 3 jobs (2 waiting, 1 failed):"), "{out}");
    // A count per name before the jobs (found by the 18th assessment: with
    // 50,000 lines and no count, the check of the names was impractical).
    assert!(out.contains(&format!("  {left}: 2 waiting, 1 failed\n")), "{out}");
    assert!(!out.contains(&format!("\n  {keyed_name}: ")), "{out}");
    assert!(out.contains(&one) && out.contains(&two) && out.contains(&dead), "{out}");
    assert!(out.contains(&format!("skipped  {running}  running")), "{out}");
    assert!(
        out.contains(&format!("skipped  {keyed}  waiting  {keyed_name}: this site's store has a job with its key")),
        "{out}"
    );
    assert!(!out.contains(&theirs), "{out}");
    assert!(out.contains("a dry run: nothing was moved; run `phv jobs adopt --yes`"), "{out}");
    assert_eq!(ids(&shared), sorted(vec![&dead, &running, &one, &two, &keyed, &theirs]));
    assert_eq!(own.list(false).unwrap().len(), 1);
    // `--name` restricts the adoption to those of the site's job scripts.
    let (ok, out) = phv(&["jobs", "adopt", "--name", &keyed_name, &dir.to_string_lossy()]);
    assert!(ok && out.contains(&format!("skipped  {keyed}  waiting")) && !out.contains("would move"), "{out}");
    assert!(!out.contains(&one) && !out.contains(&dead) && !out.contains(&running), "{out}");
    let (ok, out) = phv(&["jobs", "adopt", "--name", &left, "--name", &keyed_name, &dir.to_string_lossy()]);
    assert!(ok && out.contains("would move 3 jobs (2 waiting, 1 failed):") && out.contains(&keyed), "{out}");
    let (ok, out) = phv(&["jobs", "adopt", "--name", "nosuch", "--yes", &dir.to_string_lossy()]);
    assert!(!ok && out.contains("--name nosuch: jobs/ has no job script nosuch.phv"), "{out}");
    assert_eq!(ids(&shared), sorted(vec![&dead, &running, &one, &two, &keyed, &theirs]));
    let note = |n: usize| {
        format!("note: {n} jobs added before this site had a namespace are still in the shared store and not run")
    };
    let (ok, out) = phv(&["jobs", "list", &dir.to_string_lossy()]);
    assert!(ok && out.contains(&note(base + 6)) && out.contains("`phv jobs adopt` moves"), "{out}");
    // The move: this site's waiting and failed jobs, with their states.
    let (ok, out) = phv(&["jobs", "adopt", "--yes", &dir.to_string_lossy()]);
    assert!(ok, "{out}");
    assert!(out.contains("moved 3 jobs (2 waiting, 1 failed):"), "{out}");
    assert!(out.contains(&format!("skipped  {running}  running")), "{out}");
    assert!(out.contains(&format!("skipped  {keyed}  waiting")), "{out}");
    assert!(out.contains(&format!("note: {} jobs are still in the shared store", base + 3)), "{out}");
    assert_eq!(ids(&shared), sorted(vec![&running, &keyed, &theirs]));
    let mine = own.list(false).unwrap();
    assert_eq!(mine.len(), 4, "{mine:?}");
    let failed = mine.iter().find(|j| j.id == dead).unwrap();
    assert_eq!((failed.state, failed.error.as_str(), failed.attempts), ("failed", "left behind", 1));
    let later = mine.iter().find(|j| j.id == two).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()) as i64;
    assert!(later.state == "waiting" && later.at > now + 3_000_000, "{later:?}");
    // `phv jobs list` no longer counts them.
    let (ok, out) = phv(&["jobs", "list", &dir.to_string_lossy()]);
    assert!(ok && out.contains(&note(base + 3)) && out.contains(&one), "{out}");
    // The moved jobs run as the site's own: the one due now first.
    let t = own.take(Duration::from_secs(60)).unwrap().unwrap();
    assert_eq!((t.id.as_str(), t.payload.as_str(), t.attempts), (one.as_str(), "one", 1));
    own.end(&t, End::Done { keyed: false }).unwrap();
    // Once more: nothing more to move.
    let (ok, out) = phv(&["jobs", "adopt", "--yes", &dir.to_string_lossy()]);
    assert!(ok && !out.contains("moved 1") && !out.contains("moved 2") && out.contains("skipped"), "{out}");
    assert_eq!(ids(&shared), sorted(vec![&running, &keyed, &theirs]));
    // Ended, the running job is moved too.
    shared.end(&run, End::Done { keyed: false }).unwrap();
    let kept = shared.add(&left, "five", 0, None).unwrap().unwrap();
    let (ok, out) = phv(&["jobs", "adopt", "--yes", &dir.to_string_lossy()]);
    assert!(ok && out.contains("moved 1 job (1 waiting, 0 failed):") && out.contains(&kept), "{out}");
    assert_eq!(ids(&shared), sorted(vec![&keyed, &theirs]));
    // A site without a namespace has the shared store as its own.
    write(&dir, "phv.toml", &toml(""));
    let (ok, out) = phv(&["jobs", "adopt", &dir.to_string_lossy()]);
    assert!(ok && out.contains("nothing to adopt: the site has no namespace"), "{out}");
    drain(&shared);
    drain(&own);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Needs `PHV_TEST_REDIS_URL`; uses database 12 of the server (as the `list`
/// test uses 11: the shared keys have one name, and the two tests run at once).
#[test]
fn adopt_moves_the_jobs_left_in_the_shared_store_with_redis() {
    let Ok(url) = std::env::var("PHV_TEST_REDIS_URL") else {
        eprintln!("PHV_TEST_REDIS_URL is not set: skipped");
        return;
    };
    let url = match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((server, _)) => format!("{scheme}://{server}/12"),
            None => format!("{url}/12"),
        },
        None => format!("{url}/12"),
    };
    check_adopt("adopt-redis", &url);
}

/// Needs `PHV_TEST_POSTGRES_URL`; a schema of its own, as
/// `list_counts_the_jobs_left_in_the_shared_store_with_postgres`.
#[test]
fn adopt_moves_the_jobs_left_in_the_shared_store_with_postgres() {
    use phv_std::migrate::{Conn, Target};
    let Ok(url) = std::env::var("PHV_TEST_POSTGRES_URL") else {
        eprintln!("PHV_TEST_POSTGRES_URL is not set: skipped");
        return;
    };
    let tag = run_tag();
    let schema = format!("phv_adopt_{tag}");
    let spec = phv_vm::DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 1 };
    let mut admin = Conn::open(&Target::Server(&spec)).unwrap();
    admin.apply(&format!("9102_adopt_{tag}.sql"), &format!("CREATE SCHEMA {schema}")).unwrap();
    let sep = if url.contains('?') { '&' } else { '?' };
    check_adopt("adopt-pg", &format!("{url}{sep}options=-c%20search_path%3D{schema}"));
    admin.apply(&format!("9103_adopt_{tag}.sql"), &format!("DROP SCHEMA {schema} CASCADE")).unwrap();
}

/// Needs `PHV_TEST_MYSQL_URL`; holds the lock of the unnamed stores.
#[test]
fn adopt_moves_the_jobs_left_in_the_shared_store_with_mysql() {
    let Ok(url) = std::env::var("PHV_TEST_MYSQL_URL") else {
        eprintln!("PHV_TEST_MYSQL_URL is not set: skipped");
        return;
    };
    let _unnamed = phv_std::testing::unnamed();
    check_adopt("adopt-mysql", &url);
}

/// A TCP proxy to `127.0.0.1:target` that holds every chunk of data `delay`
/// in each direction (a store on a network with twice that round trip); its
/// port. The threads end with the connections.
fn slow_proxy(target: u16, delay: Duration) -> u16 {
    fn pipe(mut from: TcpStream, mut to: TcpStream, delay: Duration) {
        let mut buf = vec![0u8; 65536];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    std::thread::sleep(delay);
                    if to.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to.shutdown(std::net::Shutdown::Write);
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { break };
            let Ok(server) = TcpStream::connect(("127.0.0.1", target)) else { continue };
            let (c2, s2) = (client.try_clone().unwrap(), server.try_clone().unwrap());
            std::thread::spawn(move || pipe(client, s2, delay));
            std::thread::spawn(move || pipe(server, c2, delay));
        }
    });
    port
}

/// `url` with its port replaced by `port`; the port it had.
fn with_port(url: &str, port: u16) -> (String, u16) {
    let (scheme, rest) = url.split_once("://").unwrap();
    let (host_part, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
    let (auth, host_port) = host_part.rsplit_once('@').map_or(("", host_part), |(a, h)| (a, h));
    let (host, old) = host_port.rsplit_once(':').unwrap();
    let at = if auth.is_empty() { "" } else { "@" };
    (format!("{scheme}://{auth}{at}{host}:{port}/{path}"), old.parse().unwrap())
}

/// `phv jobs adopt --yes` moves 10,200 jobs through a store 10 ms away (a
/// proxy that holds every chunk 5 ms) and every one arrives; its time is
/// compared with that of 200 single jobs added through the same proxy, so
/// the bound does not depend on the machine (found by the 18th assessment:
/// a batch took 4 round trips per job within 10 s, so a store 4 ms away
/// never moved anything, and 50,000 local jobs took 3 minutes). `run_sql`
/// runs a statement on the shared store directly (the bulk of the jobs is
/// inserted with it, and the tables are cleaned with it).
fn check_adopt_many(name: &str, url: &str, run_sql: &mut dyn FnMut(&str)) {
    use phv_std::jobs::Store;
    let dir = base(name);
    let tag = run_tag();
    let site = format!("many-{tag}");
    let job_name = format!("bulk_{tag}");
    let (proxied, target) = with_port(url, 0);
    let proxy = slow_proxy(target, Duration::from_millis(5));
    let (proxied, _) = with_port(&proxied, proxy);
    write(
        &dir,
        "phv.toml",
        &format!(
            "[project]\nname = \"{site}\"\n[databases.q]\nurl = \"{proxied}\"\n[jobs]\nstore = \"db:q\"\nmax_attempts = 1\n"
        ),
    );
    write(&dir, "public/index.phv", "<?phv echo 1;");
    write(&dir, &format!("jobs/{job_name}.phv"), JOB);
    let spec = |url: &str| phv_vm::DatabaseSpec { url: Some(url.to_string()), url_env: None, tls: None, pool_size: 2 };
    let shared = Store::Db(spec(url), None);
    let own = Store::Db(spec(url), Some(phv_std::store_namespace(&site)));
    let own_table = format!("phv_jobs_{}", phv_std::store_namespace(&site));
    drain(&shared);
    drain(&own);
    let base = own.left_in_shared().unwrap();
    // 10,000 jobs of the site's name, in a few statements.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let prefix = format!("{:016x}", nanos as u64);
    for chunk in 0..5 {
        let rows: Vec<String> = (chunk * 2000..(chunk + 1) * 2000)
            .map(|i| format!("('{prefix}{i:016x}', '{job_name}', 'p{i}', 0, 0, 0, NULL, NULL, 0, 2)"))
            .collect();
        run_sql(&format!(
            "INSERT INTO phv_jobs (id, name, payload, run_at, attempts, state, uniq, error, created, size) VALUES {}",
            rows.join(", ")
        ));
    }
    // 200 more, one by one through the proxy: the time of single statements.
    let slow = Store::Db(spec(&proxied), None);
    let started = Instant::now();
    for i in 0..200 {
        slow.add(&job_name, &format!("s{i}"), 0, None).unwrap().unwrap();
    }
    let singles = started.elapsed();
    assert_eq!(own.left_in_shared().unwrap(), base + 10_200);
    let started = Instant::now();
    let (ok, out) = phv(&["jobs", "adopt", "--yes", &dir.to_string_lossy()]);
    let moved = started.elapsed();
    assert!(ok, "{}", out.lines().take(3).chain(out.lines().rev().take(3)).collect::<Vec<_>>().join("\n"));
    assert!(out.contains("moved 10200 jobs (10200 waiting, 0 failed):"), "{}", out.lines().next().unwrap_or(""));
    assert!(out.contains(&format!("  {job_name}: 10200 waiting, 0 failed\n")), "{}", out.lines().nth(1).unwrap_or(""));
    let skipped: Vec<&str> = out.lines().filter(|l| l.starts_with("skipped ")).take(3).collect();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(own.left_in_shared().unwrap(), base, "jobs were left in the shared store");
    assert!(
        moved < singles * 5,
        "the move of 10,200 jobs took {moved:?}, 200 single jobs {singles:?} (a round trip of 10 ms)"
    );
    run_sql(&format!("DELETE FROM {own_table} WHERE name = '{job_name}'"));
    run_sql(&format!("DELETE FROM phv_jobs WHERE name = '{job_name}'"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Needs `PHV_TEST_POSTGRES_URL`; a schema of its own.
#[test]
fn adopt_moves_many_jobs_through_a_slow_store_with_postgres() {
    use phv_std::migrate::{Conn, Target};
    let Ok(url) = std::env::var("PHV_TEST_POSTGRES_URL") else {
        eprintln!("PHV_TEST_POSTGRES_URL is not set: skipped");
        return;
    };
    let tag = run_tag();
    let schema = format!("phv_many_{tag}");
    let spec = phv_vm::DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 1 };
    let mut admin = Conn::open(&Target::Server(&spec)).unwrap();
    admin.apply(&format!("9104_many_{tag}.sql"), &format!("CREATE SCHEMA {schema}")).unwrap();
    let sep = if url.contains('?') { '&' } else { '?' };
    let url = format!("{url}{sep}options=-c%20search_path%3D{schema}");
    let in_schema = phv_vm::DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 1 };
    let mut conn = Conn::open(&Target::Server(&in_schema)).unwrap();
    let mut n = 0;
    check_adopt_many("many-pg", &url, &mut |sql| {
        n += 1;
        conn.apply(&format!("9200_many_{n}_{tag}.sql"), sql).unwrap();
    });
    admin.apply(&format!("9105_many_{tag}.sql"), &format!("DROP SCHEMA {schema} CASCADE")).unwrap();
}

/// Needs `PHV_TEST_MYSQL_URL`; holds the lock of the unnamed stores.
#[test]
fn adopt_moves_many_jobs_through_a_slow_store_with_mysql() {
    use phv_std::migrate::{Conn, Target};
    let Ok(url) = std::env::var("PHV_TEST_MYSQL_URL") else {
        eprintln!("PHV_TEST_MYSQL_URL is not set: skipped");
        return;
    };
    let _unnamed = phv_std::testing::unnamed();
    let tag = run_tag();
    let spec = phv_vm::DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 1 };
    let mut conn = Conn::open(&Target::Server(&spec)).unwrap();
    let mut n = 0;
    check_adopt_many("many-mysql", &url, &mut |sql| {
        n += 1;
        conn.apply(&format!("9200_many_{n}_{tag}.sql"), sql).unwrap();
    });
}
