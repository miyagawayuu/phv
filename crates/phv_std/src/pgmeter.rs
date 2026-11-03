//! PostgreSQL connections, driven by a small runtime of their own over a
//! metered socket. Before each statement the script's deadline and the memory
//! it has left are set on the meter; reads and writes stop at that deadline,
//! and reading more bytes than the budget fails. The connection is then closed
//! (and not reused).
//!
//! This holds whatever the server does: a script that turns `statement_timeout`
//! off, a statement that returns one huge row, a flood of notifications, or a
//! hostile server that never answers. Notifications and notices are dropped as
//! they arrive (scripts cannot read them), so they never pile up.

use futures_util::StreamExt;
use postgres::types::ToSql;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres::{Client, Config, Error, Row, Statement};

/// Why the meter stopped a connection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Trip {
    Deadline,
    Budget,
}

#[derive(Default)]
struct State {
    deadline: Option<Instant>,
    /// Bytes that may still be read.
    budget: usize,
    tripped: Option<Trip>,
}

/// Shared between a connection's socket and the code that runs statements on it.
#[derive(Clone, Default)]
pub(crate) struct Meter(Arc<Mutex<State>>);

impl Meter {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// From now on, reads and writes stop at `deadline` and after `budget` bytes.
    pub(crate) fn set(&self, deadline: Option<Instant>, budget: usize) {
        let mut st = self.state();
        st.deadline = deadline;
        st.budget = budget;
        st.tripped = None;
    }

    /// Why the last operation failed, when the meter stopped it.
    pub(crate) fn tripped(&self) -> Option<Trip> {
        self.state().tripped
    }

    fn trip(&self, why: Trip) -> io::Error {
        self.state().tripped = Some(why);
        match why {
            Trip::Deadline => io::Error::new(io::ErrorKind::TimedOut, "the time limit was reached"),
            Trip::Budget => io::Error::other("the result is larger than the memory left to the script"),
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.state().deadline
    }
}

pub(crate) struct MeteredStream<S> {
    inner: S,
    meter: Meter,
    /// A timer for the deadline, created when a read or write has to wait.
    timer: Option<(Instant, Pin<Box<tokio::time::Sleep>>)>,
}

impl<S> MeteredStream<S> {
    /// Whether the deadline has passed (checked before and while waiting).
    fn past_deadline(&mut self, cx: &mut Context<'_>) -> bool {
        let Some(deadline) = self.meter.deadline() else {
            self.timer = None;
            return false;
        };
        if Instant::now() >= deadline {
            return true;
        }
        if self.timer.as_ref().is_none_or(|(at, _)| *at != deadline) {
            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
            self.timer = Some((deadline, Box::pin(sleep)));
        }
        let (_, sleep) = self.timer.as_mut().expect("timer");
        sleep.as_mut().poll(cx).is_ready()
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for MeteredStream<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if this.meter.deadline().is_some_and(|d| Instant::now() >= d) {
            return Poll::Ready(Err(this.meter.trip(Trip::Deadline)));
        }
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let n = buf.filled().len() - before;
                let mut st = this.meter.state();
                if n > st.budget {
                    drop(st);
                    return Poll::Ready(Err(this.meter.trip(Trip::Budget)));
                }
                st.budget -= n;
                Poll::Ready(Ok(()))
            }
            Poll::Pending => {
                if this.past_deadline(cx) {
                    return Poll::Ready(Err(this.meter.trip(Trip::Deadline)));
                }
                Poll::Pending
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for MeteredStream<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        if this.meter.deadline().is_some_and(|d| Instant::now() >= d) {
            return Poll::Ready(Err(this.meter.trip(Trip::Deadline)));
        }
        match Pin::new(&mut this.inner).poll_write(cx, data) {
            Poll::Pending if this.past_deadline(cx) => Poll::Ready(Err(this.meter.trip(Trip::Deadline))),
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Pending if this.past_deadline(cx) => Poll::Ready(Err(this.meter.trip(Trip::Deadline))),
            other => other,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A PostgreSQL connection used from synchronous code.
pub(crate) struct PgClient {
    rt: tokio::runtime::Runtime,
    client: Client,
}

impl PgClient {
    /// Connects to the first host of `config` that answers, within `timeout`.
    /// `tls` is the TLS connector when the connection must be encrypted.
    pub(crate) fn connect(
        config: &Config,
        tls: Option<tokio_postgres_rustls::MakeRustlsConnect>,
        timeout: Duration,
        meter: Meter,
    ) -> Result<PgClient, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "cannot start the database connection".to_string())?;
        let until = std::time::Instant::now() + timeout;
        let client = rt.block_on(async {
            match tokio::time::timeout(timeout, open(config, tls, meter, until)).await {
                Ok(r) => r,
                Err(_) => Err("timed out".to_string()),
            }
        })?;
        Ok(PgClient { rt, client })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    pub(crate) fn is_valid(&self, timeout: Duration) -> bool {
        self.rt
            .block_on(async { matches!(tokio::time::timeout(timeout, self.client.simple_query("")).await, Ok(Ok(_))) })
    }

    pub(crate) fn batch_execute(&self, text: &str) -> Result<(), Error> {
        self.rt.block_on(self.client.batch_execute(text))
    }

    pub(crate) fn prepare(&self, text: &str) -> Result<Statement, Error> {
        self.rt.block_on(self.client.prepare(text))
    }

    pub(crate) fn execute(&self, text: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, Error> {
        self.rt.block_on(self.client.execute(text, params))
    }

    pub(crate) fn query(&self, text: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, Error> {
        self.rt.block_on(self.client.query(text, params))
    }

    /// Runs `stmt` and calls `each` for every row until it returns false.
    pub(crate) fn query_each<'a, I>(
        &self,
        stmt: &Statement,
        params: I,
        mut each: impl FnMut(Row) -> Result<bool, Error>,
    ) -> Result<(), Error>
    where
        I: IntoIterator<Item = &'a dyn ToSql>,
        I::IntoIter: ExactSizeIterator,
    {
        self.rt.block_on(async {
            let rows = self.client.query_raw(stmt, params).await?;
            let mut rows = std::pin::pin!(rows);
            while let Some(row) = rows.next().await {
                if !each(row?)? {
                    break;
                }
            }
            Ok(())
        })
    }
}

async fn open(
    config: &Config,
    tls: Option<tokio_postgres_rustls::MakeRustlsConnect>,
    meter: Meter,
    until: std::time::Instant,
) -> Result<Client, String> {
    use tokio_postgres::config::Host;
    let ports = config.get_ports();
    let mut last = "the URL names no host".to_string();
    for (i, host) in config.get_hosts().iter().enumerate() {
        // Unix sockets (on Unix) are not used: connections go through the metered TCP socket.
        #[allow(irrefutable_let_patterns)]
        let Host::Tcp(name) = host
        else {
            continue;
        };
        let port = ports.get(i).or(ports.first()).copied().unwrap_or(5432);
        // Looked up here, with a deadline: tokio's own lookup runs on a thread
        // that dropping the runtime would wait for.
        let addrs = match config.get_hostaddrs().get(i) {
            Some(ip) => vec![std::net::SocketAddr::new(*ip, port)],
            None => match crate::netio::resolve(name, port, until) {
                Ok(a) => a,
                Err(e) => {
                    last = e;
                    continue;
                }
            },
        };
        let tcp = match TcpStream::connect(&addrs[..]).await {
            Ok(t) => t,
            Err(e) => {
                last = crate::netio::describe(&e);
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let stream = MeteredStream { inner: tcp, meter: meter.clone(), timer: None };
        let message = |e: Error| crate::netdb::pg_message(&e);
        let client = match &tls {
            Some(make) => {
                let mut make = make.clone();
                let connector = MakeTlsConnect::<MeteredStream<TcpStream>>::make_tls_connect(&mut make, name)
                    .map_err(|_| "invalid host name".to_string())?;
                let (client, connection) = config.connect_raw(stream, connector).await.map_err(message)?;
                tokio::spawn(drive(connection));
                client
            }
            None => {
                let (client, connection) = config.connect_raw(stream, tokio_postgres::NoTls).await.map_err(message)?;
                tokio::spawn(drive(connection));
                client
            }
        };
        return Ok(client);
    }
    Err(last)
}

/// Runs a connection until it closes, dropping notices and notifications.
async fn drive<S, T>(mut connection: tokio_postgres::Connection<S, T>)
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(Ok(_)) = std::future::poll_fn(|cx| connection.poll_message(cx)).await {}
}
