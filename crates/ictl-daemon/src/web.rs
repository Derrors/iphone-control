//! Web Inspector tools (part of the developer tools): list inspectable pages and WebViews, evaluate JavaScript.
//!
//! One Web Inspector connection is kept and reused across requests. Opening a new one shortly after the last one
//! closed takes about 8 seconds, and after a few connect/disconnect rounds the phone stopped reporting Safari to new
//! connections until Safari was restarted (iPhone 15 Pro Max, iOS 27.0.1); on a kept connection listing and
//! evaluating take a few milliseconds. The connection is closed after [`WEB_IDLE`] without a request, when the
//! daemon controls another device or none, and after an error (the next request opens a new one).
//!
//! Web requests take their own lock, and the daemon lock only to look up the device, so a slow page doesn't hold up
//! taps and screenshots.

use std::time::{Duration, Instant};

use ictl_webinspector::{App, Eval, Page, WebInspector};

use ictl_device::Device;

use crate::core::Daemon;

/// A kept connection is closed after this long without a request
const WEB_IDLE: Duration = Duration::from_secs(5 * 60);

/// The kept Web Inspector connection, for one device
pub(crate) struct WebConn {
    udid: String,
    w: WebInspector,
    used: Instant,
}

/// How long applications get to report their pages
const LISTING_WAIT: Duration = Duration::from_secs(3);
const MAX_RESULT_CHARS: usize = 50_000;

impl Daemon {
    /// `slot` is [`Daemon::web`], already locked by the caller (see `Daemon::handle`)
    pub(crate) async fn web_targets(&self, slot: &mut Option<WebConn>, device: &Device) -> Result<String, String> {
        let (apps, pages) = web_pages(slot, device).await?;
        if pages.is_empty() {
            return Ok(ictl_l10n::tr!(
                "没有可检查的页面。需要:设置 → Safari → 高级 → 网页检查器 打开;App 里的 WebView 要是调试构建或设了 isInspectable;页面所在的 App 在前台(刚打开的 App 等几秒再试)",
                "No inspectable pages. Needed: Settings → Safari → Advanced → Web Inspector on; WebViews in apps must be debug builds or have isInspectable set; the app showing the page is in the foreground (wait a few seconds after opening it)"
            )
            .into());
        }
        let mut out = ictl_l10n::tr!(
            "可检查的页面(id 用于 iphone_web_eval 的 target):",
            "Inspectable pages (pass the id as target to iphone_web_eval):"
        )
        .to_owned();
        for p in &pages {
            out.push('\n');
            out += &target_line(p, &apps);
        }
        Ok(out)
    }

    pub(crate) async fn web_eval(
        &self,
        slot: &mut Option<WebConn>,
        device: &Device,
        target: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<String, String> {
        let (apps, pages) = web_pages(slot, device).await?;
        let page = pick(&pages, target).map_err(|e| match e {
            Pick::None => ictl_l10n::trf!(
                "没有和「{target}」对应的页面,用 iphone_web_targets 查看",
                "No page matches “{target}”; see iphone_web_targets"
            ),
            Pick::Several(c) => ictl_l10n::trf!(
                "「{target}」对应多个页面,用 id 指定其中一个:\n{}",
                "“{target}” matches several pages; pass one id:\n{}",
                c.iter().map(|p| target_line(p, &apps)).collect::<Vec<_>>().join("\n")
            ),
        })?;
        let conn = slot.as_mut().ok_or("Web Inspector")?;
        conn.used = Instant::now();
        // not retried on a new connection: the script may already have run
        let r = match conn.w.evaluate(page, expression, timeout).await {
            Ok(r) => r,
            Err(e) => {
                if breaks_connection(&e) {
                    *slot = None;
                }
                return Err(page_error(&e));
            }
        };
        Ok(match r {
            Eval::Value(None) => "undefined".into(),
            Eval::Value(Some(v)) => truncate(v.to_string()),
            Eval::Thrown(d) => return Err(ictl_l10n::trf!("页面里抛出异常:{d}", "The page threw: {d}")),
        })
    }

    /// Closes the kept connection after [`WEB_IDLE`], or when the daemon no longer controls its device. Called from
    /// the health check; skipped while a web request is running
    pub(crate) fn web_housekeeping(&self, controlled: Option<&str>) {
        if let Ok(mut slot) = self.web.try_lock()
            && slot.as_ref().is_some_and(|c| c.used.elapsed() > WEB_IDLE || Some(c.udid.as_str()) != controlled)
        {
            *slot = None;
        }
    }
}

/// The pages, on the kept connection (opened if there is none, or it belongs to another device). A connection that
/// turns out to be broken is replaced once: listing pages changes nothing on the phone
async fn web_pages(slot: &mut Option<WebConn>, device: &Device) -> Result<(Vec<App>, Vec<Page>), String> {
    if slot.as_ref().is_some_and(|c| c.udid != device.udid) {
        *slot = None;
    }
    for attempt in 0..2 {
        if slot.is_none() {
            let w = device.web_inspector().await.map_err(|e| match e {
                ictl_device::DeviceError::Io(e) => closed_by_phone(&e),
                e => e.to_string(),
            })?;
            *slot = Some(WebConn { udid: device.udid.clone(), w, used: Instant::now() });
        }
        let conn = slot.as_mut().ok_or("Web Inspector")?;
        conn.used = Instant::now();
        match conn.w.pages(LISTING_WAIT).await {
            Ok(r) => return Ok(r),
            Err(e) if breaks_connection(&e) => {
                *slot = None;
                if attempt == 1 {
                    return Err(page_error(&e));
                }
                tracing::debug!("Web Inspector connection broken, opening a new one: {e}");
            }
            Err(e) => return Err(page_error(&e)),
        }
    }
    unreachable!("the second attempt returns")
}

/// The Web Inspector's own errors are in English; the one an agent or user meets in normal use follows the language
fn page_error(e: &ictl_webinspector::Error) -> String {
    match e {
        ictl_webinspector::Error::Timeout(_) => {
            ictl_l10n::tr!("等待页面回应超时", "timed out waiting for the page to answer").into()
        }
        ictl_webinspector::Error::Io(e) => closed_by_phone(e),
        e => e.to_string(),
    }
}

/// The phone closes the Web Inspector connection right away when Settings → Safari → Advanced → Web Inspector is
/// off (seen on iOS 27.2.1: `peer closed connection without sending TLS close_notify`); the raw I/O error doesn't
/// say that
fn closed_by_phone(e: &std::io::Error) -> String {
    ictl_l10n::trf!(
        "手机断开了 Web Inspector 连接({e})。多半是没打开 设置 → Safari → 高级 → 网页检查器,打开后重试",
        "The phone closed the Web Inspector connection ({e}). Usually Settings → Safari → Advanced → Web Inspector is off; turn it on and retry"
    )
}

/// Errors after which the connection can't be trusted: everything except a page that didn't answer in time or
/// refused a command (reads are cancel-safe and late answers for a closed session are dropped). Reopening costs
/// about 8 seconds, and repeated reopening made the phone stop reporting Safari, so only reopen when needed
fn breaks_connection(e: &ictl_webinspector::Error) -> bool {
    !matches!(e, ictl_webinspector::Error::Timeout(_) | ictl_webinspector::Error::Command(_))
}

/// `PID:812/1  WebPage  Safari (com.apple.mobilesafari)  Title — https://…`
fn target_line(p: &Page, apps: &[App]) -> String {
    let app = apps.iter().find(|a| a.id == p.app_id);
    // a WebContent process stands in for the app that shows the page
    let shown = app.and_then(|a| a.host.as_deref()).and_then(|h| apps.iter().find(|a| a.id == h)).or(app);
    let who = shown.map(|a| format!("{} ({})", a.name, a.bundle_id)).unwrap_or_else(|| p.app_id.clone());
    let busy =
        if p.in_use { ictl_l10n::tr!(" [另一个调试器已连接]", " [another debugger is attached]") } else { "" };
    format!("{}/{}  {}  {who}  {} — {}{busy}", p.app_id, p.id, p.kind, p.title, p.url)
}

enum Pick<'a> {
    None,
    Several(Vec<&'a Page>),
}

/// `target` is an id from the list (`PID:812/1`), or text in the URL or title; empty means the only page there is
fn pick<'a>(pages: &'a [Page], target: &str) -> Result<&'a Page, Pick<'a>> {
    if let Some(p) = pages.iter().find(|p| format!("{}/{}", p.app_id, p.id) == target) {
        return Ok(p);
    }
    let t = target.to_lowercase();
    let hits: Vec<&Page> =
        pages.iter().filter(|p| p.url.to_lowercase().contains(&t) || p.title.to_lowercase().contains(&t)).collect();
    match hits.len() {
        0 => Err(Pick::None),
        1 => Ok(hits[0]),
        _ => Err(Pick::Several(hits)),
    }
}

fn truncate(s: String) -> String {
    let n = s.chars().count();
    if n <= MAX_RESULT_CHARS {
        return s;
    }
    let head: String = s.chars().take(MAX_RESULT_CHARS).collect();
    head + &ictl_l10n::trf!(
        "\n…(共 {n} 个字符,只返回前 {MAX_RESULT_CHARS} 个;在表达式里只取需要的部分)",
        "\n… ({n} characters in total, only the first {MAX_RESULT_CHARS} returned; select just what you need in the expression)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(app: &str, id: u64, url: &str, title: &str) -> Page {
        Page { app_id: app.into(), id, kind: "WebPage".into(), title: title.into(), url: url.into(), in_use: false }
    }

    #[test]
    fn only_a_page_timeout_keeps_the_connection() {
        use ictl_webinspector::Error;
        assert!(!breaks_connection(&Error::Timeout("the page to answer")));
        assert!(!breaks_connection(&Error::Command("Missing injected script for given objectId".into())));
        assert!(breaks_connection(&Error::Protocol("x".into())));
        assert!(breaks_connection(&Error::Io(std::io::Error::other("closed"))));
    }

    #[test]
    fn a_closed_connection_points_at_the_web_inspector_setting() {
        use ictl_webinspector::Error;
        let eof = || std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "peer closed connection");
        let msg = page_error(&Error::Io(eof()));
        assert!(msg.contains("网页检查器") && msg.contains("peer closed connection"), "{msg}");
        assert_eq!(closed_by_phone(&eof()), msg);
        assert!(!page_error(&Error::Protocol("x".into())).contains("网页检查器"));
    }

    #[test]
    fn picks_by_id_or_text() {
        let pages =
            [page("PID:1", 1, "capacitor://localhost", "Taulit"), page("PID:2", 4, "https://example.com/a", "Example")];
        assert_eq!(pick(&pages, "PID:2/4").ok().map(|p| p.id), Some(4));
        assert_eq!(pick(&pages, "capacitor").ok().map(|p| p.id), Some(1));
        assert_eq!(pick(&pages, "EXAMPLE").ok().map(|p| p.id), Some(4));
        assert!(matches!(pick(&pages, ""), Err(Pick::Several(v)) if v.len() == 2));
        assert!(matches!(pick(&pages, "nothing"), Err(Pick::None)));
        assert_eq!(pick(&pages[..1], "").ok().map(|p| p.id), Some(1));
    }

    #[test]
    fn webcontent_proxies_show_their_host_app() {
        let apps = [
            App {
                id: "PID:640".into(),
                name: "Taulit".into(),
                bundle_id: "com.example.taulit".into(),
                host: None,
                active: true,
            },
            App {
                id: "PID:812".into(),
                name: "WebContent".into(),
                bundle_id: "com.apple.WebKit.WebContent".into(),
                host: Some("PID:640".into()),
                active: true,
            },
        ];
        let line = target_line(&page("PID:812", 1, "capacitor://localhost", "Home"), &apps);
        assert_eq!(line, "PID:812/1  WebPage  Taulit (com.example.taulit)  Home — capacitor://localhost");
    }
}
