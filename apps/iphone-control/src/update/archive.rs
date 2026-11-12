//! The release metadata and signature are deliberately separate: verify the exact response bytes
//! with an embedded key before parsing any download instructions.
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

#[derive(Debug, Deserialize)]
struct Manifest {
    schema: u32,
    publisher: String,
    version: String,
    target: String,
    asset: String,
    sha256: String,
    size: u64,
    unattended: bool,
    rollback_compatible: bool,
    rollback_from: Option<String>,
}

#[derive(Debug)]
pub(super) struct ManualMigration(String);

impl std::fmt::Display for ManualMigration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "release does not support rollback to installed version {}; see the release notes", self.0)
    }
}

impl std::error::Error for ManualMigration {}

/// The automatic job found a release not marked for unattended installation: a normal release that is installed
/// with `update apply`, not a failure
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

fn verify(
    raw: &[u8],
    sig: &[u8],
    keys: &str,
    publisher: &str,
    target: &str,
    version: &str,
    installed_version: &str,
) -> anyhow::Result<Manifest> {
    ensure!(
        keys.split(',').filter_map(key_bytes).any(|key| UnparsedPublicKey::new(&ED25519, key).verify(raw, sig).is_ok()),
        "invalid release signature"
    );
    let m: Manifest = serde_json::from_slice(raw)?;
    ensure!(
        m.schema == 1
            && m.publisher == publisher
            && m.target == target
            && m.version == version
            && m.asset == format!("iphone-control-v{version}-{target}.tar.gz"),
        "manifest does not match this release"
    );
    ensure!(
        (1..=MAX_ARCHIVE).contains(&m.size)
            && key_bytes(&m.sha256).is_some()
            && m.sha256.bytes().all(|b| !b.is_ascii_uppercase()),
        "invalid signed digest or size"
    );
    if !m.rollback_compatible || m.rollback_from.as_deref() != Some(installed_version) {
        return Err(ManualMigration(installed_version.into()).into());
    }
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
    let keys = option_env!("ICTL_UPDATE_PUBLIC_KEYS")
        .filter(|s| !s.is_empty())
        .context("this build has no update trust key")?;
    let base = format!("https://github.com/{}/releases/download/v{version}", receipt.publisher);
    let manifest_url = format!("{base}/iphone-control-v{version}-{}.manifest.json", receipt.target);
    let client = client()?;
    let mut raw = Vec::new();
    let mut sig = Vec::new();
    download(&client, &manifest_url, MAX_MANIFEST, &mut raw).await?;
    download(&client, &format!("{manifest_url}.sig"), 64, &mut sig).await?;
    let m = verify(&raw, &sig, keys, &receipt.publisher, &receipt.target, version, &receipt.version)?;
    if automatic && !m.unattended {
        return Err(NeedsOperator(version.to_string()).into());
    }
    download(&client, &format!("{base}/{}", m.asset), m.size, File::create_new(dir.join("archive"))?).await?;
    let candidate = unpack(&m, dir)?;
    preflight(&candidate, version).await?;
    Ok(candidate)
}

pub(super) async fn preflight(candidate: &Path, version: &str) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new(candidate);
    cmd.arg("--version").kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .context("candidate --version timed out")??;
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
    async fn verifies_publisher_signed_bytes_before_staging_an_executable() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("archive");
        make_archive(&file, tar::EntryType::Regular, &format!("{ROOT}/iphone-control"), 0o755);
        let digest = super::super::digest(&file).unwrap();
        let mut doc = json!({"schema":1,"publisher":"example/update","target":"macos-arm64","version":"0.2.1",
            "asset":format!("{ROOT}.tar.gz"),"sha256":digest,"size":file.metadata().unwrap().len(),
            "unattended":true,"rollback_compatible":true,"rollback_from":"0.2.0"});
        let key = key();
        let trusted = hex(&key);
        let check = |doc: &[u8], sig: &[u8], keys: &str| {
            verify(doc, sig, keys, "example/update", "macos-arm64", "0.2.1", "0.2.0")
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
            ("rollback_compatible", json!(false)),
            ("rollback_from", json!("0.1.9")),
            ("asset", json!("other.tar.gz")),
        ] {
            let original = doc[field].clone();
            doc[field] = value;
            let bad = serde_json::to_vec(&doc).unwrap();
            let err = check(&bad, key.sign(&bad).as_ref(), &trusted).unwrap_err();
            assert_eq!(err.is::<ManualMigration>(), matches!(field, "rollback_from" | "rollback_compatible"));
            doc[field] = original;
        }
        doc.as_object_mut().unwrap().remove("rollback_from");
        let legacy = serde_json::to_vec(&doc).unwrap();
        assert!(check(&legacy, key.sign(&legacy).as_ref(), &trusted).is_err_and(|e| e.is::<ManualMigration>()));
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
}
