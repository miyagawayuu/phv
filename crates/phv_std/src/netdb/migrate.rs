// ----- migrations (`phv db`) ---------------------------------------------------

use super::handle::{kind_of, tls_init, Kind};
use super::my::{my_connect, my_message, MyConn};
use super::pg::{pg_connect, pg_message, PgConn};
use phv_vm::DatabaseSpec;
use std::time::Duration;

/// A connection to a server database for `phv db`. Migrations are the
/// project's own SQL run from the command line, so no time or size limit applies.
pub struct MigrationConn(MigrationInner);

enum MigrationInner {
    Pg(Box<PgConn>),
    My(Box<MyConn>),
}

const MIGRATIONS_TABLE: &str = "CREATE TABLE IF NOT EXISTS phv_migrations (name varchar(255) PRIMARY KEY, \
                                checksum varchar(64) NOT NULL, applied_at varchar(40) NOT NULL)";

pub fn migration_conn(spec: &DatabaseSpec) -> Result<MigrationConn, String> {
    let Some(url) = spec.url.clone() else {
        return Err(format!("the environment variable {} is not set", spec.url_env.as_deref().unwrap_or("?")));
    };
    tls_init();
    let wait = Duration::from_secs(30);
    let mut conn = match kind_of(&url) {
        Kind::Postgres => {
            let c = pg_connect(&url, spec, wait)?;
            c.meter.set(None, usize::MAX);
            MigrationInner::Pg(Box::new(c))
        }
        Kind::MySql => MigrationInner::My(Box::new(my_connect(&url, spec, wait, None, None)?)),
        Kind::Redis => return Err("it is a Redis server; migrations are for SQL databases".into()),
    };
    match &mut conn {
        MigrationInner::Pg(c) => c.store_create(MIGRATIONS_TABLE).map_err(|e| pg_message(&e))?,
        MigrationInner::My(c) => {
            use mysql::prelude::Queryable;
            c.conn.query_drop(MIGRATIONS_TABLE).map_err(|e| my_message(&e))?
        }
    }
    Ok(MigrationConn(conn))
}

impl MigrationConn {
    /// The migrations applied so far: name and checksum.
    pub fn applied(&mut self) -> Result<Vec<(String, String)>, String> {
        match &mut self.0 {
            MigrationInner::Pg(c) => {
                let rows = c
                    .client
                    .query("SELECT name, checksum FROM phv_migrations ORDER BY name", &[])
                    .map_err(|e| pg_message(&e))?;
                let get = |e: postgres::Error| pg_message(&e);
                rows.iter().map(|r| Ok((r.try_get(0).map_err(get)?, r.try_get(1).map_err(get)?))).collect()
            }
            MigrationInner::My(c) => {
                use mysql::prelude::Queryable;
                c.conn.query("SELECT name, checksum FROM phv_migrations ORDER BY name").map_err(|e| my_message(&e))
            }
        }
    }

    /// Runs one migration and records it. On PostgreSQL both happen in one
    /// transaction; MySQL commits each schema change at once, so a failing
    /// migration may be left half applied there.
    pub fn apply(&mut self, name: &str, checksum: &str, sql: &str, at: &str) -> Result<(), String> {
        match &mut self.0 {
            MigrationInner::Pg(c) => {
                let client = &c.client;
                client.batch_execute("BEGIN").map_err(|e| pg_message(&e))?;
                let run = || -> Result<(), postgres::Error> {
                    client.batch_execute(sql)?;
                    client.execute(
                        "INSERT INTO phv_migrations (name, checksum, applied_at) VALUES ($1, $2, $3)",
                        &[&name, &checksum, &at],
                    )?;
                    client.batch_execute("COMMIT")
                };
                run().map_err(|e| {
                    let _ = client.batch_execute("ROLLBACK");
                    pg_message(&e)
                })
            }
            MigrationInner::My(c) => {
                use mysql::prelude::Queryable;
                c.conn.query_drop(sql).map_err(|e| my_message(&e))?;
                c.conn
                    .exec_drop(
                        "INSERT INTO phv_migrations (name, checksum, applied_at) VALUES (?, ?, ?)",
                        (name, checksum, at),
                    )
                    .map_err(|e| my_message(&e))
            }
        }
    }
}
