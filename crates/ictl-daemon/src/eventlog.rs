//! 守护进程的事件日志（`logs/daemon.log`）：一行一条，带时间，超过 5MB 轮转成 `.1`。
//! 相当于 toolkit 的 supervisor.log，`iphone-control status` 显示它的最后几行。

use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct EventLog {
    path: PathBuf,
    max: u64,
}

impl EventLog {
    pub fn new(dir: &Path) -> Self {
        Self { path: dir.join("daemon.log"), max: MAX_BYTES }
    }

    #[cfg(test)]
    fn with_max(dir: &Path, max: u64) -> Self {
        Self { path: dir.join("daemon.log"), max }
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }

    pub fn log(&self, msg: &str) {
        tracing::info!("{msg}");
        if let Err(e) = self.append(msg) {
            tracing::warn!("{}", ictl_l10n::log_trf!("写事件日志失败：{e}", "Writing the event log failed: {e}"));
        }
    }

    fn append(&self, msg: &str) -> std::io::Result<()> {
        if std::fs::metadata(&self.path).map(|m| m.len() >= self.max).unwrap_or(false) {
            std::fs::rename(&self.path, self.path.with_extension("log.1"))?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(f, "{} {msg}", timestamp())
    }

    /// 最后 n 行。只读文件末尾一段（日志最大 5MB，每次 status 都整个读太浪费）
    pub fn tail(&self, n: usize) -> Vec<String> {
        use std::io::{Read, Seek, SeekFrom};
        const WINDOW: u64 = 16 * 1024;
        let Ok(mut f) = std::fs::File::open(&self.path) else { return Vec::new() };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(WINDOW);
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(start)).is_err() || f.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        let text = String::from_utf8_lossy(&buf);
        let mut lines: Vec<&str> = text.lines().collect();
        // 从中间截开的话第一行不完整，丢掉
        if start > 0 && !lines.is_empty() {
            lines.remove(0);
        }
        lines[lines.len().saturating_sub(n)..].iter().map(|s| (*s).to_owned()).collect()
    }
}

/// 现在的本地时间，带时区偏移（见 [`format_local`]）
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    format_local(secs)
}

/// 给人看的时间：本地时间加时区偏移，`2026-11-24 16:08:52 +08:00`。以前只写 UTC 又不标明，东八区的用户看到的
/// 日志时间差 8 小时。偏移按那一刻算（夏令时前后不同）；取不到本地时区时就是 UTC（`+00:00`）
pub fn format_local(secs: u64) -> String {
    format_at(secs, local_offset(secs))
}

/// `secs` 那一刻本地时间比 UTC 快多少秒（东边为正）
fn local_offset(secs: u64) -> i64 {
    let Ok(t) = libc::time_t::try_from(secs) else { return 0 };
    // SAFETY: localtime_r only writes the tm it is given; a zeroed tm is a valid value to start from
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    // c_long: i64 on the 64-bit hosts we ship for, narrower elsewhere
    #[allow(clippy::unnecessary_cast)]
    let offset = tm.tm_gmtoff as i64;
    offset
}

/// `secs`（UTC）按偏移 `offset` 秒写成本地时间并标出偏移
fn format_at(secs: u64, offset: i64) -> String {
    let local = u64::try_from(i64::try_from(secs).unwrap_or(i64::MAX).saturating_add(offset)).unwrap_or(0);
    let (sign, abs) = if offset < 0 { ('-', -offset) } else { ('+', offset) };
    format!("{} {sign}{:02}:{:02}", format_utc(local), abs / 3600, abs % 3600 / 60)
}

pub fn format_utc(secs: u64) -> String {
    let (days, rem) = (secs / 86400, secs % 86400);
    // civil_from_days（Howard Hinnant）
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_time_carries_its_offset() {
        assert_eq!(format_at(1_793_448_000, 8 * 3600), "2026-10-31 20:00:00 +08:00");
        assert_eq!(format_at(1_793_448_000, 0), "2026-10-31 12:00:00 +00:00");
        assert_eq!(format_at(1_793_448_000, -(5 * 3600 + 1800)), "2026-10-31 06:30:00 -05:30");
        // just after midnight UTC, west of Greenwich: the day before
        assert_eq!(format_at(1_793_404_860, -7 * 3600), "2026-10-30 17:01:00 -07:00");
        // whatever this machine's zone is, the shape is the same
        assert!(regex_like(&format_local(1_793_448_000)), "{}", format_local(1_793_448_000));
    }

    /// `YYYY-MM-DD HH:MM:SS ±HH:MM`
    fn regex_like(s: &str) -> bool {
        let b = s.as_bytes();
        b.len() == 26
            && [4, 7].iter().all(|&i| b[i] == b'-')
            && b[10] == b' '
            && [13, 16, 23].iter().all(|&i| b[i] == b':')
            && b[19] == b' '
            && matches!(b[20], b'+' | b'-')
            && s.chars().filter(char::is_ascii_digit).count() == 18
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
        assert_eq!(format_utc(1_793_448_000), "2026-10-31 12:00:00");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00:00");
    }

    #[test]
    fn rotates_when_too_big_and_tails() {
        let dir = std::env::temp_dir().join(format!("ictl-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = EventLog::with_max(&dir, 64);
        for i in 0..10 {
            log.log(&format!("第 {i} 行"));
        }
        assert!(dir.join("daemon.log.1").exists());
        let tail = log.tail(5);
        assert!(!tail.is_empty() && tail.len() <= 3, "轮转后当前文件只留最近几行：{tail:?}");
        assert!(tail.last().unwrap().ends_with("第 9 行"));
        assert!(std::fs::metadata(log.path()).unwrap().len() < 64 + 40);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
