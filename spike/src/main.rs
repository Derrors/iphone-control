//! 第 0 步真机验证（docs/spike-plan.md）。每个子命令验证一项，可独立重跑。
//! 运行前须停掉 toolkit 容器；`wda` 会中断 toolkit 的 WDA，须先征得用户同意。

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    udid: Option<String>,
    #[command(subcommand)]
    check: Check,
}

#[derive(Subcommand)]
enum Check {
    /// 1. 开发者镜像：cryptexd::install_ddi（个性化 + TSS），再 installed_ddi 确认
    Ddi { restore_dir: PathBuf },
    /// 2. HID：媒体流 + universalhidservice tap/drag + indigo Home 键
    Hid { x: f64, y: f64 },
    /// 3. 截图：screencaptureservice PNG
    Screenshot { out: PathBuf },
    /// 4. 隧道稳定性：持续 N 分钟截图 + RSD 小请求
    Soak {
        #[arg(long, default_value_t = 30)]
        minutes: u64,
    },
    /// 5. 启动 WDA：run_until_wda_ready，直连设备端口读 /status、/source、/screenshot
    Wda { bundle_id: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();
    let _ = &cli.udid;
    match cli.check {
        Check::Ddi { .. } => todo!(),
        Check::Hid { .. } => todo!(),
        Check::Screenshot { .. } => todo!(),
        Check::Soak { .. } => todo!(),
        Check::Wda { .. } => todo!(),
    }
}
