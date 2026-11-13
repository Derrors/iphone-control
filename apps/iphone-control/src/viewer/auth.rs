//! Login for the control page and `/mcp` (docs/task-control.md 7.5).
//!
//! Every HTTP request needs it, from this machine too: a reverse proxy or tunnel on this machine (frp, nginx,
//! cloudflared, `tailscale serve`) makes every visitor's connection come from `127.0.0.1`, so the page can't tell
//! the user at their own browser from someone on the internet. The Unix socket (the CLI, `iphone-control mcp` over
//! stdio, `task run`) isn't HTTP and doesn't need it.
//!
//! One token, made on the first start and kept in `<data dir>/viewer-token` (0600); `iphone-control viewer token`
//! shows it with a link that logs in, `--new` replaces it (and every session with it). A browser logs in once (the
//! login page, or the link with `?token=`) and gets a session cookie: `HttpOnly`, `SameSite=Lax`, `Secure` over
//! HTTPS, 30 days. Its value is an HMAC of the token, so it shows nothing of the token and ends when the token is
//! replaced. Programs send `Authorization: Bearer <token>`.
//!
//! Wrong tokens are counted for the whole page, not per address (behind a proxy every address is `127.0.0.1`): after
//! [`MAX_FAILURES`] in a minute, tokens aren't checked until the minute is over. The token is 256 random bits, so
//! this only makes guessing slower still; a session that is already logged in isn't affected.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Under the data directory
pub const TOKEN_FILE: &str = "viewer-token";
/// Wrong tokens in [`WINDOW`] before tokens are refused until it is over
const MAX_FAILURES: usize = 20;
const WINDOW: Duration = Duration::from_secs(60);
/// How long a browser stays logged in
const SESSION_DAYS: u64 = 30;

pub fn token_file(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKEN_FILE)
}

/// The token, made now if there is none
pub fn load_or_create(data_dir: &Path) -> io::Result<String> {
    match std::fs::read_to_string(token_file(data_dir)) {
        Ok(t) if valid_token(t.trim()) => Ok(t.trim().to_owned()),
        Ok(_) => replace(data_dir),
        Err(e) if e.kind() == io::ErrorKind::NotFound => replace(data_dir),
        Err(e) => Err(e),
    }
}

/// A new token in place of the old one; every browser has to log in again
pub fn replace(data_dir: &Path) -> io::Result<String> {
    let token = new_token()?;
    write_private(&token_file(data_dir), token.as_bytes())?;
    Ok(token)
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

/// What a request shows of the login
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A valid session cookie or `Authorization: Bearer` token
    Ok,
    /// Nothing (or only a stale cookie): log in
    Missing,
    /// A wrong token
    Wrong,
    /// Too many wrong tokens just now: not checked
    Limited,
}

pub struct Auth {
    token: String,
    /// The session cookie's value: HMAC-SHA256 of a fixed text under the token
    session: String,
    /// One per port: cookies aren't separated by port, and another viewer on this machine has another token
    cookie: String,
    failures: Mutex<VecDeque<Instant>>,
}

impl Auth {
    pub fn new(token: String, port: u16) -> Self {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, token.as_bytes());
        let tag = ring::hmac::sign(&key, b"iphone-control viewer session v1");
        let session = tag.as_ref().iter().map(|x| format!("{x:02x}")).collect();
        Self { token, session, cookie: format!("ictl_viewer_{port}"), failures: Mutex::default() }
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
                if ours.is_some_and(|(_, v)| same(v.trim(), &self.session)) {
                    return Verdict::Ok;
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

    /// A token someone typed or sent: right, wrong (counted), or not checked now
    pub fn try_token(&self, presented: &str) -> Verdict {
        let mut f = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        while f.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
            f.pop_front();
        }
        if f.len() >= MAX_FAILURES {
            return Verdict::Limited;
        }
        if same(presented.trim(), &self.token) {
            Verdict::Ok
        } else {
            f.push_back(now);
            Verdict::Wrong
        }
    }

    /// `Set-Cookie` for a browser that logged in; `secure` over HTTPS (the browser then only sends it there)
    pub fn set_cookie(&self, secure: bool) -> String {
        format!(
            "{}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax{}",
            self.cookie,
            self.session,
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
            "Run <code>iphone-control viewer token</code> on the machine the phone is connected to: it shows the token and a link that logs in.",
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

    #[test]
    fn the_token_is_made_once_kept_private_and_replaced_on_demand() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let a = load_or_create(tmp.path()).unwrap();
        assert_eq!(a.len(), 64);
        assert!(valid_token(&a));
        assert_eq!(load_or_create(tmp.path()).unwrap(), a, "kept");
        let mode = std::fs::metadata(token_file(tmp.path())).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let b = replace(tmp.path()).unwrap();
        assert_ne!(a, b);
        assert_eq!(load_or_create(tmp.path()).unwrap(), b);
        std::fs::write(token_file(tmp.path()), "short").unwrap();
        assert_ne!(load_or_create(tmp.path()).unwrap(), "short", "a broken file is replaced");
    }

    #[test]
    fn a_session_cookie_or_a_bearer_token_lets_a_request_in() {
        let auth = Auth::new("a".repeat(64), 8202);
        let cookie = auth.set_cookie(false);
        let pair = cookie.split(';').next().unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax") && !cookie.contains("Secure"));
        assert!(auth.set_cookie(true).ends_with("; Secure"));
        assert!(!pair.contains(&"a".repeat(64)), "the cookie doesn't carry the token");

        assert_eq!(auth.check(&head(&[])), Verdict::Missing);
        assert_eq!(auth.check(&head(&[&format!("Cookie: theme=dark; {pair}")])), Verdict::Ok);
        assert_eq!(auth.check(&head(&["Cookie: ictl_viewer_8202=deadbeef"])), Verdict::Missing, "stale: log in");
        assert_eq!(auth.check(&head(&[&format!("Cookie: {}", pair.replace("8202", "8203"))])), Verdict::Missing);
        assert_eq!(auth.check(&head(&[&format!("Authorization: Bearer {}", "a".repeat(64))])), Verdict::Ok);
        assert_eq!(auth.check(&head(&[&format!("authorization: bearer {}", "a".repeat(64))])), Verdict::Ok);
        assert_eq!(auth.check(&head(&["Authorization: Bearer nope"])), Verdict::Wrong);
        assert_eq!(auth.check(&head(&["Authorization: Basic Zm9vOmJhcg=="])), Verdict::Missing);
        // a new token ends the old sessions
        let other = Auth::new("b".repeat(64), 8202);
        assert_eq!(other.check(&head(&[pair])), Verdict::Missing);
        assert_eq!(other.check(&head(&[&format!("Cookie: {pair}")])), Verdict::Missing);
    }

    #[test]
    fn wrong_tokens_are_limited_for_everyone() {
        let auth = Auth::new("a".repeat(64), 8202);
        let session = auth.set_cookie(false).split(';').next().unwrap().to_owned();
        for _ in 0..MAX_FAILURES {
            assert_eq!(auth.try_token("guess"), Verdict::Wrong);
        }
        assert_eq!(auth.try_token("guess"), Verdict::Limited);
        assert_eq!(auth.try_token(&"a".repeat(64)), Verdict::Limited, "not even checked for a minute");
        assert_eq!(auth.check(&head(&[&format!("Cookie: {session}")])), Verdict::Ok, "logged-in browsers go on");
        // the minute is over
        auth.failures.lock().unwrap().iter_mut().for_each(|t| *t -= WINDOW + Duration::from_secs(1));
        assert_eq!(auth.try_token(&"a".repeat(64)), Verdict::Ok);
    }

    #[test]
    fn the_login_form_and_page() {
        assert_eq!(form_token(b"token=abc%2Bd+e&x=1").as_deref(), Some("abc+d e"));
        assert_eq!(form_token(b"x=1"), None);
        let page = login_page(Some("<b>wrong</b>"));
        assert!(page.contains("&lt;b&gt;wrong&lt;/b&gt;") && page.contains("action=\"/login\""));
    }
}
