//! HTTPS: `[server] tls_cert` and `tls_key` (PEM files). The certificate is
//! offered with ALPN for HTTP/2 and HTTP/1.1, and read again when the files
//! change (a renewed certificate is used without a restart).

use phv_driver::config::Tls;
use rustls::crypto::CryptoProvider;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio_rustls::TlsAcceptor;

/// The files are looked at again at most this often.
const RECHECK: Duration = Duration::from_secs(60);
/// A certificate chain or key file larger than this is refused.
const MAX_FILE: u64 = 1 << 20;

/// The modification times of the two files.
type Stamp = (Option<SystemTime>, Option<SystemTime>);

/// Where `path` leads (links followed), unless that is on another machine:
/// opening a file on a network share makes Windows contact its host with the
/// user's credentials (found by the 1.1.1 review: a link to a share, or a
/// network drive, passed the check of phv.toml).
fn local_file(what: &str, path: &Path) -> Result<PathBuf, String> {
    let remote = || format!("the TLS {what} {} is on another machine; give a file on this machine", path.display());
    if phv_std::safefs::on_another_machine(path) {
        return Err(remote());
    }
    let real = path.canonicalize().map_err(|e| format!("cannot read the TLS {what} {}: {e}", path.display()))?;
    if phv_std::safefs::on_another_machine(&real) {
        return Err(remote());
    }
    Ok(real)
}

/// Reads one PEM file. Messages name the file, never its contents.
fn read_pem(what: &str, path: &Path) -> Result<Vec<u8>, String> {
    let real = local_file(what, path)?;
    let meta = std::fs::metadata(&real).map_err(|e| format!("cannot read the TLS {what} {}: {e}", path.display()))?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return Err(format!("the TLS {what} {} is not a file of at most 1 MB", path.display()));
    }
    std::fs::read(&real).map_err(|e| format!("cannot read the TLS {what} {}: {e}", path.display()))
}

fn load_key(provider: &CryptoProvider, tls: &Tls) -> Result<Arc<CertifiedKey>, String> {
    let cert_pem = read_pem("certificate", &tls.cert)?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|_| format!("{} is not a PEM certificate chain", tls.cert.display()))?;
    if certs.is_empty() {
        return Err(format!("{} holds no certificate", tls.cert.display()));
    }
    let key_pem = read_pem("key", &tls.key)?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|_| format!("{} is not a PEM private key", tls.key.display()))?;
    let signing = provider
        .key_provider
        .load_private_key(key)
        .map_err(|_| format!("the private key in {} cannot be used", tls.key.display()))?;
    let ck = CertifiedKey::new(certs, signing);
    ck.keys_match().map_err(|_| {
        format!("the private key in {} does not belong to the certificate in {}", tls.key.display(), tls.cert.display())
    })?;
    Ok(Arc::new(ck))
}

fn stamp(tls: &Tls) -> Stamp {
    // Looked at only where `read_pem` would read.
    let m = |p: &PathBuf| local_file("file", p).ok().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    (m(&tls.cert), m(&tls.key))
}

/// Hands out the current certificate, reloading it when its files change.
struct Reloading {
    tls: Tls,
    provider: Arc<CryptoProvider>,
    state: Mutex<(Instant, Stamp, Arc<CertifiedKey>)>,
}

impl std::fmt::Debug for Reloading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reloading").field("cert", &self.tls.cert).finish()
    }
}

impl ResolvesServerCert for Reloading {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

impl Reloading {
    /// The certificate to offer, read again when its files changed.
    fn current(&self) -> Arc<CertifiedKey> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.0.elapsed() >= RECHECK {
            st.0 = Instant::now();
            let now = stamp(&self.tls);
            if now != st.1 {
                match load_key(&self.provider, &self.tls) {
                    Ok(k) => {
                        eprintln!("TLS: loaded the new certificate from {}", self.tls.cert.display());
                        st.1 = now;
                        st.2 = k;
                    }
                    // Halfway through a renewal, or broken: keep the one that works.
                    Err(e) => eprintln!("TLS: keeping the current certificate: {e}"),
                }
            }
        }
        st.2.clone()
    }
}

/// Builds the acceptor for `[server] tls_cert` / `tls_key`. The files must not
/// be under `public/`, where the server would hand the key to anyone.
pub fn acceptor(tls: &Tls, public: &Path) -> Result<TlsAcceptor, String> {
    if let Ok(public) = public.canonicalize() {
        for (what, name, p) in [("tls_cert", "certificate", &tls.cert), ("tls_key", "key", &tls.key)] {
            // Looked at for another machine before it is resolved (`local_file`).
            if local_file(name, p).is_ok_and(|p| p.starts_with(&public)) {
                return Err(format!("[server].{what}: {} is inside public/", p.display()));
            }
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = load_key(&provider, tls)?;
    let resolver = Reloading {
        tls: tls.clone(),
        provider: provider.clone(),
        state: Mutex::new((Instant::now(), stamp(tls), key)),
    };
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_pair(tls: &Tls) -> Vec<u8> {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(&tls.cert, ck.cert.pem()).unwrap();
        std::fs::write(&tls.key, ck.key_pair.serialize_pem()).unwrap();
        ck.cert.der().to_vec()
    }

    /// A renewed certificate is used once its files change; a broken one is
    /// not, and the last good one stays.
    #[test]
    fn a_renewed_certificate_is_loaded() {
        let dir = std::env::temp_dir().join(format!("phv-tls-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tls = Tls { cert: dir.join("cert.pem"), key: dir.join("key.pem") };
        let first = write_pair(&tls);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let r = Reloading {
            tls: tls.clone(),
            provider: provider.clone(),
            state: Mutex::new((Instant::now(), stamp(&tls), load_key(&provider, &tls).unwrap())),
        };
        let der = |r: &Reloading| r.current().cert[0].to_vec();
        assert_eq!(der(&r), first);

        // Renewed, but not looked at again before RECHECK has passed.
        std::thread::sleep(Duration::from_millis(20));
        let second = write_pair(&tls);
        let set_mtime = |p: &Path, secs: u64| {
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
        };
        set_mtime(&tls.cert, 1_000);
        assert_eq!(der(&r), first);
        r.state.lock().unwrap().0 = Instant::now() - RECHECK;
        assert_eq!(der(&r), second);

        // Halfway through a renewal (a key that does not match): kept.
        let other = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(&tls.key, other.key_pair.serialize_pem()).unwrap();
        set_mtime(&tls.key, 2_000);
        r.state.lock().unwrap().0 = Instant::now() - RECHECK;
        assert_eq!(der(&r), second);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
