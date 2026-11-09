//! Developer tools (`IPHONE_CONTROL_DEV_TOOLS`): the phone's logs, crash reports, installing and removing apps.
//!
//! The MCP server only lists these tools when the switch is on; the daemon always understands the requests.
//!
//! Logs come from os_trace_relay. It only streams what happens from now on, so the first `AppLogs` request
//! starts a capture into a ring buffer (at most [`MAX_LINES`]); later requests read from it, which is what makes
//! `since` work. The capture follows the controlled device: it stops when the daemon switches to another device or
//! the device goes offline, and by itself after [`IDLE`] without a request, so a session that used the tool once
//! doesn't keep a log stream running.
//!
//! The stream leaves out debug and info messages (see `ictl_device::Device::log_stream`: asking for them slows the
//! phone). It is still every process, tens of lines a second, so the buffer is bounded by size as well as by count
//! ([`MAX_BYTES`]): messages are cut to
//! [`MAX_LINE_CHARS`] and processes kept by name only when they arrive. How far back it reaches depends on how busy
//! the phone is, so every answer says so.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ictl_device::{LogEntry, LogLevel};
use tokio::task::JoinHandle;

use crate::core::{Daemon, Inner};

const MAX_LINES: usize = 20_000;
/// Text kept in the buffer (messages, process names, subsystems); the oldest lines go first
const MAX_BYTES: usize = 8 * 1024 * 1024;
const IDLE: Duration = Duration::from_secs(15 * 60);
/// Longest crash report text returned; the start holds the exception, the crashed thread and the app version
const MAX_REPORT_CHARS: usize = 40_000;
const MAX_LINE_CHARS: usize = 600;

pub(crate) struct Capture {
    udid: String,
    started: Instant,
    buf: Arc<Mutex<Buffer>>,
    last_query: Arc<Mutex<Instant>>,
    task: JoinHandle<()>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Daemon {
    pub(crate) async fn app_logs(
        &self,
        g: &mut Inner,
        process: &str,
        grep: &str,
        since: Duration,
        limit: usize,
        errors_only: bool,
    ) -> Result<String, String> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let running = g.log_capture.as_ref().is_some_and(|c| c.udid == device.udid && !c.task.is_finished());
        if !running {
            let mut stream = device.log_stream().await.map_err(|e| e.to_string())?;
            let buf = Arc::new(Mutex::new(Buffer::default()));
            let last_query = Arc::new(Mutex::new(Instant::now()));
            let (b, q) = (buf.clone(), last_query.clone());
            let task = tokio::spawn(async move {
                let mut check = tokio::time::interval(Duration::from_secs(30));
                loop {
                    tokio::select! {
                        e = stream.next() => {
                            let Ok(e) = e else { return };
                            b.lock().unwrap_or_else(|p| p.into_inner()).push(Instant::now(), compact(e));
                        }
                        _ = check.tick() => {
                            if q.lock().unwrap_or_else(|p| p.into_inner()).elapsed() > IDLE {
                                return;
                            }
                        }
                    }
                }
            });
            g.log_capture = Some(Capture { udid: device.udid.clone(), started: Instant::now(), buf, last_query, task });
        }
        let c = g.log_capture.as_ref().expect("set above");
        *c.last_query.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();

        // an app's name or bundle ID → the process it runs as; anything else is taken as a process name
        let wanted = if process.is_empty() { None } else { Some(self.process_name(g, process).await) };
        let c = g.log_capture.as_ref().expect("set above");
        let filter = LogFilter { process: wanted.as_deref(), grep, errors_only };
        let from = Instant::now().checked_sub(since);
        let (lines, oldest) = {
            let b = c.buf.lock().unwrap_or_else(|p| p.into_inner());
            let mut hits: Vec<String> = b
                .lines
                .iter()
                .rev()
                .filter(|(t, _)| from.is_none_or(|f| *t >= f))
                .filter(|(_, e)| filter.matches(e))
                .take(limit)
                .map(|(t, e)| format_line(t.elapsed(), e))
                .collect();
            hits.reverse();
            (hits, b.lines.front().map(|(t, _)| t.elapsed().as_secs()))
        };
        let mut out = if running {
            ictl_l10n::trf!(
                "最近 {} 秒内 {} 行匹配(日志从 {} 秒前开始记录):",
                "In the last {} s, {} lines match (capture running for {} s):",
                since.as_secs(),
                lines.len(),
                c.started.elapsed().as_secs()
            )
        } else {
            ictl_l10n::tr!(
                "刚开始记录日志,这次调用之前的日志拿不到。复现问题后再调一次。",
                "Log capture started just now; lines from before this call aren't available. Reproduce the problem, then call again."
            )
            .to_owned()
        };
        if running && lines.is_empty() {
            out += ictl_l10n::tr!(" 没有。", " none.");
        }
        // the buffer may reach back less far than `since`: say how far, so a missing line isn't taken as "didn't happen"
        if running && let Some(o) = oldest.filter(|o| *o < since.as_secs()) {
            out += &ictl_l10n::trf!(
                "\n(缓冲里最早一行是 {o} 秒前,更早的已经挤出去了)",
                "\n(the oldest line in the buffer is from {o} s ago; earlier lines have been dropped)"
            );
        }
        for l in lines {
            out.push('\n');
            out += &l;
        }
        Ok(out)
    }

    /// The executable name of an installed app given by name or bundle ID, or `process` itself
    async fn process_name(&self, g: &mut Inner, process: &str) -> String {
        for system in [false, true] {
            if let Ok(apps) = self.list_apps(g, system).await
                && let Some(a) = apps.iter().find(|a| a.bundle_id == process || a.name == process)
                && !a.executable.is_empty()
            {
                return a.executable.clone();
            }
        }
        process.to_owned()
    }

    pub(crate) async fn crash_reports(&self, g: &mut Inner, filter: &str, name: &str) -> Result<String, String> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        if !name.is_empty() {
            if name.contains('/') || name.contains("..") {
                return Err(ictl_l10n::tr!(
                    "name 要是 iphone_crash_reports 列出的文件名",
                    "name must be a file name listed by iphone_crash_reports"
                )
                .into());
            }
            let data = device.crash_report(name).await.map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&data);
            let n = text.chars().count();
            if n <= MAX_REPORT_CHARS {
                return Ok(text.into_owned());
            }
            let head: String = text.chars().take(MAX_REPORT_CHARS).collect();
            return Ok(head
                + &ictl_l10n::trf!(
                    "\n…(共 {n} 个字符,只返回前 {MAX_REPORT_CHARS} 个)",
                    "\n… ({n} characters in total, only the first {MAX_REPORT_CHARS} returned)"
                ));
        }
        let names = device.crash_reports().await.map_err(|e| e.to_string())?;
        let list = crash_list(names, filter);
        if list.is_empty() {
            return Ok(if filter.is_empty() {
                ictl_l10n::tr!("没有崩溃报告", "No crash reports").into()
            } else {
                ictl_l10n::trf!("没有包含「{filter}」的崩溃报告", "No crash reports containing “{filter}”")
            });
        }
        Ok(ictl_l10n::tr!(
            "崩溃报告(新的在前;用 name 取其中一份的内容):\n",
            "Crash reports (newest first; pass one as name to read it):\n"
        )
        .to_owned()
            + &list.join("\n"))
    }

    /// Runs without holding the daemon lock (an install can take minutes), like re-signing
    pub(crate) async fn install_app(&self, path: &str) -> Result<String, String> {
        let device =
            self.inner.lock().await.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let p = std::path::Path::new(path);
        if !p.exists() {
            return Err(ictl_l10n::trf!(
                "找不到 {path}(路径要在运行守护进程的机器上,容器部署时是容器里的路径)",
                "{path} not found (the path is on the machine that runs the daemon; inside the container for a container deployment)"
            ));
        }
        device.install(p).await.map_err(|e| e.to_string())?;
        self.inner.lock().await.apps.clear();
        Ok(ictl_l10n::trf!("已安装 {path}", "Installed {path}"))
    }

    pub(crate) async fn uninstall_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        let bid = self.resolve_app(g, app).await?;
        if is_wda_runner(&bid, g.persisted.wda_bundle.as_deref()) {
            return Err(ictl_l10n::trf!(
                "{bid} 是 WebDriverAgent,iphone-control 靠它控制手机,不能卸载",
                "{bid} is WebDriverAgent, which iphone-control needs to control the phone; it can't be uninstalled"
            ));
        }
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        device.uninstall(&bid).await.map_err(|e| e.to_string())?;
        g.apps.clear();
        Ok(ictl_l10n::trf!("已卸载 {bid}", "Uninstalled {bid}"))
    }
}

/// The captured lines, bounded by [`MAX_LINES`] and [`MAX_BYTES`]
#[derive(Default)]
pub(crate) struct Buffer {
    lines: VecDeque<(Instant, LogEntry)>,
    bytes: usize,
}

impl Buffer {
    fn push(&mut self, at: Instant, e: LogEntry) {
        self.bytes += size(&e);
        self.lines.push_back((at, e));
        while self.lines.len() > MAX_LINES || self.bytes > MAX_BYTES {
            let Some((_, old)) = self.lines.pop_front() else { break };
            self.bytes -= size(&old);
        }
    }
}

fn size(e: &LogEntry) -> usize {
    e.process.len() + e.message.len() + e.subsystem.as_ref().map_or(0, String::len)
}

/// What the buffer keeps of an entry: the process by name (the filter and the output only use the name) and the
/// message cut to [`MAX_LINE_CHARS`] (marked with `…`)
fn compact(mut e: LogEntry) -> LogEntry {
    if let Some(name) = e.process.rsplit('/').next()
        && name.len() < e.process.len()
    {
        e.process = name.to_owned();
    }
    if let Some((i, _)) = e.message.char_indices().nth(MAX_LINE_CHARS) {
        e.message.truncate(i);
        e.message.push('…');
    }
    e.message.shrink_to_fit();
    e
}

/// The WDA runner this daemon signed, or any other WebDriverAgent build
fn is_wda_runner(bid: &str, signed: Option<&str>) -> bool {
    signed.is_some_and(|s| s.eq_ignore_ascii_case(bid)) || bid.to_ascii_lowercase().contains("webdriveragentrunner")
}

struct LogFilter<'a> {
    /// executable name, matched against the last path component, ignoring case
    process: Option<&'a str>,
    grep: &'a str,
    errors_only: bool,
}

impl LogFilter<'_> {
    fn matches(&self, e: &LogEntry) -> bool {
        if self.errors_only && !matches!(e.level, LogLevel::Error | LogLevel::Fault) {
            return false;
        }
        if let Some(p) = self.process {
            let name = e.process.rsplit('/').next().unwrap_or_default();
            if !name.eq_ignore_ascii_case(p) {
                return false;
            }
        }
        if !self.grep.is_empty() {
            let g = self.grep.to_lowercase();
            let sub = e.subsystem.as_deref().unwrap_or_default();
            if !e.message.to_lowercase().contains(&g) && !sub.to_lowercase().contains(&g) {
                return false;
            }
        }
        true
    }
}

/// `-12.3s Process[pid] <Error> subsystem:category message`, the message cut to [`MAX_LINE_CHARS`]
fn format_line(age: Duration, e: &LogEntry) -> String {
    let name = e.process.rsplit('/').next().unwrap_or_default();
    let level = match e.level {
        LogLevel::Debug => "<Debug> ",
        LogLevel::Info => "<Info> ",
        LogLevel::Notice => "",
        LogLevel::Error => "<Error> ",
        LogLevel::Fault => "<Fault> ",
    };
    let sub = e.subsystem.as_deref().map(|s| format!("{s} ")).unwrap_or_default();
    // already cut when stored (`compact`); cut again for entries that didn't go through it
    let mut msg: String = e.message.chars().take(MAX_LINE_CHARS).collect();
    if msg.len() < e.message.len() {
        msg.push('…');
    }
    format!("-{:.1}s {name}[{}] {level}{sub}{}", age.as_secs_f64(), e.pid, msg.replace('\n', " ⏎ "))
}

/// Report files (not directories such as `Retired`), filtered, newest first, at most 50
fn crash_list(names: Vec<String>, filter: &str) -> Vec<String> {
    let f = filter.to_lowercase();
    let mut v: Vec<String> = names
        .into_iter()
        .filter(|n| n.contains('.') && !n.starts_with('.'))
        .filter(|n| f.is_empty() || n.to_lowercase().contains(&f))
        .collect();
    v.sort_by(|a, b| report_date(b).cmp(report_date(a)).then_with(|| a.cmp(b)));
    v.truncate(50);
    v
}

/// `MyApp-2026-11-08-120000.ips` → `2026-11-08-120000`, for sorting by date across apps
fn report_date(name: &str) -> &str {
    let stem = name.split('.').next().unwrap_or(name);
    let b = stem.as_bytes();
    (0..b.len().saturating_sub(16))
        .rev()
        .find(|&i| {
            b[i..].len() >= 17
                && b[i..i + 4].iter().all(u8::is_ascii_digit)
                && b[i + 4] == b'-'
                && b[i + 7] == b'-'
                && b[i + 10] == b'-'
        })
        .map_or("", |i| &stem[i..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(process: &str, level: LogLevel, msg: &str) -> LogEntry {
        LogEntry {
            pid: 42,
            process: process.into(),
            level,
            subsystem: Some("com.example:net".into()),
            message: msg.into(),
        }
    }

    #[test]
    fn wda_runner_cannot_be_uninstalled() {
        let signed = Some("com.facebook.WebDriverAgentRunner.xctrunner.ABC123");
        assert!(is_wda_runner("com.facebook.WebDriverAgentRunner.xctrunner.ABC123", signed));
        // another build of the runner, or ours before a re-sign changed the suffix
        assert!(is_wda_runner("com.example.WebDriverAgentRunner.xctrunner", None));
        assert!(!is_wda_runner("com.example.myapp", signed));
    }

    #[test]
    fn log_filter() {
        let app = "/private/var/containers/Bundle/Application/X/My App.app/MyApp";
        let e = entry(app, LogLevel::Error, "Request failed: timeout");
        let f = |process, grep, errors_only| LogFilter { process, grep, errors_only };
        assert!(f(Some("myapp"), "", false).matches(&e));
        assert!(!f(Some("App"), "", false).matches(&e));
        assert!(f(None, "TIMEOUT", true).matches(&e));
        assert!(f(None, "example:net", false).matches(&e));
        assert!(!f(None, "", true).matches(&entry(app, LogLevel::Notice, "ok")));
    }

    #[test]
    fn log_line_format() {
        let e = entry("/usr/libexec/backboardd", LogLevel::Fault, "a\nb");
        assert_eq!(format_line(Duration::from_millis(1500), &e), "-1.5s backboardd[42] <Fault> com.example:net a ⏎ b");
        let long = entry("/x/P", LogLevel::Notice, &"é".repeat(700));
        assert!(format_line(Duration::ZERO, &long).ends_with("é…"));
    }

    /// Lines are kept short and the buffer stays within its byte budget, dropping the oldest lines first
    #[test]
    fn buffer_is_bounded_by_size() {
        let e = compact(entry("/private/var/containers/X.app/MyApp", LogLevel::Debug, &"x".repeat(5000)));
        assert_eq!(e.process, "MyApp");
        assert_eq!(e.message.chars().count(), MAX_LINE_CHARS + 1);
        let mut b = Buffer::default();
        let t = Instant::now();
        let per = size(&e);
        for i in 0..(MAX_BYTES / per + 100) {
            b.push(t + Duration::from_micros(i as u64), e.clone());
        }
        assert!(b.bytes <= MAX_BYTES && b.lines.len() <= MAX_LINES, "{} bytes, {} lines", b.bytes, b.lines.len());
        assert_eq!(b.bytes, b.lines.iter().map(|(_, e)| size(e)).sum::<usize>());
        assert!(b.lines.front().unwrap().0 > t, "the oldest lines went first");
    }

    #[test]
    fn crash_reports_newest_first() {
        let names = vec![
            "Retired".into(),
            "MyApp-2026-11-07-090000.ips".into(),
            "Other-2026-11-08-120000.ips".into(),
            "MyApp-2026-11-08-101500.ips".into(),
        ];
        assert_eq!(
            crash_list(names.clone(), ""),
            ["Other-2026-11-08-120000.ips", "MyApp-2026-11-08-101500.ips", "MyApp-2026-11-07-090000.ips"]
        );
        assert_eq!(crash_list(names, "myapp").len(), 2);
        assert_eq!(report_date("JetsamEvent-2026-11-08-101500.ips"), "2026-11-08-101500");
        assert_eq!(report_date("weird.ips"), "");
    }
}
