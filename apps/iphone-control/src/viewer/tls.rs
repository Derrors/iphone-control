//! HTTPS for the control page opened from the local network (docs/task-control.md 7.1, item 4).
//!
//! Browsers only offer WebCodecs (and some other APIs) in a secure context: HTTPS, or `localhost`. Opened at
//! `http://<LAN address>` the page can't decode H.264 or the phone's HEVC and falls back to MJPEG, and everything,
//! taps and typed text included, crosses the network in the clear. So with `--lan` the same port also speaks TLS:
//! the first byte of a connection tells (a TLS handshake starts with 0x16, an HTTP request with a letter). Plain
//! HTTP stays for `127.0.0.1`; from the network it is redirected to HTTPS.
//!
//! The certificate is the user's (`--tls-cert`, `--tls-key`: mkcert, `tailscale cert`, an internal CA), or one made
//! here, self-signed, kept in the data directory and reused, so the browser asks once rather than every start. Its
//! SHA-256 fingerprint is printed, to check against the browser's warning the first time. A new name or address
//! (DHCP gave the machine another one) makes a new certificate that also covers the old ones.
//!
//! Not a login: anyone who can reach the page can still use it.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// Days a made certificate is valid: under Apple's 825-day limit for TLS server certificates
const VALID_DAYS: u64 = 800;
/// A made certificate this close to its end is made again
const RENEW_DAYS: u64 = 30;
const DAY: u64 = 24 * 60 * 60;
/// A TLS record starts with its content type; 22 is a handshake (the client's hello)
const HANDSHAKE: u8 = 0x16;
/// For the first byte and for the handshake: a connection that says nothing is dropped
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Under the data directory: the certificate made here
const DIR: &str = "viewer-tls";

/// `--tls`
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// HTTPS with `--lan` or with a certificate of the user's
    Auto,
    On,
    Off,
}

/// `--tls`, `--tls-cert`, `--tls-key`
#[derive(Debug, Clone)]
pub struct Setting {
    pub mode: Mode,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

impl Setting {
    /// Whether the page speaks HTTPS
    pub fn enabled(&self, lan: bool) -> bool {
        match self.mode {
            Mode::On => true,
            Mode::Off => false,
            Mode::Auto => lan || self.cert.is_some(),
        }
    }

    /// The certificate: the user's files, or the one made here for `names`
    pub fn load(&self, data_dir: &Path, names: &[String]) -> anyhow::Result<Tls> {
        match (&self.cert, &self.key) {
            (Some(c), Some(k)) => Tls::from_files(c, k),
            (None, None) => Tls::made(data_dir, names),
            _ => bail!(ictl_l10n::tr!("--tls-cert 和 --tls-key 要一起给", "--tls-cert and --tls-key go together")),
        }
    }
}

pub struct Tls {
    acceptor: tokio_rustls::TlsAcceptor,
    /// SHA-256 of the certificate, as browsers show it (`AB:CD:…`)
    pub fingerprint: String,
    /// The certificate's file
    pub cert: PathBuf,
    /// Made here (self-signed): browsers warn the first time
    pub made: bool,
}

impl Tls {
    /// The user's certificate (PEM, may hold the chain after it) and key (PEM)
    pub fn from_files(cert: &Path, key: &Path) -> anyhow::Result<Self> {
        let c = std::fs::read(cert).with_context(|| cert.display().to_string())?;
        let k = std::fs::read(key).with_context(|| key.display().to_string())?;
        Self::new(&c, &k, cert, false)
    }

    /// The certificate made here, made (again) when there is none, it doesn't cover all of `names`, or it ends soon
    pub fn made(data_dir: &Path, names: &[String]) -> anyhow::Result<Self> {
        let dir = data_dir.join(DIR);
        let (cert, key) = ensure_made(&dir, names, now())?;
        Self::new(&cert, &key, &dir.join("cert.pem"), true)
    }

    fn new(cert_pem: &[u8], key_pem: &[u8], path: &Path, made: bool) -> anyhow::Result<Self> {
        let certs = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("{}: {e:?}", path.display()))?;
        let Some(first) = certs.first() else {
            bail!(ictl_l10n::trf!("{} 里没有证书", "No certificate in {}", path.display()));
        };
        let fingerprint = fingerprint(first);
        let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| anyhow::anyhow!("key: {e:?}"))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .with_context(|| {
                ictl_l10n::trf!("证书和私钥对不上（{}）", "The certificate and key don't match ({})", path.display())
            })?;
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self { acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(cfg)), fingerprint, cert: path.to_owned(), made })
    }

    /// The connection as it comes: TLS when it starts with a handshake, plain HTTP otherwise
    pub async fn accept(&self, tcp: TcpStream) -> io::Result<Conn> {
        let timed_out = |_| io::Error::from(io::ErrorKind::TimedOut);
        let mut first = [0u8; 1];
        let n = tokio::time::timeout(HANDSHAKE_TIMEOUT, tcp.peek(&mut first)).await.map_err(timed_out)??;
        if n == 0 || first[0] != HANDSHAKE {
            return Ok(Conn::Plain(tcp));
        }
        let s = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.acceptor.accept(tcp)).await.map_err(timed_out)??;
        Ok(Conn::Tls(Box::new(s)))
    }
}

/// The fingerprint of the certificate made here, if there is one (for `service status`)
pub fn made_fingerprint(data_dir: &Path) -> Option<String> {
    let pem = std::fs::read(data_dir.join(DIR).join("cert.pem")).ok()?;
    let first = CertificateDer::pem_slice_iter(&pem).next()?.ok()?;
    Some(fingerprint(&first))
}

fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// What a made certificate covers, next to it
#[derive(Serialize, Deserialize)]
struct Made {
    names: Vec<String>,
    not_after: u64,
}

/// The made certificate and key (PEM) in `dir`, made now if needed
fn ensure_made(dir: &Path, names: &[String], now: u64) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let (cert_f, key_f, meta_f) = (dir.join("cert.pem"), dir.join("key.pem"), dir.join("names.json"));
    let meta: Option<Made> = std::fs::read(&meta_f).ok().and_then(|b| serde_json::from_slice(&b).ok());
    if let (Some(m), Ok(c), Ok(k)) = (&meta, std::fs::read(&cert_f), std::fs::read(&key_f))
        && names.iter().all(|n| m.names.contains(n))
        && m.not_after > now + RENEW_DAYS * DAY
    {
        return Ok((c, k));
    }
    // the names it had stay: a page opened at an old address keeps working
    let mut all: Vec<String> = meta.map(|m| m.names).unwrap_or_default();
    for n in names {
        if !all.contains(n) {
            all.push(n.clone());
        }
    }
    let not_after = now + VALID_DAYS * DAY;
    let mut p = rcgen::CertificateParams::new(all.clone());
    let (y, m, d) = ymd(now.saturating_sub(DAY));
    p.not_before = rcgen::date_time_ymd(y, m, d);
    let (y, m, d) = ymd(not_after);
    p.not_after = rcgen::date_time_ymd(y, m, d);
    p.distinguished_name = rcgen::DistinguishedName::new();
    p.distinguished_name.push(rcgen::DnType::CommonName, "iphone-control viewer");
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    // a new certificate under the same name gets a new serial number (Firefox refuses a reused one)
    p.serial_number = Some(rcgen::SerialNumber::from(now.max(1)));
    let cert = rcgen::Certificate::from_params(p)?;
    let (c, k) = (cert.serialize_pem()?.into_bytes(), cert.serialize_private_key_pem().into_bytes());
    write_private(dir, &key_f, &k)?;
    std::fs::write(&cert_f, &c)?;
    std::fs::write(&meta_f, serde_json::to_vec_pretty(&Made { names: all, not_after })?)?;
    Ok((c, k))
}

/// The key: only for this user
fn write_private(dir: &Path, file: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(file)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(data)
}

/// Year, month, day of a Unix time (UTC)
fn ymd(secs: u64) -> (i32, u8, u8) {
    use chrono::Datelike;
    let t = chrono::DateTime::from_timestamp(secs as i64, 0).unwrap_or_default();
    (t.year(), t.month() as u8, t.day() as u8)
}

/// A connection to the page: plain HTTP or HTTPS
pub enum Conn {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl Conn {
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
            Self::Tls(s) => s.get_ref().0,
        }
    }

    pub fn is_tls(&self) -> bool {
        matches!(self, Self::Tls(_))
    }
}

impl AsyncRead for Conn {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_made_certificate_is_kept_and_grows_with_new_names() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Tls::made(tmp.path(), &names(&["localhost", "127.0.0.1"])).unwrap();
        assert!(a.made);
        assert_eq!(a.fingerprint.len(), 32 * 3 - 1);
        let b = Tls::made(tmp.path(), &names(&["127.0.0.1"])).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint, "covered: the same certificate");
        let c = Tls::made(tmp.path(), &names(&["localhost", "192.168.1.9"])).unwrap();
        assert_ne!(a.fingerprint, c.fingerprint, "a new address: a new certificate");
        let meta: Made =
            serde_json::from_slice(&std::fs::read(tmp.path().join(DIR).join("names.json")).unwrap()).unwrap();
        assert_eq!(meta.names, names(&["localhost", "127.0.0.1", "192.168.1.9"]), "keeping the old names");
        assert_eq!(made_fingerprint(tmp.path()), Some(c.fingerprint.clone()));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(tmp.path().join(DIR).join("key.pem")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // close to its end: made again
        let (cert, _) = ensure_made(&tmp.path().join(DIR), &names(&["localhost"]), meta.not_after - DAY).unwrap();
        assert_ne!(fingerprint(&CertificateDer::pem_slice_iter(&cert).next().unwrap().unwrap()), c.fingerprint);
    }

    #[test]
    fn the_user_s_files() {
        let tmp = tempfile::tempdir().unwrap();
        let made = Tls::made(tmp.path(), &names(&["localhost"])).unwrap();
        let dir = tmp.path().join(DIR);
        let own = Tls::from_files(&dir.join("cert.pem"), &dir.join("key.pem")).unwrap();
        assert_eq!(own.fingerprint, made.fingerprint);
        assert!(!own.made);
        std::fs::write(tmp.path().join("bad.pem"), "not a certificate").unwrap();
        assert!(Tls::from_files(&tmp.path().join("bad.pem"), &dir.join("key.pem")).is_err());
        assert!(Tls::from_files(&tmp.path().join("missing.pem"), &dir.join("key.pem")).is_err());
        // a key of another certificate
        let other = tempfile::tempdir().unwrap();
        Tls::made(other.path(), &names(&["localhost"])).unwrap();
        assert!(Tls::from_files(&dir.join("cert.pem"), &other.path().join(DIR).join("key.pem")).is_err());
        let half = Setting { mode: Mode::On, cert: Some(dir.join("cert.pem")), key: None };
        assert!(half.load(tmp.path(), &[]).is_err());
    }

    #[test]
    fn when_https_is_on() {
        let s = |mode, cert: bool| Setting { mode, cert: cert.then(|| "c.pem".into()), key: None };
        assert!(s(Mode::Auto, false).enabled(true));
        assert!(!s(Mode::Auto, false).enabled(false));
        assert!(s(Mode::Auto, true).enabled(false));
        assert!(s(Mode::On, false).enabled(false));
        assert!(!s(Mode::Off, true).enabled(true));
    }

    /// One port: a TLS client gets HTTPS, a plain one plain HTTP, and the first byte isn't lost to either
    #[tokio::test]
    async fn the_same_port_speaks_both() {
        let tmp = tempfile::tempdir().unwrap();
        let tls = Arc::new(Tls::made(tmp.path(), &names(&["localhost", "127.0.0.1"])).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = {
            let tls = tls.clone();
            tokio::spawn(async move {
                for _ in 0..2 {
                    let (tcp, _) = listener.accept().await.unwrap();
                    let mut conn = tls.accept(tcp).await.unwrap();
                    let mut buf = [0u8; 14];
                    conn.read_exact(&mut buf).await.unwrap();
                    let reply = if conn.is_tls() { "secure" } else { "plain" };
                    conn.write_all(format!("{}:{reply}", String::from_utf8_lossy(&buf)).as_bytes()).await.unwrap();
                    conn.flush().await.unwrap();
                    conn.shutdown().await.unwrap();
                }
            })
        };

        let mut plain = TcpStream::connect(addr).await.unwrap();
        plain.write_all(b"GET / HTTP/1.1").await.unwrap();
        let mut got = String::new();
        plain.read_to_string(&mut got).await.unwrap();
        assert_eq!(got, "GET / HTTP/1.1:plain");

        let mut roots = rustls::RootCertStore::empty();
        let pem = std::fs::read(&tls.cert).unwrap();
        roots.add(CertificateDer::pem_slice_iter(&pem).next().unwrap().unwrap()).unwrap();
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
        let mut secure = connector.connect(name, TcpStream::connect(addr).await.unwrap()).await.unwrap();
        secure.write_all(b"GET / HTTP/1.1").await.unwrap();
        let mut got = String::new();
        secure.read_to_string(&mut got).await.unwrap();
        assert_eq!(got, "GET / HTTP/1.1:secure");
        server.await.unwrap();
    }
}
