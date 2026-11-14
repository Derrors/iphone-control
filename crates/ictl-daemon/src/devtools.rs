//! Developer tools (`IPHONE_CONTROL_DEV_TOOLS`): the phone's logs, crash reports, installing and removing apps.
//!
//! The MCP server only lists these tools when the switch is on; the daemon always understands the requests.
//!
//! Logs come from os_trace_relay. It only streams what happens from now on, so the first `AppLogs` request
//! starts a capture into a ring buffer (at most [`MAX_LINES`]); later requests read from it, which is what makes
//! `since` work. The capture follows the controlled device: it stops when the daemon switches to another device or
//! the device goes offline, and after [`IDLE`] without a request (checked by the health check, see
//! [`Daemon::log_housekeeping`]), so a session that used the tool once doesn't keep a log stream running.
//!
//! The reader task only ever awaits the next entry: an entry is several reads, and cancelling one half-way (as a
//! `select!` against a timer would) leaves the stream out of step.
//!
//! The stream leaves out debug messages (see `ictl_device::Device::log_stream`: asking for them slows the phone), and
//! the info ones that still arrive are dropped ([`kept`]). It is still every process, tens of lines a second, so the
//! buffer is bounded by size as well as by count ([`MAX_BYTES`]): messages are cut to [`MAX_LINE_CHARS`] and processes
//! kept by name only when they arrive. How far back it reaches depends on how busy the phone is, so every answer says
//! so.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ictl_device::{AppInfo, LogEntry, LogLevel};
use tokio::task::JoinHandle;

use crate::core::{Daemon, Inner};

const MAX_LINES: usize = 20_000;
/// Text kept in the buffer (messages, process names, subsystems); the oldest lines go first
const MAX_BYTES: usize = 8 * 1024 * 1024;
const IDLE: Duration = Duration::from_secs(15 * 60);
/// Longest crash report text returned; the start holds the exception, the crashed thread and the app version
const MAX_REPORT_CHARS: usize = 40_000;
const MAX_LINE_CHARS: usize = 600;
/// The first line arrives a moment after recording starts; a gap larger than this means older lines were dropped
const DROPPED_MARGIN: u64 = 3;

pub(crate) struct Capture {
    udid: String,
    started: Instant,
    buf: Arc<Mutex<Buffer>>,
    last_query: Instant,
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
    ) -> crate::error::Result<String> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let running = g.dev.log_capture.as_ref().is_some_and(|c| c.udid == device.udid && !c.task.is_finished());
        if !running {
            let mut stream = device.log_stream().await?;
            let buf = Arc::new(Mutex::new(Buffer::default()));
            let b = buf.clone();
            let task = tokio::spawn(async move {
                // never cancelled mid-entry (see the module docs); stopped by aborting the task
                while let Ok(e) = stream.next().await {
                    if kept(&e) {
                        b.lock().unwrap_or_else(|p| p.into_inner()).push(Instant::now(), compact(e));
                    }
                }
            });
            let now = Instant::now();
            g.dev.log_capture = Some(Capture { udid: device.udid.clone(), started: now, buf, last_query: now, task });
        }
        if let Some(c) = g.dev.log_capture.as_mut() {
            c.last_query = Instant::now();
        }

        // an app's name or bundle ID → the process it runs as; anything else is taken as a process name
        let wanted = if process.is_empty() { None } else { Some(self.process_name(g, process).await) };
        let c = g.dev.log_capture.as_ref().expect("set above");
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
        // (only when lines were really dropped: right after the start the first sentence already says how far it goes)
        let recording_for = c.started.elapsed().as_secs();
        if running && let Some(o) = oldest.filter(|o| *o < since.as_secs() && o + DROPPED_MARGIN < recording_for) {
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

    /// Stops the log capture after [`IDLE`] without a request, or once its stream has ended. Called from the health
    /// check, which holds the daemon lock
    pub(crate) fn log_housekeeping(g: &mut Inner) {
        if g.dev.log_capture.as_ref().is_some_and(|c| c.last_query.elapsed() > IDLE || c.task.is_finished()) {
            g.dev.log_capture = None;
        }
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

    pub(crate) async fn crash_reports(&self, g: &mut Inner, filter: &str, name: &str) -> crate::error::Result<String> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        if !name.is_empty() {
            if name.contains('/') || name.contains("..") {
                return Err(ictl_l10n::tr!(
                    "name 要是 iphone_crash_reports 列出的文件名",
                    "name must be a file name listed by iphone_crash_reports"
                )
                .into());
            }
            let data = device.crash_report(name).await?;
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
        let names = device.crash_reports().await?;
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
    /// Installs are queued one at a time inside `Device::install` (they share one upload path on the phone)
    pub(crate) async fn install_app(&self, device: &ictl_device::Device, path: &str) -> crate::error::Result<String> {
        let p = std::path::Path::new(path);
        if !p.exists() {
            return Err(ictl_l10n::trf!(
                "找不到 {path}(路径要在运行守护进程的机器上)",
                "{path} not found (the path is on the machine that runs the daemon)"
            )
            .into());
        }
        device.install(p).await?;
        self.inner.lock().await.dev.apps.clear();
        Ok(ictl_l10n::trf!("已安装 {path}", "Installed {path}"))
    }

    /// Uninstalling deletes the app's data, so unlike launching, the name must match exactly: a bundle ID or the
    /// whole display name. The app lists are read again first, so an app installed a moment ago is found
    pub(crate) async fn uninstall_app(&self, g: &mut Inner, app: &str) -> crate::error::Result<String> {
        g.dev.apps.clear();
        let mut all = self.list_apps(g, false).await?;
        all.extend(self.list_apps(g, true).await?);
        let bid = uninstall_target(app, &all)?;
        if is_wda_runner(&bid, g.persisted.wda_bundle.as_deref()) {
            return Err(ictl_l10n::trf!(
                "{bid} 是 WebDriverAgent,iphone-control 靠它控制手机,不能卸载",
                "{bid} is WebDriverAgent, which iphone-control needs to control the phone; it can't be uninstalled"
            )
            .into());
        }
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        device.uninstall(&bid).await?;
        g.dev.apps.clear();
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

/// Only the default (Notice), Error and Fault levels are kept. The stream is started without debug messages, but some
/// processes still send Info ones (seen from MobileTimer on iOS 27.2.1); they are dropped here so the tool returns
/// what it says, and they don't take room in the buffer
fn kept(e: &LogEntry) -> bool {
    !matches!(e.level, LogLevel::Debug | LogLevel::Info)
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

/// The bundle ID to uninstall: `app` is an installed bundle ID, or exactly one app's display name (ignoring case).
/// Anything else is refused, listing similar names, since a wrong guess deletes an app with its data
fn uninstall_target(app: &str, apps: &[AppInfo]) -> Result<String, String> {
    let app = app.trim();
    if app.is_empty() {
        return Err(ictl_l10n::tr!("app 不能为空", "app can't be empty").into());
    }
    if let Some(a) = apps.iter().find(|a| a.bundle_id.eq_ignore_ascii_case(app)) {
        return Ok(a.bundle_id.clone());
    }
    let exact: Vec<&AppInfo> = apps.iter().filter(|a| a.name.eq_ignore_ascii_case(app)).collect();
    let line = |a: &&AppInfo| format!("{}\t{}", a.name, a.bundle_id);
    match exact.as_slice() {
        [one] => Ok(one.bundle_id.clone()),
        [] => {
            let needle = app.to_lowercase();
            let similar: Vec<String> =
                apps.iter().filter(|a| a.name.to_lowercase().contains(&needle)).take(5).map(|a| line(&a)).collect();
            Err(if similar.is_empty() {
                ictl_l10n::trf!(
                    "没有装 \"{app}\"(卸载要用 Bundle ID 或完整的 App 名字),用 iphone_list_apps 查看",
                    "\"{app}\" is not installed (uninstalling needs the bundle ID or the full app name); see iphone_list_apps"
                )
            } else {
                ictl_l10n::trf!(
                    "没有名字正好是 \"{app}\" 的 App,没有卸载;卸载要用 Bundle ID 或完整的名字。相近的:\n{}",
                    "No app is named exactly \"{app}\", nothing uninstalled; uninstalling needs the bundle ID or the full name. Similar:\n{}",
                    similar.join("\n")
                )
            })
        }
        many => Err(ictl_l10n::trf!(
            "有 {} 个 App 叫 \"{app}\",没有卸载;用 Bundle ID 指定:\n{}",
            "{} apps are named \"{app}\", nothing uninstalled; pass the bundle ID:\n{}",
            many.len(),
            many.iter().map(line).collect::<Vec<_>>().join("\n")
        )),
    }
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

/// `MyApp-2026-11-08-120000.ips` → `2026-11-08-120000`, for sorting by date across apps. Searched in the whole name:
/// process names can contain dots (`com.apple.WebKit.WebContent-2026-….ips`, `Foo.cpu_resource-….ips`)
fn report_date(name: &str) -> &str {
    let b = name.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    (0..b.len().saturating_sub(16))
        .rev()
        .find(|&i| {
            digits(i..i + 4)
                && b[i + 4] == b'-'
                && digits(i + 5..i + 7)
                && b[i + 7] == b'-'
                && digits(i + 8..i + 10)
                && b[i + 10] == b'-'
                && digits(i + 11..i + 17)
        })
        .map_or("", |i| &name[i..i + 17])
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
    fn uninstall_needs_an_exact_name() {
        let app = |n: &str, b: &str| AppInfo { name: n.into(), bundle_id: b.into(), executable: String::new() };
        let apps = [
            app("Test Flight", "com.apple.TestFlight"),
            app("MyTest", "com.example.mytest"),
            app("Notes", "com.apple.mobilenotes"),
            app("Notes", "com.other.notes"),
        ];
        assert!(uninstall_target("", &apps).is_err(), "empty must not match the first app");
        assert!(uninstall_target("  ", &apps).is_err());
        assert_eq!(uninstall_target("com.example.MyTest", &apps).as_deref(), Ok("com.example.mytest"));
        assert_eq!(uninstall_target("mytest", &apps).as_deref(), Ok("com.example.mytest"));
        let e = uninstall_target("Test", &apps).unwrap_err();
        assert!(e.contains("com.apple.TestFlight") && e.contains("com.example.mytest"), "{e}");
        assert!(uninstall_target("Notes", &apps).unwrap_err().contains("com.other.notes"), "ambiguous");
        assert!(uninstall_target("com.not.installed", &apps).is_err());
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
    fn info_and_debug_entries_are_not_kept() {
        let level = |l| kept(&entry("/x/P", l, "m"));
        assert!(!level(LogLevel::Debug) && !level(LogLevel::Info));
        assert!(level(LogLevel::Notice) && level(LogLevel::Error) && level(LogLevel::Fault));
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
        assert_eq!(report_date("com.apple.WebKit.WebContent-2026-11-09-101500.ips"), "2026-11-09-101500");
        assert_eq!(report_date("Foo.cpu_resource-2026-11-01-080000.ips.synced"), "2026-11-01-080000");
        let names =
            vec!["Old-2026-11-01-080000.ips".into(), "com.apple.WebKit.WebContent-2026-11-09-101500.ips".into()];
        assert_eq!(crash_list(names, "")[0], "com.apple.WebKit.WebContent-2026-11-09-101500.ips");
    }
}
