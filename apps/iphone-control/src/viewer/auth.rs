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

/// The tokens; the first start makes [`OWNER`] (from the earlier single token if there is one)
pub fn load(data_dir: &Path) -> anyhow::Result<Vec<Entry>> {
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
    let mut v = load(data_dir)?;
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
    let mut v = load(data_dir)?;
    let e = v.iter_mut().find(|e| e.name == name).ok_or_else(|| no_such(name))?;
    e.token = new_token()?;
    e.created = now();
    let e = e.clone();
    save(data_dir, &v)?;
    Ok(e)
}

/// Removes `name`; the last token that may operate stays (nobody could act otherwise)
pub fn remove(data_dir: &Path, name: &str) -> anyhow::Result<()> {
    let mut v = load(data_dir)?;
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
    let tmp = file.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, file)
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

/// A token as the viewer checks it
struct Known {
    name: String,
    role: Role,
    token: String,
    /// The session cookie's value: HMAC-SHA256 of a fixed text under the token
    session: String,
}

impl Known {
    fn of(e: Entry) -> Self {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, e.token.as_bytes());
        let tag = ring::hmac::sign(&key, b"iphone-control viewer session v1");
        let session = tag.as_ref().iter().map(|x| format!("{x:02x}")).collect();
        Self { name: e.name, role: e.role, token: e.token, session }
    }

    fn who(&self, bearer: bool) -> Who {
        Who { name: self.name.clone(), role: self.role, bearer, session: self.session.clone() }
    }
}

/// The tokens, read again when the file changes (`viewer token …` while the viewer runs)
struct Tokens {
    known: Vec<Known>,
    modified: Option<SystemTime>,
}

pub struct Auth {
    /// `None`: fixed tokens (tests)
    data_dir: Option<PathBuf>,
    tokens: Mutex<Tokens>,
    /// One per port: cookies aren't separated by port, and another viewer on this machine has other tokens
    cookie: String,
    failures: Mutex<VecDeque<Instant>>,
}

impl Auth {
    /// The tokens in `data_dir` ([`load`])
    pub fn load(data_dir: &Path, port: u16) -> anyhow::Result<Self> {
        let known = load(data_dir)?.into_iter().map(Known::of).collect();
        let modified = std::fs::metadata(tokens_file(data_dir)).and_then(|m| m.modified()).ok();
        Ok(Self::with(Some(data_dir.to_owned()), Tokens { known, modified }, port))
    }

    /// These tokens and no file
    #[cfg(test)]
    pub fn fixed(entries: Vec<Entry>, port: u16) -> Self {
        Self::with(None, Tokens { known: entries.into_iter().map(Known::of).collect(), modified: None }, port)
    }

    fn with(data_dir: Option<PathBuf>, tokens: Tokens, port: u16) -> Self {
        Self { data_dir, tokens: Mutex::new(tokens), cookie: format!("ictl_viewer_{port}"), failures: Mutex::default() }
    }

    /// The tokens, read again if the file changed. A file that can't be read leaves them as they were
    fn tokens(&self) -> std::sync::MutexGuard<'_, Tokens> {
        let mut t = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(dir) = &self.data_dir {
            let modified = std::fs::metadata(tokens_file(dir)).and_then(|m| m.modified()).ok();
            if modified != t.modified
                && let Ok(Some(v)) = read(dir)
            {
                *t = Tokens { known: v.into_iter().map(Known::of).collect(), modified };
            }
        }
        t
    }

    /// Whether the token `who` logged in with is still there (not removed or replaced since): a stream it opened
    /// ends when it isn't
    pub fn knows(&self, who: &Who) -> bool {
        self.tokens().known.iter().any(|k| same(&who.session, &k.session))
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
                    && let Some(k) = self.tokens().known.iter().find(|k| same(v.trim(), &k.session))
                {
                    return Verdict::Ok(k.who(false));
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

    /// Too many wrong tokens just now
    pub fn limited(&self) -> bool {
        let mut f = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        while f.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
            f.pop_front();
        }
        f.len() >= MAX_FAILURES
    }

    /// A wrong token (or task token, see `viewer::login_gate`) was sent: [`Verdict::Wrong`], or
    /// [`Verdict::Limited`] once there were too many
    pub fn failed(&self) -> Verdict {
        let limited = self.limited();
        let mut f = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        // counted up to the limit only: a flood doesn't keep it going after the minute
        if !limited {
            f.push_back(Instant::now());
        }
        if limited { Verdict::Limited } else { Verdict::Wrong }
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

/// Equal, in a time that doesn't depend on where they differ
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The login page, in the viewer's language. `error`: why the last try didn't work
pub fn login_page(error: Option<&str>) -> String {
    let (lang, title, lead, label, button, hint) = match ictl_l10n::lang() {
        ictl_l10n::Lang::En => (
            "en",
            "Log in · iphone-control",
            "This control page needs its token.",
            "Token",
            "Log in",
            "Run <code>iphone-control viewer token</code> on the machine the phone is connected to: it shows a token and a link that logs in.",
        ),
        ictl_l10n::Lang::Zh => (
            "zh-CN",
            "登录 · iphone-control",
            "打开控制台要先输入令牌。",
            "令牌",
            "登录",
            "在接着手机的那台机器上运行 <code>iphone-control viewer token</code>，会显示令牌和一个点开就能登录的链接。",
        ),
    };
    let error = error.map(|e| format!("<p class=\"err\" role=\"alert\">{}</p>", html_escape(e))).unwrap_or_default();
    format!(
        r#"<!doctype html>
<html lang="{lang}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
:root {{ color-scheme: light dark; font-family: system-ui, -apple-system, "PingFang SC", "Microsoft YaHei", sans-serif; }}
body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: Canvas; color: CanvasText; }}
main {{ width: min(360px, calc(100vw - 48px)); }}
h1 {{ font-size: 20px; margin: 0 0 6px; }}
p {{ margin: 0 0 18px; opacity: .75; line-height: 1.5; font-size: 14px; }}
label {{ display: block; font-size: 13px; margin-bottom: 6px; }}
input {{ box-sizing: border-box; width: 100%; padding: 10px 12px; font: inherit; font-family: ui-monospace, monospace; border: 1px solid color-mix(in srgb, CanvasText 25%, transparent); border-radius: 8px; background: Field; color: FieldText; }}
button {{ margin-top: 14px; width: 100%; padding: 10px; font: inherit; font-weight: 600; border: 0; border-radius: 8px; background: #2563eb; color: #fff; cursor: pointer; }}
.err {{ color: #dc2626; opacity: 1; }}
.hint {{ margin-top: 18px; font-size: 13px; }}
code {{ font-size: 12px; padding: 1px 4px; border-radius: 4px; background: color-mix(in srgb, CanvasText 10%, transparent); }}
</style></head>
<body><main>
<h1>iphone-control</h1>
<p>{lead}</p>
{error}<form method="post" action="/login">
<label for="token">{label}</label>
<input id="token" name="token" type="password" autocomplete="current-password" autofocus required>
<button type="submit">{button}</button>
</form>
<p class="hint">{hint}</p>
</main></body></html>
"#
    )
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
        let page = login_page(Some("<b>wrong</b>"));
        assert!(page.contains("&lt;b&gt;wrong&lt;/b&gt;") && page.contains("action=\"/login\""));
    }
}
