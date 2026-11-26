//! `iphone-control status` and `iphone-control notify-test`.

use std::path::Path;
use std::time::Duration;

use anyhow::bail;

use crate::client::snapshot;

pub(crate) async fn status(socket: &Path) -> anyhow::Result<()> {
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
    if s.native {
        // 原生控制用不到 WDA 和签名：不提示装 WDA（签名详情见 doctor）
        println!(
            "{}",
            ictl_l10n::tr!(
                "控制方式  原生（iOS 27 起不用 WDA；要改回 WDA，在 ~/.config/iphone-control/env 里设 ICTL_IOS_BACKEND=wda）",
                "Control   native (no WDA from iOS 27; to go back to WDA, set ICTL_IOS_BACKEND=wda in ~/.config/iphone-control/env)"
            )
        );
    } else {
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

pub(crate) async fn notify_test(socket: &Path, data_dir: &Path, only: Option<&str>) -> anyhow::Result<()> {
    use ictl_daemon::notify::{Event, Target, send};
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
    // the device the daemon controls, as a real notification names it; without the daemon, a sample
    let target = match tokio::time::timeout(Duration::from_secs(2), snapshot(socket)).await {
        Ok(Ok(s)) if s.device.is_some() => Target::of(s.device.as_ref()),
        _ => Target::sample(),
    };
    let mut failed = 0;
    for e in &events {
        let (title, body) = e.title_body(&target);
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
