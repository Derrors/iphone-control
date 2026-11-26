//! WDA HTTP 客户端。会话失效（WDA 重启后）自动重建一次。

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use ictl_device::{Rotation, ScreenSize};
use ictl_ui as elements;
use serde_json::{Value, json};

use crate::http::Transport;
use crate::{Result, WdaError};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// [`WdaClient::xctest_responsive`] 的时限。正常几十到几百毫秒；守护进程每轮巡检问一次，等太久会拖住巡检
pub const XCTEST_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// 经 XCTest 的请求连续这么多次送到了却没有应答（超时，或应答到一半断了），就当 WDA 卡死（见 [`WdaClient::stalled`]）
pub const STALL_LIMIT: u32 = 3;
/// 高位单独记录「长请求及其短探测都未应答」，低位仍是实际的连续无应答数。
const UNANSWERED_PROBE: u32 = 1 << 31;

/// 无响应的判定依据，不等同于已证明设备端永久死锁。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallReason {
    ConsecutiveRequests(u32),
    LongRequestAndProbe,
}

impl StallReason {
    pub fn describe(self) -> String {
        match self {
            Self::ConsecutiveRequests(n) => {
                ictl_l10n::trf!("连续 {n} 次请求没有完整应答", "{n} requests in a row had no complete reply")
            }
            Self::LongRequestAndProbe => ictl_l10n::trf!(
                "较长的请求和随后 {} 秒的 /wda/activeAppInfo 探测都没有完整应答",
                "a long request and the following {} s /wda/activeAppInfo probe both had no complete reply",
                XCTEST_PROBE_TIMEOUT.as_secs()
            ),
        }
    }
}
/// How long a measured screen size is used before it is measured again, and then in the background
/// ([`WdaClient::display_sample`] with `refresh_stale`), not at the start of a gesture: only Display Zoom changes the
/// size, and `/wda/screen` takes about 100 ms on an iPhone with iOS 27 but 0.95 s on an iPad mini 5 (iPadOS 26.7.3),
/// which the first tap or swipe after a minute paid on top of its own 0.85 s
const SCREEN_RECHECK: Duration = Duration::from_secs(60);
/// 普通点击按下的时长（毫秒）。太短可能不被识别成点击；解锁按键另有自己的时长
const TAP_PRESS_MS: u64 = 50;
/// 一个手势里依次点几个点时，前一个抬起到后一个按下停多久（毫秒）。真机上 20、40、60 都没有漏键、多键
pub const SEQUENCE_GAP_MS: u64 = 40;
/// 连点时两下之间停多久（毫秒）：双击要在约 250 毫秒内，「800 毫秒内点 5 下」要每下不到 160 毫秒
pub const MULTI_TAP_GAP_MS: u64 = 100;

/// W3C pointer 动作的一步
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerStep {
    Move { x: f64, y: f64, ms: u64 },
    Down,
    Up,
    Pause { ms: u64 },
}

/// 单指点一下：按下停 `press_ms` 再抬起
pub fn tap_steps(x: f64, y: f64, press_ms: u64) -> Vec<PointerStep> {
    vec![PointerStep::Move { x, y, ms: 0 }, PointerStep::Down, PointerStep::Pause { ms: press_ms }, PointerStep::Up]
}

/// 同一点连点 `count` 下，两下之间停 `gap_ms`：一个手势，一根手指只移动一次。
/// 一根手指在同一个手势里移到别的点再按，iPhone 17（iOS 26.5.2）上每下都被识别两次（数字键盘上点 7 个键输出 14 位），
/// 所以这里只点一个点
pub fn multi_tap_steps(x: f64, y: f64, count: u32, press_ms: u64, gap_ms: u64) -> Vec<PointerStep> {
    let mut steps = vec![PointerStep::Move { x, y, ms: 0 }];
    for i in 0..count {
        if i > 0 {
            steps.push(PointerStep::Pause { ms: gap_ms });
        }
        steps.extend([PointerStep::Down, PointerStep::Pause { ms: press_ms }, PointerStep::Up]);
    }
    steps
}

/// 滑动：按下、停 50ms、在 `ms` 毫秒内移到终点、抬起（与 toolkit 一致）
pub fn swipe_steps(from: (f64, f64), to: (f64, f64), ms: u64) -> Vec<PointerStep> {
    vec![
        PointerStep::Move { x: from.0, y: from.1, ms: 0 },
        PointerStep::Down,
        PointerStep::Pause { ms: 50 },
        PointerStep::Move { x: to.0, y: to.1, ms },
        PointerStep::Up,
    ]
}

/// 按住再拖：按下后先原地按住 `hold_ms`，再用 `ms` 移到终点。按住要写成一段原地的 pointerMove，不能是 pause：
/// iPhone 12 mini（iOS 26.4.1）上主屏幕图标按 pause 按住 0.6～1 秒再移动，0/6 次拖得动（只当成滑动翻页），
/// 原地 move 0.5 秒再移动 3/3，0.3 秒 0/3。`hold_ms` 为 0 时就是 [`swipe_steps`]
pub fn drag_steps(from: (f64, f64), to: (f64, f64), hold_ms: u64, ms: u64) -> Vec<PointerStep> {
    if hold_ms == 0 {
        return swipe_steps(from, to, ms);
    }
    vec![
        PointerStep::Move { x: from.0, y: from.1, ms: 0 },
        PointerStep::Down,
        PointerStep::Move { x: from.0, y: from.1, ms: hold_ms },
        PointerStep::Move { x: to.0, y: to.1, ms },
        PointerStep::Up,
    ]
}

/// 一个 actions 请求里依次点几个点，每个点一根手指：第 k 根手指先空等前面 k 个点按完（每个点占 5 个节拍：移动、按下、
/// 按住 `press_ms`、抬起、停 `gap_ms`），再移过去按下抬起。W3C 按节拍对齐各手指，所以这些点一个接一个、不重叠。
/// 不用一根手指移来移去：iPhone 17（iOS 26.5.2）上一根手指在一个手势里移到别的点再按，每个点都被识别两次
/// （数字键盘上点 7 个键输出 14 位）
pub fn tap_sequence_body(points: &[(f64, f64)], press_ms: u64, gap_ms: u64) -> Value {
    let pause = |ms: u64| json!({"type": "pause", "duration": ms});
    let fingers: Vec<Value> = points
        .iter()
        .enumerate()
        .map(|(k, &(x, y))| {
            let mut actions = Vec::with_capacity(5 * k + 4);
            for _ in 0..k {
                actions.extend([pause(0), pause(0), pause(press_ms), pause(0), pause(gap_ms)]);
            }
            actions.extend([
                json!({"type": "pointerMove", "duration": 0, "x": x, "y": y}),
                json!({"type": "pointerDown", "button": 0}),
                pause(press_ms),
                json!({"type": "pointerUp", "button": 0}),
            ]);
            json!({"type": "pointer", "id": format!("finger{}", k + 1), "parameters": {"pointerType": "touch"}, "actions": actions})
        })
        .collect();
    json!({"actions": fingers})
}

/// `POST /session/{id}/actions` 的请求体
pub fn actions_body(steps: &[PointerStep]) -> Value {
    let actions: Vec<Value> = steps
        .iter()
        .map(|s| match *s {
            PointerStep::Move { x, y, ms } => json!({"type": "pointerMove", "duration": ms, "x": x, "y": y}),
            PointerStep::Down => json!({"type": "pointerDown", "button": 0}),
            PointerStep::Up => json!({"type": "pointerUp", "button": 0}),
            PointerStep::Pause { ms } => json!({"type": "pause", "duration": ms}),
        })
        .collect();
    json!({"actions": [{"type": "pointer", "id": "finger1", "parameters": {"pointerType": "touch"}, "actions": actions}]})
}

/// appium settings 中与等待有关的两项。锁屏解锁、点击期间要临时调小。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdleSettings {
    pub wait_for_idle_timeout: f64,
    pub animation_cool_off_timeout: f64,
}

impl IdleSettings {
    /// WDA 的默认值
    pub const DEFAULT: Self = Self { wait_for_idle_timeout: 10.0, animation_cool_off_timeout: 2.0 };
    /// 锁屏解锁期间临时调小的值（0 会导致锁屏唤醒失败，所以不是 0）。点击、长按不用它，见 [`WdaClient::tap`]
    pub const QUICK: Self = Self { wait_for_idle_timeout: 1.0, animation_cool_off_timeout: 0.2 };
    /// 不等动画冷却：滑动、以及「等界面稳定」时轮询元素树用（由调用方比较前后两次判断稳定）。真机（iOS 27，设置列表）：
    /// - 上下滑 0.3 秒：冷却 0 约 1.0 秒；默认 2 秒约 3 秒；0.2～1.5 秒反而要 18～19 秒（惯性滚动期间一直等）
    /// - 滑完马上读元素树：冷却 0 约 0.2 秒；默认约 2.2 秒；0.2 秒要约 18 秒
    pub const NO_COOL_OFF: Self = Self { wait_for_idle_timeout: 1.0, animation_cool_off_timeout: 0.0 };
}

/// 读元素树时 WDA 快照的上限：appium settings 的 `snapshotMaxDepth`、`snapshotMaxChildren`，新会话重置成 WDA 的默认
/// （50 层、子元素不限，WDA 16.15.3 `FBConfiguration resetSessionSettings`）。
///
/// 元素很多的界面（信息流、很长的列表）一次快照要很久，WDA 又在主线程上一个一个处理请求，后面的点击、截图都排在它后面。
/// 读得慢（[`SLOW_SOURCE`]）或超时就收紧一级，读得快（[`FAST_SOURCE`]）就放宽一级（[`next_snapshot_level`]）。
/// 先限子元素个数：长列表是宽不是深，只减层数没用；再把层数、子元素数各减一半，最少 12 层、60 个。
/// 级别和数字参考 Poki 的 PokiCore（docs/ios-core.md「A tree that times out is read again shallower」）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotLimits {
    pub depth: u32,
    /// None = 不限（WDA 的 `INT_MAX`）
    pub children: Option<u32>,
}

/// 从不限到最紧，[`WdaClient`] 记的是下标
pub const SNAPSHOT_LEVELS: [SnapshotLimits; 4] = [
    SnapshotLimits { depth: 50, children: None },
    SnapshotLimits { depth: 50, children: Some(150) },
    SnapshotLimits { depth: 25, children: Some(75) },
    SnapshotLimits { depth: 12, children: Some(60) },
];
/// 读一次元素树超过这么久就收紧一级。排除慢属性后平时 0.2～2 秒
pub const SLOW_SOURCE: Duration = Duration::from_secs(8);
/// 收紧过的读得比这快就放宽一级
pub const FAST_SOURCE: Duration = Duration::from_millis(1500);
/// 读元素树的时限：再长就算 WDA 卡住了（见 [`STALL_LIMIT`]）
const SOURCE_TIMEOUT: Duration = Duration::from_secs(60);
/// 结束会话（[`WdaClient::in_session`] 里动作没有应答时）的时限。WDA 不排队处理它，平时立刻应答
const SESSION_DELETE_TIMEOUT: Duration = Duration::from_secs(3);

/// 读完一次元素树后下次用哪一级上限：`took` 为 None 表示超时
pub fn next_snapshot_level(level: usize, took: Option<Duration>) -> usize {
    let tightest = SNAPSHOT_LEVELS.len() - 1;
    match took {
        None => (level + 1).min(tightest),
        Some(t) if t >= SLOW_SOURCE => (level + 1).min(tightest),
        Some(t) if t < FAST_SOURCE => level.saturating_sub(1),
        Some(_) => level,
    }
}

/// WDA 判断前台 App 用的点（见 `WdaClient::move_detection_point`）：横竖屏都在屏幕里、在通知横幅和灵动岛底下。
/// 取短边的一半，横竖都一样：竖屏在屏幕上部偏中间（430×932 的 iPhone 上是 (215, 215)，横幅最低到 125 左右），
/// 横屏在中间偏左
pub fn detection_point(screen: ScreenSize) -> (u32, u32) {
    let half = (screen.width.min(screen.height) / 2.0).round() as u32;
    (half, half)
}

/// 改变手机状态的请求：没有应答时要结束会话，免得它和排在它后面的请求过一阵才执行（见 [`WdaClient::in_session`]）。
/// 读的请求（GET、`POST /element`、`POST /elements` 找元素）不算：排在读后面的东西不会动手机，结束会话只会让下一次开会话
/// 也排到它后面。对元素的操作（`POST /element/:id/clear`）算
fn acts(method: &str, path: &str) -> bool {
    method != "GET" && !path.ends_with("/element") && !path.ends_with("/elements")
}

/// The element a find or `/element/active` answered: `{"ELEMENT": id, "element-6066-…": id}`
fn element_id(v: &Value) -> Option<String> {
    let o = v.as_object()?;
    o.get("ELEMENT").or_else(|| o.values().next())?.as_str().map(str::to_owned)
}

/// The element with keyboard focus ([`WdaClient::focused_field`])
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedField {
    /// WDA's id for it (`/element/:id/…`)
    pub id: String,
    /// The type without `XCUIElementType`: `TextField`, `SearchField`, `TextView`, `SecureTextField`…
    pub kind: String,
    pub placeholder: String,
}

/// 正常响应和远端错误都证明请求获得了回答；连接失败则不算。
fn answered(reply: &Result<Value>) -> bool {
    matches!(reply, Ok(_) | Err(WdaError::Remote { .. }))
}

/// Frames a second and size (percent) of WDA's MJPEG stream with [`WdaClient::fast_stream`].
///
/// Full size, so WDA passes on the JPEG that testmanagerd encodes in hardware. Any other size makes WDA decode,
/// scale (CoreGraphics) and re-encode every frame: at 75 % that was 86–93 % of a core on an iPhone 15 Pro Max
/// (iOS 27.0.1), and iOS kills a process above 80 % over 60 s (`cpu_resource_fatal`), so with the picture open WDA
/// was killed every 1.5–2.5 minutes. At 100 % it delivers the same ~34 fps there, with smaller frames
/// (1290×2796, about 80 KB against 104 KB), and ran 15 minutes without being killed. Set explicitly: a WDA left
/// running in the background (the daemon launches it so) keeps the value an older version set
const MJPEG_FPS: u32 = 60;
const MJPEG_SCALE: u32 = 100;

/// 主屏、锁屏、系统弹窗所属的进程
pub const SPRINGBOARD: &str = "com.apple.springboard";
/// 系统 App 第一次打开前的隐私说明页（「欢迎使用“备忘录”… 接受并继续」）。它在自己的进程里，盖在要启动的 App 上面，
/// 点了它的按钮 App 才会到前台
pub const PRIVACY_SHEET: &str = "com.apple.PDUIApp";
/// IOHID 消费者页（Consumer）
const HID_PAGE_CONSUMER: u32 = 0x0C;
/// 消费者页里的电源键
const HID_POWER: u32 = 0x30;
/// 电源键按多久。太长太短都锁不了屏，实测（每次先解锁，按完看 `/wda/locked`）：
/// - iPhone 18 Pro Max（iOS 27.2.1）：0.05～0.15 秒约 0.7 秒锁屏；按住 0.5 秒没反应（当成了长按）
/// - iPhone 15 Pro Max（iOS 27.0.1）：0.05、0.1 秒调用卡约 5.3 秒、不锁；0.15、0.2、0.3 秒都在 3 秒内锁上
/// - iPad mini 5（iPadOS 26.7.3）：0.1 秒同样卡约 5.4 秒、不锁；0.15、0.2、0.3、0.5 秒都在 1 秒内锁上
///
/// 0.15 秒是这几台都能锁的
const POWER_PRESS_SECS: f64 = 0.15;
/// 消费者页里的 AC Home
const HID_AC_HOME: u32 = 0x223;
/// 启动 App 后至少过这么久才返回：App 切到前台的放大动画约 0.5 秒，期间的点击会被吞
const LAUNCH_MIN: Duration = Duration::from_millis(800);

/// [`WdaClient::launch_app`] 的结果
#[derive(Debug, Clone, PartialEq)]
pub enum Launch {
    /// 到了前台
    Foreground,
    /// 系统弹窗挡着（弹窗文字）
    Blocked(String),
    /// 8 秒内没到前台，也没有弹窗
    Slow,
}

/// WDA 不认识这个路径（旧版本没有的接口）时的报错，同 [`WdaClient::interface_rotation`]
fn unknown_route(kind: &str, message: &str) -> bool {
    matches!(kind, "unknown command" | "unknown method" | "unsupported operation")
        || (kind == "http" && matches!(message, "404" | "405"))
}

/// [`WdaClient::check_lock`] 的结果
#[derive(Debug)]
pub struct LockCheck {
    pub locked: bool,
    /// 拿不准：WDA 说没锁，屏幕上却是锁屏样式的界面，认不出是锁屏还是通知中心（见 [`WdaClient::check_lock`]）。
    /// `locked` 这时为 false；能自动解锁的调用方应当按可能锁着处理
    pub uncertain: bool,
    /// 核对时取到的 SpringBoard 元素树（没核对或前台不是 SpringBoard 时为 None）
    pub tree: Option<elements::Source>,
}

pub struct WdaClient<T> {
    transport: T,
    session: Option<String>,
    /// `/wda/screen` 第一次给的尺寸（当时的方向）。当前方向下的宽高见 [`Self::view`]
    screen: Option<ScreenSize>,
    /// When [`Self::screen`] was measured
    screen_at: Option<Instant>,
    rotation_unsupported: bool,
    /// 本客户端（对应一次 WDA 启动）是否确认过一次「确实没锁屏」，确认后 `/wda/locked` 就可信了
    lock_state_trusted: bool,
    /// 点击前后要恢复的等待设置（第一次点击时读一次）
    idle_baseline: Option<IdleSettings>,
    /// What WDA has now, as far as we know: sending the same again is a wasted round trip
    idle_current: Option<IdleSettings>,
    /// See [`Self::fast_stream`]
    fast_stream: bool,
    /// 低位为连续无应答数；高位记录长请求后的短探测也未应答，详见 [`Self::stall_reason`]。
    /// 工具调用和守护进程的探测都算；获得正常回答或远端错误才清零。
    unanswered: AtomicU32,
    /// 下次读元素树用的上限，[`SNAPSHOT_LEVELS`] 的下标
    snapshot_level: usize,
    /// WDA 现在用的上限。None = 不知道：这个客户端还没开过会话、也没设过；开会话时 WDA 重置成不限（0）。
    /// 不知道时按不限算，不为读树专门开会话：守护进程重启后接上的后台 WDA 若留着上一个客户端收紧的值，
    /// 第一次开会话（第一次点击、改设置）就重置了
    snapshot_applied: Option<usize>,
    /// 上一次读到的元素树用的上限（[`Self::snapshot_note`]）
    snapshot_read: usize,
    /// WDA 判断前台 App 的点已经挪过（[`detection_point`]）。这个值在 WDA 进程里一直有效，换会话也不重置
    detection_point_set: bool,
}

// WdaClient's methods, by what they do: session (transport, XCTest session, stalls), screen (what is on it),
// actions (gestures, buttons, apps), typing (the focused field)
mod actions;
mod screen;
mod session;
mod typing;

#[cfg(test)]
mod tests;
