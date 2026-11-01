//! `iphone-control doctor`：逐层诊断，每项失败都给出修复办法。

use std::path::Path;
use std::time::Duration;

use crate::core::{Daemon, Inner};
use crate::supervise::SYSFS_USB;

impl Daemon {
    pub(crate) async fn doctor(&self, g: &mut Inner) -> String {
        let mut out: Vec<String> = Vec::new();
        let mut fails = 0;

        out.push("== 设备连接".into());
        match ictl_device::list_devices().await {
            Ok(_) => out.push("✅ usbmuxd 在运行".into()),
            Err(e) => {
                out.push(format!(
                    "❌ 连不上 usbmuxd：{e}\n     → {}",
                    if self.cfg.manage_usbmuxd {
                        "由守护进程自动拉起，看数据目录 logs/usbmuxd.log；宿主机已有 usbmuxd 服务时设 ICTL_MANAGE_USBMUXD=0"
                    } else {
                        "用的是系统的 usbmuxd 服务（插上手机时由 udev 拉起）：确认已安装 usbmuxd、手机已插好"
                    }
                ));
                fails += 1;
            }
        }
        if ictl_device::apple_usb_present(Path::new(SYSFS_USB)) {
            out.push("✅ USB 上有 Apple 设备".into());
        } else {
            out.push("❌ USB 上没有 Apple 设备\n     → 插好数据线;用 USB-C 转接头的话换一根试试".into());
            fails += 1;
        }
        match (&g.device, &g.info) {
            (Some(d), info) => {
                let i = info.clone().unwrap_or_default();
                out.push(format!("✅ 已连接 {} {} iOS {}", d.udid, i.product_type, i.product_version));
                let major = i.major_version();
                if major > 0 && major < 17 {
                    out.push("⚠️  iOS 低于 17  未在此版本测试过".into());
                } else if major > 0 && major < 27 {
                    out.push("⚠️  iOS 低于 27  HID 后备路线(免签名截图/点击)不可用,只能走 WDA".into());
                }
            }
            (None, _) => {
                out.push(
                    if self.cfg.manage_usbmuxd {
                        "❌ 守护进程看不到手机\n     → 等 30 秒,会自动重启 usbmuxd;仍不行就重启容器(或守护进程)"
                    } else {
                        "❌ 守护进程看不到手机\n     → 重新插拔数据线;仍不行就 sudo systemctl restart usbmuxd"
                    }
                    .into(),
                );
                fails += 1;
            }
        }
        out.push(format!("   状态：{}", g.state.describe()));

        out.push("== Apple ID 与签名".into());
        match self.account().selected_email().await {
            Ok(_) => out.push("✅ 已登录 Apple ID".into()),
            Err(_) => {
                out.push("❌ 没有登录 Apple ID\n     → iphone-control login".into());
                fails += 1;
            }
        }
        match (&g.persisted.wda_bundle, g.persisted.profile_left()) {
            (Some(b), Some(left)) => {
                out.push(format!("✅ WDA 已签名 {b}"));
                if left.is_zero() {
                    out.push("❌ 描述文件已过期\n     → iphone-control resign --force".into());
                    fails += 1;
                } else if left < Duration::from_secs(48 * 3600) {
                    out.push(format!("⚠️  描述文件只剩 {} 小时  守护进程会自动重签", left.as_secs() / 3600));
                } else {
                    out.push(format!("✅ 描述文件有效  还剩 {} 天", left.as_secs() / 86400));
                }
            }
            _ => {
                out.push("❌ WDA 还没签名安装\n     → iphone-control sign".into());
                fails += 1;
            }
        }
        if let Some(r) = &g.persisted.last_resign {
            out.push(format!("   上次重签：{r}"));
        }

        out.push("== WDA".into());
        match self.wda(g) {
            Some(w) if w.ready().await.unwrap_or(false) => {
                out.push("✅ WDA 就绪（经 usbmuxd 直连；守护进程不监听端口，wda-bridge 运行时除外）".into())
            }
            _ => {
                out.push(format!("❌ WDA 不可用：{}", g.state.describe()));
                fails += 1;
            }
        }

        out.push("== 附加功能".into());
        if self.cfg.notify.any() {
            out.push("✅ 通知已配置".into());
        } else {
            out.push("⚠️  没有配置通知  重签失败、手机离线、需要输密码时不会提醒你".into());
        }
        let unlock = self.unlock_cfg();
        if unlock.disabled_by_failure() {
            out.push(
                "❌ 自动解锁已停用(之前输错过或输入中途出错)\n     → 核对密码后 iphone-control unlock-reset".into(),
            );
            fails += 1;
        } else if unlock.enabled() {
            out.push("✅ 自动解锁已启用".into());
        } else {
            out.push("⚠️  自动解锁未启用  手机锁屏时需要人工解锁(可选:iphone-control passcode set)".into());
        }
        out.push(String::new());
        out.push(if fails == 0 { "全部正常".into() } else { format!("{fails} 项需要处理") });
        out.join("\n")
    }
}
