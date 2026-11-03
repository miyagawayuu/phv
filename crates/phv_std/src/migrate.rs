//! `phv db migrate` / `phv db status`: applies the SQL files of `migrations/`
//! in order, once each, and records them in the table `phv_migrations` with a
//! checksum, so a file changed after it was applied is noticed.

use crate::netdb::{migration_conn, MigrationConn};
use phv_vm::DatabaseSpec;
use std::path::{Path, PathBuf};

/// A migration file: `0001_create_posts.sql`.
#[derive(Debug)]
pub struct Migration {
    pub name: String,
    pub path: PathBuf,
}

/// The database migrations run on.
pub enum Target<'a> {
    /// A SQLite file (inside the project).
    Sqlite(PathBuf),
    /// A database server of `[databases]`.
    Server(&'a DatabaseSpec),
}

pub enum Conn {
    Sqlite(Box<rusqlite::Connection>),
    Server(MigrationConn),
}

/// The migration files of `dir`, in order. Names start with digits and `_`
/// and end in `.sql`; other files are an error, so a typo is not skipped.
pub fn files(dir: &Path) -> Result<Vec<Migration>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let digits = name.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || name.as_bytes().get(digits) != Some(&b'_') || !name.ends_with(".sql") {
            return Err(format!("{}: migration files are named like `0001_create_posts.sql`", e.path().display()));
        }
        out.push(Migration { name, path: e.path() });
    }
    // By number first, so `10_x` comes after `9_y` whatever the padding.
    let num = |n: &str| n.split('_').next().and_then(|d| d.parse::<u64>().ok()).unwrap_or(u64::MAX);
    out.sort_by(|a, b| num(&a.name).cmp(&num(&b.name)).then_with(|| a.name.cmp(&b.name)));
    for w in out.windows(2) {
        if num(&w[0].name) == num(&w[1].name) {
            return Err(format!("{} and {} have the same number", w[0].name, w[1].name));
        }
    }
    Ok(out)
}

pub fn checksum(sql: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(sql.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

impl Conn {
    pub fn open(target: &Target) -> Result<Conn, String> {
        match target {
            Target::Sqlite(path) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                }
                let c = rusqlite::Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS phv_migrations (name TEXT PRIMARY KEY, checksum TEXT NOT NULL, \
                     applied_at TEXT NOT NULL)",
                )
                .map_err(|e| e.to_string())?;
                Ok(Conn::Sqlite(Box::new(c)))
            }
            Target::Server(spec) => migration_conn(spec).map(Conn::Server),
        }
    }

    pub fn applied(&mut self) -> Result<Vec<(String, String)>, String> {
        match self {
            Conn::Sqlite(c) => {
                let mut st =
                    c.prepare("SELECT name, checksum FROM phv_migrations ORDER BY name").map_err(|e| e.to_string())?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?;
                rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
            }
            Conn::Server(c) => c.applied(),
        }
    }

    /// Runs one migration and records it, in one transaction where the database allows.
    pub fn apply(&mut self, name: &str, sql: &str) -> Result<(), String> {
        let sum = checksum(sql);
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        match self {
            Conn::Sqlite(c) => {
                let tx = c.transaction().map_err(|e| e.to_string())?;
                tx.execute_batch(sql).map_err(|e| e.to_string())?;
                tx.execute(
                    "INSERT INTO phv_migrations (name, checksum, applied_at) VALUES (?1, ?2, ?3)",
                    [name, sum.as_str(), at.as_str()],
                )
                .map_err(|e| e.to_string())?;
                tx.commit().map_err(|e| e.to_string())
            }
            Conn::Server(c) => c.apply(name, &sum, sql, &at),
        }
    }
}

/// What `status` reports for each file.
#[derive(Debug, PartialEq)]
pub enum State {
    Applied,
    Pending,
    /// Applied, but the file has changed since.
    Changed,
}

/// The state of each migration file, and the applied names whose file is gone.
#[allow(clippy::type_complexity)]
pub fn plan(conn: &mut Conn, files: &[Migration]) -> Result<(Vec<(String, State)>, Vec<String>), String> {
    let applied = conn.applied()?;
    let mut states = Vec::new();
    for f in files {
        let sql = std::fs::read_to_string(&f.path).map_err(|e| format!("cannot read {}: {e}", f.path.display()))?;
        let state = match applied.iter().find(|(n, _)| *n == f.name) {
            None => State::Pending,
            Some((_, sum)) if *sum == checksum(&sql) => State::Applied,
            Some(_) => State::Changed,
        };
        states.push((f.name.clone(), state));
    }
    let gone = applied.into_iter().filter(|(n, _)| !files.iter().any(|f| f.name == *n)).map(|(n, _)| n).collect();
    Ok((states, gone))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_run_once_in_order() {
        let dir = std::env::temp_dir().join(format!("phv-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("m")).unwrap();
        let write = |n: &str, sql: &str| std::fs::write(dir.join("m").join(n), sql).unwrap();
        write("10_more.sql", "INSERT INTO t VALUES (10);");
        write("2_table.sql", "CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (2);");
        let names: Vec<String> = files(&dir.join("m")).unwrap().into_iter().map(|m| m.name).collect();
        assert_eq!(names, ["2_table.sql", "10_more.sql"]);

        let target = Target::Sqlite(dir.join("db/app.db"));
        let mut conn = Conn::open(&target).unwrap();
        for m in files(&dir.join("m")).unwrap() {
            conn.apply(&m.name, &std::fs::read_to_string(&m.path).unwrap()).unwrap();
        }
        let (states, gone) = plan(&mut conn, &files(&dir.join("m")).unwrap()).unwrap();
        assert!(states.iter().all(|(_, s)| *s == State::Applied) && gone.is_empty());
        // A failing migration leaves nothing behind.
        assert!(conn.apply("3_bad.sql", "INSERT INTO t VALUES (3); SELECT nope FROM missing;").is_err());
        let Conn::Sqlite(c) = &conn else { unreachable!() };
        let rows: i64 = c.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2);
        write("2_table.sql", "CREATE TABLE t (n INTEGER);");
        let (states, _) = plan(&mut conn, &files(&dir.join("m")).unwrap()).unwrap();
        assert_eq!(states[0].1, State::Changed);

        write("02_dup.sql", "");
        assert!(files(&dir.join("m")).unwrap_err().contains("the same number"));
        std::fs::remove_file(dir.join("m/02_dup.sql")).unwrap();
        write("notes.txt", "");
        assert!(files(&dir.join("m")).is_err());
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
