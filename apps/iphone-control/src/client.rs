//! Talking to the daemon from this program: the CLI's commands, the setup guide, the viewer and the updater ask
//! through these, not through the CLI's entry point

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use ictl_daemon::config::env as env_nonempty;
use ictl_daemon::protocol::{self, Request, Response};

pub async fn ask(socket: &Path, req: Request) -> anyhow::Result<Response> {
    // 签名要等装完，不设时限；其余命令守护进程忙也最多等 3 分钟
    let timeout = match req {
        Request::Resign { .. } => None,
        _ => Some(Duration::from_secs(180)),
    };
    protocol::call(socket, &req, timeout).await.with_context(|| {
        ictl_l10n::trf!("连不上守护进程（{}）：先启动它（iphone-control service start；没装后台服务的，运行 iphone-control 按引导装，或在另一个终端运行 iphone-control daemon）；\
             守护进程以别的用户运行时，用 --socket 或 ICTL_SOCKET 指到它的控制接口", "Can't reach the daemon ({}): start it first (iphone-control service start; without a background service, run iphone-control and let the guide set one up, or run iphone-control daemon in another terminal); if the daemon runs as another user, point --socket or ICTL_SOCKET at its control socket",
            socket.display()
        )
    })
}

pub async fn print_text(socket: &Path, req: Request) -> anyhow::Result<()> {
    match ask(socket, req).await? {
        Response::Text { text, .. } => {
            println!("{text}");
            Ok(())
        }
        Response::Error { message, .. } => bail!("{message}"),
        Response::Image { caption, .. } => {
            println!("{caption}");
            Ok(())
        }
    }
}

pub async fn snapshot(socket: &Path) -> anyhow::Result<ictl_daemon::Snapshot> {
    match ask(socket, Request::Daemon).await? {
        Response::Text { text, .. } => Ok(serde_json::from_str(&text)?),
        Response::Error { message, .. } => bail!("{message}"),
        Response::Image { .. } => {
            bail!("{}", ictl_l10n::tr!("守护进程返回了意外的响应", "The daemon sent an unexpected response"))
        }
    }
}

/// 账号目录和 anisette 地址：与守护进程同一份配置（路径、环境变量的读法都在 Config 里）
pub fn account_dir(data_dir: &Path) -> ictl_sign::AccountDir {
    let cfg = ictl_daemon::Config::from_env(data_dir);
    ictl_sign::AccountDir { dir: cfg.account_dir(), anisette_url: cfg.anisette_url }
}

/// 命令要针对哪台设备：`IPHONE_UDID` > 守护进程正在控制的（或选定的）> USB 上唯一的一台。
/// 给了 `query`（UDID 或设备名）就在接着的设备里找
pub async fn target_device(socket: &Path, query: Option<&str>) -> anyhow::Result<ictl_device::DeviceInfo> {
    let snap = snapshot(socket).await.ok();
    let mut infos = snap.as_ref().map(|s| s.devices.clone()).unwrap_or_default();
    if infos.is_empty() {
        for d in ictl_device::list_devices().await.unwrap_or_default() {
            let i = ictl_device::Device::from_usb(&d).info().await.unwrap_or_default();
            infos.push(ictl_device::DeviceInfo { udid: d.udid, ..i });
        }
    }
    let pick = |u: &str| {
        infos
            .iter()
            .find(|d| d.udid.eq_ignore_ascii_case(u))
            .cloned()
            .unwrap_or_else(|| ictl_device::DeviceInfo { udid: u.to_owned(), ..Default::default() })
    };
    if let Some(q) = query {
        let i = ictl_daemon::devices::resolve_device(&infos, q).map_err(anyhow::Error::msg)?;
        return Ok(infos[i].clone());
    }
    if let Some(u) = env_nonempty(ictl_daemon::protocol::env::UDID) {
        return Ok(pick(&u));
    }
    if let Some(s) = &snap {
        if let Some(d) = &s.device {
            return Ok(d.clone());
        }
        if let Some(p) = &s.pinned {
            return Ok(pick(p));
        }
    }
    match &infos[..] {
        [one] => Ok(one.clone()),
        [] => bail!(
            "{}",
            ictl_l10n::tr!(
                "没接设备：用数据线接好并解锁",
                "No device connected: plug it in with a data cable and unlock it"
            )
        ),
        _ => bail!(
            "{}",
            ictl_l10n::trf!(
                "接着 {} 台设备，不知道是哪台：先用 iphone-control select 选一台，或加 --device 指定",
                "{} devices connected, not sure which one: choose one with iphone-control select, or pass --device",
                infos.len()
            )
        ),
    }
}
