//! 守护进程核心：唯一持有设备的进程。共享状态、请求分发、快照。
//!
//! 并发模型（全部跑在主循环的 LocalSet 里，单线程协作）：
//! - 设备相关的状态在 [`Inner`] 里，一把锁串行访问：HID 会话、WDA 会话只能有一个持有者，
//!   也不会出现 toolkit 那种「长请求期间被巡检误判 WDA 挂了」的问题
//! - 每个控制请求一个任务，拿到锁才执行；巡检（[`crate::supervise`]）拿不到锁就跳过这一轮
//! - 耗时的事不持锁：启动 WDA（含挂开发者镜像，最长约 2 分钟）、重签、会话检查在后台跑，做完再持锁写结果
//! - `status` 读发布出来的快照（watch 通道），不碰锁
//! - 改变手机状态的请求排队超过 [`STALE`] 就放弃，免得点击落到已经变了的屏幕上

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
use crate::protocol::{Request, Response};
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

/// 持久化到 `state.json` 的状态（不含秘密）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persisted {
    pub team_id: Option<String>,
    pub wda_bundle: Option<String>,
    /// 描述文件到期时间（Unix 秒）
    pub profile_expires: Option<u64>,
    /// 设备竖放时的屏幕尺寸（WDA 不可用时 HID 换算坐标用；旧版本可能存的是横屏尺寸，用的时候都先转成竖放）
    pub screen: Option<ScreenSize>,
    /// `screen` 是哪台设备的；换了设备就不能拿来给 HID 换算坐标。也是上次就绪的那台（自动选设备时先接着用它）
    pub screen_udid: Option<String>,
    /// Every device's screen size, by UDID: `screen` only holds the last one, so after switching devices and back
    /// the HID fallback didn't know the first one's size until its WDA came up again ([`Self::screen_for`])
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub screens: std::collections::BTreeMap<String, ScreenSize>,
    /// 上次重签的结果，旧格式：拼好的中文整句。照旧写，旧版本（降级、旧 CLI）还读它；新版本显示时用
    /// [`Self::resign_result`]
    pub last_resign: Option<String>,
    /// 上次重签的结果，按看的人的语言显示（[`Self::last_resign_text`]）。旧版本写的 state.json 没有它
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resign_result: Option<ResignRecord>,
    /// 运行中选定要控制的设备（`iphone-control select`、`iphone_select_device`）；None 是自动选。
    /// 设了 `IPHONE_UDID` 时以它为准，这个不起作用
    pub selected_udid: Option<String>,
    /// 后台启动的 WDA：最近记下的那个后台启动的 WDA（哪台设备上的哪个进程）。只留给旧版本读
    /// （降级后它据此接上或结束这一个）；新版本看 [`Self::detached_wdas`]，经 [`Self::detached_for`] 等读写
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_wda: Option<DetachedWda>,
    /// 每台设备上后台启动的 WDA 的进程号，按 UDID。守护进程重启、切换设备再切回来时
    /// 据此直接接上，不重新启动；改走原生控制、`service stop`/`uninstall` 时据此结束它（不然它一直
    /// 占着 8100 端口，新 runner 的 HTTP 服务起不来）。见 [`crate::detached`]
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub detached_wdas: std::collections::BTreeMap<String, u64>,
    /// Which way Home and locking work on each device, by UDID ([`Self::ways_for`]). Kept here, not only in memory:
    /// after a restart or switching devices the first Home on an iPad mini 5 tried the way that doesn't work there,
    /// waited 2 s for the home screen, then pressed again (4.4 s instead of 0.8)
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub button_ways: std::collections::BTreeMap<String, ButtonWays>,
}

/// Which way the buttons work on a device, learned when one way didn't and the other did. Unknown is `false` both
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonWays {
    /// Home through XCTest first: IOHID's didn't get to the home screen and XCTest's did (IOHID's Home hardly ever works
    /// on an iPhone 18 Pro Max / iOS 27.2.1, nor from an app on an iPad mini 5 / iPadOS 26.7.3; an iPhone 12 mini is the
    /// other way round). See `tools::keys::press_home`
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub home_via_xctest: bool,
    /// Lock with WDA's `/wda/lock` first: the power key through IOHID didn't lock and WDA did. On an iPhone 15 Pro Max
    /// (iOS 27.0.1) the power key doesn't work (the call hangs about 5 s, then 3 s more before falling back, about 10 s
    /// a lock), `/wda/lock` takes about 2 s; an iPhone 18 Pro Max (iOS 27.2.1) is the other way round. See
    /// `tools::keys::lock_order`
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lock_via_wda: bool,
}

/// 后台启动的 WDA 进程
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachedWda {
    pub udid: String,
    pub pid: u64,
}

/// 一次重签的结果
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResignRecord {
    /// 结束时间（Unix 秒）
    pub at: u64,
    /// 失败原因；成功时为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Persisted {
    /// The recorded screen size of `udid` (`None`: no device): its own entry, else `screen` if it is this device's
    /// (or from an older version that didn't say whose)
    pub fn screen_for(&self, udid: Option<&str>) -> Option<ScreenSize> {
        if let Some(u) = udid
            && let Some(s) = self.screens.iter().find(|(k, _)| k.eq_ignore_ascii_case(u)).map(|(_, s)| *s)
        {
            return Some(s);
        }
        let same = match (&self.screen_udid, udid) {
            (Some(s), Some(u)) => s.eq_ignore_ascii_case(u),
            (Some(_), None) => false,
            (None, _) => true,
        };
        self.screen.filter(|_| same)
    }

    /// Records `udid`'s screen size; whether anything changed (then save)
    pub fn record_screen(&mut self, udid: &str, size: ScreenSize) -> bool {
        let changed = self.screen != Some(size)
            || self.screen_udid.as_deref() != Some(udid)
            || self.screens.get(udid) != Some(&size);
        self.screen = Some(size);
        self.screen_udid = Some(udid.to_owned());
        self.screens.insert(udid.to_owned(), size);
        changed
    }

    /// How the buttons work on `udid` (`None`: no device), as learned so far
    pub fn ways_for(&self, udid: Option<&str>) -> ButtonWays {
        udid.and_then(|u| self.button_ways.iter().find(|(k, _)| k.eq_ignore_ascii_case(u)).map(|(_, w)| *w))
            .unwrap_or_default()
    }

    /// Changes what is known of `udid`'s buttons with `f`; whether anything changed (then save)
    pub fn record_ways(&mut self, udid: &str, f: impl FnOnce(&mut ButtonWays)) -> bool {
        let key =
            self.button_ways.keys().find(|k| k.eq_ignore_ascii_case(udid)).cloned().unwrap_or_else(|| udid.to_owned());
        let before = self.button_ways.get(&key).copied().unwrap_or_default();
        let mut now = before;
        f(&mut now);
        if now == before {
            return false;
        }
        self.button_ways.insert(key, now);
        true
    }

    /// The WDA launched in the background on `udid`, if one is recorded (by an older version: only the last one)
    pub fn detached_for(&self, udid: &str) -> Option<u64> {
        self.detached_wdas
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(udid))
            .map(|(_, p)| *p)
            .or_else(|| self.detached_wda.as_ref().filter(|d| d.udid.eq_ignore_ascii_case(udid)).map(|d| d.pid))
    }

    /// Every recorded background WDA: (UDID, pid)
    pub fn detached_all(&self) -> Vec<(String, u64)> {
        let mut all: Vec<_> = self.detached_wdas.iter().map(|(u, p)| (u.clone(), *p)).collect();
        if let Some(d) = &self.detached_wda
            && !all.iter().any(|(u, _)| u.eq_ignore_ascii_case(&d.udid))
        {
            all.push((d.udid.clone(), d.pid));
        }
        all
    }

    /// Records the WDA launched in the background on `udid` (the other devices' stay); whether anything changed
    /// (then save)
    pub fn record_detached(&mut self, udid: &str, pid: u64) -> bool {
        // an older version's only record is overwritten below: keep it among the devices' records first
        if let Some(d) = self.detached_wda.clone()
            && !self.detached_wdas.keys().any(|k| k.eq_ignore_ascii_case(&d.udid))
        {
            self.detached_wdas.insert(d.udid, d.pid);
        }
        let key = self
            .detached_wdas
            .keys()
            .find(|k| k.eq_ignore_ascii_case(udid))
            .cloned()
            .unwrap_or_else(|| udid.to_owned());
        let last = Some(DetachedWda { udid: key.clone(), pid });
        let changed = self.detached_wdas.get(&key) != Some(&pid) || self.detached_wda != last;
        self.detached_wdas.insert(key, pid);
        self.detached_wda = last;
        changed
    }

    /// Forgets the background WDA recorded for `udid` (only if it is `pid`, when given); whether anything changed
    /// (then save)
    pub fn forget_detached(&mut self, udid: &str, pid: Option<u64>) -> bool {
        let before = (self.detached_wdas.len(), self.detached_wda.is_some());
        self.detached_wdas.retain(|k, p| !(k.eq_ignore_ascii_case(udid) && pid.is_none_or(|x| x == *p)));
        if self
            .detached_wda
            .as_ref()
            .is_some_and(|d| d.udid.eq_ignore_ascii_case(udid) && pid.is_none_or(|x| x == d.pid))
        {
            self.detached_wda = None;
        }
        before != (self.detached_wdas.len(), self.detached_wda.is_some())
    }

    /// 记下这次重签的结果：新字段，加上旧版本认的中文整句
    pub fn record_resign(&mut self, at: u64, error: Option<String>) {
        let when = crate::eventlog::format_utc(at);
        self.last_resign = Some(match &error {
            None => format!("{when} 成功"),
            Some(e) => format!("{when} 失败：{e}"),
        });
        self.resign_result = Some(ResignRecord { at, error });
    }

    /// 上次重签的结果，按当前语言；只有旧格式时原样给出
    pub fn last_resign_text(&self) -> Option<String> {
        let Some(r) = &self.resign_result else { return self.last_resign.clone() };
        let when = crate::eventlog::format_utc(r.at);
        Some(match &r.error {
            None => ictl_l10n::trf!("{when} 成功", "{when} succeeded"),
            Some(e) => ictl_l10n::trf!("{when} 失败：{e}", "{when} failed: {e}"),
        })
    }

    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else { return Self::default() };
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            // 损坏时按空状态启动（会显示「WDA 还没签名」），但要留痕
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!(
                    "state.json 解析失败，按空状态启动：{e}",
                    "Parsing state.json failed, starting with an empty state: {e}"
                )
            );
            Self::default()
        })
    }

    pub(crate) fn save(&self, path: &Path) {
        use std::io::Write;
        let tmp = path.with_extension("json.tmp");
        // sync before the rename, or a power cut can leave state.json empty
        let r = std::fs::File::create(&tmp)
            .and_then(|mut f| {
                f.write_all(&serde_json::to_vec_pretty(self).unwrap_or_default())?;
                f.sync_all()
            })
            .and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = r {
            tracing::warn!("{}", ictl_l10n::log_trf!("保存 state.json 失败：{e}", "Saving state.json failed: {e}"));
        }
    }

    /// 描述文件还剩多久；不知道返回 None
    pub fn profile_left(&self) -> Option<Duration> {
        self.profile_expires.and_then(left_until)
    }
}

/// 离 Unix 时间 `at`（秒）还有多久，过了是 0
fn left_until(at: u64) -> Option<Duration> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(at.saturating_sub(now)))
}

/// 这台手机上装着的 WDA 描述文件（经 misagent 读）。[`Persisted`] 只记最近一次签名：给别的手机签过之后，
/// 它的到期时间不是这台手机上那份的（这台装的还是更早签的），按它判断会该重签时不重签
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum DeviceProfile {
    /// 还没读（手机没接、刚接上还没配对）
    #[default]
    NotChecked,
    /// 读不出来：按签名记录算
    Unknown(String),
    /// 手机上没有当前 Apple ID 给 WDA 签的、包含这台手机的描述文件：没装过 WDA，或装的是别的账号签的
    Missing,
    /// 到期时间（Unix 秒）
    Expires(u64),
}

impl DeviceProfile {
    /// 还剩多久：读到了用手机上的；还没读、读不出来按签名记录；手机上没有返回 None
    pub fn left(&self, persisted: &Persisted) -> Option<Duration> {
        match self {
            Self::Expires(e) => left_until(*e),
            Self::Missing => None,
            Self::NotChecked | Self::Unknown(_) => persisted.profile_left(),
        }
    }
}

/// 从手机上的描述文件（原始内容）里挑 WDA 的：app-id 是「团队 ID.WDA Bundle ID」（不知道团队 ID 时只看 Bundle ID）、
/// 设备列表里有这台手机，取最晚到期的那份。装过几次会留下几份，旧的过期了也还在
pub fn wda_profile(profiles: &[Vec<u8>], team: Option<&str>, bundle: &str, udid: &str) -> Option<ictl_sign::Profile> {
    profiles
        .iter()
        .filter_map(|p| ictl_sign::Profile::parse(p))
        .filter(|p| {
            p.application_identifier
                .split_once('.')
                .is_some_and(|(t, b)| b == bundle && team.is_none_or(|team| t == team))
                && p.devices.iter().any(|d| d.eq_ignore_ascii_case(udid))
        })
        .max_by_key(|p| p.expires)
}

pub fn wda_profile_expiry(profiles: &[Vec<u8>], team: Option<&str>, bundle: &str, udid: &str) -> Option<SystemTime> {
    wda_profile(profiles, team, bundle, udid).map(|p| p.expires)
}

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
    /// 最近一个操作开始的时间：原生控制等界面稳定时，从这之后的推送算起
    pub acted_at: Option<Instant>,
    /// How long the request being handled spent unlocking the phone first ([`Daemon::ensure_unlocked`]); taken by
    /// [`Daemon::dispatch`], which says so in the answer
    pub unlocked: Option<Duration>,
    /// 原生控制：进程号 → Bundle ID（前台 App 由无障碍服务给进程号）
    pub bundles: HashMap<i64, String>,
    /// 原生控制：进程号 → 是不是网页内容进程（WebKit WebContent，网页的元素来自它）
    pub web_pids: HashMap<i64, bool>,
}

/// What preparing a phone for native control gives: the developer disk image's version if it was mounted just now,
/// and what became of the WDA launched in the background earlier (if one was recorded for it); failed, whether it was
/// the mount itself
pub(crate) type Prepared =
    std::result::Result<(Option<String>, Option<(String, crate::detached::StoppedWda)>), (DeviceError, bool)>;

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
    pub elements_cache: Option<(Instant, ictl_wda::elements::Tree)>,
    /// 滑动等列表停下时最后读到的树（[`crate::tools`] 的 `settle` 拿来当比较的起点）
    pub swiped_tree: Option<(Instant, ictl_wda::elements::Tree)>,
    pub dedup: Dedup,
    /// 各设备最近一次因为证书自动重签的时间（按 UDID，换设备、重插也记着）：[`crate::supervise::CERT_HEAL_EVERY`] 内
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
    /// When the last action was done: background checks wait for a quiet moment ([`quiet_before`])
    last_action: std::sync::Mutex<Option<Instant>>,
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
}

/// 丢掉隧道。一律走这里：先停 HID 媒体流再断（见 [`close_hid`]）
pub(crate) async fn drop_tunnel(g: &mut Inner) {
    g.dev.touch_rotation = None;
    if let Some(t) = g.tunnel.take() {
        // boxed: the future holds the whole tunnel, and every tool that may drop it would carry that
        Box::pin(t.close()).await;
    }
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

/// HID 后备路线（免 WDA 截图、点击）只有 iOS 27 起能用；不知道版本时照常尝试
pub(crate) fn hid_supported(info: Option<&DeviceInfo>) -> bool {
    info.is_none_or(|i| matches!(i.major_version(), 0 | 27..))
}

impl Daemon {
    pub fn new(cfg: Config) -> Arc<Self> {
        let log = EventLog::new(&cfg.logs());
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
            ax_used: None,
            preparing: None,
            screen_viewers: 0,
            low_bandwidth_viewers: 0,
            elements_cache: None,
            swiped_tree: None,
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
            unlock_disabled: cfg.unlock_disabled_flag().exists(),
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
        })
    }

    /// 自动解锁配置：用设备 `udid` 自己的密码。没接设备时密码文件不存在（自动解锁不可用）
    pub(crate) fn unlock_cfg(&self, udid: Option<&str>) -> UnlockConfig {
        UnlockConfig {
            passcode_file: self.cfg.passcode_file_for(udid.unwrap_or("_none")),
            disabled_flag: self.cfg.unlock_disabled_flag(),
            timing: Default::default(),
        }
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

    /// 旧版本那份不知道是哪台的密码：USB 上只接着一台、正好是要控制的这台、这台还没有自己的密码时，归到它名下。
    /// 接着多台时不动它（也不用它）
    pub(crate) fn adopt_legacy_passcode(&self, g: &Inner) {
        let legacy = self.cfg.legacy_passcode_file();
        let ([only], Some(d)) = (&g.connected[..], &g.device) else { return };
        if only.udid != d.udid || !legacy.is_file() {
            return;
        }
        let dst = self.cfg.passcode_file_for(&d.udid);
        if dst.exists() {
            return;
        }
        let r = self.cfg.ensure_passcodes_dir().and_then(|_| std::fs::rename(&legacy, &dst));
        match r {
            Ok(()) => self.log.log(&ictl_l10n::log_trf!("旧的锁屏密码只有一份，USB 上只接着 {}，归到它名下", "There is a single passcode saved by an older version and only {} is on USB; assigning it to that device", d.udid)),
            Err(e) => tracing::warn!("{}", ictl_l10n::log_trf!("把旧的锁屏密码归到 {} 名下失败：{e}", "Assigning the old passcode to {} failed: {e}", d.udid)),
        }
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

    pub(crate) fn require_wda<'a>(&self, g: &'a mut Inner) -> crate::error::Result<&'a mut WdaClient<DeviceTransport>> {
        let msg = wda_down(&g.state, g.info.as_ref());
        self.wda(g).ok_or(msg.into())
    }

    pub(crate) fn account(&self) -> ictl_sign::AccountDir {
        ictl_sign::AccountDir { dir: self.cfg.account_dir(), anisette_url: self.cfg.anisette_url.clone() }
    }

    /// The device being controlled, from the last published snapshot (no lock)
    pub(crate) fn controlled_udid(&self) -> Option<String> {
        self.snap.borrow().device.as_ref().map(|d| d.udid.clone()).filter(|u| !u.is_empty())
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

    // ------------------------------------------------------------------ 请求处理

    /// 处理一个控制请求。`queued_at`：请求到达的时间，用来放弃排队太久的操作
    pub async fn handle(self: &Arc<Self>, req: Request, queued_at: Instant, mut ctx: CallCtx) -> Response {
        // 不需要设备锁的请求
        if matches!(req, Request::Daemon) {
            let mut s = self.snap.borrow().clone();
            let unlock = self.unlock_cfg(s.device.as_ref().map(|d| d.udid.as_str()));
            s.unlock_disabled = unlock.disabled_by_failure();
            s.auto_unlock = unlock.enabled();
            s.log_tail = self.log.tail(15);
            s.maintenance = crate::maintenance::active();
            return Response::text(serde_json::to_string_pretty(&s).unwrap_or_default());
        }
        if let Err(e) = check_request(&req) {
            return Response::error(e);
        }
        if let Some(r) = self.tasks.handle(&req, ctx.task.as_deref(), ctx.peer, &mut ctx.gone).await {
            return r;
        }
        // while a task runs, only it acts on the phone (or the user, after taking over): see `crate::tasks`
        let (admission, waited) =
            match self.tasks.admit(&req, ctx.task.as_deref(), &mut ctx.expect_udid, &mut ctx.gone).await {
                Ok(a) => a,
                Err(r) => return r,
            };
        // a task limited to some apps: outside them, the action waits for the user (docs/task-control.md §2.6)
        let mut asked = false;
        if let crate::tasks::Admission::Actor(t) = &admission
            && t.scoped()
        {
            match self.scope_gate(t, &req, ctx.task_step, &mut ctx.gone).await {
                Ok(a) => asked = a,
                Err(r) => return r,
            }
        }
        // held while the task was paused or waiting for the user: the time in the queue starts now
        let queued_at = if waited || asked { Instant::now() } else { queued_at };
        // an action of the task that no announced step covers is a step of its own
        let auto = match &admission {
            crate::tasks::Admission::Actor(t) if ctx.task_step.is_none() && req.is_device_action() => {
                Some((t.clone(), self.tasks.auto_step(t, &req)))
            }
            _ => None,
        };
        let t0 = Instant::now();
        let looks = crate::tasks::observes(&req);
        // boxed: the request's state machine is large, and a connection's task shouldn't carry it on the stack
        let resp = Box::pin(self.handle_admitted(req, queued_at, ctx, &admission)).await;
        if let Some((t, step)) = auto {
            self.tasks.auto_step_done(&t, step, &resp, t0.elapsed().as_millis() as u64);
        }
        // the agent has seen the screen only once its look succeeded
        if let crate::tasks::Admission::Actor(t) = &admission
            && looks
            && !matches!(resp, Response::Error { .. })
        {
            t.observed();
        }
        resp
    }

    /// A request's lock, once the tasks still let it through ([`crate::tasks::Tasks::still`]): it may have queued
    /// behind other requests while the user paused, took over or handed back. A task's request waits for the task to
    /// run again, without the lock
    async fn lock_admitted<'a, T>(
        &self,
        lock: &'a Mutex<T>,
        req: &Request,
        admission: &crate::tasks::Admission,
        queued_at: &mut Instant,
        gone: &mut Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Result<tokio::sync::MutexGuard<'a, T>, Response> {
        use crate::tasks::Still;
        loop {
            let g = acquire(lock, gone).await?;
            match self.tasks.still(admission, req) {
                Still::Admitted => return Ok(g),
                Still::Refused(r) => return Err(r),
                Still::Wait(t) => {
                    drop(g);
                    t.wait_running(gone).await?;
                    *queued_at = Instant::now();
                }
            }
        }
    }

    /// What an action of a scoped task touches, then [`crate::tasks::Task::check_scope`]. The device lock is only held
    /// to look it up, not while the user decides (the user may take over meanwhile). Returns whether it waited
    async fn scope_gate(
        &self,
        task: &crate::tasks::Task,
        req: &Request,
        step: Option<u64>,
        gone: &mut Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Result<bool, Response> {
        use crate::tasks::{Check, Target};
        let Some(check) = crate::tasks::scope_check(req) else { return Ok(false) };
        let target = match check {
            Check::Url(u) => Target::Url(u),
            Check::Always { action, detail } => Target::Always { action, detail },
            Check::Launch(app) => {
                let mut g = acquire(&self.inner, gone).await?;
                Target::Launch(self.resolve_app(&mut g, &app).await.map_err(Response::error)?)
            }
            Check::Foreground => {
                let mut g = acquire(&self.inner, gone).await?;
                // can't tell which app is in front: a scoped task doesn't act blind
                let unknown = |why: String| {
                    Response::error(ictl_l10n::trf!(
                        "看不出前台是哪个 App（{why}）；这个任务限定了 App 范围，所以没有操作。稍后截图再试",
                        "Can't tell which app is in front ({why}); this task is limited to some apps, so nothing was done. Take a screenshot and try again shortly"
                    ))
                };
                // the lock is let go before the user is asked (below)
                let app = if self.wda(&mut g).is_none() && self.native(&g) {
                    Box::pin(self.native_foreground(&mut g)).await.map_err(|e| unknown(e.to_string()))?
                } else {
                    let Some(w) = self.wda(&mut g) else {
                        return Err(unknown(ictl_l10n::tr!("WDA 不可用", "WDA unavailable").into()));
                    };
                    w.active_app().await.map_err(|e| unknown(e.to_string()))?
                };
                Target::Foreground(app)
            }
        };
        task.check_scope(&target, step, gone).await
    }

    /// A task's scope from names or bundle IDs: the bundle IDs, each once. `Err` names the app that isn't there
    pub(crate) async fn resolve_scope(&self, apps: &[String]) -> crate::error::Result<Vec<String>> {
        let apps: Vec<&str> = apps.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect();
        if apps.is_empty() {
            return Ok(Vec::new());
        }
        let mut g = self.inner.lock().await;
        let mut out: Vec<String> = Vec::new();
        for a in apps {
            let b = self.resolve_app(&mut g, a).await?;
            if !out.iter().any(|x| x.eq_ignore_ascii_case(&b)) {
                out.push(b);
            }
        }
        Ok(out)
    }

    /// [`Self::handle`] once the tasks let the request through
    async fn handle_admitted(
        self: &Arc<Self>,
        req: Request,
        mut queued_at: Instant,
        mut ctx: CallCtx,
        admission: &crate::tasks::Admission,
    ) -> Response {
        let text = |r: crate::error::Result<String>| {
            r.map(|t| Response::text_with(t, crate::protocol::Outcome::default())).unwrap_or_else(Response::error)
        };
        match lane(&req) {
            Lane::Snapshot => unreachable!("handled above"),
            Lane::Task => {
                return Response::error(ictl_l10n::tr!(
                    "task_open、task_events 是流式请求，要用 open_task / task_events 打开",
                    "task_open and task_events are streaming requests: open them with open_task / task_events"
                ));
            }
            // 重签自己分段持锁：签名、安装要几十秒，期间不挡住别的请求
            Lane::Resign => {
                let Request::Resign { force } = req else { unreachable!("lane") };
                return self.resign_now(force).await.map(Response::text).unwrap_or_else(Response::error);
            }
            // Web Inspector talks to the page, not to WDA: its own lock, the device lock only to look up the device
            Lane::Web => {
                let mut slot = match self.lock_admitted(&self.web, &req, admission, &mut queued_at, &mut ctx.gone).await
                {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let device = self.inner.lock().await.device.clone();
                if let Err(r) = admit(&req, queued_at, &ctx, device.as_ref().map(|d| d.udid.as_str())) {
                    return r;
                }
                let Some(device) = device else {
                    return Response::error(ictl_l10n::tr!("手机未连接", "Phone not connected"));
                };
                return text(match req {
                    Request::WebTargets => self.web_targets(&mut slot, &device).await,
                    Request::WebEval { target, expression, timeout_ms } => {
                        self.web_eval(&mut slot, &device, &target, &expression, Duration::from_millis(timeout_ms)).await
                    }
                    _ => unreachable!("lane"),
                });
            }
            // an install can take minutes and doesn't hold the device lock; installs queue among themselves
            Lane::Install => {
                if ctx.gone.as_mut().is_some_and(|g| g.try_recv().is_ok()) {
                    return cancelled();
                }
                loop {
                    match self.tasks.still(admission, &req) {
                        crate::tasks::Still::Admitted => break,
                        crate::tasks::Still::Refused(r) => return r,
                        crate::tasks::Still::Wait(t) => {
                            if let Err(r) = t.wait_running(&mut ctx.gone).await {
                                return r;
                            }
                            queued_at = Instant::now();
                        }
                    }
                }
                let device = self.inner.lock().await.device.clone();
                if let Err(r) = admit(&req, queued_at, &ctx, device.as_ref().map(|d| d.udid.as_str())) {
                    return r;
                }
                let Some(device) = device else {
                    return Response::error(ictl_l10n::tr!("手机未连接", "Phone not connected"));
                };
                let Request::InstallApp { path } = req else { unreachable!("lane") };
                let r = self.install_app(&device, &path).await;
                if r.is_ok() {
                    self.inner.lock().await.dev.apps.clear();
                }
                return text(r);
            }
            Lane::Device => {}
        }

        // a check the page repeats anyway: only while nothing else wants the phone ([`ictl_protocol::BACKGROUND`])
        let background = ctx.background && matches!(req, Request::Status | Request::AlertCheck);
        // someone is acting on the phone: a slow check now would sit in WDA's queue before their next action (giving
        // up waiting for its answer doesn't stop WDA working on it)
        let acting = || {
            let last = *self.last_action.lock().unwrap_or_else(|p| p.into_inner());
            last.is_some_and(|t| t.elapsed() < quiet_before(&req))
        };
        let mut g = if background {
            if acting() {
                return gave_way();
            }
            match self.inner.try_lock() {
                Ok(g) if self.waiting.load(std::sync::atomic::Ordering::SeqCst) == 0 => g,
                _ => return gave_way(),
            }
        } else {
            let _waiting = Waiting::new(self);
            // still let through now that it holds the lock: the task may have been paused or taken over meanwhile
            match self.lock_admitted(&self.inner, &req, admission, &mut queued_at, &mut ctx.gone).await {
                Ok(g) => g,
                Err(r) => return r,
            }
        };
        let g = &mut *g;
        if let Err(r) = admit(&req, queued_at, &ctx, g.device.as_ref().map(|d| d.udid.as_str())) {
            return r;
        }
        let view = ctx.expect_view.as_deref();
        // 改变界面的操作做完后缓存的元素树就作废；只有调用方声明「不改变布局」的按名字点击（max_age_ms > 0）保留
        let keeps_cache = matches!(req, Request::TapElement { max_age_ms, .. } if max_age_ms > 0);
        let invalidates = req.is_device_action() && !keeps_cache;
        // WDA 卡死（经 XCTest 的请求接连没有应答）：先重启它，这个请求按 WDA 正在启动处理。工具调用接连不断时巡检抢不到设备，
        // 等不到它来重启
        if g.wda.as_ref().is_some_and(|w| w.stalled()) {
            self.restart_stalled_wda(g).await;
        }
        let response = if background {
            // an action that comes meanwhile gets the phone at once: the check is read-only, dropping it is safe
            let action = self.give_way.notified();
            tokio::pin!(action);
            action.as_mut().enable();
            if self.waiting.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                gave_way()
            } else {
                tokio::select! {
                    r = Box::pin(self.dispatch(g, req, view, ctx.watched)) => r,
                    () = action => gave_way(),
                }
            }
        } else {
            let acted = req.is_device_action();
            if acted {
                // 这个操作之前读过的界面就是「操作前」；操作期间读的（按名字点击先读树）也是操作前，操作完都不再算
                g.dev.before_action = g.dev.seen.take_fresh();
                // 原生控制等界面稳定时，从这之后手机推来的树算起
                g.dev.acted_at = Some(Instant::now());
            }
            // boxed, like the request in `handle`: two of its large state machine on the stack overflowed a test thread
            let r = Box::pin(self.dispatch(g, req, view, ctx.watched)).await;
            if acted {
                g.dev.seen = Default::default();
                *self.last_action.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
            }
            r
        };
        if invalidates {
            g.elements_cache = None;
        }
        self.publish(g);
        response
    }

    async fn dispatch(&self, g: &mut Inner, req: Request, view: Option<&str>, watched: bool) -> Response {
        // what the text reports, for clients that act on it (Response::Text.outcome)
        let mut outcome = crate::protocol::Outcome::default();
        // the same facts as data, for programs (Response::Text.data)
        let mut data = None;
        g.dev.unlocked = None;
        let r = match req {
            Request::Status => {
                let (text, status) = self.status(g).await;
                data = Some(crate::protocol::Data::Status(status));
                Ok(text)
            }
            // streams on its own connection (`lib::serve_conn` hands it to `screen::stream`); never queued here
            Request::ScreenStream => {
                Err("screen_stream is a streaming request: open it with open_screen_stream".into())
            }
            Request::Screenshot { region } => {
                let r = self.screenshot(g, region).await.unwrap_or_else(Response::error);
                return with_unlock_note(r, g.dev.unlocked.take());
            }
            Request::ListElements { filter, limit, max_age_ms, keep_locked } => self
                .list_elements(g, &filter, limit, Duration::from_millis(max_age_ms), keep_locked)
                .await
                .map(|(text, elements)| {
                    data = Some(crate::protocol::Data::Elements { elements });
                    text
                }),
            Request::Tap { x, y } => self.tap(g, x, y, view).await,
            Request::TapElement { text, index, max_age_ms } => {
                let r = self.tap_element(g, &text, index, Duration::from_millis(max_age_ms)).await;
                r.map(|(t, missed)| {
                    outcome.tapped_nothing = missed;
                    // nothing was tapped: there is no "before the action" to compare the screen with, or the next
                    // settle would say the tap may not have taken effect
                    if missed {
                        g.dev.before_action = None;
                    }
                    t
                })
            }
            Request::LongPress { x, y, seconds } => self.long_press(g, x, y, seconds, view).await,
            Request::Swipe { from, to, seconds } => self.swipe(g, from, to, seconds, view, watched).await,
            Request::TypeText { text, submit } => self.type_text(g, &text, submit).await,
            Request::PressButton { button } => self.press_button(g, &button, watched).await,
            Request::Touch { phase, x, y } => {
                return self
                    .touch(g, phase, x, y, view)
                    .await
                    .map(|()| Response::text_with("", outcome))
                    .unwrap_or_else(|e| e);
            }
            Request::ListApps { include_system } => self.list_apps(g, include_system).await.map(|a| {
                let apps = a
                    .iter()
                    .map(|a| crate::protocol::AppData { name: a.name.clone(), bundle_id: a.bundle_id.clone() })
                    .collect();
                data = Some(crate::protocol::Data::Apps { apps });
                a.iter().map(|a| format!("{}\t{}", a.name, a.bundle_id)).collect::<Vec<_>>().join("\n")
            }),
            Request::LaunchApp { app } => self.launch_app(g, &app).await.map(|(t, alert)| {
                outcome.alert_reported = alert;
                t
            }),
            Request::TerminateApp { app } => self.terminate_app(g, &app).await,
            Request::OpenUrl { url } => self.open_url(g, &url).await.map(|(t, alert)| {
                outcome.alert_reported = alert;
                t
            }),
            Request::Settle { max_ms } => self.settle(g, Duration::from_millis(max_ms)).await,
            Request::AlertCheck => self.alert_check(g).await.map(|(text, alert)| {
                data = Some(crate::protocol::Data::Alert { text: alert });
                text
            }),
            Request::WaitFor { text, timeout_ms } => self.wait_for(g, &text, Duration::from_millis(timeout_ms)).await,
            Request::Doctor => Ok(self.doctor(g).await),
            Request::UnlockReset => {
                let _ = std::fs::remove_file(self.cfg.unlock_disabled_flag());
                g.dedup.clear("unlock");
                Ok(ictl_l10n::tr!("已恢复自动解锁", "Auto-unlock re-enabled").into())
            }
            Request::ListDevices => Ok(self.list_devices_text(g)),
            Request::SelectDevice { device } => self.select_device(g, &device).await,
            Request::AppLogs { process, grep, since_secs, limit, errors_only } => {
                self.app_logs(g, &process, &grep, Duration::from_secs(since_secs), limit, errors_only).await
            }
            Request::CrashReports { filter, name } => self.crash_reports(g, &filter, &name).await,
            Request::UninstallApp { app } => self.uninstall_app(g, &app).await,
            Request::Daemon
            | Request::Resign { .. }
            | Request::InstallApp { .. }
            | Request::WebTargets
            | Request::WebEval { .. }
            | Request::TaskOpen { .. }
            | Request::TaskEvents { .. }
            | Request::TaskList
            | Request::TaskControl { .. }
            | Request::TaskAgent
            | Request::TaskCheck
            | Request::TaskStep { .. }
            | Request::TaskStepDone { .. }
            | Request::TaskConfirm { .. }
            | Request::TaskNote { .. }
            | Request::TaskFinish { .. } => unreachable!("other lanes"),
        };
        let r = r.map(|t| Response::text_data(t, outcome, data)).unwrap_or_else(Response::error);
        with_unlock_note(r, g.dev.unlocked.take())
    }

    pub async fn shutdown(&self) {
        let mut g = self.inner.lock().await;
        self.log.log(ictl_l10n::log_tr!("收到停止信号,退出", "Received stop signal, exiting"));
        // 停 HID 媒体流、runner（经系统的 usbmuxd）
        self.drop_session(&mut g).await;
    }
}

/// 参数检查（与设备无关，不必拿锁）
/// Which path a request takes through [`Daemon::handle`]. Every variant is listed (no `_`), so a new request type
/// doesn't compile until it is given a lane; a lane decides the lock, and every lane but the snapshot and re-signing
/// goes through [`admit`] (cancelled callers, stale actions, the expected device)
enum Lane {
    /// `status`'s snapshot: no lock
    Snapshot,
    /// re-signing takes the device lock in short steps
    Resign,
    /// the Web Inspector connection's own lock
    Web,
    /// no device lock; installs queue among themselves
    Install,
    /// the device lock
    Device,
    /// tasks: the streams are served by the connection (`lib::serve_conn`), the rest before any lane
    Task,
}

fn lane(req: &Request) -> Lane {
    match req {
        Request::Daemon => Lane::Snapshot,
        Request::Resign { .. } => Lane::Resign,
        Request::WebTargets | Request::WebEval { .. } => Lane::Web,
        Request::InstallApp { .. } => Lane::Install,
        Request::TaskOpen { .. }
        | Request::TaskEvents { .. }
        | Request::TaskList
        | Request::TaskControl { .. }
        | Request::TaskAgent
        | Request::TaskCheck
        | Request::TaskStep { .. }
        | Request::TaskStepDone { .. }
        | Request::TaskConfirm { .. }
        | Request::TaskNote { .. }
        | Request::TaskFinish { .. } => Lane::Task,
        Request::Status
        | Request::ScreenStream
        | Request::Screenshot { .. }
        | Request::ListElements { .. }
        | Request::Tap { .. }
        | Request::TapElement { .. }
        | Request::LongPress { .. }
        | Request::Swipe { .. }
        | Request::TypeText { .. }
        | Request::PressButton { .. }
        | Request::Touch { .. }
        | Request::ListApps { .. }
        | Request::LaunchApp { .. }
        | Request::TerminateApp { .. }
        | Request::OpenUrl { .. }
        | Request::Settle { .. }
        | Request::AlertCheck
        | Request::WaitFor { .. }
        | Request::Doctor
        | Request::UnlockReset
        | Request::ListDevices
        | Request::SelectDevice { .. }
        | Request::AppLogs { .. }
        | Request::CrashReports { .. }
        | Request::UninstallApp { .. } => Lane::Device,
    }
}

/// Says in a request's answer that the phone was unlocked for it first, and how long that took: on the first line,
/// like the other notes about what was done. An action that had to unlock took seconds longer than usual (14-15 s
/// on an iPad through WDA, 8-9 s natively on an iPhone), and nothing in the answer said why
fn with_unlock_note(r: Response, unlocked: Option<Duration>) -> Response {
    let Some(took) = unlocked else { return r };
    let note = ictl_l10n::trf!(
        "(手机锁着,先自动解锁了,用时 {:.1} 秒)",
        " (the phone was locked and was unlocked first, which took {:.1} s)",
        took.as_secs_f64()
    );
    let add = |s: String| match s.split_once('\n') {
        Some((first, rest)) => format!("{first}{note}\n{rest}"),
        None => s + &note,
    };
    match r {
        Response::Text { text, outcome, data } => Response::Text { text: add(text), outcome, data },
        Response::Image { jpeg_base64, caption } => Response::Image { jpeg_base64, caption: add(caption) },
        Response::Error { message, retry_as_gesture } => Response::Error { message: add(message), retry_as_gesture },
    }
}

fn cancelled() -> Response {
    Response::error(ictl_l10n::tr!(
        "调用方已取消,请求没有执行",
        "The caller cancelled; the request was not carried out"
    ))
}

/// Takes `lock`, unless the caller goes away first (see [`CallCtx::gone`])
async fn acquire<'a, T>(
    lock: &'a Mutex<T>,
    gone: &mut Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<tokio::sync::MutexGuard<'a, T>, Response> {
    match gone {
        Some(gone) => tokio::select! {
            biased;
            g = lock.lock() => Ok(g),
            Ok(()) = &mut *gone => Err(cancelled()),
        },
        None => Ok(lock.lock().await),
    }
}

/// How long after an action a background check waits. The alert check takes WDA about 0.8 s on the home screen
/// (it snapshots SpringBoard; not waiting for idle doesn't help) and WDA does one thing at a time, so a tap or a Home
/// press right after it waited that long; the status check is about 0.2 s
fn quiet_before(req: &Request) -> Duration {
    match req {
        Request::AlertCheck => Duration::from_secs(4),
        _ => Duration::from_secs(1),
    }
}

/// A background check that didn't run, or stopped: an action wanted the phone ([`ictl_protocol::Data::Busy`])
fn gave_way() -> Response {
    Response::text_data(
        ictl_l10n::tr!(
            "手机正在执行操作，这次没查，稍后再查",
            "The phone is busy with an action; not checked this time"
        ),
        crate::protocol::Outcome::default(),
        Some(crate::protocol::Data::Busy),
    )
}

/// An action waiting for the device lock, counted in [`Daemon::waiting`] until it has it
struct Waiting<'a>(&'a Daemon);

impl<'a> Waiting<'a> {
    fn new(d: &'a Daemon) -> Self {
        d.waiting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        d.give_way.notify_waiters();
        Self(d)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Checks done once a request got its lock: the caller's expected device ([`CallCtx::expect_udid`]) and, for actions,
/// how long it queued ([`STALE`])
fn admit(req: &Request, queued_at: Instant, ctx: &CallCtx, controlled: Option<&str>) -> Result<(), Response> {
    if let Some(want) = &ctx.expect_udid
        && controlled != Some(want.as_str())
    {
        return Err(Response::error(ictl_l10n::trf!(
            "正在控制的设备变了(原来是 {want},现在{}),没有执行;先用 iphone_list_devices 确认",
            "The controlled device changed (it was {want}, now {}); not carried out. Check with iphone_list_devices first",
            controlled.map_or(ictl_l10n::tr!("没有设备", "there is no device").into(), |u| ictl_l10n::trf!(
                "是 {u}",
                "it is {u}"
            ))
        )));
    }
    if req.is_device_action() && queued_at.elapsed() > STALE {
        return Err(Response::error(ictl_l10n::trf!(
            "守护进程正忙,这个操作排队超过 {} 秒,已放弃执行(以免落到已经变化的屏幕上);先截图看看再重试",
            "The daemon is busy and this action waited in the queue for more than {} seconds, so it was dropped (it could have landed on a changed screen); take a screenshot and try again",
            STALE.as_secs()
        )));
    }
    Ok(())
}

fn check_request(req: &Request) -> Result<(), String> {
    match req {
        Request::WebEval { expression, timeout_ms, .. }
            if expression.trim().is_empty() || *timeout_ms > MAX_WEB_EVAL_MS =>
        {
            return Err(ictl_l10n::trf!(
                "expression 不能为空,timeout 最多 {} 秒",
                "expression can't be empty, and timeout can be at most {} seconds",
                MAX_WEB_EVAL_MS / 1000
            ));
        }
        Request::Settle { max_ms } if *max_ms > MAX_WAIT_MS => {
            return Err(ictl_l10n::trf!("max_ms 不能超过 {MAX_WAIT_MS}", "max_ms can't exceed {MAX_WAIT_MS}"));
        }
        Request::WaitFor { timeout_ms, .. } if *timeout_ms > MAX_WAIT_MS => {
            return Err(ictl_l10n::trf!(
                "timeout 不能超过 {} 秒",
                "timeout can't exceed {} seconds",
                MAX_WAIT_MS / 1000
            ));
        }
        Request::Touch { x, y, .. } if !x.is_finite() || !y.is_finite() => {
            return Err(ictl_l10n::tr!("x、y 要是有效的数", "x and y must be valid numbers").into());
        }
        Request::WaitFor { text, .. } if text.is_empty() => {
            return Err(ictl_l10n::tr!("text 不能为空", "text can't be empty").into());
        }
        Request::AppLogs { since_secs, limit, .. } if *since_secs > 3600 || *limit == 0 || *limit > 1000 => {
            return Err(ictl_l10n::tr!(
                "since 最多 3600 秒,limit 要在 1 到 1000 之间",
                "since can be at most 3600 seconds, and limit must be between 1 and 1000"
            )
            .into());
        }
        _ => {}
    }
    let seconds = match req {
        Request::LongPress { seconds, .. } | Request::Swipe { seconds, .. } => *seconds,
        _ => return Ok(()),
    };
    if !seconds.is_finite() || !(0.0..=MAX_GESTURE_SECONDS).contains(&seconds) {
        return Err(ictl_l10n::trf!(
            "seconds 要在 0 到 {MAX_GESTURE_SECONDS} 之间",
            "seconds must be between 0 and {MAX_GESTURE_SECONDS}"
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Each device keeps its own background WDA: switching to another device and back attaches to it again, and
    /// `service stop` sees all of them. The last one is still written the old way for older versions
    #[test]
    fn each_device_keeps_its_background_wda() {
        let mut p = Persisted::default();
        assert!(p.record_detached("IPHONE", 10));
        assert!(p.record_detached("IPAD", 20));
        assert!(!p.record_detached("ipad", 20), "the same again changes nothing");
        assert_eq!(p.detached_for("IPHONE"), Some(10), "switching to the iPad kept the iPhone's");
        assert_eq!(p.detached_for("iPad"), Some(20));
        assert_eq!(
            p.detached_wda,
            Some(DetachedWda { udid: "IPAD".into(), pid: 20 }),
            "the last one, for old versions"
        );
        assert_eq!(p.detached_all(), vec![("IPAD".into(), 20), ("IPHONE".into(), 10)]);

        assert!(!p.forget_detached("IPHONE", Some(11)), "another pid: a newer one, kept");
        assert!(p.forget_detached("IPHONE", Some(10)));
        assert_eq!(p.detached_for("IPHONE"), None);
        assert!(p.forget_detached("ipad", None));
        assert_eq!((p.detached_all(), p.detached_wda), (vec![], None));
    }

    /// A state.json from an older version has only the one record: it is still found, attached to and forgotten
    #[test]
    fn an_older_versions_background_wda_is_still_known() {
        let p: Persisted = serde_json::from_str(r#"{"detached_wda":{"udid":"IPHONE","pid":7}}"#).unwrap();
        assert_eq!(p.detached_for("iphone"), Some(7));
        assert_eq!(p.detached_for("IPAD"), None);
        assert_eq!(p.detached_all(), vec![("IPHONE".into(), 7)]);

        let mut q = p.clone();
        assert!(q.forget_detached("IPHONE", Some(7)));
        assert_eq!(q.detached_all(), vec![]);

        let mut r = p;
        assert!(r.record_detached("IPAD", 8));
        assert_eq!(r.detached_all(), vec![("IPAD".into(), 8), ("IPHONE".into(), 7)], "the old record is kept");
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["detached_wdas"], serde_json::json!({"IPAD": 8, "IPHONE": 7}));
        assert_eq!(json["detached_wda"], serde_json::json!({"udid": "IPAD", "pid": 8}));
    }

    /// 重签结果存成时间和成败，按看的人的语言显示；旧字段照旧写中文整句，旧版本的 state.json 照样读
    #[test]
    fn resign_result_is_shown_in_the_reader_language() {
        use ictl_l10n::{Lang, with_lang};
        let mut p = Persisted::default();
        p.record_resign(1_800_000_000, None);
        assert_eq!(p.last_resign.as_deref(), Some("2027-01-15 08:00:00 成功"));
        assert_eq!(with_lang(Lang::En, || p.last_resign_text()).as_deref(), Some("2027-01-15 08:00:00 succeeded"));
        assert_eq!(with_lang(Lang::Zh, || p.last_resign_text()).as_deref(), Some("2027-01-15 08:00:00 成功"));
        p.record_resign(1_800_000_000, Some("boom".into()));
        assert_eq!(with_lang(Lang::En, || p.last_resign_text()).as_deref(), Some("2027-01-15 08:00:00 failed: boom"));

        // 旧版本写的：只有中文整句，原样显示
        let old: Persisted = serde_json::from_str(r#"{"last_resign":"2026-11-07 01:11:54 成功"}"#).unwrap();
        assert_eq!(with_lang(Lang::En, || old.last_resign_text()).as_deref(), Some("2026-11-07 01:11:54 成功"));
        // 新版本写的，旧版本读：多出来的字段不影响（这里用只认旧字段的结构模拟）
        #[derive(serde::Deserialize)]
        struct OldPersisted {
            last_resign: Option<String>,
        }
        let json = serde_json::to_string(&p).unwrap();
        let o: OldPersisted = serde_json::from_str(&json).unwrap();
        assert_eq!(o.last_resign.as_deref(), Some("2027-01-15 08:00:00 失败：boom"));
    }

    pub(crate) fn test_daemon(tag: &str) -> (Arc<Daemon>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ictl-core-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (Daemon::new(Config::from_env(&dir)), dir)
    }

    /// A background check (the page's status and alert polls) gives way: it doesn't wait for the device lock, nor
    /// start while an action waits for it; without the flag the same request waits as before
    #[tokio::test]
    async fn a_background_check_gives_way_to_actions() {
        let (d, dir) = test_daemon("background");
        let busy = |r: &Response| matches!(r, Response::Text { data: Some(crate::protocol::Data::Busy), .. });
        let bg = || CallCtx { background: true, ..Default::default() };
        // the lock taken (an action runs): at once, busy
        let held = d.inner.lock().await;
        let t = Instant::now();
        let r = d.handle(Request::AlertCheck, Instant::now(), bg()).await;
        assert!(busy(&r) && t.elapsed() < Duration::from_millis(100), "{r:?}");
        let plain = d.handle(Request::Status, Instant::now(), CallCtx::default());
        tokio::pin!(plain);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut plain).await.is_err(),
            "a check without the flag waits for the lock"
        );
        drop(held);
        assert!(!busy(&plain.await));
        // an action waiting for the lock: not started
        let waiting = Waiting::new(&d);
        assert!(busy(&d.handle(Request::Status, Instant::now(), bg()).await));
        drop(waiting);
        // nothing else wants the phone: it runs
        let r = d.handle(Request::Status, Instant::now(), bg()).await;
        assert!(matches!(r, Response::Text { data: Some(crate::protocol::Data::Status(_)), .. }), "{r:?}");
        // right after an action: not yet (the alert check waits longer than the status check)
        *d.last_action.lock().unwrap() = Some(Instant::now() - Duration::from_secs(2));
        assert!(
            d.acting_now(Duration::from_secs(3)) && !d.acting_now(Duration::from_secs(1)),
            "slow requests wait too"
        );
        assert!(!busy(&d.handle(Request::Status, Instant::now(), bg()).await), "status: 1 s after");
        assert!(busy(&d.handle(Request::AlertCheck, Instant::now(), bg()).await), "the alert check: 4 s after");
        assert!(
            !busy(&d.handle(Request::AlertCheck, Instant::now(), CallCtx::default()).await),
            "unless asked outright"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 调用方已经断开：还在排队等设备锁的请求不执行
    #[tokio::test]
    async fn queued_request_is_dropped_when_caller_is_gone() {
        let (d, dir) = test_daemon("gone");
        let busy = d.inner.lock().await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(()).unwrap();
        let r = d
            .handle(
                Request::Status,
                Instant::now(),
                CallCtx { gone: Some(rx), expect_udid: None, ..Default::default() },
            )
            .await;
        drop(busy);
        assert!(matches!(&r, Response::Error { message, .. } if message.contains("已取消")), "{r:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn display_identity_expires_on_rotation_resize_and_reconnect() {
        let size = ScreenSize { width: 393.0, height: 852.0 };
        let portrait = Some((size, Rotation::Portrait));
        let mut display = None;
        assert!(DisplayGeometry::update(&mut display, portrait));
        let first = display.clone().unwrap().id;
        assert!(!DisplayGeometry::update(&mut display, portrait));
        assert_eq!(display.as_ref().unwrap().id, first);
        assert!(DisplayGeometry::update(&mut display, Some((size, Rotation::UpsideDown))));
        assert_ne!(display.as_ref().unwrap().id, first);
        let zoom = ScreenSize { width: 375.0, height: 812.0 };
        assert!(DisplayGeometry::update(&mut display, Some((zoom, Rotation::UpsideDown))));
        assert!(DisplayGeometry::update(&mut display, None));
        assert!(!DisplayGeometry::update(&mut display, None));
        assert!(DisplayGeometry::update(&mut display, portrait));
        assert_ne!(display.as_ref().unwrap().id, first);
    }

    #[test]
    fn a_down_dropped_in_the_queue_cannot_be_replayed() {
        let req = Request::Touch { phase: crate::protocol::TouchPhase::Down, x: 1., y: 2. };
        let ctx =
            CallCtx { expect_udid: Some("phone".into()), expect_view: Some("portrait".into()), ..Default::default() };
        let expired = Instant::now() - STALE - Duration::from_secs(1);
        assert!(matches!(
            admit(&req, expired, &ctx, Some("phone")),
            Err(Response::Error { retry_as_gesture: false, .. })
        ));
    }

    #[tokio::test]
    async fn coordinate_from_a_previous_view_is_rejected_before_dispatch() {
        let (d, dir) = test_daemon("previous-view");
        let ctx = CallCtx { expect_view: Some("old-view".into()), ..Default::default() };
        let r = d.handle(Request::Tap { x: 3.0, y: 4.0 }, Instant::now(), ctx).await;
        assert!(matches!(r, Response::Error { message, retry_as_gesture: false } if message.contains("画面")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn transient_status_miss_keeps_the_view_and_finger_until_loss_is_confirmed() {
        let (d, dir) = test_daemon("status-miss-display");
        let mut g = d.inner.lock().await;
        let size = ScreenSize { width: 393., height: 852. };
        DisplayGeometry::update(&mut g.dev.display, Some((size, Rotation::Portrait)));
        let id = g.dev.display.as_ref().unwrap().id.clone();
        g.dev.touch_rotation = Some(Rotation::Portrait);
        for _ in 1..Misses::LIMIT {
            assert!(!d.confirm_wda_loss(&mut g, true).await);
            assert_eq!(g.dev.display.as_ref().unwrap().id, id);
            assert_eq!(g.dev.touch_rotation, Some(Rotation::Portrait));
        }
        assert!(d.confirm_wda_loss(&mut g, true).await);
        assert!(g.dev.display.is_none());
        assert!(g.dev.touch_rotation.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn drag_continuations_only_validate_the_existing_view() {
        let (d, dir) = test_daemon("display-move-up");
        let mut g = d.inner.lock().await;
        DisplayGeometry::update(
            &mut g.dev.display,
            Some((ScreenSize { width: 393., height: 852. }, Rotation::Portrait)),
        );
        let id = g.dev.display.as_ref().unwrap().id.clone();
        // No WDA in this fixture: resampling would invalidate the geometry before the HID path.
        for phase in [crate::protocol::TouchPhase::Move, crate::protocol::TouchPhase::Up] {
            g.dev.touch_rotation = Some(Rotation::Portrait);
            let _ = d.touch(&mut g, phase, 20., 50., Some(&id)).await;
            assert_eq!(g.dev.display.as_ref().unwrap().id, id);
            let stale = d.touch(&mut g, phase, 20., 50., Some("old")).await.unwrap_err();
            assert!(
                matches!(stale, Response::Error { message, retry_as_gesture: false } if message.contains("画面") || message.contains("display changed"))
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_known_view_losing_its_measurement_cannot_start_a_finger() {
        let (d, dir) = test_daemon("lost-display");
        let mut g = d.inner.lock().await;
        let size = ScreenSize { width: 393.0, height: 852.0 };
        DisplayGeometry::update(&mut g.dev.display, Some((size, Rotation::Portrait)));
        let ctx = CallCtx { expect_view: Some(g.dev.display.as_ref().unwrap().id.clone()), ..Default::default() };
        drop(g);
        let req = Request::Touch { phase: crate::protocol::TouchPhase::Down, x: 20.0, y: 50.0 };
        let response = d.handle(req, Instant::now(), ctx).await;
        assert!(matches!(response, Response::Error { message, retry_as_gesture: false } if message.contains("画面")));
        let g = d.inner.lock().await;
        assert!(g.dev.display.is_none());
        assert!(g.dev.touch_rotation.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 批量执行要求的设备不是正在控制的那台（例如拔线后改控了另一台）：不执行
    #[tokio::test]
    async fn request_for_another_device_is_refused() {
        let (d, dir) = test_daemon("expect");
        let ctx = CallCtx { gone: None, expect_udid: Some("00008130-AAAA".into()), ..Default::default() };
        let r = d.handle(Request::Status, Instant::now(), ctx).await;
        assert!(matches!(&r, Response::Error { message, .. } if message.contains("设备变了")), "{r:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 手机上的描述文件：CMS 外壳里包着 XML plist（同 ictl-sign 的解析方式）
    fn profile(app_id: &str, devices: &[&str], expires: &str) -> Vec<u8> {
        let devs: String = devices.iter().map(|d| format!("<string>{d}</string>")).collect();
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
<key>Name</key><string>iOS Team Provisioning Profile</string>
<key>TeamIdentifier</key><array><string>TEAM1</string></array>
<key>ExpirationDate</key><date>{expires}</date>
<key>ProvisionedDevices</key><array>{devs}</array>
<key>Entitlements</key><dict><key>application-identifier</key><string>{app_id}</string></dict>
</dict></plist>"#
        );
        [&[0x30u8, 0x82][..], xml.as_bytes(), &[0x00, 0xa0]].concat()
    }

    #[test]
    fn wda_down_when_no_device_does_not_promise_recovery() {
        assert!(wda_down(&DeviceState::Ready, None).starts_with("WDA 不可用(刚掉线)"));
        let m = wda_down(&DeviceState::Absent, None);
        assert!(m.contains("手机未连接") && !m.contains("1~2 分钟") && !m.contains("可照常用"), "{m}");
    }

    #[test]
    fn wda_down_mentions_hid_fallback_only_from_ios_27() {
        let info = |v: &str| DeviceInfo { product_version: v.into(), ..Default::default() };
        let st = DeviceState::WdaStarting { last_error: None };
        assert!(wda_down(&st, Some(&info("27.0.1"))).contains("可照常用"));
        assert!(wda_down(&st, None).contains("可照常用"));
        let old = wda_down(&st, Some(&info("26.6.1")));
        assert!(old.contains("没有 HID 后备路线") && !old.contains("可照常用"), "{old}");
    }

    const WDA: &str = "com.facebook.WebDriverAgentRunner.xctrunner.TEAM1";

    /// 和上面 XML 里的日期对应的时刻
    fn at(s: &str) -> SystemTime {
        let secs = match s {
            "2026-11-10T15:35:00Z" => 1794324900,
            "2026-11-12T00:00:00Z" => 1794441600,
            _ => unreachable!(),
        };
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// 真机情况：iPhone 上装的是 11-03 签的（11-10 到期），之后给 iPad 签的那份（11-13）只装到了 iPad 上
    #[test]
    fn wda_profile_is_the_one_on_this_phone() {
        let iphone = "00008130-0019295E012B803A";
        let ps = vec![
            profile(&format!("TEAM1.{WDA}"), &[iphone], "2026-11-10T15:35:00Z"),
            profile("TEAM1.com.example.other", &[iphone], "2027-01-01T00:00:00Z"),
            profile(&format!("TEAM2.{WDA}"), &[iphone], "2027-01-01T00:00:00Z"),
            profile(&format!("TEAM1.{WDA}"), &["00008020-000925043499002E"], "2026-11-13T12:26:31Z"),
            b"not a profile".to_vec(),
        ];
        assert_eq!(wda_profile_expiry(&ps, Some("TEAM1"), WDA, iphone), Some(at("2026-11-10T15:35:00Z")));
        // UDID 大小写不敏感；装过两次取晚的
        let mut twice = ps.clone();
        twice.push(profile(&format!("TEAM1.{WDA}"), &[&iphone.to_lowercase()], "2026-11-12T00:00:00Z"));
        assert_eq!(wda_profile_expiry(&twice, Some("TEAM1"), WDA, iphone), Some(at("2026-11-12T00:00:00Z")));
        // 没装过：没有包含这台手机的
        assert_eq!(wda_profile_expiry(&ps, Some("TEAM1"), WDA, "00008101-AAAA"), None);
        // 不知道团队 ID 时只看 Bundle ID
        assert!(wda_profile_expiry(&ps, None, WDA, iphone).is_some());
    }

    #[test]
    fn device_profile_left_falls_back_to_signing_record_only_when_unknown() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let p = Persisted { profile_expires: Some(now + 6 * 86400), ..Default::default() };
        let days = |d: Option<Duration>| d.map(|d| (d.as_secs() + 3600) / 86400);
        assert_eq!(days(DeviceProfile::Expires(now + 4 * 86400).left(&p)), Some(4));
        assert_eq!(days(DeviceProfile::Unknown("x".into()).left(&p)), Some(6));
        assert_eq!(days(DeviceProfile::NotChecked.left(&p)), Some(6));
        assert_eq!(DeviceProfile::Missing.left(&p), None);
        assert_eq!(DeviceProfile::Expires(now - 10).left(&p), Some(Duration::ZERO));
    }

    #[test]
    fn gesture_seconds_are_bounded() {
        let swipe = |s| Request::Swipe { from: (0.0, 0.0), to: (1.0, 1.0), seconds: s };
        assert!(check_request(&swipe(0.3)).is_ok());
        assert!(check_request(&swipe(1e6)).is_err());
        assert!(check_request(&swipe(-1.0)).is_err());
        assert!(check_request(&swipe(f64::NAN)).is_err());
        assert!(check_request(&Request::LongPress { x: 0.0, y: 0.0, seconds: 10.0 }).is_ok());
    }

    #[test]
    fn corrupt_state_file_loads_as_default() {
        let p = std::env::temp_dir().join(format!("ictl-state-{}.json", std::process::id()));
        std::fs::write(&p, b"{not json").unwrap();
        assert!(Persisted::load(&p).wda_bundle.is_none());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn an_unlock_first_is_said_on_the_answers_first_line() {
        let took = Some(Duration::from_millis(14_100));
        // an action with `then=elements`: the note goes on the action's line, the screen after it stays below
        let r = with_unlock_note(
            Response::text_with("已按 home\n--- 操作后的界面 ---\nButton \"设置\"", Default::default()),
            took,
        );
        let Response::Text { text, outcome, .. } = &r else { panic!("{r:?}") };
        let lines: Vec<_> = text.lines().collect();
        assert!(lines[0].starts_with("已按 home") && lines[0].contains("14.1"), "{text}");
        assert_eq!(lines[1..], ["--- 操作后的界面 ---", "Button \"设置\""]);
        assert!(outcome.is_some());
        // a screenshot's caption, and an action that failed after the unlock
        let r = with_unlock_note(Response::Image { jpeg_base64: "x".into(), caption: "430x932".into() }, took);
        assert!(
            matches!(&r, Response::Image { caption, .. } if caption.starts_with("430x932") && caption.contains("14.1"))
        );
        let r = with_unlock_note(Response::error("没找到"), took);
        assert!(
            matches!(&r, Response::Error { message, .. } if message.starts_with("没找到") && message.contains("14.1"))
        );
        // no unlock: the answer as it was
        let plain = Response::text_with("已按 home", Default::default());
        assert_eq!(with_unlock_note(plain.clone(), None), plain);
    }
}
