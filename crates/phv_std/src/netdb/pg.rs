// ----- PostgreSQL ------------------------------------------------------------

use super::handle::{is_local_host, needs_new_timeout, out_of_memory, read_budget, Kind, TIMEOUT_MARGIN_MS};
use super::jobs_add::JobsTx;
use super::pool::{Driver, Pooled};
use super::query_async::Scalar;
use super::{R, RESET_BUDGET, RESET_TIMEOUT};
use crate::db::{parts, render_sql, Dialect, ResultRows};
use crate::pgmeter::{Meter, PgClient, Trip};
use phv_vm::{DatabaseSpec, Value, Vm, VmError};
use postgres::types::ToSql;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Runs a prepared statement (after a prelude, see `PgClient::run`).
type RunStatement<'a, T> =
    &'a mut dyn FnMut(&PgClient, &postgres::Statement, Option<&str>) -> Result<T, postgres::Error>;

/// Statements a connection keeps prepared (the most recently used ones).
const KEPT_STATEMENTS: usize = 64;
/// Longer statements are prepared each time, not kept.
const KEPT_TEXT: usize = 4 << 10;
/// The texts of the statements a connection keeps, at most (64 KB for each of
/// up to 256 connections of a pool).
const KEPT_BYTES: usize = 64 << 10;

/// A statement kept prepared on a connection, by its text.
pub(super) struct Kept {
    pub(super) text: String,
    stmt: postgres::Statement,
    /// The `PgConn::epoch` it was prepared in.
    epoch: u64,
    /// Its name on the server (`statement_name`).
    pub(super) name: Option<String>,
}

/// The name a statement was prepared under on the server. The client shows
/// it only in `Debug` (`Statement { name: "s12", .. }`).
pub(super) fn statement_name(stmt: &postgres::Statement) -> Option<String> {
    let shown = format!("{stmt:?}");
    let rest = shown.strip_prefix("Statement { name: \"")?;
    Some(rest.split_once('"')?.0.to_string()).filter(|n| !n.is_empty())
}

/// The most statements a connection's client prepared for itself that the
/// reset checks (`PgConn::own`): the sentinel, and the client's statements
/// that look types up (three at most).
const OWN_STATEMENTS: usize = 16;

pub(super) struct PgConn {
    pub(super) client: PgClient,
    /// The `statement_timeout` last set on the session, in milliseconds.
    pub(super) timeout_ms: Option<u64>,
    /// Stops reads and writes at the statement's deadline and byte budget.
    pub(super) meter: Meter,
    /// Statements kept prepared, least recently used first. A kept statement
    /// whose tables changed since fails with an error of its own (`stale`);
    /// outside a transaction it is then prepared again and run once more.
    /// Inside one that is impossible (the error ends the transaction), so
    /// there only statements prepared in the same `epoch` are used.
    pub(super) kept: Vec<Kept>,
    /// Changes at every transaction boundary and after every statement that
    /// may change the schema or the session (anything other than plain
    /// `SELECT` / `INSERT` / `UPDATE` / `DELETE` / `WITH` / `VALUES`).
    pub(super) epoch: u64,
    /// A transaction the script began with its own `BEGIN` (not `transaction()`).
    pub(super) in_own_transaction: bool,
    /// The jobs the script's open transaction added (`job_add_in_transaction`).
    pub(super) jobs_tx: Option<JobsTx>,
    /// The deadline of an operation of PHV's own stores (sessions, jobs) on
    /// this connection: each of its statements is sent with a
    /// `statement_timeout` that ends there (see `store_prelude`).
    pub(super) store_until: Option<Instant>,
    /// A statement prepared when the connection was made and never closed:
    /// gone only when a script deallocated the client's statements (see
    /// `PG_RESET`). Held so the client never closes it; the reset looks for
    /// it by its name, the first of `own`.
    #[allow(dead_code)]
    sentinel: postgres::Statement,
    /// The server's names of the statements the client prepared for itself
    /// and keeps for its whole life: the sentinel's, then those that look
    /// types up, as the reset finds them (see `PgConn::reset_session`).
    pub(super) own: Vec<String>,
    /// Whether the client looked a type up (with statements of its own) since
    /// it was made.
    looked_up: bool,
}

/// Clears what a script can leave on a session, in one round trip. This is
/// `DISCARD ALL` without its `DEALLOCATE ALL`, which would also remove the
/// statements the client prepares for itself (to look up types, and the kept
/// statements) and break the connection for the next script; the statements
/// scripts prepare are deallocated by name. Outside a transaction `ROLLBACK`
/// only warns; inside one it ends it, so the reset itself does not run inside
/// the script's transaction.
///
/// The last statement lists the session's prepared statements: those of
/// SQL's `PREPARE` (`sql <name>`), which are deallocated, and those the
/// client prepared (`own <name>`). When a script deallocated statements the
/// client prepared for itself (`DEALLOCATE ALL`, or `DISCARD ALL`, perhaps
/// inside a function where PHV cannot see it), the client still holds those
/// it prepared to look up types, and would fail on them for every later
/// script ("prepared statement ... does not exist"); such a connection is
/// closed instead of reused (found by the 1.1.1 tests). Each statement is
/// looked for by its name, which a script cannot give its own statements
/// (found by the 1.2 audit: the sentinel was looked for by its text, which a
/// script could prepare again, and a script could deallocate the type
/// lookups alone).
///
/// `DISCARD PLANS` is left out: the server drops a cached plan itself when
/// a table or type it uses changes, and plans the statement again from its
/// text, so a kept statement sees the schema the next script finds (see
/// `a_schema_change_is_seen_through_a_kept_statement_after_the_reset`).
const PG_RESET: &str = "ROLLBACK; CLOSE ALL; SET SESSION AUTHORIZATION DEFAULT; RESET ALL; UNLISTEN *; \
    SELECT pg_advisory_unlock_all(); DISCARD TEMP; DISCARD SEQUENCES; \
    SELECT CASE WHEN from_sql THEN 'sql ' ELSE 'own ' END || name FROM pg_prepared_statements";

/// What a statement's text is, for the kept statements: only plain data
/// statements are kept; the others are run as before, and may change the
/// schema or the transaction, so the kept statements are dropped after them.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum PgText {
    Plain,
    Begin,
    /// `COMMIT` / `ROLLBACK` / `END` / `ABORT` alone (`ROLLBACK TO ...` is `Other`).
    End,
    Other,
}

pub(super) fn pg_text(text: &str) -> PgText {
    let words: Vec<String> =
        text.trim().trim_end_matches(';').split_ascii_whitespace().take(3).map(|w| w.to_ascii_uppercase()).collect();
    let word = |i: usize| words.get(i).map_or("", String::as_str);
    match word(0) {
        // `SELECT ... INTO` creates a table.
        "SELECT" | "WITH" if text.split_ascii_whitespace().any(|w| w.eq_ignore_ascii_case("INTO")) => PgText::Other,
        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "WITH" | "VALUES" => PgText::Plain,
        "BEGIN" | "START" => PgText::Begin,
        "COMMIT" | "ROLLBACK" | "END" | "ABORT"
            if matches!(word(1), "" | "WORK" | "TRANSACTION") && words.len() <= 2 =>
        {
            PgText::End
        }
        _ => PgText::Other,
    }
}

/// Whether an error of a kept statement may come from the statement being
/// older than the schema: it was deallocated, its result changed shape, or a
/// column it compares or sets changed type (its parameters keep the types
/// they had when it was prepared). Each is raised before the statement runs,
/// so running it again, prepared anew, cannot repeat what it did.
pub(super) fn stale(e: &postgres::Error) -> bool {
    use postgres::error::SqlState;
    match e.as_db_error() {
        Some(db) => {
            let code = db.code();
            code == &SqlState::INVALID_SQL_STATEMENT_NAME
                || code == &SqlState::DATATYPE_MISMATCH
                || code == &SqlState::UNDEFINED_FUNCTION
                || (code == &SqlState::FEATURE_NOT_SUPPORTED
                    && db.message().contains("cached plan must not change result type"))
        }
        None => false,
    }
}

impl PgConn {
    /// Meters the statements PHV runs on its own (reset, check).
    fn meter_internal(&self) {
        self.meter.set(Some(Instant::now() + RESET_TIMEOUT), RESET_BUDGET);
    }

    fn reset_session(&mut self) -> bool {
        let Ok(rows) = self.client.simple_last_column(PG_RESET) else { return false };
        let mut on_server = HashSet::new();
        for row in &rows {
            if let Some(n) = row.strip_prefix("sql ") {
                if self.client.batch_execute(&format!("DEALLOCATE \"{}\"", n.replace('"', "\"\""))).is_err() {
                    return false;
                }
            } else if let Some(n) = row.strip_prefix("own ") {
                on_server.insert(n);
            }
        }
        // Every statement the client prepared for itself is still there.
        if !self.own.iter().all(|n| on_server.contains(n.as_str())) {
            return false;
        }
        // The client's other statements are the kept ones (any other was
        // closed when the client dropped it, before this reset): the rest are
        // those it prepared to look types up, which it keeps from now on.
        let kept: Option<HashSet<&str>> = self.kept.iter().map(|k| k.name.as_deref()).collect();
        let Some(kept) = kept else { return true };
        for n in on_server {
            if !kept.contains(n) && !self.own.iter().any(|o| o == n) && self.own.len() < OWN_STATEMENTS {
                self.own.push(n.to_string());
            }
        }
        // A type was looked up, but no statement that does is left: a script
        // deallocated them in the same run as the first lookup.
        !(self.looked_up && self.own.len() < 2)
    }

    /// Drops the kept statements (and the types the client looked up, which a
    /// script's `DEALLOCATE ALL` or a changed type would make wrong).
    pub(super) fn forget_statements(&mut self) {
        self.kept.clear();
        self.client.client().clear_type_cache();
        self.epoch += 1;
    }

    /// The kept statement for `text`, when it may be used: inside a
    /// transaction only one prepared in the same epoch.
    fn kept(&mut self, text: &str, in_transaction: bool) -> Option<postgres::Statement> {
        let i = self.kept.iter().position(|k| k.text == text)?;
        let k = self.kept.remove(i);
        if in_transaction && k.epoch != self.epoch {
            return None;
        }
        let stmt = k.stmt.clone();
        self.kept.push(k);
        Some(stmt)
    }

    pub(super) fn keep(&mut self, text: &str, stmt: &postgres::Statement) {
        if text.len() > KEPT_TEXT {
            return;
        }
        while !self.kept.is_empty()
            && (self.kept.len() >= KEPT_STATEMENTS
                || self.kept.iter().map(|k| k.text.len()).sum::<usize>() + text.len() > KEPT_BYTES)
        {
            self.kept.remove(0);
        }
        let name = statement_name(stmt);
        self.kept.push(Kept { text: text.to_string(), stmt: stmt.clone(), epoch: self.epoch, name });
    }

    /// Runs `run` with the statement for `text`: a kept one, or one prepared
    /// now (kept when `keep`). A `prelude` goes with the first request (see
    /// `PgClient::run`). A kept statement that turns out stale is prepared
    /// again and run once more; if that cannot be done (inside a
    /// transaction), its first error is returned.
    pub(super) fn with_statement<T>(
        &mut self,
        text: &str,
        keep: bool,
        in_transaction: bool,
        prelude: Option<&str>,
        run: RunStatement<'_, T>,
    ) -> Result<T, postgres::Error> {
        let store = if prelude.is_none() { self.store_prelude() } else { None };
        let prelude = prelude.or(store.as_deref());
        if let Some(stmt) = keep.then(|| self.kept(text, in_transaction)).flatten() {
            match run(&self.client, &stmt, prelude) {
                Err(e) if stale(&e) => {
                    // The prelude, if any, ran before the statement failed.
                    self.forget_statements();
                    let again = self.client.run(None, self.client.client().prepare(text)).and_then(|fresh| {
                        self.note_types(&fresh);
                        let r = run(&self.client, &fresh, None);
                        self.keep(text, &fresh);
                        r
                    });
                    return match again {
                        Err(e2) if e2.code() == Some(&postgres::error::SqlState::IN_FAILED_SQL_TRANSACTION) => Err(e),
                        r => r,
                    };
                }
                r => return r,
            }
        }
        let stmt = self.client.run(prelude, self.client.client().prepare(text))?;
        self.note_types(&stmt);
        if keep {
            self.keep(text, &stmt);
        }
        run(&self.client, &stmt, None)
    }

    /// Notes when `stmt` has a type the client does not know by itself: the
    /// client looked it up, with statements it prepared for itself.
    fn note_types(&mut self, stmt: &postgres::Statement) {
        use postgres::types::Type;
        let looked_up = |t: &Type| Type::from_oid(t.oid()).is_none();
        if stmt.params().iter().any(looked_up) || stmt.columns().iter().any(|c| looked_up(c.type_())) {
            self.looked_up = true;
        }
    }
}

/// PHV's own statements (sessions, jobs), through the kept statements. During
/// an operation of a store each goes with a `statement_timeout` that ends at
/// the operation's deadline, so the server stops it there, lock waits
/// included (found by the 1.1.1 audit: a job added with a key that an open
/// transaction had added waited on the server for as long as that transaction
/// lasted, after the client had given up).
impl PgConn {
    pub(super) fn store_prelude(&self) -> Option<String> {
        let until = self.store_until?;
        let ms = until.saturating_duration_since(Instant::now()).as_millis().clamp(1, i32::MAX as u128);
        Some(format!("SET statement_timeout = {ms}"))
    }

    /// Runs `sql` (simple statements) after the store's `statement_timeout`.
    pub(super) fn store_batch(&self, sql: &str) -> Result<(), postgres::Error> {
        match self.store_prelude() {
            Some(set) => self.client.batch_execute(&format!("{set}; {sql}")),
            None => self.client.batch_execute(sql),
        }
    }

    /// `CREATE … IF NOT EXISTS` statements: when another connection creates the
    /// same table or index at the same moment, PostgreSQL reports a duplicate
    /// (in its catalog) instead of finding it, once that connection commits. The
    /// statements are run again, and then find it (found by CI: several
    /// processes making the shared table of job counts at once).
    pub(super) fn store_create(&self, sql: &str) -> Result<(), postgres::Error> {
        use postgres::error::SqlState;
        let mut tries = 0;
        loop {
            match self.store_batch(sql) {
                Err(e)
                    if tries < 3
                        && [SqlState::UNIQUE_VIOLATION, SqlState::DUPLICATE_TABLE, SqlState::DUPLICATE_OBJECT]
                            .iter()
                            .any(|c| e.code() == Some(c)) =>
                {
                    tries += 1;
                }
                r => return r,
            }
        }
    }

    pub(super) fn query_kept(
        &mut self,
        text: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<postgres::Row>, postgres::Error> {
        self.with_statement(text, true, false, None, &mut |client, stmt, prelude| {
            client.run(prelude, client.client().query(stmt, params))
        })
    }

    pub(super) fn execute_kept(&mut self, text: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, postgres::Error> {
        self.with_statement(text, true, false, None, &mut |client, stmt, prelude| {
            client.run(prelude, client.client().execute(stmt, params))
        })
    }
}

impl Driver for PgConn {
    fn reset(&mut self) -> bool {
        if self.client.is_closed() {
            return false;
        }
        self.timeout_ms = None;
        self.in_own_transaction = false;
        self.jobs_tx = None;
        self.store_until = None;
        self.epoch += 1;
        self.meter_internal();
        let ok = self.reset_session();
        self.meter.set(None, RESET_BUDGET);
        ok && !self.client.is_closed()
    }

    fn check(&mut self) -> bool {
        self.meter_internal();
        let ok = self.client.is_valid(Duration::from_secs(5));
        self.meter.set(None, RESET_BUDGET);
        ok
    }

    fn closed(&self) -> bool {
        self.client.is_closed()
    }
}

pub(super) fn pg_connect(url: &str, spec: &DatabaseSpec, timeout: Duration) -> Result<PgConn, String> {
    use tokio_postgres::config::{Host, SslMode};
    // The parser's own message could quote the URL, which may hold a password.
    let mut config: tokio_postgres::Config =
        url.parse().map_err(|_| "the URL is not a valid PostgreSQL URL".to_string())?;
    let local = config.get_hosts().iter().all(|h| match h {
        Host::Tcp(name) => is_local_host(name),
        #[allow(unreachable_patterns)]
        _ => true,
    }) && config.get_hostaddrs().iter().all(|ip| ip.is_loopback());
    // `sslmode=require` in the URL is kept even when phv.toml would allow plain connections.
    let tls = config.get_ssl_mode() == SslMode::Require || spec.tls.unwrap_or(!local);
    config.ssl_mode(if tls { SslMode::Require } else { SslMode::Disable });
    let meter = Meter::default();
    meter.set(Some(Instant::now() + timeout + RESET_TIMEOUT), RESET_BUDGET);
    let make_tls = || tokio_postgres_rustls::MakeRustlsConnect::new((*crate::netio::tls_config()).clone());
    let client = PgClient::connect(&config, tls.then(make_tls), timeout, meter.clone())?;
    let text = format!("SELECT 1 AS phv_{}", crate::random_hex(8));
    let sentinel = client.run(None, client.client().prepare(&text)).map_err(|e| pg_message(&e))?;
    // Its name, which no script has had the chance to prepare a statement under yet.
    let named = client
        .query("SELECT name FROM pg_prepared_statements WHERE NOT from_sql AND statement = $1", &[&text])
        .map_err(|e| pg_message(&e))?;
    let name = named
        .first()
        .and_then(|r| r.try_get::<_, String>(0).ok())
        .or_else(|| statement_name(&sentinel))
        .ok_or_else(|| "the server does not list the connection's statements".to_string())?;
    meter.set(None, RESET_BUDGET);
    Ok(PgConn {
        client,
        timeout_ms: None,
        meter,
        kept: Vec::new(),
        epoch: 0,
        in_own_transaction: false,
        jobs_tx: None,
        store_until: None,
        sentinel,
        own: vec![name],
        looked_up: false,
    })
}

pub(crate) fn pg_message(e: &postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => match db.detail() {
            Some(d) => format!("{} ({d})", db.message()),
            None => db.message().to_string(),
        },
        // Network and TLS errors are described without the server's address.
        None if network_cause(e).is_some() => network_cause(e).unwrap_or_default(),
        // Conversion errors keep the reason in the source ("error serializing parameter 0: ...").
        None => match std::error::Error::source(e) {
            Some(cause) => format!("{e}: {cause}"),
            None => e.to_string(),
        },
    }
}

/// A network or TLS failure somewhere in an error's sources, in fixed words.
pub(super) fn network_cause(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return Some(crate::netio::describe(io));
        }
        if err.downcast_ref::<rustls::Error>().is_some() {
            return Some("the TLS connection failed".into());
        }
        cur = err.source();
    }
    None
}

/// Why a statement failed, as `pg_failure` sees it.
pub(super) enum PgFailure {
    /// The result was larger than the byte budget.
    Budget,
    /// The client-side deadline passed.
    Deadline,
    /// The server stopped it (`statement_timeout`, set from the time limit).
    Canceled,
    Other,
}

/// Classifies a failed statement; the connection is marked broken when it
/// cannot be reused (closed, or stopped by the meter in the middle of a reply).
pub(super) fn pg_failure(c: &mut Pooled<PgConn>, e: &postgres::Error) -> PgFailure {
    if c.get().client.is_closed() {
        c.broken = true;
    }
    // A prepared statement that does not exist, past the retry of a stale
    // kept one: perhaps one the client prepared for itself to look types up,
    // which a script deallocated where the reset could not tell. The client
    // would fail on it for every later script (found by the 1.2 audit).
    if e.code() == Some(&postgres::error::SqlState::INVALID_SQL_STATEMENT_NAME) {
        c.broken = true;
    }
    match c.get().meter.tripped() {
        Some(Trip::Budget) => {
            c.broken = true;
            PgFailure::Budget
        }
        Some(Trip::Deadline) => {
            c.broken = true;
            PgFailure::Deadline
        }
        None if e.code() == Some(&postgres::error::SqlState::QUERY_CANCELED) => PgFailure::Canceled,
        None => PgFailure::Other,
    }
}

fn pg_error(vm: &Vm, c: &mut Pooled<PgConn>, e: postgres::Error) -> VmError {
    match pg_failure(c, &e) {
        PgFailure::Budget => out_of_memory(vm),
        PgFailure::Deadline => {
            if let Err(fatal) = vm.check_limits() {
                return fatal;
            }
            vm.throw("DatabaseError", crate::netio::TIME_LIMIT)
        }
        PgFailure::Canceled => {
            if let Err(fatal) = vm.check_limits() {
                return fatal;
            }
            vm.throw("DatabaseError", pg_message(&e))
        }
        PgFailure::Other => vm.throw("DatabaseError", pg_message(&e)),
    }
}

/// Before a statement: sets the client-side deadline and byte budget, and
/// returns the `SET statement_timeout` to send with it when the server's
/// timeout must be lowered (`needs_new_timeout`) — which stops the statement
/// without closing the connection when the script has not changed it.
pub(super) fn pg_limits(c: &mut Pooled<PgConn>, until: Option<Instant>, budget: usize) -> Option<(String, u64)> {
    c.get().meter.set(until.map(|u| u + Duration::from_millis(TIMEOUT_MARGIN_MS)), budget);
    let left = until?.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    let ms = left.as_millis().min(i32::MAX as u128) as u64;
    needs_new_timeout(c.get().timeout_ms, ms)
        .then(|| (format!("SET statement_timeout = {}", ms + TIMEOUT_MARGIN_MS), ms))
}

/// Runs one statement on a script's connection with its limits (see
/// `pg_limits`), recording the server timeout once it was set.
pub(super) fn pg_run<T>(
    vm: &Vm,
    c: &mut Pooled<PgConn>,
    run: impl FnOnce(&mut Pooled<PgConn>, Option<&str>) -> Result<T, postgres::Error>,
) -> Result<T, VmError> {
    vm.check_limits()?;
    let set = pg_limits(c, vm.deadline(), read_budget(vm));
    let r = run(c, set.as_ref().map(|s| s.0.as_str()));
    let r = r.map_err(|e| pg_error(vm, c, e))?;
    if let Some((_, ms)) = set {
        c.get().timeout_ms = Some(ms);
    }
    Ok(r)
}

/// The first `n` words of a statement, in upper case, after any leading
/// comments (`--`, `#`, `/* */`; a MySQL `/*! */` is run, so its words count).
/// Comments may also stand between words (`XA/**/PREPARE`), so they are
/// separators wherever they are; PostgreSQL nests `/* */` comments (`nested`),
/// MySQL ends one at the first `*/`.
fn leading_words(text: &str, n: usize, nested: bool) -> Vec<String> {
    let b = text.as_bytes();
    let mut words: Vec<String> = Vec::new();
    let mut word: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < b.len() && words.len() < n {
        let rest = &b[i..];
        let end_word = |word: &mut Vec<u8>, words: &mut Vec<String>| {
            if !word.is_empty() {
                words.push(String::from_utf8_lossy(word).to_ascii_uppercase());
                word.clear();
            }
        };
        if rest.starts_with(b"/*!") {
            // MySQL runs the words inside: only the opener and its version number go.
            end_word(&mut word, &mut words);
            i += 3;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        } else if rest.starts_with(b"/*") {
            end_word(&mut word, &mut words);
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if nested && b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if rest.starts_with(b"*/") {
            end_word(&mut word, &mut words);
            i += 2;
        } else if rest.starts_with(b"--") || b[i] == b'#' {
            end_word(&mut word, &mut words);
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i].is_ascii_whitespace() || b[i] == b';' || b[i] == b'(' {
            end_word(&mut word, &mut words);
            i += 1;
        } else {
            word.push(b[i]);
            i += 1;
        }
    }
    if words.len() < n && !word.is_empty() {
        words.push(String::from_utf8_lossy(&word).to_ascii_uppercase());
    }
    words
}

/// Why a script may not run `text` on a server of `kind`, if it may not:
/// a statement that prepares a transaction for a two-phase commit leaves it
/// on the server when the script ends, with its locks, until someone ends it
/// by name, also after a restart (the reset of a pooled connection does not:
/// MySQL detaches a prepared XA transaction from its connection; found by the
/// 1.2 audit), and `LOAD DATA LOCAL INFILE` would send the server a file of
/// this host.
pub(super) fn refused_statement(text: &str, kind: Kind) -> Option<String> {
    let words = leading_words(text, 4, matches!(kind, Kind::Postgres));
    let word = |i: usize| words.get(i).map_or("", String::as_str);
    let two = (word(0), word(1));
    match kind {
        Kind::MySql if two == ("XA", "PREPARE") => Some(
            "XA PREPARE is not allowed: a prepared transaction would keep its locks after the script \
             until XA ROLLBACK or XA COMMIT ends it"
                .into(),
        ),
        Kind::MySql if two == ("LOAD", "DATA") && words.iter().any(|w| w == "LOCAL") => {
            Some("LOAD DATA LOCAL INFILE is not allowed: it would send the server a file of this host".into())
        }
        Kind::Postgres if two == ("PREPARE", "TRANSACTION") => Some(
            "PREPARE TRANSACTION is not allowed: a prepared transaction would keep its locks after the script \
             until ROLLBACK PREPARED or COMMIT PREPARED ends it"
                .into(),
        ),
        _ => None,
    }
}

/// `refused_statement` as the script's error.
pub(super) fn check_statement(vm: &Vm, text: &str, kind: Kind) -> Result<(), VmError> {
    match refused_statement(text, kind) {
        Some(why) => Err(vm.throw("DatabaseError", why)),
        None => Ok(()),
    }
}

/// Runs a script's statement through the kept statements (`with_statement`),
/// then notes what it did to the transaction and the schema (`pg_text`).
fn pg_statement<T>(vm: &Vm, c: &mut Pooled<PgConn>, text: &str, run: RunStatement<'_, T>) -> Result<T, VmError> {
    check_statement(vm, text, Kind::Postgres)?;
    let kind = pg_text(text);
    let in_transaction = c.depth > 0 || c.get().in_own_transaction;
    let r =
        pg_run(vm, c, |c, prelude| c.get().with_statement(text, kind == PgText::Plain, in_transaction, prelude, run));
    let conn = c.get();
    match kind {
        PgText::Plain => {}
        PgText::Begin | PgText::End => {
            if r.is_ok() {
                conn.in_own_transaction = kind == PgText::Begin;
            }
            conn.epoch += 1;
            if let Some(j) = conn.jobs_tx.take() {
                j.ended();
            }
        }
        PgText::Other => conn.forget_statements(),
    }
    r
}

pub(super) fn pg_query(vm: &mut Vm, c: &mut Pooled<PgConn>, q: &Value) -> R {
    use postgres::types::ToSql;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::Postgres);
    let bound: Vec<PgParam> = params.iter().map(PgParam::new).collect();
    let mut out = ResultRows::new(Vec::new());
    let mut over = false;
    let vm_ref: &Vm = vm;
    pg_statement(vm_ref, c, &text, &mut |client, stmt, prelude| {
        out = ResultRows::new(stmt.columns().iter().map(|col| col.name().to_string()));
        let width = stmt.columns().len();
        client.query_each(prelude, stmt, bound.iter().map(|p| p as &dyn ToSql), |row| {
            let mut values = Vec::with_capacity(width);
            for i in 0..width {
                let PgOut(v) = row.try_get(i)?;
                values.push(v.into_value());
            }
            if out.push(vm_ref, values).is_err() {
                over = true;
                return Ok(false);
            }
            Ok(true)
        })
    })?;
    if over {
        // The rest of the result is not read: the connection is closed, not reused.
        c.broken = true;
        return Err(out_of_memory(vm));
    }
    Ok(out.into_value())
}

pub(super) fn pg_execute(vm: &mut Vm, c: &mut Pooled<PgConn>, q: &Value) -> R {
    use postgres::types::ToSql;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::Postgres);
    let bound: Vec<PgParam> = params.iter().map(PgParam::new).collect();
    let refs: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let n = pg_statement(vm, c, &text, &mut |client, stmt, prelude| {
        client.run(prelude, client.client().execute(stmt, &refs))
    })?;
    Ok(Value::Int(n.try_into().unwrap_or(i64::MAX)))
}

type BoxError = Box<dyn std::error::Error + Sync + Send>;

/// A PHV value bound to a PostgreSQL parameter, converted to the parameter's
/// type when it is sent.
#[derive(Debug)]
pub(super) enum PgParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl PgParam {
    pub(super) fn new(v: &Value) -> PgParam {
        match v {
            Value::Null => PgParam::Null,
            Value::Bool(b) => PgParam::Bool(*b),
            Value::Int(i) => PgParam::Int(*i),
            Value::Float(f) => PgParam::Float(*f),
            other => PgParam::Text(other.to_display_string()),
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            PgParam::Null => "null",
            PgParam::Bool(_) => "a bool",
            PgParam::Int(_) => "an int",
            PgParam::Float(_) => "a float",
            PgParam::Text(_) => "a string",
        }
    }
}

fn unsupported(what: &str, ty: &postgres::types::Type) -> BoxError {
    format!("cannot pass {what} as a parameter of type {ty}; cast it in the SQL, e.g. ({{$value}}::text)::{ty}").into()
}

impl postgres::types::ToSql for PgParam {
    fn to_sql(
        &self,
        ty: &postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<postgres::types::IsNull, BoxError> {
        pg_encode(self, ty, out)
    }

    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }

    postgres::types::to_sql_checked!();
}

fn pg_encode(
    v: &PgParam,
    ty: &postgres::types::Type,
    out: &mut bytes::BytesMut,
) -> Result<postgres::types::IsNull, BoxError> {
    use bytes::BufMut;
    use postgres::types::{IsNull, Kind, ToSql, Type};
    if let Kind::Domain(inner) = ty.kind() {
        return pg_encode(v, inner, out);
    }
    match v {
        PgParam::Null => return Ok(IsNull::Yes),
        PgParam::Bool(b) if *ty == Type::BOOL => return b.to_sql(ty, out),
        PgParam::Int(i) => match *ty {
            Type::INT2 => return i16::try_from(*i).map_err(|_| "integer out of range for smallint")?.to_sql(ty, out),
            Type::INT4 => return i32::try_from(*i).map_err(|_| "integer out of range for integer")?.to_sql(ty, out),
            Type::INT8 => return i.to_sql(ty, out),
            Type::FLOAT4 => return (*i as f32).to_sql(ty, out),
            Type::FLOAT8 => return (*i as f64).to_sql(ty, out),
            Type::NUMERIC => return numeric_encode(&i.to_string(), out),
            _ => {}
        },
        PgParam::Float(f) => match *ty {
            Type::FLOAT4 => return (*f as f32).to_sql(ty, out),
            Type::FLOAT8 => return f.to_sql(ty, out),
            Type::NUMERIC if f.is_finite() => return numeric_encode(&f.to_string(), out),
            _ => {}
        },
        PgParam::Text(text) => {
            let t = text.as_str();
            if is_text_type(ty) {
                out.put_slice(t.as_bytes());
                return Ok(IsNull::No);
            }
            match *ty {
                Type::JSON => {
                    out.put_slice(t.as_bytes());
                    return Ok(IsNull::No);
                }
                Type::JSONB => {
                    out.put_u8(1);
                    out.put_slice(t.as_bytes());
                    return Ok(IsNull::No);
                }
                Type::NUMERIC => return numeric_encode(t, out),
                Type::UUID => {
                    out.put_slice(&uuid_parse(t).ok_or_else(|| format!("\"{t}\" is not a UUID"))?);
                    return Ok(IsNull::No);
                }
                Type::DATE => return parse_date(t)?.to_sql(ty, out),
                Type::TIME => return parse_time(t)?.to_sql(ty, out),
                Type::TIMESTAMP => return parse_datetime(t)?.to_sql(ty, out),
                Type::TIMESTAMPTZ => return parse_datetime_tz(t)?.to_sql(ty, out),
                _ => {}
            }
        }
        _ => {}
    }
    Err(unsupported(v.type_name(), ty))
}

fn is_text_type(ty: &postgres::types::Type) -> bool {
    use postgres::types::{Kind, Type};
    matches!(*ty, Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN)
        || matches!(ty.kind(), Kind::Enum(_))
        || ty.name() == "citext"
}

fn parse_date(t: &str) -> Result<chrono::NaiveDate, BoxError> {
    chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").map_err(|_| format!("\"{t}\" is not a date (YYYY-MM-DD)").into())
}

fn parse_time(t: &str) -> Result<chrono::NaiveTime, BoxError> {
    chrono::NaiveTime::parse_from_str(t, "%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveTime::parse_from_str(t, "%H:%M"))
        .map_err(|_| format!("\"{t}\" is not a time (HH:MM:SS)").into())
}

fn parse_datetime(t: &str) -> Result<chrono::NaiveDateTime, BoxError> {
    chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f"))
        .or_else(|_| parse_date(t).map(|d| d.and_time(chrono::NaiveTime::MIN)).map_err(|_| ()))
        .map_err(|_| format!("\"{t}\" is not a timestamp (YYYY-MM-DD HH:MM:SS)").into())
}

/// A timestamp with an offset (`+09:00`, `Z`); without one it is taken as UTC.
fn parse_datetime_tz(t: &str) -> Result<chrono::DateTime<chrono::Utc>, BoxError> {
    let with_offset = chrono::DateTime::parse_from_rfc3339(t)
        .or_else(|_| chrono::DateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f%:z"))
        .or_else(|_| chrono::DateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f%#z"));
    match with_offset {
        Ok(d) => Ok(d.with_timezone(&chrono::Utc)),
        Err(_) => Ok(parse_datetime(t)?.and_utc()),
    }
}

pub(super) fn uuid_parse(t: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = t.bytes().filter(|&b| b != b'-').collect();
    if hex.len() != 32 || t.len() > 36 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// Writes a decimal string in PostgreSQL's binary NUMERIC format.
pub(super) fn numeric_encode(t: &str, out: &mut bytes::BytesMut) -> Result<postgres::types::IsNull, BoxError> {
    use bytes::BufMut;
    let bad = || -> BoxError { format!("\"{t}\" is not a number").into() };
    let s = t.trim();
    if s.eq_ignore_ascii_case("nan") {
        out.put_slice(&[0, 0, 0, 0, 0xC0, 0, 0, 0]);
        return Ok(postgres::types::IsNull::No);
    }
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (ip, fp) = body.split_once('.').unwrap_or((body, ""));
    let digits_only = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if (ip.is_empty() && fp.is_empty()) || !digits_only(ip) || !digits_only(fp) || fp.len() > 16383 || ip.len() > 131072
    {
        return Err(bad());
    }
    let ip = ip.trim_start_matches('0');
    let int_digits = format!("{}{ip}", "0".repeat((4 - ip.len() % 4) % 4));
    let frac_digits = format!("{fp}{}", "0".repeat((4 - fp.len() % 4) % 4));
    let group = |c: &[u8]| c.iter().fold(0i16, |n, d| n * 10 + (d - b'0') as i16);
    let mut groups: Vec<i16> =
        int_digits.as_bytes().chunks(4).chain(frac_digits.as_bytes().chunks(4)).map(group).collect();
    let mut weight = (int_digits.len() / 4) as i32 - 1;
    let lead = groups.iter().take_while(|&&g| g == 0).count();
    groups.drain(..lead);
    weight -= lead as i32;
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    let sign: u16 = if neg && !groups.is_empty() { 0x4000 } else { 0 };
    out.put_i16(groups.len() as i16);
    out.put_i16(i16::try_from(weight).map_err(|_| bad())?);
    out.put_u16(sign);
    out.put_u16(fp.len() as u16);
    for g in groups {
        out.put_i16(g);
    }
    Ok(postgres::types::IsNull::No)
}

/// Reads PostgreSQL's binary NUMERIC format as a decimal string (no precision is lost).
pub(super) fn numeric_decode(raw: &[u8]) -> Result<String, BoxError> {
    use std::fmt::Write;
    let rd = |i: usize| -> Result<u16, BoxError> {
        raw.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]])).ok_or_else(|| "invalid numeric value".into())
    };
    let ndigits = rd(0)? as usize;
    let weight = rd(2)? as i16 as i32;
    let sign = rd(4)?;
    let dscale = rd(6)? as usize;
    match sign {
        0xC000 => return Ok("NaN".into()),
        0xD000 => return Ok("Infinity".into()),
        0xF000 => return Ok("-Infinity".into()),
        0 | 0x4000 => {}
        _ => return Err("invalid numeric value".into()),
    }
    let digits: Vec<i16> = (0..ndigits).map(|i| rd(8 + 2 * i).map(|d| d as i16)).collect::<Result<_, _>>()?;
    let digit = |i: i32| if i >= 0 { digits.get(i as usize).copied().unwrap_or(0) } else { 0 };
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                write!(s, "{}", digit(i)).unwrap();
            } else {
                write!(s, "{:04}", digit(i)).unwrap();
            }
        }
    }
    if dscale > 0 {
        let mut frac = String::with_capacity(dscale + 4);
        let mut i = weight + 1;
        while frac.len() < dscale {
            write!(frac, "{:04}", digit(i)).unwrap();
            i += 1;
        }
        frac.truncate(dscale);
        s.push('.');
        s.push_str(&frac);
    }
    Ok(s)
}

/// A column value read from PostgreSQL.
pub(super) struct PgOut(pub(super) Scalar);

impl<'a> postgres::types::FromSql<'a> for PgOut {
    fn from_sql(ty: &postgres::types::Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        pg_decode(ty, raw).map(PgOut)
    }

    fn from_sql_null(_: &postgres::types::Type) -> Result<Self, BoxError> {
        Ok(PgOut(Scalar::Null))
    }

    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }
}

fn pg_decode(ty: &postgres::types::Type, raw: &[u8]) -> Result<Scalar, BoxError> {
    use postgres::types::{FromSql, Kind, Type};
    use std::fmt::Write;
    if let Kind::Domain(inner) = ty.kind() {
        return pg_decode(inner, raw);
    }
    if is_text_type(ty) || *ty == Type::JSON {
        return Ok(Scalar::Str(String::from_utf8_lossy(raw).into_owned()));
    }
    Ok(match *ty {
        Type::BOOL => Scalar::Bool(bool::from_sql(ty, raw)?),
        Type::INT2 => Scalar::Int(i16::from_sql(ty, raw)? as i64),
        Type::INT4 => Scalar::Int(i32::from_sql(ty, raw)? as i64),
        Type::INT8 => Scalar::Int(i64::from_sql(ty, raw)?),
        Type::OID => Scalar::Int(u32::from_sql(ty, raw)? as i64),
        Type::FLOAT4 => Scalar::Float(f32::from_sql(ty, raw)? as f64),
        Type::FLOAT8 => Scalar::Float(f64::from_sql(ty, raw)?),
        Type::NUMERIC => Scalar::Str(numeric_decode(raw)?),
        Type::JSONB => match raw.split_first() {
            Some((1, rest)) => Scalar::Str(String::from_utf8_lossy(rest).into_owned()),
            _ => return Err("unsupported jsonb version".into()),
        },
        Type::UUID if raw.len() == 16 => {
            let mut s = String::with_capacity(36);
            for (i, b) in raw.iter().enumerate() {
                if matches!(i, 4 | 6 | 8 | 10) {
                    s.push('-');
                }
                write!(s, "{b:02x}").unwrap();
            }
            Scalar::Str(s)
        }
        Type::BYTEA => {
            let mut s = String::with_capacity(2 + raw.len() * 2);
            s.push_str("\\x");
            for b in raw {
                write!(s, "{b:02x}").unwrap();
            }
            Scalar::Str(s)
        }
        Type::DATE => Scalar::Str(chrono::NaiveDate::from_sql(ty, raw)?.format("%Y-%m-%d").to_string()),
        Type::TIME => Scalar::Str(chrono::NaiveTime::from_sql(ty, raw)?.format("%H:%M:%S%.f").to_string()),
        Type::TIMESTAMP => {
            Scalar::Str(chrono::NaiveDateTime::from_sql(ty, raw)?.format("%Y-%m-%d %H:%M:%S%.f").to_string())
        }
        Type::TIMESTAMPTZ => Scalar::Str(
            chrono::DateTime::<chrono::Utc>::from_sql(ty, raw)?.format("%Y-%m-%d %H:%M:%S%.f+00:00").to_string(),
        ),
        Type::VOID => Scalar::Null,
        _ => {
            return Err(
                format!("PHV cannot read values of type {ty}; cast the column in the SQL, e.g. column::text").into()
            )
        }
    })
}
