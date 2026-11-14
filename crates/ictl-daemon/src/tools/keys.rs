//! The hardware buttons: Home (WDA or HID, whichever works on the device) and locking the screen

use super::*;

impl Daemon {
    pub(crate) async fn press_button(&self, g: &mut Inner, button: &str) -> Result<String, String> {
        let hid_button = Button::parse(button).ok_or(ictl_l10n::tr!(
            "button 只能是 home / volume_up / volume_down / lock",
            "button must be home / volume_up / volume_down / lock"
        ))?;
        let wda_name = match hid_button {
            Button::Lock => return self.lock_screen(g).await,
            Button::Home => "home",
            Button::VolumeUp => "volumeUp",
            Button::VolumeDown => "volumeDown",
        };
        self.ensure_unlocked(g).await?;
        let xctest_first = g.dev.home_via_xctest;
        if let Some(w) = self.wda(g) {
            // Home 在系统弹窗（定位、通知权限等）挡着时不生效，WDA 却照样返回成功。弹窗属于 SpringBoard，
            // 看前台 App 发现不了，要单独查；按之前查（App 静止时约 0.4 秒，按下后动画中查要好几秒）
            if hid_button == Button::Home
                && let Ok(Some(text)) = w.alert_text().await
            {
                return Ok(ictl_l10n::trf!(
                    "{},按 Home 不会生效,没有按;{TAP_ALERT}",
                    "{}, pressing Home would have no effect, not pressed; {TAP_ALERT_EN}",
                    blocking(&text)
                ));
            }
            let mut home_worked = None;
            let r = if hid_button == Button::Home {
                press_home(w, xctest_first).await.map(|(text, worked)| {
                    home_worked = worked;
                    text
                })
            } else {
                w.press_button(wda_name).await.map(|()| ictl_l10n::trf!("已按 {button}", "Pressed {button}"))
            };
            if let Some(via_xctest) = home_worked {
                g.dev.home_via_xctest = via_xctest;
            }
            match r {
                Ok(text) => return Ok(text),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        self.hid_do(g, |h| Box::pin(h.press(hid_button))).await?;
        Ok(ictl_l10n::trf!("已按 {button}", "Pressed {button}"))
    }

    pub(super) async fn lock_screen(&self, g: &mut Inner) -> Result<String, String> {
        let via_wda = g.dev.lock_via_wda;
        // Ok(way)：用 way 锁上了；Err：WDA 两种办法没锁上的原因（WDA 不可用时为空），退到 HID
        let outcome = match self.wda(g) {
            Some(w) => {
                // 已经锁着就不按：熄屏时再按电源键会把屏幕点亮
                if matches!(w.locked().await, Ok(true)) {
                    return self.locked_now(g);
                }
                let mut failed = Vec::new();
                let mut worked = None;
                for way in lock_order(via_wda) {
                    let r = match way {
                        LockWay::PowerKey => lock_by_power_hid(w).await,
                        LockWay::Wda => lock_by_wda(w).await,
                    };
                    match r {
                        Ok(()) => {
                            worked = Some(way);
                            break;
                        }
                        Err(why) => failed.push(why),
                    }
                }
                worked.ok_or(failed)
            }
            None => Err(Vec::new()),
        };
        let way = match outcome {
            Ok(way) => Some(way),
            // WDA 在、只是没锁上：没有 HID 后备时如实说两种办法各自怎么了，不能说成「WDA 不可用」
            Err(failed) if !failed.is_empty() && !hid_supported(g.info.as_ref()) => {
                return Err(lock_failed(&failed));
            }
            Err(_) => None,
        };
        if let Some(way) = way {
            let wda = way == LockWay::Wda;
            if wda != via_wda {
                self.log.log(if wda {
                    ictl_l10n::log_tr!(
                        "这台设备上经 IOHID 短按电源键锁不了屏，之后锁屏先用 WDA 的",
                        "Pressing the power key via IOHID doesn't lock this device, WDA's lock goes first from now on"
                    )
                } else {
                    ictl_l10n::log_tr!(
                        "这台设备上 WDA 的锁屏不起作用，之后锁屏先经 IOHID 按电源键",
                        "WDA's lock doesn't work on this device, the power key via IOHID goes first from now on"
                    )
                });
            }
            g.dev.lock_via_wda = wda;
            return self.locked_now(g);
        }
        self.hid_do(g, |h| Box::pin(h.press(Button::Lock))).await?;
        self.locked_now(g)
    }

    pub(super) fn locked_now(&self, g: &mut Inner) -> Result<String, String> {
        g.dev.last_lock = Some(Instant::now());
        Ok(ictl_l10n::tr!("已锁屏", "Locked").into())
    }
}

/// 按 Home，等主屏真的到前台再说「已按」（紧接着查状态时前台不再是旧 App）。
/// 两种按法各有设备不认：XCTest 的 home 在 iPhone 12 mini / iOS 26.4.1 上不起作用却报成功；IOHID 的 AC Home
/// 在 iPhone 18 Pro Max / iOS 27.2.1 上基本不起作用（按住 0.05～0.5 秒都一样）。所以默认先 IOHID，
/// `xctest_first` 时先 XCTest；[`HOME_SETTLE`] 内没回到主屏（或这一下报错）再换另一种补一次。
/// 同时返回回到主屏的是不是 XCTest 那一下（都没回到为 `None`），调用方记下来，这台设备下次先用它。
/// 两种都没回到主屏时，再用第一种补按一次：App 刚到前台、转场动画没走完时 iPadOS 会把两下都忽略
/// （iPad mini 5 / iPadOS 26.6.1：启动 App 后马上按 Home，约三次有一次这样），转场完了再按就行
pub(super) async fn press_home(
    w: &mut WdaClient<DeviceTransport>,
    xctest_first: bool,
) -> Result<(String, Option<bool>), WdaError> {
    let mut app = String::new();
    let order = if xctest_first { [false, true, false] } else { [true, false, true] };
    for (i, via_hid) in order.into_iter().enumerate() {
        let last = i == order.len() - 1;
        let sent = if via_hid { w.press_home_hid().await } else { w.press_button("home").await };
        let (way, other) = if via_hid { ("IOHID", "XCTest") } else { ("XCTest", "IOHID") };
        match sent {
            Ok(()) => {}
            Err(e @ WdaError::Unavailable(_)) => return Err(e),
            Err(e) if !last => {
                tracing::info!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "经 {way} 按 Home 报错,改用 {other}：{e}",
                        "Pressing Home via {way} failed, using {other}: {e}"
                    )
                );
                continue;
            }
            Err(e) => return Err(e),
        }
        let deadline = Instant::now() + HOME_SETTLE;
        loop {
            app = w.active_app().await.unwrap_or_default();
            if app == SPRINGBOARD {
                return Ok((ictl_l10n::tr!("已按 home", "Pressed home").into(), Some(!via_hid)));
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        if !last {
            tracing::info!(
                "{}",
                ictl_l10n::log_trf!(
                    "经 {way} 按 Home 后前台还是 {app},改用 {other} 再按一次",
                    "After pressing Home via {way} the foreground is still {app}, pressing again via {other}"
                )
            );
        }
    }
    Ok((
        ictl_l10n::trf!(
            "已按 home,但前台还是 {app},先截图看看",
            "Pressed home, but the foreground is still {app}; take a screenshot"
        ),
        None,
    ))
}

/// 经 WDA 锁屏的两种办法
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LockWay {
    PowerKey,
    Wda,
}

/// 先试哪种：默认先经 IOHID 短按电源键（iPhone 18 Pro Max 上 `/wda/lock` 按住 0.5 秒不认，很慢），
/// 这台设备上记过 `lock_via_wda` 就先用 WDA 的（见 [`crate::core::PerDevice::lock_via_wda`]）。
/// 先试的那种不灵，另一种照样会试，所以记错了（比如系统升级后）也只是慢一次
pub(super) fn lock_order(via_wda: bool) -> [LockWay; 2] {
    if via_wda { [LockWay::Wda, LockWay::PowerKey] } else { [LockWay::PowerKey, LockWay::Wda] }
}

/// WDA 的 `/wda/lock`。偶尔等 5 秒报 500（手机其实已锁）：报错时看 WDA 报的锁屏状态，确认锁了就不退到 HID
/// （HID 会开屏幕媒体流）。没锁上时返回原因
pub(super) async fn lock_by_wda(w: &mut WdaClient<DeviceTransport>) -> Result<(), String> {
    match w.lock().await {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::info!("{}", ictl_l10n::log_trf!("WDA 锁屏报错：{e}", "WDA lock returned an error: {e}"));
            if w.wda_reports_locked().await.unwrap_or(false) {
                return Ok(());
            }
            Err(ictl_l10n::trf!("WDA 的锁屏报错：{e}", "WDA's lock returned an error: {e}"))
        }
    }
}

/// 经 IOHID 按电源键锁屏，等 WDA 报锁屏，最多 [`LOCK_WAIT`]。没锁上时返回原因。
/// WDA 的 `/wda/lock` 在 iOS 27 以下用 XCTest 的 `pressLockButton`，iPhone 12 mini（iOS 26.4.1）上锁不了屏
pub(super) async fn lock_by_power_hid(w: &mut WdaClient<DeviceTransport>) -> Result<(), String> {
    if let Err(e) = w.press_power_hid().await {
        tracing::info!(
            "{}",
            ictl_l10n::log_trf!("经 IOHID 按电源键报错：{e}", "Pressing the power key via IOHID failed: {e}")
        );
        return Err(ictl_l10n::trf!("经 IOHID 按电源键报错：{e}", "pressing the power key via IOHID failed: {e}"));
    }
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        if w.wda_reports_locked().await.unwrap_or(false) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let secs = LOCK_WAIT.as_secs();
            tracing::info!(
                "{}",
                ictl_l10n::log_trf!(
                    "经 IOHID 按电源键后 {secs} 秒还没锁屏",
                    "Still not locked {secs} s after pressing the power key via IOHID"
                )
            );
            return Err(ictl_l10n::trf!(
                "经 IOHID 按电源键后 {secs} 秒还没锁屏",
                "still not locked {secs} s after pressing the power key via IOHID"
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 经 WDA 的两种办法都没锁上、又没有 HID 后备（iOS 27 以下）时的报错：各自的原因，不说成 WDA 不可用
pub(super) fn lock_failed(failed: &[String]) -> String {
    ictl_l10n::trf!(
        "没锁上屏：{}。这台手机 iOS 低于 27，没有别的锁屏办法；先截图看看屏幕现在的样子",
        "Couldn't lock the screen: {}. This phone runs iOS below 27, so there is no other way to lock it; take a screenshot to see the screen as it is now",
        failed.join(ictl_l10n::tr!("；", "; "))
    )
}
