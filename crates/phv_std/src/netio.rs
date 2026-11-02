//! Connections for the network clients (`std.net`): TCP with an optional TLS
//! layer whose certificates are checked against the public CAs.

use crate::netdb::{is_local_host, tls_init};
use phv_vm::Vm;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

pub(crate) enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Stream {
    pub(crate) fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(t) => t,
            Stream::Tls(t) => &t.sock,
        }
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

/// Connects to `host:port` before `until`, then starts TLS when `tls` is set.
/// Reads and writes on the stream also stop at `until`.
pub(crate) fn dial(host: &str, port: u16, tls: bool, until: Instant) -> Result<Stream, String> {
    let left = || until.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<_> = (bare, port).to_socket_addrs().map_err(|e| format!("cannot resolve {host}: {e}"))?.collect();
    let mut last = format!("cannot resolve {host}");
    let mut tcp = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, left()) {
            Ok(t) => {
                tcp = Some(t);
                break;
            }
            Err(e) => last = e.to_string(),
        }
    }
    let tcp = tcp.ok_or(last)?;
    let _ = tcp.set_nodelay(true);
    tcp.set_read_timeout(Some(left())).map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(left())).map_err(|e| e.to_string())?;
    if !tls {
        return Ok(Stream::Plain(tcp));
    }
    tls_init();
    let name = rustls::pki_types::ServerName::try_from(bare.to_string()).map_err(|_| "invalid host name")?;
    let session = rustls::ClientConnection::new(TLS.clone(), name).map_err(|e| e.to_string())?;
    Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(session, tcp))))
}
