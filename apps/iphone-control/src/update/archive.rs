//! The release metadata and signature are deliberately separate: verify the exact response bytes
//! with an embedded key before parsing any download instructions. The same signed manifest covers the managed
//! archive (`iphone-control-vX.Y.Z-<target>.tar.gz`) and, on Linux, the Debian package
//! (`iphone-control_X.Y.Z_<arch>.deb`), each in its own `<asset>.manifest.json` beside it.
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, ensure};
use reqwest::{Client, Url};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::Deserialize;

const MAX_ARCHIVE: u64 = 128 << 20;
const MAX_EXPANDED: u64 = 256 << 20;
const MAX_MANIFEST: u64 = 16 << 10;

/// A signed release manifest. Manifests for 0.2.2's updater also carry `rollback_compatible` and `rollback_from`,
/// which this version ignores (see `packaging/release.py`)
#[derive(Debug, Deserialize)]
pub(super) struct Manifest {
    schema: u32,
    publisher: String,
    version: String,
    target: String,
    pub asset: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default)]
    unattended: bool,
}

/// The automatic job found a release not marked for unattended installation: a normal release that is installed
/// with `iphone-control update`, not a failure
#[derive(Debug)]
pub(super) struct NeedsOperator(pub String);

impl std::fmt::Display for NeedsOperator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is not marked for automatic installation; install it with: iphone-control update", self.0)
    }
}

impl std::error::Error for NeedsOperator {}

fn trusted(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some("github.com" | "release-assets.githubusercontent.com" | "objects.githubusercontent.com")
        )
}

pub(super) fn client() -> anyhow::Result<Client> {
    Ok(Client::builder()
        .user_agent("iphone-control-updater")
        .https_only(true)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 5 && trusted(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("untrusted release redirect")
            }
        }))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(180))
        .build()?)
}

pub(super) async fn download(client: &Client, url: &str, max: u64, mut out: impl Write) -> anyhow::Result<u64> {
    let mut response =
        client.get(url).send().await.and_then(|r| r.error_for_status()).map_err(reqwest::Error::without_url)?;
    ensure!(response.content_length().is_none_or(|s| s <= max), "release asset exceeds size limit");
    let mut size = 0;
    while let Some(chunk) = response.chunk().await.map_err(reqwest::Error::without_url)? {
        size += chunk.len() as u64;
        ensure!(size <= max, "release asset exceeds size limit");
        out.write_all(&chunk)?;
    }
    Ok(size)
}

fn key_bytes(key: &str) -> Option<[u8; 32]> {
    if key.len() != 64 || !key.as_bytes().iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut decoded = [0; 32];
    for (i, byte) in decoded.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&key[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(decoded)
}

/// The release signing keys this build trusts (`ICTL_UPDATE_PUBLIC_KEYS`)
pub(super) fn keys() -> anyhow::Result<&'static str> {
    option_env!("ICTL_UPDATE_PUBLIC_KEYS").filter(|s| !s.is_empty()).context("this build has no update trust key")
}

/// Downloads `<asset>.manifest.json` and its signature from release `version` of `publisher` and checks both
/// against the trusted keys and this exact asset
pub(super) async fn signed_manifest(
    client: &Client,
    publisher: &str,
    target: &str,
    version: &str,
    asset: &str,
) -> anyhow::Result<Manifest> {
    let url = format!("https://github.com/{publisher}/releases/download/v{version}/{asset}.manifest.json");
    let mut raw = Vec::new();
    let mut sig = Vec::new();
    download(client, &url, MAX_MANIFEST, &mut raw).await?;
    download(client, &format!("{url}.sig"), 64, &mut sig).await?;
    verify(&raw, &sig, keys()?, publisher, target, version, asset)
}

/// The tar archive's name for a release and target
pub(super) fn archive_asset(version: &str, target: &str) -> String {
    format!("iphone-control-v{version}-{target}.tar.gz")
}

fn verify(
    raw: &[u8],
    sig: &[u8],
    keys: &str,
    publisher: &str,
    target: &str,
    version: &str,
    asset: &str,
) -> anyhow::Result<Manifest> {
    ensure!(
        keys.split(',').filter_map(key_bytes).any(|key| UnparsedPublicKey::new(&ED25519, key).verify(raw, sig).is_ok()),
        "invalid release signature"
    );
    let m: Manifest = serde_json::from_slice(raw)?;
    ensure!(
        m.schema == 1 && m.publisher == publisher && m.target == target && m.version == version && m.asset == asset,
        "manifest does not match this release"
    );
    ensure!(
        (1..=MAX_ARCHIVE).contains(&m.size)
            && key_bytes(&m.sha256).is_some()
            && m.sha256.bytes().all(|b| !b.is_ascii_uppercase()),
        "invalid signed digest or size"
    );
    Ok(m)
}

/// The staging directory is private and on the same filesystem as the destination. The archive
/// owns no paths on disk: copy only the expected regular executable to our chosen filename.
fn extract(archive: &Path, root: &str, candidate: &Path) -> anyhow::Result<()> {
    let gzip = flate2::read::MultiGzDecoder::new(File::open(archive)?);
    let mut tar = tar::Archive::new(gzip.take(MAX_EXPANDED + 1));
    let mut found = false;
    for (n, item) in tar.entries()?.enumerate() {
        ensure!(n < 32, "too many archive entries");
        let mut item = item?;
        let name = item.path()?;
        let kind = item.header().entry_type();
        if name == Path::new(root) && kind.is_dir() {
            continue;
        }
        ensure!(name.parent() == Some(Path::new(root)) && kind.is_file(), "unexpected path or link in release archive");
        if name.file_name().is_some_and(|n| n == "iphone-control") {
            ensure!(
                !found && (1..=MAX_EXPANDED).contains(&item.size()) && item.header().mode()? & 0o111 != 0,
                "invalid executable entry"
            );
            let mut out = File::create_new(candidate)?;
            ensure!(std::io::copy(&mut item, &mut out)? == item.size(), "truncated executable");
            out.set_permissions(std::fs::Permissions::from_mode(0o755))?;
            out.sync_all()?;
            found = true;
        }
    }
    let mut decoder = tar.into_inner();
    std::io::copy(&mut decoder, &mut std::io::sink())?;
    ensure!(decoder.limit() > 0 && found, "invalid expanded archive size or missing executable");
    Ok(())
}

fn unpack(m: &Manifest, stage: &Path) -> anyhow::Result<PathBuf> {
    let archive = stage.join("archive");
    ensure!(
        archive.metadata()?.len() == m.size && super::digest(&archive)? == m.sha256,
        "release archive digest or size mismatch"
    );
    let candidate = stage.join("candidate");
    extract(&archive, m.asset.trim_end_matches(".tar.gz"), &candidate)?;
    Ok(candidate)
}

pub(super) async fn stage(
    dir: &Path,
    receipt: &super::Receipt,
    version: &str,
    automatic: bool,
) -> anyhow::Result<PathBuf> {
    let client = client()?;
    let asset = archive_asset(version, &receipt.target);
    let m = signed_manifest(&client, &receipt.publisher, &receipt.target, version, &asset).await?;
    let base = format!("https://github.com/{}/releases/download/v{version}", receipt.publisher);
    if automatic && !m.unattended {
        return Err(NeedsOperator(version.to_string()).into());
    }
    download(&client, &format!("{base}/{}", m.asset), m.size, File::create_new(dir.join("archive"))?).await?;
    let candidate = unpack(&m, dir)?;
    preflight(&candidate, version).await?;
    Ok(candidate)
}

pub(super) async fn preflight(candidate: &Path, version: &str) -> anyhow::Result<()> {
    // "text file busy" (ETXTBSY): another thread forked while the candidate was still open for writing, and the
    // child's copy of that descriptor lives until it execs. It is gone in a moment, so try again
    let mut busy = 0;
    let out = loop {
        let mut cmd = tokio::process::Command::new(candidate);
        cmd.arg("--version").kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(10), cmd.output())
            .await
            .context("candidate --version timed out")?
        {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && busy < 20 => {
                busy += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            r => break r?,
        }
    };
    ensure!(
        out.status.success() && std::str::from_utf8(&out.stdout)?.trim() == format!("iphone-control {version}"),
        "unexpected candidate --version"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;

    const ROOT: &str = "iphone-control-v0.2.1-macos-arm64";

    fn make_archive(path: &Path, kind: tar::EntryType, name: &str, mode: u32) {
        let zip = flate2::write::GzEncoder::new(File::create(path).unwrap(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(zip);
        let mut header = tar::Header::new_gnu();
        let content: &[u8] = b"#!/bin/sh\necho iphone-control 0.2.1\n";
        header.set_size(content.len() as u64);
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_cksum();
        tar.append_data(&mut header, name, content).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }

    fn key() -> Ed25519KeyPair {
        let der = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(der.as_ref()).unwrap()
    }

    fn hex(k: &Ed25519KeyPair) -> String {
        k.public_key().as_ref().iter().map(|v| format!("{v:02x}")).collect()
    }

    #[tokio::test]
    async fn the_candidate_runs_once_it_is_no_longer_open_for_writing() {
        // the race behind "text file busy": the candidate is still open for writing somewhere when it is run
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("iphone-control");
        std::fs::write(&file, "#!/bin/sh\necho iphone-control 0.2.1\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        let open = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        let released = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(open);
        });
        preflight(&file, "0.2.1").await.unwrap();
        released.await.unwrap();
    }

    #[tokio::test]
    async fn verifies_publisher_signed_bytes_before_staging_an_executable() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("archive");
        make_archive(&file, tar::EntryType::Regular, &format!("{ROOT}/iphone-control"), 0o755);
        let digest = super::super::digest(&file).unwrap();
        let mut doc = json!({"schema":1,"publisher":"example/update","target":"macos-arm64","version":"0.2.1",
            "asset":format!("{ROOT}.tar.gz"),"sha256":digest,"size":file.metadata().unwrap().len(),
            "unattended":true});
        let key = key();
        let trusted = hex(&key);
        let asset = archive_asset("0.2.1", "macos-arm64");
        let check = |doc: &[u8], sig: &[u8], keys: &str| {
            verify(doc, sig, keys, "example/update", "macos-arm64", "0.2.1", &asset)
        };
        let bytes = serde_json::to_vec(&doc).unwrap();
        let sig = key.sign(&bytes);
        let m = check(&bytes, sig.as_ref(), &trusted).unwrap();
        let next = unpack(&m, root.path()).unwrap();
        preflight(&next, "0.2.1").await.unwrap();
        assert!(preflight(&next, "0.2.2").await.is_err());
        let wrong_key = "00".repeat(32);
        assert!(check(&bytes, sig.as_ref(), &wrong_key).is_err());
        let mut edited = bytes.clone();
        edited.push(b' ');
        assert!(check(&edited, sig.as_ref(), &trusted).is_err());
        for (field, value) in [
            ("target", json!("linux-aarch64")),
            ("size", json!(MAX_ARCHIVE + 1)),
            ("publisher", json!("elsewhere/other")),
            ("version", json!("0.2.2")),
            ("asset", json!("other.tar.gz")),
            ("asset", json!("iphone-control_0.2.1_arm64.deb")),
        ] {
            let original = doc[field].clone();
            doc[field] = value;
            let bad = serde_json::to_vec(&doc).unwrap();
            assert!(check(&bad, key.sign(&bad).as_ref(), &trusted).is_err(), "{field}");
            doc[field] = original;
        }
        // a manifest written for 0.2.2's updater as well: its rollback fields don't matter any more, from any
        // installed version
        for (compatible, from) in [(json!(true), json!("0.2.0")), (json!(false), json!(null))] {
            doc["rollback_compatible"] = compatible;
            doc["rollback_from"] = from;
            let legacy = serde_json::to_vec(&doc).unwrap();
            check(&legacy, key.sign(&legacy).as_ref(), &trusted).unwrap();
        }
        std::fs::write(file, vec![0; m.size as usize]).unwrap();
        assert!(unpack(&m, root.path()).is_err());
    }

    #[test]
    fn rejects_links_wrong_directory_and_non_executable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("archive");
        let candidate = dir.path().join("candidate");
        for (kind, path, mode) in [
            (tar::EntryType::Symlink, format!("{ROOT}/iphone-control"), 0o755),
            (tar::EntryType::Link, format!("{ROOT}/iphone-control"), 0o755),
            (tar::EntryType::Regular, "different/iphone-control".into(), 0o755),
            (tar::EntryType::Regular, format!("{ROOT}/iphone-control"), 0o644),
        ] {
            make_archive(&file, kind, &path, mode);
            assert!(extract(&file, ROOT, &candidate).is_err());
            assert!(!candidate.exists());
        }
    }

    #[test]
    fn redirect_trust_is_limited_to_https_release_hosts() {
        for url in ["https://release-assets.githubusercontent.com/item", "https://github.com/a/b"] {
            assert!(trusted(&Url::parse(url).unwrap()));
        }
        for url in ["http://github.com/a", "https://github.com.example.com/a", "https://other@github.com:444/a"] {
            assert!(!trusted(&Url::parse(url).unwrap()));
        }
    }

    /// The Debian package's manifest is bound to the .deb's name: a tar manifest of the same release or the other
    /// architecture's doesn't pass for it
    #[test]
    fn deb_manifest_is_bound_to_its_package() {
        let key = key();
        let trusted = hex(&key);
        let deb = "iphone-control_0.2.3_amd64.deb";
        let doc = json!({"schema":1,"publisher":"example/update","target":"linux-x86_64","version":"0.2.3",
            "asset":deb,"sha256":"ab".repeat(32),"size":4096,"unattended":false});
        let bytes = serde_json::to_vec(&doc).unwrap();
        let sig = key.sign(&bytes);
        let m = verify(&bytes, sig.as_ref(), &trusted, "example/update", "linux-x86_64", "0.2.3", deb).unwrap();
        assert_eq!((m.asset.as_str(), m.size), (deb, 4096));
        for asset in [archive_asset("0.2.3", "linux-x86_64"), "iphone-control_0.2.3_arm64.deb".into()] {
            assert!(verify(&bytes, sig.as_ref(), &trusted, "example/update", "linux-x86_64", "0.2.3", &asset).is_err());
        }
    }
}
