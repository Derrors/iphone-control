//! 守护进程核心：唯一持有设备的进程。共享状态、请求分发、快照。
//!
//! 并发模型（全部跑在主循环的 LocalSet 里，单线程协作）：
//! - 设备相关的状态在 `Inner` 里，一把锁串行访问：HID 会话、WDA 会话只能有一个持有者，
//!   也不会出现 toolkit 那种「长请求期间被巡检误判 WDA 挂了」的问题
//! - 每个控制请求一个任务，拿到锁才执行；巡检（`crate::supervise`）拿不到锁就跳过这一轮
//! - 耗时的事不持锁：启动 WDA（含挂开发者镜像，最长约 2 分钟）、重签、会话检查在后台跑，做完再持锁写结果
//! - `status` 读发布出来的快照（watch 通道），不碰锁
//! - 改变手机状态的请求排队超过 [`STALE`] 就放弃，免得点击落到已经变了的屏幕上
//!
//! 分在几个文件里：这里是状态和设备会话的基本操作，`persist` 是存到 `state.json` 的状态和签名描述文件，`dispatch` 是
//! 请求的分流、准入和执行

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ictl_device::{AppInfo, Device, DeviceError, DeviceInfo, Rotation, ScreenSize, Tunnel, UsbDevice};
use ictl_unlock::UnlockConfig;
use ictl_wda::{WdaClient, WdaRunner, http::DeviceTransport};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::eventlog::EventLog;
use crate::machine::{Backoff, DeviceState, Misses};
use crate::notify::{self, Dedup, Event};
use crate::protocol::{Delivery, FailureStage, Request, Response};
use crate::supervise::StartOutcome;
use crate::watchdog::Heartbeat;

pub const TICK: Duration = Duration::from_secs(10);
/// 改变手机状态的请求排队超过这么久就放弃
pub const STALE: Duration = Duration::from_secs(30);
/// 长按、滑动的时长上限（秒）：HID 拖动按时长换算采样点，不设上限一个请求就能占住设备很久
pub const MAX_GESTURE_SECONDS: f64 = 10.0;
/// 等界面稳定、等元素出现的上限（毫秒）：持着设备锁等，不能太久
pub const MAX_WAIT_MS: u64 = 30_000;
/// Longest a JavaScript evaluation in a page may take (`WebEval`)
pub const MAX_WEB_EVAL_MS: u64 = 60_000;

mod dispatch;
mod persist;
#[cfg(test)]
pub(crate) mod tests;
mod unlock;

pub(crate) use dispatch::in_background;
pub use persist::{ButtonWays, DetachedWda, DeviceProfile, Persisted, ResignRecord, wda_profile, wda_profile_expiry};

/// 一次请求的附带条件（来自请求里的 [`ictl_protocol::CANCEL_ON_CLOSE`]、[`ictl_protocol::EXPECT_UDID`]）
#[derive(Default)]
pub struct CallCtx {
    /// 调用方断开时收到信号。还在排队等设备锁的请求就不执行了；
    /// 已经开始执行的照常做完（中途打断会留下改了一半的 WDA 设置）
    pub gone: Option<tokio::sync::oneshot::Receiver<()>>,
    /// 正在控制的不是这台就不执行
    pub expect_udid: Option<String>,
    /// Identity of the measured display used for this coordinate.
    pub expect_view: Option<String>,
    /// The token of the task this request belongs to ([`ictl_protocol::TASK`])
    pub task: Option<String>,
    /// The task's step it belongs to ([`ictl_protocol::TASK_STEP`])
    pub task_step: Option<u64>,
    /// The process that sent it (the socket's peer), when the system says
    pub peer: Option<u32>,
    /// A poll that gives way to actions ([`ictl_protocol::BACKGROUND`])
    pub background: bool,
    /// A screen stream for a page in low-bandwidth mode ([`ictl_protocol::LOW_BANDWIDTH`])
    pub low_bandwidth: bool,
    /// Someone watches the screen ([`ictl_protocol::WATCHED`])
    pub watched: bool,
}

/// Logical points measured on this connection. A different id invalidates queued coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayGeometry {
    pub id: String,
    pub width: f64,
    pub height: f64,
    pub rotation: u16,
}

impl DisplayGeometry {
    pub(crate) fn update(current: &mut Option<Self>, measurement: Option<(ScreenSize, Rotation)>) -> bool {
        let Some((size, rotation)) = measurement else { return current.take().is_some() };
        let width = size.width;
        let height = size.height;
        let rotation = rotation.degrees();
        if current.as_ref().is_some_and(|v| (v.width, v.height, v.rotation) == (width, height, rotation)) {
            return false;
        }
        *current = Some(Self { id: uuid::Uuid::new_v4().to_string(), width, height, rotation });
        true
    }
}

/// 快照：`iphone-control status` 显示
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Older daemons do not report process identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<bool>,
    pub state: DeviceState,
    pub device: Option<DeviceInfo>,
    /// USB 上接着的全部设备（读不到名字的只有 UDID）
    #[serde(default)]
    pub devices: Vec<DeviceInfo>,
    #[serde(default)]
    pub display: Option<DisplayGeometry>,
    /// 选定的设备：`IPHONE_UDID` 或运行中选的；None 是自动选
    #[serde(default)]
    pub pinned: Option<String>,
    #[serde(default)]
    pub pinned_by_env: bool,
    pub persisted: Persisted,
    /// 当前这台手机上的 WDA 描述文件
    #[serde(default)]
    pub device_profile: DeviceProfile,
    /// 当前这台手机原生控制（iOS 27 起默认，不用 WDA）。旧的守护进程不报，当作 false
    #[serde(default)]
    pub native: bool,
    pub auto_unlock: bool,
    pub unlock_disabled: bool,
    pub notify_configured: bool,
    pub log_tail: Vec<String>,
}

/// What is known about the device being controlled. Replaced as a whole when that device goes away or changes
/// ([`Daemon::forget_device`]), so a new field can't be left holding the previous device's value
#[derive(Default)]
pub(crate) struct PerDevice {
    pub display: Option<DisplayGeometry>,
    /// Last complete measurement; passive partial samples never extend this window.
    pub display_verified_at: Option<Instant>,
    /// 这台手机上的 WDA 描述文件
    pub device_profile: DeviceProfile,
    /// 那份描述文件里没有本机签名私钥的证书（Some(true)）：WDA 是别的机器、旧的数据目录签的，或本机的证书是后来才
    /// 新建的。这时 WDA 起不来多半是签它的证书被吊销了，用本机的证书重签就好（[`Daemon::heal_certificate`]）。
    /// None：不知道（没读到描述文件、本机还没签过名）
    pub foreign_wda: Option<bool>,
    /// 最近一次由 WDA 或 SpringBoard 确认的界面旋转及读取时间。只在短暂掉线时给 HID 使用，不持久化
    pub last_rotation: Option<(Instant, Rotation)>,
    /// 开发者镜像连续挂载失败次数，到 3 次清缓存重新下载
    pub ddi_failures: u32,
    /// 原生控制就绪后，上次查开发者镜像和无障碍服务还在不在的时间，和接连几次没查成（[`crate::supervise::native::Liveness`]）
    pub alive_checked: Option<Instant>,
    pub alive_misses: u32,
    /// 最近一次经本进程锁屏的时间（自动解锁要等锁屏过渡走完，见 [`crate::tools::LOCK_SETTLE`]）
    pub last_lock: Option<Instant>,
    /// The orientation of the finger the web viewer is moving (`Request::Touch`), worked out when it touched down
    pub touch_rotation: Option<Rotation>,
    pub apps: HashMap<bool, Vec<AppInfo>>,
    /// Log capture for `AppLogs` (developer tools), started by the first request
    pub log_capture: Option<crate::devtools::Capture>,
    /// HID 粘贴前存下的剪贴板和粘贴的时间，等着写回（[`Daemon::restore_pasteboard`]）
    pub pending_paste: Option<(Instant, ictl_device::PendingRestore)>,
    /// 上一个操作做完以后读过的界面（元素树、截图的指纹）
    pub seen: crate::tools::Seen,
    /// 最近一个操作开始前的界面：操作做完、界面稳定时和它比，告诉调用方界面变没变（`Daemon::settle`）。
    /// 按名字点击会在点之前读树，用那棵树
    pub before_action: Option<crate::tools::Seen>,
    /// 最近一个操作原样是什么（请求的 JSON），和它连续几次做完界面都没变：同一个操作原样重做、界面还是不变，
    /// 结果里说是第几次了（[`crate::tools::Repeats`]）
    pub repeats: crate::tools::Repeats,
    /// 最近一个操作开始的时间：原生控制等界面稳定时，从这之后的推送算起
    pub acted_at: Option<Instant>,
    /// 原生控制：上次因为无障碍服务不应答而重启手机上 axauditd 的时间（`tools::native::AX_RESTART_EVERY` 内只重启一次）
    pub ax_restarted: Option<Instant>,
    /// How long the request being handled spent unlocking the phone first ([`Daemon::ensure_unlocked`]); taken by
    /// [`Daemon::dispatch`], which says so in the answer
    pub unlocked: Option<Duration>,
    /// 原生控制：进程号 → Bundle ID（前台 App 由无障碍服务给进程号）
    pub bundles: PidCache<String>,
    /// 原生控制：进程号 → 是不是网页内容进程（WebKit WebContent，网页的元素来自它）
    pub web_pids: PidCache<bool>,
}

/// What a process of the phone is, by its number, for a while ([`PidCache::FOR`]). A number is given to another
/// process only once the phone has gone through all of them (iOS: up to 99999), which takes far longer than that; kept
/// for good, an entry could name an app that is long gone, and a task's scope would go by it (a daemon runs for weeks)
#[derive(Debug)]
pub struct PidCache<V> {
    entries: HashMap<i64, (Instant, V)>,
}

impl<V> Default for PidCache<V> {
    fn default() -> Self {
        Self { entries: HashMap::new() }
    }
}

impl<V> PidCache<V> {
    pub const FOR: Duration = Duration::from_secs(10 * 60);
    /// More than this and it starts again (processes come and go)
    const MAX: usize = 200;

    pub fn get(&self, pid: i64) -> Option<&V> {
        self.get_at(pid, Instant::now())
    }

    fn get_at(&self, pid: i64, now: Instant) -> Option<&V> {
        self.entries.get(&pid).filter(|(at, _)| now.saturating_duration_since(*at) < Self::FOR).map(|(_, v)| v)
    }

    pub fn insert(&mut self, pid: i64, v: V) {
        self.insert_at(pid, v, Instant::now());
    }

    fn insert_at(&mut self, pid: i64, v: V, now: Instant) {
        // an entry renewed takes no room
        if self.entries.len() >= Self::MAX && !self.entries.contains_key(&pid) {
            self.entries.retain(|_, (at, _)| now.saturating_duration_since(*at) < Self::FOR);
            if self.entries.len() >= Self::MAX {
                self.entries.clear();
            }
        }
        self.entries.insert(pid, (now, v));
    }

    pub fn remove(&mut self, pid: i64) {
        self.entries.remove(&pid);
    }
}

/// What preparing a phone for native control gives: the developer disk image's version if it was mounted just now,
/// and what became of the WDA launched in the background earlier (if one was recorded for it); failed, whether it was
/// the mount itself
pub(crate) type Prepared =
    std::result::Result<(Option<String>, Option<(String, crate::detached::StoppedWda)>), (DeviceError, bool)>;

/// A read taken as a swiped list stopped ([`Inner::swiped_tree`])
pub(crate) struct Swiped<T> {
    pub at: Instant,
    pub read: T,
    /// Two reads in a row agreed the list stood still. A screen with few elements isn't waited for: its single read
    /// only serves as a starting point for waiting, not as the screen now
    pub still: bool,
}

impl<T> Swiped<T> {
    pub fn now(read: T, still: bool) -> Self {
        Self { at: Instant::now(), read, still }
    }
}

pub(crate) struct Inner {
    pub persisted: Persisted,
    pub dev: PerDevice,
    pub state: DeviceState,
    pub device: Option<Device>,
    pub info: Option<DeviceInfo>,
    /// 最近一轮巡检时 USB 上接着的全部设备
    pub connected: Vec<UsbDevice>,
    /// 每次 USB 接入的设备信息；字段不齐时限制重试频率
    pub infos: HashMap<String, crate::devices::CachedInfo>,
    /// HID 和截图用的隧道（HID 会话归它所有）
    pub tunnel: Option<Tunnel>,
    pub runner: Option<WdaRunner>,
    pub wda: Option<WdaClient<DeviceTransport>>,
    /// 后台启动 WDA 的任务（含挂开发者镜像），巡检里收结果
    pub starting: Option<JoinHandle<StartOutcome>>,
    pub resigning: bool,
    pub wda_misses: Misses,
    pub backoff: Backoff,
    /// 连续列不出设备的次数（usbmuxd 抖一下不立刻当成离线）
    pub list_errors: u32,
    pub offline_since: Option<Instant>,
    /// The device that went offline, for the notification once it has been gone long enough (its details are gone)
    pub offline_target: Option<crate::notify::Target>,
    /// HID 会话最近一次使用的时间；空闲超过 [`crate::supervise::HID_IDLE`] 就关掉（它持有屏幕媒体流）
    pub hid_used: Option<Instant>,
    /// The marker of the HID session open now ([`crate::hidmark`]), removed when it closes properly
    pub hid_marker: Option<std::path::PathBuf>,
    /// Phones an earlier daemon left "viewed remotely" (it ended with a HID session open), and the tries at clearing
    /// that so far ([`crate::hidmark`])
    pub hid_orphans: HashMap<String, u32>,
    /// 原生控制：无障碍会话最近一次使用的时间；空闲超过 [`crate::supervise::AX_IDLE`] 就关掉
    pub ax_used: Option<Instant>,
    /// 原生控制：后台挂开发者镜像（结果：挂上的版本，已挂载为 None；失败时带上「是不是挂载这一步」）
    pub preparing: Option<JoinHandle<Prepared>>,
    /// Control pages watching the screen media stream (`screen.rs`): while there are any, the HID session (which owns
    /// the stream) stays open however long it is idle
    pub screen_viewers: usize,
    /// Of those, the pages in low-bandwidth mode ([`ictl_protocol::LOW_BANDWIDTH`]): while there are any, the HID
    /// session's video is capped at [`ictl_device::LOW_BANDWIDTH_BITRATE`]
    pub low_bandwidth_viewers: usize,
    /// 最近一次读到的元素树。只在调用方允许（max_age_ms）时复用；改变界面的操作做完就清掉
    pub elements_cache: Option<(Instant, ictl_ui::Tree)>,
    /// 滑动等列表停下时最后读到的树（[`crate::tools`] 的 `settle` 拿来当比较的起点）
    pub swiped_tree: Option<Swiped<ictl_ui::Tree>>,
    /// The same under native control: the frontmost app's last read once the list stopped, without what SpringBoard
    /// shows over it (that is read once by whoever uses it)
    pub swiped_snap: Option<Swiped<ictl_device::ax::AxScreen>>,
    pub dedup: Dedup,
    /// 各设备最近一次因为证书自动重签的时间（按 UDID，换设备、重插也记着）：[`crate::supervise::resign::CERT_HEAL_EVERY`] 内
    /// 不再自动重签同一台，免得重签也治不好时反复签
    pub cert_heals: HashMap<String, Instant>,
    /// 本机的签名证书还在不在团队里，和查的时间（会话检查、自动重签前联网查；doctor 显示它，不在持锁时联网）
    pub own_cert: Option<(Instant, bool)>,
    pub next_resign: Instant,
    pub next_session_check: Instant,
}

pub struct Daemon {
    pub cfg: Config,
    pub log: EventLog,
    pub(crate) inner: Mutex<Inner>,
    snap: watch::Sender<Snapshot>,
    /// The kept Web Inspector connection (developer tools), separate from `inner` so a slow page doesn't hold up
    /// taps and screenshots; see [`crate::web`]
    pub(crate) web: Mutex<Option<crate::web::WebConn>>,
    pub(crate) heartbeat: crate::watchdog::Heartbeat,
    /// AI tasks: who may act on the phone while one runs (see [`crate::tasks`])
    pub(crate) tasks: crate::tasks::Tasks,
    /// A supervision round is due now, not at the next [`TICK`]: starting WDA finished, so its outcome is taken
    /// (ready seconds sooner after a restart or a hang)
    pub(crate) wake: Arc<tokio::sync::Notify>,
    /// Actions waiting for the device lock: a background check doesn't start while there are any
    /// ([`ictl_protocol::BACKGROUND`])
    waiting: std::sync::atomic::AtomicUsize,
    /// An action started waiting: a background check that runs gives the phone up
    give_way: tokio::sync::Notify,
    /// When the last action was done: background checks wait for a quiet moment (`dispatch::quiet_before`)
    last_action: std::sync::Mutex<Option<Instant>>,
    /// Details of devices other than the controlled one, read in the background (a lockdown query can take up to
    /// 15 s, which the device lock shouldn't wait for); taken into [`Inner::infos`] at the next round
    pub(crate) probed: crate::devices::Probed,
    /// The interface rotation (degrees) for the pages watching the screen video, sampled once for all of them
    /// while any watches (see [`crate::screen`])
    pub(crate) rotation: watch::Sender<Option<u32>>,
    /// The rotation sampler is running
    pub(crate) rotation_sampling: std::sync::atomic::AtomicBool,
}

impl Daemon {
    /// Whether an action was done in the last `within`, or one waits for the device lock: slow background requests
    /// to WDA wait for a quiet moment ([`ictl_protocol::BACKGROUND`], the screen size in `sample_display`)
    pub(crate) fn acting_now(&self, within: Duration) -> bool {
        self.waiting.load(std::sync::atomic::Ordering::SeqCst) > 0
            || self.last_action.lock().unwrap_or_else(|p| p.into_inner()).is_some_and(|t| t.elapsed() < within)
    }
}

/// 停掉 HID 会话（屏幕媒体流），隧道保留。媒体流不停的话手机一直以为「配对的电脑」在看屏幕：
/// 灵动岛常驻图标、不会自动锁屏，本进程退出、拔插数据线都清不掉（iOS 27 真机）
pub(crate) async fn close_hid(g: &mut Inner) {
    g.dev.touch_rotation = None;
    if let Some(t) = g.tunnel.as_mut() {
        t.close_hid().await;
    }
    hid_closed(g);
}

/// The HID session was closed properly: its marker goes, and with it any state an earlier daemon left on that phone
/// (closing a session clears it, see [`crate::hidmark`])
fn hid_closed(g: &mut Inner) {
    if let Some(p) = g.hid_marker.take() {
        if let Some(udid) = p.file_name().and_then(|n| n.to_str()) {
            g.hid_orphans.remove(udid);
        }
        let _ = std::fs::remove_file(p);
    }
}

/// 丢掉隧道。一律走这里：先停 HID 媒体流再断（见 [`close_hid`]）
pub(crate) async fn drop_tunnel(g: &mut Inner) {
    g.dev.touch_rotation = None;
    if let Some(t) = g.tunnel.take() {
        // boxed: the future holds the whole tunnel, and every tool that may drop it would carry that
        Box::pin(t.close()).await;
    }
    hid_closed(g);
}

/// 一台设备的简短说明：`名字(型号 iOS 版本,UDID)`，英文是 `name (model iOS version, UDID)`；读不到名字时只有 UDID
pub fn device_label(i: &DeviceInfo) -> String {
    if i.name.is_empty() && i.product_type.is_empty() {
        i.udid.clone()
    } else {
        let (name, model, version, udid) = (&i.name, &i.product_type, &i.product_version, &i.udid);
        ictl_l10n::trf!("{name}({model} iOS {version},{udid})", "{name} ({model} iOS {version}, {udid})")
    }
}

/// WDA 不可用时的说明。HID 后备路线靠 iOS 27 起才有的 CoreDevice 屏幕服务，低版本不能说「照常用」
pub(crate) fn wda_down(state: &DeviceState, info: Option<&DeviceInfo>) -> String {
    // 没接设备时 WDA 不会自己起来，也没有 HID 后备
    if matches!(state, DeviceState::Absent) {
        return ictl_l10n::tr!(
            "WDA 不可用(手机未连接);接上设备后守护进程会自动拉起 WDA",
            "WDA unavailable (phone not connected); the daemon starts WDA once a device is plugged in"
        )
        .into();
    }
    // 原生控制（iOS 27 起不用 WDA）：只是还没准备好
    if let DeviceState::Preparing { .. } = state {
        return ictl_l10n::trf!(
            "手机还没准备好({}),通常几秒到半分钟;截图、点击、滑动、按键可照常用",
            "The phone isn't ready yet ({}), usually a few seconds to half a minute; screenshots, taps, swipes and buttons still work",
            state.describe()
        );
    }
    let fallback = if hid_supported(info) {
        ictl_l10n::tr!("截图/点击/滑动/按键可照常用", "screenshots/taps/swipes/buttons still work")
    } else {
        ictl_l10n::tr!(
            "这台手机 iOS 低于 27,没有 HID 后备路线,WDA 恢复前截图、点击等操作都用不了",
            "this phone runs iOS below 27 and has no HID fallback, so screenshots, taps etc. don't work until WDA is back"
        )
    };
    // 状态还是「就绪」：WDA 在两次巡检之间刚掉线，下一轮巡检才会发现并重新拉起
    let why = if *state == DeviceState::Ready {
        ictl_l10n::tr!("刚掉线", "just dropped").to_owned()
    } else {
        state.describe()
    };
    ictl_l10n::trf!(
        "WDA 不可用({why}),守护进程一般 1~2 分钟内会拉起;{fallback}",
        "WDA unavailable ({why}), the daemon usually restarts it within 1-2 minutes; {fallback}"
    )
}

/// How an action reaches the device in use, decided once per action ([`Daemon::route`]). An action that has a way for
/// each matches all four (no `_`): a new action then has to say what it does natively and with WDA down too, which
/// is where a forgotten branch used to hide.
///
/// Not for fallback chains: an action that tries WDA and goes on over HID when WDA turns out unreachable (taps,
/// swipes, typing, buttons, screenshots) starts with `self.wda(g)` and falls through, since the route was WDA when it
/// started. Nor for questions only WDA answers (its snapshot limits, `doctor`)
pub(crate) enum Route<'a> {
    /// WDA is up
    Wda(&'a mut WdaClient<DeviceTransport>),
    /// iOS 27 and later driven natively, without WDA
    Native,
    /// WDA is down on a device that has the HID fallback (iOS 27 and later set to use WDA): screenshots, touches and
    /// buttons go over HID
    Hid,
    /// WDA is down and there is no HID fallback (below iOS 27)
    Unavailable,
}

/// The error for an action that needs WDA while it is down ([`wda_down`])
pub(crate) fn wda_unavailable(g: &Inner) -> crate::error::Error {
    wda_down(&g.state, g.info.as_ref()).into()
}

/// HID 后备路线（免 WDA 截图、点击）只有 iOS 27 起能用；不知道版本时照常尝试
pub(crate) fn hid_supported(info: Option<&DeviceInfo>) -> bool {
    info.is_none_or(|i| matches!(i.major_version(), 0 | 27..))
}

impl Daemon {
    pub fn new(cfg: Config) -> Arc<Self> {
        let log = EventLog::new(&cfg.logs());
        unlock::migrate_unlock_disabled(&cfg, &log);
        let persisted = Persisted::load(&cfg.state_file());
        let now = Instant::now();
        let inner = Inner {
            persisted,
            dev: PerDevice::default(),
            state: DeviceState::Absent,
            device: None,
            info: None,
            connected: Vec::new(),
            infos: HashMap::new(),
            tunnel: None,
            runner: None,
            wda: None,
            starting: None,
            resigning: false,
            wda_misses: Misses::default(),
            backoff: Backoff::default(),
            list_errors: 0,
            offline_since: Some(now),
            offline_target: None,
            hid_used: None,
            hid_marker: None,
            hid_orphans: crate::hidmark::left_over(&cfg.hid_marker_dir()),
            ax_used: None,
            preparing: None,
            screen_viewers: 0,
            low_bandwidth_viewers: 0,
            elements_cache: None,
            swiped_tree: None,
            swiped_snap: None,
            dedup: Dedup::default(),
            cert_heals: HashMap::new(),
            own_cert: None,
            // 启动一分钟后再做第一次重签检查和会话验证，先让设备就绪
            next_resign: now + Duration::from_secs(60),
            next_session_check: now + Duration::from_secs(90),
        };
        let first = Snapshot {
            version: Some(env!("CARGO_PKG_VERSION").into()),
            pid: Some(std::process::id()),
            maintenance: None,
            state: DeviceState::Absent,
            device: None,
            devices: Vec::new(),
            display: None,
            pinned: cfg.udid.clone().or_else(|| inner.persisted.selected_udid.clone()),
            pinned_by_env: cfg.udid.is_some(),
            persisted: inner.persisted.clone(),
            device_profile: DeviceProfile::NotChecked,
            native: false,
            auto_unlock: false,
            unlock_disabled: cfg
                .unlock_disabled()
                .device(cfg.udid.as_deref().or(inner.persisted.selected_udid.as_deref()).unwrap_or(unlock::NO_DEVICE))
                .is_disabled(),
            notify_configured: cfg.notify.any(),
            log_tail: Vec::new(),
        };
        let (snap, _) = watch::channel(first);
        let tasks = crate::tasks::Tasks::load(Some(cfg.data_dir.join("tasks")));
        Arc::new(Self {
            cfg,
            log,
            inner: Mutex::new(inner),
            snap,
            web: Mutex::new(None),
            heartbeat: Heartbeat::new(),
            tasks,
            wake: Arc::default(),
            waiting: std::sync::atomic::AtomicUsize::new(0),
            give_way: tokio::sync::Notify::new(),
            last_action: std::sync::Mutex::new(None),
            probed: Default::default(),
            rotation: watch::channel(None).0,
            rotation_sampling: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// 选定的设备：`IPHONE_UDID` 优先，其次是运行中选的
    pub(crate) fn pinned(&self, g: &Inner) -> Option<String> {
        self.cfg.udid.clone().or_else(|| g.persisted.selected_udid.clone())
    }

    /// 接着的全部设备，读不到名字的只有 UDID
    pub(crate) fn connected_infos(&self, g: &Inner) -> Vec<DeviceInfo> {
        g.connected
            .iter()
            .map(|d| {
                g.infos
                    .get(&d.udid)
                    .and_then(|entry| entry.value.clone())
                    .unwrap_or_else(|| DeviceInfo { udid: d.udid.clone(), ..Default::default() })
            })
            .collect()
    }

    pub(crate) fn notify(&self, g: &mut Inner, e: Event) {
        if !g.dedup.should_send(&e, Instant::now()) {
            return;
        }
        // 通知和事件日志按守护进程自己的语言：触发它的可能是另一种语言的请求（例如自动解锁失败）
        // the device it is about; one that went offline has lost its details, so the one kept then
        let target = match (&e, &g.offline_target) {
            (Event::DeviceOffline { .. }, Some(t)) => t.clone(),
            _ => ictl_l10n::in_own_lang(|| notify::Target::of(g.info.as_ref())),
        };
        let (title, body) = ictl_l10n::in_own_lang(|| e.title_body(&target));
        self.log.log(&ictl_l10n::log_trf!("通知：{title}", "Notification: {title}"));
        let cfg = self.cfg.notify.clone();
        if cfg.any() {
            tokio::spawn(async move {
                notify::send(&cfg, &title, &body).await;
            });
        }
    }

    pub(crate) fn set_state(&self, g: &mut Inner, s: DeviceState) {
        if g.state != s {
            self.log.log(&ictl_l10n::log_trf!("状态：{}", "State: {}", ictl_l10n::in_own_lang(|| s.describe())));
            g.state = s;
        }
    }

    /// 重读这台手机上的 WDA 描述文件（misagent，一次约几十毫秒）
    pub(crate) async fn refresh_device_profile(&self, g: &mut Inner) {
        let Some(d) = g.device.clone() else {
            g.dev.device_profile = DeviceProfile::NotChecked;
            return;
        };
        let Some(bundle) = g.persisted.wda_bundle.clone() else {
            g.dev.device_profile = DeviceProfile::Missing;
            return;
        };
        g.dev.foreign_wda = None;
        g.dev.device_profile = match d.installed_profiles().await {
            Ok(ps) => match wda_profile(&ps, g.persisted.team_id.as_deref(), &bundle, &d.udid) {
                Some(p) => {
                    let keys = self.account().local_public_keys();
                    g.dev.foreign_wda = (!keys.is_empty()).then(|| !p.signed_with_any(&keys));
                    DeviceProfile::Expires(p.expires.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
                }
                None => DeviceProfile::Missing,
            },
            Err(e) => {
                tracing::warn!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "读手机上的描述文件失败，按签名记录算：{e}",
                        "Reading the provisioning profiles on the phone failed, going by the signing record: {e}"
                    )
                );
                DeviceProfile::Unknown(e.to_string())
            }
        };
    }

    /// 丢掉整个会话：runner、WDA 客户端、正在后台启动的任务、隧道（先停 HID 媒体流）
    pub(crate) async fn drop_session(&self, g: &mut Inner) {
        if let Some(h) = g.starting.take() {
            h.abort();
        }
        if let Some(h) = g.preparing.take() {
            h.abort();
        }
        if let Some(r) = g.runner.take() {
            r.stop();
        }
        g.wda = None;
        g.dev.display = None;
        g.dev.display_verified_at = None;
        g.elements_cache = None;
        g.swiped_tree = None;
        g.swiped_snap = None;
        drop_tunnel(g).await;
        g.wda_misses.reset();
    }

    pub(crate) async fn ensure_tunnel<'a>(
        &self,
        g: &'a mut Inner,
        device: &Device,
    ) -> Result<&'a mut Tunnel, DeviceError> {
        if g.tunnel.is_none() {
            // boxed: the tunnel's handshake is a large future (about 35 KB), and nearly every tool may open one
            g.tunnel = Some(Box::pin(device.open_tunnel()).await?);
        }
        Ok(g.tunnel.as_mut().expect("刚建好"))
    }

    /// 不再控制当前这台（断开、重新接入、换设备）：停掉它的会话，清掉关于它的一切
    pub(crate) async fn forget_device(&self, g: &mut Inner) {
        // 还连着就先把 HID 粘贴前的剪贴板写回去：之后就没人管它了
        if g.dev.pending_paste.is_some() && g.tunnel.is_some() {
            self.restore_pasteboard(g, false).await;
        }
        self.drop_session(g).await;
        g.dev = PerDevice::default();
    }

    /// WDA 可用就返回客户端（本进程 runner 存活）
    pub(crate) fn wda<'a>(&self, g: &'a mut Inner) -> Option<&'a mut WdaClient<DeviceTransport>> {
        if g.runner.as_ref().is_some_and(|r| r.alive()) { g.wda.as_mut() } else { None }
    }

    /// How an action reaches the device in use now ([`Route`])
    pub(crate) fn route<'a>(&self, g: &'a mut Inner) -> Route<'a> {
        let native = self.native(g);
        let hid = hid_supported(g.info.as_ref());
        match self.wda(g) {
            Some(w) => Route::Wda(w),
            None if native => Route::Native,
            None if hid => Route::Hid,
            None => Route::Unavailable,
        }
    }

    pub(crate) fn account(&self) -> ictl_sign::AccountDir {
        self.cfg.account()
    }

    /// The device being controlled, from the last published snapshot (no lock)
    pub(crate) fn controlled_udid(&self) -> Option<String> {
        self.snap.borrow().device.as_ref().map(|d| d.udid.clone()).filter(|u| !u.is_empty())
    }

    /// Returns once the daemon no longer controls `udid` (another device was selected, or this one went away)
    pub(crate) async fn controlled_changes_from(&self, udid: &str) {
        let mut snap = self.snap.subscribe();
        loop {
            let now = snap.borrow_and_update().device.as_ref().map(|d| d.udid.clone()).filter(|u| !u.is_empty());
            if !now.is_some_and(|u| u.eq_ignore_ascii_case(udid)) || snap.changed().await.is_err() {
                return;
            }
        }
    }

    /// Tests: pretend the daemon controls `udid`
    #[cfg(test)]
    pub(crate) fn fake_controlled(&self, udid: &str) {
        self.snap.send_modify(|s| s.device = Some(DeviceInfo { udid: udid.into(), ..Default::default() }));
    }

    /// 发布快照给不拿锁的读者（`status`）。日志尾巴在读的时候再取
    pub(crate) fn publish(&self, g: &Inner) {
        self.heartbeat.beat();
        self.tasks.device_is(g.device.as_ref().map(|d| d.udid.as_str()));
        let unlock = self.unlock_cfg(g.device.as_ref().map(|d| d.udid.as_str()));
        let pinned = self.pinned(g);
        let devices = self.connected_infos(g);
        // USB identity is available even when lockdown refuses the metadata connection.
        let device = g.device.as_ref().map(|selected| {
            g.info
                .as_ref()
                .or_else(|| devices.iter().find(|info| info.udid == selected.udid))
                .cloned()
                .unwrap_or_else(|| DeviceInfo { udid: selected.udid.clone(), ..Default::default() })
        });
        self.snap.send_replace(Snapshot {
            version: Some(env!("CARGO_PKG_VERSION").into()),
            pid: Some(std::process::id()),
            maintenance: None,
            state: g.state.clone(),
            device,
            devices,
            display: g.dev.display.clone(),
            pinned,
            pinned_by_env: self.cfg.udid.is_some(),
            persisted: g.persisted.clone(),
            device_profile: g.dev.device_profile.clone(),
            native: self.native(g),
            auto_unlock: unlock.enabled(),
            unlock_disabled: unlock.disabled_by_failure(),
            notify_configured: self.cfg.notify.any(),
            log_tail: Vec::new(),
        });
    }

    pub async fn shutdown(&self) {
        let mut g = self.inner.lock().await;
        self.log.log(ictl_l10n::log_tr!("收到停止信号,退出", "Received stop signal, exiting"));
        // 停 HID 媒体流、runner（经系统的 usbmuxd）
        self.drop_session(&mut g).await;
    }
}
