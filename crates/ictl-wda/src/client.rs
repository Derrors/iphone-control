//! WDA HTTP 客户端。会话失效（WDA 重启后）自动重建一次。

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use ictl_device::{Rotation, ScreenSize};
use serde_json::{Value, json};

use crate::http::Transport;
use crate::{Result, WdaError, elements};

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

/// WDA 判断前台 App 用的点（见 [`WdaClient::move_detection_point`]）：横竖屏都在屏幕里、在通知横幅和灵动岛底下。
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
    pub tree: Option<String>,
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
    /// WDA 判断前台 App 的点已经挪过（[`Self::detection_point`]）。这个值在 WDA 进程里一直有效，换会话也不重置
    detection_point_set: bool,
}

impl<T: Transport> WdaClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            session: None,
            screen: None,
            screen_at: None,
            rotation_unsupported: false,
            lock_state_trusted: false,
            idle_baseline: None,
            idle_current: None,
            fast_stream: false,
            unanswered: AtomicU32::new(0),
            snapshot_level: 0,
            snapshot_applied: None,
            snapshot_read: 0,
            detection_point_set: false,
        }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        self.call_or_abandon(method, path, body, timeout, None).await
    }

    /// [`Self::call`]；`abandon` 是会话号时，请求没有应答就先结束这个会话，再判断 WDA 是不是卡死了（见 [`Self::in_session`]）
    async fn call_or_abandon(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
        abandon: Option<&str>,
    ) -> Result<Value> {
        // `/status` 不经 XCTest：WDA 半死时它照样应答，不能拿它算 WDA 还活着
        if path == "/status" {
            return self.send(method, path, body, timeout).await;
        }
        // 已判定卡死：不再发、不再一次次等超时（一个工具调用里接连几个请求，每个都等满时限的话，
        // 设备会被占好几分钟，守护进程没机会重启 WDA）
        if let Some(reason) = self.stall_reason() {
            let why = reason.describe();
            return Err(WdaError::Unavailable(ictl_l10n::trf!(
                "WDA 卡死判定：{why}，等守护进程重启它",
                "WDA is considered stuck: {why}; waiting for the daemon to restart it"
            )));
        }
        let r = self.send(method, path, body, timeout).await;
        match &r {
            Err(e) if e.no_answer() => {
                if let Some(sid) = abandon {
                    self.delete_session(sid).await;
                }
                let n = self.unanswered.fetch_add(1, Ordering::Relaxed) + 1;
                // 长请求未应答，补一次短探测；保留实际计数，不把两个请求写成三次无应答。
                if n < STALL_LIMIT && timeout > XCTEST_PROBE_TIMEOUT {
                    match self.send("GET", "/wda/activeAppInfo", None, XCTEST_PROBE_TIMEOUT).await {
                        Err(p) if p.no_answer() => {
                            self.unanswered.store((n + 1) | UNANSWERED_PROBE, Ordering::Relaxed);
                        }
                        reply if answered(&reply) => self.unanswered.store(0, Ordering::Relaxed),
                        _ => {}
                    }
                }
            }
            _ if answered(&r) => self.unanswered.store(0, Ordering::Relaxed),
            _ => {}
        }
        r
    }

    /// 连续三次无应答，或长请求和其短探测均无应答时，后续请求不再发送，等待守护进程恢复。
    pub fn stalled(&self) -> bool {
        self.stall_reason().is_some()
    }

    /// 读取一致的计数与判定原因；`/status` 的响应不参与此状态。
    pub fn stall_reason(&self) -> Option<StallReason> {
        let state = self.unanswered.load(Ordering::Relaxed);
        if state & UNANSWERED_PROBE != 0 {
            Some(StallReason::LongRequestAndProbe)
        } else if state >= STALL_LIMIT {
            Some(StallReason::ConsecutiveRequests(state))
        } else {
            None
        }
    }

    /// 实际连续无应答次数，不含判定标记。
    pub fn unanswered(&self) -> u32 {
        self.unanswered.load(Ordering::Relaxed) & !UNANSWERED_PROBE
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        let body = body.map(|b| b.to_string().into_bytes());
        let (status, raw) = self.transport.send(method, path, body, timeout).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::TimedOut => WdaError::Timeout(e.to_string()),
            crate::http::WRITE_FAILED => WdaError::Write(e.to_string()),
            crate::http::AFTER_SENT => WdaError::Interrupted(e.to_string()),
            _ => WdaError::Unavailable(e.to_string()),
        })?;
        let v: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(_) if status >= 400 => {
                return Err(WdaError::Remote { kind: "http".into(), message: status.to_string() });
            }
            Err(e) => {
                return Err(WdaError::Interrupted(ictl_l10n::trf!("响应不是 JSON：{e}", "response is not JSON: {e}")));
            }
        };
        if let Some(err) = v.get("value").and_then(|x| x.get("error")).and_then(|e| e.as_str()) {
            let msg = v["value"].get("message").and_then(|m| m.as_str()).unwrap_or_default();
            return Err(WdaError::Remote {
                kind: err.to_owned(),
                message: msg.lines().next().unwrap_or_default().to_owned(),
            });
        }
        // Older runners may return a JSON 404 without a W3C value.error.
        if status >= 400 {
            return Err(WdaError::Remote { kind: "http".into(), message: status.to_string() });
        }
        Ok(v)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        Ok(self.call("GET", path, None, DEFAULT_TIMEOUT).await?["value"].take())
    }

    async fn session_id(&mut self) -> Result<String> {
        if let Some(s) = &self.session {
            return Ok(s.clone());
        }
        let r =
            self.call("POST", "/session", Some(json!({"capabilities": {"alwaysMatch": {}}})), DEFAULT_TIMEOUT).await?;
        let id = r
            .get("sessionId")
            .and_then(|s| s.as_str())
            .or_else(|| r["value"].get("sessionId").and_then(|s| s.as_str()))
            .ok_or_else(|| {
                WdaError::Unavailable(
                    ictl_l10n::tr!("创建会话没有返回 sessionId", "creating a session returned no sessionId").into(),
                )
            })?
            .to_owned();
        self.session = Some(id.clone());
        self.idle_current = None;
        self.snapshot_applied = Some(0);
        if self.fast_stream {
            // fixOrientation explicitly off: a WDA left running in the background keeps what an
            // older version set
            let settings = json!({
                "mjpegServerFramerate": MJPEG_FPS,
                "mjpegScalingFactor": MJPEG_SCALE,
                "mjpegFixOrientation": false,
            });
            let path = format!("/session/{id}/appium/settings");
            if let Err(e) = self.call("POST", &path, Some(json!({"settings": settings})), DEFAULT_TIMEOUT).await {
                tracing::debug!("MJPEG settings: {e}");
            }
        }
        Ok(id)
    }

    /// 带会话的请求；会话失效时重建一次再试。
    ///
    /// 改变手机状态的请求（[`acts`]）没有应答时，马上结束这个会话（`DELETE /session/:id`，WDA 16.15.3 不排队处理它）：
    /// WDA 在主线程上一个一个处理请求，我们不等了它还会接着做。排在慢请求后面的点击、滑动会在报了失败之后很久才落到屏幕上
    /// （Poki 在真机上见过）。结束会话后，这个会话里还在排队的请求 WDA 直接回「会话不存在」，不再执行；正在执行的那个照样做完。
    /// 下一个请求开新会话
    pub async fn in_session(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        for attempt in 0..2 {
            let input = acts(method, path);
            let sid = self.session_id().await.map_err(|e| if input { e.before_input() } else { e })?;
            let abandon = input.then_some(sid.as_str());
            let r =
                self.call_or_abandon(method, &format!("/session/{sid}{path}"), body.clone(), timeout, abandon).await;
            match r {
                Err(WdaError::Remote { kind, .. }) if attempt == 0 && kind == "invalid session id" => {
                    self.session = None
                }
                Err(e) if abandon.is_some() && e.no_answer() => {
                    self.forget_session();
                    return Err(e);
                }
                other => return other.map(|mut v| v["value"].take()),
            }
        }
        unreachable!()
    }

    /// 结束会话，不管结果：结束不了的会话 WDA 下次开会话时也会结束它
    async fn delete_session(&self, sid: &str) {
        // send 而不是 call：WDA 卡死时也要发，也不算进卡死计数
        if let Err(e) = self.send("DELETE", &format!("/session/{sid}"), None, SESSION_DELETE_TIMEOUT).await {
            tracing::debug!("DELETE /session/{sid}: {e}");
        }
    }

    /// 会话已经结束（或要当作结束）：下一个请求开新会话，WDA 那时把设置重置成默认
    fn forget_session(&mut self) {
        self.session = None;
        self.idle_current = None;
    }

    async fn post(&mut self, path: &str, body: Value) -> Result<Value> {
        self.in_session("POST", path, Some(body), DEFAULT_TIMEOUT).await
    }

    pub async fn ready(&self) -> Result<bool> {
        Ok(self.call("GET", "/status", None, Duration::from_secs(5)).await?["value"]["ready"] == true)
    }

    /// WDA 还能不能经 XCTest 办事：问一次前台 App，限时 [`XCTEST_PROBE_TIMEOUT`]。
    /// `/status` 不经 XCTest，WDA 半死（iPad 真机：`/status` 照常 200，前台 App、截图、读元素、`/wda/locked` 全部超时）时
    /// 它照样报 ready；这一问才看得出来
    pub async fn xctest_responsive(&self) -> Result<()> {
        self.call("GET", "/wda/activeAppInfo", None, XCTEST_PROBE_TIMEOUT).await.map(drop)
    }

    /// Read the WDA screen size again, including changes to Display Zoom.
    pub async fn measure_screen(&mut self) -> Result<ScreenSize> {
        let v = self.get("/wda/screen").await?;
        let s = &v["screenSize"];
        let positive = |key: &str| s[key].as_f64().filter(|n| n.is_finite() && *n > 0.0);
        let size = positive("width")
            .zip(positive("height"))
            .map(|(width, height)| ScreenSize { width, height })
            .ok_or_else(|| {
                WdaError::Unavailable(ictl_l10n::tr!("/wda/screen 尺寸无效", "Invalid /wda/screen dimensions").into())
            })?;
        self.screen = Some(size);
        self.screen_at = Some(Instant::now());
        Ok(size)
    }

    pub async fn base_screen_size(&mut self) -> Result<ScreenSize> {
        match self.screen {
            Some(s) => Ok(s),
            None => self.measure_screen().await,
        }
    }

    /// 当前方向下的屏幕点尺寸（转屏后宽高跟着变）
    pub async fn screen_size(&mut self) -> Result<ScreenSize> {
        Ok(self.view().await?.screen)
    }

    /// 当前方向下的屏幕尺寸和方向。尺寸只问一次 `/wda/screen`（约 0.3 秒），方向每次问（约 20 毫秒）：
    /// 尺寸缓存的是问的那一刻的方向，转屏后不换算的话截图会被压扁、屏幕下半截的元素被当成屏幕外。
    /// 问不到方向（旧版 WDA 等）就按缓存的尺寸、方向未知（不换算元素坐标）
    pub async fn view(&mut self) -> Result<elements::View> {
        let base = self.base_screen_size().await?;
        Ok(match self.orientation().await {
            Ok(o) => elements::View { screen: o.orient(base), orientation: Some(o) },
            Err(e) => {
                tracing::debug!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "读屏幕方向失败，按缓存的屏幕尺寸算：{e}",
                        "Reading the screen orientation failed, using the cached screen size: {e}"
                    )
                );
                elements::View { screen: base, orientation: None }
            }
        })
    }

    /// 界面相对设备竖放转了多少（`GET /rotation` 的 z）。能分出两个横屏方向，`/orientation` 分不出
    pub async fn rotation(&mut self) -> Result<ictl_device::Rotation> {
        let v = self.in_session("GET", "/rotation", None, Duration::from_secs(10)).await?;
        v["z"].as_f64().and_then(ictl_device::Rotation::from_degrees).ok_or_else(|| {
            WdaError::Unavailable(ictl_l10n::trf!(
                "看不懂 WDA 给的旋转角：{v}",
                "Can't make sense of the rotation WDA reported: {v}"
            ))
        })
    }

    /// Older WDA versions lack /rotation. Remember an unsupported route for this runner;
    /// the regular /orientation endpoint folds opposite directions into the same string.
    async fn interface_rotation(&mut self, fallback: impl Future<Output = Option<Rotation>>) -> Option<Rotation> {
        if !self.rotation_unsupported {
            match self.rotation().await {
                Ok(r) => return Some(r),
                Err(WdaError::Remote { kind, message })
                    if matches!(kind.as_str(), "unknown command" | "unknown method" | "unsupported operation")
                        || (kind == "http" && matches!(message.as_str(), "404" | "405")) =>
                {
                    self.rotation_unsupported = true;
                }
                Err(_) => return None,
            }
        }
        fallback.await
    }

    /// Poll rotation to detect a changed view. The screen size (a slower request) is measured on acquisition and on
    /// rotation, and again once the last measurement is older than [`SCREEN_RECHECK`] (Display Zoom) when
    /// `refresh_stale`: a poll while nobody acts, never a gesture's start, which costs only the rotation request.
    pub async fn display_sample(
        &mut self,
        previous: Option<u16>,
        refresh_stale: bool,
        fallback: impl Future<Output = Option<Rotation>>,
    ) -> (Option<ScreenSize>, Option<Rotation>) {
        let rotation = self.interface_rotation(fallback).await;
        let stale = self.screen_at.is_none_or(|t| t.elapsed() >= SCREEN_RECHECK);
        let rotated = rotation.is_some_and(|r| Some(r.degrees()) != previous);
        let size = if rotated || (refresh_stale && stale) {
            self.measure_screen().await
        } else {
            self.base_screen_size().await
        };
        (size.ok().map(ScreenSize::portrait), rotation)
    }

    /// 前台界面的方向
    pub async fn orientation(&mut self) -> Result<elements::Orientation> {
        let v = self.in_session("GET", "/orientation", None, Duration::from_secs(10)).await?;
        v.as_str().and_then(elements::Orientation::from_wda).ok_or_else(|| {
            WdaError::Unavailable(ictl_l10n::trf!(
                "看不懂 WDA 给的方向：{v}",
                "Can't make sense of the orientation WDA reported: {v}"
            ))
        })
    }

    /// 是否锁屏。WDA 的 `/wda/locked` 只靠 `com.apple.springboard.lockstate` 的变化通知维护
    /// （WebDriverAgent `XCUIDevice+FBHelpers.m`），有两处误报，用元素树纠正（[`elements::cover_sheet`]）：
    /// - 初值为 false：WDA 在手机锁着时启动就一直报未锁屏，直到下一次锁屏/解锁。确认过一次「确实没锁屏」之后
    ///   WDA 就跟上了，此后它说没锁就直接信，不多发请求
    /// - 下拉通知中心时报 true（和锁屏同一个界面）：WDA 说锁了时核对一次，是通知中心就算没锁
    ///
    /// 核对失败时按 WDA 的结果；需要「拿不准就算锁着」时用 [`Self::check_lock`] 的 strict 模式
    pub async fn locked(&mut self) -> Result<bool> {
        Ok(self.check_lock(None, false).await?.locked)
    }

    /// 锁屏判断，并带回核对时取到的元素树（解锁流程接着用，省一次 `/source`）。
    /// - `reported`：调用方刚问过的 `/wda/locked`，省一次请求
    /// - `strict`：拿不准就算锁着。确认整串密码是否解开时用：把仍锁着误判成已解锁，下一次会把同一个错误密码再输一遍
    pub async fn check_lock(&mut self, reported: Option<bool>, strict: bool) -> Result<LockCheck> {
        let wda_locked = match reported {
            Some(v) => v,
            None => self.wda_reports_locked().await?,
        };
        if self.lock_state_trusted && (!wda_locked || strict) {
            // 已同步过：WDA 说没锁就是没锁；strict 场景在密码键盘上，WDA 说锁了也不会是通知中心
            return Ok(LockCheck { locked: wda_locked, uncertain: false, tree: None });
        }
        let (sheet, tree) = match self.cover_tree().await {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "核对锁屏界面失败，按{}处理：{e}",
                        "Checking the lock screen failed, going by {}: {e}",
                        if strict {
                            ictl_l10n::tr!("锁着", "locked")
                        } else {
                            ictl_l10n::tr!(" WDA 的结果", "what WDA reported")
                        }
                    )
                );
                return Ok(LockCheck { locked: strict || wda_locked, uncertain: false, tree: None });
            }
        };
        let (locked, uncertain) = match (wda_locked, sheet) {
            (true, elements::CoverSheet::NotificationCenter) => (false, false),
            (true, _) => (true, false),
            (false, elements::CoverSheet::Locked) => {
                tracing::info!(
                    "{}",
                    ictl_l10n::log_tr!(
                        "WDA 报未锁屏，但屏幕上是锁屏界面（WDA 在锁屏时启动，状态未同步）",
                        "WDA says unlocked, but the screen shows the lock screen (WDA started while locked and its state is stale)"
                    )
                );
                (true, false)
            }
            // 锁屏样式、却没有锁的标记：可能是通知中心，也可能是 Touch ID 机型熄屏、有通知时的锁屏
            // （iPad mini 5 / iPadOS 26.6.1 真机：没有「按下主屏幕按钮以解锁」那行）。WDA 在锁屏时启动的话一直报 false，
            // 不能在这里认定它可信，否则之后再也不核对、永远不会自动解锁
            // 严格核对时拿不准就算锁着（同 `strict` 的说明）
            (false, elements::CoverSheet::NotificationCenter) => (strict, !strict),
            // 普通界面上确认过一次：WDA 的状态是同步的
            (false, elements::CoverSheet::None) => {
                self.lock_state_trusted = true;
                (false, false)
            }
        };
        Ok(LockCheck { locked, uncertain, tree })
    }

    /// WDA 自己报的锁屏状态，不核对。可能误报，见 [`Self::locked`]；配合 [`Self::lock_state_trusted`] 做快速判断
    pub async fn wda_reports_locked(&self) -> Result<bool> {
        Ok(self.call("GET", "/wda/locked", None, Duration::from_secs(10)).await?["value"] == true)
    }

    /// 本次 WDA 启动后是否确认过一次没锁屏。确认过且 WDA 说没锁，就确定没锁
    pub fn lock_state_trusted(&self) -> bool {
        self.lock_state_trusted
    }

    /// 前台是 SpringBoard 时看元素树是锁屏、通知中心还是普通界面，并带回元素树；前台是别的 App 就是普通界面
    async fn cover_tree(&mut self) -> Result<(elements::CoverSheet, Option<String>)> {
        if self.active_app().await? != SPRINGBOARD {
            return Ok((elements::CoverSheet::None, None));
        }
        let xml = self.source_xml().await?;
        Ok((elements::cover_sheet(&xml), Some(xml)))
    }

    pub async fn active_app(&self) -> Result<String> {
        Ok(self.get("/wda/activeAppInfo").await?["bundleId"].as_str().unwrap_or_default().to_owned())
    }

    pub async fn screenshot_png(&self) -> Result<Vec<u8>> {
        // base64 约 11MB：从 JSON 里直接拿走字符串，原地去掉换行，不再另外复制
        let Value::String(mut b64) = self.get("/screenshot").await? else {
            return Err(WdaError::Unavailable(
                ictl_l10n::tr!("截图响应不是字符串", "screenshot response is not a string").into(),
            ));
        };
        b64.retain(|c| !c.is_whitespace());
        base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| {
            WdaError::Unavailable(ictl_l10n::trf!("截图不是有效的 base64：{e}", "screenshot is not valid base64: {e}"))
        })
    }

    /// 元素树 XML，排除可见性等慢属性（完整树 7～8 秒，排除后约 0.3 秒）。读得慢就收紧快照上限（[`SnapshotLimits`]）
    pub async fn source_xml(&mut self) -> Result<String> {
        let level = self.snapshot_level;
        if self.snapshot_applied.unwrap_or(0) != level {
            let l = SNAPSHOT_LEVELS[level];
            let body = json!({"settings": {
                "snapshotMaxDepth": l.depth,
                "snapshotMaxChildren": l.children.unwrap_or(i32::MAX as u32),
            }});
            // 设不上照样读，下次再设
            match self.post("/appium/settings", body).await {
                Ok(_) => self.snapshot_applied = Some(level),
                Err(e) => tracing::debug!("snapshot limits: {e}"),
            }
        }
        let applied = self.snapshot_applied.unwrap_or(0);
        let path = format!("/source?format=xml&excluded_attributes={}", elements::SOURCE_EXCLUDED);
        let t0 = Instant::now();
        let r = self.call("GET", &path, None, SOURCE_TIMEOUT).await;
        let took = match r {
            Ok(_) => Some(t0.elapsed()),
            Err(WdaError::Timeout(_)) => None,
            Err(e) => return Err(e),
        };
        let next = next_snapshot_level(applied, took);
        if next != applied {
            tracing::info!(
                "{}",
                ictl_l10n::log_trf!(
                    "读元素树{}，快照上限改为 {:?}",
                    "Reading the element tree {}; snapshot limits now {:?}",
                    took.map_or_else(
                        || ictl_l10n::log_tr!("超时", "timed out").to_owned(),
                        |t| ictl_l10n::log_trf!("用了 {:.1} 秒", "took {:.1} s", t.as_secs_f64())
                    ),
                    SNAPSHOT_LEVELS[next]
                )
            );
        }
        self.snapshot_level = next;
        let v = r?;
        self.snapshot_read = applied;
        Ok(v["value"].as_str().unwrap_or_default().to_owned())
    }

    /// 上一次读到的元素树受了快照上限（[`SnapshotLimits`]）的限制时，给人和 agent 看的说明
    pub fn snapshot_note(&self) -> Option<String> {
        let l = SNAPSHOT_LEVELS[self.snapshot_read];
        let children = l.children?;
        Some(ictl_l10n::trf!(
            "(界面元素太多、读得慢,这次只读了 {} 层、每个元素最多 {children} 个子元素:长列表靠后的行、很深的元素可能没列出;读得快了会自动放宽)",
            "(This screen has so many elements that reading them was slow, so this read went {} levels deep with at most {children} children per element: rows far down a long list or deeply nested elements may be missing; the limits loosen again once reads are fast)",
            l.depth
        ))
    }

    pub async fn elements(&mut self) -> Result<Vec<elements::Element>> {
        Ok(self.tree().await?.elements)
    }

    /// 元素树，带上「谁盖住谁」「有没有加载圈」等信息
    pub async fn tree(&mut self) -> Result<elements::Tree> {
        let view = self.view().await?;
        self.move_detection_point(view.screen).await;
        let mut tree = elements::parse_tree(&self.source_xml().await?, view);
        tree.incomplete |= SNAPSHOT_LEVELS[self.snapshot_read].children.is_some();
        Ok(tree)
    }

    /// 把 WDA 判断前台 App 的点（appium setting `activeAppDetectionPoint`）挪到 [`detection_point`]，每个客户端做一次。
    ///
    /// WDA 读元素树、问前台 App 时，看屏幕上这个点下面的元素属于哪个进程。默认点在离左上角短边 20% 的地方
    /// （WDA 16.15.3 `FBActiveAppDetectionPoint`，430 点宽的 iPhone 上是 (86, 86)），正好在通知横幅底下：横幅弹着时
    /// 读到的是 SpringBoard 的树，按名字点 App 里的按钮就找不到（iPhone16,2 / iOS 27.2.1 的备忘录，「Device Hub」横幅）。
    /// 设不上不影响读树，只是还用默认的点；连不上 WDA 时下次再试
    async fn move_detection_point(&mut self, screen: ScreenSize) {
        if self.detection_point_set {
            return;
        }
        let (x, y) = detection_point(screen);
        let body = json!({"settings": {"activeAppDetectionPoint": format!("{x},{y}")}});
        match self.post("/appium/settings", body).await {
            Ok(_) => self.detection_point_set = true,
            Err(e) => {
                tracing::debug!("activeAppDetectionPoint: {e}");
                self.detection_point_set = !matches!(e, WdaError::Unavailable(_));
            }
        }
    }

    /// 点击：一个 W3C actions 单指手势（按下 50ms），期间把等待设置临时调小。真机计时（iOS 27）：
    /// - `/wda/tap` 先等界面空闲：主屏点图标约 1.7 秒（调小等待也没用），App 里约 0.6 秒，锁屏上约 2.9 秒
    /// - actions 手势做完要等动画冷却（`animationCoolOffTimeout`，默认 2 秒）：App 里切标签约 1.9 秒
    /// - actions + 临时调小等待：App 里约 0.7 秒，主屏点图标约 0.9 秒
    /// - 冷却不能是 0.2 秒（[`IdleSettings::QUICK`]）：界面上一直有动画时（键盘弹着、光标在闪）要等约 26 秒才返回，
    ///   iPhone 18 Pro Max（iOS 27.2.1）在设置里点搜索结果每次都这样；冷却 0 约 0.4 秒。和滑动一样用
    ///   [`IdleSettings::NO_COOL_OFF`]
    ///
    /// 和 `/wda/tap` 一样不等点击引起的动画结束，调用方要看结果就再截图
    pub async fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        self.quick_gesture(&tap_steps(x, y, TAP_PRESS_MS), IdleSettings::NO_COOL_OFF).await
    }

    /// 依次点几个点，一个手势（[`tap_sequence_body`]）：同一个键盘、键盘上连按时用。分开点每下都要等界面空闲，
    /// 数字键盘上点 7 个键约 3.6 秒，一个手势约 1.9 秒。点的过程中界面变了（跳了页面）后面的点也照样点下去，
    /// 所以只给点的时候布局不变的一串点用
    pub async fn tap_sequence(&mut self, points: &[(f64, f64)]) -> Result<()> {
        let body = tap_sequence_body(points, TAP_PRESS_MS, SEQUENCE_GAP_MS);
        self.quick_actions(body, IdleSettings::NO_COOL_OFF).await
    }

    /// 同一点连点 `count` 下（双击、「短时间内连点 N 下」的隐藏入口），一个 actions 手势，每下之间停
    /// [`MULTI_TAP_GAP_MS`]。分开点每下都要等界面空闲（约 0.6 秒），`iphone_batch` 连点 5 下约 2.9 秒，赶不上这类入口的时间窗
    pub async fn multi_tap(&mut self, x: f64, y: f64, count: u32) -> Result<()> {
        let steps = multi_tap_steps(x, y, count, TAP_PRESS_MS, MULTI_TAP_GAP_MS);
        self.quick_gesture(&steps, IdleSettings::NO_COOL_OFF).await
    }

    /// 长按：按下、停 `seconds` 秒、抬起，一个 actions 手势。`/wda/touchAndHold` 前后都等界面空闲，
    /// 真机上主屏长按图标 1.2 秒要约 12 秒才返回。等待设置同 [`Self::tap`]
    pub async fn touch_and_hold(&mut self, x: f64, y: f64, seconds: f64) -> Result<()> {
        self.quick_gesture(&tap_steps(x, y, (seconds * 1000.0) as u64), IdleSettings::NO_COOL_OFF).await
    }

    /// 滑动：一个 actions 手势，期间用 [`IdleSettings::NO_COOL_OFF`]。做完时列表可能还在惯性滚动，
    /// 要看结果的调用方自己等界面稳定
    pub async fn swipe(&mut self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        self.quick_gesture(&swipe_steps(from, to, ms), IdleSettings::NO_COOL_OFF).await
    }

    /// The MJPEG stream (the live viewer) at up to [`MJPEG_FPS`] frames a second instead of WDA's 10, at full size
    /// ([`MJPEG_SCALE`], why not smaller) and never turned upright, set with each new session: the picture follows a
    /// drag closely. In landscape the frames stay portrait with the content sideways (on an iPhone; an iPad's are
    /// already landscape) and the viewer page turns them by the interface rotation, as it does the phone's own video:
    /// `mjpegFixOrientation` makes WDA decode, rotate and re-encode every frame, 92 % of a core on an iPhone 15 Pro Max
    /// in landscape, and iOS killed it within two minutes. WDA only captures while a client is connected
    pub fn fast_stream(&mut self) {
        self.fast_stream = true;
    }

    /// Swipes and stays in polling mode for the caller, who ends it with [`Self::end_polling`]: restoring the settings
    /// only to change them again right after would cost two round trips
    /// `hold_ms` > 0 holds the finger still at `from` first ([`drag_steps`])
    pub async fn swipe_into_polling(&mut self, from: (f64, f64), to: (f64, f64), hold_ms: u64, ms: u64) -> Result<()> {
        self.baseline().await.map_err(WdaError::before_input)?;
        self.set_idle_settings(IdleSettings::NO_COOL_OFF).await.map_err(WdaError::before_input)?;
        let r = self.actions(&drag_steps(from, to, hold_ms, ms)).await;
        if r.is_err() {
            self.end_polling().await;
        }
        r
    }

    /// 开始轮询界面：读元素树时不等界面空闲、动画冷却（约 0.2 秒一次），由调用方比较前后两次判断稳定。
    /// 用完调 [`Self::end_polling`] 恢复
    pub async fn begin_polling(&mut self) -> Result<()> {
        self.baseline().await?;
        self.set_idle_settings(IdleSettings::NO_COOL_OFF).await
    }

    pub async fn end_polling(&mut self) {
        let b = self.idle_baseline.unwrap_or(IdleSettings::DEFAULT);
        if let Err(e) = self.set_idle_settings(b).await {
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!(
                    "轮询后恢复 WDA 等待设置失败：{e}",
                    "Restoring the WDA wait settings after polling failed: {e}"
                )
            );
        }
    }

    /// 平时的等待设置（第一次用时读一次）。临时调小、改回之前都按它恢复
    pub async fn baseline(&mut self) -> Result<IdleSettings> {
        if let Some(b) = self.idle_baseline {
            return Ok(b);
        }
        // 上次没恢复成功的话读到的就是调小的值：按 WDA 默认值算
        let b = match self.idle_settings().await? {
            s if s == IdleSettings::QUICK || s == IdleSettings::NO_COOL_OFF => IdleSettings::DEFAULT,
            s => s,
        };
        self.idle_baseline = Some(b);
        Ok(b)
    }

    /// 发一个手势，期间把等待设置临时换成 `during`（见 [`Self::tap`]），做完恢复
    async fn quick_gesture(&mut self, steps: &[PointerStep], during: IdleSettings) -> Result<()> {
        self.quick_actions(actions_body(steps), during).await
    }

    async fn quick_actions(&mut self, body: Value, during: IdleSettings) -> Result<()> {
        let baseline = self.baseline().await.map_err(WdaError::before_input)?;
        self.set_idle_settings(during).await.map_err(WdaError::before_input)?;
        let r = self.post("/actions", body).await.map(drop);
        if let Err(e) = self.set_idle_settings(baseline).await {
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!(
                    "手势后恢复 WDA 等待设置失败：{e}",
                    "Restoring the WDA wait settings after the gesture failed: {e}"
                )
            );
        }
        r
    }

    /// 一次 W3C actions 请求。注意：多次点击合并成一个请求会漏键，解锁时每位单独发
    pub async fn actions(&mut self, steps: &[PointerStep]) -> Result<()> {
        self.post("/actions", actions_body(steps)).await.map(drop)
    }

    /// 屏幕上的系统弹窗（定位权限、通知权限等）的文字；没有弹窗返回 None。
    /// 弹窗挡着时 Home 不生效、App 也到不了前台，WDA 却照样报成功
    pub async fn alert_text(&mut self) -> Result<Option<String>> {
        match self.in_session("GET", "/alert/text", None, DEFAULT_TIMEOUT).await {
            Ok(v) => Ok(v.as_str().map(str::to_owned).filter(|t| !t.is_empty())),
            Err(WdaError::Remote { kind, .. }) if kind.contains("alert") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 屏幕上有没有软键盘（有输入框获得了焦点）。`/wda/keys` 在没有焦点时也返回成功，什么都没输进去
    pub async fn keyboard_shown(&mut self) -> Result<bool> {
        let v = self.post("/elements", json!({"using": "class name", "value": "XCUIElementTypeKeyboard"})).await?;
        Ok(v.as_array().is_some_and(|a| !a.is_empty()))
    }

    /// The element with keyboard focus (`GET /element/active`); None when no element has it
    pub async fn focused_field(&mut self) -> Result<Option<FocusedField>> {
        let v = match self.in_session("GET", "/element/active", None, DEFAULT_TIMEOUT).await {
            Ok(v) => v,
            Err(WdaError::Remote { kind, .. }) if kind.contains("no such element") => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(id) = element_id(&v) else { return Ok(None) };
        let kind = self.attribute(&id, "type").await?;
        let kind = kind.strip_prefix("XCUIElementType").unwrap_or(&kind).to_owned();
        let placeholder = self.attribute(&id, "placeholderValue").await?;
        Ok(Some(FocusedField { id, kind, placeholder }))
    }

    /// An attribute of an element as text; a missing one (`null`) reads as empty
    pub async fn attribute(&mut self, id: &str, name: &str) -> Result<String> {
        let v = self.in_session("GET", &format!("/element/{id}/attribute/{name}"), None, DEFAULT_TIMEOUT).await?;
        Ok(match v {
            Value::String(s) => s,
            Value::Null => String::new(),
            other => other.to_string(),
        })
    }

    /// What the field holds: an empty field reads its placeholder as its value (Spotlight's reads «搜索»), so that
    /// counts as empty, as with native control
    pub async fn field_text(&mut self, f: &FocusedField) -> Result<String> {
        let v = self.attribute(&f.id, "value").await?;
        Ok(if !f.placeholder.is_empty() && v == f.placeholder { String::new() } else { v })
    }

    /// Empties a field (`POST /element/:id/clear`): WDA deletes the text with the keyboard until the field reads
    /// empty, and answers an error when it doesn't (some custom fields)
    pub async fn clear(&mut self, id: &str) -> Result<()> {
        self.in_session("POST", &format!("/element/{id}/clear"), Some(json!({})), Duration::from_secs(60))
            .await
            .map(drop)
    }

    pub async fn keys(&mut self, text: &str) -> Result<()> {
        let chars: Vec<String> = text.chars().map(String::from).collect();
        self.in_session("POST", "/wda/keys", Some(json!({"value": chars})), Duration::from_secs(120)).await.map(drop)
    }

    /// home / volumeUp / volumeDown，走 XCTest 的 `XCUIDevice pressButton`。
    /// iPhone 12 mini（iOS 26.4.1）上 home 不起作用但照样报成功，见 [`Self::press_home_hid`]
    pub async fn press_button(&mut self, name: &str) -> Result<()> {
        self.post("/wda/pressButton", json!({"name": name})).await.map(drop)
    }

    /// 直接发一个 IOHID 按键事件（`/wda/performIoHidEvent`），按住 `seconds` 秒
    pub async fn hid_event(&mut self, page: u32, usage: u32, seconds: f64) -> Result<()> {
        self.post("/wda/performIoHidEvent", json!({"page": page, "usage": usage, "duration": seconds})).await.map(drop)
    }

    /// 经 IOHID 按 Home（消费者页 AC Home）。iPhone 12 mini（iOS 26.4.1）实测：XCTest 的 home 无效时
    /// 它能从 App 回到主屏、关掉 Spotlight、点亮熄灭的屏幕；同页的 Menu（0x40）没反应
    pub async fn press_home_hid(&mut self) -> Result<()> {
        self.hid_event(HID_PAGE_CONSUMER, HID_AC_HOME, 0.05).await
    }

    /// 经 IOHID 短按电源键（[`POWER_PRESS_SECS`]，和手指按一下差不多）。WDA 自己的 `/wda/lock` 在 iOS 27 上按住 0.5 秒，
    /// iOS 27 以下用 XCTest 的 `pressLockButton`，iPhone 12 mini（iOS 26.4.1）、iPad mini 5（iPadOS 26.7.3）上锁不了屏。
    /// 屏幕已熄灭时按它会把屏幕点亮，调用方要先确认没锁屏
    pub async fn press_power_hid(&mut self) -> Result<()> {
        self.hid_event(HID_PAGE_CONSUMER, HID_POWER, POWER_PRESS_SECS).await
    }

    pub async fn lock(&mut self) -> Result<()> {
        self.post("/wda/lock", json!({})).await.map(drop)
    }

    /// 启动并等到前台、界面稳定（冷启动后立即点击会被吞），最多等 8 秒。
    /// 等待期间查前台出错不算失败（启动命令已经成功了）
    pub async fn launch_app(&mut self, bundle_id: &str) -> Result<Launch> {
        let started = tokio::time::Instant::now();
        // 不经 XCTest 启动（LSApplicationWorkspace），发出就返回，到没到前台由下面自己查。`/wda/apps/launch` 要等 XCTest
        // 看到 App 在前台：系统 App 第一次打开时隐私说明页盖在上面，它就一直等到 60 秒超时，WDA 也被判卡死重启
        // （iPhone 12 mini，iOS 26.4.1，「备忘录」「提醒事项」都是 65 秒）。这时它回「failed to launch」，说明页已经出来了
        let body = json!({"bundleId": bundle_id});
        match self.call("POST", "/wda/apps/launchUnattached", Some(body.clone()), DEFAULT_TIMEOUT).await {
            Ok(_) => {}
            Err(WdaError::Remote { message, .. }) if message.contains("failed to launch") => {}
            // older WDA without the route: launch as before
            Err(WdaError::Remote { kind, message }) if unknown_route(&kind, &message) => {
                self.in_session("POST", "/wda/apps/launch", Some(body), Duration::from_secs(60)).await?;
            }
            Err(e) => return Err(e),
        }
        let t0 = tokio::time::Instant::now();
        let deadline = t0 + Duration::from_secs(8);
        let mut foreground = false;
        while tokio::time::Instant::now() < deadline {
            match self.active_app().await {
                Ok(a) if a == bundle_id => {
                    foreground = true;
                    break;
                }
                Ok(a) if a == PRIVACY_SHEET => {
                    return Ok(Launch::Blocked(
                        ictl_l10n::tr!(
                            "第一次打开时的隐私说明页(点「继续」或「接受并继续」后才进入 App)",
                            "the privacy notice shown on first launch (tap Continue or Accept & Continue to get into the app)"
                        )
                        .into(),
                    ));
                }
                // 系统弹窗（权限请求）挡着时前台一直是 SpringBoard，原来要白等满 8 秒。
                // 刚启动的一瞬间前台也还是 SpringBoard，过 1 秒还是才去查弹窗（查一次约 0.4 秒）
                Ok(a) if a == SPRINGBOARD && bundle_id != SPRINGBOARD && t0.elapsed() >= Duration::from_secs(1) => {
                    if let Ok(Some(text)) = self.alert_text().await {
                        return Ok(Launch::Blocked(text));
                    }
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !foreground {
            return Ok(match self.alert_text().await {
                Ok(Some(text)) => Launch::Blocked(text),
                _ => Launch::Slow,
            });
        }
        self.settle_launched(started).await;
        Ok(Launch::Foreground)
    }

    /// App 到了前台之后再等它画出来。`started`：发出启动请求的时刻
    async fn settle_launched(&mut self, started: tokio::time::Instant) {
        // 冷启动后立即点击会被吞（回归用例 #13）。原来固定再等 1 秒；改为等界面稳定：连续两次元素树一样
        // （读元素树时 WDA 本身也等界面空闲），最多仍是 1 秒
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut prev = None;
        while tokio::time::Instant::now() < deadline {
            let cur = self.source_xml().await.ok();
            if cur.is_some() && cur == prev {
                break;
            }
            prev = cur;
        }
        // 切回已在后台的 App 时元素树马上就稳定了（约 0.4 秒），放大进场的动画还没完，这时点击会被吞（真机：小红书、高德各一次）
        if let Some(left) = LAUNCH_MIN.checked_sub(started.elapsed()) {
            tokio::time::sleep(left).await;
        }
    }

    pub async fn terminate_app(&mut self, bundle_id: &str) -> Result<()> {
        self.post("/wda/apps/terminate", json!({"bundleId": bundle_id})).await.map(drop)
    }

    /// 打开网址或 URL Scheme，等打开它的 App 到前台、画出来再返回（最多约 8 秒），同 [`Self::launch_app`]。
    /// 不等的话 App 冷启动时还是黑屏，紧接着截图会把两帧黑屏当成「画面已静止」（iPad Safari 真机）。
    /// 返回结果和前台 App；在原来的前台 App 里打开（Safari 里开网址）时等 1 秒没换就算打开了
    pub async fn open_url(&mut self, url: &str) -> Result<(Launch, String)> {
        let started = tokio::time::Instant::now();
        let before = self.active_app().await.unwrap_or_default();
        self.in_session("POST", "/url", Some(json!({"url": url})), Duration::from_secs(60)).await?;
        let deadline = started + Duration::from_secs(8);
        // 前台要连续两次是同一个才算：Safari 冷启动的一瞬间前台是 `com.apple.DocumentManager.DockFolderViewService`（iPad 真机）
        let mut last = String::new();
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let waited = started.elapsed() >= Duration::from_secs(1);
            let now = self.active_app().await;
            let steady = now.as_ref().is_ok_and(|a| *a == last);
            if let Ok(a) = &now {
                last.clone_from(a);
            }
            match now {
                Ok(a) if a != before && a != SPRINGBOARD && steady => {
                    self.settle_launched(started).await;
                    return Ok((Launch::Foreground, a));
                }
                Ok(a) if a == before && a != SPRINGBOARD && waited => {
                    self.settle_launched(started).await;
                    return Ok((Launch::Foreground, a));
                }
                // 「要在某 App 中打开吗」之类的系统弹窗挡着时前台一直是 SpringBoard
                Ok(a) if a == SPRINGBOARD && waited => {
                    if let Ok(Some(text)) = self.alert_text().await {
                        return Ok((Launch::Blocked(text), a));
                    }
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                let now = self.active_app().await.unwrap_or_default();
                return Ok((Launch::Slow, now));
            }
        }
    }

    pub async fn idle_settings(&mut self) -> Result<IdleSettings> {
        let v = self.in_session("GET", "/appium/settings", None, DEFAULT_TIMEOUT).await?;
        Ok(IdleSettings {
            wait_for_idle_timeout: v["waitForIdleTimeout"].as_f64().unwrap_or(10.0),
            animation_cool_off_timeout: v["animationCoolOffTimeout"].as_f64().unwrap_or(2.0),
        })
    }

    pub async fn set_idle_settings(&mut self, s: IdleSettings) -> Result<()> {
        if self.idle_current == Some(s) {
            return Ok(());
        }
        // not known if this fails
        self.idle_current = None;
        let body = json!({"settings": {"waitForIdleTimeout": s.wait_for_idle_timeout, "animationCoolOffTimeout": s.animation_cool_off_timeout}});
        self.post("/appium/settings", body).await?;
        self.idle_current = Some(s);
        Ok(())
    }

    /// 锁屏密码栏的 (总位数, 已输入位数)；找不到密码栏返回 None
    pub async fn passcode_field(&mut self) -> Result<Option<(u32, u32)>> {
        let els =
            self.post("/elements", json!({"using": "class name", "value": "XCUIElementTypeSecureTextField"})).await?;
        for e in els.as_array().into_iter().flatten() {
            let Some(eid) = e.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_str()) else { continue };
            let v = self.in_session("GET", &format!("/element/{eid}/attribute/value"), None, DEFAULT_TIMEOUT).await?;
            if let Some(p) = v.as_str().and_then(elements::parse_passcode_value) {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 按顺序返回预设响应，并记录收到的请求
    #[derive(Default)]
    struct Mock {
        replies: Mutex<VecDeque<(u16, String)>>,
        seen: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl Mock {
        fn with(replies: &[(u16, &str)]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().map(|(s, b)| (*s, b.to_string())).collect()),
                ..Default::default()
            }
        }
    }

    impl Transport for &Mock {
        async fn send(
            &self,
            method: &str,
            path: &str,
            body: Option<Vec<u8>>,
            _: Duration,
        ) -> std::io::Result<(u16, Vec<u8>)> {
            self.seen.lock().unwrap().push((method.into(), path.into(), body.map(|b| String::from_utf8(b).unwrap())));
            let (s, b) = self.replies.lock().unwrap().pop_front().ok_or_else(|| std::io::Error::other("连接被拒绝"))?;
            Ok((s, b.into_bytes()))
        }
    }

    #[tokio::test]
    async fn display_poll_remeasures_size_only_when_needed() {
        let session = (200, r#"{"sessionId":"A"}"#);
        let size = (200, r#"{"value":{"screenSize":{"width":393,"height":852}}}"#);
        let portrait = (200, r#"{"value":{"z":0}}"#);
        let landscape = (200, r#"{"value":{"z":90}}"#);
        let m =
            Mock::with(&[session, portrait, size, portrait, landscape, size, landscape, landscape, landscape, size]);
        let mut w = WdaClient::new(&m);
        // (previous rotation, a poll that may refresh the size, size older than SCREEN_RECHECK, expected rotation)
        for (previous, refresh, stale, expected) in [
            (None, true, false, Rotation::Portrait),
            (Some(0), true, false, Rotation::Portrait),
            // rotated: measured whatever the caller
            (Some(0), false, false, Rotation::TopToRight),
            // a touch-down with a recent size: rotation only
            (Some(90), false, false, Rotation::TopToRight),
            // a touch-down with an old size: still rotation only (0.95 s on an iPad mini 5)
            (Some(90), false, true, Rotation::TopToRight),
            // a quiet poll with an old size: measured again (Display Zoom)
            (Some(90), true, true, Rotation::TopToRight),
        ] {
            if stale {
                w.screen_at = Some(Instant::now() - SCREEN_RECHECK);
            }
            let (size, rotation) = w.display_sample(previous, refresh, async { panic!("unexpected fallback") }).await;
            assert_eq!(rotation, Some(expected));
            assert_eq!(size.unwrap().width, 393.);
        }
        let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
        assert_eq!(
            paths,
            [
                "/session",
                "/session/A/rotation",
                "/wda/screen",
                "/session/A/rotation",
                "/session/A/rotation",
                "/wda/screen",
                "/session/A/rotation",
                "/session/A/rotation",
                "/session/A/rotation",
                "/wda/screen"
            ]
        );
    }

    #[tokio::test]
    async fn legacy_http_404_is_distinct_from_a_transient_rotation_failure() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (500, r#"{"value":"runner busy"}"#),
            (200, r#"{"value":{"z":90}}"#),
            (404, r#"{"value":{"message":"Unhandled endpoint"}}"#),
        ]);
        let mut w = WdaClient::new(&m);
        assert_eq!(w.interface_rotation(async { panic!("500 is not a missing route") }).await, None);
        assert_eq!(w.interface_rotation(async { panic!("supported route") }).await, Some(Rotation::TopToRight));
        assert_eq!(w.interface_rotation(async { Some(Rotation::UpsideDown) }).await, Some(Rotation::UpsideDown));
    }

    #[tokio::test]
    async fn legacy_wda_uses_a_fresh_interface_rotation_without_retrying_the_missing_route() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (404, r#"{"value":{"error":"unknown command"}}"#),
            (200, r#"{"value":{"screenSize":{"width":393,"height":852}}}"#),
        ]);
        let mut w = WdaClient::new(&m);
        let (size, rotation) = w.display_sample(None, false, async { Some(Rotation::UpsideDown) }).await;
        assert_eq!(size.unwrap().width, 393.);
        assert_eq!(rotation, Some(Rotation::UpsideDown));
        assert_eq!(w.interface_rotation(async { Some(Rotation::TopToLeft) }).await, Some(Rotation::TopToLeft));
        assert_eq!(w.interface_rotation(async { None }).await, None);
        assert_eq!(m.seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn session_is_recreated_once_after_invalid_session() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A","value":{}}"#),
            (404, r#"{"value":{"error":"invalid session id","message":"gone\nmore"}}"#),
            (200, r#"{"value":{"sessionId":"B"}}"#),
            (200, r#"{"value":null}"#),
        ]);
        let mut c = WdaClient::new(&m);
        c.press_button("home").await.unwrap();
        let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
        assert_eq!(paths, ["/session", "/session/A/wda/pressButton", "/session", "/session/B/wda/pressButton"]);
    }

    #[tokio::test]
    async fn hid_buttons_send_consumer_page_events() {
        let ok = r#"{"value":null}"#;
        let m = Mock::with(&[(200, r#"{"sessionId":"A"}"#), (200, ok), (200, ok)]);
        let mut c = WdaClient::new(&m);
        c.press_home_hid().await.unwrap();
        c.press_power_hid().await.unwrap();
        let seen = m.seen.lock().unwrap().clone();
        assert_eq!(seen[1].1, "/session/A/wda/performIoHidEvent");
        let body = |i: usize| serde_json::from_str::<Value>(seen[i].2.as_deref().unwrap()).unwrap();
        assert_eq!((body(1)["page"].as_u64(), body(1)["usage"].as_u64()), (Some(0x0C), Some(0x223)));
        assert_eq!((body(2)["page"].as_u64(), body(2)["usage"].as_u64()), (Some(0x0C), Some(0x30)));
        // 0.1 秒在 iPhone 15 Pro Max、iPad mini 5 上锁不了屏，0.5 秒在 iPhone 18 Pro Max 上锁不了（见 POWER_PRESS_SECS）
        assert_eq!(body(2)["duration"], 0.15);
    }

    #[tokio::test]
    async fn tap_is_one_gesture_with_quick_waits_then_restores_baseline() {
        let ok = r#"{"value":null}"#;
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (200, r#"{"value":{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}}"#),
            (200, ok),
            (200, ok),
            (200, ok),
            // 第二次点击不再读设置
            (200, ok),
            (200, ok),
            (200, ok),
        ]);
        let mut c = WdaClient::new(&m);
        c.tap(10.0, 20.0).await.unwrap();
        c.tap(30.0, 40.0).await.unwrap();
        let seen = m.seen.lock().unwrap().clone();
        let paths: Vec<_> = seen.iter().map(|s| s.1.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/session",
                "/session/A/appium/settings",
                "/session/A/appium/settings",
                "/session/A/actions",
                "/session/A/appium/settings",
                "/session/A/appium/settings",
                "/session/A/actions",
                "/session/A/appium/settings",
            ]
        );
        let body = |i: usize| serde_json::from_str::<Value>(seen[i].2.as_deref().unwrap()).unwrap();
        assert_eq!(body(2)["settings"]["animationCoolOffTimeout"], 0.0);
        assert_eq!(body(4)["settings"]["animationCoolOffTimeout"], 2.0);
        let a = body(3);
        let steps = a["actions"][0]["actions"].as_array().unwrap();
        assert_eq!(steps.iter().filter(|s| s["type"] == "pointerDown").count(), 1);
    }

    /// 启动不经 XCTest（`launchUnattached`），发出就返回；系统 App 第一次打开时它回 failed to launch，前台是隐私说明页，
    /// 马上报被挡住，不再等 60 秒
    #[tokio::test]
    async fn a_first_launch_privacy_notice_is_reported_at_once() {
        let fail = r#"{"value":{"error":"unknown error","message":"LSApplicationWorkspace failed to launch app"}}"#;
        let m = Mock::with(&[(500, fail), (200, r#"{"value":{"bundleId":"com.apple.PDUIApp"}}"#)]);
        let mut w = WdaClient::new(&m);
        let t0 = std::time::Instant::now();
        let r = w.launch_app("com.apple.mobilenotes").await.unwrap();
        assert!(matches!(r, Launch::Blocked(ref t) if !t.is_empty()), "{r:?}");
        assert!(t0.elapsed() < Duration::from_secs(1));
        let seen = m.seen.lock().unwrap();
        assert_eq!(seen[0].1, "/wda/apps/launchUnattached");
        assert!(seen.iter().all(|(_, p, _)| !p.ends_with("/wda/apps/launch")));
    }

    /// 旧版 WDA 没有 launchUnattached：照旧经会话启动
    #[tokio::test]
    async fn an_older_wda_without_unattached_launch_launches_as_before() {
        let m = Mock::with(&[
            (404, r#"{"value":{"error":"unknown command","message":"Unhandled endpoint"}}"#),
            (200, r#"{"sessionId":"A"}"#),
            (200, r#"{"value":null}"#),
            (200, r#"{"value":{"bundleId":"com.apple.PDUIApp"}}"#),
        ]);
        let mut w = WdaClient::new(&m);
        assert!(matches!(w.launch_app("com.apple.mobilenotes").await.unwrap(), Launch::Blocked(_)));
        let seen = m.seen.lock().unwrap();
        assert_eq!(seen[2].1, "/session/A/wda/apps/launch");
    }

    /// 别的错误照样报出来，不当成启动了
    #[tokio::test]
    async fn other_launch_errors_are_errors() {
        let m = Mock::with(&[(500, r#"{"value":{"error":"invalid argument","message":"bad bundle"}}"#)]);
        let mut w = WdaClient::new(&m);
        assert!(matches!(w.launch_app("x").await, Err(WdaError::Remote { .. })));
    }

    /// 依次点几个点：每个点一根手指，第 k 根先空等前面的点按完；各手指的节拍数对得上，按下抬起一个接一个、不重叠
    #[test]
    fn a_sequence_is_one_finger_per_point_in_turn() {
        let body = tap_sequence_body(&[(10.0, 20.0), (30.0, 40.0), (50.0, 60.0)], 50, 40);
        let fingers = body["actions"].as_array().unwrap();
        assert_eq!(fingers.len(), 3);
        for (k, f) in fingers.iter().enumerate() {
            let a = f["actions"].as_array().unwrap();
            assert_eq!(a.len(), 5 * k + 4, "finger {k}");
            // pauses only until its own turn, then move, down, hold, up at its point
            assert!(a[..5 * k].iter().all(|s| s["type"] == "pause"));
            assert_eq!(a[5 * k]["type"], "pointerMove");
            assert_eq!(a[5 * k]["x"].as_f64(), Some(10.0 + 20.0 * k as f64));
            assert_eq!(a[5 * k + 1]["type"], "pointerDown");
            assert_eq!(a[5 * k + 3]["type"], "pointerUp");
            assert_ne!(fingers[0]["id"], fingers[k.max(1)]["id"]);
        }
        // finger k waits 5 ticks per earlier point: move, down, hold, up, gap
        assert_eq!(fingers[1]["actions"][2]["duration"], 50);
        assert_eq!(fingers[1]["actions"][4]["duration"], 40);
    }

    /// 连点是一个 actions 请求：一根手指只移动一次，在同一点按下抬起 N 次，两下之间停 MULTI_TAP_GAP_MS；
    /// 5 下在 0.8 秒内点完
    #[tokio::test]
    async fn multi_tap_is_one_request_at_one_point() {
        let ok = r#"{"value":null}"#;
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (200, r#"{"value":{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}}"#),
            (200, ok),
            (200, ok),
            (200, ok),
        ]);
        let mut c = WdaClient::new(&m);
        c.multi_tap(10.0, 20.0, 5).await.unwrap();
        let seen = m.seen.lock().unwrap().clone();
        let actions: Vec<_> = seen.iter().filter(|s| s.1.ends_with("/actions")).collect();
        assert_eq!(actions.len(), 1);
        let body = serde_json::from_str::<Value>(actions[0].2.as_deref().unwrap()).unwrap();
        let steps = body["actions"][0]["actions"].as_array().unwrap();
        let count = |t: &str| steps.iter().filter(|s| s["type"] == t).count();
        assert_eq!((count("pointerMove"), count("pointerDown"), count("pointerUp")), (1, 5, 5));
        assert_eq!((steps[0]["x"].as_f64(), steps[0]["y"].as_f64()), (Some(10.0), Some(20.0)));
        let ms: u64 = steps.iter().filter_map(|s| s["duration"].as_u64()).sum();
        assert_eq!(ms, 5 * TAP_PRESS_MS + 4 * MULTI_TAP_GAP_MS);
        assert!(ms < 800);
    }

    #[tokio::test]
    async fn remote_error_keeps_first_line_and_unreachable_maps_to_unavailable() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (500, r#"{"value":{"error":"unknown error","message":"boom\nstack"}}"#),
        ]);
        let mut c = WdaClient::new(&m);
        match c.press_button("home").await {
            Err(WdaError::Remote { kind, message }) => {
                assert_eq!((kind.as_str(), message.as_str()), ("unknown error", "boom"))
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(c.active_app().await, Err(WdaError::Unavailable(_))));
    }

    /// 传输层报的错误按「请求送没送到」分类：只有没送到的才是 Unavailable（调用方会退到 HID 重做）
    #[tokio::test]
    async fn errors_after_sending_are_not_unavailable() {
        struct Fails(std::io::ErrorKind);
        impl Transport for Fails {
            async fn send(&self, _: &str, _: &str, _: Option<Vec<u8>>, _: Duration) -> std::io::Result<(u16, Vec<u8>)> {
                Err(std::io::Error::new(self.0, "x"))
            }
        }
        let kind = |k| async move { WdaClient::new(Fails(k)).active_app().await };
        assert!(matches!(kind(crate::http::AFTER_SENT).await, Err(WdaError::Interrupted(_))));
        assert!(matches!(kind(crate::http::WRITE_FAILED).await, Err(WdaError::Write(_))));
        assert!(matches!(kind(std::io::ErrorKind::TimedOut).await, Err(WdaError::Timeout(_))));
        assert!(matches!(kind(std::io::ErrorKind::NotConnected).await, Err(WdaError::Unavailable(_))));
        // 应答了但不是 JSON：WDA 收到了请求
        let m = Mock::with(&[(200, "<html>")]);
        assert!(matches!(WdaClient::new(&m).active_app().await, Err(WdaError::Interrupted(_))));
    }

    /// 经 XCTest 的请求（工具调用和探测一样算）连续 3 次没有应答就判定卡死：之后不再发，直接报不可用；
    /// `/status` 不经 XCTest，既不算数也不受影响；中间有一次应答（哪怕是错误）就从头数。
    /// 长请求没应答时马上问一次前台 App（5 秒）：它也没应答就立刻判定卡死，它应答了只是慢
    #[tokio::test]
    async fn three_unanswered_requests_mean_stalled() {
        type Reply = std::io::Result<(u16, Vec<u8>)>;
        struct Scripted {
            replies: Mutex<VecDeque<Reply>>,
            sent: Mutex<Vec<String>>,
        }
        impl Transport for &Scripted {
            async fn send(
                &self,
                _: &str,
                path: &str,
                _: Option<Vec<u8>>,
                _: Duration,
            ) -> std::io::Result<(u16, Vec<u8>)> {
                self.sent.lock().unwrap().push(path.to_owned());
                self.replies.lock().unwrap().pop_front().expect("unexpected request")
            }
        }
        let timeout = || Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "t"));
        let ok = |b: &str| Ok((200, b.as_bytes().to_vec()));
        let t = Scripted {
            replies: Mutex::new(VecDeque::from([
                // a long request unanswered, the quick question after it answered: slow, not stuck
                timeout(),
                ok(r#"{"value":{}}"#),
                timeout(),
                ok(r#"{"value":{}}"#),
                ok(r#"{"value":{"error":"unknown error","message":"busy"}}"#),
                // the probe itself is short: no question after it
                timeout(),
                ok(r#"{"value":{"ready":true}}"#),
                // a long request and the quick question both unanswered: stuck at once
                timeout(),
                timeout(),
                ok(r#"{"value":{"ready":true}}"#),
            ])),
            sent: Mutex::default(),
        };
        let c = WdaClient::new(&t);
        assert!(c.wda_reports_locked().await.unwrap_err().no_answer());
        assert!(c.active_app().await.unwrap_err().no_answer());
        assert_eq!(c.unanswered(), 0, "两次短探测均获得回答，应清零");
        // 回了错误：还活着，从头数
        assert!(matches!(c.active_app().await, Err(WdaError::Remote { .. })));
        assert_eq!(c.unanswered(), 0);
        assert!(c.xctest_responsive().await.unwrap_err().no_answer());
        // /status 应答了也不清零
        assert!(c.ready().await.unwrap());
        assert_eq!(c.unanswered(), 1);
        assert!(!c.stalled());
        assert!(c.wda_reports_locked().await.is_err());
        assert!(c.stalled(), "the quick question went unanswered too");
        assert!(t.replies.lock().unwrap().len() == 1, "every scripted reply but /status was used");
        // 卡死后经 XCTest 的请求不再发出，直接报不可用（调用方可以退到 HID）；/status 照常
        let sent = t.sent.lock().unwrap().len();
        assert!(matches!(c.screenshot_png().await, Err(WdaError::Unavailable(_))));
        assert!(matches!(c.xctest_responsive().await, Err(WdaError::Unavailable(_))));
        assert_eq!(t.sent.lock().unwrap().len(), sent);
        assert!(c.ready().await.unwrap());
        assert!(c.stalled());
    }

    /// 较长请求超时后，短探测的正常回答或远端错误都证明 WDA 仍有响应，应重新计数。
    #[tokio::test]
    async fn answered_probe_resets_stall_count_after_each_slow_request() {
        for (status, body) in [
            (200, r#"{"value":{}}"#),
            (500, r#"{"value":{"error":"unknown error","message":"busy"}}"#),
            (404, r#"{"message":"unknown route"}"#),
            (502, "temporarily unavailable"),
        ] {
            let replies = (0..4).flat_map(|_| [timed_out(), Ok((status, body.as_bytes().to_vec()))]).collect();
            let t = Script::new(replies);
            let c = WdaClient::new(&t);
            for _ in 0..4 {
                assert!(c.wda_reports_locked().await.unwrap_err().no_answer());
                assert_eq!(c.unanswered(), 0, "探测返回 {status} {body}");
                assert!(!c.stalled());
            }
            assert_eq!(t.sent().len(), 8, "每个慢请求后都应继续做短探测");
        }
    }

    /// 连不上不是收到回答，不能抹掉之前已经发生的无应答。
    #[tokio::test]
    async fn connection_error_does_not_reset_unanswered_count() {
        let t = Script::new(vec![
            timed_out(),
            Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "offline")),
            Ok((500, br#"{"value":{"error":"unknown error","message":"busy"}}"#.to_vec())),
        ]);
        let c = WdaClient::new(&t);
        assert!(c.xctest_responsive().await.unwrap_err().no_answer());
        assert_eq!(c.unanswered(), 1);
        assert!(matches!(c.xctest_responsive().await, Err(WdaError::Unavailable(_))));
        assert_eq!(c.unanswered(), 1);
        assert!(matches!(c.xctest_responsive().await, Err(WdaError::Remote { .. })));
        assert_eq!(c.unanswered(), 0);
        assert!(!c.stalled());
    }

    /// 两个请求未应答即可触发快速恢复，但实际计数和原因不能伪装成三次无应答。
    #[tokio::test]
    async fn long_request_and_failed_probe_keep_the_actual_count_and_reason() {
        let t = Script::new(vec![timed_out(), timed_out(), ok(r#"{"value":{"ready":true}}"#)]);
        let c = WdaClient::new(&t);
        assert!(c.wda_reports_locked().await.unwrap_err().no_answer());
        assert_eq!(c.unanswered(), 2);
        assert_eq!(c.stall_reason(), Some(StallReason::LongRequestAndProbe));
        assert!(c.stalled());
        let why = c.stall_reason().unwrap().describe();
        assert!(why.contains("/wda/activeAppInfo"));
        assert!(!why.contains("3 次"));
        // 本地 /status 的回答不应把已记录的判定抹掉。
        assert!(c.ready().await.unwrap());
        assert_eq!(c.stall_reason(), Some(StallReason::LongRequestAndProbe));
        assert_eq!(t.sent(), ["GET /wda/locked", "GET /wda/activeAppInfo", "GET /status"]);
    }

    #[tokio::test]
    async fn three_short_unanswered_requests_report_a_consecutive_stall() {
        let t = Script::new(vec![timed_out(), timed_out(), timed_out()]);
        let c = WdaClient::new(&t);
        for n in 1..=STALL_LIMIT {
            assert!(c.xctest_responsive().await.unwrap_err().no_answer());
            assert_eq!(c.unanswered(), n);
            assert_eq!(c.stalled(), n == STALL_LIMIT);
        }
        assert_eq!(c.stall_reason(), Some(StallReason::ConsecutiveRequests(STALL_LIMIT)));
        // 后续调用仍立即拒绝，不再发送请求。
        assert!(matches!(c.xctest_responsive().await, Err(WdaError::Unavailable(_))));
        assert_eq!(t.sent().len(), STALL_LIMIT as usize);
    }

    /// 结束会话的回答只用于清理，不应代替真正的前台响应探测。
    #[tokio::test]
    async fn answered_session_delete_does_not_mask_a_failed_probe() {
        let t = Script::new(vec![ok(r#"{"sessionId":"A"}"#), timed_out(), ok(r#"{"value":null}"#), timed_out()]);
        let mut c = WdaClient::new(&t);
        assert!(matches!(c.press_button("home").await, Err(WdaError::Timeout(_))));
        assert_eq!(c.unanswered(), 2);
        assert_eq!(c.stall_reason(), Some(StallReason::LongRequestAndProbe));
        assert_eq!(t.sent()[2], "DELETE /session/A");
    }

    /// 经 XCTest 的探测：只有「送到了却没有应答」算卡死的迹象；WDA 回了错误说明它还在办事，连不上归 `/status` 管
    #[tokio::test]
    async fn xctest_probe_counts_only_missing_answers() {
        struct Fails(std::io::ErrorKind);
        impl Transport for Fails {
            async fn send(&self, _: &str, _: &str, _: Option<Vec<u8>>, t: Duration) -> std::io::Result<(u16, Vec<u8>)> {
                assert_eq!(t, XCTEST_PROBE_TIMEOUT);
                Err(std::io::Error::new(self.0, "x"))
            }
        }
        let probe = |k| async move { WdaClient::new(Fails(k)).xctest_responsive().await };
        assert!(probe(std::io::ErrorKind::TimedOut).await.unwrap_err().no_answer());
        assert!(probe(crate::http::AFTER_SENT).await.unwrap_err().no_answer());
        assert!(!probe(std::io::ErrorKind::NotConnected).await.unwrap_err().no_answer());
        let m = Mock::with(&[
            (200, r#"{"value":{"bundleId":"com.apple.springboard","pid":60}}"#),
            (500, r#"{"value":{"error":"unknown error","message":"busy"}}"#),
        ]);
        let c = WdaClient::new(&m);
        assert!(c.xctest_responsive().await.is_ok());
        assert!(!c.xctest_responsive().await.unwrap_err().no_answer());
        assert_eq!(m.seen.lock().unwrap()[0].1, "/wda/activeAppInfo");
    }

    #[tokio::test]
    async fn screen_size_is_cached_and_status_ready() {
        let unknown = r#"{"value":{"error":"unknown command","message":"no"}}"#;
        let m = Mock::with(&[
            (200, r#"{"value":{"screenSize":{"width":430,"height":932},"scale":3}}"#),
            (200, r#"{"sessionId":"S"}"#),
            (404, unknown),
            (404, unknown),
            (200, r#"{"value":{"ready":true}}"#),
        ]);
        let mut c = WdaClient::new(&m);
        // 问不到方向：按缓存的尺寸
        assert_eq!(c.screen_size().await.unwrap(), ScreenSize { width: 430.0, height: 932.0 });
        assert_eq!(c.screen_size().await.unwrap().width, 430.0);
        assert!(c.ready().await.unwrap());
        let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
        assert_eq!(paths.iter().filter(|p| *p == "/wda/screen").count(), 1);
    }

    #[tokio::test]
    async fn rotation_reads_z_angle() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":{"x":0,"y":0,"z":270}}"#),
            (200, r#"{"value":{"x":0,"y":0,"z":12}}"#),
        ]);
        let mut c = WdaClient::new(&m);
        assert_eq!(c.rotation().await.unwrap(), ictl_device::Rotation::TopToLeft);
        assert!(c.rotation().await.is_err());
        assert_eq!(m.seen.lock().unwrap()[1].1, "/session/S/rotation");
    }

    /// WDA 在横屏时启动，缓存的是 1024x768；转成竖屏后要给 768x1024
    #[tokio::test]
    async fn screen_size_follows_orientation_after_rotation() {
        let m = Mock::with(&[
            (200, r#"{"value":{"screenSize":{"width":1024,"height":768},"scale":2}}"#),
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":"LANDSCAPE"}"#),
            (200, r#"{"value":"PORTRAIT"}"#),
        ]);
        let mut c = WdaClient::new(&m);
        let v = c.view().await.unwrap();
        assert_eq!(
            (v.screen, v.orientation),
            (ScreenSize { width: 1024.0, height: 768.0 }, Some(elements::Orientation::LandscapeLeft))
        );
        assert_eq!(c.screen_size().await.unwrap(), ScreenSize { width: 768.0, height: 1024.0 });
        assert_eq!(m.seen.lock().unwrap()[2].1, "/session/S/orientation");
    }

    /// 从主屏打开网址：等 Safari 到前台、元素树稳定才返回，不是发完请求就走；一闪而过的系统服务不算
    #[tokio::test]
    async fn open_url_waits_for_the_opening_app() {
        let m = Mock::with(&[
            (200, r#"{"value":{"bundleId":"com.apple.springboard"}}"#),
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":null}"#),
            (200, r#"{"value":{"bundleId":"com.apple.DocumentManager.DockFolderViewService"}}"#),
            (200, r#"{"value":{"bundleId":"com.apple.mobilesafari"}}"#),
            (200, r#"{"value":{"bundleId":"com.apple.mobilesafari"}}"#),
            (200, r#"{"value":"<a/>"}"#),
            (200, r#"{"value":"<a/>"}"#),
        ]);
        let mut c = WdaClient::new(&m);
        let t0 = std::time::Instant::now();
        let (r, app) = c.open_url("https://example.com").await.unwrap();
        assert_eq!((r, app.as_str()), (Launch::Foreground, "com.apple.mobilesafari"));
        assert!(t0.elapsed() >= LAUNCH_MIN);
        let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
        assert_eq!(paths[2], "/session/S/url");
        assert!(paths[6].starts_with("/source"), "{paths:?}");
    }

    #[tokio::test]
    async fn source_request_excludes_slow_attributes() {
        let m = Mock::with(&[(200, r#"{"value":"<x/>"}"#)]);
        let mut c = WdaClient::new(&m);
        c.source_xml().await.unwrap();
        let path = m.seen.lock().unwrap()[0].1.clone();
        assert!(path.starts_with("/source?format=xml&excluded_attributes=visible,"), "{path}");
    }

    #[tokio::test]
    async fn passcode_field_reads_secure_text_field_value() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":[{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}]}"#),
            (200, r#"{"value":"共6位数，已输入2位"}"#),
        ]);
        let mut c = WdaClient::new(&m);
        assert_eq!(c.passcode_field().await.unwrap(), Some((6, 2)));
        assert_eq!(m.seen.lock().unwrap()[2].1, "/session/S/element/E1/attribute/value");
    }

    #[tokio::test]
    async fn focused_field_reads_its_type_and_placeholder() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":{"ELEMENT":"F1","element-6066-11e4-a52e-4f735466cecf":"F1"}}"#),
            (200, r#"{"value":"XCUIElementTypeSearchField"}"#),
            (200, r#"{"value":"搜索"}"#),
            // empty: reads its placeholder
            (200, r#"{"value":"搜索"}"#),
            (200, r#"{"value":"wifi"}"#),
        ]);
        let mut c = WdaClient::new(&m);
        let f = c.focused_field().await.unwrap().unwrap();
        assert_eq!(f, FocusedField { id: "F1".into(), kind: "SearchField".into(), placeholder: "搜索".into() });
        assert_eq!(c.field_text(&f).await.unwrap(), "");
        assert_eq!(c.field_text(&f).await.unwrap(), "wifi");
        let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
        assert_eq!(
            paths[1..4],
            [
                "/session/S/element/active",
                "/session/S/element/F1/attribute/type",
                "/session/S/element/F1/attribute/placeholderValue"
            ]
        );
    }

    #[tokio::test]
    async fn no_focused_element_is_none() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"S"}"#),
            (404, r#"{"value":{"error":"no such element","message":"No element is in focus"}}"#),
        ]);
        assert_eq!(WdaClient::new(&m).focused_field().await.unwrap(), None);
    }

    /// Finding elements reads; clearing one changes the phone (its session is ended when it doesn't answer)
    #[test]
    fn element_finds_read_and_element_edits_act() {
        assert!(!acts("POST", "/elements"));
        assert!(!acts("POST", "/element"));
        assert!(!acts("POST", "/element/E1/elements"));
        assert!(!acts("GET", "/element/active"));
        assert!(acts("POST", "/element/E1/clear"));
        assert!(acts("POST", "/wda/keys"));
    }

    #[test]
    fn actions_body_is_one_pointer_with_steps() {
        let b = actions_body(&tap_steps(10.0, 20.0, 80));
        let a = &b["actions"][0];
        assert_eq!(a["parameters"]["pointerType"], "touch");
        let types: Vec<_> = a["actions"].as_array().unwrap().iter().map(|s| s["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["pointerMove", "pointerDown", "pause", "pointerUp"]);
        assert_eq!(a["actions"][2]["duration"], 80);
        let s = swipe_steps((1.0, 2.0), (3.0, 4.0), 300);
        assert_eq!(s[3], PointerStep::Move { x: 3.0, y: 4.0, ms: 300 });
    }

    /// 按住再拖：按下后原地 move 按住（不是 pause，pause 拖不动图标），再移到终点；不按住就是普通滑动
    #[test]
    fn a_held_drag_holds_with_a_move_in_place() {
        let s = drag_steps((1.0, 2.0), (3.0, 4.0), 600, 700);
        assert_eq!(
            s,
            [
                PointerStep::Move { x: 1.0, y: 2.0, ms: 0 },
                PointerStep::Down,
                PointerStep::Move { x: 1.0, y: 2.0, ms: 600 },
                PointerStep::Move { x: 3.0, y: 4.0, ms: 700 },
                PointerStep::Up,
            ]
        );
        assert_eq!(drag_steps((1.0, 2.0), (3.0, 4.0), 0, 300), swipe_steps((1.0, 2.0), (3.0, 4.0), 300));
    }

    #[tokio::test]
    async fn locked_checks_tree_until_unlocked_once_then_trusts_wda() {
        let lock_xml = serde_json::to_string(include_str!("../tests/fixtures/lockscreen-ios27.xml")).unwrap();
        let home_xml = serde_json::to_string(include_str!("../tests/fixtures/home.xml")).unwrap();
        let sb = r#"{"value":{"bundleId":"com.apple.springboard"}}"#;
        let no = r#"{"value":false}"#;
        let (lock_src, home_src) = (format!(r#"{{"value":{lock_xml}}}"#), format!(r#"{{"value":{home_xml}}}"#));
        let m = Mock::with(&[
            // 1. WDA 在锁屏时启动：/wda/locked 误报 false，元素树是锁屏
            (200, no),
            (200, sb),
            (200, &lock_src),
            // 2. 人工解锁后：主屏幕，确认一次
            (200, no),
            (200, sb),
            (200, &home_src),
            // 3. 之后 WDA 说没锁就直接信
            (200, no),
            // 4. WDA 说锁了：核对一次，确实是锁屏
            (200, r#"{"value":true}"#),
            (200, sb),
            (200, &lock_src),
        ]);
        let mut c = WdaClient::new(&m);
        assert!(c.locked().await.unwrap());
        assert!(!c.locked().await.unwrap());
        assert!(!c.locked().await.unwrap());
        assert!(c.locked().await.unwrap());
        let paths: Vec<String> = m.seen.lock().unwrap().iter().map(|(_, p, _)| p.clone()).collect();
        assert_eq!(paths.iter().filter(|p| p.starts_with("/source")).count(), 3, "{paths:?}");
    }

    #[tokio::test]
    async fn notification_center_is_not_locked_even_if_wda_says_so() {
        let nc = serde_json::to_string(include_str!("../tests/fixtures/notification-center-ios27.xml")).unwrap();
        let pin = serde_json::to_string(include_str!("../tests/fixtures/passcode-view-ios27.xml")).unwrap();
        let sb = r#"{"value":{"bundleId":"com.apple.springboard"}}"#;
        let yes = r#"{"value":true}"#;
        let (nc, pin) = (format!(r#"{{"value":{nc}}}"#), format!(r#"{{"value":{pin}}}"#));
        let m = Mock::with(&[(200, yes), (200, sb), (200, &nc), (200, yes), (200, sb), (200, &pin), (200, yes)]);
        let mut c = WdaClient::new(&m);
        assert!(!c.locked().await.unwrap(), "通知中心");
        assert!(c.locked().await.unwrap(), "密码键盘界面");
        // 核对失败时按 WDA 的结果
        assert!(c.locked().await.unwrap());
    }

    #[tokio::test]
    async fn locked_falls_back_to_wda_when_tree_check_fails() {
        // 核对失败（例如取元素树超时）不能把可用的手机误判成锁屏，也不能就此信任 WDA
        let m = Mock::with(&[(200, r#"{"value":false}"#)]);
        let mut c = WdaClient::new(&m);
        assert!(!c.locked().await.unwrap());
        assert!(!c.lock_state_trusted);
    }

    /// 传输层给出的一个结果：状态码和响应体，或者超时之类的错误
    type Reply = std::io::Result<(u16, Vec<u8>)>;

    /// 按顺序给出预设结果（可以是超时），记下每个请求的方法、路径和请求体
    #[derive(Default)]
    struct Script {
        replies: Mutex<VecDeque<Reply>>,
        sent: Mutex<Vec<(String, String, Option<Value>)>>,
    }

    impl Script {
        fn new(replies: Vec<Reply>) -> Self {
            Self { replies: Mutex::new(replies.into()), ..Default::default() }
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().iter().map(|(m, p, _)| format!("{m} {p}")).collect()
        }

        fn body(&self, i: usize) -> Value {
            self.sent.lock().unwrap()[i].2.clone().expect("a body")
        }
    }

    impl Transport for &Script {
        async fn send(
            &self,
            method: &str,
            path: &str,
            body: Option<Vec<u8>>,
            _: Duration,
        ) -> std::io::Result<(u16, Vec<u8>)> {
            let body = body.map(|b| serde_json::from_slice(&b).unwrap());
            self.sent.lock().unwrap().push((method.into(), path.into(), body));
            self.replies.lock().unwrap().pop_front().expect("unexpected request")
        }
    }

    fn timed_out() -> Reply {
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "t"))
    }

    fn ok(body: &str) -> Reply {
        Ok((200, body.as_bytes().to_vec()))
    }

    /// 动作没有应答：先结束会话（WDA 不再执行这个会话里还在排队的请求），再问一次前台 App 判断是不是卡死；
    /// 下一个动作开新会话
    #[tokio::test]
    async fn an_unanswered_action_ends_its_session() {
        let t = Script::new(vec![
            ok(r#"{"sessionId":"A"}"#),
            timed_out(),
            ok(r#"{"value":null}"#),
            ok(r#"{"value":{}}"#),
            ok(r#"{"sessionId":"B"}"#),
            ok(r#"{"value":null}"#),
        ]);
        let mut c = WdaClient::new(&t);
        assert!(matches!(c.press_button("home").await, Err(WdaError::Timeout(_))));
        c.press_button("home").await.unwrap();
        assert_eq!(
            t.sent(),
            [
                "POST /session",
                "POST /session/A/wda/pressButton",
                "DELETE /session/A",
                "GET /wda/activeAppInfo",
                "POST /session",
                "POST /session/B/wda/pressButton",
            ]
        );
    }

    /// 读没有应答不结束会话：排在读后面的不会动手机，结束了只会让下一次开会话也排到它后面
    #[tokio::test]
    async fn an_unanswered_read_keeps_its_session() {
        let t = Script::new(vec![
            ok(r#"{"sessionId":"A"}"#),
            timed_out(),
            ok(r#"{"value":{}}"#),
            timed_out(),
            ok(r#"{"value":{}}"#),
            ok(r#"{"value":null}"#),
        ]);
        let mut c = WdaClient::new(&t);
        assert!(c.alert_text().await.unwrap_err().no_answer());
        assert!(c.keyboard_shown().await.unwrap_err().no_answer());
        c.press_button("home").await.unwrap();
        assert_eq!(
            t.sent(),
            [
                "POST /session",
                "GET /session/A/alert/text",
                "GET /wda/activeAppInfo",
                "POST /session/A/elements",
                "GET /wda/activeAppInfo",
                "POST /session/A/wda/pressButton",
            ]
        );
    }

    #[test]
    fn snapshot_limits_tighten_when_slow_and_loosen_when_fast() {
        let slow = Some(SLOW_SOURCE);
        let fast = Some(Duration::from_millis(300));
        let fair = Some(Duration::from_secs(3));
        assert_eq!(next_snapshot_level(0, slow), 1);
        assert_eq!(next_snapshot_level(1, None), 2);
        assert_eq!(next_snapshot_level(3, None), 3, "never tighter than the last level");
        assert_eq!(next_snapshot_level(2, fair), 2);
        assert_eq!(next_snapshot_level(2, fast), 1);
        assert_eq!(next_snapshot_level(0, fast), 0);
        assert_eq!(next_snapshot_level(0, fair), 0);
        // 先限子元素再减层数，都有下限
        assert_eq!(SNAPSHOT_LEVELS[1], SnapshotLimits { depth: 50, children: Some(150) });
        assert!(SNAPSHOT_LEVELS.windows(2).all(|w| w[1].depth <= w[0].depth));
        assert_eq!(SNAPSHOT_LEVELS[3], SnapshotLimits { depth: 12, children: Some(60) });
    }

    /// 读树超时后下一次先设上限再读，读得快了再放宽；新会话里 WDA 是不限的，不为读树专门设
    #[tokio::test]
    async fn a_timed_out_tree_is_read_again_with_limits() {
        let xml = r#"{"value":"<XCUIElementTypeApplication/>"}"#;
        let t = Script::new(vec![
            timed_out(),
            ok(r#"{"value":{}}"#),
            ok(r#"{"sessionId":"A"}"#),
            ok(r#"{"value":null}"#),
            ok(xml),
            ok(r#"{"value":null}"#),
            ok(xml),
            ok(xml),
        ]);
        let mut c = WdaClient::new(&t);
        assert!(matches!(c.source_xml().await, Err(WdaError::Timeout(_))));
        assert_eq!(c.snapshot_note(), None, "nothing was read with limits yet");
        assert_eq!(c.source_xml().await.unwrap(), "<XCUIElementTypeApplication/>");
        assert!(c.snapshot_note().unwrap().contains("150"));
        c.source_xml().await.unwrap();
        assert_eq!(c.snapshot_note(), None);
        c.source_xml().await.unwrap();
        let sent = t.sent();
        assert!(sent[0].starts_with("GET /source?"));
        assert_eq!(sent[1..4], ["GET /wda/activeAppInfo", "POST /session", "POST /session/A/appium/settings"]);
        assert_eq!(t.body(3)["settings"], json!({"snapshotMaxDepth": 50, "snapshotMaxChildren": 150}));
        assert_eq!(sent[5], "POST /session/A/appium/settings");
        assert_eq!(t.body(5)["settings"]["snapshotMaxChildren"], i32::MAX);
        assert_eq!(sent.len(), 8, "the last read needs no settings: {sent:?}");
    }

    #[test]
    fn the_detection_point_is_below_banners_in_either_orientation() {
        assert_eq!(detection_point(ScreenSize { width: 430.0, height: 932.0 }), (215, 215));
        assert_eq!(detection_point(ScreenSize { width: 932.0, height: 430.0 }), (215, 215));
        assert_eq!(detection_point(ScreenSize { width: 744.0, height: 1133.0 }), (372, 372));
    }

    /// 第一次读元素树时挪一次判断前台 App 的点，之后不再设
    #[tokio::test]
    async fn the_first_tree_read_moves_the_detection_point_once() {
        let xml = r#"{"value":"<XCUIElementTypeApplication/>"}"#;
        let t = Script::new(vec![
            ok(r#"{"value":{"screenSize":{"width":430,"height":932}}}"#),
            ok(r#"{"sessionId":"A"}"#),
            ok(r#"{"value":"PORTRAIT"}"#),
            ok(r#"{"value":null}"#),
            ok(xml),
            ok(r#"{"value":"PORTRAIT"}"#),
            ok(xml),
        ]);
        let mut c = WdaClient::new(&t);
        c.tree().await.unwrap();
        c.tree().await.unwrap();
        let sent = t.sent();
        assert_eq!(sent[3], "POST /session/A/appium/settings", "{sent:?}");
        assert_eq!(t.body(3)["settings"], json!({"activeAppDetectionPoint": "215,215"}));
        assert_eq!(sent.len(), 7, "{sent:?}");
        assert!(sent[6].starts_with("GET /source?"));
    }
}
