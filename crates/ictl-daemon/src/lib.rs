//! 守护进程：唯一持有设备的进程。替代 toolkit 的 supervisor.sh + 重签定时器 + notify.sh。
//!
//! - 状态机（[`machine::DeviceState`]，docs/architecture.md §3）每 10 秒推进一次
//! - usbmuxd 作为子进程看护；容器无 udev，靠 sysfs 发现漏掉的设备并重启它
//! - 控制接口：Unix socket，每行一个 JSON（[`protocol`]），MCP 和 CLI 都通过它访问设备

pub mod config;
pub mod core;
pub mod eventlog;
pub mod imaging;
pub mod machine;
pub mod notify;
pub mod protocol;

use tokio::sync::oneshot;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub use config::Config;
pub use core::{Daemon, Snapshot};

/// 运行守护进程直到收到 SIGTERM / SIGINT
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    cfg.prepare_dirs()?;
    let socket = cfg.socket.clone();
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }

    let daemon = Daemon::new(cfg);
    daemon.log.log("守护进程启动");

    // 连接任务只负责收发，请求经通道交给主循环串行处理：设备只能有一个持有者，
    // 而且处理设备的 future 不必是 Send（plume_core / 泛型 async 的 Send 推断有限制）
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(protocol::Request, oneshot::Sender<protocol::Response>)>(16);
    let server = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(serve_conn(tx.clone(), stream));
                }
                Err(e) => tracing::warn!("控制接口 accept 失败：{e}"),
            }
        }
    });

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut tick = tokio::time::interval(core::TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            Some((req, reply)) = rx.recv() => {
                let _ = reply.send(daemon.handle(req).await);
            }
            _ = tick.tick() => daemon.tick().await,
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    server.abort();
    daemon.shutdown().await;
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

async fn serve_conn(tx: tokio::sync::mpsc::Sender<(protocol::Request, oneshot::Sender<protocol::Response>)>, stream: tokio::net::UnixStream) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let resp = match serde_json::from_str::<protocol::Request>(&line) {
            Ok(req) => {
                let (otx, orx) = oneshot::channel();
                if tx.send((req, otx)).await.is_err() {
                    break;
                }
                orx.await.unwrap_or_else(|_| protocol::Response::error("守护进程正在退出"))
            }
            Err(e) => protocol::Response::error(format!("请求格式错误：{e}")),
        };
        let mut out = serde_json::to_string(&resp).unwrap_or_default();
        out.push('\n');
        if w.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }
}
