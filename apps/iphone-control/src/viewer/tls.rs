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
//! (DHCP gave the machine another one) makes a new certificate that also covers the old ones, with the same key.
//!
//! The certificate made here is every instance's (docs/dev/multi-instance-plan.md 4.7): one file, `bundle.pem`, holds
//! the certificate, its key and the names it covers, 0600 from the moment it is made, replaced whole under
//! `viewer-tls.lock`; a viewer reading it gets one whole set, never one round's key with another's certificate. Each
//! new TLS connection looks at it (time, size, inode) and takes a new one another viewer wrote; one that doesn't load
//! is left for the next connection (at most once a second), the certificate of before still served. The three files
//! of before (`cert.pem`, `key.pem`, `names.json`) are still written, under the same lock and in #218's order, for an
//! older version; this one doesn't read them, but takes them over once, when there is no bundle yet.
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
const BUNDLE: &str = "bundle.pem";
/// Beside [`DIR`], held while the certificate made here is written
const LOCK: &str = "viewer-tls.lock";
/// A bundle that didn't load is tried again no sooner than this
const RELOAD_EVERY: Duration = Duration::from_secs(1);

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
    serving: std::sync::RwLock<Serving>,
    /// The bundle followed for a new certificate (the one made here); `None`: the user's, loaded once
    bundle: Option<PathBuf>,
    /// The certificate's file (a remote client's `--ca` is a copy of it)
    pub cert: PathBuf,
    /// Made here (self-signed): browsers warn the first time
    pub made: bool,
}

/// What a [`Tls`] serves now
struct Serving {
    acceptor: tokio_rustls::TlsAcceptor,
    fingerprint: String,
    /// The bundle's (modified, length, inode) it was loaded from
    stamp: Option<(SystemTime, u64, u64)>,
    tried: std::time::Instant,
}

impl Tls {
    /// The user's certificate (PEM, may hold the chain after it) and key (PEM)
    pub fn from_files(cert: &Path, key: &Path) -> anyhow::Result<Self> {
        let c = std::fs::read(cert).with_context(|| cert.display().to_string())?;
        let k = std::fs::read(key).with_context(|| key.display().to_string())?;
        let (acceptor, fingerprint) = acceptor(&c, &k, cert)?;
        let serving = Serving { acceptor, fingerprint, stamp: None, tried: std::time::Instant::now() };
        Ok(Self { serving: std::sync::RwLock::new(serving), bundle: None, cert: cert.to_owned(), made: false })
    }

    /// The certificate made here, made (again) when there is none, it doesn't cover all of `names`, or it ends soon
    pub fn made(data_dir: &Path, names: &[String]) -> anyhow::Result<Self> {
        let dir = data_dir.join(DIR);
        let pem = ensure_made(data_dir, names, now())?;
        let bundle = dir.join(BUNDLE);
        let (acceptor, fingerprint) = acceptor(&pem, &pem, &bundle)?;
        let serving = Serving { acceptor, fingerprint, stamp: stamp(&bundle), tried: std::time::Instant::now() };
        Ok(Self {
            serving: std::sync::RwLock::new(serving),
            bundle: Some(bundle),
            cert: dir.join("cert.pem"),
            made: true,
        })
    }

    /// SHA-256 of the certificate served now, as browsers show it (`AB:CD:…`)
    pub fn fingerprint(&self) -> String {
        self.serving.read().unwrap_or_else(|p| p.into_inner()).fingerprint.clone()
    }

    /// The certificate another viewer wrote since, taken; one that doesn't load is tried again later
    fn reload(&self) {
        let Some(bundle) = &self.bundle else { return };
        let now_stamp = stamp(bundle);
        {
            let s = self.serving.read().unwrap_or_else(|p| p.into_inner());
            if now_stamp == s.stamp || s.tried.elapsed() < RELOAD_EVERY {
                return;
            }
        }
        let mut s = self.serving.write().unwrap_or_else(|p| p.into_inner());
        s.tried = std::time::Instant::now();
        match std::fs::read(bundle).map_err(anyhow::Error::from).and_then(|pem| acceptor(&pem, &pem, bundle)) {
            Ok((acceptor, fingerprint)) => {
                if fingerprint != s.fingerprint {
                    tracing::info!(
                        "{}",
                        ictl_l10n::log_trf!(
                            "控制台证书换了（别的控制台写的），SHA-256 指纹 {fingerprint}",
                            "The viewer's certificate changed (written by another viewer), SHA-256 fingerprint {fingerprint}"
                        )
                    );
                }
                (s.acceptor, s.fingerprint, s.stamp) = (acceptor, fingerprint, now_stamp);
            }
            Err(e) => tracing::warn!("{}: {e}", bundle.display()),
        }
    }

    /// The connection as it comes: TLS when it starts with a handshake, plain HTTP otherwise
    pub async fn accept(&self, tcp: TcpStream) -> io::Result<Conn> {
        let timed_out = |_| io::Error::from(io::ErrorKind::TimedOut);
        let mut first = [0u8; 1];
        let n = tokio::time::timeout(HANDSHAKE_TIMEOUT, tcp.peek(&mut first)).await.map_err(timed_out)??;
        if n == 0 || first[0] != HANDSHAKE {
            return Ok(Conn::Plain(tcp));
        }
        self.reload();
        let acceptor = self.serving.read().unwrap_or_else(|p| p.into_inner()).acceptor.clone();
        let s = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await.map_err(timed_out)??;
        Ok(Conn::Tls(Box::new(s)))
    }
}

/// A TLS acceptor for the certificate (chain) in `cert_pem` and the key in `key_pem` (the same bundle may hold both),
/// with the certificate's fingerprint
fn acceptor(cert_pem: &[u8], key_pem: &[u8], path: &Path) -> anyhow::Result<(tokio_rustls::TlsAcceptor, String)> {
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
    Ok((tokio_rustls::TlsAcceptor::from(Arc::new(cfg)), fingerprint))
}

fn stamp(p: &Path) -> Option<(SystemTime, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some((m.modified().ok()?, m.len(), m.ino()))
}

/// The fingerprint of the certificate made here, if there is one (for `service status`)
pub fn made_fingerprint(data_dir: &Path) -> Option<String> {
    let dir = data_dir.join(DIR);
    let pem = std::fs::read(dir.join(BUNDLE)).or_else(|_| std::fs::read(dir.join("cert.pem"))).ok()?;
    let first = CertificateDer::pem_slice_iter(&pem).next()?.ok()?;
    Some(fingerprint(&first))
}

fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// What a made certificate covers: `names.json` of before, and the header of the bundle
#[derive(Serialize, Deserialize)]
struct Made {
    names: Vec<String>,
    not_after: u64,
}

/// The bundle's header line that holds [`Made`] (text outside a PEM block is left alone by readers)
const MADE_LINE: &str = "iphone-control viewer certificate: ";

/// A bundle: what it covers, its certificate (PEM) and its key (PEM)
struct Bundle {
    made: Made,
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl Bundle {
    fn render(&self) -> anyhow::Result<Vec<u8>> {
        let mut out = format!("{MADE_LINE}{}\n", serde_json::to_string(&self.made)?).into_bytes();
        out.extend_from_slice(&self.cert);
        out.extend_from_slice(&self.key);
        Ok(out)
    }

    fn parse(pem: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(pem).ok()?;
        let made = serde_json::from_str(text.lines().next()?.strip_prefix(MADE_LINE)?).ok()?;
        let section = |kind: &str| {
            let (begin, end) = (format!("-----BEGIN {kind}-----"), format!("-----END {kind}-----"));
            let start = text.find(&begin)?;
            let stop = start + text[start..].find(&end)? + end.len();
            Some(format!("{}\n", &text[start..stop]).into_bytes())
        };
        Some(Self { made, cert: section("CERTIFICATE")?, key: section("PRIVATE KEY")? })
    }

    fn covers(&self, names: &[String], now: u64) -> bool {
        names.iter().all(|n| self.made.names.contains(n)) && self.made.not_after > now + RENEW_DAYS * DAY
    }
}

/// The bundle made here (PEM: certificate and key), made now if needed: with the lock held, so two viewers starting at
/// once make one, and each reads a whole one
fn ensure_made(data_dir: &Path, names: &[String], now: u64) -> anyhow::Result<Vec<u8>> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = data_dir.join(DIR);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(data_dir.join(LOCK))?;
    // SAFETY: a valid descriptor, owned by `lock` for as long as it is held; waited for: a certificate is made in
    // milliseconds
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let bundle_f = dir.join(BUNDLE);
    let current = std::fs::read(&bundle_f).ok().and_then(|b| Bundle::parse(&b)).or_else(|| legacy(&dir));
    if let Some(b) = &current
        && b.covers(names, now)
    {
        let pem = b.render()?;
        if std::fs::read(&bundle_f).ok().as_deref() != Some(&pem[..]) {
            // the older version's pair, taken over once
            write_atomic(&bundle_f, &pem, 0o600)?;
        }
        return Ok(pem);
    }
    // the names it had stay: a page opened at an old address keeps working; so does the key
    let mut all: Vec<String> = current.as_ref().map(|b| b.made.names.clone()).unwrap_or_default();
    for n in names {
        if !all.contains(n) {
            all.push(n.clone());
        }
    }
    let not_after = now + VALID_DAYS * DAY;
    let mut p = rcgen::CertificateParams::new(all.clone())?;
    let (y, m, d) = ymd(now.saturating_sub(DAY));
    p.not_before = rcgen::date_time_ymd(y, m, d);
    let (y, m, d) = ymd(not_after);
    p.not_after = rcgen::date_time_ymd(y, m, d);
    p.distinguished_name = rcgen::DistinguishedName::new();
    p.distinguished_name.push(rcgen::DnType::CommonName, "iphone-control viewer");
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    // a new certificate under the same name gets a new serial number (Firefox refuses a reused one)
    p.serial_number = Some(rcgen::SerialNumber::from(now.max(1)));
    // ECDSA P-256, the same key type rcgen 0.12 made; the one there was, kept
    let kept = current.as_ref().and_then(|b| rcgen::KeyPair::from_pem(std::str::from_utf8(&b.key).ok()?).ok());
    let key = match kept {
        Some(k) => k,
        None => rcgen::KeyPair::generate()?,
    };
    let cert = p.self_signed(&key)?;
    let b = Bundle {
        made: Made { names: all, not_after },
        cert: cert.pem().into_bytes(),
        key: key.serialize_pem().into_bytes(),
    };
    let pem = b.render()?;
    write_atomic(&bundle_f, &pem, 0o600)?;
    // the older version's three files, in #218's order: names.json is what makes that pair reused, gone first and
    // written last, so a stop halfway is made again rather than failing to load
    let meta_f = dir.join("names.json");
    match std::fs::remove_file(&meta_f) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    write_atomic(&dir.join("key.pem"), &b.key, 0o600)?;
    write_atomic(&dir.join("cert.pem"), &b.cert, 0o644)?;
    write_atomic(&meta_f, &serde_json::to_vec_pretty(&b.made)?, 0o644)?;
    Ok(pem)
}

/// The pair an older version made (three files), if whole: the certificate and key that go together, and its names
fn legacy(dir: &Path) -> Option<Bundle> {
    let made: Made = serde_json::from_slice(&std::fs::read(dir.join("names.json")).ok()?).ok()?;
    let (cert, key) = (std::fs::read(dir.join("cert.pem")).ok()?, std::fs::read(dir.join("key.pem")).ok()?);
    acceptor(&cert, &key, &dir.join("cert.pem")).ok()?;
    Some(Bundle { made, cert, key })
}

/// Writes `data` to a new temporary file beside `file` (with `mode` from the start: the key is only ever this
/// user's), syncs it and renames it into place: `file` is the old content or all of the new. The temporary file is
/// made here, never one already there (a leftover with other permissions would keep them)
fn write_atomic(file: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = file.with_file_name(format!(".{name}.{}-{n}.tmp", std::process::id()));
    let f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
    let r = (|| {
        let mut f = f;
        // the umask may have taken bits off
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
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

    /// A key written over an old one is all new and only this user's, whatever the umask, and nothing is left beside it
    #[test]
    fn a_key_replaces_the_old_one_whole_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key.pem");
        std::fs::write(&key, "old").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&key, b"new", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&key).unwrap(), "new");
        assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["key.pem"]);
    }

    #[test]
    fn the_made_certificate_is_kept_and_grows_with_new_names() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let a = Tls::made(tmp.path(), &names(&["localhost", "127.0.0.1"])).unwrap();
        assert!(a.made);
        assert_eq!(a.fingerprint().len(), 32 * 3 - 1);
        let key_a = std::fs::read(tmp.path().join(DIR).join("key.pem")).unwrap();
        let b = Tls::made(tmp.path(), &names(&["127.0.0.1"])).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint(), "covered: the same certificate");
        let c = Tls::made(tmp.path(), &names(&["localhost", "192.168.1.9"])).unwrap();
        assert_ne!(a.fingerprint(), c.fingerprint(), "a new address: a new certificate");
        assert_eq!(std::fs::read(tmp.path().join(DIR).join("key.pem")).unwrap(), key_a, "with the same key");
        let meta: Made =
            serde_json::from_slice(&std::fs::read(tmp.path().join(DIR).join("names.json")).unwrap()).unwrap();
        assert_eq!(meta.names, names(&["localhost", "127.0.0.1", "192.168.1.9"]), "keeping the old names");
        assert_eq!(made_fingerprint(tmp.path()), Some(c.fingerprint()));
        for (f, want) in [("key.pem", 0o600), ("bundle.pem", 0o600), ("cert.pem", 0o644)] {
            let mode = std::fs::metadata(tmp.path().join(DIR).join(f)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, want, "{f}");
        }
        // close to its end: made again
        let pem = ensure_made(tmp.path(), &names(&["localhost"]), meta.not_after - DAY).unwrap();
        assert_ne!(fingerprint(&CertificateDer::pem_slice_iter(&pem).next().unwrap().unwrap()), c.fingerprint());
    }

    /// The pair an older version made is taken over once (the same certificate, now in the bundle); one it left
    /// halfway (a new key beside the old certificate, names.json gone) is made again and loads; nothing temporary is
    /// left
    #[test]
    fn an_older_versions_pair_is_taken_over_or_made_again() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Tls::made(tmp.path(), &names(&["localhost"])).unwrap();
        let dir = tmp.path().join(DIR);
        std::fs::remove_file(dir.join(BUNDLE)).unwrap();
        let b = Tls::made(tmp.path(), &names(&["localhost"])).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint(), "the older version's, taken over");
        assert!(dir.join(BUNDLE).is_file());
        // as an older version leaves it, stopped after the key: names.json removed, the key another one
        std::fs::remove_file(dir.join(BUNDLE)).unwrap();
        std::fs::remove_file(dir.join("names.json")).unwrap();
        let other = rcgen::KeyPair::generate().unwrap().serialize_pem();
        std::fs::write(dir.join("key.pem"), other).unwrap();
        let c = Tls::made(tmp.path(), &names(&["localhost"])).expect("made again, and loads");
        assert_ne!(a.fingerprint(), c.fingerprint());
        let left: Vec<String> =
            std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(left.iter().all(|n| !n.ends_with(".tmp")), "{left:?}");
    }

    /// A viewer takes the certificate another one made since, at its next TLS connection; a bundle that doesn't load
    /// leaves the one served before
    #[test]
    fn a_certificate_another_viewer_made_is_taken_without_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Tls::made(tmp.path(), &names(&["localhost"])).unwrap();
        let before = a.fingerprint();
        let other = Tls::made(tmp.path(), &names(&["localhost", "nas.local"])).unwrap();
        a.reload();
        assert_eq!(a.fingerprint(), before, "not tried again within a second");
        std::thread::sleep(RELOAD_EVERY + Duration::from_millis(100));
        a.reload();
        assert_eq!(a.fingerprint(), other.fingerprint(), "taken");
        write_atomic(&tmp.path().join(DIR).join(BUNDLE), b"broken", 0o600).unwrap();
        std::thread::sleep(RELOAD_EVERY + Duration::from_millis(100));
        a.reload();
        assert_eq!(a.fingerprint(), other.fingerprint(), "kept");
    }

    #[test]
    fn the_user_s_files() {
        let tmp = tempfile::tempdir().unwrap();
        let made = Tls::made(tmp.path(), &names(&["localhost"])).unwrap();
        let dir = tmp.path().join(DIR);
        let own = Tls::from_files(&dir.join("cert.pem"), &dir.join("key.pem")).unwrap();
        assert_eq!(own.fingerprint(), made.fingerprint());
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
