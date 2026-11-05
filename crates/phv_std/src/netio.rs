//! Connections for the network clients (`std.net`): TCP with an optional TLS
//! layer whose certificates are checked against the public CAs.
//!
//! Every socket read and write waits at most until the stream's deadline, so a
//! server that sends one byte at a time cannot keep a script past its time
//! limit (a socket timeout alone limits each read, not the total). Name
//! lookups, which the OS cannot cancel, are given up on at the deadline too
//! (`with_deadline`).
//!
//! Looked-up addresses are kept for `DNS_TTL` (`resolve`), and lookups of the
//! same name at the same time share one lookup. Failed lookups are not kept.

use crate::netdb::{is_local_host, tls_init};
use phv_vm::Vm;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
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

    /// Upgrades a plain connection to TLS (SMTP's `STARTTLS`), checking `host`'s certificate.
    pub(crate) fn start_tls(self, host: &str) -> Result<Stream, String> {
        let Stream::Plain(socket) = self else { return Ok(self) };
        tls_init();
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let name = rustls::pki_types::ServerName::try_from(bare.to_string()).map_err(|_| "invalid host name")?;
        let session = rustls::ClientConnection::new(tls_config(), name).map_err(|_| "the TLS connection failed")?;
        Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(session, socket))))
    }

    /// Reads and writes from now on stop at `until`.
    pub(crate) fn set_deadline(&mut self, until: Instant) {
        self.socket().until = until;
    }

    /// Whether nothing arrived on an idle connection: no byte waiting (also
    /// none TLS has already decrypted) and no close. Only such a connection
    /// may carry another request, or the next one would read what was sent
    /// meanwhile as its answer. Checked without waiting.
    pub(crate) fn quiet(&mut self) -> bool {
        if let Stream::Tls(t) = self {
            match t.conn.process_new_packets() {
                Ok(state) if state.plaintext_bytes_to_read() == 0 && !state.peer_has_closed() => {}
                _ => return false,
            }
        }
        let tcp = &self.socket().tcp;
        if tcp.set_nonblocking(true).is_err() {
            return false;
        }
        let r = tcp.peek(&mut [0u8; 1]);
        let blocking = tcp.set_nonblocking(false).is_ok();
        blocking && matches!(r, Err(e) if e.kind() == ErrorKind::WouldBlock)
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

/// The TLS client settings of every connection PHV makes (HTTP, SMTP, Redis,
/// WebSocket, PostgreSQL): the public CAs, safe protocol versions, no client
/// certificate. One configuration, so TLS sessions are resumed across them.
pub(crate) fn tls_config() -> Arc<rustls::ClientConfig> {
    TLS.clone()
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
pub(crate) const MAX_PENDING: usize = 32;

/// Runs `work` on a thread of the pool (`workers`) and waits for it until
/// `until`. The OS cannot cancel a name lookup, so on timeout the work is left
/// to finish on its own and its result is dropped. (Name lookups go through
/// `resolve`, which counts them the same way.)
#[cfg(test)]
pub(crate) fn with_deadline<T: Send + 'static>(
    until: Instant,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    with_deadline_in(&PENDING, until, work)
}

/// `with_deadline`, counting the work in `pending` (at most `MAX_PENDING`):
/// work that can hang for long (the MySQL driver's connect, when a server
/// accepts but never answers) has its own count, so it cannot use up the
/// slots of name lookups.
pub(crate) fn with_deadline_in<T: Send + 'static>(
    pending: &'static AtomicUsize,
    until: Instant,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    if pending.fetch_add(1, Ordering::SeqCst) >= MAX_PENDING {
        pending.fetch_sub(1, Ordering::SeqCst);
        return Err("too many network connections are being set up".into());
    }
    let (send, recv) = std::sync::mpsc::sync_channel(1);
    let spawned = crate::workers::spawn(move || {
        let _ = send.send(work());
        pending.fetch_sub(1, Ordering::SeqCst);
    });
    if spawned.is_err() {
        pending.fetch_sub(1, Ordering::SeqCst);
        return Err("cannot start the connection".into());
    }
    recv.recv_timeout(until.saturating_duration_since(Instant::now())).map_err(|_| "timed out".to_string())
}

/// How long looked-up addresses are kept.
const DNS_TTL: Duration = Duration::from_secs(30);
/// Names kept at most (in practice, the hosts of phv.toml's services).
const DNS_NAMES: usize = 1024;

/// The addresses of a name, or why it has none.
type Found = Result<Arc<[IpAddr]>, String>;

/// A lookup in progress, which the callers that want the same name wait for.
struct Flight {
    found: Mutex<Option<Found>>,
    done: Condvar,
}

enum Entry {
    Known { addrs: Arc<[IpAddr]>, at: Instant },
    Looking { flight: Arc<Flight>, since: Instant },
}

/// Looked-up names, with how long they are kept and the lookup that finds them.
struct Names {
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    lookup: fn(&str) -> std::io::Result<Vec<IpAddr>>,
}

fn system_lookup(name: &str) -> std::io::Result<Vec<IpAddr>> {
    Ok((name, 0).to_socket_addrs()?.map(|a| a.ip()).collect())
}

static NAMES: LazyLock<Arc<Names>> =
    LazyLock::new(|| Arc::new(Names { entries: Mutex::default(), ttl: DNS_TTL, lookup: system_lookup }));

impl Names {
    fn entries(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The addresses of `name`: kept ones while they are fresh, otherwise those
    /// of a lookup (shared with every caller that wants the name meanwhile),
    /// waited for until `until`.
    fn find(self: &Arc<Self>, name: &str, until: Instant) -> Found {
        let flight = {
            let mut entries = self.entries();
            match entries.get(name) {
                Some(Entry::Known { addrs, at }) if at.elapsed() < self.ttl => return Ok(addrs.clone()),
                // A lookup still running after a whole TTL is not waited for:
                // a new one starts (each counts in `PENDING`, as before).
                Some(Entry::Looking { flight, since }) if since.elapsed() < self.ttl => flight.clone(),
                _ => {
                    if entries.len() >= DNS_NAMES {
                        let ttl = self.ttl;
                        entries.retain(|_, e| match e {
                            Entry::Known { at, .. } => at.elapsed() < ttl,
                            Entry::Looking { since, .. } => since.elapsed() < ttl,
                        });
                        if entries.len() >= DNS_NAMES {
                            entries.retain(|_, e| matches!(e, Entry::Looking { .. }));
                        }
                    }
                    let flight = Arc::new(Flight { found: Mutex::new(None), done: Condvar::new() });
                    entries.insert(name.to_string(), Entry::Looking { flight: flight.clone(), since: Instant::now() });
                    drop(entries);
                    self.start(name, flight.clone());
                    flight
                }
            }
        };
        let mut found = flight.found.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(f) = &*found {
                return f.clone();
            }
            let now = Instant::now();
            if now >= until {
                return Err("timed out".into());
            }
            found = flight.done.wait_timeout(found, until - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Starts the lookup of `name` on the pool; its end fills `flight` (and
    /// the entry, when the lookup found addresses).
    fn start(self: &Arc<Self>, name: &str, flight: Arc<Flight>) {
        let names = self.clone();
        let owned = name.to_string();
        let finish = move |found: Found| {
            let mut entries = names.entries();
            // Only the lookup the entry waits for replaces it; a failure is not kept.
            if matches!(entries.get(&owned), Some(Entry::Looking { flight: f, .. }) if Arc::ptr_eq(f, &flight)) {
                match &found {
                    Ok(addrs) => entries.insert(owned, Entry::Known { addrs: addrs.clone(), at: Instant::now() }),
                    Err(_) => entries.remove(&owned),
                };
            }
            drop(entries);
            *flight.found.lock().unwrap_or_else(|e| e.into_inner()) = Some(found);
            flight.done.notify_all();
        };
        let (lookup, name) = (self.lookup, name.to_string());
        // Counted and given up on like any blocking work (`with_deadline`).
        if PENDING.fetch_add(1, Ordering::SeqCst) >= MAX_PENDING {
            PENDING.fetch_sub(1, Ordering::SeqCst);
            return finish(Err("too many network connections are being set up".into()));
        }
        let job = std::sync::Arc::new(Mutex::new(Some(finish)));
        let in_job = job.clone();
        let spawned = crate::workers::spawn(move || {
            let found = match lookup(&name) {
                Ok(a) if !a.is_empty() => Ok(Arc::from(a)),
                _ => Err("cannot resolve the host name".to_string()),
            };
            PENDING.fetch_sub(1, Ordering::SeqCst);
            if let Some(finish) = in_job.lock().unwrap_or_else(|e| e.into_inner()).take() {
                finish(found);
            }
        });
        if spawned.is_err() {
            PENDING.fetch_sub(1, Ordering::SeqCst);
            if let Some(finish) = job.lock().unwrap_or_else(|e| e.into_inner()).take() {
                finish(Err("cannot start the connection".into()));
            }
        }
    }
}

/// The addresses of `host`, looked up before `until`, or kept from a lookup
/// less than `DNS_TTL` ago.
pub(crate) fn resolve(host: &str, port: u16, until: Instant) -> Result<Vec<SocketAddr>, String> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs = NAMES.find(bare, until)?;
    Ok(addrs.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
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
    let session = rustls::ClientConnection::new(tls_config(), name).map_err(|_| "the TLS connection failed")?;
    Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(session, socket))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Tests that fill the process-wide slots of `with_deadline` run one at a
    /// time, or one would see the other's stuck work.
    static SLOTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Work that runs until `gate` is set: the slots it holds stay taken
    /// however slowly the test fills them (a fixed sleep could end on a
    /// slow machine before the last slot is taken).
    fn held_until(gate: &'static std::sync::atomic::AtomicBool) -> impl FnOnce() + Send + 'static {
        move || {
            while !gate.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Waits until no work is counted in `pending`.
    fn wait_until_idle(pending: &AtomicUsize) {
        let start = Instant::now();
        while pending.load(Ordering::SeqCst) > 0 {
            assert!(start.elapsed() < Duration::from_secs(20), "work still running");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

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
        let _slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        let start = Instant::now();
        let r = with_deadline(start + Duration::from_millis(200), || std::thread::sleep(Duration::from_secs(3)));
        assert_eq!(r, Err("timed out".to_string()));
        assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
        assert_eq!(with_deadline(Instant::now() + Duration::from_secs(1), || 7), Ok(7));

        static GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let stuck: Vec<_> = (0..MAX_PENDING + 2)
            .map(|_| with_deadline(Instant::now() + Duration::from_millis(10), held_until(&GATE)))
            .collect();
        assert!(stuck.iter().any(|r| r.as_ref().is_err_and(|e| e.starts_with("too many"))), "{stuck:?}");
        assert_eq!(resolve("[::1]", 80, Instant::now()).unwrap(), vec!["[::1]:80".parse().unwrap()]);
        GATE.store(true, Ordering::SeqCst);
        wait_until_idle(&PENDING);
        let local = resolve("localhost", 80, Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(local.iter().all(|a| a.ip().is_loopback()), "{local:?}");
    }

    /// Work counted on its own (MySQL connects to a server that never
    /// answers) cannot use up the slots of name lookups.
    #[test]
    fn hanging_work_of_its_own_kind_leaves_lookups_alone() {
        let _slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        static OWN: AtomicUsize = AtomicUsize::new(0);
        static GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let stuck: Vec<_> = (0..MAX_PENDING + 2)
            .map(|_| with_deadline_in(&OWN, Instant::now() + Duration::from_millis(10), held_until(&GATE)))
            .collect();
        assert!(stuck.iter().any(|r| r.as_ref().is_err_and(|e| e.starts_with("too many"))), "{stuck:?}");
        assert_eq!(with_deadline(Instant::now() + Duration::from_secs(1), || 7), Ok(7));
        GATE.store(true, Ordering::SeqCst);
        wait_until_idle(&OWN);
    }

    static LOOKUPS: AtomicUsize = AtomicUsize::new(0);

    /// A slow lookup that counts its calls and finds 192.0.2.<calls>.
    fn counted_lookup(_: &str) -> std::io::Result<Vec<IpAddr>> {
        let n = LOOKUPS.fetch_add(1, Ordering::SeqCst) + 1;
        std::thread::sleep(Duration::from_millis(100));
        Ok(vec![IpAddr::from([192, 0, 2, n as u8])])
    }

    fn failing_lookup(_: &str) -> std::io::Result<Vec<IpAddr>> {
        LOOKUPS.fetch_add(1, Ordering::SeqCst);
        Err(Error::other("no such name"))
    }

    /// Looked-up addresses are kept for the TTL, then looked up again;
    /// lookups of one name at the same time share one lookup; failures are
    /// not kept.
    #[test]
    fn names_are_kept_for_their_ttl() {
        let _slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        LOOKUPS.store(0, Ordering::SeqCst);
        let ttl = Duration::from_millis(400);
        let names = Arc::new(Names { entries: Mutex::default(), ttl, lookup: counted_lookup });
        let until = || Instant::now() + Duration::from_secs(10);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let names = names.clone();
                std::thread::spawn(move || names.find("api.example", until()))
            })
            .collect();
        for t in threads {
            assert_eq!(&*t.join().unwrap().unwrap(), &[IpAddr::from([192, 0, 2, 1])]);
        }
        assert_eq!(LOOKUPS.load(Ordering::SeqCst), 1, "one lookup for eight callers");
        assert_eq!(&*names.find("api.example", until()).unwrap(), &[IpAddr::from([192, 0, 2, 1])]);
        assert_eq!(LOOKUPS.load(Ordering::SeqCst), 1, "kept");
        std::thread::sleep(ttl + Duration::from_millis(50));
        assert_eq!(&*names.find("api.example", until()).unwrap(), &[IpAddr::from([192, 0, 2, 2])]);
        assert_eq!(LOOKUPS.load(Ordering::SeqCst), 2, "looked up again after the TTL");

        // A caller whose deadline passes stops waiting; the lookup still fills the entry.
        let other = names.find("slow.example", Instant::now() + Duration::from_millis(10));
        assert_eq!(other, Err("timed out".to_string()));
        assert_eq!(&*names.find("slow.example", until()).unwrap(), &[IpAddr::from([192, 0, 2, 3])]);

        let failing = Arc::new(Names { entries: Mutex::default(), ttl, lookup: failing_lookup });
        LOOKUPS.store(0, Ordering::SeqCst);
        for _ in 0..2 {
            assert_eq!(failing.find("nowhere.example", until()), Err("cannot resolve the host name".to_string()));
        }
        assert_eq!(LOOKUPS.load(Ordering::SeqCst), 2, "a failure is looked up again");
        wait_until_idle(&PENDING);
    }
}
