// ----- MySQL -----------------------------------------------------------------

use super::handle::{is_local_host, needs_new_timeout, out_of_memory, remaining, Kind, TIMEOUT_MARGIN_MS};
use super::jobs_add::JobsTx;
use super::pg::{check_statement, network_cause};
use super::pool::{lock, Driver, Pooled};
use super::query_async::Scalar;
use super::R;
use crate::db::{parts, render_sql, Dialect, Ended, ResultRows};
use phv_vm::{DatabaseSpec, Value, Vm, VmError};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

pub(super) struct MyConn {
    pub(super) conn: mysql::Conn,
    /// The `max_execution_time` last set on the session, in milliseconds.
    timeout_ms: Option<u64>,
    /// For the side connection that stops a statement at the deadline.
    pub(super) opts: Arc<mysql::Opts>,
    /// The jobs the script's open transaction added (`job_add_in_transaction`).
    pub(super) jobs_tx: Option<JobsTx>,
    /// Stops the statement of an operation of PHV's own stores at its
    /// deadline (see `my_store_limits`).
    pub(super) store_kill: Option<KillAt>,
    /// Set when a statement of a store failed other than with the server's
    /// own error (a network error, a read that timed out): the connection may
    /// be out of step and is not reused (found by the 1.1.1 audit: resetting
    /// it waited for the read timeout again).
    failed: Arc<std::sync::atomic::AtomicBool>,
    /// `innodb_rollback_on_timeout`, once read (`rolls_back_on_timeout`).
    rollback_on_timeout: Option<bool>,
    /// The read and write timeout of the socket, and the largest packet,
    /// fixed when the connection was made from the limits of the script that
    /// made it (`my_io_limits`); the driver cannot change them later.
    io_timeout: Duration,
    packet: usize,
}

impl MyConn {
    /// Notes the errors of a store's statements (see `failed`).
    pub(super) fn watch(&self) -> MyWatch {
        MyWatch(self.failed.clone())
    }

    /// Whether a lock wait timeout rolls the whole transaction back
    /// (`innodb_rollback_on_timeout`, a server setting read once per
    /// connection; when it cannot be read, taken as on this time).
    pub(super) fn rolls_back_on_timeout(&mut self) -> bool {
        use mysql::prelude::Queryable;
        if self.rollback_on_timeout.is_none() {
            match self.conn.query_first::<i64, _>("SELECT @@innodb_rollback_on_timeout") {
                Ok(v) => self.rollback_on_timeout = Some(v != Some(0)),
                Err(_) => return true,
            }
        }
        self.rollback_on_timeout != Some(false)
    }
}

/// Marks a MySQL connection as not to be reused after an error other than the
/// server's own (see `MyConn::failed`).
pub(super) struct MyWatch(Arc<std::sync::atomic::AtomicBool>);

impl MyWatch {
    pub(super) fn raw(&self, e: mysql::Error) -> mysql::Error {
        if !matches!(e, mysql::Error::MySqlError(_)) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        e
    }

    pub(super) fn msg(&self, e: mysql::Error) -> String {
        my_message(&self.raw(e))
    }
}

impl Driver for MyConn {
    fn reset(&mut self) -> bool {
        self.timeout_ms = None;
        self.jobs_tx = None;
        // A KILL QUERY sent (or being sent) at the deadline could stop the
        // next statement on the connection.
        let killed = self.store_kill.take().is_some_and(KillAt::disarm);
        if killed || self.failed.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        // Rolls back an open transaction and clears session variables and temporary tables.
        if self.conn.reset().is_err() {
            return false;
        }
        // COM_RESET_CONNECTION keeps the active role and the current database:
        // both are put back with one query of two statements (one round trip
        // instead of two), whose replies are checked in order. The command
        // itself cannot go in the same round trip: the client keeps the
        // functions that write a command and read a packet to itself.
        match self.opts.get_db_name() {
            Some(db) => {
                my_statements(&mut self.conn, &format!("SET ROLE DEFAULT; USE `{}`", db.replace('`', "``"))).is_ok()
            }
            // Without a database in the URL there is none to go back to: a script that
            // chose one leaves a connection that is not reused.
            None => matches!(
                my_statements(&mut self.conn, "SET ROLE DEFAULT; SELECT DATABASE()").as_deref(),
                Ok([row]) if row.get::<Option<String>, _>(0) == Some(None)
            ),
        }
    }

    fn check(&mut self) -> bool {
        self.conn.ping().is_ok()
    }
}

/// Runs `sql`, several statements in one round trip, and returns the rows of
/// the last. The replies are read in order; the first statement that fails
/// gives its error (the server runs none after it), whichever it is.
pub(super) fn my_statements(conn: &mut mysql::Conn, sql: &str) -> Result<Vec<mysql::Row>, mysql::Error> {
    use mysql::prelude::Queryable;
    let mut result = conn.query_iter(sql)?;
    let mut last = Vec::new();
    while let Some(set) = result.iter() {
        last = set.collect::<Result<Vec<_>, _>>()?;
    }
    Ok(last)
}

/// MySQL connects still running (see `netio::with_deadline_in`).
static MY_CONNECTING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(super) fn my_connect(
    url: &str,
    spec: &DatabaseSpec,
    timeout: Duration,
    memory: Option<usize>,
    time_limit: Option<Duration>,
) -> Result<MyConn, String> {
    // The parser's own message could quote the URL, which may hold a password.
    let opts = mysql::Opts::from_url(url).map_err(|_| "the URL is not a valid MySQL URL".to_string())?;
    let local = opts.get_socket().is_some() || is_local_host(&opts.get_ip_or_hostname());
    // `require_ssl` in the URL is kept even when phv.toml would allow plain connections.
    let tls = opts.get_ssl_opts().is_some() || spec.tls.unwrap_or(!local);
    let ssl = match (tls, opts.get_ssl_opts().cloned()) {
        (false, _) => None,
        (true, Some(o)) => Some(o),
        (true, None) => Some(mysql::SslOpts::default()),
    };
    let (io_timeout, packet) = my_io_limits(memory, time_limit);
    let builder = mysql::OptsBuilder::from_opts(opts)
        .ssl_opts(ssl)
        .tcp_connect_timeout(Some(timeout))
        .read_timeout(Some(io_timeout))
        .write_timeout(Some(io_timeout))
        .max_allowed_packet(Some(packet));
    let opts = Arc::new(mysql::Opts::from(builder.clone()));
    // The driver looks the host up itself, with no deadline of its own, and
    // waits for a server's greeting as long as for any answer.
    let conn =
        crate::netio::with_deadline_in(&MY_CONNECTING, Instant::now() + timeout, move || mysql::Conn::new(builder))?
            .map_err(|e| my_message(&e))?;
    Ok(MyConn {
        conn,
        timeout_ms: None,
        opts,
        jobs_tx: None,
        store_kill: None,
        failed: Default::default(),
        rollback_on_timeout: None,
        io_timeout,
        packet,
    })
}

/// The socket timeout and the largest packet of a connection made for a
/// script with these limits. A server that stops answering is given up on a
/// little after the time limit (statements are stopped at the deadline with
/// KILL QUERY before that); no packet (one row, mostly) is larger than half
/// the script's memory.
pub(super) fn my_io_limits(memory: Option<usize>, time_limit: Option<Duration>) -> (Duration, usize) {
    let io_timeout = time_limit.map_or(Duration::from_secs(300), |t| t + Duration::from_secs(10));
    (io_timeout, memory.unwrap_or(128 << 20) / 2)
}

/// Whether an idle connection made for another script serves one with these
/// limits: its socket waits long enough, and it takes packets as large
/// (found while checking the fixes of the 19th assessment: a connection made
/// by a script with a 2-second limit, taken by one with 60 seconds, gave up
/// on a 14-second statement after 12 seconds with "the time limit was
/// reached"). A connection made for a somewhat longer limit serves a shorter
/// one: its statements are stopped at the deadline by KILL QUERY, and the
/// socket's timeout only bounds the wait for a server that stops answering,
/// where KILL QUERY cannot get through either. That wait is bounded above
/// too, at twice the script's own (found by the 20th audit: a page could
/// take a connection a job with a one-hour limit made, and wait an hour past
/// its 30 seconds for a server that stopped answering).
pub(super) fn my_fits(memory: Option<usize>, time_limit: Option<Duration>) -> impl Fn(&MyConn) -> bool {
    let (io_timeout, packet) = my_io_limits(memory, time_limit);
    move |c: &MyConn| io_timeout_fits(c.io_timeout, io_timeout) && c.packet >= packet
}

/// Whether a connection's socket timeout `have` serves a script that needs
/// `need` (see `my_fits`): at least as long, and at most twice as long.
pub(super) fn io_timeout_fits(have: Duration, need: Duration) -> bool {
    have >= need && have <= need.saturating_mul(2)
}

pub(super) fn my_message(e: &mysql::Error) -> String {
    match e {
        mysql::Error::MySqlError(m) => m.message.clone(),
        // Network, TLS and connection errors are described without the server's address.
        mysql::Error::IoError(io) => crate::netio::describe(io),
        mysql::Error::TlsError(_) => "the TLS connection failed".into(),
        mysql::Error::CodecError(c) => match network_cause(c) {
            Some(m) => m,
            None => "the server sent an invalid packet (larger than the memory left to the script?)".into(),
        },
        mysql::Error::DriverError(mysql::DriverError::CouldNotConnect(_)) => "cannot connect to the server".into(),
        e => e.to_string(),
    }
}

/// `KILL QUERY`s being sent at once on the pool's threads (each on a side
/// connection of its own); more start a thread of their own, as before.
pub(crate) const MAX_KILLING: usize = 32;
static KILLING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A statement to stop at its deadline: the connection's options and id.
struct Watched {
    opts: Arc<mysql::Opts>,
    id: u32,
}

/// The statements being watched, by deadline (and a number, to tell apart
/// statements with the same deadline), and the thread that watches them.
struct Watchdog {
    due: Mutex<std::collections::BTreeMap<(Instant, u64), Watched>>,
    /// Woken when a statement with an earlier deadline than all others comes.
    sooner: Condvar,
    next: std::sync::atomic::AtomicU64,
}

static WATCHDOG: LazyLock<Option<&'static Watchdog>> = LazyLock::new(|| {
    let dog: &'static Watchdog = Box::leak(Box::new(Watchdog {
        due: Mutex::new(Default::default()),
        sooner: Condvar::new(),
        next: Default::default(),
    }));
    std::thread::Builder::new().name("phv-mysql-watchdog".into()).spawn(move || dog.watch()).ok().map(|_| dog)
});

impl Watchdog {
    /// Sends `KILL QUERY` for each statement whose deadline has passed.
    fn watch(&self) {
        let mut due = lock(&self.due);
        loop {
            let now = Instant::now();
            let next = due.first_key_value().map(|(k, _)| *k);
            match next {
                None => due = self.sooner.wait(due).unwrap_or_else(|e| e.into_inner()),
                Some(key) if key.0 > now => {
                    due = self.sooner.wait_timeout(due, key.0 - now).unwrap_or_else(|e| e.into_inner()).0;
                }
                Some(key) => {
                    let w = due.remove(&key).expect("first key");
                    drop(due);
                    kill_query(w);
                    due = lock(&self.due);
                }
            }
        }
    }
}

/// Stops the statement on connection `w.id` from a second connection.
fn kill_query(w: Watched) {
    let send = move || {
        use mysql::prelude::Queryable;
        let side = mysql::OptsBuilder::from_opts((*w.opts).clone())
            .tcp_connect_timeout(Some(Duration::from_secs(5)))
            .read_timeout(Some(Duration::from_secs(5)))
            .write_timeout(Some(Duration::from_secs(5)));
        if let Ok(mut side) = mysql::Conn::new(side) {
            let _ = side.query_drop(format!("KILL QUERY {}", w.id));
        }
    };
    use std::sync::atomic::Ordering;
    if KILLING.fetch_add(1, Ordering::SeqCst) < MAX_KILLING {
        let job = Arc::new(Mutex::new(Some(send)));
        let in_pool = job.clone();
        let pooled = crate::workers::spawn(move || {
            if let Some(send) = lock(&in_pool).take() {
                send();
            }
            KILLING.fetch_sub(1, Ordering::SeqCst);
        });
        if pooled.is_ok() {
            return;
        }
        KILLING.fetch_sub(1, Ordering::SeqCst);
        if let Some(send) = lock(&job).take() {
            let _ = std::thread::Builder::new().spawn(send);
        }
        return;
    }
    KILLING.fetch_sub(1, Ordering::SeqCst);
    let _ = std::thread::Builder::new().spawn(send);
}

/// Stops the statement running on connection `id` with `KILL QUERY` (from a
/// second connection) when it is still running at `deadline`. Dropping the
/// guard before then cancels it. One thread watches the deadlines of every
/// statement in the process.
pub(super) enum KillAt {
    None,
    Watched((Instant, u64)),
    /// Watched by a thread of its own (when the watchdog could not start).
    Alone(std::sync::mpsc::Sender<()>),
}

impl KillAt {
    pub(super) fn arm(opts: &Arc<mysql::Opts>, id: u32, deadline: Option<Instant>) -> KillAt {
        let Some(deadline) = deadline else { return KillAt::None };
        let w = Watched { opts: opts.clone(), id };
        let Some(dog) = *WATCHDOG else {
            let (stop, wait) = std::sync::mpsc::channel::<()>();
            std::thread::spawn(move || {
                let left = deadline.saturating_duration_since(Instant::now());
                if wait.recv_timeout(left) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                    kill_query(w);
                }
            });
            return KillAt::Alone(stop);
        };
        let key = (deadline, dog.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let mut due = lock(&dog.due);
        let first = due.first_key_value().is_none_or(|(k, _)| key < *k);
        due.insert(key, w);
        drop(due);
        if first {
            dog.sooner.notify_one();
        }
        KillAt::Watched(key)
    }
}

impl KillAt {
    /// Cancels the kill once the statement has ended. True when it was sent
    /// (or is being sent) already: it could stop the next statement on the
    /// connection, which is then not used again (found by the 1.0.0 audit).
    pub(super) fn disarm(mut self) -> bool {
        let fired = match &self {
            KillAt::Watched(key) => WATCHDOG.is_some_and(|dog| lock(&dog.due).remove(key).is_none()),
            KillAt::Alone(stop) => stop.send(()).is_err(),
            KillAt::None => false,
        };
        self = KillAt::None;
        drop(self);
        fired
    }
}

impl Drop for KillAt {
    fn drop(&mut self) {
        match self {
            KillAt::Watched(key) => {
                if let Some(dog) = *WATCHDOG {
                    lock(&dog.due).remove(key);
                }
            }
            KillAt::Alone(stop) => {
                let _ = stop.send(());
            }
            KillAt::None => {}
        }
    }
}

fn my_kill_at(vm: &Vm, c: &mut Pooled<MyConn>) -> KillAt {
    let id = c.get().conn.connection_id();
    // The statement ends on the client side just after the script's deadline.
    KillAt::arm(&c.get().opts, id, crate::deadline::after(vm, Duration::from_millis(TIMEOUT_MARGIN_MS)))
}

pub(super) fn my_error(vm: &Vm, c: &mut Pooled<MyConn>, e: mysql::Error) -> VmError {
    match &e {
        mysql::Error::MySqlError(m) => {
            // 3024: stopped by the max_execution_time set from the script's time limit;
            // 1317: stopped by KILL QUERY at the deadline.
            if m.code == 3024 || m.code == 1317 {
                if let Err(fatal) = vm.check_limits() {
                    return fatal;
                }
            }
            // 1213 (a deadlock) rolls the whole transaction back, and 1205 (a
            // lock wait timeout) does with `innodb_rollback_on_timeout`: the
            // statements after it would run outside any transaction (found by
            // the 19th audit). What std.orm kept may be undone.
            let whole = match m.code {
                1213 => Some(Ended::Deadlock),
                1205 if c.get().rolls_back_on_timeout() => Some(Ended::LockWait),
                _ => None,
            };
            if let Some(ended) = whole {
                crate::orm::rolled_back();
                if c.depth > 0 && c.ended.is_none() {
                    c.ended = Some(ended);
                }
            }
        }
        _ => c.broken = true,
    }
    vm.throw("DatabaseError", my_message(&e))
}

/// Lowers the server's `max_execution_time` to `left` when needed (see
/// `needs_new_timeout`).
pub(super) fn my_set_timeout(c: &mut Pooled<MyConn>, left: Duration) -> Result<(), mysql::Error> {
    use mysql::prelude::Queryable;
    let ms = left.as_millis().min(u32::MAX as u128) as u64;
    if needs_new_timeout(c.get().timeout_ms, ms) {
        c.get().conn.query_drop(format!("SET SESSION max_execution_time = {}", ms + TIMEOUT_MARGIN_MS))?;
        c.get().timeout_ms = Some(ms);
    }
    Ok(())
}

fn my_limit_time(vm: &Vm, c: &mut Pooled<MyConn>) -> Result<(), VmError> {
    let Some(left) = remaining(vm)? else { return Ok(()) };
    my_set_timeout(c, left).map_err(|e| my_error(vm, c, e))
}

/// Runs one statement of PHV's own on a script's connection, with the
/// script's limits (stopped at its deadline with `KILL QUERY`).
pub(super) fn my_run<T>(
    vm: &Vm,
    c: &mut Pooled<MyConn>,
    run: impl FnOnce(&mut mysql::Conn) -> Result<T, mysql::Error>,
) -> Result<T, VmError> {
    my_limit_time(vm, c)?;
    let kill = my_kill_at(vm, c);
    let r = run(&mut c.get().conn);
    if kill.disarm() {
        c.broken = true;
    }
    r.map_err(|e| my_error(vm, c, e))
}

pub(super) fn my_params(params: &[Value]) -> mysql::Params {
    if params.is_empty() {
        return mysql::Params::Empty;
    }
    mysql::Params::Positional(
        params
            .iter()
            .map(|v| match v {
                Value::Null => mysql::Value::NULL,
                Value::Bool(b) => mysql::Value::Int(*b as i64),
                Value::Int(i) => mysql::Value::Int(*i),
                Value::Float(f) => mysql::Value::Double(*f),
                other => mysql::Value::Bytes(other.to_display_string().into_bytes()),
            })
            .collect(),
    )
}

pub(super) fn my_value(v: mysql::Value, col: &mysql::Column) -> Result<Scalar, String> {
    use mysql::consts::ColumnType;
    Ok(match v {
        mysql::Value::NULL => Scalar::Null,
        mysql::Value::Int(i) => Scalar::Int(i),
        mysql::Value::UInt(u) => {
            Scalar::Int(i64::try_from(u).map_err(|_| format!("column {}: {u} is too large for int", col.name_str()))?)
        }
        mysql::Value::Float(f) => Scalar::Float(f as f64),
        mysql::Value::Double(f) => Scalar::Float(f),
        mysql::Value::Bytes(b) => Scalar::Str(String::from_utf8_lossy(&b).into_owned()),
        mysql::Value::Date(y, mo, d, h, mi, s, us) => Scalar::Str(match col.column_type() {
            ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => format!("{y:04}-{mo:02}-{d:02}"),
            _ if us > 0 => format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}"),
            _ => format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"),
        }),
        mysql::Value::Time(neg, days, h, mi, s, us) => {
            let sign = if neg { "-" } else { "" };
            let hours = days as u64 * 24 + h as u64;
            Scalar::Str(if us > 0 {
                format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}")
            } else {
                format!("{sign}{hours:02}:{mi:02}:{s:02}")
            })
        }
    })
}

pub(super) fn my_query(vm: &mut Vm, c: &mut Pooled<MyConn>, q: &Value) -> R {
    use mysql::prelude::Queryable;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::MySql);
    check_statement(vm, &text, Kind::MySql)?;
    my_limit_time(vm, c)?;
    let kill = my_kill_at(vm, c);
    let mut out = ResultRows::new(Vec::new());
    let mut convert_error = None;
    let mut over = false;
    let result: Result<(), mysql::Error> = (|| {
        let mut result = c.get().conn.exec_iter(text, my_params(params))?;
        let Some(set) = result.iter() else { return Ok(()) };
        let columns: Vec<mysql::Column> = set.columns().as_ref().to_vec();
        out = ResultRows::new(columns.iter().map(|col| col.name_str().into_owned()));
        for row in set {
            let raw = row?.unwrap();
            let mut values = Vec::with_capacity(columns.len());
            for (v, col) in raw.into_iter().zip(&columns) {
                match my_value(v, col) {
                    Ok(v) => values.push(v.into_value()),
                    Err(e) => {
                        convert_error = Some(e);
                        return Ok(());
                    }
                }
            }
            if out.push(vm, values).is_err() {
                over = true;
                return Ok(());
            }
        }
        Ok(())
    })();
    if kill.disarm() {
        c.broken = true;
    }
    result.map_err(|e| my_error(vm, c, e))?;
    if let Some(e) = convert_error {
        // The rest of the result was not read: the connection is out of step.
        c.broken = true;
        return Err(vm.throw("DatabaseError", e));
    }
    if over {
        c.broken = true;
        return Err(out_of_memory(vm));
    }
    Ok(out.into_value())
}

pub(super) fn my_execute(vm: &mut Vm, c: &mut Pooled<MyConn>, q: &Value) -> R {
    use mysql::prelude::Queryable;
    let (text, params) = parts(q);
    let text = render_sql(text, Dialect::MySql);
    check_statement(vm, &text, Kind::MySql)?;
    my_limit_time(vm, c)?;
    let kill = my_kill_at(vm, c);
    let mut r = c.get().conn.exec_drop(&text, my_params(params));
    // Statements such as BEGIN cannot be prepared (error 1295). Without
    // parameters they run as plain text, but only when the text holds no `;`,
    // since the text protocol would run a second statement after it.
    let unsupported = matches!(&r, Err(mysql::Error::MySqlError(m)) if m.code == 1295);
    if unsupported && params.is_empty() && !text.contains(';') {
        r = c.get().conn.query_drop(&text);
    }
    if kill.disarm() {
        c.broken = true;
    }
    r.map_err(|e| my_error(vm, c, e))?;
    Ok(Value::Int(c.get().conn.affected_rows().try_into().unwrap_or(i64::MAX)))
}
