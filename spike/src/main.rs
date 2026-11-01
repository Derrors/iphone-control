//! 第 0 步真机验证（docs/spike-plan.md）。每个子命令验证一项，可独立重跑，结果记到 spike/RESULTS.md。
//! 运行前须停掉 toolkit 容器；`wda` 会中断 toolkit 的 WDA，`ddi --force-uninstall` 会卸载手机上的开发者镜像，都须先征得用户同意。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use ictl_device::{Button, Device, ScreenSize};

#[derive(Parser)]
struct Cli {
    #[arg(long, env = "IPHONE_UDID")]
    udid: Option<String>,
    /// 屏幕点尺寸（HID 坐标换算用）
    #[arg(long, default_value_t = 430.0)]
    width: f64,
    #[arg(long, default_value_t = 932.0)]
    height: f64,
    #[command(subcommand)]
    check: Check,
}

#[derive(Subcommand)]
enum Check {
    /// 1. 开发者镜像：已挂载就报告版本；否则用 restore_dir（缺文件会下载）个性化 + TSS 并挂载
    Ddi {
        restore_dir: PathBuf,
        #[arg(long, default_value = ictl_device::ddi::DEFAULT_BASE_URL)]
        url: String,
    },
    /// 诊断：逐个调用 cryptexd 只读例程（不改设备状态）
    CryptexdProbe,
    /// 诊断：分步挂载开发者镜像（不依赖 copy-installed），挂完用截图服务确认
    DdiSteps { restore_dir: PathBuf },
    /// 2. HID：媒体流 + universalhidservice tap，再按 Home（以截图对比为准，调用成功不代表生效）
    Hid { x: f64, y: f64 },
    /// 3. 截图：screencaptureservice PNG
    Screenshot { out: PathBuf },
    /// 4. 隧道稳定性：同一条隧道持续截图 N 分钟
    Soak {
        #[arg(long, default_value_t = 30)]
        minutes: u64,
    },
    /// 5. 启动 WDA：读 /status、/source、/screenshot，最后停掉 runner
    Wda { bundle_id: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let device = Device::find(cli.udid.as_deref()).await?;
    let info = device.info().await?;
    println!("手机 {} iOS {}", info.product_type, info.product_version);
    let screen = ScreenSize { width: cli.width, height: cli.height };
    match cli.check {
        Check::Ddi { restore_dir, url } => {
            let mut t = device.open_tunnel().await?;
            match t.ddi_mounted().await? {
                Some(v) => println!("开发者镜像已挂载 {v}（要测挂载流程，先在手机重启后的未挂载状态下运行）"),
                None => {
                    let dir = ictl_device::ddi::ensure_cache(&restore_dir, &url).await?;
                    let t0 = Instant::now();
                    let v = t.mount_ddi(&dir).await?;
                    println!("挂载成功 {v}，耗时 {:.1}s", t0.elapsed().as_secs_f64());
                }
            }
            let t = device.open_tunnel().await?;
            println!("universalhidservice: {}", t.has_service("com.apple.coredevice.hid.universalhidservice"));
        }
        Check::CryptexdProbe => {
            let mut t = device.open_tunnel().await?;
            for (routine, r) in t.probe_cryptexd(Duration::from_secs(20)).await {
                println!("{routine}: {r}");
            }
        }
        Check::DdiSteps { restore_dir } => {
            let mut t = device.open_tunnel().await?;
            let t0 = Instant::now();
            t.mount_ddi_steps(&restore_dir, Duration::from_secs(120)).await?;
            println!("挂载流程结束，总耗时 {:.1}s；用截图服务确认镜像是否生效", t0.elapsed().as_secs_f64());
            let mut t = device.open_tunnel().await?;
            println!(
                "screenshot: {}",
                match t.screenshot_png().await {
                    Ok(p) => format!("成功 {} 字节", p.len()),
                    Err(e) => format!("失败 {e}"),
                }
            );
        }
        Check::Hid { x, y } => {
            let mut t = device.open_tunnel().await?;
            let mut hid = t.hid(screen).await?;
            hid.tap(x, y).await?;
            println!("已点 ({x},{y})，3 秒后按 Home");
            tokio::time::sleep(Duration::from_secs(3)).await;
            hid.press(Button::Home).await?;
            hid.close().await;
            println!("完成：请以截图/肉眼确认是否真的生效");
        }
        Check::Screenshot { out } => {
            let mut t = device.open_tunnel().await?;
            let t0 = Instant::now();
            let png = t.screenshot_png().await?;
            std::fs::write(&out, &png)?;
            println!("{} 字节，耗时 {:.2}s → {}", png.len(), t0.elapsed().as_secs_f64(), out.display());
        }
        Check::Soak { minutes } => {
            let mut t = device.open_tunnel().await?;
            let end = Instant::now() + Duration::from_secs(minutes * 60);
            let (mut n, mut bytes, mut worst, mut fails) = (0u64, 0usize, 0f64, 0u64);
            while Instant::now() < end {
                let t0 = Instant::now();
                match t.screenshot_png().await {
                    Ok(p) => {
                        n += 1;
                        bytes += p.len();
                        worst = worst.max(t0.elapsed().as_secs_f64());
                    }
                    Err(e) => {
                        fails += 1;
                        eprintln!("截图失败：{e}，重建隧道");
                        t = device.open_tunnel().await?;
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            println!("{n} 次成功、{fails} 次失败，共 {:.1} MB，最慢 {worst:.2}s", bytes as f64 / 1e6);
        }
        Check::Wda { bundle_id } => {
            let t0 = Instant::now();
            let (runner, mut wda) =
                ictl_wda::WdaRunner::start(device.provider(), &bundle_id, Duration::from_secs(60)).await?;
            println!("WDA 就绪，耗时 {:.1}s", t0.elapsed().as_secs_f64());
            println!("屏幕 {:?}", wda.screen_size().await?);
            let t1 = Instant::now();
            let xml = wda.source_xml().await?;
            println!(
                "元素树 {} 字节，耗时 {:.2}s，解析出 {} 个元素",
                xml.len(),
                t1.elapsed().as_secs_f64(),
                wda.elements().await?.len()
            );
            let t2 = Instant::now();
            println!("截图 {} 字节，耗时 {:.2}s", wda.screenshot_png().await?.len(), t2.elapsed().as_secs_f64());
            runner.stop();
            println!("runner 已停止");
        }
    }
    Ok(())
}
