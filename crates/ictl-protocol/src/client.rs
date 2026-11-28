//! Unix-socket client, request key constants, task scoping, and streaming helpers.

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use crate::Data;
use crate::{ActionFailure, Delivery, FailureStage, Request, Response, TaskEvent, TaskInfo, env};

/// Next to the control socket while the daemon is stopping, and left there when it has stopped (the next one removes
/// it when it is up): a client that can't connect just then waits for the daemon to be back ([`connect`])
pub fn stopping_marker(socket: &std::path::Path) -> std::path::PathBuf {
    socket.with_extension("stopping")
}

/// A daemon that stopped this recently is taken to be restarting
const RESTART_RECENT: std::time::Duration = std::time::Duration::from_secs(20);
/// How long a client waits for a restarting daemon
const RESTART_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Connects to the daemon. While it restarts (`systemctl restart`, an update) the socket is briefly gone or refuses:
/// then this waits for it, up to `RESTART_WAIT`. Nothing was sent yet, so trying again does nothing twice. A daemon
/// that isn't running (stopped a while ago, or never started) is reported at once
pub async fn connect(socket: &std::path::Path) -> std::io::Result<tokio::net::UnixStream> {
    let started = std::time::Instant::now();
    loop {
        match tokio::net::UnixStream::connect(socket).await {
            Err(e)
                if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
                    && started.elapsed() < RESTART_WAIT
                    && restarting(socket) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            r => return r,
        }
    }
}

fn restarting(socket: &std::path::Path) -> bool {
    std::fs::metadata(stopping_marker(socket))
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|e| e < RESTART_RECENT))
}

/// Opens a [`Request::MjpegStream`] for `expect_udid` (the device the page shows): `Ok(Ok(stream))` reads WDA's MJPEG
/// answer from its first byte (buffered past the daemon's line); closing it ends the stream. `Ok(Err(why))` when the
/// daemon can't, `Err` when it can't be reached
pub async fn open_mjpeg_stream(
    socket: &std::path::Path,
    expect_udid: Option<&str>,
) -> std::io::Result<Result<tokio::io::BufReader<tokio::net::UnixStream>, String>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stream = BufReader::new(connect(socket).await?);
    let mut v = serde_json::to_value(Request::MjpegStream)?;
    if let Some(o) = v.as_object_mut() {
        o.insert(LANG.into(), ictl_l10n::lang().code().into());
        if let Some(u) = expect_udid {
            o.insert(EXPECT_UDID.into(), u.into());
        }
    }
    let mut line = v.to_string();
    line.push('\n');
    stream.get_mut().write_all(line.as_bytes()).await?;
    let mut first = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(30), stream.read_line(&mut first))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "MJPEG stream: no answer"))??;
    Ok(match serde_json::from_str::<Response>(&first).map_err(std::io::Error::other)? {
        Response::Error { message, .. } => Err(message),
        _ => Ok(stream),
    })
}

/// Opens a [`Request::WdaForward`] to WDA's port on `udid`, which the daemon must be controlling: `Ok(Ok(..))` with
/// the connection (bytes both ways from here on; buffered, as the answer line was read off it), `Ok(Err(why))` when the
/// daemon won't, `Err` when it can't be reached
pub async fn open_wda_forward(
    socket: &std::path::Path,
    udid: &str,
) -> std::io::Result<Result<tokio::io::BufReader<tokio::net::UnixStream>, String>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stream = BufReader::new(connect(socket).await?);
    let mut v = serde_json::to_value(Request::WdaForward { udid: udid.to_owned() })?;
    if let Some(o) = v.as_object_mut() {
        o.insert(LANG.into(), ictl_l10n::lang().code().into());
    }
    let mut line = v.to_string();
    line.push('\n');
    stream.get_mut().write_all(line.as_bytes()).await?;
    let mut first = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(30), stream.read_line(&mut first))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "WDA forward: no answer"))??;
    Ok(match serde_json::from_str::<Response>(&first).map_err(std::io::Error::other)? {
        Response::Error { message, .. } => Err(message),
        _ => Ok(stream),
    })
}

/// Opens a [`Request::ScreenStream`]: `Ok(Ok(..))` with the reading half (buffered: it may already hold the first
/// messages) and the writing half, `Ok(Err(why))` when the daemon can't stream, `Err` when it can't be reached
pub async fn open_screen_stream(
    socket: &std::path::Path,
    expect_udid: Option<&str>,
    low_bandwidth: bool,
) -> std::io::Result<
    Result<(tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>, tokio::net::unix::OwnedWriteHalf), String>,
> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = connect(socket).await?;
    let (r, mut w) = stream.into_split();
    let mut v = serde_json::to_value(Request::ScreenStream)?;
    if let Some(o) = v.as_object_mut() {
        o.insert(LANG.into(), ictl_l10n::lang().code().into());
        if let Some(u) = expect_udid {
            o.insert(EXPECT_UDID.into(), u.into());
        }
        if low_bandwidth {
            o.insert(LOW_BANDWIDTH.into(), true.into());
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
pub const TASK_TOKEN_ENV: &str = env::TASK_TOKEN;
/// The HTTP header that does the same for MCP over HTTP (the viewer's `/mcp`), per request
pub const TASK_TOKEN_HEADER: &str = "iphone-control-task-token";

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

/// Whether the requests sent from here belong to a task ([`as_task`] or [`join_task`])
pub fn in_task() -> bool {
    current_task().0.is_some()
}

fn current_task() -> (Option<String>, Option<u64>) {
    let (token, step) = CALL_TASK.try_with(Clone::clone).unwrap_or_default();
    (token.or_else(|| process_task().map(str::to_owned)), step)
}

/// Request key carrying the client's language (`zh` / `en`). The daemon answers in it; requests without it
/// (older clients) get Chinese, as before. Older daemons ignore the extra key
pub const LANG: &str = "lang";
/// Request key: a poll the caller repeats anyway (the control page's status and alert checks). The daemon skips it
/// when an action waits for the phone, and drops it when one comes while it runs, answering [`Data::Busy`]: such a
/// check took the phone for up to a second (WDA's alert lookup on the home screen) while a tap waited behind it
pub const BACKGROUND: &str = "background";
/// Request key: someone watches the screen (the control page), so the daemon doesn't look before acting for what they
/// see anyway. Pressing Home skips the alert lookup that comes first (0.8 s of a 1.3 s press on the home screen) and
/// looks only when the press didn't get to the home screen; a swipe through WDA doesn't wait for the list to stop (up
/// to 2.5 s of reading the element tree again and again, 3.1 s for a swipe on an iPad mini 5)
pub const WATCHED: &str = "watched";
/// Request key of a [`Request::ScreenStream`]: the page is in low-bandwidth mode, so the phone's video is capped at a
/// lower bitrate while it watches (for every page watching that phone: there is one stream). Older daemons ignore it
pub const LOW_BANDWIDTH: &str = "low_bandwidth";

/// 发一个请求、读一个响应（客户端用）。`timeout` 为 None 时一直等（例如 `sign` 要等签名装完）。
/// 返回之前被丢掉（超时、调用方取消）时连接随之关闭，守护进程会放弃还在排队的这个请求
pub async fn call(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
) -> std::io::Result<Response> {
    call_expecting(socket, req, timeout, None).await
}

/// A [`BACKGROUND`] request: answered with [`Data::Busy`] instead of keeping an action waiting
pub async fn call_background(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
) -> std::io::Result<Response> {
    exchange_on(socket, req, timeout, None, None, Some(BACKGROUND)).await.map_err(|e| e.source)
}

/// 同 [`call`]；`expect_udid` 见 [`EXPECT_UDID`]
pub async fn call_expecting(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
    expect_udid: Option<&str>,
) -> std::io::Result<Response> {
    exchange_on(socket, req, timeout, expect_udid, None, None).await.map_err(|e| e.source)
}

/// [`call_expecting`] for someone watching the screen ([`WATCHED`])
pub async fn call_watched(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
    expect_udid: Option<&str>,
) -> std::io::Result<Response> {
    exchange_on(socket, req, timeout, expect_udid, None, Some(WATCHED)).await.map_err(|e| e.source)
}

/// Send a coordinate action only if this device and its measured geometry still match. Only the control page sends
/// these, so they say someone watches the screen ([`WATCHED`])
pub async fn call_in_view(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
    udid: &str,
    view: &str,
) -> std::io::Result<Response> {
    exchange_on(socket, req, timeout, Some(udid), Some(view), Some(WATCHED)).await.map_err(|e| e.source)
}

/// Transport failure with the phase of this single exchange. Once writing begins, a missing
/// reply cannot tell the caller whether the daemon acted.
#[derive(Debug)]
pub struct ExchangeError {
    pub source: std::io::Error,
    pub failure: ActionFailure,
}

/// Like the convenience call functions above, retaining transport phase for action results.
pub async fn exchange_on(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
    expect_udid: Option<&str>,
    expect_view: Option<&str>,
    flag: Option<&str>,
) -> Result<Response, ExchangeError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stage = FailureStage::Connect;
    let exchange = async {
        let stream = connect(socket).await?;
        let (r, mut w) = stream.into_split();
        stage = FailureStage::Preflight;
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
            // [`BACKGROUND`] or [`WATCHED`]
            if let Some(f) = flag {
                o.insert(f.into(), true.into());
            }
            add_task_keys(o);
        }
        let mut line = v.to_string();
        line.push('\n');
        stage = FailureStage::Send;
        w.write_all(line.as_bytes()).await?;
        stage = FailureStage::Reply;
        let mut resp = String::new();
        BufReader::new(r).read_line(&mut resp).await?;
        serde_json::from_str(&resp).map_err(std::io::Error::other)
    };
    let result = match timeout {
        None => exchange.await,
        Some(t) => tokio::time::timeout(t, exchange).await.unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                ictl_l10n::trf!("守护进程 {} 秒内没有应答（可能正忙，例如正在启动 WDA 或重签）", "The daemon didn't answer within {} seconds (it may be busy, for example starting WDA or re-signing)", t.as_secs()),
            ))
        }),
    };
    result.map_err(|source| {
        let delivery = match stage {
            FailureStage::Connect | FailureStage::Preflight => Delivery::NotSent,
            _ => Delivery::Unknown,
        };
        ExchangeError { source, failure: ActionFailure { delivery, stage } }
    })
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
    let stream = connect(socket).await?;
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
/// `apps`: the task's scope (see [`Request::TaskOpen`]), empty for none
pub async fn open_task(
    socket: &std::path::Path,
    title: &str,
    owner: &str,
    apps: &[String],
) -> Result<(TaskOpened, TaskStream), String> {
    let req = Request::TaskOpen { title: title.to_owned(), owner: owner.to_owned(), apps: apps.to_vec() };
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

/// The id of the active task with `token` ([`Request::TaskCheck`]); `Err` is why not (ended, unknown, no daemon)
pub async fn task_check(socket: &std::path::Path, token: &str) -> Result<String, String> {
    let call = call(socket, &Request::TaskCheck, Some(std::time::Duration::from_secs(10)));
    match as_task(Some(token.to_owned()), None, call).await.map_err(|e| e.to_string())? {
        Response::Text { text, .. } => Ok(text),
        Response::Error { message, .. } => Err(message),
        Response::Image { caption, .. } => Err(caption),
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
