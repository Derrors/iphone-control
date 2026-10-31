//! iphone-control：唯一的可执行程序。

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "iphone-control", version, about = "在 Linux 上控制 iPhone，并提供 MCP 工具")]
struct Cli {
    /// 数据根目录（配对记录、账号、描述文件、日志等）
    #[arg(long, env = "ICTL_DATA_DIR", default_value = "/var/lib/iphone-control")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 常驻守护进程（容器入口）
    Daemon,
    /// MCP stdio 服务（由 agent 拉起）
    Mcp,
    /// 逐层诊断：USB → usbmuxd → 配对 → 开发者模式 → 镜像 → 隧道 → WDA → 描述文件 → Apple ID 会话
    Doctor,
    /// 首次安装向导
    Setup,
    /// 交互式登录 Apple ID（密码 + 双重认证）
    Login { apple_id: String },
    /// 签名并安装 WDA
    Sign,
    /// 重签（默认剩余不足 2 天才执行）
    Resign {
        #[arg(long)]
        force: bool,
    },
    /// 查看守护进程和设备状态
    Status,
    /// 显式把 WDA 桥接到 127.0.0.1 供外部工具使用（默认不监听任何端口）
    WdaBridge {
        #[arg(long, default_value_t = 8100)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let cli = Cli::parse();
    let _ = &cli.data_dir;
    match cli.command {
        Command::Daemon => todo!("ictl_daemon::run"),
        Command::Mcp => todo!("ictl_mcp::serve_stdio"),
        Command::Doctor => todo!(),
        Command::Setup => todo!(),
        Command::Login { .. } => todo!("ictl_sign::Account::login"),
        Command::Sign => todo!(),
        Command::Resign { .. } => todo!(),
        Command::Status => todo!(),
        Command::WdaBridge { .. } => todo!(),
    }
}
