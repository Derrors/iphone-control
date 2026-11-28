//! iphone-control：唯一的可执行程序。守护进程、MCP 服务和运维命令都在这里。
//!
//! 不带参数运行是首次运行引导（[`guide`]）：没设置好就一步步引导，设置好以后只显示状态。
//! 运维命令（status、doctor、resign、unlock-reset）经控制接口问守护进程；
//! setup、login、passcode 需要交互，直接在本进程里做。除静态补全外，命令启动时先读 ~/.config/iphone-control/env（[`envfile`]）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use account::{login, passcode, read_line, read_secret};
use anyhow::bail;
use bridge::{HEAD_TIMEOUT, MAX_HEAD, wda_bridge};
use clap::{Parser, Subcommand};
use client::{print_text, snapshot};
use ictl_daemon::config::env as env_nonempty;
use ictl_daemon::protocol::{self, Request};
use service_cmd::service_cmd;
use setup::{device_setup, wait_until};
use status::{notify_test, status};
use viewer_cmd::{ViewerAt, viewer_token};

mod account;
mod agent;
mod bridge;
mod client;
mod completions;
#[cfg(feature = "debug-cmds")]
mod debug;
mod envfile;
mod guide;
mod mcp_bridge;
mod procgroup;
mod remote;
mod schedule;
mod service;
mod service_cmd;
mod setup;
mod status;
mod task;
mod update;
mod video;
mod viewer;
mod viewer_cmd;

#[derive(Parser)]
#[command(name = "iphone-control", version, about = ictl_l10n::tr!("在 Linux 或 macOS 上控制 iPhone，并提供 MCP 工具", "Control an iPhone from Linux or macOS, with MCP tools"))]
struct Cli {
    #[arg(help = ictl_l10n::tr!("数据根目录（账号、描述文件、日志等）。默认：root 是 /var/lib/iphone-control，普通用户是 ~/.local/share/iphone-control", "Data directory (account, provisioning profiles, logs, ...). Default: /var/lib/iphone-control for root, ~/.local/share/iphone-control for a regular user"))]
    #[arg(long, env = ictl_daemon::protocol::env::DATA_DIR, default_value_os_t = protocol::default_data_dir())]
    data_dir: PathBuf,
    #[arg(help = ictl_l10n::tr!("守护进程控制接口。默认：root 是 /run/iphone-control/ctl.sock，普通用户是 $XDG_RUNTIME_DIR/iphone-control/ctl.sock", "Daemon control socket. Default: /run/iphone-control/ctl.sock for root, $XDG_RUNTIME_DIR/iphone-control/ctl.sock for a regular user"))]
    #[arg(long, env = ictl_daemon::protocol::env::SOCKET, default_value_os_t = protocol::default_socket())]
    socket: PathBuf,
    /// 不给就是首次运行引导（设置好以后显示状态）
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = ictl_l10n::tr!("生成 Bash、Zsh 或 Fish 补全脚本到标准输出（不读取设置或连接设备）", "Generate Bash, Zsh or Fish completions on stdout (without loading settings or connecting to devices)"))]
    #[command(after_help = ictl_l10n::tr!("用法示例：iphone-control completions bash > iphone-control.bash\n通过 IPHONE_CONTROL_LANG=en 或 zh 选择补全说明的语言。升级后重新生成脚本。", "Example: iphone-control completions bash > iphone-control.bash\nSet IPHONE_CONTROL_LANG=en or zh to choose completion descriptions. Regenerate after upgrading."))]
    Completions {
        #[arg(value_enum, help = ictl_l10n::tr!("要生成补全脚本的 shell", "Shell to generate completions for"))]
        shell: completions::Shell,
    },
    #[command(about = ictl_l10n::tr!("检查主机软件版本，有新版时升级", "Check for a new host software release and install it if available"))]
    Update {
        #[arg(short, long, help = ictl_l10n::tr!("不询问，直接升级（脚本里或没有终端时用）", "Upgrade without asking (for scripts, or without a terminal)"))]
        yes: bool,
        #[command(subcommand)]
        action: Option<UpdateAction>,
    },
    #[command(about = ictl_l10n::tr!("常驻守护进程（安装时装成用户服务：macOS 是 launchd，Linux 是 systemd；也可以手动启动）", "The long-running daemon (installed as a user service: launchd on macOS, systemd on Linux; can also be started by hand)"))]
    Daemon,
    #[command(about = ictl_l10n::tr!("MCP stdio 服务（由 agent 拉起）。--url（或 IPHONE_CONTROL_URL）时转接另一台机器的控制台 /mcp：给只能填命令的 MCP 客户端用", "MCP stdio server (started by the agent). With --url (or IPHONE_CONTROL_URL), a bridge to another machine's viewer /mcp, for MCP clients that only start a command"))]
    Mcp {
        #[arg(long, value_name = "URL", env = ictl_daemon::protocol::env::URL, help = ictl_l10n::tr!("手机接在另一台机器上：那台机器的控制台地址。有任务口令（IPHONE_CONTROL_TASK_TOKEN）就只带它，否则用 IPHONE_CONTROL_VIEWER_TOKEN 登录；task run --url 启动的命令会自动设好", "The phone is on another machine: that machine's viewer. With a task's token (IPHONE_CONTROL_TASK_TOKEN) only that is sent, otherwise the login from IPHONE_CONTROL_VIEWER_TOKEN; a command under task run --url has it set already"))]
        url: Option<String>,
        #[arg(long, value_name = "PEM", env = ictl_daemon::protocol::env::CA, help = ictl_l10n::tr!("--url 是本机生成的自签证书时，信任这张证书", "With --url and a self-signed certificate: trust it"))]
        ca: Option<PathBuf>,
        /// The task's token, from this file instead of IPHONE_CONTROL_TASK_TOKEN: the control page starts its agent's
        /// MCP server this way, so the token isn't on a command line every user can read
        #[arg(long, value_name = "FILE", hide = true, conflicts_with = "url")]
        task_token_file: Option<PathBuf>,
    },
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
    #[command(about = ictl_l10n::tr!("解除一台设备的自动解锁停用（输错或中途出错后停用，只停用出错的那台）。只删停用标记，不核对密码：先确认保存的密码对，必要时 passcode set 重设", "Re-enable auto-unlock for one device after it was disabled there (by a wrong passcode or an error midway; only that device is disabled). Only removes the flag, doesn't check the passcode: make sure the saved one is right first, or set it again with passcode set"))]
    UnlockReset {
        #[arg(help = ictl_l10n::tr!("哪台设备（UDID 或设备名）；不给就是守护进程正在控制的那台", "Which device (UDID or name); defaults to the one the daemon controls"))]
        #[arg(long)]
        device: Option<String>,
    },
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
    #[command(about = ictl_l10n::tr!("浏览器里的控制台：实时画面上直接操作手机、看设备状态，把任务交给本机的 Claude Code 或 Codex 执行。默认只开在 127.0.0.1（--lan 也对局域网开放，走 HTTPS），要用令牌登录（iphone-control viewer token），操作都经守护进程（不开控制台时不监听任何端口）", "Control page in a browser: act on the live screen, see the device's state, and hand tasks to a local Claude Code or Codex. Serves 127.0.0.1 by default (--lan opens it to the local network too, over HTTPS), behind a login token (iphone-control viewer token); every action goes through the daemon (without the viewer no port is opened)"))]
    Viewer {
        #[arg(long, env = ictl_daemon::protocol::env::VIEWER_PORT, default_value_t = 8200)]
        port: u16,
        // from the settings file as 1/0, yes/no, on/off or true/false (a plain flag's parser only takes true/false)
        #[arg(long, env = ictl_daemon::protocol::env::VIEWER_LAN, value_parser = clap::builder::BoolishValueParser::new(), help = ictl_l10n::tr!("也对局域网开放（同一个端口，局域网里用 HTTPS，要令牌登录：iphone-control viewer token），只接受局域网地址来的连接", "Also open to the local network (same port, HTTPS there, login with the token: iphone-control viewer token); only connections from local-network addresses"))]
        lan: bool,
        #[arg(long = "lan-host", value_name = "HOST", env = ictl_daemon::protocol::env::VIEWER_LAN_HOSTS, value_delimiter = ',', help = ictl_l10n::tr!("--lan 时还认可的主机名（可写多次）；本机主机名、<主机名>.local 和各网卡 IP 已自动认可", "With --lan, another host name the page answers to (repeatable); this machine's name, <name>.local and its addresses are already accepted"))]
        lan_hosts: Vec<String>,
        #[arg(long, value_enum, env = ictl_daemon::protocol::env::VIEWER_TLS, default_value = "auto", help = ictl_l10n::tr!("HTTPS（同一个端口，本机仍可用 http://127.0.0.1）：auto 在 --lan 或给了证书时开启；on、off", "HTTPS (same port; http://127.0.0.1 still works on this machine): auto turns it on with --lan or a certificate; on, off"))]
        tls: viewer::tls::Mode,
        #[arg(long = "tls-cert", value_name = "PEM", env = ictl_daemon::protocol::env::VIEWER_TLS_CERT, requires = "tls_key", help = ictl_l10n::tr!("自己的证书（PEM，可带证书链），比如 mkcert 或 tailscale cert 生成的；不给就用本机生成的自签证书", "Your own certificate (PEM, chain allowed), such as one from mkcert or tailscale cert; without it a self-signed one made on this machine"))]
        tls_cert: Option<PathBuf>,
        #[arg(long = "tls-key", value_name = "PEM", env = ictl_daemon::protocol::env::VIEWER_TLS_KEY, requires = "tls_cert", help = ictl_l10n::tr!("--tls-cert 的私钥（PEM）", "The private key of --tls-cert (PEM)"))]
        tls_key: Option<PathBuf>,
        #[arg(long, env = ictl_daemon::protocol::env::VIEWER_MCP, value_name = "on|off", default_value = "on", action = clap::ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new(), help = ictl_l10n::tr!("同一个端口上的 MCP（HTTP）：/mcp，本机和（--lan 时）局域网的客户端用 URL 接入；off 关掉", "MCP over HTTP on the same port: /mcp, for clients on this machine and (with --lan) the local network that connect by URL; off turns it off"))]
        mcp: bool,
        #[command(subcommand)]
        action: Option<ViewerAction>,
    },
    #[command(about = ictl_l10n::tr!("AI 任务：在任务里运行外部 agent（task run），查看、暂停、接管、停止、确认任务", "AI tasks: run an external agent in a task (task run); list, pause, take over, stop and confirm tasks"))]
    Task {
        #[arg(long, global = true, value_name = "URL", env = ictl_daemon::protocol::env::URL, help = ictl_l10n::tr!("手机接在另一台机器上：那台机器的控制台地址，比如 https://192.168.1.5:8200；登录令牌放在 IPHONE_CONTROL_VIEWER_TOKEN 里（task finish 用任务口令）", "The phone is on another machine: that machine's viewer, such as https://192.168.1.5:8200; the login token goes in IPHONE_CONTROL_VIEWER_TOKEN (task finish uses the task's token)"))]
        url: Option<String>,
        #[arg(long, global = true, value_name = "PEM", env = ictl_daemon::protocol::env::CA, help = ictl_l10n::tr!("--url 是本机生成的自签证书时，信任这张证书（那台机器上的 viewer-tls/cert.pem）", "With --url and a self-signed certificate made there: trust it (viewer-tls/cert.pem on that machine)"))]
        ca: Option<PathBuf>,
        #[command(subcommand)]
        action: task::TaskAction,
    },
    #[command(about = ictl_l10n::tr!("真机诊断（开发用，要用 `--features debug-cmds` 编译）。`wda`/`e2e`/`hid` 会和守护进程抢 XCTest、HID，先停掉守护进程", "Real-device diagnostics (for development, build with `--features debug-cmds`). `wda`/`e2e`/`hid` compete with the daemon for XCTest and HID, so stop the daemon first"))]
    #[cfg(feature = "debug-cmds")]
    #[command(hide = true)]
    Debug(debug::DebugArgs),
    #[command(about = ictl_l10n::tr!("按已配置的渠道发测试通知：每类事件各一条（标题带「[测试]」），或只发指定的一类", "Send test notifications over the configured channels: one per event type (title starts with “[test]”), or only the given one"))]
    NotifyTest {
        #[arg(help = ictl_l10n::tr!("resign / session / pin / pin-recovered / unlock / offline / online / cert / daemon；不给就全发", "resign / session / pin / pin-recovered / unlock / offline / online / cert / daemon; all if not given"))]
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
    #[command(about = ictl_l10n::tr!("停止并移除本程序装的后台服务（数据保留），后台启动的 WDA 也一并结束", "Stop and remove the service this program set up (data is kept), and the WDA launched in the background with it"))]
    Uninstall,
    Start,
    #[command(about = ictl_l10n::tr!("停止后台服务；守护进程在后台启动的 WDA 也一并结束（restart 和升级不会）", "Stop the service, and the WDA the daemon launched in the background with it (restart and updates leave it running)"))]
    Stop,
    #[command(about = ictl_l10n::tr!("重启，改过 ~/.config/iphone-control/env 后用", "Restart, after editing ~/.config/iphone-control/env"))]
    Restart,
    Status,
    #[command(about = ictl_l10n::tr!("控制台（viewer）的后台服务：install / uninstall / start / stop / restart。设置在 ~/.config/iphone-control/env 的 IPHONE_CONTROL_VIEWER_*", "The viewer's own service: install / uninstall / start / stop / restart. Settings: IPHONE_CONTROL_VIEWER_* in ~/.config/iphone-control/env"))]
    Viewer {
        #[command(subcommand)]
        action: ViewerServiceAction,
    },
}

#[derive(Subcommand)]
enum ViewerAction {
    #[command(about = ictl_l10n::tr!("控制台的登录令牌。不带子命令：显示 owner 令牌、点开就能登录的链接和 HTTP MCP 的接入方式（--new 换一个新的）；add / list / show / remove 管理有名字的令牌，可以是只读的", "The viewer's login tokens. Alone: show the owner token, links that log in and how to connect to its HTTP MCP (--new replaces it); add / list / show / remove manage named tokens, which may be read-only"))]
    Token {
        #[command(subcommand)]
        action: Option<TokenAction>,
        #[arg(long, help = ictl_l10n::tr!("换一个新的 owner 令牌（用它登录的浏览器都要重新登录）", "Replace the owner token (browsers logged in with it log in again)"))]
        new: bool,
    },
}

#[derive(Subcommand)]
enum TokenAction {
    #[command(about = ictl_l10n::tr!("加一个有名字的令牌（比如每人一个、每台设备一个），显示它和登录链接；--read-only 只能看画面、设备状态和任务，不能操作", "Add a named token (one per person or device, say) and show it with links that log in; --read-only can watch the picture, the device's state and tasks but not act"))]
    Add {
        name: String,
        #[arg(long)]
        read_only: bool,
    },
    #[command(about = ictl_l10n::tr!("列出令牌的名字和权限（不显示令牌本身）", "List the tokens' names and roles (not the tokens)"))]
    List,
    #[command(about = ictl_l10n::tr!("显示一个令牌和它的登录链接；--new 换一个新的", "Show a token and links that log in with it; --new replaces it"))]
    Show {
        name: String,
        #[arg(long)]
        new: bool,
    },
    #[command(about = ictl_l10n::tr!("删掉一个令牌，用它登录的浏览器和程序马上进不来；最后一个能操作的令牌不能删", "Remove a token: browsers and programs logged in with it are out at once; the last token that may operate stays"))]
    Remove { name: String },
}

#[derive(Subcommand)]
enum ViewerServiceAction {
    #[command(about = ictl_l10n::tr!("装成登录后自动启动的服务并启动；会记下当前终端的 PATH（找 claude、codex 用），PATH 变了再装一次", "Set up the service that starts at login and start it; it keeps this shell's PATH (to find claude and codex), so install again if that changes"))]
    Install,
    Uninstall,
    Start,
    Stop,
    #[command(about = ictl_l10n::tr!("重启，改过 IPHONE_CONTROL_VIEWER_* 设置后用", "Restart, after changing the IPHONE_CONTROL_VIEWER_* settings"))]
    Restart,
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
    // 静态补全必须在读含密钥的设置、创建线程和维护锁之前返回。
    if completions::requested(std::env::args_os().skip(1)) {
        let cli = Cli::parse();
        if let Some(Command::Completions { shell }) = cli.command {
            return completions::write(shell, &mut std::io::stdout().lock());
        }
    }
    // 设置文件先读进环境（命令行上已设的优先），要在启动任何线程之前：语言、代理、通知等对所有命令都生效。
    // 控制台给它的 agent 起的 MCP 服务（`mcp --task-token-file`）只连本机：设置文件里给自己的命令连别处用的地址不读，
    // 读了会和这个参数冲突、服务起不来
    let local_only = mcp_with_token_file(std::env::args_os().skip(1));
    let skip: &[&str] =
        if local_only { &[ictl_daemon::protocol::env::URL, ictl_daemon::protocol::env::CA] } else { &[] };
    envfile::load_into_process(skip);
    tokio::runtime::Runtime::new()?.block_on(run())
}

/// `mcp --task-token-file FILE` (or `=FILE`): read off the raw arguments, before clap (which caches the language,
/// known only once the settings are loaded). Not taken for an argument of another command that happens to read the
/// same (a prompt of `task run`), and arguments that aren't UTF-8 are read too
fn mcp_with_token_file(args: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    let mut args = args.into_iter();
    if completions::command(&mut args).is_none_or(|c| c != "mcp") {
        return false;
    }
    while let Some(arg) = args.next() {
        if arg == "--task-token-file" || arg.as_encoded_bytes().starts_with(b"--task-token-file=") {
            return true;
        }
        if arg == "--" {
            return false;
        }
        // an option's value isn't an option (`--ca --task-token-file` is a certificate of that name)
        if arg == "--url" || arg == "--ca" {
            args.next();
        }
    }
    false
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
                | Command::UnlockReset { .. }
                | Command::NotifyTest { .. }
                | Command::Update { action: Some(UpdateAction::Check { .. }), .. }
        )
    ) || matches!(command, Some(Command::Task { action, .. }) if task::prints_and_exits(action))
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

/// The task's token from `mcp --task-token-file`. Given the file, there must be a token in it: without one the server
/// would run outside the task (no scope, no pause, no confirmation), so it doesn't start at all
fn read_task_token(f: &std::path::Path) -> anyhow::Result<String> {
    let t = std::fs::read_to_string(f).map_err(|e| anyhow::anyhow!("{}: {e}", f.display()))?;
    let t = t.trim();
    anyhow::ensure!(
        !t.is_empty(),
        "{}: {}",
        f.display(),
        ictl_l10n::tr!("任务口令文件是空的", "the task token file is empty")
    );
    Ok(t.to_owned())
}

async fn run() -> anyhow::Result<()> {
    // 依赖树里 rustls 的 ring 和 aws-lc-rs 都开着，不装默认 provider 的话第一个 HTTPS 请求就 panic
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut cli = Cli::parse();
    if prints_and_exits(cli.command.as_ref()) {
        restore_sigpipe();
    }
    let quiet = matches!(cli.command, Some(Command::Mcp { .. }));
    // ICTL_LOG 留空（配置文件里常见的 `ICTL_LOG=`）当作没设；原来空值会生成一个什么都不放行的过滤器
    let filter = env_nonempty(ictl_daemon::protocol::env::LOG)
        .and_then(|v| tracing_subscriber::EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| {
            tracing_subscriber::EnvFilter::new(if quiet { "warn" } else { "info,idevice=warn,apple_codesign=warn" })
        });
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(filter)
        .init();

    // Lifetime clients hold this through pauses and confirmations, and direct-device CLI
    // commands participate even when they bypass the daemon. The MCP server, which agents keep running for hours,
    // takes it for each tool call instead (`ictl_mcp::serve_stdio`); over --url it talks to another machine.
    let _client = match &cli.command {
        Some(
            Command::Daemon
            | Command::Update { .. }
            | Command::McpConfig
            | Command::Mcp { .. }
            | Command::Status
            | Command::Completions { .. },
        ) => None,
        _ => Some(ictl_daemon::maintenance::shared()?),
    };
    let Some(command) = cli.command.take() else { return guide::first_run(&cli).await };
    match command {
        Command::Completions { shell } => completions::write(shell, &mut std::io::stdout().lock()),
        Command::Update { action: None, yes } => update::upgrade(&cli.socket, update::check(false).await?, yes).await,
        Command::Update { action: Some(action), .. } => update_command(action, &cli.socket).await,
        Command::Daemon => {
            let mut cfg = ictl_daemon::Config::from_env(&cli.data_dir);
            cfg.socket = cli.socket;
            ictl_daemon::run(cfg).await
        }
        Command::Mcp { url: Some(url), ca, .. } => mcp_bridge::run(&url, ca.as_deref()).await,
        Command::Mcp { url: None, task_token_file, .. } => {
            let token = task_token_file.as_deref().map(read_task_token).transpose()?;
            ictl_mcp::serve_stdio(cli.socket, Some(ictl_daemon::maintenance::shared), token).await
        }
        Command::McpConfig => {
            println!("{}", mcp_config(&cli.socket)?);
            Ok(())
        }
        Command::Status => status(&cli.socket).await,
        Command::Doctor => print_text(&cli.socket, Request::Doctor).await,
        Command::Setup { reconfigure } => guide::setup(&cli, reconfigure).await,
        Command::Service { action } => service_cmd(&cli.socket, &cli.data_dir, action).await,
        Command::Login { email } => login(&cli.data_dir, email).await,
        Command::Sign => print_text(&cli.socket, Request::Resign { force: true }).await,
        Command::Resign { force } => print_text(&cli.socket, Request::Resign { force }).await,
        Command::Passcode { action } => passcode(&cli.data_dir, &cli.socket, action).await,
        Command::UnlockReset { device } => {
            // a device named by the user is looked up here, like passcode --device; none: the daemon's own
            let udid = match device {
                Some(q) => Some(client::target_device(&cli.socket, Some(&q)).await?.udid),
                None => None,
            };
            print_text(&cli.socket, Request::UnlockResetDevice { udid }).await
        }
        Command::Devices => print_text(&cli.socket, Request::ListDevices).await,
        Command::Select { device } => print_text(&cli.socket, Request::SelectDevice { device }).await,
        Command::WdaBridge { port } => wda_bridge(&cli.socket, port).await,
        Command::Viewer { port, lan, lan_hosts, tls, tls_cert, tls_key, mcp, action } => {
            let tls = viewer::tls::Setting { mode: tls, cert: tls_cert, key: tls_key };
            match action {
                Some(ViewerAction::Token { action, new }) => {
                    let at = ViewerAt { port, lan, lan_hosts: &lan_hosts, tls: &tls, mcp };
                    viewer_token(&cli.data_dir, &at, action, new)
                }
                None => viewer::run(&cli.socket, &cli.data_dir, port, lan, &lan_hosts, &tls, mcp).await,
            }
        }
        Command::Task { url, ca, action } => task::run(&cli.socket, url, ca, action).await,
        #[cfg(feature = "debug-cmds")]
        Command::Debug(args) => debug::run(env_nonempty(ictl_daemon::protocol::env::UDID).as_deref(), args).await,
        Command::NotifyTest { event } => notify_test(&cli.socket, &cli.data_dir, event.as_deref()).await,
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
                bail!("{}", ictl_l10n::trf!("检查更新失败：{code}", "Update check failed: {code}"))
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

/// MCP 进程由客户端直接拉起本程序：显式带上 socket，免得 Claude Code 的环境里没有 XDG_RUNTIME_DIR 时算出别的位置。
/// 英文、开发者工具开关写进注册命令：MCP 客户端不带用户 shell 的环境
fn mcp_config(socket: &Path) -> anyhow::Result<String> {
    Ok(mcp_command(&service::stable_exe()?, socket))
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

#[cfg(test)]
mod tests {
    use super::mcp_command;

    /// The viewer's settings come from the settings file as environment variables (also for the viewer service):
    /// `IPHONE_CONTROL_VIEWER_LAN=1` turns `--lan` on. A plain flag only took true/false, so the service didn't start
    /// `--url` anywhere in `task …`, or from IPHONE_CONTROL_URL (which `task run --url` gives its command), and for
    /// `mcp`
    /// Given a token file, the server works for that task or doesn't start: an empty one isn't "no task"
    #[test]
    fn an_empty_task_token_file_is_refused() {
        let dir = std::env::temp_dir().join(format!("ictl-token-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.token");
        for (content, ok) in [("", false), (" \n\t", false), ("abc\n", true)] {
            std::fs::write(&f, content).unwrap();
            assert_eq!(super::read_task_token(&f).ok().as_deref(), ok.then_some("abc"), "{content:?}");
        }
        assert!(super::read_task_token(&dir.join("missing")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_remote_viewer_from_a_flag_or_the_environment() {
        use clap::Parser;
        let task = |args: &[&str]| match super::Cli::try_parse_from(args).unwrap().command {
            Some(super::Command::Task { url, .. }) => url,
            _ => panic!("not task"),
        };
        let mcp = |args: &[&str]| match super::Cli::try_parse_from(args).unwrap().command {
            Some(super::Command::Mcp { url, .. }) => url,
            _ => panic!("not mcp"),
        };
        // SAFETY: only this test sets this variable
        unsafe { std::env::remove_var(ictl_daemon::protocol::env::URL) };
        assert_eq!(task(&["i", "task", "pause", "abc"]), None);
        assert_eq!(mcp(&["i", "mcp"]), None, "the local stdio server, as before");
        assert_eq!(task(&["i", "task", "--url", "https://a:1", "pause", "abc"]).as_deref(), Some("https://a:1"));
        assert_eq!(task(&["i", "task", "approve", "abc", "--url", "https://a:1"]).as_deref(), Some("https://a:1"));
        assert_eq!(mcp(&["i", "mcp", "--url", "https://a:1"]).as_deref(), Some("https://a:1"));
        // the token file is for the local server only: never silently dropped for a bridge
        let both = ["i", "mcp", "--url", "https://a:1", "--task-token-file", "/x/t.token"];
        assert!(super::Cli::try_parse_from(both).is_err());
        assert!(super::Cli::try_parse_from(["i", "mcp", "--task-token-file", "/x/t.token"]).is_ok());
        unsafe { std::env::set_var(ictl_daemon::protocol::env::URL, "https://b:2") };
        assert!(
            super::Cli::try_parse_from(["i", "mcp", "--task-token-file", "/x/t.token"]).is_err(),
            "nor for one from the environment"
        );
        assert_eq!(task(&["i", "task", "finish", "done"]).as_deref(), Some("https://b:2"));
        assert_eq!(mcp(&["i", "mcp"]).as_deref(), Some("https://b:2"));
        unsafe { std::env::remove_var(ictl_daemon::protocol::env::URL) };
    }

    #[test]
    fn viewer_settings_from_the_environment() {
        use clap::Parser;
        let parse = |lan: Option<&str>| {
            // SAFETY: only this test sets these variables
            unsafe {
                match lan {
                    Some(v) => std::env::set_var(ictl_daemon::protocol::env::VIEWER_LAN, v),
                    None => std::env::remove_var(ictl_daemon::protocol::env::VIEWER_LAN),
                }
                std::env::set_var(ictl_daemon::protocol::env::VIEWER_PORT, "8202");
                std::env::set_var(ictl_daemon::protocol::env::VIEWER_LAN_HOSTS, "nas.example,nas2");
            }
            match super::Cli::try_parse_from(["iphone-control", "viewer"]).map(|c| c.command) {
                Ok(Some(super::Command::Viewer { port, lan, lan_hosts, action: None, .. })) => (port, lan, lan_hosts),
                other => panic!("{lan:?}: {:?}", other.err()),
            }
        };
        for v in ["1", "yes", "on", "true"] {
            assert!(parse(Some(v)).1, "{v}");
        }
        for v in ["0", "no", "off", "false"] {
            assert!(!parse(Some(v)).1, "{v}");
        }
        assert_eq!(parse(None), (8202, false, vec!["nas.example".to_owned(), "nas2".to_owned()]));
        unsafe {
            std::env::remove_var(ictl_daemon::protocol::env::VIEWER_PORT);
            std::env::remove_var(ictl_daemon::protocol::env::VIEWER_LAN_HOSTS);
        }
    }
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

    /// Only `mcp`'s own option, in either form, after any root options; not a prompt that reads the same, and an
    /// argument that isn't UTF-8 doesn't stop it
    #[test]
    fn the_token_file_option_is_found_only_for_mcp() {
        use super::mcp_with_token_file;
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let found = |args: &[&str]| mcp_with_token_file(args.iter().map(OsString::from));
        assert!(found(&["mcp", "--task-token-file", "/t"]));
        assert!(found(&["mcp", "--task-token-file=/t"]));
        assert!(found(&["--socket", "/s.sock", "--data-dir=/d", "mcp", "--task-token-file", "/t"]));
        assert!(!found(&["mcp"]));
        assert!(!found(&["task", "run", "--", "echo", "--task-token-file"]), "another command's argument");
        assert!(!found(&["--socket", "mcp", "status", "--task-token-file"]), "a root option's value isn't the command");
        assert!(!found(&["mcp", "--ca", "--task-token-file"]), "an option's value");
        let odd =
            vec![OsString::from("mcp"), OsString::from_vec(vec![0xff, 0xfe]), OsString::from("--task-token-file=/t")];
        assert!(mcp_with_token_file(odd));
    }
}
