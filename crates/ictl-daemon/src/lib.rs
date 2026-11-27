//! 守护进程：唯一持有设备的进程。替代 toolkit 的 supervisor.sh + 重签定时器 + notify.sh。
//!
//! - 状态机（[`machine::DeviceState`]，docs/architecture.md §3）每 10 秒巡检一次（`supervise`），并经 `usbmuxd` `Listen` 事件流在 USB 插拔时立即唤醒（`watch`）
//! - usbmuxd 用系统的（macOS 自带，Linux 是 usbmuxd 包，插上手机时由 udev 拉起）
//! - 控制接口：Unix socket，每行一个 JSON（[`protocol`]），MCP 和 CLI 都通过它访问设备
//! - 并发模型见 [`core`]
//! - AI 任务（谁能在任务进行中操作手机）见 [`tasks`]

pub mod config;
pub mod core;
pub mod detached;
pub mod devices;
mod devtools;
mod doctor;
pub mod error;
pub mod eventlog;
mod hidmark;
pub mod imaging;
pub mod machine;
pub mod maintenance;
mod mjpeg;
pub mod notify;
pub mod procs;
mod screen;
mod supervise;
pub mod tasks;
mod tools;
mod watch;
mod watchdog;
mod wda_pkg;
mod web;

pub use ictl_protocol as protocol;

use std::sync::Arc;
use std::time::Instant;

use anyhow::bail;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::task::{JoinHandle, JoinSet};

pub use config::Config;
pub use core::{Daemon, Snapshot};

/// 一行请求的长度上限（正常不到 1KB；输入的文字也远小于这个）
const MAX_REQUEST_LINE: u64 = 1024 * 1024;

/// 运行守护进程直到收到 SIGTERM / SIGINT
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    cfg.prepare_dirs()?;
    let socket = cfg.socket.clone();
    if let Some(dir) = socket.parent()
        && !dir.exists()
    {
        // made here: this user's alone (a directory that was there, such as /tmp, is left as it is)
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    // one daemon per socket, for as long as it runs: two started at once both found no one answering and bound in
    // turn, the second taking the socket from the first
    let _instance = single_instance(&socket, INSTANCE_WAIT).await?;
    // 已有守护进程在用这个 socket 就退出：两个守护进程会争 usbmuxd、WDA 和 HID
    if tokio::net::UnixStream::connect(&socket).await.is_ok() {
        bail!(
            "{}",
            ictl_l10n::trf!("已经有守护进程在运行（{}）", "A daemon is already running ({})", socket.display())
        );
    }
    let _ = std::fs::remove_file(&socket);
    // created 0600 from the start (the umask for the bind alone): set afterwards, others could connect in between
    let old = unsafe { libc::umask(0o077) };
    let bound = tokio::net::UnixListener::bind(&socket);
    unsafe { libc::umask(old) };
    let listener = bound?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    // up again: clients that waited for the restart connect now ([`protocol::connect`])
    let _ = std::fs::remove_file(protocol::stopping_marker(&socket));
    // 处理设备的 future 不必是 Send（rustc 对泛型 async 的 Send 推断有局限，见 docs/architecture.md），全部跑在一个 LocalSet 里
    let local = tokio::task::LocalSet::new();
    let r = local.run_until(serve(cfg, listener)).await;
    let _ = std::fs::remove_file(&socket);
    r
}

/// How long a starting daemon waits for the one before it to let go of the socket (it may still be stopping)
const INSTANCE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// The lock beside the socket (`ctl.lock`), held while the daemon runs; waits up to `wait` for another daemon that is
/// stopping, and gives up at once when one answers on the socket
async fn single_instance(socket: &std::path::Path, wait: std::time::Duration) -> anyhow::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let path = socket.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let deadline = Instant::now() + wait;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        // held by another daemon (or interrupted): wait for it below. Anything else (a file system without flock)
        // won't change by waiting, and isn't another daemon
        let e = std::io::Error::last_os_error();
        if !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) {
            return Err(anyhow::Error::new(e).context(path.display().to_string()));
        }
        let running =
            || ictl_l10n::trf!("已经有守护进程在运行（{}）", "A daemon is already running ({})", socket.display());
        if tokio::net::UnixStream::connect(socket).await.is_ok() || Instant::now() >= deadline {
            bail!("{}", running());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// A supervision round asked for that couldn't take the device is asked for again this much later
const RETRY_ROUND: std::time::Duration = std::time::Duration::from_millis(200);
/// Clients served at once on the control socket; more wait in its queue
const MAX_CONNECTIONS: usize = 256;

async fn serve(cfg: Config, listener: tokio::net::UnixListener) -> anyhow::Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let daemon = Daemon::new(cfg);
    daemon.log.log(ictl_l10n::log_tr!("守护进程启动", "Daemon started"));
    log_removed_env_notices(&daemon.log, &protocol::env::active_removed());
    let run_state = Arc::new(watchdog::RunState::new(daemon.cfg.run_state_file()));
    if let Some(incident) = run_state.on_startup(daemon.cfg.notify.any()) {
        report_abnormal_restart(&daemon, incident).await;
    }
    watchdog::spawn(daemon.heartbeat.clone(), daemon.log.clone(), run_state.clone());
    let watcher = watch::spawn(daemon.wake.clone());

    let mut tick = tokio::time::interval(core::TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 每个连接一个任务；请求在任务里等设备锁，主循环本身从不阻塞，停止信号随时能处理
    let mut conns = JoinSet::new();
    // the round running, and whether it was asked for (`Daemon::wake`)
    let mut ticking: Option<(JoinHandle<bool>, bool)> = None;
    // a round asked for while one was still running: it starts once that one is done. One asked for that couldn't
    // run (a request held the device) is asked for again shortly (`retry`): a phone made ready in the background
    // waited 10 s for the next round when switching back to it
    let mut woken = false;
    let mut retry: Option<tokio::time::Instant> = None;
    loop {
        if woken && ticking.is_none() {
            woken = false;
            // this round is the one asked for: should it not run either, it asks again when it ends
            retry = None;
            ticking = Some((tokio::task::spawn_local(daemon.clone().tick()), true));
        }
        tokio::select! {
            biased;
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            // the round that runs ended: an asked-for one that couldn't take the device is asked for again
            r = async { (&mut ticking.as_mut().expect("guarded").0).await }, if ticking.is_some() => {
                let asked = ticking.take().is_some_and(|(_, asked)| asked);
                match r.unwrap_or(true) {
                    // a round that ran did what the one waiting to be retried would have
                    true => retry = None,
                    false if asked => retry = Some(tokio::time::Instant::now() + RETRY_ROUND),
                    false => {}
                }
            }
            // the rounds before new clients: a stream of short connections mustn't starve the supervision (and the
            // watchdog's heartbeat it carries)
            _ = tick.tick() => {
                // 上一轮还没跑完（例如在重启 usbmuxd）就不叠加
                if ticking.is_none() {
                    ticking = Some((tokio::task::spawn_local(daemon.clone().tick()), false));
                }
            }
            () = daemon.wake.notified() => woken = true,
            () = tokio::time::sleep_until(retry.unwrap_or_else(tokio::time::Instant::now)), if retry.is_some() => {
                retry = None;
                woken = true;
            }
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            // at most MAX_CONNECTIONS clients at once: more wait in the socket's queue
            r = listener.accept(), if conns.len() < MAX_CONNECTIONS => match r {
                Ok((stream, _)) => {
                    conns.spawn_local(serve_conn(daemon.clone(), stream));
                }
                Err(e) => {
                    tracing::warn!("{}", ictl_l10n::log_trf!("控制接口 accept 失败：{e}", "Accepting on the control socket failed: {e}"));
                    // 比如文件描述符用完：立刻重试只会空转
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
        }
    }
    // stopping: new clients are refused at once (instead of waiting in the queue for a reply that never comes), and
    // told by the marker that this is a restart, so they try again shortly ([`protocol::connect`])
    let _ = std::fs::write(protocol::stopping_marker(&daemon.cfg.socket), b"");
    drop(listener);
    let _ = std::fs::remove_file(&daemon.cfg.socket);
    // 先取消事件监听、排队和执行中的请求、巡检（放掉设备锁），再停 HID 媒体流、runner、usbmuxd。
    // 自动解锁被打断时停用标记会留着（ictl-unlock），不会因此输错第二次
    watcher.abort();
    let _ = watcher.await;
    conns.abort_all();
    while conns.join_next().await.is_some() {}
    if let Some((t, _)) = ticking {
        t.abort();
        let _ = t.await;
    }
    daemon.shutdown().await;
    run_state.mark_clean_shutdown();
    Ok(())
}

async fn report_abnormal_restart(daemon: &Daemon, incident: watchdog::AbnormalRestart) {
    let pid = incident.previous_pid;
    let recent = incident.recent;
    let note = match incident.suppressed_by_cooldown {
        Some(n) => ictl_l10n::log_trf!(
            "；通知在冷却期内已跳过（累计抑制 {n} 次）",
            "; notification suppressed during cooldown ({n} suppressed so far)"
        ),
        None => String::new(),
    };
    let line = match incident.reason {
        notify::RestartReason::Watchdog { silent_secs } => ictl_l10n::log_trf!(
            "检测到上次守护进程（pid {pid}）因卡死 {silent_secs} 秒由看门狗退出并已重启（近 1 小时异常重启 {recent} 次）{note}",
            "Detected previous daemon (pid {pid}) exited by watchdog after {silent_secs} s with no progress and restarted ({recent} abnormal restart(s) in the last hour){note}"
        ),
        notify::RestartReason::Unexpected => ictl_l10n::log_trf!(
            "检测到上次守护进程（pid {pid}）未正常关闭（崩溃、被终止或断电）并已重启（近 1 小时异常重启 {recent} 次）{note}",
            "Detected previous daemon (pid {pid}) exited unexpectedly (crash, killed, or power loss) and restarted ({recent} abnormal restart(s) in the last hour){note}"
        ),
    };
    daemon.log.log(&line);
    if let Some(event) = incident.notify {
        let mut g = daemon.inner.lock().await;
        daemon.notify(&mut g, event);
    }
}

async fn serve_conn(daemon: Arc<Daemon>, stream: tokio::net::UnixStream) {
    // the process on the other end: a task's agent may not control its task (`tasks::Tasks::handle`)
    let peer = stream.peer_cred().ok().and_then(|c| c.pid()).and_then(|p| u32::try_from(p).ok());
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(MAX_REQUEST_LINE + 1).read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let queued_at = Instant::now();
        let resp = if line.len() as u64 > MAX_REQUEST_LINE {
            protocol::Response::error(ictl_l10n::tr!("请求太长", "request too long"))
        } else {
            let parsed = parse_request(&line).map(|(r, cancel, mut ctx, lang)| {
                ctx.peer = peer;
                (r, cancel, ctx, lang)
            });
            // Status snapshots remain available throughout maintenance (including health probes).
            let needs_device = parsed.as_ref().is_ok_and(|(r, ..)| !matches!(r, protocol::Request::Daemon));
            let lease = needs_device.then(maintenance::shared).transpose();
            match (parsed, lease) {
                (_, Err(e)) => protocol::Response::error(e.to_string()),
                // Streams until either side closes: the connection is the stream's from here on. The lease is only
                // checked when it opens (no stream starts during an update) and not held while it runs: a page
                // watching for hours would otherwise keep the updater from ever finding the host idle. An update
                // stops the daemon, which ends the stream; the viewer reconnects to the new one
                (Ok((protocol::Request::ScreenStream, _, ctx, lang)), Ok(lease)) => {
                    drop(lease);
                    ictl_l10n::scope(lang, daemon.screen_stream(reader, w, ctx.expect_udid, ctx.low_bandwidth)).await;
                    return;
                }
                // the same for WDA's MJPEG picture (iOS 17-26)
                (Ok((protocol::Request::MjpegStream, _, ctx, lang)), Ok(lease)) => {
                    drop(lease);
                    ictl_l10n::scope(lang, mjpeg::stream(&daemon, reader, w, ctx.expect_udid)).await;
                    return;
                }
                // also streams; an opened task's connection is its lease, so it isn't held against updates either
                (
                    Ok((req @ (protocol::Request::TaskOpen { .. } | protocol::Request::TaskEvents { .. }), _, _, lang)),
                    Ok(lease),
                ) => {
                    drop(lease);
                    ictl_l10n::scope(lang, task_stream(&daemon, reader, w, req, peer)).await;
                    return;
                }
                (Ok((req, false, ctx, lang)), Ok(_lease)) => {
                    let handling = daemon.handle(req, queued_at, ctx);
                    ictl_l10n::scope(lang, handling).await
                }
                (Ok((req, true, mut ctx, lang)), Ok(_lease)) => {
                    // 调用方断开（取消、超时）就告诉 handle：还在排队的话不再执行
                    let (gone_tx, gone_rx) = tokio::sync::oneshot::channel();
                    let mut gone_tx = Some(gone_tx);
                    ctx.gone = Some(gone_rx);
                    let handling = ictl_l10n::scope(lang, daemon.handle(req, queued_at, ctx));
                    tokio::pin!(handling);
                    let resp = loop {
                        tokio::select! {
                            r = &mut handling => break r,
                            () = wait_closed(&mut reader), if gone_tx.is_some() => {
                                let _ = gone_tx.take().map(|t| t.send(()));
                            }
                        }
                    };
                    if gone_tx.is_none() {
                        break;
                    }
                    resp
                }
                (Err(e), _) => {
                    protocol::Response::error(ictl_l10n::trf!("请求格式错误：{e}", "malformed request: {e}"))
                }
            }
        };
        let mut out = serde_json::to_string(&resp).unwrap_or_default();
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() || line.len() as u64 > MAX_REQUEST_LINE {
            break;
        }
    }
}

/// 解析一行请求，并取出 [`protocol::CANCEL_ON_CLOSE`]、[`protocol::EXPECT_UDID`]、[`protocol::LANG`]。
/// No language (an older client) means Chinese, which is what those clients expect and parse
fn parse_request(line: &[u8]) -> serde_json::Result<(protocol::Request, bool, core::CallCtx, ictl_l10n::Lang)> {
    let mut v: serde_json::Value = serde_json::from_slice(line)?;
    let (cancel, udid, view, lang, task, task_step, background, watched, low_bandwidth) = match v.as_object_mut() {
        Some(o) => (
            o.remove(protocol::CANCEL_ON_CLOSE).is_some_and(|c| c == true),
            o.remove(protocol::EXPECT_UDID).and_then(|u| u.as_str().map(str::to_owned)),
            o.remove(protocol::EXPECT_VIEW).and_then(|v| v.as_str().map(str::to_owned)),
            o.remove(protocol::LANG).and_then(|l| l.as_str().and_then(ictl_l10n::parse)),
            o.remove(protocol::TASK).and_then(|t| t.as_str().map(str::to_owned)),
            o.remove(protocol::TASK_STEP).and_then(|s| s.as_u64()),
            o.remove(protocol::BACKGROUND).is_some_and(|b| b == true),
            o.remove(protocol::WATCHED).is_some_and(|b| b == true),
            o.remove(protocol::LOW_BANDWIDTH).is_some_and(|b| b == true),
        ),
        None => (false, None, None, None, None, None, false, false, false),
    };
    Ok((
        serde_json::from_value(v)?,
        cancel,
        core::CallCtx {
            gone: None,
            expect_udid: udid,
            expect_view: view,
            task,
            task_step,
            peer: None,
            background,
            watched,
            low_bandwidth,
        },
        lang.unwrap_or(ictl_l10n::Lang::Zh),
    ))
}

/// Serves [`protocol::Request::TaskOpen`] (the connection is the task's lease) or [`protocol::Request::TaskEvents`]:
/// one answer line, then the task's events, one JSON line each, until it ends or the client goes away
async fn task_stream(
    daemon: &Arc<Daemon>,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut w: tokio::net::unix::OwnedWriteHalf,
    req: protocol::Request,
    peer: Option<u32>,
) {
    async fn send<T: serde::Serialize>(w: &mut tokio::net::unix::OwnedWriteHalf, v: &T) -> bool {
        let mut line = serde_json::to_string(v).unwrap_or_default();
        line.push('\n');
        w.write_all(line.as_bytes()).await.is_ok()
    }
    let found = match req {
        protocol::Request::TaskOpen { title, owner, apps } => match daemon.resolve_scope(&apps).await {
            Err(m) => Err(m),
            Ok(apps) => daemon.tasks.open(&title, &owner, daemon.controlled_udid().as_deref(), apps, peer).map(|t| {
                daemon.log.log(&ictl_l10n::log_trf!(
                    "开始任务 {}（{owner}）：{title}",
                    "Task {} started ({owner}): {title}",
                    t.id
                ));
                let opened = protocol::TaskOpened { id: t.id.clone(), token: tasks::Tasks::token_of(&t).to_owned() };
                (t, Some(opened))
            }),
        },
        protocol::Request::TaskEvents { id } => daemon
            .tasks
            .get(&id)
            .map(|t| (t, None))
            .ok_or_else(|| ictl_l10n::tr!("没有这个任务", "No such task").into()),
        _ => Err("not a task stream".into()),
    };
    let (task, opened) = match found {
        Ok(f) => f,
        Err(m) => {
            send(&mut w, &protocol::Response::error(m)).await;
            return;
        }
    };
    let lease = opened.is_some();
    let first = match &opened {
        Some(o) => serde_json::to_string(o).unwrap_or_default(),
        None => "streaming".into(),
    };
    let sub = task.subscribe().await;
    let mut ok = send(&mut w, &protocol::Response::text(first)).await;
    for (_, e) in &sub.events {
        ok = ok && send(&mut w, e).await;
    }
    // Highest sequence number already covered by the initial snapshot (or sent live): deduplicates any overlap with
    // `rx` and lets a lagged reader catch up by sequence number even after the task log hit its cap.
    let mut last_seq = sub.snap_seq;
    let mut rx = sub.rx;
    if ok && !sub.ended {
        loop {
            tokio::select! {
                e = rx.recv() => match e {
                    Ok((seq, e)) => {
                        if seq <= last_seq {
                            continue;
                        }
                        last_seq = seq;
                        let done = e.event == "done";
                        if !send(&mut w, &e).await {
                            break;
                        }
                        if done {
                            return;
                        }
                    }
                    // fell behind and missed some, maybe `done` (then nothing more would come, and the connection would
                    // stay open for good): re-read the log asynchronously and replay everything past `last_seq`
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let again = task.subscribe().await;
                        for (seq, e) in &again.events {
                            if *seq <= last_seq {
                                continue;
                            }
                            last_seq = *seq;
                            if !send(&mut w, e).await {
                                ok = false;
                                break;
                            }
                        }
                        last_seq = last_seq.max(again.snap_seq);
                        rx = again.rx;
                        if !ok {
                            break;
                        }
                        if again.ended {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                () = wait_closed(&mut reader) => break,
            }
        }
    }
    // the opener went away before the task ended
    if lease && task.is_active() {
        task.abandon();
        daemon.log.log(&ictl_l10n::log_trf!(
            "任务 {} 的发起方断开，任务中断",
            "The opener of task {} went away; the task was interrupted",
            task.id
        ));
    }
}

/// Log gentle startup notices when removed environment variables are still set to non-empty values.
fn log_removed_env_notices(log: &eventlog::EventLog, removed: &[&protocol::env::RemovedVar]) {
    for v in removed {
        let msg = ictl_l10n::in_own_lang(|| {
            ictl_l10n::trf!(
                "忽略已移除的配置项 {}（{}），可从 ~/.config/iphone-control/env 或环境中移除",
                "Ignoring removed setting {} ({}); you can remove it from ~/.config/iphone-control/env or your environment",
                v.name,
                v.reason()
            )
        });
        log.log(&msg);
    }
}

/// 等到调用方关掉连接（读到 EOF 或出错）。带 cancel_on_close 的客户端一个连接只发一个请求，之后不会再写
async fn wait_closed<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) {
    let mut buf = [0u8; 64];
    while let Ok(n) = reader.read(&mut buf).await
        && n > 0
    {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole way through the socket: a task opened by a client holds the phone until its connection closes
    #[tokio::test]
    async fn an_opened_task_holds_the_phone_until_its_lease_goes() {
        use protocol::{Request, Response};
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (d, dir) = core::tests::test_daemon("lease");
                d.fake_controlled("00008150-000A");
                let socket = dir.join("ctl.sock");
                let listener = tokio::net::UnixListener::bind(&socket).unwrap();
                let daemon = d.clone();
                tokio::task::spawn_local(async move {
                    while let Ok((s, _)) = listener.accept().await {
                        tokio::task::spawn_local(serve_conn(daemon.clone(), s));
                    }
                });
                let (opened, mut events) = protocol::open_task(&socket, "看看设置", "test", &[]).await.unwrap();
                assert_eq!(events.next().await.unwrap().event, "task");
                assert!(protocol::open_task(&socket, "又一个", "test", &[]).await.is_err(), "one at a time");

                let tap = Request::Tap { x: 1.0, y: 2.0, count: 1 };
                let r = protocol::call(&socket, &tap, None).await.unwrap();
                assert!(matches!(&r, Response::Error { message, .. } if message.contains(&opened.id)), "{r:?}");
                // the task's own request gets past the task (and then finds no phone in this test)
                let r = protocol::as_task(Some(opened.token.clone()), None, protocol::call(&socket, &tap, None))
                    .await
                    .unwrap();
                assert!(matches!(&r, Response::Error { message, .. } if !message.contains(&opened.id)), "{r:?}");
                let step = Request::TaskStep { tool: "iphone_tap".into(), args: serde_json::json!({"x": 1, "y": 2}) };
                let r = protocol::as_task(Some(opened.token.clone()), None, protocol::call(&socket, &step, None))
                    .await
                    .unwrap();
                assert_eq!(r, Response::text("2"), "step 1 was the tap above, recorded on its own");

                let mut follower = protocol::task_events(&socket, &opened.id).await.unwrap();
                drop(events);
                let mut last = None;
                while let Some(e) = follower.next().await {
                    last = Some(e);
                }
                let last = last.unwrap();
                assert_eq!((last.event.as_str(), last.data["outcome"].as_str()), ("done", Some("abandoned")));
                let list = protocol::task_list(&socket).await.unwrap();
                assert_eq!(list.last().unwrap().state, "abandoned");
                let r = protocol::call(&socket, &tap, None).await.unwrap();
                assert!(matches!(&r, Response::Error { message, .. } if !message.contains(&opened.id)), "free again");

                // a scope is resolved when the task opens: bundle IDs as they are, names need the phone's app list
                // (the tap above published a snapshot without the pretend device)
                d.fake_controlled("00008150-000A");
                let prefs = vec!["com.apple.Preferences".to_owned()];
                let (scoped, _lease) = protocol::open_task(&socket, "改设置", "test", &prefs).await.unwrap();
                let list = protocol::task_list(&socket).await.unwrap();
                let info = list.iter().find(|t| t.id == scoped.id).unwrap();
                assert_eq!(info.apps, prefs);
                drop(_lease);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let named = vec!["没有这个 App".to_owned()];
                assert!(protocol::open_task(&socket, "改设置", "test", &named).await.is_err());
                std::fs::remove_dir_all(dir).unwrap();
            })
            .await;
    }

    /// A follower that falls behind (its socket full while thousands of events go by) still gets every event and
    /// `done`: before, the missed `done` left it open for good
    #[tokio::test]
    async fn a_follower_that_fell_behind_catches_up_and_ends() {
        use tokio::io::AsyncBufReadExt;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (d, dir) = core::tests::test_daemon("lagged");
                let t = d.tasks.open("x", "test", Some("phone"), Vec::new(), None).unwrap();
                let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
                let (r, w) = ours.into_split();
                let (daemon, id) = (d.clone(), t.id.clone());
                let follow = tokio::task::spawn_local(async move {
                    task_stream(&daemon, BufReader::new(r), w, protocol::Request::TaskEvents { id }, None).await
                });
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                // more than the channel holds, while nobody reads the socket
                for _ in 0..3000 {
                    t.control_as_user("pause").unwrap();
                    t.control_as_user("resume").unwrap();
                }
                t.end(tasks::End::Cancelled, "stopped");
                let mut lines = tokio::io::BufReader::new(theirs).lines();
                let mut got = Vec::new();
                while let Ok(Ok(Some(l))) =
                    tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line()).await
                {
                    got.push(l);
                }
                tokio::time::timeout(std::time::Duration::from_secs(5), follow)
                    .await
                    .expect("the stream ended")
                    .unwrap();
                let sub = t.subscribe().await;
                assert_eq!(got.len(), 1 + sub.events.len(), "the answer line, then every event once");
                assert!(got.last().unwrap().contains("\"done\""), "{}", got.last().unwrap());
                std::fs::remove_dir_all(dir).unwrap();
            })
            .await;
    }

    /// Even when a task's log hits its cap and a slow follower lags past the broadcast buffer after already receiving
    /// live events beyond the persisted prefix, the follower still receives `done` (never skipped by length) and ends.
    #[tokio::test]
    async fn a_capped_lagged_follower_still_receives_done_and_ends() {
        use tokio::io::AsyncBufReadExt;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (d, dir) = core::tests::test_daemon("lagged-capped");
                let t = d.tasks.open("x", "test", Some("phone"), Vec::new(), None).unwrap();
                t.set_log_limits(20, 16 * 1024 * 1024);
                let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
                let (r, w) = ours.into_split();
                let (daemon, id) = (d.clone(), t.id.clone());
                let follow = tokio::task::spawn_local(async move {
                    task_stream(&daemon, BufReader::new(r), w, protocol::Request::TaskEvents { id }, None).await
                });
                let mut lines = tokio::io::BufReader::new(theirs).lines();
                // Read the initial answer + `task` event + enough live events to move `last_seq` well past the cap.
                let mut got = Vec::new();
                got.push(lines.next_line().await.unwrap().unwrap());
                got.push(lines.next_line().await.unwrap().unwrap());
                for _ in 0..15 {
                    t.control_as_user("pause").unwrap();
                    t.control_as_user("resume").unwrap();
                }
                // 15 * 4 = 60 live events (> cap 20): read them so last_seq > 20 before lagging.
                for _ in 0..60 {
                    let l = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                    got.push(l);
                }
                // Now stop reading while emitting > broadcast capacity (4096) events and ending the task.
                for _ in 0..1500 {
                    t.control_as_user("pause").unwrap();
                    t.control_as_user("resume").unwrap();
                }
                t.end(tasks::End::Cancelled, "stopped");
                while let Ok(Ok(Some(l))) =
                    tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line()).await
                {
                    got.push(l);
                }
                tokio::time::timeout(std::time::Duration::from_secs(5), follow)
                    .await
                    .expect("the stream ended")
                    .unwrap();
                let done_count = got.iter().filter(|l| l.contains("\"event\":\"done\"")).count();
                assert_eq!(done_count, 1, "done must arrive exactly once even after cap + lag");
                assert!(got.last().unwrap().contains("\"done\""), "{}", got.last().unwrap());
                // Persisted log stayed capped to 20 ordinary events + terminal `done`.
                let sub = t.subscribe().await;
                assert!(sub.ended);
                assert_eq!(sub.events.len(), 21, "20 events up to cap + mandatory done");
                assert_eq!(sub.events.last().unwrap().1.event, "done");
                // Restart recovery reads the capped file and sees `cancelled`, not `interrupted`.
                let reloaded = tasks::Tasks::load(Some(dir.join("tasks")));
                let info = reloaded.infos().into_iter().find(|i| i.id == t.id).unwrap();
                assert_eq!(info.state, "cancelled");
                std::fs::remove_dir_all(dir).unwrap();
            })
            .await;
    }

    /// One daemon per socket: a second is refused while the first holds the lock, and gets it once that one is gone
    #[tokio::test]
    async fn only_one_daemon_holds_a_socket() {
        let dir = std::env::temp_dir().join(format!("ictl-instance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("ctl.sock");
        let first = single_instance(&socket, std::time::Duration::ZERO).await.unwrap();
        let e = single_instance(&socket, std::time::Duration::from_millis(300)).await.unwrap_err();
        assert!(e.to_string().contains("running") || e.to_string().contains("运行"), "{e}");
        drop(first);
        assert!(single_instance(&socket, std::time::Duration::ZERO).await.is_ok(), "free again");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn request_options_are_taken_out_before_parsing() {
        use ictl_l10n::Lang;
        let (req, cancel, expect, lang) = parse_request(
            br#"{"op":"tap","x":1.0,"y":2.0,"cancel_on_close":true,"expect_udid":"U1","expect_view":"V1","lang":"en"}"#,
        )
        .unwrap();
        assert_eq!(req, protocol::Request::Tap { x: 1.0, y: 2.0, count: 1 });
        assert!(cancel);
        assert_eq!(expect.expect_udid.as_deref(), Some("U1"));
        assert_eq!(expect.expect_view.as_deref(), Some("V1"));
        assert_eq!(lang, Lang::En);
        // 旧版本客户端：没有这几个键；没说语言就是中文（它们按中文的固定写法认结果）
        let (_, cancel, expect, lang) = parse_request(br#"{"op":"status"}"#).unwrap();
        assert!(!cancel && expect.expect_udid.is_none() && expect.task.is_none() && expect.task_step.is_none());
        let (_, _, ctx, _) = parse_request(br#"{"op":"tap","x":1.0,"y":2.0,"task":"T","task_step":3}"#).unwrap();
        assert_eq!((ctx.task.as_deref(), ctx.task_step), (Some("T"), Some(3)));
        assert!(!ctx.watched && !ctx.background);
        let (req, _, ctx, _) = parse_request(br#"{"op":"press_button","button":"home","watched":true}"#).unwrap();
        assert_eq!(req, protocol::Request::PressButton { button: "home".into() });
        assert!(ctx.watched && !ctx.background && !ctx.low_bandwidth);
        let (req, _, ctx, _) = parse_request(br#"{"op":"screen_stream","low_bandwidth":true}"#).unwrap();
        assert_eq!(req, protocol::Request::ScreenStream);
        assert!(ctx.low_bandwidth);
        assert_eq!(lang, Lang::Zh);
        let (_, _, _, lang) = parse_request(br#"{"op":"status","lang":"klingon"}"#).unwrap();
        assert_eq!(lang, Lang::Zh);
        assert!(parse_request(b"{").is_err());
    }

    /// End-to-end abnormal restart reporting: detects unexpected exit and watchdog exit across daemon restarts,
    /// writes every incident to `daemon.log`, dispatches a notification once per cooldown, and suppresses storm repeats.
    #[tokio::test]
    async fn abnormal_restart_reports_to_log_and_throttles_notifications_across_restarts() {
        let dir = std::env::temp_dir().join(format!("ictl-abnormal-restart-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let make_daemon = || {
            let mut cfg = Config::from_env(&dir);
            cfg.notify = config::NotifyConfig::default();
            Daemon::new(cfg)
        };
        let d1 = make_daemon();
        let state_file = d1.cfg.run_state_file();
        let base = 1_000_000u64;

        // First start is clean and writes the live marker.
        let rs1 = watchdog::RunState::new(state_file.clone());
        assert!(rs1.on_startup_at(base, 1001, true).is_none());

        // Daemon 1 vanishes without clean shutdown -> Daemon 2 starts and reports + notifies once.
        let rs2 = watchdog::RunState::new(state_file.clone());
        let inc1 = rs2.on_startup_at(base + 10, 1002, true).expect("unexpected exit detected");
        report_abnormal_restart(&d1, inc1).await;
        assert!(d1.inner.lock().await.dedup.was_sent("daemon"));

        // Daemon 2 also crashes 5 s later -> Daemon 3 logs suppression and does not emit a second notification.
        let d2 = make_daemon();
        let rs3 = watchdog::RunState::new(state_file.clone());
        let inc2 = rs3.on_startup_at(base + 15, 1003, true).expect("storm crash detected");
        assert_eq!(inc2.suppressed_by_cooldown, Some(1));
        assert!(inc2.notify.is_none());
        report_abnormal_restart(&d2, inc2).await;
        assert!(!d2.inner.lock().await.dedup.was_sent("daemon"));

        // Watchdog triggers after cooldown expires -> Daemon 4 logs watchdog cause and dispatches summary alert.
        let after_cd = base + 10 + watchdog::ALERT_COOLDOWN.as_secs();
        assert!(rs3.mark_watchdog_exit_at(1003, 605, after_cd - 2));
        let d3 = make_daemon();
        let rs4 = watchdog::RunState::new(state_file);
        let inc3 = rs4.on_startup_at(after_cd, 1004, true).expect("watchdog exit detected");
        assert_eq!(inc3.reason, notify::RestartReason::Watchdog { silent_secs: 605 });
        assert_eq!(
            inc3.notify,
            Some(notify::Event::DaemonRestarted {
                reason: notify::RestartReason::Watchdog { silent_secs: 605 },
                recent: 3,
                suppressed: 1,
            })
        );
        report_abnormal_restart(&d3, inc3).await;
        assert!(d3.inner.lock().await.dedup.was_sent("daemon"));

        let log_text = std::fs::read_to_string(dir.join("logs/daemon.log")).unwrap();
        assert!(log_text.contains("1001") && log_text.contains("605"), "{log_text}");
        assert!(log_text.contains("抑制 1") || log_text.contains("1 suppressed"), "{log_text}");
        let notify_lines = log_text.lines().filter(|l| l.contains("通知：") || l.contains("Notification:")).count();
        assert_eq!(notify_lines, 2, "only the first crash and the post-cooldown watchdog summary notify:\n{log_text}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn daemon_startup_logs_removed_env_notices_without_failing() {
        let dir = std::env::temp_dir().join(format!("ictl-removed-env-log-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("daemon.log");
        let log = eventlog::EventLog::new(&dir);
        log_removed_env_notices(&log, &[]);
        assert!(!log_path.exists());
        let first = &protocol::env::REMOVED[0];
        log_removed_env_notices(&log, &[first]);
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains(first.name), "{content}");
        assert!(content.contains(first.reason_zh) || content.contains(first.reason_en), "{content}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
