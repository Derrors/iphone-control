//! 守护进程控制接口：Unix socket（0600，默认位置见 [`default_socket`]），每行一个 JSON。
//! MCP 进程和 CLI 都通过它访问设备；本机不开 TCP 端口。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 以 root 运行（系统服务）时的控制接口和数据目录
pub const SYSTEM_SOCKET: &str = "/run/iphone-control/ctl.sock";
pub const SYSTEM_DATA_DIR: &str = "/var/lib/iphone-control";

pub fn running_as_root() -> bool {
    // SAFETY: geteuid 没有前置条件，也不会失败
    unsafe { libc::geteuid() == 0 }
}

/// 默认数据目录：root 用 [`SYSTEM_DATA_DIR`]；普通用户（从源码运行）用 `$XDG_DATA_HOME/iphone-control`，
/// 即 `~/.local/share/iphone-control`
pub fn default_data_dir() -> PathBuf {
    if running_as_root() {
        return SYSTEM_DATA_DIR.into();
    }
    let xdg = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    match xdg
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".local/share")))
    {
        Some(base) => base.join("iphone-control"),
        None => SYSTEM_DATA_DIR.into(),
    }
}

/// 默认控制接口：root 用 [`SYSTEM_SOCKET`]；普通用户用 `$XDG_RUNTIME_DIR/iphone-control/ctl.sock`，
/// 没有这个变量时放进默认数据目录。守护进程、MCP、CLI 用同一个函数算，不用另外配置就能找到彼此
pub fn default_socket() -> PathBuf {
    if running_as_root() {
        return SYSTEM_SOCKET.into();
    }
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(run) => PathBuf::from(run).join("iphone-control/ctl.sock"),
        None => default_data_dir().join("ctl.sock"),
    }
}

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
    },
    Tap {
        x: f64,
        y: f64,
    },
    TapElement {
        text: String,
        index: usize,
        /// 同 ListElements。调用方声明这一串点击不改变界面布局（例如在同一个键盘上连按）时才给
        #[serde(default)]
        max_age_ms: u64,
    },
    LongPress {
        x: f64,
        y: f64,
        seconds: f64,
    },
    Swipe {
        from: (f64, f64),
        to: (f64, f64),
        seconds: f64,
    },
    TypeText {
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
    /// 等包含 `text` 的元素出现，最多等 `timeout_ms`（代替 agent 反复截图轮询）
    WaitFor {
        text: String,
        timeout_ms: u64,
    },
    /// 运维：守护进程和设备的状态快照（JSON 文本）
    Daemon,
    /// 运维：逐层诊断，返回多行文字
    Doctor,
    Resign {
        force: bool,
    },
    UnlockReset,
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
    },
    /// Streaming like [`Request::TaskOpen`]: the task's events so far, then new ones, until it ends
    TaskEvents {
        id: String,
    },
    /// Recent tasks, newest last, as a JSON array of [`TaskInfo`]
    TaskList,
    /// `pause` / `resume` / `takeover` / `handback` / `stop` / `confirm` (with `approve` and the waiting `step`)
    TaskControl {
        id: String,
        action: String,
        #[serde(default)]
        step: Option<u64>,
        #[serde(default)]
        approve: bool,
    },
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

/// One event of a task, as the daemon streams it and the control page shows it: `step`, `confirm`, `state`, `done`
/// (and `task`, the first one, with the task's [`TaskInfo`])
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub event: String,
    pub data: serde_json::Value,
}

/// A task as [`Request::TaskList`] lists it
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub id: String,
    pub title: String,
    pub owner: String,
    pub udid: String,
    /// `running` / `paused` / `takeover` / `waiting`, or how it ended: `completed` / `failed` / `cancelled` /
    /// `rejected` / `abandoned` / `interrupted`
    pub state: String,
    /// Unix milliseconds
    pub started: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
    /// Steps so far
    #[serde(default)]
    pub steps: u64,
}

impl TaskInfo {
    pub fn active(&self) -> bool {
        matches!(self.state.as_str(), "running" | "paused" | "takeover" | "waiting")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Text {
        text: String,
        /// What the text reports, as fields, so clients don't have to read the text (which depends on the
        /// language). Daemons before this field existed don't send it; their text is always Chinese
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<Outcome>,
    },
    /// JPEG base64 + 说明文字（尺寸、放大倍数、坐标换算式）
    Image { jpeg_base64: String, caption: String },
    Error {
        message: String,
        /// The touch Down could not open a HID session and sent no contact report.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        retry_as_gesture: bool,
    },
}

/// Facts about a [`Response::Text`] that clients act on (see [`tapped_nothing`], [`alert_reported`])
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// `iphone_tap_element` tapped nothing: no match, index out of range, or the element is covered
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tapped_nothing: bool,
    /// The text already reports a system alert blocking the screen
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alert_reported: bool,
}

impl Response {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into(), outcome: None }
    }
    /// Text with its [`Outcome`] (the daemon answers every text request this way)
    pub fn text_with(s: impl Into<String>, outcome: Outcome) -> Self {
        Self::Text { text: s.into(), outcome: Some(outcome) }
    }
    pub fn error(s: impl Into<String>) -> Self {
        Self::Error { message: s.into(), retry_as_gesture: false }
    }
}

impl Request {
    /// 会改变手机状态的操作。守护进程忙时排队太久就放弃，免得点击落到已经变了的屏幕上
    /// Every variant is listed (no `_`), so a new request type has to be classified here
    pub fn is_device_action(&self) -> bool {
        match self {
            Self::Tap { .. }
            | Self::TapElement { .. }
            | Self::LongPress { .. }
            | Self::Swipe { .. }
            | Self::TypeText { .. }
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
            | Self::Screenshot { .. }
            | Self::ListElements { .. }
            | Self::ListApps { .. }
            | Self::Settle { .. }
            | Self::AlertCheck
            | Self::WaitFor { .. }
            | Self::Daemon
            | Self::Doctor
            | Self::Resign { .. }
            | Self::UnlockReset
            | Self::ListDevices
            | Self::AppLogs { .. }
            | Self::CrashReports { .. }
            | Self::WebTargets
            | Self::TaskOpen { .. }
            | Self::TaskEvents { .. }
            | Self::TaskList
            | Self::TaskControl { .. }
            | Self::TaskStep { .. }
            | Self::TaskStepDone { .. }
            | Self::TaskConfirm { .. }
            | Self::TaskNote { .. }
            | Self::TaskFinish { .. } => false,
        }
    }
}

/// Messages after a [`Request::ScreenStream`] answer: one type byte, a 4-byte big-endian length, the payload
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScreenMsg {
    /// JSON: `codec` (WebCodecs string), before the first frame and whenever it changes
    Config = 0,
    /// A keyframe, Annex-B, with its parameter sets
    Key = 1,
    /// Any other frame, Annex-B
    Delta = 2,
    /// JSON: `rotation` in degrees (0, 90, 180, 270: how far the portrait top edge has turned clockwise), when it
    /// becomes known or changes. The video itself is always portrait
    Info = 3,
}

impl ScreenMsg {
    pub fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Config,
            1 => Self::Key,
            2 => Self::Delta,
            3 => Self::Info,
            _ => return None,
        })
    }

    /// The message with its header
    pub fn frame(self, payload: &[u8]) -> Vec<u8> {
        let mut m = Vec::with_capacity(5 + payload.len());
        m.push(self as u8);
        m.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        m.extend_from_slice(payload);
        m
    }
}

/// The byte a [`Request::ScreenStream`] client writes to ask for a keyframe
pub const SCREEN_KEYFRAME: u8 = b'K';

/// Opens a [`Request::ScreenStream`]: `Ok(Ok(..))` with the reading half (buffered: it may already hold the first
/// messages) and the writing half, `Ok(Err(why))` when the daemon can't stream, `Err` when it can't be reached
pub async fn open_screen_stream(
    socket: &std::path::Path,
    expect_udid: Option<&str>,
) -> std::io::Result<
    Result<(tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>, tokio::net::unix::OwnedWriteHalf), String>,
> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::UnixStream::connect(socket).await?;
    let (r, mut w) = stream.into_split();
    let mut v = serde_json::to_value(Request::ScreenStream)?;
    if let Some(o) = v.as_object_mut() {
        o.insert(LANG.into(), ictl_l10n::lang().code().into());
        if let Some(u) = expect_udid {
            o.insert(EXPECT_UDID.into(), u.into());
        }
    }
    let mut line = v.to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut r = BufReader::with_capacity(256 * 1024, r);
    let mut first = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(30), r.read_line(&mut first))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "screen stream: no answer"))??;
    Ok(match serde_json::from_str::<Response>(&first).map_err(std::io::Error::other)? {
        Response::Error { message, .. } => Err(message),
        _ => Ok((r, w)),
    })
}

/// 工具结果里报告系统弹窗挡着屏幕的固定写法（后面接「弹窗文字」）。MCP 端据此判断结果里是否已经报过弹窗，
/// 不能只找「系统弹窗」几个字：按名字点到的元素文字里也可能有
pub const ALERT_BLOCKING: &str = "屏幕上有系统弹窗";

/// `iphone_tap_element` 没点到时结果开头的几种写法（不算出错，单独调用时是普通结果；批量里要停下）
pub const TAP_MISSED: [&str; 3] = ["没找到", "只匹配到", "被挡住"];

/// Did `iphone_tap_element` tap nothing? Daemons that send [`Outcome`] say so there; older ones only write
/// Chinese text, recognised by its [`TAP_MISSED`] prefix
pub fn tapped_nothing(text: &str, outcome: Option<Outcome>) -> bool {
    match outcome {
        Some(o) => o.tapped_nothing,
        None => TAP_MISSED.iter().any(|p| text.starts_with(p)),
    }
}

/// Does the result already report a blocking system alert? As [`tapped_nothing`], with [`ALERT_BLOCKING`]
pub fn alert_reported(text: &str, outcome: Option<Outcome>) -> bool {
    match outcome {
        Some(o) => o.alert_reported,
        None => text.contains(ALERT_BLOCKING),
    }
}

/// 请求里带上这个键（值为 true）：客户端发完请求不半关闭连接，守护进程据此把「读到 EOF」当成调用方已经走了，
/// 还在排队等设备的请求就不再执行。旧版本的客户端发完就半关闭、不带这个键，守护进程对它们照旧处理完。
/// 放在请求对象里而不是改协议格式：旧版本守护进程会忽略这个多出来的键
pub const CANCEL_ON_CLOSE: &str = "cancel_on_close";

/// 请求里带上这个键（值为 UDID）：守护进程正在控制的不是这台就不执行。`iphone_batch` 用它防止中途换了设备
/// （自动选设备时拔掉正在控制的那台，会改控另一台）后，剩下的步骤落到另一台上
pub const EXPECT_UDID: &str = "expect_udid";
/// The live view whose geometry was used to calculate a coordinate.
pub const EXPECT_VIEW: &str = "expect_view";

/// Request key carrying a task's token (docs/task-control.md): the request is part of that task. While a task is
/// active the daemon only lets requests with its token act on the phone (and holds them while the task is paused or
/// taken over); a token of a task that ended refuses everything. Set it with [`join_task`] or [`as_task`]
pub const TASK: &str = "task";
/// Request key with the step (from [`Request::TaskStep`]) a request belongs to; see [`as_task`]
pub const TASK_STEP: &str = "task_step";
/// The environment variable that hands a task's token to `iphone-control mcp` (and anything else using this crate)
pub const TASK_TOKEN_ENV: &str = "IPHONE_CONTROL_TASK_TOKEN";

static PROCESS_TASK: std::sync::OnceLock<String> = std::sync::OnceLock::new();

tokio::task_local! {
    static CALL_TASK: (Option<String>, Option<u64>);
}

/// Every request this process sends from now on belongs to the task with `token` (the MCP server of an agent
/// started for a task). Only the first call counts
pub fn join_task(token: &str) {
    let _ = PROCESS_TASK.set(token.to_owned());
}

/// The task this process joined ([`join_task`]), if any
pub fn process_task() -> Option<&'static str> {
    PROCESS_TASK.get().map(String::as_str)
}

/// Runs `f` with its requests sent as part of a task: `token` overrides the process's ([`join_task`]), `step` says
/// which step they belong to
pub async fn as_task<F: std::future::Future>(token: Option<String>, step: Option<u64>, f: F) -> F::Output {
    let token = token.or_else(|| current_task().0);
    CALL_TASK.scope((token, step), f).await
}

fn current_task() -> (Option<String>, Option<u64>) {
    let (token, step) = CALL_TASK.try_with(Clone::clone).unwrap_or_default();
    (token.or_else(|| process_task().map(str::to_owned)), step)
}

/// Request key carrying the client's language (`zh` / `en`). The daemon answers in it; requests without it
/// (older clients) get Chinese, as before. Older daemons ignore the extra key
pub const LANG: &str = "lang";

/// 发一个请求、读一个响应（客户端用）。`timeout` 为 None 时一直等（例如 `sign` 要等签名装完）。
/// 返回之前被丢掉（超时、调用方取消）时连接随之关闭，守护进程会放弃还在排队的这个请求
pub async fn call(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
) -> std::io::Result<Response> {
    call_expecting(socket, req, timeout, None).await
}

/// 同 [`call`]；`expect_udid` 见 [`EXPECT_UDID`]
pub async fn call_expecting(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
    expect_udid: Option<&str>,
) -> std::io::Result<Response> {
    exchange_on(socket, req, timeout, expect_udid, None).await
}

/// Send a coordinate action only if this device and its measured geometry still match.
pub async fn call_in_view(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
    udid: &str,
    view: &str,
) -> std::io::Result<Response> {
    exchange_on(socket, req, timeout, Some(udid), Some(view)).await
}

async fn exchange_on(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
    expect_udid: Option<&str>,
    expect_view: Option<&str>,
) -> std::io::Result<Response> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket).await?;
        let (r, mut w) = stream.into_split();
        let mut v = serde_json::to_value(req)?;
        if let Some(o) = v.as_object_mut() {
            o.insert(CANCEL_ON_CLOSE.into(), true.into());
            o.insert(LANG.into(), ictl_l10n::lang().code().into());
            if let Some(view) = expect_view {
                o.insert(EXPECT_VIEW.into(), view.into());
            }
            if let Some(u) = expect_udid {
                o.insert(EXPECT_UDID.into(), u.into());
            }
            add_task_keys(o);
        }
        let mut line = v.to_string();
        line.push('\n');
        w.write_all(line.as_bytes()).await?;
        let mut resp = String::new();
        BufReader::new(r).read_line(&mut resp).await?;
        serde_json::from_str(&resp).map_err(std::io::Error::other)
    };
    match timeout {
        None => exchange.await,
        Some(t) => tokio::time::timeout(t, exchange).await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                ictl_l10n::trf!("守护进程 {} 秒内没有应答（可能正忙，例如正在启动 WDA 或重签）", "The daemon didn't answer within {} seconds (it may be busy, for example starting WDA or re-signing)", t.as_secs()),
            )
        })?,
    }
}

fn add_task_keys(o: &mut serde_json::Map<String, serde_json::Value>) {
    let (token, step) = current_task();
    if let Some(t) = token {
        o.insert(TASK.into(), t.into());
    }
    if let Some(s) = step {
        o.insert(TASK_STEP.into(), s.into());
    }
}

/// What [`open_task`] got back
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOpened {
    pub id: String,
    pub token: String,
}

/// A task's events as the daemon streams them ([`Request::TaskOpen`], [`Request::TaskEvents`]). For an opened task
/// this is also its lease: dropping it ends the task as abandoned unless it ended before
pub struct TaskStream {
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    _w: tokio::net::unix::OwnedWriteHalf,
}

impl TaskStream {
    /// The next event; `None` when the task ended (after its `done`) or the daemon went away
    pub async fn next(&mut self) -> Option<TaskEvent> {
        loop {
            let line = self.lines.next_line().await.ok()??;
            if let Ok(e) = serde_json::from_str(&line) {
                return Some(e);
            }
        }
    }
}

async fn open_stream(socket: &std::path::Path, req: &Request) -> std::io::Result<(Response, TaskStream)> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::UnixStream::connect(socket).await?;
    let (r, mut w) = stream.into_split();
    let mut v = serde_json::to_value(req)?;
    if let Some(o) = v.as_object_mut() {
        o.insert(LANG.into(), ictl_l10n::lang().code().into());
        add_task_keys(o);
    }
    let mut line = v.to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut lines = BufReader::new(r).lines();
    let first = lines.next_line().await?.ok_or_else(|| std::io::Error::other("the daemon closed the connection"))?;
    let resp = serde_json::from_str(&first).map_err(std::io::Error::other)?;
    Ok((resp, TaskStream { lines, _w: w }))
}

/// Opens a task bound to the device the daemon controls now ([`Request::TaskOpen`]). `Err` is a message
pub async fn open_task(socket: &std::path::Path, title: &str, owner: &str) -> Result<(TaskOpened, TaskStream), String> {
    let req = Request::TaskOpen { title: title.to_owned(), owner: owner.to_owned() };
    match open_stream(socket, &req).await.map_err(|e| e.to_string())? {
        (Response::Text { text, .. }, s) => Ok((serde_json::from_str(&text).map_err(|e| e.to_string())?, s)),
        (Response::Error { message, .. }, _) => Err(message),
        (Response::Image { caption, .. }, _) => Err(caption),
    }
}

/// Follows a task's events ([`Request::TaskEvents`]). `Err` is a message
pub async fn task_events(socket: &std::path::Path, id: &str) -> Result<TaskStream, String> {
    match open_stream(socket, &Request::TaskEvents { id: id.to_owned() }).await.map_err(|e| e.to_string())? {
        (Response::Text { .. }, s) => Ok(s),
        (Response::Error { message, .. }, _) => Err(message),
        (Response::Image { caption, .. }, _) => Err(caption),
    }
}

/// Recent tasks ([`Request::TaskList`])
pub async fn task_list(socket: &std::path::Path) -> Result<Vec<TaskInfo>, String> {
    match call(socket, &Request::TaskList, Some(std::time::Duration::from_secs(10))).await.map_err(|e| e.to_string())? {
        Response::Text { text, .. } => serde_json::from_str(&text).map_err(|e| e.to_string()),
        Response::Error { message, .. } => Err(message),
        Response::Image { caption, .. } => Err(caption),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let r = Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.3 };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.starts_with(r#"{"op":"swipe""#), "{s}");
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        let e: Response = serde_json::from_str(r#"{"kind":"error","message":"x"}"#).unwrap();
        assert_eq!(e, Response::error("x"));
    }

    /// Outcome travels as fields; flags that are off and a missing outcome (older daemons) stay off the wire,
    /// and older clients that don't know the field ignore it
    #[test]
    fn outcome_is_optional_on_the_wire() {
        let s = serde_json::to_string(&Response::text("x")).unwrap();
        assert_eq!(s, r#"{"kind":"text","text":"x"}"#);
        let s = serde_json::to_string(&Response::text_with("x", Outcome::default())).unwrap();
        assert_eq!(s, r#"{"kind":"text","text":"x","outcome":{}}"#);
        let missed = Response::text_with("Not found", Outcome { tapped_nothing: true, ..Default::default() });
        let s = serde_json::to_string(&missed).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), missed);

        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum OldResponse {
            Text { text: String },
        }
        let OldResponse::Text { text } = serde_json::from_str(&s).unwrap();
        assert_eq!(text, "Not found");
    }

    /// The field decides; without it (older daemon) the Chinese text is read
    #[test]
    fn outcome_wins_over_text() {
        let missed = Outcome { tapped_nothing: true, ..Default::default() };
        assert!(tapped_nothing("Not found: no element contains \"x\"", Some(missed)));
        assert!(!tapped_nothing("没找到包含 \"x\" 的元素", Some(Outcome::default())));
        assert!(tapped_nothing("没找到包含 \"x\" 的元素", None));
        assert!(tapped_nothing("被挡住:Button \"x\" 整个被「浮层」盖住,没有点", None));
        assert!(!tapped_nothing("Not found: no element contains \"x\"", None));
        let alert = Outcome { alert_reported: true, ..Default::default() };
        assert!(alert_reported("Launched x, but a system alert is on screen: “Allow?”", Some(alert)));
        assert!(alert_reported("已启动 x,但屏幕上有系统弹窗「允许?」挡着", None));
        assert!(!alert_reported("Launched x", Some(Outcome::default())));
    }

    /// 旧版本守护进程直接反序列化请求：多出来的 cancel_on_close、lang 键不能让它报错（含没有字段的请求）
    #[test]
    fn extra_cancel_key_is_ignored_when_parsing() {
        for s in [
            r#"{"op":"status","cancel_on_close":true}"#,
            r#"{"op":"tap","x":1.0,"y":2.0,"cancel_on_close":true}"#,
            r#"{"op":"status","cancel_on_close":true,"lang":"en"}"#,
        ] {
            serde_json::from_str::<Request>(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        }
    }
}
