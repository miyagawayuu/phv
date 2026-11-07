//! Database servers named in phv.toml: PostgreSQL and MySQL behind the
//! `Database` class, and Redis. Scripts only name a server
//! (`Database::connect("main")`); the URL and its credentials stay in the
//! configuration.
//!
//! Connections come from a pool shared by every script in the process. A
//! connection goes back to the pool when the script ends or closes it, after
//! its session is reset (open transaction rolled back, settings cleared), so
//! nothing carries over from one request to the next.
//!
//! The script's limits hold on the client side too: statements stop at its
//! deadline even when the server ignores (or the script switched off) the
//! server's own timeout, and a result larger than the memory left to the
//! script is refused while it is read, before it fills the process.

use phv_vm::{Value, VmError};
use std::time::Duration;

mod handle;
mod jobs;
mod jobs_add;
mod jobs_run;
mod migrate;
mod my;
mod pg;
mod pool;
mod publish;
mod query_async;
mod ratelimit;
mod redis;
mod sessions;

pub use handle::{connect, connect_redis, execute, last_insert_id, query, raw};
pub(crate) use handle::{is_local_host, tls_init};
pub use jobs::{JobCap, JobInfo, JobRow, JOB_MAX_DEAD, JOB_MAX_KEPT};
pub(crate) use jobs_add::job_add_in_transaction;
pub use jobs_add::{job_add_by, JobAdder, JOB_KEY_WAITERS};
pub use jobs_run::{
    job_adopt, job_count_shared, job_end, job_list, job_requeue, job_store_is_shared, job_store_ready, job_sweep,
    job_take, Adopted, AdoptedJob, JobEnd,
};
pub use migrate::{migration_conn, MigrationConn};
pub(crate) use my::MAX_KILLING;
pub(crate) use pg::pg_message;
pub use publish::{redis_publish, redis_subscribe, Pushed, RedisSubscriber};
pub(crate) use query_async::{detached_value, Detached};
pub use query_async::{start_query, start_redis};
pub use ratelimit::{redis_command, redis_pipeline};
pub(crate) use ratelimit::{redis_hit, redis_reset, redis_value};
pub use sessions::{
    session_insert, session_read, session_remove, session_sweep, session_update, SessionLimits, SessionRow,
    SESSIONS_OF_1_1,
};

#[cfg(test)]
use crate::db::{render_sql, Dialect};
#[cfg(test)]
use crate::redisclient::Reply;
#[cfg(test)]
use handle::Kind;
#[cfg(test)]
use jobs::{job_conn, job_sized, jobs_table, now_ms, JOB_SIZE_RETRY, JOB_TABLES, JOB_UNSIZED};
#[cfg(test)]
use jobs_add::job_add;
#[cfg(test)]
use my::{my_connect, my_statements};
#[cfg(test)]
use pg::{
    numeric_decode, numeric_encode, pg_connect, pg_text, refused_statement, statement_name, uuid_parse, PgConn, PgText,
};
#[cfg(test)]
use phv_vm::DatabaseSpec;
#[cfg(test)]
use pool::{lock, Driver};
#[cfg(test)]
use query_async::PoolUse;
#[cfg(test)]
use ratelimit::redis_conn;
#[cfg(test)]
use sessions::{redis_run, SessionConn};
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::time::{Instant, SystemTime, UNIX_EPOCH};

type R = Result<Value, VmError>;

/// Longest wait for a free connection or a new one when the script has no time limit.
const MAX_WAIT: Duration = Duration::from_secs(30);
/// Longest wait for a server to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection idle for longer is checked before it is handed out again.
const CHECK_AFTER: Duration = Duration::from_secs(30);
/// Longest time for the statements that reset or check a pooled connection.
const RESET_TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes those statements (and a connection's setup) may read.
const RESET_BUDGET: usize = 16 << 20;

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(t: &str) -> String {
        let mut b = bytes::BytesMut::new();
        numeric_encode(t, &mut b).unwrap();
        numeric_decode(&b).unwrap()
    }

    #[test]
    fn numeric_round_trips() {
        for (input, want) in [
            ("0", "0"),
            ("1", "1"),
            ("-1", "-1"),
            ("12345678", "12345678"),
            ("10000", "10000"),
            ("0.5", "0.5"),
            ("-0.0001", "-0.0001"),
            ("123.4500", "123.4500"),
            ("0.000012", "0.000012"),
            ("99999999999999999999.99", "99999999999999999999.99"),
            ("+7", "7"),
            ("007.10", "7.10"),
            ("-0", "0"),
            ("NaN", "NaN"),
        ] {
            assert_eq!(roundtrip(input), want, "{input}");
        }
        let mut b = bytes::BytesMut::new();
        for bad in ["", ".", "1e5", "abc", "1.2.3", "--1"] {
            assert!(numeric_encode(bad, &mut b).is_err(), "{bad}");
        }
    }

    #[test]
    fn prepared_transactions_are_refused() {
        for (text, kind) in [
            ("XA PREPARE 'x'", Kind::MySql),
            ("  xa\tprepare 'x';", Kind::MySql),
            ("/* two phase */ XA PREPARE 'x'", Kind::MySql),
            ("-- a note\nXA PREPARE 'x'", Kind::MySql),
            ("# a note\nXA PREPARE 'x'", Kind::MySql),
            ("/*!50000 XA PREPARE 'x' */", Kind::MySql),
            ("XA/**/PREPARE 'x'", Kind::MySql),
            ("XA/* a */PREPARE/* b */'x'", Kind::MySql),
            ("XA-- a note\nPREPARE 'x'", Kind::MySql),
            ("XA#a note\nPREPARE 'x'", Kind::MySql),
            // MySQL ends a comment at the first `*/`: the statement is XA PREPARE.
            ("/* /* */ XA PREPARE 'x'", Kind::MySql),
            ("LOAD DATA LOCAL INFILE '/etc/passwd' INTO TABLE t", Kind::MySql),
            ("LOAD/**/DATA/**/LOCAL INFILE 'f' INTO TABLE t", Kind::MySql),
            ("PREPARE/**/TRANSACTION 'x'", Kind::Postgres),
            // PostgreSQL nests comments: the statement after the comment is PREPARE TRANSACTION.
            ("/* /* */ SELECT 1 */ PREPARE TRANSACTION 'x'", Kind::Postgres),
            ("LOAD DATA LOW_PRIORITY LOCAL INFILE 'f' INTO TABLE t", Kind::MySql),
            ("PREPARE TRANSACTION 'x'", Kind::Postgres),
            ("prepare transaction 'x'", Kind::Postgres),
        ] {
            assert!(refused_statement(text, kind).is_some_and(|why| why.contains("is not allowed")), "{text}");
        }
        for (text, kind) in [
            ("XA START 'x'", Kind::MySql),
            ("XA COMMIT 'x' ONE PHASE", Kind::MySql),
            ("XA ROLLBACK 'x'", Kind::MySql),
            ("XA RECOVER", Kind::MySql),
            ("PREPARE s FROM 'SELECT 1'", Kind::MySql),
            ("LOAD DATA INFILE '/tmp/f' INTO TABLE t", Kind::MySql),
            ("SELECT 'XA PREPARE'", Kind::MySql),
            ("INSERT INTO t VALUES ('PREPARE TRANSACTION')", Kind::Postgres),
            ("PREPARE s AS SELECT 1", Kind::Postgres),
            ("/* /* */ PREPARE TRANSACTION */ SELECT 2", Kind::Postgres),
            ("COMMIT PREPARED 'x'", Kind::Postgres),
            ("ROLLBACK PREPARED 'x'", Kind::Postgres),
            ("XA PREPARE 'x'", Kind::Postgres),
            ("PREPARE TRANSACTION 'x'", Kind::MySql),
        ] {
            assert!(refused_statement(text, kind).is_none(), "{text}");
        }
    }

    #[test]
    fn uuid_parses() {
        let u = uuid_parse("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(u[0], 0x55);
        assert_eq!(u[15], 0x00);
        assert!(uuid_parse("550e8400").is_none());
        assert!(uuid_parse("zz0e8400-e29b-41d4-a716-446655440000").is_none());
    }

    #[test]
    fn local_hosts() {
        for h in ["localhost", "LOCALHOST", "127.0.0.1", "::1", "[::1]"] {
            assert!(is_local_host(h), "{h}");
        }
        for h in ["db.example.com", "10.0.0.5", "localhost.example.com"] {
            assert!(!is_local_host(h), "{h}");
        }
    }

    #[test]
    fn statement_texts_are_told_apart() {
        for (text, kind) in [
            ("SELECT * FROM t", PgText::Plain),
            ("  select 1;", PgText::Plain),
            ("INSERT INTO t VALUES ($1)", PgText::Plain),
            ("WITH x AS (SELECT 1) SELECT * FROM x", PgText::Plain),
            ("SELECT * INTO t2 FROM t", PgText::Other),
            ("BEGIN", PgText::Begin),
            ("start transaction", PgText::Begin),
            ("COMMIT", PgText::End),
            ("rollback;", PgText::End),
            ("END WORK", PgText::End),
            ("ROLLBACK TO SAVEPOINT a", PgText::Other),
            ("COMMIT AND CHAIN", PgText::Other),
            ("ALTER TABLE t ADD COLUMN c int", PgText::Other),
            ("DEALLOCATE ALL", PgText::Other),
            ("/* c */ SELECT 1", PgText::Other),
            ("", PgText::Other),
        ] {
            assert_eq!(pg_text(text), kind, "{text}");
        }
    }

    /// The servers of the database tests (see crates/phv_driver/tests/databases.rs), when set.
    fn test_servers() -> Vec<(&'static str, DatabaseSpec)> {
        ["PHV_TEST_POSTGRES_URL", "PHV_TEST_MYSQL_URL", "PHV_TEST_REDIS_URL"]
            .into_iter()
            .filter_map(|var| {
                let url = std::env::var(var).ok().filter(|u| !u.is_empty());
                if url.is_none() {
                    eprintln!("skipped: {var} is not set");
                }
                Some((var, DatabaseSpec { url: Some(url?), url_env: None, tls: None, pool_size: 4 }))
            })
            .collect()
    }

    /// A database job store holds at most `JobCap::bytes` of payloads, as the
    /// memory store does, and ending a job makes room again (found by the 1.1.1
    /// audit: only the jobs were counted, so a store could hold 6.4 GB).
    #[test]
    fn job_stores_cap_the_bytes_of_payloads() {
        let ns = crate::testing::namespace(0x30);
        let ns = Some(ns.as_str());
        for (var, spec) in test_servers() {
            let clear = || {
                while let Some(j) = job_take(&spec, ns, 60_000).unwrap() {
                    job_end(&spec, ns, &j, JobEnd::Done { keyed: false }).unwrap();
                }
                job_sweep(&spec, ns).unwrap();
            };
            clear();
            let cap = JobCap { jobs: 1000, bytes: 100_000 };
            let add = |size: usize| {
                let (id, payload) = (crate::random_hex(16), "x".repeat(size));
                job_add(&spec, ns, (&id, "big", &payload), now_ms(), None, cap, None)
            };
            for _ in 0..3 {
                assert_eq!(add(30_000), Ok(Some(())), "{var}");
            }
            let err = add(30_000).unwrap_err();
            assert!(err.starts_with("the job queue holds 1000 jobs or 0 MB"), "{var}: {err}");
            assert_eq!(add(1_000), Ok(Some(())), "{var}: a small job still fits");
            let j = job_take(&spec, ns, 60_000).unwrap().unwrap();
            assert!(job_end(&spec, ns, &j, JobEnd::Done { keyed: false }).unwrap());
            // Counts of the database are a second old at most.
            std::thread::sleep(Duration::from_millis(1100));
            assert_eq!(add(30_000), Ok(Some(())), "{var}: no room after a job ended");
            clear();
        }
    }

    /// A jobs table made before 1.1.1 gets its `size` column when first used;
    /// while another transaction holds the table, the store goes on without
    /// it (the bytes are not counted) instead of waiting, and adds it later.
    #[test]
    fn a_jobs_table_of_1_1_gets_its_size_column_later_when_held() {
        let ns = crate::testing::namespace(0x31);
        let ns = Some(ns.as_str());
        let t = jobs_table(ns);
        for (var, spec) in test_servers() {
            if var == "PHV_TEST_REDIS_URL" {
                continue;
            }
            let url = spec.url.clone().unwrap();
            let old = format!(
                "CREATE TABLE {t} (id varchar(32) PRIMARY KEY, name varchar(64) NOT NULL, payload text NOT NULL, \
                 run_at bigint NOT NULL, attempts integer NOT NULL, state smallint NOT NULL, lease varchar(32), \
                 lease_until bigint, uniq varchar(64) UNIQUE, error text, created bigint NOT NULL)"
            );
            let has_size = |spec: &DatabaseSpec| -> bool {
                let (mut conn, _) = job_conn(spec, ns).unwrap();
                let sql = format!(
                    "SELECT count(*) FROM information_schema.columns WHERE table_name = '{t}' \
                                   AND column_name = 'size'"
                );
                match &mut conn {
                    SessionConn::Pg(c) => c.get().client.query(&sql, &[]).unwrap()[0].get::<_, i64>(0) > 0,
                    SessionConn::My(c) => {
                        use mysql::prelude::Queryable;
                        c.get().conn.query_first::<i64, _>(sql).unwrap().unwrap() > 0
                    }
                    SessionConn::Redis(_) => unreachable!(),
                }
            };
            let wait = Duration::from_secs(5);
            // The table as 1.1 made it, held by an open transaction.
            let mut holder: Box<dyn FnMut(&str)> = if var == "PHV_TEST_POSTGRES_URL" {
                let c = pg_connect(&url, &spec, wait).unwrap();
                c.client.batch_execute(&format!("DROP TABLE IF EXISTS {t}; {old}")).unwrap();
                c.client.batch_execute(&format!("BEGIN; LOCK TABLE {t} IN ACCESS SHARE MODE")).unwrap();
                Box::new(move |sql| c.client.batch_execute(sql).unwrap())
            } else {
                use mysql::prelude::Queryable;
                let mut c = my_connect(&url, &spec, wait, None, None).unwrap();
                c.conn.query_drop(format!("DROP TABLE IF EXISTS {t}")).unwrap();
                c.conn.query_drop(&old).unwrap();
                c.conn.query_drop("START TRANSACTION").unwrap();
                c.conn.query_drop(format!("SELECT * FROM {t} LIMIT 1")).unwrap();
                Box::new(move |sql| c.conn.query_drop(sql).unwrap())
            };
            lock(&JOB_TABLES).remove(&format!("{url}|{t}"));
            lock(&JOB_UNSIZED).remove(&t);
            let started = Instant::now();
            let cap = JobCap { jobs: 1000, bytes: 1 << 20 };
            let add = || job_add(&spec, ns, (&crate::random_hex(16), "j", "payload"), now_ms(), None, cap, None);
            assert_eq!(add(), Ok(Some(())), "{var}");
            assert!(started.elapsed() < Duration::from_secs(3), "{var}: waited {:?}", started.elapsed());
            assert!(!job_sized(&t), "{var}: the column was added under the open transaction");
            let j = job_take(&spec, ns, 60_000).unwrap().unwrap();
            assert!(job_end(&spec, ns, &j, JobEnd::Done { keyed: false }).unwrap(), "{var}");
            // Once the transaction ended, the next try adds it.
            holder("COMMIT");
            lock(&JOB_UNSIZED).insert(t.clone(), Instant::now() - JOB_SIZE_RETRY);
            assert_eq!(add(), Ok(Some(())), "{var}");
            assert!(job_sized(&t) && has_size(&spec), "{var}: the column was not added");
            holder(&format!("DROP TABLE {t}"));
            lock(&JOB_TABLES).remove(&format!("{url}|{t}"));
        }
    }

    /// A site with a namespace also clears what 1.1 left in the shared session
    /// store once no site can use it, and nothing that is still alive (found
    /// by the 1.1.1 audit: only sites without a namespace swept it).
    #[test]
    fn sites_with_a_namespace_sweep_what_1_1_left() {
        // The store every site shared in 1.1 is one for every test process.
        let _unnamed = crate::testing::unnamed();
        let sweeper = crate::testing::namespace(0x32);
        let limits = SessionLimits {
            ttl: Duration::from_secs(3600),
            max_lifetime: Duration::from_secs(24 * 3600),
            max_sessions: 50_000,
            max_bytes: 256 << 20,
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let row = |created: u64, last: u64| SessionRow { data: "{}".into(), created, last, returned: true };
        let old = Some(SESSIONS_OF_1_1);
        for (var, spec) in test_servers() {
            let dead = format!("a-i2-dead-{}", crate::random_hex(8));
            let alive = format!("a-i2-alive-{}", crate::random_hex(8));
            if var == "PHV_TEST_REDIS_URL" {
                // As 1.1 stored them: the dead one has a second of its lifetime left.
                let lifetime = limits.max_lifetime.as_secs();
                assert!(session_insert(&spec, old, &dead, &row(now - lifetime + 1, now), &limits).unwrap());
                assert!(session_insert(&spec, old, &alive, &row(now, now), &limits).unwrap());
                std::thread::sleep(Duration::from_millis(2100));
                let until = Instant::now() + Duration::from_secs(5);
                let mut c = redis_conn(&spec, PoolUse::Sessions, until).unwrap();
                let mut counted = |key: &str| {
                    let r = redis_run(&mut c, &[b"ZSCORE", b"phv:sessions:back", key.as_bytes()], until).unwrap();
                    !matches!(r, Reply::Nil)
                };
                assert!(session_read(&spec, old, &dead).unwrap().is_none(), "Redis expired it");
                assert!(counted(&dead), "still counted in the shared bookkeeping");
                session_sweep(&spec, Some(&sweeper), &limits).unwrap();
                assert!(!counted(&dead), "{var}: the shared bookkeeping of an expired session was kept");
                assert!(counted(&alive), "{var}: a live session was forgotten");
            } else {
                let day = 24 * 3600;
                assert!(session_insert(&spec, old, &dead, &row(now - day - 60, now - day - 60), &limits).unwrap());
                assert!(session_insert(&spec, old, &alive, &row(now - 100, now - 100), &limits).unwrap());
                session_sweep(&spec, Some(&sweeper), &limits).unwrap();
                assert!(session_read(&spec, old, &dead).unwrap().is_none(), "{var}: an expired session was kept");
                assert!(session_read(&spec, old, &alive).unwrap().is_some(), "{var}: a live session was removed");
            }
            session_remove(&spec, old, &alive).unwrap();
        }
    }

    /// A site without a namespace keeps its sessions apart from the store
    /// every site shared in 1.1: its short ttl and small limits do not remove
    /// the sessions other sites kept there, and its own sessions of 1.1 are
    /// still read (found by the 1.2 audit: it swept the shared store by its
    /// own ttl and limits).
    /// The reset knows the client's statements by the names the server
    /// lists: those the client shows (`statement_name`) and the sentinel's.
    #[test]
    fn statements_are_known_by_their_names() {
        let Some(url) = std::env::var("PHV_TEST_POSTGRES_URL").ok().filter(|u| !u.is_empty()) else { return };
        let spec = DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 1 };
        let mut c = pg_connect(&url, &spec, Duration::from_secs(10)).unwrap();
        let stmt = c.client.run(None, c.client.client().prepare("SELECT 2 AS phv_named")).unwrap();
        let name = statement_name(&stmt).expect("the client shows the name");
        c.keep("SELECT 2 AS phv_named", &stmt);
        let listed = c.client.simple_last_column("SELECT name FROM pg_prepared_statements WHERE NOT from_sql").unwrap();
        assert!(listed.contains(&name) && listed.contains(&c.own[0]), "{listed:?}");
        assert!(c.reset(), "a reset of a sound connection");
        assert_eq!(c.own.len(), 1, "a kept statement is not one of the client's own");
    }

    /// A schema change made by one script is seen by the next script on the
    /// same connection through a kept statement, with no `DISCARD PLANS` in
    /// the reset: the server drops the plan itself when a table the statement
    /// uses changes, and plans it again from its text (a table dropped and
    /// made again), or the statement fails as stale and is prepared again (a
    /// column added or changed, which changes the rows' shape).
    #[test]
    fn a_schema_change_is_seen_through_a_kept_statement_after_the_reset() {
        let Some(url) = std::env::var("PHV_TEST_POSTGRES_URL").ok().filter(|u| !u.is_empty()) else { return };
        let spec = DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 2 };
        let wait = Duration::from_secs(10);
        let mut c = pg_connect(&url, &spec, wait).unwrap();
        // Another process's connection.
        let other = pg_connect(&url, &spec, wait).unwrap();
        let t = format!("phv_{}_plans", crate::testing::run_id());
        let select = format!("SELECT * FROM {t}");
        c.client
            .batch_execute(&format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} (a int); INSERT INTO {t} VALUES (1)"))
            .unwrap();
        assert_eq!(c.query_kept(&select, &[]).unwrap()[0].get::<_, i32>("a"), 1);
        let prepared_as =
            |c: &PgConn| c.kept.iter().find(|k| k.text == select).and_then(|k| k.name.clone()).expect("kept");
        let first = prepared_as(&c);

        // The script adds a column; the next script sees it. The kept
        // statement, whose rows changed shape, was prepared again.
        c.client.batch_execute(&format!("ALTER TABLE {t} ADD COLUMN b text DEFAULT 'x'")).unwrap();
        assert!(c.reset());
        let rows = c.query_kept(&select, &[]).unwrap();
        assert_eq!((rows[0].get::<_, i32>("a"), rows[0].get::<_, String>("b")), (1, "x".into()), "the column added");
        let second = prepared_as(&c);
        assert_ne!(second, first, "prepared again");

        // Another process drops the table and makes it again with the same
        // shape: the server plans the kept statement again, on the new table.
        other
            .client
            .batch_execute(&format!(
                "DROP TABLE {t}; CREATE TABLE {t} (a int, b text); INSERT INTO {t} VALUES (2, 'y')"
            ))
            .unwrap();
        assert!(c.reset());
        let rows = c.query_kept(&select, &[]).unwrap();
        assert_eq!((rows[0].get::<_, i32>("a"), rows[0].get::<_, String>("b")), (2, "y".into()), "the new table");
        assert_eq!(prepared_as(&c), second, "the same statement, planned again by the server");

        // Another process changes a column's type: the rows' shape changed.
        other.client.batch_execute(&format!("ALTER TABLE {t} ALTER COLUMN a TYPE bigint")).unwrap();
        assert!(c.reset());
        let rows = c.query_kept(&select, &[]).unwrap();
        assert_eq!(rows[0].get::<_, i64>("a"), 2, "the new type");
        assert_ne!(prepared_as(&c), second, "prepared again");
        c.client.batch_execute(&format!("DROP TABLE {t}")).unwrap();
    }

    /// The MySQL reset puts the role and the database back with one query
    /// of two statements, whose replies are checked in order: an error in
    /// either (the server runs nothing after it) means the connection is
    /// not reused, and the connection is in step after an error.
    #[test]
    fn the_mysql_reset_checks_its_settings_in_order() {
        use mysql::prelude::Queryable;
        let Some(url) = std::env::var("PHV_TEST_MYSQL_URL").ok().filter(|u| !u.is_empty()) else { return };
        let spec = DatabaseSpec { url: Some(url.clone()), url_env: None, tls: None, pool_size: 2 };
        let wait = Duration::from_secs(10);
        let mut c = my_connect(&url, &spec, wait, None, None).unwrap();
        let Some(db) = c.opts.get_db_name().map(str::to_string) else { panic!("the test URL names a database") };
        let missing = format!("phv_{}_missing", crate::testing::run_id());
        let value = |rows: &[mysql::Row]| rows.first().and_then(|r| r.get::<Option<String>, _>(0)).flatten();

        // The replies of several statements come in order; the rows are the last's.
        let rows = my_statements(&mut c.conn, "SET @phv_p = 1; SELECT @phv_p + 1").unwrap();
        assert_eq!(value(&rows), Some("2".into()));
        // The second statement fails: its error, and the connection is in step after it.
        assert!(my_statements(&mut c.conn, &format!("SELECT 1; SELECT * FROM {missing}")).is_err());
        assert_eq!(value(&my_statements(&mut c.conn, "SELECT 3").unwrap()), Some("3".into()));
        // The first fails: the second does not run.
        assert!(my_statements(&mut c.conn, &format!("SELECT * FROM {missing}; SET @phv_p = 9")).is_err());
        assert_eq!(value(&my_statements(&mut c.conn, "SELECT @phv_p").unwrap()), Some("1".into()));

        // A script that took a role and changed the database: both are put back.
        let role = format!("phv_{}_role", crate::testing::run_id());
        c.conn.query_drop(format!("CREATE ROLE IF NOT EXISTS {role}")).unwrap();
        c.conn.query_drop(format!("GRANT {role} TO CURRENT_USER()")).unwrap();
        c.conn.query_drop(format!("SET ROLE {role}")).unwrap();
        c.conn.query_drop("USE information_schema").unwrap();
        assert!(c.reset());
        let rows = my_statements(&mut c.conn, "SELECT CONCAT(CURRENT_ROLE(), ' ', DATABASE())").unwrap();
        assert_eq!(value(&rows), Some(format!("NONE {db}")));
        c.conn.query_drop(format!("DROP ROLE {role}")).unwrap();

        // The URL names a database the server does not have: the second
        // statement fails, and the connection is not reused.
        c.opts = Arc::new(mysql::OptsBuilder::from_opts((*c.opts).clone()).db_name(Some(&missing)).into());
        assert!(!c.reset());
        c.opts = Arc::new(mysql::OptsBuilder::from_opts((*c.opts).clone()).db_name(Some(&db)).into());
        assert!(c.reset(), "in step after the failed reset");

        // A URL without a database: a script that chose one leaves a
        // connection that is not reused.
        let no_db = url.rsplit_once('/').map(|(before, _)| before.to_string()).unwrap();
        let mut d = my_connect(&no_db, &spec, wait, None, None).unwrap();
        assert_eq!(d.opts.get_db_name(), None);
        assert!(d.reset());
        d.conn.query_drop(format!("USE {db}")).unwrap();
        assert!(!d.reset());
    }

    #[test]
    fn a_site_without_a_namespace_keeps_to_its_own_sessions() {
        // The stores of sites without a namespace are one for every test process.
        let _unnamed = crate::testing::unnamed();
        let short = SessionLimits {
            ttl: Duration::from_secs(60),
            max_lifetime: Duration::from_secs(24 * 3600),
            max_sessions: 1,
            max_bytes: 256 << 20,
        };
        let long = SessionLimits { ttl: Duration::from_secs(7200), max_sessions: 50_000, ..short };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let row = |at: u64| SessionRow { data: "{}".into(), created: at, last: at, returned: true };
        let old = Some(SESSIONS_OF_1_1);
        for (var, spec) in test_servers() {
            // Kept by 1.1: another site's session, and one of this site's own,
            // both idle for two minutes (past this site's ttl).
            let theirs = format!("f3-theirs-{}", crate::random_hex(8));
            let mine = format!("f3-mine-{}", crate::random_hex(8));
            let redis = var == "PHV_TEST_REDIS_URL";
            // Redis takes the time of the last use from its own writes.
            let idle = if redis { now } else { now - 120 };
            assert!(session_insert(&spec, old, &theirs, &row(idle), &long).unwrap(), "{var}");
            assert!(session_insert(&spec, old, &mine, &row(idle), &long).unwrap(), "{var}");
            if redis {
                std::thread::sleep(Duration::from_millis(2100));
            }
            // The site without a namespace writes two sessions (past its
            // limit of one) and sweeps, with a ttl of one second in Redis.
            let brief = SessionLimits { ttl: if redis { Duration::from_secs(1) } else { short.ttl }, ..short };
            let new = [format!("f3-new-{}", crate::random_hex(8)), format!("f3-new-{}", crate::random_hex(8))];
            for k in &new {
                assert!(session_insert(&spec, None, k, &row(now), &brief).unwrap(), "{var}");
            }
            session_sweep(&spec, None, &brief).unwrap();
            assert!(session_read(&spec, old, &theirs).unwrap().is_some(), "{var}: another site's session was removed");
            if redis {
                let until = Instant::now() + Duration::from_secs(5);
                let mut c = redis_conn(&spec, PoolUse::Sessions, until).unwrap();
                let r = redis_run(&mut c, &[b"ZSCORE", b"phv:sessions:back", theirs.as_bytes()], until).unwrap();
                assert!(!matches!(r, Reply::Nil), "{var}: forgotten by the shared bookkeeping");
            }
            // Its own session of 1.1 is read, updated where it is, and logged out.
            assert!(session_read(&spec, None, &mine).unwrap().is_some(), "{var}: its own old session was not read");
            assert!(session_update(&spec, None, &mine, &row(now), &long).unwrap(), "{var}");
            assert!(session_remove(&spec, None, &mine).unwrap(), "{var}");
            assert!(session_read(&spec, old, &mine).unwrap().is_none(), "{var}");
            for k in new.iter().chain([&theirs]) {
                session_remove(&spec, old, k).unwrap();
                session_remove(&spec, None, k).unwrap();
            }
        }
    }

    #[test]
    fn placeholders_render_per_database() {
        let text = "SELECT * FROM \u{E001}t\u{E002} WHERE a = \u{E000} AND b ? 'k' AND c = \u{E000}";
        assert_eq!(render_sql(text, Dialect::Postgres), "SELECT * FROM \"t\" WHERE a = $1 AND b ? 'k' AND c = $2");
        assert_eq!(render_sql(text, Dialect::MySql), "SELECT * FROM `t` WHERE a = ? AND b ? 'k' AND c = ?");
    }
}
