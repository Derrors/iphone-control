//! 守护进程：唯一持有设备的进程。替代 toolkit 的 supervisor.sh + 重签定时器 + notify.sh。
//!
//! - 状态机（[`machine::DeviceState`]，docs/architecture.md §3）每 10 秒推进一次（[`supervise`]）
//! - usbmuxd 用系统的（macOS 自带，Linux 是 usbmuxd 包，插上手机时由 udev 拉起）
//! - 控制接口：Unix socket，每行一个 JSON（[`protocol`]），MCP 和 CLI 都通过它访问设备
//! - 并发模型见 [`core`]
//! - AI 任务（谁能在任务进行中操作手机）见 [`tasks`]

pub mod config;
pub mod core;
pub mod devices;
mod devtools;
mod doctor;
pub mod error;
pub mod eventlog;
pub mod imaging;
pub mod machine;
pub mod maintenance;
pub mod notify;
pub mod procs;
mod screen;
mod supervise;
pub mod tasks;
mod tools;
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
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 已有守护进程在用这个 socket 就退出：两个守护进程会争 usbmuxd、WDA 和 HID
    if tokio::net::UnixStream::connect(&socket).await.is_ok() {
        bail!(
            "{}",
            ictl_l10n::trf!("已经有守护进程在运行（{}）", "A daemon is already running ({})", socket.display())
        );
    }
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    // up again: clients that waited for the restart connect now ([`protocol::connect`])
    let _ = std::fs::remove_file(protocol::stopping_marker(&socket));
    // 处理设备的 future 不必是 Send（plume_core / 泛型 async 的 Send 推断有限制），全部跑在一个 LocalSet 里
    let local = tokio::task::LocalSet::new();
    let r = local.run_until(serve(cfg, listener)).await;
    let _ = std::fs::remove_file(&socket);
    r
}

async fn serve(cfg: Config, listener: tokio::net::UnixListener) -> anyhow::Result<()> {
    let daemon = Daemon::new(cfg);
    daemon.log.log(ictl_l10n::log_tr!("守护进程启动", "Daemon started"));
    watchdog::spawn(daemon.heartbeat.clone(), daemon.log.clone());

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut tick = tokio::time::interval(core::TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 每个连接一个任务；请求在任务里等设备锁，主循环本身从不阻塞，停止信号随时能处理
    let mut conns = JoinSet::new();
    let mut ticking: Option<JoinHandle<()>> = None;
    // a round asked for (`Daemon::wake`) while one was still running: it starts once that one is done
    let mut woken = false;
    loop {
        if woken && ticking.as_ref().is_none_or(JoinHandle::is_finished) {
            woken = false;
            ticking = Some(tokio::task::spawn_local(daemon.clone().tick()));
        }
        tokio::select! {
            biased;
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            r = listener.accept() => match r {
                Ok((stream, _)) => {
                    conns.spawn_local(serve_conn(daemon.clone(), stream));
                }
                Err(e) => {
                    tracing::warn!("{}", ictl_l10n::log_trf!("控制接口 accept 失败：{e}", "Accepting on the control socket failed: {e}"));
                    // 比如文件描述符用完：立刻重试只会空转
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            _ = tick.tick() => {
                // 上一轮还没跑完（例如在重启 usbmuxd）就不叠加
                if ticking.as_ref().is_none_or(JoinHandle::is_finished) {
                    ticking = Some(tokio::task::spawn_local(daemon.clone().tick()));
                }
            }
            () = daemon.wake.notified() => woken = true,
            // waiting for the round that runs to end, before the one asked for
            () = tokio::time::sleep(std::time::Duration::from_millis(200)), if woken => {}
        }
    }
    // stopping: new clients are refused at once (instead of waiting in the queue for a reply that never comes), and
    // told by the marker that this is a restart, so they try again shortly ([`protocol::connect`])
    let _ = std::fs::write(protocol::stopping_marker(&daemon.cfg.socket), b"");
    drop(listener);
    let _ = std::fs::remove_file(&daemon.cfg.socket);
    // 先取消排队和执行中的请求、巡检（放掉设备锁），再停 HID 媒体流、runner、usbmuxd。
    // 自动解锁被打断时停用标记会留着（ictl-unlock），不会因此输错第二次
    conns.abort_all();
    while conns.join_next().await.is_some() {}
    if let Some(t) = ticking {
        t.abort();
        let _ = t.await;
    }
    daemon.shutdown().await;
    Ok(())
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
                    ictl_l10n::scope(lang, daemon.screen_stream(reader, w, ctx.expect_udid)).await;
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
    let (cancel, udid, view, lang, task, task_step) = match v.as_object_mut() {
        Some(o) => (
            o.remove(protocol::CANCEL_ON_CLOSE).is_some_and(|c| c == true),
            o.remove(protocol::EXPECT_UDID).and_then(|u| u.as_str().map(str::to_owned)),
            o.remove(protocol::EXPECT_VIEW).and_then(|v| v.as_str().map(str::to_owned)),
            o.remove(protocol::LANG).and_then(|l| l.as_str().and_then(ictl_l10n::parse)),
            o.remove(protocol::TASK).and_then(|t| t.as_str().map(str::to_owned)),
            o.remove(protocol::TASK_STEP).and_then(|s| s.as_u64()),
        ),
        None => (false, None, None, None, None, None),
    };
    Ok((
        serde_json::from_value(v)?,
        cancel,
        core::CallCtx { gone: None, expect_udid: udid, expect_view: view, task, task_step, peer: None },
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
    let (log, mut rx, ended) = task.subscribe();
    let mut ok = send(&mut w, &protocol::Response::text(first)).await;
    for e in &log {
        ok = ok && send(&mut w, e).await;
    }
    if ok && !ended {
        loop {
            tokio::select! {
                e = rx.recv() => match e {
                    Ok(e) => {
                        let done = e.event == "done";
                        if !send(&mut w, &e).await {
                            break;
                        }
                        if done {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
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

                let tap = Request::Tap { x: 1.0, y: 2.0 };
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

    #[test]
    fn request_options_are_taken_out_before_parsing() {
        use ictl_l10n::Lang;
        let (req, cancel, expect, lang) = parse_request(
            br#"{"op":"tap","x":1.0,"y":2.0,"cancel_on_close":true,"expect_udid":"U1","expect_view":"V1","lang":"en"}"#,
        )
        .unwrap();
        assert_eq!(req, protocol::Request::Tap { x: 1.0, y: 2.0 });
        assert!(cancel);
        assert_eq!(expect.expect_udid.as_deref(), Some("U1"));
        assert_eq!(expect.expect_view.as_deref(), Some("V1"));
        assert_eq!(lang, Lang::En);
        // 旧版本客户端：没有这几个键；没说语言就是中文（它们按中文的固定写法认结果）
        let (_, cancel, expect, lang) = parse_request(br#"{"op":"status"}"#).unwrap();
        assert!(!cancel && expect.expect_udid.is_none() && expect.task.is_none() && expect.task_step.is_none());
        let (_, _, ctx, _) = parse_request(br#"{"op":"tap","x":1.0,"y":2.0,"task":"T","task_step":3}"#).unwrap();
        assert_eq!((ctx.task.as_deref(), ctx.task_step), (Some("T"), Some(3)));
        assert_eq!(lang, Lang::Zh);
        let (_, _, _, lang) = parse_request(br#"{"op":"status","lang":"klingon"}"#).unwrap();
        assert_eq!(lang, Lang::Zh);
        assert!(parse_request(b"{").is_err());
    }
}
