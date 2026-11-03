//! Connections for the network clients (`std.net`): TCP with an optional TLS
//! layer whose certificates are checked against the public CAs.
//!
//! Every socket read and write waits at most until the stream's deadline, so a
//! server that sends one byte at a time cannot keep a script past its time
//! limit (a socket timeout alone limits each read, not the total). Name
//! lookups, which the OS cannot cancel, are given up on at the deadline too
//! (`with_deadline`).

use crate::netdb::{is_local_host, tls_init};
use phv_vm::Vm;
use std::io::{Error, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

/// A TCP connection whose reads and writes stop at `until`.
pub(crate) struct Socket {
    tcp: TcpStream,
    until: Instant,
}

impl Socket {
    /// The time left, or a `TimedOut` error once the deadline has passed.
    fn left(&self) -> std::io::Result<Duration> {
        let now = Instant::now();
        if now >= self.until {
            return Err(Error::new(ErrorKind::TimedOut, "timed out"));
        }
        Ok((self.until - now).max(Duration::from_millis(1)))
    }
}

/// A socket timeout is `WouldBlock` on Unix and `TimedOut` on Windows; it is `TimedOut` here.
fn timed_out<T>(r: std::io::Result<T>) -> std::io::Result<T> {
    match r {
        Err(e) if e.kind() == ErrorKind::WouldBlock => Err(Error::new(ErrorKind::TimedOut, "timed out")),
        r => r,
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.left()?;
        self.tcp.set_read_timeout(Some(left))?;
        timed_out(self.tcp.read(buf))
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let left = self.left()?;
        self.tcp.set_write_timeout(Some(left))?;
        timed_out(self.tcp.write(buf))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp.flush()
    }
}

pub(crate) enum Stream {
    Plain(Socket),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, Socket>>),
}

impl Stream {
    fn socket(&mut self) -> &mut Socket {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(t) => &mut t.sock,
        }
    }

    /// Reads and writes from now on stop at `until`.
    pub(crate) fn set_deadline(&mut self, until: Instant) {
        self.socket().until = until;
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(t) => t.read(buf),
            Stream::Tls(t) => t.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(t) => t.write(buf),
            Stream::Tls(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(t) => t.flush(),
            Stream::Tls(t) => t.flush(),
        }
    }
}

static TLS: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
});

/// The script's deadline, or `cap` from now when it has none.
pub(crate) fn until(vm: &Vm, cap: Duration) -> Instant {
    let now = Instant::now();
    vm.deadline().map_or(now + cap, |d| d.min(now + cap))
}

/// Refuses an unencrypted connection to another machine unless `tls = false`
/// was set (`tls_setting` is the setting from phv.toml).
pub(crate) fn check_plain(host: &str, tls: bool, tls_setting: Option<bool>, secure: &str) -> Result<(), String> {
    if !tls && tls_setting.unwrap_or(!is_local_host(host)) {
        return Err(format!("the server is not on this machine: use a {secure} URL (TLS), or set tls = false"));
    }
    Ok(())
}

/// A network error in words that never name the host: messages can reach the
/// script and error pages, and the host of a configured service is not shown.
pub(crate) fn describe(e: &Error) -> String {
    if let Some(tls) = e.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()) {
        return match tls {
            rustls::Error::InvalidCertificate(_) => "the server's TLS certificate is not valid".into(),
            rustls::Error::AlertReceived(_) => "the server refused the TLS connection".into(),
            _ => "the TLS connection failed".into(),
        };
    }
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => "timed out".into(),
        ErrorKind::ConnectionRefused => "the connection was refused".into(),
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => "the connection was reset".into(),
        ErrorKind::UnexpectedEof => "the server closed the connection".into(),
        _ => "network error".into(),
    }
}

/// Blocking work (name lookups, the MySQL driver's connect) that is still
/// running, including work whose caller gave up at its deadline.
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// At most this much abandoned blocking work at once: a resolver that never
/// answers then costs that many threads, not one per request.
const MAX_PENDING: usize = 32;

/// Runs `work` on its own thread and waits for it until `until`. The OS cannot
/// cancel a name lookup, so on timeout the thread is left to finish on its own
/// and its result is dropped.
pub(crate) fn with_deadline<T: Send + 'static>(
    until: Instant,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    if PENDING.fetch_add(1, Ordering::SeqCst) >= MAX_PENDING {
        PENDING.fetch_sub(1, Ordering::SeqCst);
        return Err("too many network connections are being set up".into());
    }
    let (send, recv) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new().stack_size(256 << 10).spawn(move || {
        let _ = send.send(work());
        PENDING.fetch_sub(1, Ordering::SeqCst);
    });
    if spawned.is_err() {
        PENDING.fetch_sub(1, Ordering::SeqCst);
        return Err("cannot start the connection".into());
    }
    recv.recv_timeout(until.saturating_duration_since(Instant::now())).map_err(|_| "timed out".to_string())
}

/// The addresses of `host`, looked up before `until`.
pub(crate) fn resolve(host: &str, port: u16, until: Instant) -> Result<Vec<SocketAddr>, String> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let name = bare.to_string();
    let addrs = with_deadline(until, move || (name.as_str(), port).to_socket_addrs().map(|a| a.collect::<Vec<_>>()))?;
    match addrs {
        Ok(a) if !a.is_empty() => Ok(a),
        _ => Err("cannot resolve the host name".into()),
    }
}

/// Connects to `host:port` before `until`, then starts TLS when `tls` is set.
/// Reads and writes on the stream also stop at `until` (see `set_deadline`).
pub(crate) fn dial(host: &str, port: u16, tls: bool, until: Instant) -> Result<Stream, String> {
    let left = || until.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs = resolve(bare, port, until)?;
    let mut last = "cannot resolve the host name".to_string();
    let mut tcp = None;
    for addr in addrs {
        if Instant::now() >= until {
            last = "timed out".into();
            break;
        }
        match TcpStream::connect_timeout(&addr, left()) {
            Ok(t) => {
                tcp = Some(t);
                break;
            }
            Err(e) => last = describe(&e),
        }
    }
    let tcp = tcp.ok_or(last)?;
    let _ = tcp.set_nodelay(true);
    let socket = Socket { tcp, until };
    if !tls {
        return Ok(Stream::Plain(socket));
    }
    tls_init();
    let name = rustls::pki_types::ServerName::try_from(bare.to_string()).map_err(|_| "invalid host name")?;
    let session = rustls::ClientConnection::new(TLS.clone(), name).map_err(|_| "the TLS connection failed")?;
    Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(session, socket))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A server that drips one byte every 200 ms: a read loop must still stop at the deadline.
    #[test]
    fn reads_stop_at_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                for _ in 0..50 {
                    if s.write_all(b"x").is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        });
        let start = Instant::now();
        let mut stream = dial("127.0.0.1", port, false, start + Duration::from_millis(700)).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 16];
        let err = loop {
            match stream.read(&mut buf) {
                Ok(0) => panic!("the server closed early"),
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) => break e,
            }
        };
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_millis(1500), "took {:?}", start.elapsed());
        assert!(!got.is_empty() && got.len() < 10, "{got:?}");
        assert_eq!(describe(&err), "timed out");
    }

    /// Work that never finishes (a resolver that does not answer) is given up
    /// on at the deadline, and abandoned work is capped.
    #[test]
    fn blocking_work_stops_at_the_deadline() {
        let start = Instant::now();
        let r = with_deadline(start + Duration::from_millis(200), || std::thread::sleep(Duration::from_secs(3)));
        assert_eq!(r, Err("timed out".to_string()));
        assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
        assert_eq!(with_deadline(Instant::now() + Duration::from_secs(1), || 7), Ok(7));

        let stuck: Vec<_> = (0..MAX_PENDING + 2)
            .map(|_| {
                with_deadline(Instant::now() + Duration::from_millis(10), || std::thread::sleep(Duration::from_secs(2)))
            })
            .collect();
        assert!(stuck.iter().any(|r| r.as_ref().is_err_and(|e| e.starts_with("too many"))), "{stuck:?}");
        assert_eq!(resolve("[::1]", 80, Instant::now()).unwrap(), vec!["[::1]:80".parse().unwrap()]);
        std::thread::sleep(Duration::from_secs(2));
        let local = resolve("localhost", 80, Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(local.iter().all(|a| a.ip().is_loopback()), "{local:?}");
    }
}
