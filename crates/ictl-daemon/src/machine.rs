//! 状态机的纯逻辑部分：设备状态、WDA 启动退避、usbmuxd 漏设备判定。可单测，不碰设备。

use std::time::{Duration, Instant};

/// 设备状态（docs/architecture.md §3）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeviceState {
    /// usbmuxd 看不到手机
    Absent,
    /// 没有配对记录，需要在手机上点「信任」（iphone-control setup）
    NeedsTrust,
    DeveloperModeOff,
    /// 锁屏时建不了隧道（BFU 只能人工解锁）
    NeedsUnlock,
    /// 还没签名安装 WDA（iphone-control sign）
    NeedsSigning,
    /// 正在建隧道、挂镜像、启动 WDA，或启动失败在退避
    WdaStarting {
        last_error: Option<String>,
    },
    /// 手机重启后首次启用 UI 自动化，等用户在手机上输锁屏密码
    NeedsAutomationPin,
    Ready,
}

impl DeviceState {
    pub fn describe(&self) -> String {
        match self {
            Self::Absent => "手机未连接".into(),
            Self::NeedsTrust => "未配对：运行 iphone-control setup，在手机上点「信任」".into(),
            Self::DeveloperModeOff => "开发者模式未打开：运行 iphone-control setup".into(),
            Self::NeedsUnlock => "手机锁屏，隧道建不起来：请解锁手机".into(),
            Self::NeedsSigning => "WDA 还没签名安装：运行 iphone-control sign".into(),
            Self::WdaStarting { last_error: None } => "正在启动 WDA".into(),
            Self::WdaStarting { last_error: Some(e) } => format!("WDA 启动失败，稍后重试：{e}"),
            Self::NeedsAutomationPin => "WDA 卡在手机上的「Enable UI Automation」密码框：请在手机上输入锁屏密码".into(),
            Self::Ready => "就绪".into(),
        }
    }
}

/// WDA 启动失败退避：60s 起翻倍，上限 300s；等自动化密码时固定 60s；手机重新连上时重置
#[derive(Debug, Clone)]
pub struct Backoff {
    next_at: Option<Instant>,
    delay: Duration,
}

pub const BACKOFF_START: Duration = Duration::from_secs(60);
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);
pub const PIN_RETRY: Duration = Duration::from_secs(60);

impl Default for Backoff {
    fn default() -> Self {
        Self { next_at: None, delay: BACKOFF_START }
    }
}

impl Backoff {
    pub fn ready(&self, now: Instant) -> bool {
        self.next_at.is_none_or(|t| now >= t)
    }

    pub fn failed(&mut self, now: Instant) {
        self.next_at = Some(now + self.delay);
        self.delay = (self.delay * 2).min(BACKOFF_MAX);
    }

    pub fn waiting_for_pin(&mut self, now: Instant) {
        self.next_at = Some(now + PIN_RETRY);
        self.delay = BACKOFF_START;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn current_delay(&self) -> Duration {
        self.delay
    }
}

/// 容器里没有 udev，usbmuxd 收不到热插拔：sysfs 上有 Apple 设备但 usbmuxd 看不到，连续 3 次就该重启它
#[derive(Debug, Default)]
pub struct MuxWatch {
    misses: u32,
}

pub const USBMUXD_MISS_LIMIT: u32 = 3;

impl MuxWatch {
    /// 返回 true 表示该重启 usbmuxd 了
    pub fn observe(&mut self, usb_present: bool, mux_sees_device: bool) -> bool {
        if mux_sees_device || !usb_present {
            self.misses = 0;
            return false;
        }
        self.misses += 1;
        if self.misses >= USBMUXD_MISS_LIMIT {
            self.misses = 0;
            return true;
        }
        false
    }
}

/// WDA 连续几次探测不通才算挂（避免一次慢请求就重启它）
#[derive(Debug, Default)]
pub struct Misses(u32);

impl Misses {
    pub const LIMIT: u32 = 3;

    pub fn hit(&mut self) -> bool {
        self.0 += 1;
        self.0 >= Self::LIMIT
    }

    pub fn reset(&mut self) {
        self.0 = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_cap_and_resets() {
        let t = Instant::now();
        let mut b = Backoff::default();
        assert!(b.ready(t));
        let delays: Vec<_> = (0..5)
            .map(|_| {
                b.failed(t);
                b.next_at.unwrap() - t
            })
            .collect();
        assert_eq!(delays.iter().map(|d| d.as_secs()).collect::<Vec<_>>(), [60, 120, 240, 300, 300]);
        assert!(!b.ready(t + Duration::from_secs(10)));
        b.reset();
        assert!(b.ready(t));
    }

    /// 回归用例 #8：等自动化密码时固定 60 秒重试，不指数退避
    #[test]
    fn pin_wait_retries_every_minute() {
        let t = Instant::now();
        let mut b = Backoff::default();
        b.failed(t);
        b.failed(t);
        b.waiting_for_pin(t);
        assert!(!b.ready(t + Duration::from_secs(59)));
        assert!(b.ready(t + Duration::from_secs(60)));
        assert_eq!(b.current_delay(), BACKOFF_START);
    }

    /// 回归用例 #7：sysfs 有设备但 usbmuxd 看不到，连续 3 次才重启
    #[test]
    fn usbmuxd_restart_after_three_misses() {
        let mut w = MuxWatch::default();
        assert!(!w.observe(true, false));
        assert!(!w.observe(true, false));
        assert!(w.observe(true, false));
        assert!(!w.observe(true, false), "重启后重新计数");
        assert!(!w.observe(false, false), "USB 上没有设备不算漏");
        assert!(!w.observe(true, true));
    }

    #[test]
    fn misses_need_three_in_a_row() {
        let mut m = Misses::default();
        assert!(!m.hit());
        assert!(!m.hit());
        m.reset();
        assert!(!m.hit());
        assert!(!m.hit());
        assert!(m.hit());
    }

    #[test]
    fn state_serializes_with_tag() {
        let s = serde_json::to_string(&DeviceState::WdaStarting { last_error: None }).unwrap();
        assert_eq!(s, r#"{"state":"wda_starting","last_error":null}"#);
    }
}
