//! 守护进程：唯一持有设备的进程。替代 toolkit 的 supervisor.sh + resign 定时器 + notify.sh。
//!
//! 设备状态机（docs/architecture.md §3）：
//! Absent → Attached → Paired → Tunneling → DdiCheck → WdaStarting → Ready
//! 以及 NeedsTrust / NeedsUnlock（BFU）/ NeedsAutomationPin 三个等人的状态。

pub mod protocol;

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeviceState {
    Absent,
    Attached,
    NeedsTrust,
    Paired,
    Tunneling,
    NeedsUnlock,
    DdiCheck,
    WdaStarting,
    NeedsAutomationPin,
    Ready { locked: bool },
}

/// usbmuxd 看护：sysfs 有 Apple 设备但 usbmuxd 看不到，连续这么多次就重启 usbmuxd
pub const USBMUXD_MISS_LIMIT: u32 = 3;
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// WDA 启动失败退避：60s 起翻倍，上限 300s；等自动化密码时固定 60s
pub const WDA_BACKOFF_START: Duration = Duration::from_secs(60);
pub const WDA_BACKOFF_MAX: Duration = Duration::from_secs(300);
pub const AUTOMATION_PIN_RETRY: Duration = Duration::from_secs(60);
/// 描述文件剩余少于这个就重签
pub const RESIGN_THRESHOLD: Duration = Duration::from_secs(2 * 24 * 3600);

pub struct Config {
    pub data_dir: PathBuf,
    pub udid: Option<String>,
    pub wda_bundle_id: Option<String>,
    pub auto_unlock: bool,
}

/// 需要推送的事件；同一事件去重，恢复时推送一次
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    ResignFailed(String),
    SessionExpired,
    NeedsAutomationPin,
    WdaRecovered,
    UnlockDisabled,
    DeviceOffline,
    DeviceOnline,
}

pub trait Notifier: Send + Sync {
    fn notify(&self, event: &Event) -> impl std::future::Future<Output = ()> + Send;
}

/// 运行守护进程直到收到停止信号
pub async fn run(config: Config) -> anyhow::Result<()> {
    let _ = config;
    todo!("状态机循环、usbmuxd 子进程、控制接口、重签调度")
}

