//! Names and locks for the tests that use the shared database servers
//! (`PHV_TEST_POSTGRES_URL`, `PHV_TEST_MYSQL_URL`, `PHV_TEST_REDIS_URL`), so
//! several test processes can run against them at once. Not part of the
//! library's interface.
//!
//! Each process has a `run_id`, which names its tables, namespaces and keys;
//! tests that must use the stores of a site without a namespace (shared by
//! every process) take the `unnamed` lock; `sweep` removes what a run that
//! died left behind, by the hour in its id.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The id of this process's run: 16 lowercase hex digits, `7`, 5 of the hour
/// (unix seconds / 3600), 6 of the process id and 4 random. The `7` marks
/// names the tests made; the hour lets `sweep` find old ones.
pub fn run_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let hour = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() / 3600);
        format!("7{:05x}{:06x}{}", hour & 0xfffff, std::process::id() & 0xff_ffff, crate::random_hex(2))
    })
}

/// A store namespace of this run (`Host::store_namespace`): the run id and
/// two hex digits of `tag`, 18 in all (the tables and keys accept any hex).
pub fn namespace(tag: u8) -> String {
    format!("{}{tag:02x}", run_id())
}

/// The namespaces the `tests/db/jobs_*.phv` scripts name in their SQL, and
/// the tag of the namespace of this run that `rewrite` puts in their place
/// (the test that runs a script gives its host `namespace(tag)`).
pub const SCRIPT_NAMESPACES: &[(&str, u8)] = &[
    ("cafe0123cafe0123cafe0123cafe0123", 0xca),
    ("f1f1f1f1f1f1f1f1", 0xf1),
    ("f2f2f2f2f2f2f2f2", 0xf2),
    ("f2f2f2f2f2f2f2f3", 0xf3),
    ("f4f4f4f4f4f4f4f4", 0xf4),
];

/// The names the `tests/db/*.phv` scripts make on a server: tables,
/// PostgreSQL types, functions and a notification channel, a MySQL database
/// and role (what lives in one session, such as a temporary table, is not
/// shared and stays as it is).
const SCRIPT_NAMES: &[&str] = &[
    "phv_items",
    "phv_async",
    "phv_pid",
    "phv_kept",
    "phv_kept_dealloc",
    "phv_jobs_dealloc",
    "phv_dealloc_a",
    "phv_dealloc_b",
    "phv_dealloc_all",
    "phv_ti_a",
    "phv_ti_b",
    "phv_ti_poison",
    "phv_mood",
    "phv_color",
    "phv_flood",
    "phv_other",
    "phv_role",
    "phv_orm_things",
    "phv_orm_orders",
    "phv_orm_tokens",
];

/// `script` with the fixed names of `tests/db/*.phv` replaced by this run's:
/// `phv_items` becomes `phv_<run_id>_items`, the job tables of
/// `SCRIPT_NAMESPACES` those of the namespaces of this run, and the Redis
/// keys `"t:…"` of the scripts `"t:<run_id>:…"`.
pub fn rewrite(script: &str) -> String {
    let script = script.replace("\"t:", &format!("\"t:{}:", run_id()));
    let mut out = String::with_capacity(script.len() + 64);
    let mut rest = script.as_str();
    while let Some(at) = rest.find("phv_") {
        let before = &rest[..at];
        // Only a whole identifier is a name (`xphv_items` is not).
        let starts = before.chars().next_back().is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        let end = rest[at..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(rest.len(), |e| at + e);
        let ident = &rest[at..end];
        out.push_str(before);
        if starts && SCRIPT_NAMES.contains(&ident) {
            out.push_str(&format!("phv_{}_{}", run_id(), &ident["phv_".len()..]));
        } else if let Some((_, tag)) =
            ident.strip_prefix("phv_jobs_").and_then(|ns| SCRIPT_NAMESPACES.iter().find(|(n, _)| *n == ns))
        {
            out.push_str(&format!("phv_jobs_{}", namespace(*tag)));
        } else {
            out.push_str(ident);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// The inverse of `rewrite` for a script's output, so an error that names a
/// table compares with the expected output as written.
pub fn restore(output: &str) -> String {
    let mut out = output.replace(&format!("phv_{}_", run_id()), "phv_").replace(&format!("t:{}:", run_id()), "t:");
    for (fixed, tag) in SCRIPT_NAMESPACES {
        out = out.replace(&namespace(*tag), fixed);
    }
    out
}

/// The lock of the stores of sites without a namespace (`phv_jobs`,
/// `phv:jobs:`, `phv_sessions`, `phv_sessions_unnamed`, `phv:sessions:*`),
/// which every process shares; held while a test uses them.
pub struct Guard {
    _process: MutexGuard<'static, ()>,
    held: Held,
}

enum Held {
    Redis(String),
    File(std::path::PathBuf),
}

/// Takes the cross-process lock of the unnamed stores (and this process's):
/// in Redis when `PHV_TEST_REDIS_URL` is set (`phv:test:unnamed`, for ten
/// minutes at most), else a file in the temporary directory (stale after
/// fifteen minutes). Waits up to ten minutes for it.
pub fn unnamed() -> Guard {
    static PROCESS: Mutex<()> = Mutex::new(());
    let process = PROCESS.lock().unwrap_or_else(|e| e.into_inner());
    let deadline = Instant::now() + Duration::from_secs(600);
    if let Some(url) = std::env::var("PHV_TEST_REDIS_URL").ok().filter(|u| !u.is_empty()) {
        match redis_lock(&url, deadline) {
            Ok(()) => return Guard { _process: process, held: Held::Redis(url) },
            Err(e) => eprintln!("the lock of the unnamed stores in Redis failed ({e}): using a file"),
        }
    }
    let dir = std::env::temp_dir().join("phv-tests");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("unnamed.lock");
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "{}", run_id());
                return Guard { _process: process, held: Held::File(path) };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(15 * 60));
                if stale {
                    let _ = std::fs::remove_file(&path);
                } else {
                    assert!(
                        Instant::now() < deadline,
                        "the lock of the unnamed stores ({}) was held ten minutes",
                        path.display()
                    );
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            Err(e) => panic!("cannot make {}: {e}", path.display()),
        }
    }
}

const UNNAMED_KEY: &[u8] = b"phv:test:unnamed";

fn redis_command(url: &str, args: &[&[u8]]) -> Result<crate::redisclient::Reply, String> {
    use crate::redisclient::RedisError;
    let until = Instant::now() + Duration::from_secs(10);
    let mut c = crate::redisclient::RedisConn::connect(url, None, until)?;
    c.command(args, until, 1 << 20).map_err(|e| match e {
        RedisError::Server(m) | RedisError::Broken(m) => m,
        RedisError::TooLarge => "the reply is too large".into(),
    })
}

fn redis_lock(url: &str, deadline: Instant) -> Result<(), String> {
    loop {
        let r = redis_command(url, &[b"SET", UNNAMED_KEY, run_id().as_bytes(), b"NX", b"PX", b"600000"])?;
        if matches!(r, crate::redisclient::Reply::Simple(_)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("held by another process for ten minutes".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        match &self.held {
            Held::Redis(url) => {
                // Only this run's lock, never one taken after ours ran out.
                let script =
                    b"if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) end return 0";
                let _ = redis_command(url, &[b"EVAL", script, b"1", UNNAMED_KEY, run_id().as_bytes()]);
            }
            Held::File(path) => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// The hour of a name made by a run, from the run id at `suffix`: `7`, five
/// hex digits of the hour, then ten more hex digits, and then either `_` (a
/// table of `rewrite`) or the two digits of a `namespace` and the end or `:`
/// (a Redis key). A site's own namespace (32 hex digits, from a hash) can
/// start with `7` too: its shape tells it apart.
fn hour_of(suffix: &str) -> Option<u64> {
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_hexdigit());
    let id = suffix.get(..16)?;
    if !id.starts_with('7') || !hex(id) {
        return None;
    }
    let rest = &suffix[16..];
    let made_by_a_run = rest.starts_with('_')
        || rest.get(..2).is_some_and(hex) && rest.get(2..).is_none_or(|r| r.is_empty() || r.starts_with(':'));
    made_by_a_run.then(|| u64::from_str_radix(&id[1..6], 16).ok())?
}

/// Whether `name` (a table, or a Redis key) was made by a run older than two
/// hours: `prefix`, then a run id with such an hour.
fn is_old(name: &str, prefix: &str, now_hour: u64) -> bool {
    name.strip_prefix(prefix).and_then(hour_of).is_some_and(|h| h + 2 < now_hour)
}

/// Removes from the server at `url` what runs older than two hours left:
/// tables `phv_7…`, `phv_jobs_7…` and `phv_sessions_7…` (PostgreSQL types
/// and functions, and MySQL databases and roles, named so too), or Redis keys
/// `phv:jobs:7…` and `phv:sessions:7…`. Never anything of a run of the last
/// two hours. Failures are reported on stderr: the tests go on without the sweep.
pub fn sweep(url: &str) {
    let now_hour = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() / 3600);
    let old = |name: &str| ["phv_jobs_", "phv_sessions_", "phv_"].iter().any(|p| is_old(name, p, now_hour));
    let result = if url.starts_with("redis") {
        sweep_redis(url, now_hour)
    } else if url.starts_with("mysql") {
        sweep_mysql(url, &old)
    } else {
        sweep_postgres(url, &old)
    };
    if let Err(e) = result {
        eprintln!("sweeping the leftovers of old test runs failed: {e}");
    }
}

fn sweep_postgres(url: &str, old: &dyn Fn(&str) -> bool) -> Result<(), String> {
    let mut c = postgres::Client::connect(url, postgres::NoTls).map_err(|e| e.to_string())?;
    let e = |e: postgres::Error| e.to_string();
    let tables = c
        .query("SELECT table_name::text FROM information_schema.tables WHERE table_schema = current_schema()", &[])
        .map_err(e)?;
    for t in tables.iter().map(|r| r.get::<_, String>(0)).filter(|t| old(t)) {
        c.batch_execute(&format!("DROP TABLE IF EXISTS \"{t}\"")).map_err(e)?;
    }
    let functions = c
        .query("SELECT proname::text FROM pg_proc WHERE pronamespace = current_schema()::regnamespace", &[])
        .map_err(e)?;
    for f in functions.iter().map(|r| r.get::<_, String>(0)).filter(|f| old(f)) {
        c.batch_execute(&format!("DROP FUNCTION IF EXISTS \"{f}\"")).map_err(e)?;
    }
    let types = c
        .query(
            "SELECT typname::text FROM pg_type WHERE typtype = 'e' AND typnamespace = current_schema()::regnamespace",
            &[],
        )
        .map_err(e)?;
    for t in types.iter().map(|r| r.get::<_, String>(0)).filter(|t| old(t)) {
        c.batch_execute(&format!("DROP TYPE IF EXISTS \"{t}\"")).map_err(e)?;
    }
    // The counts the job stores keep of their tables.
    if let Ok(rows) = c.query("SELECT t FROM phv_jobs_counts", &[]) {
        for t in rows.iter().map(|r| r.get::<_, String>(0)).filter(|t| old(t)) {
            c.execute("DELETE FROM phv_jobs_counts WHERE t = $1", &[&t]).map_err(e)?;
        }
    }
    Ok(())
}

fn sweep_mysql(url: &str, old: &dyn Fn(&str) -> bool) -> Result<(), String> {
    use mysql::prelude::Queryable;
    let opts = mysql::Opts::from_url(url).map_err(|e| e.to_string())?;
    let mut c = mysql::Conn::new(opts).map_err(|e| e.to_string())?;
    let e = |e: mysql::Error| e.to_string();
    let tables: Vec<String> =
        c.query("SELECT table_name FROM information_schema.tables WHERE table_schema = database()").map_err(e)?;
    for t in tables.iter().filter(|t| old(t)) {
        c.query_drop(format!("DROP TABLE IF EXISTS `{t}`")).map_err(e)?;
    }
    let databases: Vec<String> = c.query("SHOW DATABASES").map_err(e)?;
    for d in databases.iter().filter(|d| old(d)) {
        c.query_drop(format!("DROP DATABASE IF EXISTS `{d}`")).map_err(e)?;
    }
    if let Ok(roles) = c.query::<String, _>("SELECT user FROM mysql.user WHERE host = '%'") {
        for r in roles.iter().filter(|r| old(r)) {
            c.query_drop(format!("DROP ROLE IF EXISTS `{r}`")).map_err(e)?;
        }
    }
    for table in ["phv_jobs_counts", "phv_jobs_lock"] {
        if let Ok(rows) = c.query::<String, _>(format!("SELECT t FROM {table}")) {
            for t in rows.iter().filter(|t| old(t)) {
                c.exec_drop(format!("DELETE FROM {table} WHERE t = ?"), (t,)).map_err(e)?;
            }
        }
    }
    Ok(())
}

fn sweep_redis(url: &str, now_hour: u64) -> Result<(), String> {
    use crate::redisclient::Reply;
    let text = |r: &Reply| match r {
        Reply::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
        Reply::Simple(s) => s.clone(),
        _ => String::new(),
    };
    for prefix in ["phv:jobs:", "phv:sessions:"] {
        let pattern = format!("{prefix}7*");
        let mut cursor = "0".to_string();
        loop {
            let r = redis_command(url, &[b"SCAN", cursor.as_bytes(), b"MATCH", pattern.as_bytes(), b"COUNT", b"1000"])?;
            let Reply::Array(parts) = r else { return Err("SCAN did not answer with an array".into()) };
            let [next, Reply::Array(keys)] = parts.as_slice() else { return Err("SCAN answered oddly".into()) };
            let old: Vec<String> = keys.iter().map(text).filter(|k| is_old(k, prefix, now_hour)).collect();
            if !old.is_empty() {
                let mut args: Vec<&[u8]> = vec![b"DEL"];
                args.extend(old.iter().map(|k| k.as_bytes()));
                redis_command(url, &args)?;
            }
            cursor = text(next);
            if cursor == "0" {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_namespaces_are_hex_of_this_run() {
        let id = run_id();
        assert_eq!(id.len(), 16);
        assert!(id.starts_with('7') && id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        assert_eq!(run_id(), id, "one id per process");
        let ns = namespace(0xab);
        assert_eq!(ns.len(), 18);
        assert!(ns.starts_with(id) && ns.ends_with("ab"));
        assert_ne!(namespace(1), namespace(2));
    }

    #[test]
    fn scripts_are_rewritten_to_this_run_and_outputs_restored() {
        let id = run_id();
        let script = "DROP TABLE phv_items; SELECT phv_kept_dealloc(); xphv_items phv_itemsx \
                      DELETE FROM phv_jobs_f1f1f1f1f1f1f1f1 LIKE '%phv_jobs_f1f1f1f1f1f1f1f1%' phv_jobs_counts \
                      $r->get(\"t:greeting\")";
        let out = rewrite(script);
        assert_eq!(
            out,
            format!(
                "DROP TABLE phv_{id}_items; SELECT phv_{id}_kept_dealloc(); xphv_items phv_itemsx \
                 DELETE FROM phv_jobs_{ns} LIKE '%phv_jobs_{ns}%' phv_jobs_counts $r->get(\"t:{id}:greeting\")",
                ns = namespace(0xf1)
            )
        );
        assert_eq!(restore(&out), script);
        assert_eq!(rewrite("no names"), "no names");
    }

    #[test]
    fn old_names_are_told_by_their_hour() {
        let now = 0x7a000;
        assert!(is_old("phv_jobs_77a000123456abcdef", "phv_jobs_", now + 3));
        assert!(!is_old("phv_jobs_77a000123456abcdef", "phv_jobs_", now + 2));
        assert!(!is_old("phv_jobs_77a000123456abcdef", "phv_jobs_", now));
        assert!(is_old("phv_77a000123456abcd_items", "phv_", now + 3));
        assert!(is_old("phv:sessions:77a000123456abcdef:x", "phv:sessions:", now + 3));
        assert!(!is_old("phv_jobs_cafe0123cafe0123cafe0123cafe0123", "phv_jobs_", now + 3));
        assert!(!is_old("phv_jobs", "phv_jobs_", now + 3));
        assert!(!is_old("phv_sessions_unnamed", "phv_", now + 3), "not a run id");
        assert!(!is_old("phv_jobs_7zzzzz123456abcd", "phv_jobs_", now + 3), "not hex");
        // A site's namespace from a hash (32 hex digits) is never a run's.
        assert!(!is_old("phv_jobs_77a000123456abcdef0123456789abcdef", "phv_jobs_", now + 3));
        assert!(!is_old("phv:jobs:77a000123456abcdef0123456789abcdef:job", "phv:jobs:", now + 3));
        assert!(!is_old("phv_77a000123456abcd", "phv_", now + 3), "a bare run id names nothing");
        assert!(is_old("phv:jobs:77a000123456abcdef", "phv:jobs:", now + 3));
        assert!(is_old("phv:jobs:77a000123456abcdef:key", "phv:jobs:", now + 3));
    }

    /// The lock of the unnamed stores is held by one thread of this process at
    /// a time, and taken again once it is dropped.
    #[test]
    fn the_unnamed_lock_is_taken_in_turn() {
        let first = unnamed();
        let taken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let t = {
            let taken = taken.clone();
            std::thread::spawn(move || {
                let _g = unnamed();
                taken.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(!taken.load(std::sync::atomic::Ordering::SeqCst));
        drop(first);
        t.join().unwrap();
        assert!(taken.load(std::sync::atomic::Ordering::SeqCst));
    }
}
