//! iphone-control：唯一的可执行程序。守护进程、MCP 服务和运维命令都在这里。
//!
//! 不带参数运行是首次运行引导（[`guide`]）：没设置好就一步步引导，设置好以后只显示状态。
//! 运维命令（status、doctor、resign、unlock-reset）经控制接口问守护进程；
//! setup、login、passcode 需要交互，直接在本进程里做。每个命令启动时先读 ~/.config/iphone-control/env（[`envfile`]）。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use ictl_daemon::config::env as env_nonempty;
use ictl_daemon::protocol::{self, Request, Response};

mod agent;
#[cfg(feature = "debug-cmds")]
mod debug;
mod envfile;
mod guide;
mod schedule;
mod service;
mod update;
mod video;
mod viewer;

#[derive(Parser)]
#[command(name = "iphone-control", version, about = ictl_l10n::tr!("在 Linux 或 macOS 上控制 iPhone，并提供 MCP 工具", "Control an iPhone from Linux or macOS, with MCP tools"))]
struct Cli {
    #[arg(help = ictl_l10n::tr!("数据根目录（账号、描述文件、日志等）。默认：root 是 /var/lib/iphone-control，普通用户是 ~/.local/share/iphone-control", "Data directory (account, provisioning profiles, logs, ...). Default: /var/lib/iphone-control for root, ~/.local/share/iphone-control for a regular user"))]
    #[arg(long, env = "ICTL_DATA_DIR", default_value_os_t = protocol::default_data_dir())]
    data_dir: PathBuf,
    #[arg(help = ictl_l10n::tr!("守护进程控制接口。默认：root 是 /run/iphone-control/ctl.sock，普通用户是 $XDG_RUNTIME_DIR/iphone-control/ctl.sock", "Daemon control socket. Default: /run/iphone-control/ctl.sock for root, $XDG_RUNTIME_DIR/iphone-control/ctl.sock for a regular user"))]
    #[arg(long, env = "ICTL_SOCKET", default_value_os_t = protocol::default_socket())]
    socket: PathBuf,
    /// 不给就是首次运行引导（设置好以后显示状态）
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = ictl_l10n::tr!("检查主机软件版本，有新版时升级", "Check for a new host software release and install it if available"))]
    Update {
        #[arg(short, long, help = ictl_l10n::tr!("不询问，直接升级（脚本里或没有终端时用）", "Upgrade without asking (for scripts, or without a terminal)"))]
        yes: bool,
        #[command(subcommand)]
        action: Option<UpdateAction>,
    },
    #[command(about = ictl_l10n::tr!("常驻守护进程（安装时装成用户服务：macOS 是 launchd，Linux 是 systemd；也可以手动启动）", "The long-running daemon (installed as a user service: launchd on macOS, systemd on Linux; can also be started by hand)"))]
    Daemon,
    #[command(about = ictl_l10n::tr!("MCP stdio 服务（由 agent 拉起）", "MCP stdio server (started by the agent)"))]
    Mcp,
    #[command(about = ictl_l10n::tr!("打印把 MCP 注册到 Claude Code 的命令", "Print the command that registers the MCP server with Claude Code"))]
    McpConfig,
    #[command(about = ictl_l10n::tr!("守护进程和设备状态、最近的日志", "Daemon and device state, recent log"))]
    Status,
    #[command(about = ictl_l10n::tr!("逐层诊断，每项失败都给出修复办法", "Layer-by-layer health check with a fix for each failure"))]
    Doctor,
    #[command(about = ictl_l10n::tr!("设置引导：语言、代理、后台服务、配对、开发者模式、Apple ID、安装 WDA、自动解锁、通知、注册 MCP（可重复运行，做过的步骤跳过）", "Guided setup: language, proxy, background service, pairing, Developer Mode, Apple ID, WDA, auto-unlock, notifications, MCP registration (safe to re-run; done steps are skipped)"))]
    Setup {
        #[arg(help = ictl_l10n::tr!("重新设置：全部重走，或只改其中一项", "Set up again: all of it, or one part"))]
        #[arg(long)]
        reconfigure: bool,
    },
    #[command(about = ictl_l10n::tr!("后台服务（macOS 是 launchd，Linux 是 systemd 用户服务）：install / uninstall / start / stop / restart / status", "The background service (launchd on macOS, a systemd user service on Linux): install / uninstall / start / stop / restart / status"))]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    #[command(about = ictl_l10n::tr!("登录 Apple ID（密码和双重认证验证码只在这里输入）", "Sign in to the Apple ID (the password and two-factor code are only entered here)"))]
    Login {
        #[arg(help = ictl_l10n::tr!("Apple ID 邮箱；不给就交互输入", "Apple ID email; asked for interactively if not given"))]
        email: Option<String>,
    },
    #[command(about = ictl_l10n::tr!("立即签名并安装 WDA", "Sign and install WDA now"))]
    Sign,
    #[command(about = ictl_l10n::tr!("检查描述文件，快过期就重签；--force 立即重签", "Check the provisioning profile and re-sign if it expires soon; --force re-signs now"))]
    Resign {
        #[arg(long)]
        force: bool,
    },
    #[command(about = ictl_l10n::tr!("设置/删除锁屏密码（自动解锁，默认关闭）", "Set or remove the passcode (auto-unlock, off by default)"))]
    Passcode {
        #[command(subcommand)]
        action: PasscodeAction,
    },
    #[command(about = ictl_l10n::tr!("解除自动解锁的停用（输错或中途出错后停用）。只删停用标记，不核对密码：先确认保存的密码对，必要时 passcode set 重设", "Re-enable auto-unlock after it was disabled (by a wrong passcode or an error midway). Only removes the flag, doesn't check the passcode: make sure the saved one is right first, or set it again with passcode set"))]
    UnlockReset,
    #[command(about = ictl_l10n::tr!("列出 USB 上接着的设备，标出正在控制哪台", "List devices connected over USB and mark the one being controlled"))]
    Devices,
    #[command(about = ictl_l10n::tr!("改为控制另一台设备，重启后还记得；auto 改回自动选（正在控制的那台断开才换）", "Control another device, remembered across restarts; auto goes back to automatic (switches only when the controlled one disconnects)"))]
    Select {
        #[arg(help = ictl_l10n::tr!("UDID、设备名或 auto", "UDID, device name or auto"))]
        device: String,
    },
    #[command(about = ictl_l10n::tr!("显式把 WDA 桥接到 127.0.0.1 供外部工具使用（默认不监听任何端口）", "Explicitly expose WDA on 127.0.0.1 for other tools (by default no port is opened)"))]
    WdaBridge {
        #[arg(long, default_value_t = 8100)]
        port: u16,
    },
    #[command(about = ictl_l10n::tr!("浏览器里的控制台：实时画面上直接操作手机、看设备状态，把任务交给本机的 Claude Code 或 Codex 执行。只开在 127.0.0.1，操作都经守护进程（默认不监听任何端口）", "Control page in a browser: act on the live screen, see the device's state, and hand tasks to a local Claude Code or Codex. Serves 127.0.0.1 only, every action goes through the daemon (by default no port is opened)"))]
    Viewer {
        #[arg(long, default_value_t = 8200)]
        port: u16,
    },
    #[command(about = ictl_l10n::tr!("真机诊断（开发用，要用 `--features debug-cmds` 编译）。`wda`/`e2e`/`hid` 会和守护进程抢 XCTest、HID，先停掉守护进程", "Real-device diagnostics (for development, build with `--features debug-cmds`). `wda`/`e2e`/`hid` compete with the daemon for XCTest and HID, so stop the daemon first"))]
    #[cfg(feature = "debug-cmds")]
    #[command(hide = true)]
    Debug(debug::DebugArgs),
    #[command(about = ictl_l10n::tr!("按已配置的渠道发测试通知：每类事件各一条（标题带「[测试]」），或只发指定的一类", "Send test notifications over the configured channels: one per event type (title starts with “[test]”), or only the given one"))]
    NotifyTest {
        #[arg(help = ictl_l10n::tr!("resign / session / pin / pin-recovered / unlock / offline / online；不给就全发", "resign / session / pin / pin-recovered / unlock / offline / online; all if not given"))]
        event: Option<String>,
    },
}

#[derive(Subcommand)]
enum UpdateAction {
    /// Check the latest stable release for this build and installation
    Check {
        #[arg(long)]
        json: bool,
    },
    /// Save the local update policy and configure its user job
    Policy { policy: update::Policy },
    /// Install a verified release without asking (managed user service only)
    Apply,
    /// Release an explicit installer version pin without enabling automatic updates
    Unpin,
    /// Recover an interrupted replacement
    Recover {
        #[arg(long, hide = true)]
        scheduled: bool,
    },
    #[command(hide = true)]
    Schedule,
    #[command(hide = true)]
    Run,
}

#[derive(Subcommand)]
enum ServiceAction {
    #[command(about = ictl_l10n::tr!("装成登录后自动启动的后台服务并启动（已有就直接启动）", "Set up the service that starts at login and start it (just start it if it exists)"))]
    Install,
    #[command(about = ictl_l10n::tr!("停止并移除本程序装的后台服务（数据保留）", "Stop and remove the service this program set up (data is kept)"))]
    Uninstall,
    Start,
    Stop,
    #[command(about = ictl_l10n::tr!("重启，改过 ~/.config/iphone-control/env 后用", "Restart, after editing ~/.config/iphone-control/env"))]
    Restart,
    Status,
}

/// 锁屏密码按设备分开存：一台的密码不会输进另一台
#[derive(Subcommand)]
enum PasscodeAction {
    Set {
        #[arg(help = ictl_l10n::tr!("哪台设备（UDID 或设备名）；不给就是守护进程正在控制的那台", "Which device (UDID or name); defaults to the one the daemon controls"))]
        #[arg(long)]
        device: Option<String>,
    },
    Clear {
        #[arg(help = ictl_l10n::tr!("哪台设备（UDID 或设备名）；不给就是守护进程正在控制的那台", "Which device (UDID or name); defaults to the one the daemon controls"))]
        #[arg(long)]
        device: Option<String>,
        #[arg(help = ictl_l10n::tr!("删掉所有设备的密码", "Delete the passcodes of all devices"))]
        #[arg(long, conflicts_with = "device")]
        all: bool,
    },
}

fn main() -> anyhow::Result<()> {
    // 设置文件先读进环境（命令行上已设的优先），要在启动任何线程之前：语言、代理、通知等对所有命令都生效
    envfile::load_into_process();
    tokio::runtime::Runtime::new()?.block_on(run())
}

/// Commands that ask once, print the result and end; see [`restore_sigpipe`]
fn prints_and_exits(command: Option<&Command>) -> bool {
    matches!(
        command,
        Some(
            Command::Status
                | Command::Doctor
                | Command::Devices
                | Command::McpConfig
                | Command::Select { .. }
                | Command::UnlockReset
                | Command::NotifyTest { .. }
                | Command::Update { action: Some(UpdateAction::Check { .. }), .. }
        )
    )
}

/// Rust starts every program with SIGPIPE ignored, so once the reader of stdout has gone (`iphone-control status |
/// head -1`), the next write is an EPIPE error and `println!` panics on it (exit 101 and a panic message). Commands
/// that only print a result get the usual behaviour back: the reader going away ends the process quietly, like any
/// other command-line tool. Not for the long-running commands (daemon, mcp, viewer, wda-bridge), nor for those
/// that keep talking to the daemon across its restarts (update, service, setup): there a write to a socket or a
/// child's pipe whose other end closed would end the process instead of returning an error it handles
fn restore_sigpipe() {
    // SAFETY: setting a signal's disposition back to its default, before any other thread exists that writes
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

async fn run() -> anyhow::Result<()> {
    // 依赖树里 rustls 的 ring 和 aws-lc-rs 都开着，不装默认 provider 的话第一个 HTTPS 请求就 panic
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut cli = Cli::parse();
    if prints_and_exits(cli.command.as_ref()) {
        restore_sigpipe();
    }
    let quiet = matches!(cli.command, Some(Command::Mcp));
    // ICTL_LOG 留空（配置文件里常见的 `ICTL_LOG=`）当作没设；原来空值会生成一个什么都不放行的过滤器
    let filter =
        env_nonempty("ICTL_LOG").and_then(|v| tracing_subscriber::EnvFilter::try_new(v).ok()).unwrap_or_else(|| {
            tracing_subscriber::EnvFilter::new(if quiet { "warn" } else { "info,idevice=warn,apple_codesign=warn" })
        });
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(filter)
        .init();

    // Lifetime clients hold this through pauses and confirmations, and direct-device CLI
    // commands participate even when they bypass the daemon.
    let _client = match &cli.command {
        Some(Command::Daemon | Command::Update { .. } | Command::McpConfig | Command::Status) => None,
        _ => Some(ictl_daemon::maintenance::shared()?),
    };
    let Some(command) = cli.command.take() else { return guide::first_run(&cli).await };
    match command {
        Command::Update { action: None, yes } => update::upgrade(&cli.socket, update::check(false).await?, yes).await,
        Command::Update { action: Some(action), .. } => update_command(action, &cli.socket).await,
        Command::Daemon => {
            let mut cfg = ictl_daemon::Config::from_env(&cli.data_dir);
            cfg.socket = cli.socket;
            ictl_daemon::run(cfg).await
        }
        Command::Mcp => ictl_mcp::serve_stdio(cli.socket).await,
        Command::McpConfig => {
            println!("{}", mcp_config(&cli.socket)?);
            Ok(())
        }
        Command::Status => status(&cli.socket).await,
        Command::Doctor => print_text(&cli.socket, Request::Doctor).await,
        Command::Setup { reconfigure } => guide::setup(&cli, reconfigure).await,
        Command::Service { action } => service_cmd(&cli.socket, action).await,
        Command::Login { email } => login(&cli.data_dir, email).await,
        Command::Sign => print_text(&cli.socket, Request::Resign { force: true }).await,
        Command::Resign { force } => print_text(&cli.socket, Request::Resign { force }).await,
        Command::Passcode { action } => passcode(&cli.data_dir, &cli.socket, action).await,
        Command::UnlockReset => print_text(&cli.socket, Request::UnlockReset).await,
        Command::Devices => print_text(&cli.socket, Request::ListDevices).await,
        Command::Select { device } => print_text(&cli.socket, Request::SelectDevice { device }).await,
        Command::WdaBridge { port } => wda_bridge(&cli.socket, port).await,
        Command::Viewer { port } => viewer::run(&cli.socket, &cli.data_dir, port).await,
        #[cfg(feature = "debug-cmds")]
        Command::Debug(args) => debug::run(env_nonempty("IPHONE_UDID").as_deref(), args).await,
        Command::NotifyTest { event } => notify_test(&cli.data_dir, event.as_deref()).await,
    }
}

async fn report_daemon(report: &mut update::Report, socket: &Path) {
    report.daemon_version = tokio::time::timeout(Duration::from_secs(1), snapshot(socket))
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|s| s.version);
}

async fn update_command(action: UpdateAction, socket: &Path) -> anyhow::Result<()> {
    use update::transaction;
    match action {
        UpdateAction::Check { json } => {
            let mut report = update::check(false).await?;
            report_daemon(&mut report, socket).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{report}");
            }
            if let Some(code) = report.error_code {
                bail!("Update check failed: {code}")
            }
            Ok(())
        }
        UpdateAction::Policy { policy } => schedule::set_policy(socket, policy),
        UpdateAction::Schedule => schedule::configure(socket),
        UpdateAction::Apply => transaction::apply(socket, false, update::check(false).await?).await,
        UpdateAction::Unpin => transaction::unpin(),
        UpdateAction::Recover { scheduled } => transaction::recover(socket, scheduled).await,
        UpdateAction::Run => {
            let report = update::check(true).await?;
            if report.automatic_patch() {
                transaction::apply(socket, true, report).await?;
            }
            Ok(())
        }
    }
}

async fn notify_test(data_dir: &Path, only: Option<&str>) -> anyhow::Result<()> {
    use ictl_daemon::notify::{Event, send};
    let cfg = ictl_daemon::Config::from_env(data_dir).notify;
    let channels =
        usize::from(cfg.bark_key.is_some()) + usize::from(cfg.tg_bot_token.is_some() && cfg.tg_chat_id.is_some());
    if channels == 0 {
        bail!(
            "{}",
            ictl_l10n::tr!(
                "没有配置通知渠道：运行 iphone-control setup --reconfigure 选「通知」，\
             或在 ~/.config/iphone-control/env 里填 BARK_KEY、或 TG_BOT_TOKEN + TG_CHAT_ID，再重启守护进程",
                "No notification channel configured: run iphone-control setup --reconfigure and pick notifications, or set BARK_KEY, or TG_BOT_TOKEN + TG_CHAT_ID, in ~/.config/iphone-control/env and restart the daemon"
            )
        );
    }
    let events: Vec<Event> =
        Event::samples().into_iter().filter(|e| only.is_none_or(|n| e.sample_name() == n)).collect();
    if events.is_empty() {
        let names: Vec<&str> = Event::samples().iter().map(|e| e.sample_name()).collect();
        bail!("{}", ictl_l10n::trf!("没有这类事件，可选：{}", "No such event; choose from: {}", names.join(" / ")));
    }
    let mut failed = 0;
    for e in &events {
        let (title, body) = e.title_body();
        let ok = send(&cfg, &ictl_l10n::trf!("[测试] {title}", "[test] {title}"), &body).await;
        println!(
            "{}",
            ictl_l10n::trf!(
                "{} {:<14} {title}（{ok}/{channels} 个渠道成功）",
                "{} {:<14} {title} ({ok}/{channels} channels succeeded)",
                if ok == channels { "✅" } else { "❌" },
                e.sample_name()
            )
        );
        failed += usize::from(ok < channels);
        // Bark 同组消息太密会被手机合并，隔一下
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if failed > 0 {
        bail!(
            "{}",
            ictl_l10n::trf!(
                "{failed} 条没有全部发出去，详见上面的 WARN 日志",
                "{failed} notification(s) were not sent to every channel, see the WARN log above"
            )
        );
    }
    Ok(())
}

/// MCP 进程由客户端直接拉起本程序：显式带上 socket，免得 Claude Code 的环境里没有 XDG_RUNTIME_DIR 时算出别的位置。
/// 英文、开发者工具开关写进注册命令：MCP 客户端不带用户 shell 的环境
fn mcp_config(socket: &Path) -> anyhow::Result<String> {
    Ok(mcp_command(&service::stable_exe()?, socket))
}

async fn service_cmd(socket: &Path, action: ServiceAction) -> anyhow::Result<()> {
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
        ServiceAction::Uninstall => service::uninstall()?,
        ServiceAction::Start => service::start(&service::find().ok_or_else(none)?)?,
        ServiceAction::Stop => service::stop(&service::find().ok_or_else(none)?)?,
        ServiceAction::Restart => service::restart(&service::find().ok_or_else(none)?)?,
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

fn mcp_command(exe: &Path, socket: &Path) -> String {
    let lang = if ictl_l10n::is_en() { format!("-e {}=en ", ictl_l10n::ENV) } else { String::new() };
    let dev = if ictl_mcp::dev_tools_enabled() { "-e IPHONE_CONTROL_DEV_TOOLS=1 " } else { "" };
    format!(
        "claude mcp add iphone -s user {lang}{dev}-- {} --socket {} mcp",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&socket.to_string_lossy())
    )
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:@%".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

async fn ask(socket: &Path, req: Request) -> anyhow::Result<Response> {
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

async fn print_text(socket: &Path, req: Request) -> anyhow::Result<()> {
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

async fn snapshot(socket: &Path) -> anyhow::Result<ictl_daemon::Snapshot> {
    match ask(socket, Request::Daemon).await? {
        Response::Text { text, .. } => Ok(serde_json::from_str(&text)?),
        Response::Error { message, .. } => bail!("{message}"),
        Response::Image { .. } => {
            bail!("{}", ictl_l10n::tr!("守护进程返回了意外的响应", "The daemon sent an unexpected response"))
        }
    }
}

async fn status(socket: &Path) -> anyhow::Result<()> {
    let s = snapshot(socket).await?;
    println!("{}", ictl_l10n::trf!("状态      {}", "State     {}", s.state.describe()));
    if let Some(d) = &s.device {
        println!(
            "{}",
            ictl_l10n::trf!(
                "手机      {} {} iOS {}",
                "Phone     {} {} iOS {}",
                d.udid,
                d.product_type,
                d.product_version
            )
        );
    }
    let current = s.device.as_ref().map(|d| d.udid.as_str());
    let others: Vec<String> =
        s.devices.iter().filter(|d| Some(d.udid.as_str()) != current).map(ictl_daemon::core::device_label).collect();
    if !others.is_empty() {
        println!(
            "{}",
            ictl_l10n::trf!(
                "另外接着  {}（iphone-control select 切换）",
                "Also on   {} (switch with iphone-control select)",
                others.join(ictl_l10n::tr!("、", ", "))
            )
        );
    }
    match (&s.pinned, s.pinned_by_env) {
        (Some(p), true) => println!(
            "{}",
            ictl_l10n::trf!("选设备    IPHONE_UDID 固定为 {p}", "Device    pinned by IPHONE_UDID to {p}")
        ),
        (Some(p), false) => println!(
            "{}",
            ictl_l10n::trf!(
                "选设备    选定了 {p}（iphone-control select auto 改回自动选）",
                "Device    {p} selected (iphone-control select auto for automatic)"
            )
        ),
        (None, _) if !others.is_empty() => println!(
            "{}",
            ictl_l10n::tr!(
                "选设备    自动（正在控制的那台断开才换）",
                "Device    automatic (switches only when the controlled one disconnects)"
            )
        ),
        (None, _) => {}
    }
    let p = &s.persisted;
    println!("WDA       {}", p.wda_bundle.as_deref().unwrap_or(ictl_l10n::tr!("未签名", "not signed")));
    match &s.device_profile {
        ictl_daemon::core::DeviceProfile::Missing => {
            println!(
                "{}",
                ictl_l10n::tr!(
                    "描述文件  这台手机上没有装当前 Apple ID 签的 WDA（iphone-control setup）",
                    "Profile   no WDA signed by the current Apple ID on this phone (iphone-control setup)"
                )
            )
        }
        dp => {
            if let Some(left) = dp.left(p) {
                let whose = if matches!(dp, ictl_daemon::core::DeviceProfile::Expires(_)) {
                    ictl_l10n::tr!("手机上的", "on the phone, ")
                } else {
                    ictl_l10n::tr!("上次签的", "last signed, ")
                };
                println!(
                    "{}",
                    ictl_l10n::trf!(
                        "描述文件  {whose}还剩 {} 小时",
                        "Profile   {whose}{} hours left",
                        left.as_secs() / 3600
                    )
                );
            }
        }
    }
    if let Some(r) = p.last_resign_text() {
        println!("{}", ictl_l10n::trf!("上次重签  {r}", "Re-signed {r}"));
    }
    let unlock = match (s.auto_unlock, s.unlock_disabled) {
        (_, true) => ictl_l10n::tr!(
            "已停用（之前输错过，iphone-control unlock-reset）",
            "disabled (a wrong passcode earlier; iphone-control unlock-reset)"
        ),
        (true, false) => ictl_l10n::tr!("已启用", "enabled"),
        (false, false) => ictl_l10n::tr!("未启用", "off"),
    };
    println!("{}", ictl_l10n::trf!("自动解锁  {unlock}", "Unlock    {unlock}"));
    println!(
        "{}",
        ictl_l10n::trf!(
            "通知      {}",
            "Notify    {}",
            if s.notify_configured {
                ictl_l10n::tr!("已配置", "configured")
            } else {
                ictl_l10n::tr!("未配置", "not configured")
            }
        )
    );
    println!("{}", ictl_l10n::tr!("\n最近日志：", "\nRecent log:"));
    for l in s.log_tail {
        println!("  {l}");
    }
    Ok(())
}

fn read_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    if std::io::stdin().lock().read_line(&mut s)? == 0 {
        bail!(
            "{}",
            ictl_l10n::tr!(
                "读不到输入（标准输入已关闭）：在终端里运行",
                "Can't read input (stdin is closed): run it in a terminal"
            )
        );
    }
    Ok(s.trim().to_owned())
}

/// 不回显地读一行（用 stty 关回显，不引入额外依赖）
fn read_secret(prompt: &str) -> anyhow::Result<String> {
    let stty = |arg: &str| {
        std::process::Command::new("stty")
            .arg(arg)
            .stdin(
                std::fs::File::open("/dev/tty").map(std::process::Stdio::from).unwrap_or(std::process::Stdio::null()),
            )
            .status()
    };
    let echo_off = stty("-echo").map(|s| s.success()).unwrap_or(false);
    let r = read_line(prompt);
    if echo_off {
        let _ = stty("echo");
        println!();
    }
    r
}

/// 账号目录和 anisette 地址：与守护进程同一份配置（路径、环境变量的读法都在 Config 里）
fn account_dir(data_dir: &Path) -> ictl_sign::AccountDir {
    let cfg = ictl_daemon::Config::from_env(data_dir);
    ictl_sign::AccountDir { dir: cfg.account_dir(), anisette_url: cfg.anisette_url }
}

async fn login(data_dir: &Path, email: Option<String>) -> anyhow::Result<()> {
    println!(
        "{}",
        ictl_l10n::tr!(
            "建议用一个专门的免费 Apple ID，不要用主账号。",
            "Use a dedicated free Apple ID, not your main account."
        )
    );
    println!(
        "{}",
        ictl_l10n::tr!(
            "密码和验证码由本程序直接发给苹果；登录时会请求 anisette 服务生成设备标识（默认第三方，可用 ICTL_ANISETTE_URL 改成自建）。",
            "The password and verification code go straight from this program to Apple; signing in asks an anisette service for device identifiers (a third-party one by default, set ICTL_ANISETTE_URL to use your own)."
        )
    );
    let email = match email {
        Some(e) => e,
        None => read_line(ictl_l10n::tr!("Apple ID 邮箱: ", "Apple ID email: "))?,
    };
    if email.is_empty() {
        bail!("{}", ictl_l10n::tr!("没有输入 Apple ID 邮箱", "No Apple ID email entered"));
    }
    let password = read_secret(ictl_l10n::tr!("密码(输入不显示): ", "Password (hidden): "))?;
    if password.is_empty() {
        bail!("{}", ictl_l10n::tr!("没有输入密码", "No password entered"));
    }
    let two_factor = |req: ictl_sign::TwoFactorRequest| -> Result<ictl_sign::TwoFactorAction, String> {
        let sms = !req.trusted_phone_numbers.is_empty();
        let prompt = match req.method {
            ictl_sign::TwoFactorMethod::Sms => ictl_l10n::tr!("短信验证码: ", "SMS code: "),
            ictl_sign::TwoFactorMethod::Device if sms => ictl_l10n::tr!(
                "设备上收到的验证码(输入 sms 改为短信接收): ",
                "Code shown on your device (enter sms to get it by text message): "
            ),
            ictl_sign::TwoFactorMethod::Device => ictl_l10n::tr!("设备上收到的验证码: ", "Code shown on your device: "),
        };
        let code = read_line(prompt).map_err(|e| e.to_string())?;
        if sms && code.eq_ignore_ascii_case("sms") {
            return Ok(ictl_sign::TwoFactorAction::SendSms(req.trusted_phone_numbers[0].id));
        }
        Ok(ictl_sign::TwoFactorAction::SubmitCode(code))
    };
    account_dir(data_dir).login(&email, &password, two_factor).await?;
    println!(
        "{}",
        ictl_l10n::trf!(
            "登录成功，会话已保存到 {}",
            "Signed in, session saved to {}",
            account_dir(data_dir).dir.display()
        )
    );
    Ok(())
}

/// 命令要针对哪台设备：`IPHONE_UDID` > 守护进程正在控制的（或选定的）> USB 上唯一的一台。
/// 给了 `query`（UDID 或设备名）就在接着的设备里找
async fn target_device(socket: &Path, query: Option<&str>) -> anyhow::Result<ictl_device::DeviceInfo> {
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
    if let Some(u) = env_nonempty("IPHONE_UDID") {
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

async fn passcode(data_dir: &Path, socket: &Path, action: PasscodeAction) -> anyhow::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let cfg = ictl_daemon::Config::from_env(data_dir);
    match action {
        PasscodeAction::Clear { all: true, .. } => {
            let _ = std::fs::remove_dir_all(cfg.passcodes_dir());
            let _ = std::fs::remove_file(cfg.legacy_passcode_file());
            println!(
                "{}",
                ictl_l10n::tr!(
                    "已删除所有设备的锁屏密码，自动解锁关闭。",
                    "Deleted the passcodes of all devices, auto-unlock is off."
                )
            );
        }
        PasscodeAction::Clear { device, .. } => {
            let d = target_device(socket, device.as_deref()).await?;
            let _ = std::fs::remove_file(cfg.passcode_file_for(&d.udid));
            println!(
                "{}",
                ictl_l10n::trf!(
                    "已删除 {} 的锁屏密码，这台的自动解锁关闭。",
                    "Deleted the passcode of {}, auto-unlock is off for it.",
                    ictl_daemon::core::device_label(&d)
                )
            );
        }
        PasscodeAction::Set { device } => {
            let d = target_device(socket, device.as_deref()).await?;
            let path = cfg.passcode_file_for(&d.udid);
            println!("{}", ictl_l10n::trf!("设备：{}", "Device: {}", ictl_daemon::core::device_label(&d)));
            println!(
                "{}",
                ictl_l10n::tr!(
                    "自动解锁：锁屏后，MCP 操作前会用这个密码解锁手机。",
                    "Auto-unlock: when the phone is locked, MCP actions use this passcode to unlock it first."
                )
            );
            println!(
                "{}",
                ictl_l10n::tr!(
                    "风险：能登录这台机器、或能调用这个 MCP 的人，都能解锁你的手机。",
                    "Risk: anyone who can log in to this machine or call this MCP server can unlock your phone."
                )
            );
            println!(
                "{}",
                ictl_l10n::tr!(
                    "保护：每按一位都核对；整串输完仍锁着就停用自动解锁并推送通知，不会累计输错。只支持纯数字。",
                    "Safeguards: every digit is checked; if the phone is still locked after the whole passcode, auto-unlock is disabled and a notification is sent, so wrong attempts don't pile up. Numeric passcodes only."
                )
            );
            let p = read_secret(ictl_l10n::tr!("锁屏密码(输入不显示): ", "Passcode (hidden): "))?;
            if p.len() < 4 || !p.bytes().all(|b| b.is_ascii_digit()) {
                bail!(
                    "{}",
                    ictl_l10n::tr!(
                        "只支持 4 位以上的纯数字密码",
                        "Only numeric passcodes with 4 or more digits are supported"
                    )
                );
            }
            cfg.ensure_passcodes_dir()?;
            let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&path)?;
            // mode() 只在新建时生效：已有文件也收紧
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            f.write_all(p.as_bytes())?;
            println!(
                "{}",
                ictl_l10n::tr!(
                    "已保存（只用于这台设备）。别忘了在手机上把「自动锁定」从「永不」改回较短时间。",
                    "Saved (for this device only). Remember to set Auto-Lock on the phone back from “Never” to a shorter time."
                )
            );
        }
    }
    Ok(())
}

/// 每隔 `every` 检查一次，直到 `check` 返回 true 或超时
async fn wait_until<F, Fut>(timeout: Duration, every: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(every).await;
    }
}

/// The phone's part of the guide: connect, pair, Developer Mode, Apple ID, sign and install WDA, wait for WDA.
/// Needs the daemon running ([`guide`] starts it first)
async fn device_setup(cli: &Cli) -> anyhow::Result<()> {
    use ictl_device::{Device, DeviceError};
    let socket = &cli.socket;

    println!("{}", ictl_l10n::tr!("[1/6] 等待手机连接", "[1/6] Waiting for the phone"));
    let found = wait_until(Duration::from_secs(600), Duration::from_secs(5), || async {
        ictl_device::list_devices().await.map(|d| !d.is_empty()).unwrap_or(false)
    })
    .await;
    if !found {
        bail!(
            "{}",
            ictl_l10n::tr!(
                "10 分钟内没检测到手机：用数据线接好并解锁手机",
                "No phone detected within 10 minutes: plug it in with a data cable and unlock it"
            )
        );
    }
    // 和守护进程控制同一台：第 5 步的签名安装是守护进程对它正在控制的那台做的
    let target = target_device(socket, None).await?;
    let udid = Some(target.udid.clone());
    let device = Device::find(udid.as_deref()).await?;
    println!("{}", ictl_l10n::trf!("  手机 {}", "  Phone {}", ictl_daemon::core::device_label(&target)));

    println!("{}", ictl_l10n::tr!("[2/6] 配对", "[2/6] Pairing"));
    if device.paired().await? {
        println!("{}", ictl_l10n::tr!("  已配对", "  Already paired"));
    } else {
        println!(
            "{}",
            ictl_l10n::tr!(
                "  手机上会弹出「要信任此电脑吗?」:点「信任」,再输入锁屏密码。",
                "  The phone shows “Trust This Computer?”: tap “Trust” and enter the passcode."
            )
        );
        let ok = wait_until(Duration::from_secs(300), Duration::from_secs(5), || async {
            match device.pair("iphone-control").await {
                Ok(()) => true,
                Err(DeviceError::PairingPending | DeviceError::PasswordProtected) => false,
                Err(e) => {
                    eprintln!("  {e}");
                    false
                }
            }
        })
        .await;
        if !ok {
            bail!("{}", ictl_l10n::tr!("5 分钟内没完成配对", "Pairing not completed within 5 minutes"));
        }
        println!("{}", ictl_l10n::tr!("  配对成功", "  Paired"));
    }

    println!("{}", ictl_l10n::tr!("[3/6] 开发者模式", "[3/6] Developer Mode"));
    if device.developer_mode_enabled().await.unwrap_or(false) {
        println!("{}", ictl_l10n::tr!("  已打开", "  Already on"));
    } else {
        let _ = device.reveal_developer_mode().await;
        println!(
            "{}",
            ictl_l10n::tr!(
                "  请在手机上:设置 → 隐私与安全性 → 开发者模式 → 打开 → 重新启动;",
                "  On the phone: Settings → Privacy & Security → Developer Mode → on → Restart;"
            )
        );
        println!(
            "{}",
            ictl_l10n::tr!(
                "  重启后解锁,在弹窗里点「打开」并输入密码。手机重启期间连接会断开,会自动恢复。",
                "  after the restart, unlock it, tap “Turn On” in the prompt and enter the passcode. The connection drops while the phone restarts and recovers by itself."
            )
        );
        let ok = wait_until(Duration::from_secs(900), Duration::from_secs(10), || async {
            match Device::find(udid.as_deref()).await {
                Ok(d) => d.developer_mode_enabled().await.unwrap_or(false),
                Err(_) => false,
            }
        })
        .await;
        if !ok {
            bail!(
                "{}",
                ictl_l10n::tr!("15 分钟内没检测到开发者模式打开", "Developer Mode not turned on within 15 minutes")
            );
        }
        println!("{}", ictl_l10n::tr!("  已打开", "  Turned on"));
    }

    println!("{}", ictl_l10n::tr!("[4/6] 登录 Apple ID", "[4/6] Apple ID sign-in"));
    match account_dir(&cli.data_dir).selected_email().await {
        Ok(e) => println!("{}", ictl_l10n::trf!("  已登录 {e}", "  Signed in as {e}")),
        Err(_) => login(&cli.data_dir, None).await?,
    }

    println!(
        "{}",
        ictl_l10n::tr!(
            "[5/6] 注册设备、签名 WDA 并安装到手机（手机要保持解锁）",
            "[5/6] Registering the device, signing WDA and installing it on the phone (keep the phone unlocked)"
        )
    );
    // 前几步直接问 usbmuxd，重签却由守护进程做：它每 10 秒巡检一次才认到手机。手机刚重启完、usbmuxd 刚重启时
    // 这里会先看到手机，马上请求重签就报「手机未连接」，所以先等守护进程认到同一台
    let target = device.udid.clone();
    let seen = wait_until(Duration::from_secs(60), Duration::from_secs(2), || async {
        snapshot(socket).await.is_ok_and(|s| {
            s.state != ictl_daemon::machine::DeviceState::Absent && s.device.as_ref().is_none_or(|d| d.udid == target)
        })
    })
    .await;
    if !seen {
        bail!(
            "{}",
            ictl_l10n::trf!(
                "守护进程 1 分钟内没认到手机 {}，运行 iphone-control doctor 看看卡在哪",
                "The daemon didn't pick up the phone {} within a minute; run iphone-control doctor to see where it is stuck",
                device.udid
            )
        );
    }
    // already on the phone, signed by this Apple ID, and not about to expire: running the guide again doesn't re-sign
    // (the daemon re-signs by itself before it expires)
    let left = snapshot(socket).await.ok().and_then(|s| match s.device_profile {
        ictl_daemon::core::DeviceProfile::Expires(_) => s.device_profile.left(&s.persisted),
        _ => None,
    });
    match left.filter(|l| *l > Duration::from_secs(2 * 86400)) {
        Some(l) => println!(
            "{}",
            ictl_l10n::trf!(
                "  已签名安装，描述文件还剩 {} 天，到期前会自动重签",
                "  Already signed and installed, the profile has {} days left and is re-signed automatically before it expires",
                l.as_secs() / 86400
            )
        ),
        None => print_text(socket, Request::Resign { force: true }).await?,
    }

    println!("{}", ictl_l10n::tr!("[6/6] 等待 WDA 启动", "[6/6] Waiting for WDA to start"));
    let ready_now = snapshot(socket).await.is_ok_and(|s| s.state == ictl_daemon::machine::DeviceState::Ready);
    if !ready_now {
        println!(
            "{}",
            ictl_l10n::tr!(
                "  首次安装:在手机 设置 → 通用 → VPN 与设备管理 里找到你的 Apple ID,点「信任」(需要联网)。",
                "  First install: on the phone, open Settings → General → VPN & Device Management, find your Apple ID and tap “Trust” (needs internet)."
            )
        );
        println!(
            "{}",
            ictl_l10n::tr!(
                "  如果弹出「为 XCTest 输入 iPhone 密码」,输入锁屏密码。",
                "  If “Enter iPhone passcode for XCTest” appears, enter the passcode."
            )
        );
    }
    let ready = wait_until(Duration::from_secs(600), Duration::from_secs(10), || async {
        snapshot(socket).await.map(|s| s.state == ictl_daemon::machine::DeviceState::Ready).unwrap_or(false)
    })
    .await;
    println!(
        "  {}",
        if ready {
            ictl_l10n::tr!("WDA 就绪", "WDA ready")
        } else {
            ictl_l10n::tr!(
                "10 分钟内 WDA 没就绪,运行 iphone-control doctor 看看卡在哪",
                "WDA wasn't ready within 10 minutes; run iphone-control doctor to see where it is stuck"
            )
        }
    );
    Ok(())
}

/// 显式开启：127.0.0.1:port → 手机上的 WDA 8100。只绑本机回环地址。
/// 本机的其他用户、浏览器里的网页都能访问到，而 WDA 没有认证。
/// 浏览器那条路（DNS rebinding）靠检查 Host 头挡住；本机其他用户挡不住，用完就停。
async fn wda_bridge(socket: &Path, port: u16) -> anyhow::Result<()> {
    // 和守护进程控制同一台（WDA 只在那台上跑着）
    let target = target_device(socket, None).await?;
    let device = ictl_device::Device::find(Some(&target.udid)).await?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!(
        "{}",
        ictl_l10n::trf!(
            "WDA 桥接 127.0.0.1:{port} → 手机 {} 的 8100 端口（Ctrl-C 停止）",
            "WDA bridge 127.0.0.1:{port} → port 8100 on phone {} (Ctrl-C to stop)",
            device.udid
        )
    );
    println!(
        "{}",
        ictl_l10n::tr!(
            "注意：本机所有用户都能经这个端口控制手机（WDA 没有认证），用完就停掉。",
            "Note: every user on this machine can control the phone through this port (WDA has no authentication); stop it when you're done."
        )
    );
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (client, _) = listener.accept().await?;
        // 连接太多就不接新的（见 MAX_CONNECTIONS）
        let Ok(slot) = slots.clone().try_acquire_owned() else { continue };
        let device = device.clone();
        tokio::spawn(async move {
            if let Err(e) = bridge_conn(client, &device).await {
                tracing::warn!("{}", ictl_l10n::log_trf!("桥接连接出错：{e}", "Bridge connection error: {e}"));
            }
            drop(slot);
        });
    }
}

/// 先读到第一个请求的头部，Host 必须是本机回环地址才转发：恶意网页借 DNS rebinding 发到 127.0.0.1 的请求，
/// Host 头是它自己的域名
async fn bridge_conn(mut client: tokio::net::TcpStream, device: &ictl_device::Device) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let (head, body) = read_head(&mut client).await?;
    if !host_is_loopback(&head) {
        client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    let mut remote = device.connect_port(8100).await?;
    remote.write_all(&head).await?;
    remote.write_all(&body).await?;
    tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}

/// 本机端口（`wda-bridge`、`viewer`）同时接的连接数上限，满了新连接直接断开（比如开了太多标签页）
const MAX_CONNECTIONS: usize = 32;
const MAX_HEAD: usize = 16 * 1024;
/// 客户端发请求头、请求体时单次读的等待上限：连上不发的连接不能一直占着名额
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// 读到请求头结束；返回头部和跟着一起到的那部分请求体
async fn read_head(client: &mut tokio::net::TcpStream) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    use tokio::io::AsyncReadExt;
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let body = data.split_off(i + 4);
            return Ok((data, body));
        }
        if data.len() > MAX_HEAD {
            bail!("{}", ictl_l10n::tr!("请求头太长", "request headers too long"));
        }
        let n = tokio::time::timeout(READ_TIMEOUT, client.read(&mut buf)).await??;
        if n == 0 {
            bail!("{}", ictl_l10n::tr!("连接已关闭", "connection closed"));
        }
        data.extend_from_slice(&buf[..n]);
    }
}

/// HTTP 请求头里的 Host 是不是 localhost / 127.0.0.1 / [::1]（端口不限）
fn host_is_loopback(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head);
    let Some(host) = text.split("\r\n").skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("host").then(|| v.trim().to_ascii_lowercase())
    }) else {
        return false;
    };
    let name = match host.strip_prefix('[') {
        Some(rest) => format!("[{}]", rest.split(']').next().unwrap_or_default()),
        None => host.split(':').next().unwrap_or_default().to_owned(),
    };
    matches!(name.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::{host_is_loopback, mcp_command};
    use ictl_l10n::{Lang, with_lang};
    use std::path::Path;

    /// Only the commands that print once and end get SIGPIPE's default disposition back
    #[test]
    fn sigpipe_default_only_for_commands_that_print_and_end() {
        use super::{Cli, prints_and_exits};
        use clap::Parser;

        let prints = |args: &[&str]| {
            let cli = Cli::try_parse_from(std::iter::once("iphone-control").chain(args.iter().copied())).unwrap();
            prints_and_exits(cli.command.as_ref())
        };
        for args in [&["status"][..], &["doctor"], &["devices"], &["mcp-config"], &["update", "check", "--json"]] {
            assert!(prints(args), "{args:?}");
        }
        for args in [
            &["daemon"][..],
            &["mcp"],
            &["viewer"],
            &["wda-bridge"],
            &["update"],
            &["update", "apply"],
            &["service", "restart"],
            &["setup"],
            &[],
        ] {
            assert!(!prints(args), "{args:?}");
        }
    }

    #[test]
    fn update_accepts_no_subcommand_and_preserves_read_only_check() {
        use super::{Cli, Command, UpdateAction};
        use clap::Parser;

        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().command;
        assert!(matches!(parse(&["iphone-control", "update"]), Some(Command::Update { action: None, yes: false })));
        assert!(matches!(
            parse(&["iphone-control", "update", "-y"]),
            Some(Command::Update { action: None, yes: true })
        ));
        assert!(matches!(
            parse(&["iphone-control", "update", "check", "--json"]),
            Some(Command::Update { action: Some(UpdateAction::Check { json: true }), .. })
        ));
        assert!(Cli::try_parse_from(["iphone-control", "update", "rollback"]).is_err(), "rollback was removed");
    }

    /// 英文时注册命令带上语言（MCP 客户端不带用户 shell 的环境）；中文时不带
    #[test]
    fn mcp_config_carries_english() {
        let cmd = |l| with_lang(l, || mcp_command(Path::new("/b/iphone-control"), Path::new("/s")));
        assert_eq!(cmd(Lang::Zh), "claude mcp add iphone -s user -- /b/iphone-control --socket /s mcp");
        assert_eq!(
            cmd(Lang::En),
            "claude mcp add iphone -s user -e IPHONE_CONTROL_LANG=en -- /b/iphone-control --socket /s mcp"
        );
    }

    #[test]
    fn bridge_only_accepts_loopback_host() {
        assert!(host_is_loopback(b"GET /status HTTP/1.1\r\nHost: 127.0.0.1:8100\r\n\r\n"));
        assert!(host_is_loopback(b"GET / HTTP/1.1\r\nhost: localhost\r\n\r\n"));
        assert!(host_is_loopback(b"GET / HTTP/1.1\r\nHost: [::1]:8100\r\n\r\n"));
        assert!(!host_is_loopback(b"GET / HTTP/1.1\r\nHost: evil.example:8100\r\n\r\n"));
        assert!(!host_is_loopback(b"GET / HTTP/1.1\r\nHost: localhost.evil.example\r\n\r\n"));
        assert!(!host_is_loopback(b"GET / HTTP/1.1\r\n\r\n"));
    }
}
