//! Cache public release metadata across CLI, Viewer and scheduled checks. Hold the
//! per-publisher/platform flock through the response and atomic cache replacement.
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ictl_l10n::Lang;
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Deserialize, Serialize)]
pub(super) struct Cache {
    pub release: Option<Release>,
    pub last_success_at: Option<u64>,
    pub last_attempt_at: Option<u64>,
    pub error_code: Option<String>,
    pub retry_after: Option<u64>,
    etag: Option<String>,
    next_background_at: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Release {
    pub tag: String,
    pub published_at: Option<String>,
    pub assets: Vec<String>,
    // Absent in caches written before release notes were collected.
    pub notes: Option<ReleaseNotes>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct ReleaseNotes {
    en: Option<String>,
    zh: Option<String>,
}

impl ReleaseNotes {
    fn from_body(body: &str) -> Self {
        // Both release workflows end the changelog with a shared download/installation footer.
        let body = body
            .split_once("\n**Binaries / 二进制:**")
            .and_then(|(before, _)| before.trim_end().strip_suffix("\n---"))
            .unwrap_or(body);
        let mut en = String::new();
        let mut zh = String::new();
        let mut lang = None;
        for line in body.split_inclusive('\n') {
            match line.trim_end() {
                "## English" => lang = Some(Lang::En),
                "## 中文" => lang = Some(Lang::Zh),
                _ => match lang {
                    Some(Lang::En) => en.push_str(line),
                    Some(Lang::Zh) => zh.push_str(line),
                    None => {}
                },
            }
        }
        Self { en: excerpt(if lang.is_some() { &en } else { body }), zh: excerpt(&zh) }
    }

    pub(super) fn for_lang(&self, lang: Lang) -> Option<String> {
        match lang {
            Lang::En => self.en.as_ref().or(self.zh.as_ref()),
            Lang::Zh => self.zh.as_ref().or(self.en.as_ref()),
        }
        .cloned()
    }
}

const MAX_NOTES_BYTES: usize = 4096;

fn excerpt(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.len() <= MAX_NOTES_BYTES {
        return Some(text.to_owned());
    }
    let mut end = MAX_NOTES_BYTES - '…'.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…", text[..end].trim_end()))
}

#[derive(Deserialize)]
struct Metadata {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<String>,
    body: Option<String>,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    state: String,
    size: u64,
}

pub(super) fn stable_tag(tag: &str) -> Option<semver::Version> {
    let version = semver::Version::parse(tag.strip_prefix('v')?).ok()?;
    (version.pre.is_empty() && version.build.is_empty()).then_some(version)
}

fn parse_release(body: &[u8]) -> Option<Release> {
    let meta: Metadata = serde_json::from_slice(body).ok()?;
    if meta.draft || meta.prerelease || stable_tag(&meta.tag_name).is_none() {
        return None;
    }
    let assets = meta.assets.into_iter().filter(|a| a.state == "uploaded" && a.size > 0).map(|a| a.name).collect();
    let notes = Some(ReleaseNotes::from_body(meta.body.as_deref().unwrap_or_default()));
    Some(Release { tag: meta.tag_name, published_at: meta.published_at, assets, notes })
}

pub(super) fn read(dir: &Path) -> Result<Cache> {
    match fs::read(dir.join("release.json")) {
        Ok(data) => Ok(serde_json::from_slice(&data).unwrap_or_default()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Cache::default()),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn write_json(path: &Path, data: &impl Serialize) -> Result<()> {
    fs::create_dir_all(path.parent().context("missing update state directory")?)?;
    super::write_json_atomic(path, data)
}

fn lock(dir: &Path) -> Result<File> {
    fs::create_dir_all(dir)?;
    let f = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("check.lock"))?;
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(f);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err.into());
        }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn retry_at(headers: &header::HeaderMap, now: u64) -> u64 {
    let retry = headers.get(header::RETRY_AFTER).and_then(|h| h.to_str().ok()).and_then(|s| {
        s.parse::<u64>()
            .ok()
            .map(|secs| now.saturating_add(secs))
            .or_else(|| chrono::DateTime::parse_from_rfc2822(s).ok().and_then(|dt| dt.timestamp().try_into().ok()))
    });
    let reset = headers.get("x-ratelimit-reset").and_then(|h| h.to_str().ok()?.parse::<u64>().ok());
    retry.into_iter().chain(reset).filter(|&until| until > now).max().unwrap_or_else(|| now.saturating_add(3600))
}

enum Fetch {
    New(Release, Option<String>),
    Unchanged,
    Failed(&'static str, Option<u64>),
}

async fn fetch(client: &Client, url: &str, cache: &Cache, now: u64) -> reqwest::Result<Fetch> {
    let mut req = client
        .get(url)
        .header(header::USER_AGENT, "iphone-control-update")
        .header(header::ACCEPT, "application/vnd.github+json");
    // Fetch a complete representation once for legacy caches, even if the release itself hasn't changed.
    let etag = cache.etag.as_ref().filter(|_| cache.release.as_ref().is_some_and(|r| r.notes.is_some()));
    if let Some(etag) = etag {
        req = req.header(header::IF_NONE_MATCH, etag);
    }
    let mut res = req.send().await?;
    let status = res.status();
    let headers = res.headers();
    if status == StatusCode::NOT_MODIFIED {
        return Ok(if etag.is_some() { Fetch::Unchanged } else { Fetch::Failed("invalid_release", None) });
    }
    if status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN
            && (headers.contains_key(header::RETRY_AFTER)
                || headers.get("x-ratelimit-remaining").is_some_and(|v| v == "0")))
    {
        return Ok(Fetch::Failed("rate_limited", Some(retry_at(headers, now))));
    }
    if !status.is_success() {
        return Ok(Fetch::Failed(if status == StatusCode::NOT_FOUND { "no_release" } else { "http_error" }, None));
    }
    let etag = headers.get(header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        if chunk.len() > (2 * 1024 * 1024) - body.len() {
            return Ok(Fetch::Failed("invalid_release", None));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(match parse_release(&body) {
        Some(release) => Fetch::New(release, etag),
        None => Fetch::Failed("invalid_release", None),
    })
}

pub(super) fn client() -> Result<Client> {
    Ok(Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(12))
        .build()?)
}

pub(super) async fn check(dir: &Path, client: &Client, url: &str, background: bool) -> Result<Cache> {
    let lock_dir = dir.to_owned();
    let _lock = tokio::task::spawn_blocking(move || lock(&lock_dir)).await??;
    let mut cache = read(dir)?;
    let started = now();
    if cache.retry_after.is_some_and(|at| started < at)
        || cache.last_attempt_at.is_some_and(|at| started < at.saturating_add(60))
        || (background && cache.next_background_at.is_some_and(|at| started < at))
    {
        return Ok(cache);
    }
    let result = fetch(client, url, &cache, started).await.unwrap_or(Fetch::Failed("network", None));
    let finished = now();
    cache.last_attempt_at = Some(started);
    cache.error_code = None;
    cache.retry_after = None;
    match result {
        Fetch::New(release, etag) => {
            cache.release = Some(release);
            cache.etag = etag;
            cache.last_success_at = Some(finished);
        }
        Fetch::Unchanged => cache.last_success_at = Some(finished),
        Fetch::Failed(code, retry_after) => {
            cache.error_code = Some(code.into());
            cache.retry_after = retry_after;
        }
    }
    // Recover after a transient outage. On success spread the daily load across a 30 minute window.
    let jitter = (uuid::Uuid::new_v4().as_u128() % 1800) as u64;
    cache.next_background_at = Some(
        cache.retry_after.unwrap_or_else(|| finished + if cache.error_code.is_some() { 3600 } else { 86400 + jitter }),
    );
    write_json(&dir.join("release.json"), &cache)?;
    Ok(cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const RELEASE: &str = r#"{"tag_name":"v1.2.3","html_url":"https://other.invalid/tag","draft":false,"prerelease":false,"published_at":"2026-11-01T01:02:03Z","assets":[{"name":"complete","state":"uploaded","size":10},{"name":"partial","state":"open","size":0}]}"#;

    fn response(status: &str, headers: &str, body: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len())
    }

    async fn server(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}/latest", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                }
                requests.push(String::from_utf8(request).unwrap().to_lowercase());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (url, task)
    }

    fn local_client() -> Client {
        // Test servers are loopback only, independent of the developer's proxy environment.
        let _ = rustls::crypto::ring::default_provider().install_default();
        Client::builder().no_proxy().timeout(Duration::from_secs(3)).build().unwrap()
    }

    fn make_due(dir: &Path) {
        let mut cache = read(dir).unwrap();
        cache.last_attempt_at = Some(1);
        cache.next_background_at = Some(2);
        write_json(&dir.join("release.json"), &cache).unwrap();
    }

    #[test]
    fn release_notes_select_changelog_without_shared_install_footer() {
        let body = "## English\r\n\r\n### Added\r\n- Faster video\r\n\r\n## 中文\r\n\r\n### 新增\r\n- 画面更流畅\r\n\r\n---\r\n\r\n**Binaries / 二进制:** packages\r\n**Installer:** command";
        let notes = ReleaseNotes::from_body(body);
        assert_eq!(notes.for_lang(Lang::En).as_deref(), Some("### Added\r\n- Faster video"));
        assert_eq!(notes.for_lang(Lang::Zh).as_deref(), Some("### 新增\r\n- 画面更流畅"));
        let notes = ReleaseNotes::from_body("## English\n## What's new\n- New video\n## 中文\n## 更新内容\n- 新画面");
        assert_eq!(notes.for_lang(Lang::En).as_deref(), Some("## What's new\n- New video"));
        assert_eq!(notes.for_lang(Lang::Zh).as_deref(), Some("## 更新内容\n- 新画面"));
    }

    #[test]
    fn release_notes_fall_back_to_available_text_and_bound_each_language_separately() {
        for (body, expected) in [
            ("## 中文\n修复", Some("修复")),
            ("## 中文\n\n## English\nFixed", Some("Fixed")),
            (" A single paragraph.\n", Some("A single paragraph.")),
            ("## English\n\n## 中文\n", None),
            (" \r\n", None),
        ] {
            for lang in [Lang::En, Lang::Zh] {
                assert_eq!(ReleaseNotes::from_body(body).for_lang(lang).as_deref(), expected);
            }
        }
        let body = format!("## English\n{}\n## 中文\n修复", "🎉".repeat(MAX_NOTES_BYTES));
        let notes = ReleaseNotes::from_body(&body);
        let en = notes.for_lang(Lang::En).unwrap();
        assert!(en.ends_with('…') && en.len() <= MAX_NOTES_BYTES);
        assert_eq!(notes.for_lang(Lang::Zh).as_deref(), Some("修复"));
    }

    #[tokio::test]
    async fn legacy_cache_is_backfilled_on_next_due_check_then_keeps_its_etag() {
        let dir = tempfile::tempdir().unwrap();
        let old = serde_json::json!({"release": {"tag": "v1.2.3", "assets": ["prior"]}, "etag": "\"tag-1\""});
        write_json(&dir.path().join("release.json"), &old).unwrap();
        let legacy = read(dir.path()).unwrap().release.unwrap();
        assert_eq!(legacy.assets, ["prior"]);
        assert!(legacy.notes.is_none());

        let mut release: serde_json::Value = serde_json::from_str(RELEASE).unwrap();
        release["body"] = "## English\nNew video\n## 中文\n新画面".into();
        let (url, requests) = server(vec![
            response("200 OK", "ETag: \"tag-1\"\r\n", &release.to_string()),
            response("304 Not Modified", "", ""),
        ])
        .await;
        let http = local_client();
        check(dir.path(), &http, &url, false).await.unwrap();
        make_due(dir.path());
        let fresh = check(dir.path(), &http, &url, false).await.unwrap();
        let notes = fresh.release.unwrap().notes.unwrap();
        assert_eq!(notes.for_lang(Lang::Zh).as_deref(), Some("新画面"));
        assert_eq!(notes.for_lang(Lang::En).as_deref(), Some("New video"));
        let requests = requests.await.unwrap();
        assert!(!requests[0].contains("if-none-match:"));
        assert!(requests[1].contains("if-none-match: \"tag-1\""));
    }

    #[test]
    fn release_requires_stable_semver_and_uploaded_assets() {
        for tag in ["v01.2.3", "v1.2", "v1.2.3-rc.1", "v1.2.3+local", "v1.2.3/another", "v1.2.3 ", "1.2.3"] {
            assert!(parse_release(RELEASE.replace("v1.2.3", tag).as_bytes()).is_none(), "{tag}");
        }
        for flag in ["draft", "prerelease"] {
            assert!(
                parse_release(RELEASE.replace(&format!(r#""{flag}":false"#), &format!(r#""{flag}":true"#)).as_bytes())
                    .is_none()
            );
        }
        assert_eq!(parse_release(RELEASE.as_bytes()).unwrap().assets, ["complete"]);
    }

    #[tokio::test]
    async fn concurrent_checks_merge_and_304_preserves_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let (url, requests) =
            server(vec![response("200 OK", "ETag: \"tag-1\"\r\n", RELEASE), response("304 Not Modified", "", "")])
                .await;
        let http = local_client();
        let (first, second) =
            tokio::join!(check(dir.path(), &http, &url, false), check(dir.path(), &http, &url, false));
        assert_eq!(first.unwrap().release.unwrap().tag, "v1.2.3");
        assert_eq!(second.unwrap().release.unwrap().tag, "v1.2.3");
        let mut cache = read(dir.path()).unwrap();
        cache.last_success_at = Some(1);
        cache.last_attempt_at = Some(1);
        write_json(&dir.path().join("release.json"), &cache).unwrap();
        // Background checks respect the daily interval even when manual refresh is due.
        assert_eq!(check(dir.path(), &http, &url, true).await.unwrap().last_success_at, Some(1));
        let fresh = check(dir.path(), &http, &url, false).await.unwrap();
        assert_eq!(fresh.release.unwrap().assets, ["complete"]);
        assert!(fresh.last_success_at.unwrap() > 1);
        assert!(fresh.error_code.is_none());
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("if-none-match: \"tag-1\""));
        assert!(!requests[0].contains("authorization:"));
    }

    #[tokio::test]
    async fn rate_limit_and_network_failure_keep_last_success() {
        let dir = tempfile::tempdir().unwrap();
        let (url, requests) = server(vec![
            response("200 OK", "", RELEASE),
            response("403 Forbidden", "x-ratelimit-remaining: 0\r\nretry-after: 300\r\n", ""),
        ])
        .await;
        let http = local_client();
        let first = check(dir.path(), &http, &url, false).await.unwrap();
        make_due(dir.path());
        let throttled = check(dir.path(), &http, &url, false).await.unwrap();
        assert_eq!(throttled.error_code.as_deref(), Some("rate_limited"));
        assert!(throttled.retry_after.unwrap() >= throttled.last_attempt_at.unwrap() + 300);
        assert_eq!(throttled.last_success_at, first.last_success_at);
        make_due(dir.path());
        let deferred = check(dir.path(), &http, &url, false).await.unwrap();
        assert_eq!(deferred.last_attempt_at, Some(1));
        requests.await.unwrap(); // Drop listener; the following check cannot connect.
        let mut cache = read(dir.path()).unwrap();
        cache.retry_after = None;
        write_json(&dir.path().join("release.json"), &cache).unwrap();
        let offline = check(dir.path(), &http, &url, false).await.unwrap();
        assert_eq!(offline.error_code.as_deref(), Some("network"));
        assert_eq!(offline.release.unwrap().tag, "v1.2.3");
        assert_eq!(offline.last_success_at, first.last_success_at);
        let blank = tempfile::tempdir().unwrap();
        let offline = check(blank.path(), &http, &url, false).await.unwrap();
        assert!(offline.release.is_none() && offline.last_success_at.is_none());
        assert_eq!(offline.error_code.as_deref(), Some("network"));
    }

    #[test]
    fn reset_header_and_retry_http_date() {
        let mut headers = header::HeaderMap::new();
        headers.insert("x-ratelimit-reset", header::HeaderValue::from_static("1234"));
        assert_eq!(retry_at(&headers, 1000), 1234);
        headers.insert(header::RETRY_AFTER, header::HeaderValue::from_static("Thu, 01 Jan 1970 01:00:00 GMT"));
        assert_eq!(retry_at(&headers, 1000), 3600);
    }
}
