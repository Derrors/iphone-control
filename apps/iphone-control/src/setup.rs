//! Guided phone setup: USB detection, pairing, Developer Mode, signing WDA or native iOS 27 readiness.

use std::path::Path;
use std::time::Duration;

use anyhow::bail;
use ictl_daemon::protocol::Request;

use crate::Cli;
use crate::account::login;
use crate::client::{account_dir, print_text, snapshot, target_device};

/// 每隔 `every` 检查一次，直到 `check` 返回 true 或超时
pub(crate) async fn wait_until<F, Fut>(timeout: Duration, every: Duration, mut check: F) -> bool
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
/// Needs the daemon running ([`crate::guide`] starts it first)
pub(crate) async fn device_setup(cli: &Cli) -> anyhow::Result<()> {
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

    // iOS 27 起默认原生控制：不用 WDA，也就不用 Apple ID 和签名（ICTL_IOS_BACKEND=wda 时照旧）
    let backend =
        ictl_daemon::config::IosBackend::parse(std::env::var(ictl_daemon::protocol::env::IOS_BACKEND).ok().as_deref());
    if backend.drives_natively(Some(&target)) {
        return native_setup(socket, &device.udid).await;
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

/// The last steps of [`device_setup`] for a phone driven natively: no Apple ID, no signing, the daemon only mounts
/// the developer disk image and connects to the accessibility service
async fn native_setup(socket: &Path, udid: &str) -> anyhow::Result<()> {
    println!(
        "{}",
        ictl_l10n::tr!(
            "[4/6] 登录 Apple ID：跳过（iOS 27 起原生控制，不用 WDA，不用签名）\n[5/6] 签名安装 WDA：跳过（要改回 WDA，在 ~/.config/iphone-control/env 里设 ICTL_IOS_BACKEND=wda）",
            "[4/6] Apple ID sign-in: skipped (native control from iOS 27, no WDA, no signing)\n[5/6] Signing and installing WDA: skipped (to go back to WDA, set ICTL_IOS_BACKEND=wda in ~/.config/iphone-control/env)"
        )
    );
    println!(
        "{}",
        ictl_l10n::tr!(
            "[6/6] 等待手机准备好（挂载开发者镜像、连接无障碍服务；手机要保持解锁）",
            "[6/6] Waiting for the phone to get ready (mounting the developer disk image, connecting to the accessibility service; keep the phone unlocked)"
        )
    );
    let ready = wait_until(Duration::from_secs(300), Duration::from_secs(3), || async {
        snapshot(socket).await.is_ok_and(|s| {
            s.state == ictl_daemon::machine::DeviceState::Ready && s.device.as_ref().is_none_or(|d| d.udid == udid)
        })
    })
    .await;
    println!(
        "  {}",
        if ready {
            ictl_l10n::tr!("就绪（原生控制）", "Ready (native control)")
        } else {
            ictl_l10n::tr!(
                "5 分钟内没准备好，运行 iphone-control doctor 看看卡在哪",
                "Not ready within 5 minutes; run iphone-control doctor to see where it is stuck"
            )
        }
    );
    Ok(())
}
