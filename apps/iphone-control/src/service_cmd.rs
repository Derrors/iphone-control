//! `iphone-control service …` subcommand handlers (daemon and viewer background services).

use std::path::Path;

use ictl_daemon::config::env as env_nonempty;

use crate::client::snapshot;
use crate::{ServiceAction, ViewerServiceAction, agent, service, viewer};

/// After `service stop` / `uninstall`: the WDA launched in the background doesn't end with the
/// daemon, so restarts and updates don't interrupt it; stopping the service on purpose ends it too. Not while a
/// daemon still answers (one run by hand): it keeps using its WDA
async fn end_background_wda(socket: &Path, data_dir: &Path) {
    use ictl_daemon::detached::StoppedWda;
    if tokio::net::UnixStream::connect(socket).await.is_ok() {
        return;
    }
    let cfg = ictl_daemon::Config::from_env(data_dir);
    for (udid, s) in ictl_daemon::detached::end_recorded(&cfg).await {
        match s {
            StoppedWda::Ended(pid) => println!(
                "{}",
                ictl_l10n::trf!(
                    "已结束 {udid} 上后台启动的 WDA（进程 {pid}）",
                    "Stopped the WDA launched in the background on {udid} (pid {pid})"
                )
            ),
            StoppedWda::NotRunning(_) => {}
            StoppedWda::Failed(pid, e) => println!(
                "{}",
                ictl_l10n::trf!(
                    "没能结束 {udid} 上后台启动的 WDA（进程 {pid}）：{e}。守护进程下次准备这台手机时会再处理",
                    "Couldn't stop the WDA launched in the background on {udid} (pid {pid}): {e}. The daemon deals with it the next time it prepares this phone"
                )
            ),
        }
    }
}

pub(crate) async fn service_cmd(socket: &Path, data_dir: &Path, action: ServiceAction) -> anyhow::Result<()> {
    let none = || {
        anyhow::anyhow!(
            "{}",
            ictl_l10n::tr!(
                "没有装后台服务：iphone-control service install",
                "No service is set up: iphone-control service install"
            )
        )
    };
    match action {
        ServiceAction::Install => {
            let s = service::install()?;
            println!("{}", ictl_l10n::trf!("已安装并启动：{}", "Installed and started: {}", s.describe()));
        }
        ServiceAction::Uninstall => {
            service::uninstall()?;
            end_background_wda(socket, data_dir).await;
        }
        ServiceAction::Start => service::start(&service::find().ok_or_else(none)?)?,
        ServiceAction::Stop => {
            service::stop(&service::find().ok_or_else(none)?)?;
            end_background_wda(socket, data_dir).await;
        }
        ServiceAction::Restart => service::restart(&service::find().ok_or_else(none)?)?,
        ServiceAction::Viewer { action } => viewer_service_cmd(action)?,
        ServiceAction::Status => {
            match service::find() {
                Some(s) => println!("{}", ictl_l10n::trf!("后台服务  {}", "Service   {}", s.describe())),
                None => println!(
                    "{}",
                    ictl_l10n::tr!(
                        "后台服务  没有装（iphone-control service install）",
                        "Service   not set up (iphone-control service install)"
                    )
                ),
            }
            match service::viewer_find() {
                Some(s) => println!(
                    "{}",
                    if service::running(&s) {
                        ictl_l10n::trf!("控制台    {}，在运行", "Viewer    {}, running", s.describe())
                    } else {
                        ictl_l10n::trf!("控制台    {}，没在运行", "Viewer    {}, not running", s.describe())
                    }
                ),
                None => println!(
                    "{}",
                    ictl_l10n::tr!(
                        "控制台    没装成服务（iphone-control service viewer install）",
                        "Viewer    no service (iphone-control service viewer install)"
                    )
                ),
            }
            // to check against the browser's warning when the page is opened from another machine
            if let Some(fp) = viewer::tls::made_fingerprint(data_dir) {
                println!(
                    "{}",
                    ictl_l10n::trf!(
                        "          本机生成的 HTTPS 证书 SHA-256 {fp}",
                        "          HTTPS certificate made here, SHA-256 {fp}"
                    )
                );
            }
            let up = snapshot(socket).await.is_ok();
            println!(
                "{}",
                if up {
                    ictl_l10n::tr!("守护进程  在运行", "Daemon    running")
                } else {
                    ictl_l10n::tr!("守护进程  没在运行", "Daemon    not running")
                }
            );
        }
    }
    Ok(())
}

fn viewer_service_cmd(action: ViewerServiceAction) -> anyhow::Result<()> {
    let none = || {
        anyhow::anyhow!(
            "{}",
            ictl_l10n::tr!(
                "没有装控制台服务：iphone-control service viewer install",
                "No viewer service is set up: iphone-control service viewer install"
            )
        )
    };
    match action {
        ViewerServiceAction::Install => {
            let s = service::viewer_install()?;
            let port = env_nonempty(ictl_daemon::protocol::env::VIEWER_PORT)
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(8200);
            println!("{}", ictl_l10n::trf!("已安装并启动：{}", "Installed and started: {}", s.describe()));
            println!("{}", ictl_l10n::trf!("控制台：http://127.0.0.1:{port}/", "Viewer: http://127.0.0.1:{port}/"));
            let lan = env_nonempty(ictl_daemon::protocol::env::VIEWER_LAN)
                .is_some_and(|v| !matches!(v.as_str(), "0" | "false" | "no" | "off"));
            println!(
                "{}",
                if lan {
                    ictl_l10n::tr!(
                        "已对局域网开放（IPHONE_CONTROL_VIEWER_LAN）。登录用的令牌：iphone-control viewer token。",
                        "Open to the local network (IPHONE_CONTROL_VIEWER_LAN). The login token: iphone-control viewer token."
                    )
                } else {
                    ictl_l10n::tr!(
                        "只在本机。要对局域网开放：设置文件里设 IPHONE_CONTROL_VIEWER_LAN=1，再 iphone-control service viewer restart。",
                        "Local only. To open it to the local network: IPHONE_CONTROL_VIEWER_LAN=1 in the settings file, then iphone-control service viewer restart."
                    )
                }
            );
            let found: Vec<&str> = agent::Backend::ALL.iter().filter(|b| b.find().is_some()).map(|b| b.id()).collect();
            if found.is_empty() {
                println!(
                    "{}",
                    ictl_l10n::tr!(
                        "这个终端的 PATH 里没有 claude 或 codex，服务里的控制台也用不了 AI 任务；装好后在 PATH 里能找到它们的终端里再装一次。",
                        "Neither claude nor codex is on this shell's PATH, so the service's viewer can't run AI tasks either; install again from a shell where they are."
                    )
                );
            }
        }
        ViewerServiceAction::Uninstall => service::viewer_uninstall()?,
        ViewerServiceAction::Start => service::start(&service::viewer_find().ok_or_else(none)?)?,
        ViewerServiceAction::Stop => service::stop(&service::viewer_find().ok_or_else(none)?)?,
        ViewerServiceAction::Restart => service::restart(&service::viewer_find().ok_or_else(none)?)?,
    }
    Ok(())
}
