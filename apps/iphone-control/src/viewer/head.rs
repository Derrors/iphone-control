//! HTTP/1.1 request head parsing and request-level checks (origin, fetch site, task token, encoding).

use super::{MAX_BODY, auth, lan, protocol};

/// A request whose answer is short, and leaves nothing unread: the page, and the page's API but for task events and
/// opening a task (streams)
pub(super) fn keepable(req: &Head) -> bool {
    let short = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => true,
        ("GET" | "POST", p) => p.starts_with("/api/") && p != "/api/agent/tasks/open" && task_events_id(p).is_none(),
        _ => false,
    };
    short && !req.close && !req.chunked && req.content_length.unwrap_or(0) <= MAX_BODY
}

/// `/api/agent/tasks/<id>/events`: the task's id
pub(super) fn task_events_id(path: &str) -> Option<&str> {
    path.strip_prefix("/api/agent/tasks/")?.strip_suffix("/events").filter(|id| !id.is_empty() && !id.contains('/'))
}

/// Whether an `Accept-Encoding` list takes gzip (RFC 9110 §12.5.3): `gzip` named, or else `*`; either with a `q` that
/// isn't a number above 0 (`q=0`, `q=NaN`) refuses it. `*` stands only for the codings not named, so `gzip;q=0, *`
/// refuses gzip
pub(super) fn accepts_gzip(v: &str) -> bool {
    let (mut named, mut star) = (None::<bool>, None::<bool>);
    for c in v.split(',') {
        let mut parts = c.split(';').map(str::trim);
        let name = parts.next().unwrap_or("");
        let taken = parts.filter_map(|p| p.split_once('=')).filter(|(k, _)| k.trim().eq_ignore_ascii_case("q")).all(
            |(_, q)| q.trim().parse::<f32>().is_ok_and(|q| q > 0.0), // NaN isn't above 0 either
        );
        // named twice: refused if either refuses
        let slot = if name.eq_ignore_ascii_case("gzip") {
            &mut named
        } else if name == "*" {
            &mut star
        } else {
            continue;
        };
        *slot = Some(slot.unwrap_or(true) && taken);
    }
    named.or(star).unwrap_or(false)
}

/// Who a request is, once `server::login_gate` let it in
#[derive(Default)]
pub(super) struct Pass {
    /// Logged in with this token; `None` when the viewer has no login (tests), or came in by a task's token
    pub(super) who: Option<auth::Who>,
    /// Came in by the token of an active task, without a login: an agent, which may only work for its task
    /// ([`TASK_PATHS`])
    pub(super) agent: bool,
}

/// What an agent may use with its task's token alone: the tools, and ending the task with a summary
pub(super) const TASK_PATHS: [&str; 2] = ["/mcp", "/api/agent/tasks/finish"];

impl Pass {
    /// A program (`Authorization: Bearer`, or a task's token), which needs no `Origin`: a web page can't make a
    /// browser send either
    pub(super) fn program(&self) -> bool {
        self.agent || self.who.as_ref().is_some_and(|w| w.bearer)
    }

    /// A read-only token may look (any `GET` but `/mcp`, and report how much of a video it read); an agent only works
    /// for its task
    /// A read-only token: may look, but nothing is done on the phone for it (not even unlocking it)
    pub(super) fn watches(&self) -> bool {
        self.who.as_ref().is_some_and(|w| w.role == auth::Role::View)
    }

    pub(super) fn may(&self, req: &Head) -> bool {
        match &self.who {
            Some(w) if w.role == auth::Role::View => {
                (req.method == "GET" && req.path != "/mcp") || (req.method == "POST" && req.path == "/api/video/ack")
            }
            None if self.agent => TASK_PATHS.contains(&req.path.as_str()),
            _ => true,
        }
    }
}

/// Where a plain HTTP request from the network goes: the same page over HTTPS, at the name it asked for (one of
/// this machine's: [`lan::Access::host_allowed`] already checked it)
pub(super) fn https_location(head: &[u8], req: &Head, port: u16) -> Option<String> {
    let host = lan::host_header(head).filter(|h| !h.is_empty())?;
    let query = if req.query.is_empty() { String::new() } else { format!("?{}", req.query) };
    Some(format!("https://{host}:{port}{}{query}", req.path))
}

pub(super) struct Head {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) query: String,
    pub(super) origin: Option<String>,
    pub(super) content_type: Option<String>,
    pub(super) content_length: Option<usize>,
    /// `X-Ictl-Device`: the device the page shows; its actions are refused once the daemon controls another
    pub(super) device: Option<String>,
    pub(super) fetch_site: Option<String>,
    /// `X-Ictl-Lang`: the page's language
    pub(super) lang: Option<ictl_l10n::Lang>,
    /// [`protocol::TASK_TOKEN_HEADER`]: an agent working for a task
    pub(super) task_token: Option<String>,
    /// HTTP/1.0, or `Connection: close`: the client closes after the answer
    pub(super) close: bool,
    /// `Transfer-Encoding`: a body without a length, which this server reads only for `/mcp` (hyper decodes it)
    pub(super) chunked: bool,
    /// … and it is just `chunked`
    pub(super) transfer_chunked_only: bool,
    /// `Accept-Encoding` takes gzip ([`accepts_gzip`])
    pub(super) gzip: bool,
}

impl Head {
    /// `None` (answered 400) for what isn't a plain HTTP/1.x request: a target that isn't a path (it is put into the
    /// HTTPS redirect's `Location`, [`https_location`]), control characters or a lone CR or LF anywhere (they would
    /// end a header line we write), a second `Host` or `Content-Length`, or a body framed both ways (a request read
    /// two ways is how requests are smuggled past proxies)
    pub(super) fn parse(head: &[u8]) -> Option<Self> {
        if !clean_head(head) {
            return None;
        }
        let text = std::str::from_utf8(head).ok()?;
        // the blank line that ends the head; any other empty line is a malformed one
        let mut lines = text.trim_end_matches("\r\n").split("\r\n");
        let mut first = lines.next()?.split(' ');
        let method = first.next()?.to_owned();
        let target = first.next()?;
        let close = match first.next()? {
            "HTTP/1.1" => false,
            "HTTP/1.0" => true,
            _ => return None,
        };
        if first.next().is_some()
            || method.is_empty()
            || !method.bytes().all(|b| b.is_ascii_uppercase())
            || !target.starts_with('/')
            || target.starts_with("//")
            // visible ASCII only: a tab or a byte past ASCII went into the redirect's `Location` as it was
            || !target.bytes().all(|b| (0x21..=0x7e).contains(&b))
        {
            return None;
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let (path, query) = (path.to_owned(), query.to_owned());
        let mut h = Self {
            method,
            path,
            query,
            origin: None,
            content_type: None,
            content_length: None,
            fetch_site: None,
            lang: None,
            device: None,
            task_token: None,
            close,
            chunked: false,
            transfer_chunked_only: false,
            gzip: false,
        };
        let mut hosts = 0;
        // every `Accept-Encoding` line together: they make one list (RFC 9110 §5.3)
        let mut accept = String::new();
        for l in lines {
            // a line without a colon (a folded continuation, garbage) or a name with blanks (`Content-Length : 1`,
            // which some proxies read and others don't): 400, as RFC 9112 §5.1 asks
            let (k, v) = l.split_once(':')?;
            if k.is_empty() || !k.bytes().all(is_tchar) {
                return None;
            }
            let v = v.trim();
            match k.to_ascii_lowercase().as_str() {
                "host" => hosts += 1,
                "origin" => h.origin = Some(v.to_owned()),
                "content-type" => h.content_type = Some(v.to_ascii_lowercase()),
                // a second length is refused even when it is the same: proxies differ in which one they take
                "content-length" if h.content_length.is_some() => return None,
                "content-length" if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) => return None,
                "content-length" => h.content_length = Some(v.parse().ok()?),
                "sec-fetch-site" => h.fetch_site = Some(v.to_ascii_lowercase()),
                "x-ictl-lang" => h.lang = ictl_l10n::parse(v),
                "accept-encoding" => {
                    accept.push_str(v);
                    accept.push(',');
                }
                "x-ictl-device" if !v.is_empty() => h.device = Some(v.to_owned()),
                "connection" => h.close |= v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")),
                "transfer-encoding" => {
                    // a second Transfer-Encoding line adds codings to the first
                    h.transfer_chunked_only = !h.chunked && v.eq_ignore_ascii_case("chunked");
                    h.chunked = true;
                }
                k if k == protocol::TASK_TOKEN_HEADER && !v.is_empty() => h.task_token = Some(v.to_owned()),
                _ => {}
            }
        }
        if hosts > 1 || (h.chunked && h.content_length.is_some()) {
            return None;
        }
        h.gzip = accepts_gzip(&accept);
        Some(h)
    }

    /// A query parameter, percent-decoded
    pub(super) fn query(&self, key: &str) -> Option<String> {
        self.query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k == key).then(|| percent_decode(v))
        })
    }
}

/// A character of a header name (RFC 9110 §5.6.2 `tchar`)
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// A request head with nothing but printable text, tabs and CRLF line ends: no other control character, no CR or LF
/// on its own
pub(super) fn clean_head(head: &[u8]) -> bool {
    let mut i = 0;
    while i < head.len() {
        match head[i] {
            b'\r' if head.get(i + 1) == Some(&b'\n') => i += 2,
            b'\t' => i += 1,
            b if b < 0x20 || b == 0x7f => return false,
            _ => i += 1,
        }
    }
    true
}

pub(super) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An action must come from this page: `Origin` is the viewer's own address (one of this machine's names with
/// `--lan`), and the body is JSON, which a cross-origin page can't send without a preflight that this server never
/// answers
/// `bearer`: logged in with `Authorization: Bearer` (a program). A browser always sends `Origin` with a POST, and a
/// web page can't make it send that header, so without `Origin` only a program's request is taken
pub(super) fn action_allowed(req: &Head, access: &lan::Access, port: u16, tls: bool, bearer: bool) -> bool {
    let json = req.content_type.as_deref().is_some_and(|c| c.split(';').next() == Some("application/json"));
    json && match req.origin.as_deref() {
        Some(o) => access.origin_allowed(o, port, tls),
        None => bearer,
    }
}

/// A read from another site (an `<img>` or `<script>` on some page) is refused: browsers say so in
/// `Sec-Fetch-Site`. Requests without it (curl, older browsers) come from this machine anyway
pub(super) fn fetch_site_ok(req: &Head) -> bool {
    req.fetch_site.as_deref().is_none_or(|s| matches!(s, "same-origin" | "none"))
}
