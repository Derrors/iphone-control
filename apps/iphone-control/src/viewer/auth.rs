//! Login for the control page and `/mcp` (docs/task-control.md 7.5, 7.6).
//!
//! Every HTTP request needs it, from this machine too: a reverse proxy or tunnel on this machine (frp, nginx,
//! cloudflared, `tailscale serve`) makes every visitor's connection come from `127.0.0.1`, so the page can't tell
//! the user at their own browser from someone on the internet. The Unix socket (the CLI, `iphone-control mcp` over
//! stdio, `task run`) isn't HTTP and doesn't need it.
//!
//! Tokens have a name and a role ([`Role`]): `operate` may do everything, `view` may only look (the picture, the
//! device's state, tasks and their events). They are kept in `<data dir>/viewer-tokens.json` (0600); the first one,
//! `owner`, is made on the first start (from the single `viewer-token` of earlier versions, if there is one).
//! `iphone-control viewer token …` shows, adds, replaces and removes them; the viewer reads the file again when it
//! changes, so that takes effect at once. Who did what goes into a task's log by the token's name.
//!
//! A browser logs in once (the login page, or the link with `?token=`) and gets a session cookie: `HttpOnly`,
//! `SameSite=Lax`, `Secure` over HTTPS, 30 days. Its value is an HMAC of the token, so it shows nothing of the token
//! and ends when the token is replaced or removed. Programs send `Authorization: Bearer <token>`.
//!
//! Wrong tokens are counted for the whole page, not per address (behind a proxy every address is `127.0.0.1`): after
//! [`MAX_FAILURES`] in a minute, a wrong one is answered with 429 until the minute is over. A right token always gets
//! in: a token is 256 random bits, so nobody guesses one, and refusing right tokens too would let anyone who can reach
//! the page lock out every program (`mcp --url`, remote tasks) by sending wrong ones.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// Under the data directory: the tokens
pub const TOKENS_FILE: &str = "viewer-tokens.json";
/// Under the data directory: the single token of earlier versions, which becomes [`OWNER`]
const OLD_TOKEN_FILE: &str = "viewer-token";
/// The first token's name
pub const OWNER: &str = "owner";
/// Wrong tokens in [`WINDOW`] before wrong ones are answered with 429 until it is over
const MAX_FAILURES: usize = 20;
const WINDOW: Duration = Duration::from_secs(60);
/// How long a browser stays logged in
const SESSION_DAYS: u64 = 30;

/// What a token may do
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Everything: act on the phone, start and control tasks, `/mcp`
    Operate,
    /// Only look: the picture, the device's state, tasks and their events
    View,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Self::Operate => "operate",
            Self::View => "view",
        }
    }
}

/// A token in [`TOKENS_FILE`]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub role: Role,
    pub token: String,
    /// Unix seconds
    #[serde(default)]
    pub created: u64,
}

pub fn tokens_file(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKENS_FILE)
}

/// Runs `f` holding the lock beside the tokens file: commands changing tokens at once (and the first start making
/// them) read and write one after the other instead of one losing the other's change
fn locked<T>(data_dir: &Path, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(data_dir)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(tokens_file(data_dir).with_extension("lock"))?;
    while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        let e = io::Error::last_os_error();
        // a signal while waiting: wait again
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e.into());
        }
    }
    f()
}

/// The tokens; the first start makes [`OWNER`] (from the earlier single token if there is one)
pub fn load(data_dir: &Path) -> anyhow::Result<Vec<Entry>> {
    if let Some(v) = read(data_dir)? {
        return Ok(v);
    }
    locked(data_dir, || load_unlocked(data_dir))
}

/// [`load`] with the lock held
fn load_unlocked(data_dir: &Path) -> anyhow::Result<Vec<Entry>> {
    if let Some(v) = read(data_dir)? {
        return Ok(v);
    }
    let old = data_dir.join(OLD_TOKEN_FILE);
    let token = match std::fs::read_to_string(&old) {
        Ok(t) if valid_token(t.trim()) => t.trim().to_owned(),
        _ => new_token()?,
    };
    let v = vec![Entry { name: OWNER.into(), role: Role::Operate, token, created: now() }];
    save(data_dir, &v)?;
    let _ = std::fs::remove_file(old);
    Ok(v)
}

/// The tokens as they are on disk; `None` when there is no file yet
fn read(data_dir: &Path) -> anyhow::Result<Option<Vec<Entry>>> {
    let file = tokens_file(data_dir);
    match std::fs::read(&file) {
        Ok(b) => {
            let v: Vec<Entry> = serde_json::from_slice(&b).with_context(|| file.display().to_string())?;
            Ok(Some(v.into_iter().filter(|e| valid_token(&e.token)).collect()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(file.display().to_string())),
    }
}

fn save(data_dir: &Path, v: &[Entry]) -> anyhow::Result<()> {
    let mut json = serde_json::to_vec_pretty(v)?;
    json.push(b'\n');
    write_private(&tokens_file(data_dir), &json)?;
    Ok(())
}

/// A name the CLI and the task log can show: letters, digits, `-`, `_`, `.`, at most 40
pub fn valid_name(name: &str) -> bool {
    (1..=40).contains(&name.chars().count()) && name.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c))
}

/// A new token named `name`
pub fn add(data_dir: &Path, name: &str, role: Role) -> anyhow::Result<Entry> {
    if !valid_name(name) {
        bail!(ictl_l10n::trf!(
            "名字只能用字母、数字和 - _ .，最多 40 个字：{name}",
            "A name takes letters, digits and - _ . only, 40 at most: {name}"
        ));
    }
    locked(data_dir, || add_unlocked(data_dir, name, role))
}

fn add_unlocked(data_dir: &Path, name: &str, role: Role) -> anyhow::Result<Entry> {
    let mut v = load_unlocked(data_dir)?;
    if v.iter().any(|e| e.name == name) {
        bail!(ictl_l10n::trf!(
            "已经有叫 {name} 的令牌了；要换新的用 viewer token show {name} --new",
            "There is already a token named {name}; to replace it: viewer token show {name} --new"
        ));
    }
    let e = Entry { name: name.into(), role, token: new_token()?, created: now() };
    v.push(e.clone());
    save(data_dir, &v)?;
    Ok(e)
}

/// The token named `name`
pub fn get(data_dir: &Path, name: &str) -> anyhow::Result<Entry> {
    load(data_dir)?.into_iter().find(|e| e.name == name).ok_or_else(|| no_such(name))
}

/// A new token in place of `name`'s; its browsers have to log in again
pub fn renew(data_dir: &Path, name: &str) -> anyhow::Result<Entry> {
    locked(data_dir, || renew_unlocked(data_dir, name))
}

fn renew_unlocked(data_dir: &Path, name: &str) -> anyhow::Result<Entry> {
    let mut v = load_unlocked(data_dir)?;
    let e = v.iter_mut().find(|e| e.name == name).ok_or_else(|| no_such(name))?;
    e.token = new_token()?;
    e.created = now();
    let e = e.clone();
    save(data_dir, &v)?;
    Ok(e)
}

/// Removes `name`; the last token that may operate stays (nobody could act otherwise)
pub fn remove(data_dir: &Path, name: &str) -> anyhow::Result<()> {
    locked(data_dir, || remove_unlocked(data_dir, name))
}

fn remove_unlocked(data_dir: &Path, name: &str) -> anyhow::Result<()> {
    let mut v = load_unlocked(data_dir)?;
    let Some(i) = v.iter().position(|e| e.name == name) else { return Err(no_such(name)) };
    if v[i].role == Role::Operate && v.iter().filter(|e| e.role == Role::Operate).count() == 1 {
        bail!(ictl_l10n::trf!(
            "{name} 是最后一个能操作的令牌，不能删；要换掉它用 viewer token show {name} --new",
            "{name} is the last token that may operate, so it stays; to replace it: viewer token show {name} --new"
        ));
    }
    v.remove(i);
    save(data_dir, &v)
}

fn no_such(name: &str) -> anyhow::Error {
    anyhow::anyhow!(ictl_l10n::trf!(
        "没有叫 {name} 的令牌（viewer token list 列出所有令牌）",
        "No token named {name} (viewer token list shows them)"
    ))
}

fn valid_token(t: &str) -> bool {
    t.len() >= 32 && t.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn new_token() -> io::Result<String> {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 32];
    ring::rand::SystemRandom::new().fill(&mut b).map_err(|_| io::Error::other("no random numbers"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Only this user may read it; written whole or not at all
fn write_private(file: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // a name of its own: two commands changing tokens at once must not write into the same temporary file
    let tmp = file.with_extension(format!("{}.{}.tmp", std::process::id(), &new_token()?[..8]));
    let written = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Who a request is logged in as
#[derive(Debug, Clone)]
pub struct Who {
    pub name: String,
    pub role: Role,
    /// With `Authorization: Bearer` (a program), not a session cookie (a browser): no `Origin` needed, since a web
    /// page can't make a browser send it
    pub bearer: bool,
    /// The token's session value: tells whether this very token is still there ([`Auth::knows`])
    session: String,
}

impl PartialEq for Who {
    fn eq(&self, other: &Self) -> bool {
        (&self.name, self.role, self.bearer) == (&other.name, other.role, other.bearer)
    }
}

impl Eq for Who {}

/// What a request shows of the login
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A valid session cookie or `Authorization: Bearer` token
    Ok(Who),
    /// Nothing (or only a stale cookie): log in
    Missing,
    /// A wrong token
    Wrong,
    /// Too many wrong tokens just now: not checked
    Limited,
}

/// Which viewer a session is for (docs/dev/multi-instance-plan.md 4.7): every instance's viewer takes the same
/// tokens, but a browser's session is bound to the instance (by its id: a name can be used again) and the port
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub instance: String,
    pub id: String,
    pub port: u16,
}

impl Scope {
    /// The default instance's viewer on `port`
    #[cfg(test)]
    pub fn default_instance(port: u16) -> Self {
        Self { instance: ictl_daemon::instance::DEFAULT.into(), id: ictl_daemon::instance::DEFAULT_ID.into(), port }
    }

    fn is_default(&self) -> bool {
        self.instance == ictl_daemon::instance::DEFAULT
    }
}

/// A token as the viewer checks it
struct Known {
    name: String,
    role: Role,
    token: String,
    /// The session cookie's value: HMAC-SHA256, under the token, of a text naming the viewer ([`Scope`])
    session: String,
    /// The value of before (the same text for every viewer), still taken by the default instance's viewer, where
    /// browsers logged in before
    v1: Option<String>,
}

impl Known {
    fn of(e: Entry, scope: &Scope) -> Self {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, e.token.as_bytes());
        let hex = |text: &[u8]| ring::hmac::sign(&key, text).as_ref().iter().map(|x| format!("{x:02x}")).collect();
        let v2 =
            format!("iphone-control viewer session v2 instance={} id={} port={}", scope.instance, scope.id, scope.port);
        let session = hex(v2.as_bytes());
        let v1 = scope.is_default().then(|| hex(b"iphone-control viewer session v1"));
        Self { name: e.name, role: e.role, token: e.token, session, v1 }
    }

    /// Whether `session` is this token's, for this viewer
    fn has(&self, session: &str) -> bool {
        same(session, &self.session) || self.v1.as_deref().is_some_and(|v| same(session, v))
    }

    fn who(&self, bearer: bool) -> Who {
        Who { name: self.name.clone(), role: self.role, bearer, session: self.session.clone() }
    }
}

/// The tokens, read again when the file changes (`viewer token …` while the viewer runs)
struct Tokens {
    known: Vec<Known>,
    modified: Option<Stamp>,
}

/// What tells that the tokens file changed: its time alone misses a change within the same second on file systems
/// that keep whole seconds (a token removed right after another change would stay valid). Each save is a new file
/// renamed into place, so its inode changes every time
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
    ino: u64,
}

fn stamp(data_dir: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(tokens_file(data_dir)).ok()?;
    Some(Stamp { modified: m.modified().ok()?, len: m.len(), ino: m.ino() })
}

pub struct Auth {
    /// `None`: fixed tokens (tests)
    data_dir: Option<PathBuf>,
    scope: Scope,
    tokens: Mutex<Tokens>,
    /// One per port: cookies aren't separated by port, and another viewer on this machine has other tokens
    cookie: String,
    failures: Mutex<VecDeque<Instant>>,
}

impl Auth {
    /// `<instance> <id>`: whose viewer this is, for the login page's header
    pub fn instance_header(&self) -> String {
        format!("{} {}", self.scope.instance, self.scope.id)
    }

    /// The tokens in `data_dir` ([`load`]), for the default instance's viewer on `port`
    #[cfg(test)]
    pub fn load(data_dir: &Path, port: u16) -> anyhow::Result<Self> {
        Self::load_for(data_dir, Scope::default_instance(port))
    }

    /// The tokens in `data_dir` ([`load`]), for the viewer `scope` says
    pub fn load_for(data_dir: &Path, scope: Scope) -> anyhow::Result<Self> {
        let known = load(data_dir)?.into_iter().map(|e| Known::of(e, &scope)).collect();
        Ok(Self::with(Some(data_dir.to_owned()), Tokens { known, modified: stamp(data_dir) }, scope))
    }

    /// These tokens and no file
    #[cfg(test)]
    pub fn fixed(entries: Vec<Entry>, port: u16) -> Self {
        let scope = Scope::default_instance(port);
        let known = entries.into_iter().map(|e| Known::of(e, &scope)).collect();
        Self::with(None, Tokens { known, modified: None }, scope)
    }

    fn with(data_dir: Option<PathBuf>, tokens: Tokens, scope: Scope) -> Self {
        let cookie = format!("ictl_viewer_{}", scope.port);
        Self { data_dir, scope, tokens: Mutex::new(tokens), cookie, failures: Mutex::default() }
    }

    /// The tokens, read again if the file changed. A file that can't be read leaves them as they were
    fn tokens(&self) -> std::sync::MutexGuard<'_, Tokens> {
        let mut t = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(dir) = &self.data_dir {
            let modified = stamp(dir);
            if modified != t.modified
                && let Ok(Some(v)) = read(dir)
            {
                *t = Tokens { known: v.into_iter().map(|e| Known::of(e, &self.scope)).collect(), modified };
            }
        }
        t
    }

    /// Whether the token `who` logged in with is still there (not removed or replaced since): a stream it opened
    /// ends when it isn't
    pub fn knows(&self, who: &Who) -> bool {
        self.tokens().known.iter().any(|k| k.has(&who.session))
    }

    /// The request's session cookie or `Authorization: Bearer` token
    pub fn check(&self, head: &[u8]) -> Verdict {
        let text = String::from_utf8_lossy(head);
        let mut bearer = None;
        for line in text.split("\r\n").skip(1) {
            let Some((k, v)) = line.split_once(':') else { continue };
            let k = k.trim();
            if k.eq_ignore_ascii_case("cookie") {
                let ours = v.split(';').filter_map(|c| c.trim().split_once('=')).find(|(n, _)| *n == self.cookie);
                if let Some((_, v)) = ours
                    && let Some(k) = self.tokens().known.iter().find(|k| k.has(v.trim()))
                {
                    // the value it came with: an earlier login's (v1) is known by it too ([`Self::knows`])
                    return Verdict::Ok(Who { session: v.trim().to_owned(), ..k.who(false) });
                }
            } else if k.eq_ignore_ascii_case("authorization") {
                let v = v.trim();
                if v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer ") {
                    bearer = Some(v[7..].trim().to_owned());
                }
            }
        }
        match bearer {
            Some(t) => self.try_token(&t),
            None => Verdict::Missing,
        }
    }

    /// A token someone typed or sent: right, or wrong (counted; [`Verdict::Limited`] after too many)
    pub fn try_token(&self, presented: &str) -> Verdict {
        // every token is compared, so the time doesn't tell how many there are before the right one
        let tokens = self.tokens();
        let found =
            tokens.known.iter().fold(None, |found, k| if same(presented.trim(), &k.token) { Some(k) } else { found });
        match found {
            Some(k) => Verdict::Ok(k.who(true)),
            None => self.failed(),
        }
    }

    /// A wrong token (or task token, see `viewer::login_gate`) was sent: [`Verdict::Wrong`], or
    /// [`Verdict::Limited`] once there were too many. Checked and counted under one lock: two wrong tokens at the
    /// same moment can't both slip under the limit
    pub fn failed(&self) -> Verdict {
        let mut f = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        if recent_failures(&mut f, now) >= MAX_FAILURES {
            return Verdict::Limited;
        }
        // counted up to the limit only: a flood doesn't keep it going after the minute
        f.push_back(now);
        Verdict::Wrong
    }

    /// `Set-Cookie` for a browser that logged in with the token named `name`; `secure` over HTTPS (the browser then
    /// only sends it there)
    pub fn set_cookie(&self, name: &str, secure: bool) -> String {
        let session =
            self.tokens().known.iter().find(|k| k.name == name).map(|k| k.session.clone()).unwrap_or_default();
        format!(
            "{}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax{}",
            self.cookie,
            session,
            SESSION_DAYS * 24 * 60 * 60,
            if secure { "; Secure" } else { "" }
        )
    }
}

/// Drops the failures older than [`WINDOW`] and counts the rest
fn recent_failures(f: &mut std::collections::VecDeque<Instant>, now: Instant) -> usize {
    while f.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
        f.pop_front();
    }
    f.len()
}

/// Equal, in a time that doesn't depend on where they differ (ring's `verify_slices_are_equal` is deprecated in
/// 0.17 and promises nothing about side channels)
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The login page ([`login_page`]); its words are filled in from [`LOGIN_TEXT`]
const LOGIN_PAGE: &str = include_str!("login.html");

/// The login page's words: key, Chinese, English. The page is sent in the viewer's language and switches to the
/// other when the browser chose it on the control page (`ictl.lang`), so it carries both
const LOGIN_TEXT: &[(&str, &str, &str)] = &[
    ("page_title", "登录 · iphone-control", "Log in · iphone-control"),
    ("sub", "控制台", "Control page"),
    ("title", "登录控制台", "Log in to the control page"),
    ("lead", "输入令牌后，就能看画面、操作手机。", "Enter the token to watch and control the phone."),
    ("label", "令牌", "Token"),
    ("placeholder", "粘贴令牌", "Paste the token"),
    ("show", "显示令牌", "Show the token"),
    ("hide", "隐藏令牌", "Hide the token"),
    ("submit", "登录", "Log in"),
    ("busy", "正在登录…", "Logging in…"),
    ("help_title", "还没有令牌？", "No token yet?"),
    ("help_run", "在接着手机的那台机器上运行：", "On the machine the phone is connected to, run:"),
    (
        "help_after",
        "它会显示令牌，还有一个点开就能直接登录的链接。",
        "It shows the token, and a link that logs you in directly.",
    ),
    ("copy", "复制", "Copy"),
    ("copied", "已复制", "Copied"),
    ("remember", "登录后这个浏览器会记住 30 天", "This browser stays logged in for 30 days"),
    ("other_lang", "English", "中文"),
    ("err_wrong", "令牌不对：检查一下有没有复制完整。", "That isn't the token: check that it was copied in full."),
    ("err_limited", "错误次数太多，一分钟后再试。", "Too many wrong tokens: try again in a minute."),
];

/// Why the login page is shown again
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    /// The token sent isn't one of them
    Wrong,
    /// Too many wrong tokens just now ([`Verdict::Limited`])
    Limited,
}

/// The login page, in the viewer's language ([`LOGIN_TEXT`])
pub fn login_page(error: Option<LoginError>) -> String {
    let en = matches!(ictl_l10n::lang(), ictl_l10n::Lang::En);
    let text = |zh: &str, e: &str| html_escape(if en { e } else { zh });
    let error = error.map_or_else(String::new, |e| {
        let key = match e {
            LoginError::Wrong => "err_wrong",
            LoginError::Limited => "err_limited",
        };
        let (_, zh, e) = LOGIN_TEXT.iter().find(|(k, ..)| *k == key).expect("in LOGIN_TEXT");
        format!(
            "<p class=\"err\" role=\"alert\"><svg viewBox=\"0 0 16 16\" fill=\"none\" stroke=\"currentColor\" \
             stroke-width=\"1.5\" stroke-linecap=\"round\" aria-hidden=\"true\"><circle cx=\"8\" cy=\"8\" r=\"6.5\"/>\
             <path d=\"M8 4.75v3.75M8 11h.01\"/></svg><span data-t=\"{key}\">{}</span></p>",
            text(zh, e)
        )
    });
    let both: serde_json::Map<String, serde_json::Value> =
        LOGIN_TEXT.iter().map(|(k, zh, e)| ((*k).to_owned(), serde_json::json!([zh, e]))).collect();
    // inside <script>: no "</" that could end it
    let strings = serde_json::Value::Object(both).to_string().replace("</", "<\\/");
    let mut page = LOGIN_PAGE
        .replace("{{error}}", &error)
        .replace("{{strings}}", &strings)
        .replace("{{lang}}", if en { "en" } else { "zh" })
        .replace("{{html_lang}}", if en { "en" } else { "zh-CN" });
    for (key, zh, e) in LOGIN_TEXT {
        page = page.replace(&format!("{{{{{key}}}}}"), &text(zh, e));
    }
    page
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// `token=…` from a login form (`application/x-www-form-urlencoded`)
pub fn form_token(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    text.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "token").then(|| super::percent_decode(&v.replace('+', " ")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(lines: &[&str]) -> Vec<u8> {
        let mut h = String::from("GET /api/info HTTP/1.1\r\nHost: 127.0.0.1:8202\r\n");
        for l in lines {
            h.push_str(l);
            h.push_str("\r\n");
        }
        h.push_str("\r\n");
        h.into_bytes()
    }

    fn entry(name: &str, role: Role, c: char) -> Entry {
        Entry { name: name.into(), role, token: c.to_string().repeat(64), created: 0 }
    }

    fn who(name: &str, role: Role, bearer: bool) -> Verdict {
        Verdict::Ok(Who { name: name.into(), role, bearer, session: String::new() })
    }

    #[test]
    fn the_owner_is_made_once_from_the_earlier_single_token() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(OLD_TOKEN_FILE), format!("{}\n", "a".repeat(64))).unwrap();
        let v = load(tmp.path()).unwrap();
        assert_eq!((v.len(), v[0].name.as_str(), v[0].role), (1, OWNER, Role::Operate));
        assert_eq!(v[0].token, "a".repeat(64), "upgrading keeps the token: nobody logs in again");
        assert!(!tmp.path().join(OLD_TOKEN_FILE).exists());
        let mode = std::fs::metadata(tokens_file(tmp.path())).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(load(tmp.path()).unwrap()[0].token, v[0].token, "kept");

        let fresh = tempfile::tempdir().unwrap();
        let t = &load(fresh.path()).unwrap()[0].token;
        assert!(t.len() == 64 && valid_token(t));
    }

    /// Commands adding tokens at once each keep theirs: the read-change-write is done one after the other
    #[test]
    fn tokens_added_at_once_are_all_kept() {
        let tmp = tempfile::tempdir().unwrap();
        load(tmp.path()).unwrap();
        let dir = tmp.path().to_owned();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || add(&dir, &format!("t{i}"), Role::View).unwrap())
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let names: Vec<String> = load(&dir).unwrap().into_iter().map(|e| e.name).collect();
        for i in 0..8 {
            assert!(names.contains(&format!("t{i}")), "{names:?}");
        }
    }

    /// A token removed is let go of at once, also when the file's time reads the same (a file system keeping whole
    /// seconds, a change within the same one)
    #[test]
    fn a_removed_token_is_noticed_within_the_same_second() {
        let tmp = tempfile::tempdir().unwrap();
        load(tmp.path()).unwrap();
        let tv = add(tmp.path(), "tv", Role::View).unwrap();
        let auth = Auth::load(tmp.path(), 8200).unwrap();
        let Verdict::Ok(who) = auth.try_token(&tv.token) else { panic!("logs in") };
        assert!(auth.knows(&who));
        let file = tokens_file(tmp.path());
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        remove(tmp.path(), "tv").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(before))
            .unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), before, "the same time");
        assert!(!auth.knows(&who), "noticed all the same");
    }

    #[test]
    fn tokens_are_added_renewed_and_removed_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let owner = load(tmp.path()).unwrap().remove(0);
        let tv = add(tmp.path(), "客厅电视", Role::View).unwrap();
        assert_eq!(tv.role, Role::View);
        assert!(add(tmp.path(), "客厅电视", Role::Operate).is_err(), "taken");
        assert!(add(tmp.path(), "has space", Role::View).is_err());
        assert!(add(tmp.path(), "", Role::View).is_err());
        assert_eq!(get(tmp.path(), "客厅电视").unwrap().token, tv.token);
        let renewed = renew(tmp.path(), "客厅电视").unwrap();
        assert_ne!(renewed.token, tv.token);
        assert_eq!(get(tmp.path(), OWNER).unwrap().token, owner.token, "the others stay");
        assert!(remove(tmp.path(), OWNER).is_err(), "the last that may operate");
        remove(tmp.path(), "客厅电视").unwrap();
        assert!(get(tmp.path(), "客厅电视").is_err());
        add(tmp.path(), "ci", Role::Operate).unwrap();
        remove(tmp.path(), OWNER).unwrap();
        assert!(remove(tmp.path(), "nobody").is_err());
    }

    #[test]
    fn a_session_cookie_or_a_bearer_token_says_who() {
        let auth = Auth::fixed(vec![entry(OWNER, Role::Operate, 'a'), entry("tv", Role::View, 'b')], 8202);
        let cookie = auth.set_cookie("tv", false);
        let pair = cookie.split(';').next().unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax") && !cookie.contains("Secure"));
        assert!(auth.set_cookie(OWNER, true).ends_with("; Secure"));
        assert!(!pair.contains(&"b".repeat(64)), "the cookie doesn't carry the token");
        assert_ne!(pair, auth.set_cookie(OWNER, false).split(';').next().unwrap(), "one session per token");

        assert_eq!(auth.check(&head(&[])), Verdict::Missing);
        assert_eq!(auth.check(&head(&[&format!("Cookie: theme=dark; {pair}")])), who("tv", Role::View, false));
        assert_eq!(auth.check(&head(&["Cookie: ictl_viewer_8202=deadbeef"])), Verdict::Missing, "stale: log in");
        assert_eq!(auth.check(&head(&[&format!("Cookie: {}", pair.replace("8202", "8203"))])), Verdict::Missing);
        let bearer = |t: char| format!("Authorization: Bearer {}", t.to_string().repeat(64));
        assert_eq!(auth.check(&head(&[&bearer('a')])), who(OWNER, Role::Operate, true));
        assert_eq!(auth.check(&head(&[&bearer('b').to_lowercase()])), who("tv", Role::View, true));
        assert_eq!(auth.check(&head(&["Authorization: Bearer nope"])), Verdict::Wrong);
        assert_eq!(auth.check(&head(&["Authorization: Basic Zm9vOmJhcg=="])), Verdict::Missing);
        // another set of tokens: the old sessions end
        let other = Auth::fixed(vec![entry(OWNER, Role::Operate, 'c')], 8202);
        assert_eq!(other.check(&head(&[&format!("Cookie: {pair}")])), Verdict::Missing);
    }

    /// A session belongs to the viewer it was made by: its instance (by id) and port. Another instance's viewer, or a
    /// new instance given the port of one removed, doesn't take it; the value of before (v1) is still taken by the
    /// default instance's viewer only
    #[test]
    fn a_session_is_the_instance_s_it_was_made_for() {
        let tokens = || vec![entry(OWNER, Role::Operate, 'a')];
        let scoped = |instance: &str, id: &str, port| {
            let scope = Scope { instance: instance.into(), id: id.into(), port };
            let known = tokens().into_iter().map(|e| Known::of(e, &scope)).collect();
            Auth::with(None, Tokens { known, modified: None }, scope)
        };
        let cookie = |a: &Auth| a.set_cookie(OWNER, false).split(';').next().unwrap().to_owned();
        let default = Auth::fixed(tokens(), 8200);
        let ipad = scoped("ipad", "3f2a", 8203);
        let ok = |a: &Auth, c: &str| matches!(a.check(&head(&[&format!("Cookie: {c}")])), Verdict::Ok(_));
        assert!(ok(&ipad, &cookie(&ipad)) && ok(&default, &cookie(&default)));
        let moved = cookie(&default).replace("8200", "8203");
        assert!(!ok(&ipad, &moved), "the default instance's session isn't the instance's");
        let reused = scoped("ipad", "77aa", 8203);
        assert!(!ok(&reused, &cookie(&ipad)), "removed and added again: a new id, the old sessions end");
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, "a".repeat(64).as_bytes());
        let v1: String = ring::hmac::sign(&key, b"iphone-control viewer session v1")
            .as_ref()
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect();
        assert!(ok(&default, &format!("ictl_viewer_8200={v1}")), "logged in before: still in");
        assert!(!ok(&ipad, &format!("ictl_viewer_8203={v1}")));
        let Verdict::Ok(w) = default.check(&head(&[&format!("Cookie: ictl_viewer_8200={v1}")])) else { panic!() };
        assert!(default.knows(&w));
    }

    /// `viewer token …` while the viewer runs takes effect without a restart
    #[test]
    fn the_viewer_follows_changes_to_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let auth = Auth::load(tmp.path(), 8202).unwrap();
        let owner = get(tmp.path(), OWNER).unwrap();
        let session = auth.set_cookie(OWNER, false).split(';').next().unwrap().to_owned();
        let Verdict::Ok(was) = auth.try_token(&owner.token) else { panic!("the owner's token") };
        assert!(auth.knows(&was));
        // a file's time is often only accurate to the second
        std::thread::sleep(Duration::from_millis(1100));
        let tv = add(tmp.path(), "tv", Role::View).unwrap();
        let Verdict::Ok(watching) = auth.try_token(&tv.token) else { panic!("tv's token") };
        assert_eq!(watching, Who { name: "tv".into(), role: Role::View, bearer: true, session: String::new() });
        std::thread::sleep(Duration::from_millis(1100));
        let renewed = renew(tmp.path(), OWNER).unwrap();
        assert_eq!(auth.try_token(&owner.token), Verdict::Wrong, "the old token");
        assert_eq!(auth.check(&head(&[&format!("Cookie: {session}")])), Verdict::Missing, "and its sessions");
        assert!(!auth.knows(&was), "what it opened ends: the token was replaced");
        let Verdict::Ok(now) = auth.try_token(&renewed.token) else { panic!("the new token") };
        assert!(auth.knows(&now) && auth.knows(&watching));
        std::thread::sleep(Duration::from_millis(1100));
        remove(tmp.path(), "tv").unwrap();
        assert!(!auth.knows(&watching), "and when it was removed");
    }

    #[test]
    fn wrong_tokens_are_limited_but_right_ones_get_in() {
        let auth = Auth::fixed(vec![entry(OWNER, Role::Operate, 'a')], 8202);
        let session = auth.set_cookie(OWNER, false).split(';').next().unwrap().to_owned();
        for _ in 0..MAX_FAILURES {
            assert_eq!(auth.try_token("guess"), Verdict::Wrong);
        }
        assert_eq!(auth.try_token("guess"), Verdict::Limited);
        // sending wrong tokens doesn't lock out those who have the right one
        assert!(matches!(auth.try_token(&"a".repeat(64)), Verdict::Ok(_)), "a right token gets in");
        assert!(
            matches!(auth.check(&head(&[&format!("Cookie: {session}")])), Verdict::Ok(_)),
            "logged-in browsers go on"
        );
        for _ in 0..100 {
            auth.try_token("guess");
        }
        assert_eq!(auth.failures.lock().unwrap().len(), MAX_FAILURES, "a flood isn't counted past the limit");
        // the minute is over
        auth.failures.lock().unwrap().iter_mut().for_each(|t| *t -= WINDOW + Duration::from_secs(1));
        assert_eq!(auth.try_token("guess"), Verdict::Wrong);
    }

    #[test]
    fn the_login_form_and_page() {
        assert_eq!(form_token(b"token=abc%2Bd+e&x=1").as_deref(), Some("abc+d e"));
        assert_eq!(form_token(b"x=1"), None);
        let page = login_page(Some(LoginError::Wrong));
        assert!(page.contains("action=\"/login\"") && page.contains("role=\"alert\""));
        assert!(!page.contains("{{"), "every placeholder is filled in");
        assert!(!login_page(None).contains("role=\"alert\""));
        // both languages ride along for the page to switch to the browser's choice
        for (key, zh, en) in LOGIN_TEXT {
            assert!(page.contains(&format!("\"{key}\":")), "{key}");
            assert!(!zh.contains('<') && !en.contains('<'), "{key}: plain text, set with textContent");
        }
        let keys: Vec<&str> = LOGIN_TEXT.iter().map(|(k, ..)| *k).collect();
        for used in LOGIN_PAGE.split("data-t=\"").skip(1).filter_map(|s| s.split('"').next()) {
            assert!(keys.contains(&used), "the page uses {used}, which LOGIN_TEXT doesn't have");
        }
    }
}
