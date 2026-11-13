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
    /// `screen` 是哪台设备的；换了设备就不能拿来给 HID 换算坐标
    pub screen_udid: Option<String>,
    /// 上次重签的结果，旧格式：拼好的中文整句。照旧写，旧版本（降级、旧 CLI）还读它；新版本显示时用
    /// [`Self::resign_result`]
    pub last_resign: Option<String>,
    /// 上次重签的结果，按看的人的语言显示（[`Self::last_resign_text`]）。旧版本写的 state.json 没有它
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resign_result: Option<ResignRecord>,
    /// 运行中选定要控制的设备（`iphone-control select`、`iphone_select_device`）；None 是自动选。
    /// 设了 `IPHONE_UDID` 时以它为准，这个不起作用
    pub selected_udid: Option<String>,
    /// 实验（`ICTL_WDA_DETACHED`）：后台启动的 WDA 是哪台设备上的哪个进程。守护进程重启后据此直接接上它，
    /// 不重新启动；关掉实验开关后据此结束它（不然它一直占着 8100 端口，新 runner 的 HTTP 服务起不来）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_wda: Option<DetachedWda>,
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
    /// 这台手机上 Home 先用 XCTest 的：上一次 IOHID 的没回到主屏、XCTest 的回到了（iPhone 18 Pro Max /
    /// iOS 27.2.1 上 IOHID 的 Home 基本不起作用）。iPhone 12 mini 正相反，所以按设备记，见 `tools::press_home`
    pub home_via_xctest: bool,
    /// 这台手机上锁屏先用 WDA 的 `/wda/lock`：上一次经 IOHID 短按电源键没锁上、WDA 的锁上了。iPhone 15 Pro Max
    /// （iOS 27.0.1）上短按电源键不起作用（调用卡约 5 秒，再等 3 秒才退回，一次锁屏约 10 秒），`/wda/lock` 约 2 秒；
    /// iPhone 18 Pro Max（iOS 27.2.1）正相反，所以按设备记，见 `tools::lock_order`
    pub lock_via_wda: bool,
    /// The orientation of the finger the web viewer is moving (`Request::Touch`), worked out when it touched down
    pub touch_rotation: Option<Rotation>,
    pub apps: HashMap<bool, Vec<AppInfo>>,
    /// Log capture for `AppLogs` (developer tools), started by the first request
    pub log_capture: Option<crate::devtools::Capture>,
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
    /// HID 会话最近一次使用的时间；空闲超过 [`crate::supervise::HID_IDLE`] 就关掉（它持有屏幕媒体流）
    pub hid_used: Option<Instant>,
    /// Control pages watching the screen media stream (`screen.rs`): while there are any, the HID session (which owns
    /// the stream) stays open however long it is idle
    pub screen_viewers: usize,
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
        t.close().await;
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
            hid_used: None,
            screen_viewers: 0,
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
            auto_unlock: false,
            unlock_disabled: cfg.unlock_disabled_flag().exists(),
            notify_configured: cfg.notify.any(),
            log_tail: Vec::new(),
        };
        let (snap, _) = watch::channel(first);
        Arc::new(Self { cfg, log, inner: Mutex::new(inner), snap, web: Mutex::new(None), heartbeat: Heartbeat::new() })
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
        let (title, body) = ictl_l10n::in_own_lang(|| e.title_body());
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
            g.tunnel = Some(device.open_tunnel().await?);
        }
        Ok(g.tunnel.as_mut().expect("刚建好"))
    }

    /// 不再控制当前这台（断开、重新接入、换设备）：停掉它的会话，清掉关于它的一切
    pub(crate) async fn forget_device(&self, g: &mut Inner) {
        self.drop_session(g).await;
        g.dev = PerDevice::default();
    }

    /// WDA 可用就返回客户端（本进程 runner 存活）
    pub(crate) fn wda<'a>(&self, g: &'a mut Inner) -> Option<&'a mut WdaClient<DeviceTransport>> {
        if g.runner.as_ref().is_some_and(|r| r.alive()) { g.wda.as_mut() } else { None }
    }

    pub(crate) fn require_wda<'a>(&self, g: &'a mut Inner) -> Result<&'a mut WdaClient<DeviceTransport>, String> {
        let msg = wda_down(&g.state, g.info.as_ref());
        self.wda(g).ok_or(msg)
    }

    pub(crate) fn account(&self) -> ictl_sign::AccountDir {
        ictl_sign::AccountDir { dir: self.cfg.account_dir(), anisette_url: self.cfg.anisette_url.clone() }
    }

    /// 发布快照给不拿锁的读者（`status`）。日志尾巴在读的时候再取
    pub(crate) fn publish(&self, g: &Inner) {
        self.heartbeat.beat();
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
        let text = |r: Result<String, String>| {
            r.map(|t| Response::text_with(t, crate::protocol::Outcome::default())).unwrap_or_else(Response::error)
        };
        match lane(&req) {
            Lane::Snapshot => unreachable!("handled above"),
            // 重签自己分段持锁：签名、安装要几十秒，期间不挡住别的请求
            Lane::Resign => {
                let Request::Resign { force } = req else { unreachable!("lane") };
                return self.resign_now(force).await.map(Response::text).unwrap_or_else(Response::error);
            }
            // Web Inspector talks to the page, not to WDA: its own lock, the device lock only to look up the device
            Lane::Web => {
                let mut slot = match acquire(&self.web, &mut ctx.gone).await {
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

        let mut g = match acquire(&self.inner, &mut ctx.gone).await {
            Ok(g) => g,
            Err(r) => return r,
        };
        let g = &mut *g;
        if let Err(r) = admit(&req, queued_at, &ctx, g.device.as_ref().map(|d| d.udid.as_str())) {
            return r;
        }
        let view = ctx.expect_view.as_deref();
        // 改变界面的操作做完后缓存的元素树就作废；只有调用方声明「不改变布局」的按名字点击（max_age_ms > 0）保留
        let keeps_cache = matches!(req, Request::TapElement { max_age_ms, .. } if max_age_ms > 0);
        let invalidates = req.is_device_action() && !keeps_cache;
        let response = self.dispatch(g, req, view).await;
        if invalidates {
            g.elements_cache = None;
        }
        self.publish(g);
        response
    }

    async fn dispatch(&self, g: &mut Inner, req: Request, view: Option<&str>) -> Response {
        // what the text reports, for clients that act on it (Response::Text.outcome)
        let mut outcome = crate::protocol::Outcome::default();
        let r = match req {
            Request::Status => Ok(self.status_text(g).await),
            // streams on its own connection (`lib::serve_conn` hands it to `screen::stream`); never queued here
            Request::ScreenStream => {
                Err("screen_stream is a streaming request: open it with open_screen_stream".into())
            }
            Request::Screenshot { region } => {
                return self.screenshot(g, region).await.unwrap_or_else(Response::error);
            }
            Request::ListElements { filter, limit, max_age_ms } => {
                self.list_elements(g, &filter, limit, Duration::from_millis(max_age_ms)).await
            }
            Request::Tap { x, y } => self.tap(g, x, y, view).await,
            Request::TapElement { text, index, max_age_ms } => {
                self.tap_element(g, &text, index, Duration::from_millis(max_age_ms)).await.map(|(t, missed)| {
                    outcome.tapped_nothing = missed;
                    t
                })
            }
            Request::LongPress { x, y, seconds } => self.long_press(g, x, y, seconds, view).await,
            Request::Swipe { from, to, seconds } => self.swipe(g, from, to, seconds, view).await,
            Request::TypeText { text, submit } => self.type_text(g, &text, submit).await,
            Request::PressButton { button } => self.press_button(g, &button).await,
            Request::Touch { phase, x, y } => {
                return self
                    .touch(g, phase, x, y, view)
                    .await
                    .map(|()| Response::text_with("", outcome))
                    .unwrap_or_else(|e| e);
            }
            Request::ListApps { include_system } => self
                .list_apps(g, include_system)
                .await
                .map(|a| a.iter().map(|a| format!("{}\t{}", a.name, a.bundle_id)).collect::<Vec<_>>().join("\n")),
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
            Request::AlertCheck => self.alert_check(g).await,
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
            | Request::WebEval { .. } => unreachable!("other lanes"),
        };
        r.map(|t| Response::text_with(t, outcome)).unwrap_or_else(Response::error)
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
}

fn lane(req: &Request) -> Lane {
    match req {
        Request::Daemon => Lane::Snapshot,
        Request::Resign { .. } => Lane::Resign,
        Request::WebTargets | Request::WebEval { .. } => Lane::Web,
        Request::InstallApp { .. } => Lane::Install,
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
}
