//! Control protocol request types, bounds, and classification helpers.

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use crate::{
    Response, SCREEN_KEYFRAME, ScreenMsg, TASK, TASK_STEP, TaskEvent, TaskInfo, open_mjpeg_stream, open_screen_stream,
    open_task,
};

/// [`Request::TapElement`] 屏幕上找不到时边滑边找：`down` 看下面的内容（手指往上滑），否则看上面的；最多滑 `max` 次
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollSearch {
    pub down: bool,
    pub max: u32,
}

/// A device step of `setup` the instance holding the device does for it ([`Request::SetupStep`])
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStepKind {
    /// Pair with the phone: answers `paired`, or `pending` while the phone waits for "Trust" and the passcode
    Pair,
    /// Show the Developer Mode switch in the phone's settings: answers `revealed`
    DevModeReveal,
    /// Whether Developer Mode is on: answers `on` or `off`
    DevModeStatus,
}

/// How `text` is matched in [`ElementSelector`]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Exact label/name first, falling back to case-insensitive substring match (default)
    #[default]
    Auto,
    /// Exact label or name match only
    Exact,
    /// Case-insensitive substring match in label, name, or value
    Contains,
}

impl MatchMode {
    pub fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Optional structural and matching constraints for [`Request::TapElement`], [`Request::WaitFor`], and
/// [`Request::WaitForDisappearance`]. Omitted from JSON when empty so older daemons accept plain requests unchanged
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementSelector {
    /// Element type (e.g. `Button`, `TextField`, `SearchField`), case-insensitive
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Accessibility identifier (`name`), exact match
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// How `text` is matched (`auto`, `exact`, or `contains`)
    #[serde(default, skip_serializing_if = "MatchMode::is_auto")]
    pub mode: MatchMode,
}

impl ElementSelector {
    pub fn is_empty(&self) -> bool {
        self.kind.is_none() && self.id.is_none() && self.mode.is_auto()
    }

    pub fn has_structure(&self) -> bool {
        self.kind.as_deref().is_some_and(|s| !s.is_empty()) || self.id.as_deref().is_some_and(|s| !s.is_empty())
    }
}

/// [`ScrollSearch::max`] 的上限：每次慢滑加等列表停下约 2.5 秒，滑满 15 次也就半分钟出头
pub const MAX_SEARCH_SCROLLS: u32 = 15;
pub const MAX_SCROLL_ROWS: usize = 150;

/// Bounds and matching rules for a standalone list scan. Unknown fields are ignored, as in every other request: keys
/// that apply to all requests (`lang`, `watched`, `cancel_on_close`…) sit beside these, and one a newer client adds
/// would otherwise make an older daemon refuse this request alone
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScrollCollect {
    pub down: bool,
    pub max_scrolls: u32,
    pub filter: String,
    pub until: String,
    pub limit: usize,
    /// Where to swipe across, in points from the left; the middle of the screen if not given. An iPad split view
    /// scrolls the column swiped in: the sidebar needs an x inside it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// [`Request::Tap`] 一次最多连点几下
pub const MAX_TAP_COUNT: u32 = 10;

fn one() -> u32 {
    1
}

fn is_one(n: &u32) -> bool {
    *n == 1
}
/// [`Request::TapSequence`] 一个手势最多点几个点（同 `iphone_batch` 最多几步）
pub const MAX_SEQUENCE_POINTS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Prepare,
    Down,
    Move,
    Up,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// MCP 的 iphone_status：与 toolkit 相同的文字
    Status,
    /// The phone's own screen video (CoreDevice screen media stream, HEVC, iOS 27 and later) for the control page.
    /// Streaming: the daemon answers one line ([`Response::Text`] when it streams, [`Response::Error`] when it
    /// can't), then sends [`ScreenMsg`] messages on the same connection until either side closes it. The client may
    /// write [`SCREEN_KEYFRAME`] to ask for a keyframe. Use [`open_screen_stream`]
    ScreenStream,
    /// WDA's MJPEG stream (the phone's port 9100) for the control page on iOS 17–26, so the viewer doesn't reach for the
    /// phone itself (D2). Streaming: the daemon answers one line ([`Response::Text`] when it streams,
    /// [`Response::Error`] when it can't), then passes on WDA's HTTP answer, headers and multipart frames as they come,
    /// until either side closes. Use [`open_mjpeg_stream`]
    MjpegStream,
    /// 区域单位为点；None 表示整屏
    Screenshot {
        region: Option<(f64, f64, f64, f64)>,
    },
    ListElements {
        filter: String,
        limit: usize,
        /// 守护进程缓存的元素树不超过这么旧就直接用，不再读（0 = 总是重读）。
        /// 用于「等界面稳定」之后紧接着列元素：Settle 刚读过
        #[serde(default)]
        max_age_ms: u64,
        /// 手机锁着时不自动解锁，直接报错（控制台的只读令牌：只能看，不能让守护进程输入锁屏密码）
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keep_locked: bool,
    },
    /// Inspect successive visible viewports, leaving a matching element on screen.
    ScrollCollect(ScrollCollect),
    Tap {
        x: f64,
        y: f64,
        /// 在这一点连点几下（一个手势），1 = 普通点击。旧版本客户端不带，按 1；为 1 时不写进请求，旧版本守护进程照常认
        #[serde(default = "one", skip_serializing_if = "is_one")]
        count: u32,
    },
    /// 依次点几个点，一个手势（`iphone_batch` 的 `one_gesture`）：同一个键盘上连按。旧版本守护进程不认识，报错，不会只点一部分
    TapSequence {
        points: Vec<(f64, f64)>,
    },
    TapElement {
        text: String,
        index: usize,
        /// 同 ListElements。调用方声明这一串点击不改变界面布局（例如在同一个键盘上连按）时才给
        #[serde(default)]
        max_age_ms: u64,
        /// 屏幕上没有时边滑边找。旧版本守护进程不认识这个字段，只在当前屏幕上找
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scroll: Option<ScrollSearch>,
        /// Optional type/id/match constraints; omitted when empty for compatibility with older daemons
        #[serde(default, skip_serializing_if = "ElementSelector::is_empty")]
        selector: ElementSelector,
    },
    LongPress {
        x: f64,
        y: f64,
        seconds: f64,
    },
    /// `hold`：先在起点按住几秒再移动（拖动图标、列表排序），0 是普通滑动。旧版本守护进程不认识这个字段，当普通滑动做
    Swipe {
        from: (f64, f64),
        to: (f64, f64),
        seconds: f64,
        #[serde(default, skip_serializing_if = "is_zero")]
        hold: f64,
    },
    TypeText {
        text: String,
        submit: bool,
    },
    /// A separate operation so an older daemon cannot silently insert the text instead.
    ReplaceText {
        text: String,
        submit: bool,
    },
    PressButton {
        button: String,
    },
    /// A finger that follows the pointer in the web viewer: `down` touches at the point, `move` drags it there, `up`
    /// lifts it there; `prepare` only opens the HID session, so the first touch doesn't wait for it. Over CoreDevice
    /// HID (iOS 27 and later), one report each, so the screen moves while the pointer does
    Touch {
        phase: TouchPhase,
        #[serde(default)]
        x: f64,
        #[serde(default)]
        y: f64,
    },
    ListApps {
        include_system: bool,
    },
    LaunchApp {
        app: String,
    },
    TerminateApp {
        app: String,
    },
    OpenUrl {
        url: String,
    },
    /// 等界面稳定：连续两次读到的元素一样，最多等 `max_ms`。操作后顺带返回界面时先调它
    Settle {
        max_ms: u64,
    },
    /// 查屏幕上有没有挡着的系统弹窗：有就返回提醒文字，没有返回空文字。不带 then 的操作做完后调
    AlertCheck,
    /// 等包含 `text`（或满足 `selector`）的元素出现，最多等 `timeout_ms`（代替 agent 反复截图轮询）
    WaitFor {
        text: String,
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "ElementSelector::is_empty")]
        selector: ElementSelector,
    },
    WaitForDisappearance {
        text: String,
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "ElementSelector::is_empty")]
        selector: ElementSelector,
    },
    /// 运维：守护进程和设备的状态快照（JSON 文本）
    Daemon,
    /// 运维：逐层诊断，返回多行文字
    Doctor,
    Resign {
        force: bool,
    },
    /// What clients of 0.3.2 and earlier send for `unlock-reset`. Answered as [`Request::UnlockResetDevice`] for
    /// the device being controlled: auto-unlock is disabled per device now, and this re-enables that one only
    UnlockReset,
    /// Re-enables auto-unlock for `udid` (a device connected now), or for the device being controlled when it isn't
    /// given; the other devices stay disabled. A UDID only, never a name: names change while devices stay connected,
    /// and the daemon's list of them isn't read again once complete, so a name could pick the wrong device. A request
    /// of its own, not a field on [`Request::UnlockReset`]: a daemon of 0.3.2 or earlier would drop the field and
    /// re-enable every device, while an unknown request is refused
    UnlockResetDevice {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        udid: Option<String>,
    },
    /// `setup`'s pairing and Developer Mode steps, done by the instance that holds `udid` (docs/dev/multi-instance-plan.md
    /// 4.5): refused unless it holds that device and no task is active or being opened
    SetupStep {
        udid: String,
        step: SetupStepKind,
    },
    /// `wda-bridge`: one connection to WDA's port on the device being controlled, relayed both ways once the answer
    /// line says `streaming`. Refused while a task is active; ended when a task is opened or the device switched
    WdaForward {
        udid: String,
    },
    /// 列出 USB 上接着的全部设备，标出正在控制哪台
    ListDevices,
    /// 改为控制另一台设备：`device` 是 UDID、设备名或 `auto`（取消选定，改回自动选）
    SelectDevice {
        device: String,
    },
    /// Developer tools: the phone's unified log. `process` is an app (name or bundle ID) or a process name, empty for
    /// all; `grep` matches message or subsystem; only lines from the last `since_secs` seconds
    AppLogs {
        #[serde(default)]
        process: String,
        #[serde(default)]
        grep: String,
        since_secs: u64,
        limit: usize,
        #[serde(default)]
        errors_only: bool,
    },
    /// Developer tools: list crash reports (filtered by `filter`), or return the one called `name`
    CrashReports {
        #[serde(default)]
        filter: String,
        #[serde(default)]
        name: String,
    },
    /// Developer tools: install a signed .ipa or .app from a path on the daemon's machine
    InstallApp {
        path: String,
    },
    /// Developer tools: remove an app (never the WebDriverAgent runner)
    UninstallApp {
        app: String,
    },
    /// Developer tools: inspectable pages and WebViews (Web Inspector)
    WebTargets,
    /// Developer tools: evaluate JavaScript in a page. `target` is an id from `WebTargets`, or text in its URL or title
    WebEval {
        #[serde(default)]
        target: String,
        expression: String,
        timeout_ms: u64,
    },
    /// Tasks (docs/task-control.md): opens a task bound to the controlled device. Streaming: the daemon answers one
    /// line ([`Response::Text`] whose text is `{"id","token"}`, or [`Response::Error`]), then sends the task's
    /// events (one JSON [`TaskEvent`] per line) until it ends. The connection is the task's lease: when it closes
    /// before the task ended, the task is abandoned. Use [`open_task`]
    TaskOpen {
        title: String,
        /// Who runs it, shown on the control page (`claude`, `codex`, the name of an external platform…)
        #[serde(default)]
        owner: String,
        /// Apps (names or bundle IDs) the task may act in; outside them each action waits for the user. Empty: no
        /// limit
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        apps: Vec<String>,
    },
    /// Streaming like [`Request::TaskOpen`]: the task's events so far, then new ones, until it ends
    TaskEvents {
        id: String,
    },
    /// Recent tasks, newest last, as a JSON array of [`TaskInfo`]
    TaskList,
    /// `pause` / `resume` / `takeover` / `handback` / `stop` / `confirm` (with `approve` and the waiting `step`).
    /// The user's: refused with a task's token, or from the task's agent ([`Request::TaskAgent`]), for everything but
    /// `stop`
    TaskControl {
        id: String,
        action: String,
        #[serde(default)]
        step: Option<u64>,
        #[serde(default)]
        approve: bool,
        /// Who asks, for the task's log: `viewer`, `cli`, `owner`…
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        /// The process the request comes from when it is passed on (the page's API: the local client that called
        /// it); checked like the connection's own process
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_pid: Option<u32>,
    },
    /// From the process that opened the task, with its token, before it starts the agent: every process it starts
    /// from now on (and the ones those start) is the task's agent, whose control requests are refused
    /// ([`Request::TaskControl`])
    TaskAgent,
    /// With a task's token: the task's id while it is active, an error once it ended or for a token nobody knows.
    /// The viewer lets an agent on another machine use `/mcp` by its task's token alone (docs/task-control.md 7.6)
    TaskCheck,
    /// With the task's token ([`TASK`]): a tool call starts. Waits while the task isn't running, then records the
    /// step; the text is the step number. The calls the tool makes carry it ([`TASK_STEP`])
    TaskStep {
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// With the task's token: the tool call's result, shown on the page
    TaskStepDone {
        step: u64,
        ok: bool,
        #[serde(default)]
        ms: u64,
        #[serde(default)]
        text: String,
    },
    /// With the task's token: waits until the user approves (text) or rejects (error) a sensitive action
    TaskConfirm {
        action: String,
        #[serde(default)]
        detail: String,
    },
    /// With the task's token: what the agent says it is doing, shown with the next step
    TaskNote {
        text: String,
    },
    /// With the task's token: the task ended on its own
    TaskFinish {
        ok: bool,
        #[serde(default)]
        summary: String,
    },
}

impl Request {
    /// Inputs whose normal reply acknowledges sending the requested gesture or text.
    pub fn reports_delivery(&self) -> bool {
        matches!(
            self,
            Self::Tap { .. }
                | Self::TapElement { .. }
                | Self::TypeText { .. }
                | Self::ReplaceText { .. }
                | Self::Swipe { .. }
                | Self::LongPress { .. }
        )
    }

    /// Shared by the daemon and the MCP batch preflight, before any earlier steps run.
    pub fn check_required_text(&self) -> Result<(), String> {
        match self {
            // an empty ReplaceText empties the field
            Self::WaitFor { text, selector, .. } | Self::WaitForDisappearance { text, selector, .. }
                if text.is_empty() && !selector.has_structure() =>
            {
                Err(ictl_l10n::tr!("text 不能为空", "text can't be empty").into())
            }
            Self::TapElement { text, selector, .. } if text.is_empty() && !selector.has_structure() => Err(
                ictl_l10n::tr!("text 不能为空(或指定 kind / id)", "text can't be empty (unless kind or id is given)")
                    .into(),
            ),
            _ => Ok(()),
        }
    }

    /// 会改变手机状态的操作。守护进程忙时排队太久就放弃，免得点击落到已经变了的屏幕上
    /// Every variant is listed (no `_`), so a new request type has to be classified here
    pub fn is_device_action(&self) -> bool {
        match self {
            // only opens the HID session ahead of the finger: nothing on the screen changes
            Self::Touch { phase: TouchPhase::Prepare, .. } => false,
            Self::Tap { .. }
            | Self::ScrollCollect(_)
            | Self::TapElement { .. }
            | Self::TapSequence { .. }
            | Self::LongPress { .. }
            | Self::Swipe { .. }
            | Self::TypeText { .. }
            | Self::ReplaceText { .. }
            | Self::PressButton { .. }
            | Self::Touch { .. }
            | Self::LaunchApp { .. }
            | Self::TerminateApp { .. }
            | Self::OpenUrl { .. }
            | Self::SelectDevice { .. }
            | Self::InstallApp { .. }
            | Self::UninstallApp { .. }
            | Self::WebEval { .. } => true,
            Self::Status
            | Self::ScreenStream
            | Self::MjpegStream
            | Self::Screenshot { .. }
            | Self::ListElements { .. }
            | Self::ListApps { .. }
            | Self::Settle { .. }
            | Self::AlertCheck
            | Self::WaitFor { .. }
            | Self::WaitForDisappearance { .. }
            | Self::Daemon
            | Self::Doctor
            | Self::Resign { .. }
            | Self::UnlockReset
            | Self::UnlockResetDevice { .. }
            | Self::SetupStep { .. }
            | Self::WdaForward { .. }
            | Self::ListDevices
            | Self::AppLogs { .. }
            | Self::CrashReports { .. }
            | Self::WebTargets
            | Self::TaskOpen { .. }
            | Self::TaskEvents { .. }
            | Self::TaskList
            | Self::TaskControl { .. }
            | Self::TaskAgent
            | Self::TaskCheck
            | Self::TaskStep { .. }
            | Self::TaskStepDone { .. }
            | Self::TaskConfirm { .. }
            | Self::TaskNote { .. }
            | Self::TaskFinish { .. } => false,
        }
    }
}
