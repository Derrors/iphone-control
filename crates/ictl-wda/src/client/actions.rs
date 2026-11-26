//! [`WdaClient`]: Gestures, buttons, IOHID events, launching and terminating apps, opening links

use super::*;

impl<T: Transport> WdaClient<T> {
    /// 点击：一个 W3C actions 单指手势（按下 50ms），期间把等待设置临时调小。真机计时（iOS 27）：
    /// - `/wda/tap` 先等界面空闲：主屏点图标约 1.7 秒（调小等待也没用），App 里约 0.6 秒，锁屏上约 2.9 秒
    /// - actions 手势做完要等动画冷却（`animationCoolOffTimeout`，默认 2 秒）：App 里切标签约 1.9 秒
    /// - actions + 临时调小等待：App 里约 0.7 秒，主屏点图标约 0.9 秒
    /// - 冷却不能是 0.2 秒（[`IdleSettings::QUICK`]）：界面上一直有动画时（键盘弹着、光标在闪）要等约 26 秒才返回，
    ///   iPhone 18 Pro Max（iOS 27.2.1）在设置里点搜索结果每次都这样；冷却 0 约 0.4 秒。和滑动一样用
    ///   [`IdleSettings::NO_COOL_OFF`]
    ///
    /// 和 `/wda/tap` 一样不等点击引起的动画结束，调用方要看结果就再截图
    pub async fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        self.quick_gesture(&tap_steps(x, y, TAP_PRESS_MS), IdleSettings::NO_COOL_OFF).await
    }

    /// 依次点几个点，一个手势（[`tap_sequence_body`]）：同一个键盘、键盘上连按时用。分开点每下都要等界面空闲，
    /// 数字键盘上点 7 个键约 3.6 秒，一个手势约 1.9 秒。点的过程中界面变了（跳了页面）后面的点也照样点下去，
    /// 所以只给点的时候布局不变的一串点用
    pub async fn tap_sequence(&mut self, points: &[(f64, f64)]) -> Result<()> {
        let body = tap_sequence_body(points, TAP_PRESS_MS, SEQUENCE_GAP_MS);
        self.quick_actions(body, IdleSettings::NO_COOL_OFF).await
    }

    /// 同一点连点 `count` 下（双击、「短时间内连点 N 下」的隐藏入口），一个 actions 手势，每下之间停
    /// [`MULTI_TAP_GAP_MS`]。分开点每下都要等界面空闲（约 0.6 秒），`iphone_batch` 连点 5 下约 2.9 秒，赶不上这类入口的时间窗
    pub async fn multi_tap(&mut self, x: f64, y: f64, count: u32) -> Result<()> {
        let steps = multi_tap_steps(x, y, count, TAP_PRESS_MS, MULTI_TAP_GAP_MS);
        self.quick_gesture(&steps, IdleSettings::NO_COOL_OFF).await
    }

    /// 长按：按下、停 `seconds` 秒、抬起，一个 actions 手势。`/wda/touchAndHold` 前后都等界面空闲，
    /// 真机上主屏长按图标 1.2 秒要约 12 秒才返回。等待设置同 [`Self::tap`]
    pub async fn touch_and_hold(&mut self, x: f64, y: f64, seconds: f64) -> Result<()> {
        self.quick_gesture(&tap_steps(x, y, (seconds * 1000.0) as u64), IdleSettings::NO_COOL_OFF).await
    }

    /// 滑动：一个 actions 手势，期间用 [`IdleSettings::NO_COOL_OFF`]。做完时列表可能还在惯性滚动，
    /// 要看结果的调用方自己等界面稳定
    pub async fn swipe(&mut self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        self.quick_gesture(&swipe_steps(from, to, ms), IdleSettings::NO_COOL_OFF).await
    }

    /// The MJPEG stream (the live viewer) at up to `MJPEG_FPS` frames a second instead of WDA's 10, at full size
    /// (`MJPEG_SCALE`, why not smaller) and never turned upright, set with each new session: the picture follows a
    /// drag closely. In landscape the frames stay portrait with the content sideways (on an iPhone; an iPad's are
    /// already landscape) and the viewer page turns them by the interface rotation, as it does the phone's own video:
    /// `mjpegFixOrientation` makes WDA decode, rotate and re-encode every frame, 92 % of a core on an iPhone 15 Pro Max
    /// in landscape, and iOS killed it within two minutes. WDA only captures while a client is connected
    pub fn fast_stream(&mut self) {
        self.fast_stream = true;
    }

    /// Swipes and stays in polling mode for the caller, who ends it with [`Self::end_polling`]: restoring the settings
    /// only to change them again right after would cost two round trips
    /// `hold_ms` > 0 holds the finger still at `from` first ([`drag_steps`])
    pub async fn swipe_into_polling(&mut self, from: (f64, f64), to: (f64, f64), hold_ms: u64, ms: u64) -> Result<()> {
        self.baseline().await.map_err(WdaError::before_input)?;
        self.set_idle_settings(IdleSettings::NO_COOL_OFF).await.map_err(WdaError::before_input)?;
        let r = self.actions(&drag_steps(from, to, hold_ms, ms)).await;
        if r.is_err() {
            self.end_polling().await;
        }
        r
    }

    /// 开始轮询界面：读元素树时不等界面空闲、动画冷却（约 0.2 秒一次），由调用方比较前后两次判断稳定。
    /// 用完调 [`Self::end_polling`] 恢复
    pub async fn begin_polling(&mut self) -> Result<()> {
        self.baseline().await?;
        self.set_idle_settings(IdleSettings::NO_COOL_OFF).await
    }

    pub async fn end_polling(&mut self) {
        let b = self.idle_baseline.unwrap_or(IdleSettings::DEFAULT);
        if let Err(e) = self.set_idle_settings(b).await {
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!(
                    "轮询后恢复 WDA 等待设置失败：{e}",
                    "Restoring the WDA wait settings after polling failed: {e}"
                )
            );
        }
    }

    /// 平时的等待设置（第一次用时读一次）。临时调小、改回之前都按它恢复
    pub async fn baseline(&mut self) -> Result<IdleSettings> {
        if let Some(b) = self.idle_baseline {
            return Ok(b);
        }
        // 上次没恢复成功的话读到的就是调小的值：按 WDA 默认值算
        let b = match self.idle_settings().await? {
            s if s == IdleSettings::QUICK || s == IdleSettings::NO_COOL_OFF => IdleSettings::DEFAULT,
            s => s,
        };
        self.idle_baseline = Some(b);
        Ok(b)
    }

    /// 发一个手势，期间把等待设置临时换成 `during`（见 [`Self::tap`]），做完恢复
    pub(super) async fn quick_gesture(&mut self, steps: &[PointerStep], during: IdleSettings) -> Result<()> {
        self.quick_actions(actions_body(steps), during).await
    }

    pub(super) async fn quick_actions(&mut self, body: Value, during: IdleSettings) -> Result<()> {
        let baseline = self.baseline().await.map_err(WdaError::before_input)?;
        self.set_idle_settings(during).await.map_err(WdaError::before_input)?;
        let r = self.post("/actions", body).await.map(drop);
        if let Err(e) = self.set_idle_settings(baseline).await {
            tracing::warn!(
                "{}",
                ictl_l10n::log_trf!(
                    "手势后恢复 WDA 等待设置失败：{e}",
                    "Restoring the WDA wait settings after the gesture failed: {e}"
                )
            );
        }
        r
    }

    /// 一次 W3C actions 请求。注意：多次点击合并成一个请求会漏键，解锁时每位单独发
    pub async fn actions(&mut self, steps: &[PointerStep]) -> Result<()> {
        self.post("/actions", actions_body(steps)).await.map(drop)
    }

    /// home / volumeUp / volumeDown，走 XCTest 的 `XCUIDevice pressButton`。
    /// iPhone 12 mini（iOS 26.4.1）上 home 不起作用但照样报成功，见 [`Self::press_home_hid`]
    pub async fn press_button(&mut self, name: &str) -> Result<()> {
        self.post("/wda/pressButton", json!({"name": name})).await.map(drop)
    }

    /// 直接发一个 IOHID 按键事件（`/wda/performIoHidEvent`），按住 `seconds` 秒
    pub async fn hid_event(&mut self, page: u32, usage: u32, seconds: f64) -> Result<()> {
        self.post("/wda/performIoHidEvent", json!({"page": page, "usage": usage, "duration": seconds})).await.map(drop)
    }

    /// 经 IOHID 按 Home（消费者页 AC Home）。iPhone 12 mini（iOS 26.4.1）实测：XCTest 的 home 无效时
    /// 它能从 App 回到主屏、关掉 Spotlight、点亮熄灭的屏幕；同页的 Menu（0x40）没反应
    pub async fn press_home_hid(&mut self) -> Result<()> {
        self.hid_event(HID_PAGE_CONSUMER, HID_AC_HOME, 0.05).await
    }

    /// 经 IOHID 短按电源键（`POWER_PRESS_SECS`，和手指按一下差不多）。WDA 自己的 `/wda/lock` 在 iOS 27 上按住 0.5 秒，
    /// iOS 27 以下用 XCTest 的 `pressLockButton`，iPhone 12 mini（iOS 26.4.1）、iPad mini 5（iPadOS 26.7.3）上锁不了屏。
    /// 屏幕已熄灭时按它会把屏幕点亮，调用方要先确认没锁屏
    pub async fn press_power_hid(&mut self) -> Result<()> {
        self.hid_event(HID_PAGE_CONSUMER, HID_POWER, POWER_PRESS_SECS).await
    }

    pub async fn lock(&mut self) -> Result<()> {
        self.post("/wda/lock", json!({})).await.map(drop)
    }

    /// 启动并等到前台、界面稳定（冷启动后立即点击会被吞），最多等 8 秒。
    /// 等待期间查前台出错不算失败（启动命令已经成功了）
    pub async fn launch_app(&mut self, bundle_id: &str) -> Result<Launch> {
        let started = tokio::time::Instant::now();
        // 不经 XCTest 启动（LSApplicationWorkspace），发出就返回，到没到前台由下面自己查。`/wda/apps/launch` 要等 XCTest
        // 看到 App 在前台：系统 App 第一次打开时隐私说明页盖在上面，它就一直等到 60 秒超时，WDA 也被判卡死重启
        // （iPhone 12 mini，iOS 26.4.1，「备忘录」「提醒事项」都是 65 秒）。这时它回「failed to launch」，说明页已经出来了
        let body = json!({"bundleId": bundle_id});
        match self.call("POST", "/wda/apps/launchUnattached", Some(body.clone()), DEFAULT_TIMEOUT).await {
            Ok(_) => {}
            Err(WdaError::Remote { message, .. }) if message.contains("failed to launch") => {}
            // older WDA without the route: launch as before
            Err(WdaError::Remote { kind, message }) if unknown_route(&kind, &message) => {
                self.in_session("POST", "/wda/apps/launch", Some(body), Duration::from_secs(60)).await?;
            }
            Err(e) => return Err(e),
        }
        let t0 = tokio::time::Instant::now();
        let deadline = t0 + Duration::from_secs(8);
        let mut foreground = false;
        while tokio::time::Instant::now() < deadline {
            match self.active_app().await {
                Ok(a) if a == bundle_id => {
                    foreground = true;
                    break;
                }
                Ok(a) if a == PRIVACY_SHEET => {
                    return Ok(Launch::Blocked(
                        ictl_l10n::tr!(
                            "第一次打开时的隐私说明页(点「继续」或「接受并继续」后才进入 App)",
                            "the privacy notice shown on first launch (tap Continue or Accept & Continue to get into the app)"
                        )
                        .into(),
                    ));
                }
                // 系统弹窗（权限请求）挡着时前台一直是 SpringBoard，原来要白等满 8 秒。
                // 刚启动的一瞬间前台也还是 SpringBoard，过 1 秒还是才去查弹窗（查一次约 0.4 秒）
                Ok(a) if a == SPRINGBOARD && bundle_id != SPRINGBOARD && t0.elapsed() >= Duration::from_secs(1) => {
                    if let Ok(Some(text)) = self.alert_text().await {
                        return Ok(Launch::Blocked(text));
                    }
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !foreground {
            return Ok(match self.alert_text().await {
                Ok(Some(text)) => Launch::Blocked(text),
                _ => Launch::Slow,
            });
        }
        self.settle_launched(started).await;
        Ok(Launch::Foreground)
    }

    /// App 到了前台之后再等它画出来。`started`：发出启动请求的时刻
    pub(super) async fn settle_launched(&mut self, started: tokio::time::Instant) {
        // 冷启动后立即点击会被吞（回归用例 #13）。原来固定再等 1 秒；改为等界面稳定：连续两次元素树一样
        // （读元素树时 WDA 本身也等界面空闲），最多仍是 1 秒
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut prev = None;
        while tokio::time::Instant::now() < deadline {
            let cur = self.source_xml().await.ok();
            if cur.is_some() && cur == prev {
                break;
            }
            prev = cur;
        }
        // 切回已在后台的 App 时元素树马上就稳定了（约 0.4 秒），放大进场的动画还没完，这时点击会被吞（真机：小红书、高德各一次）
        if let Some(left) = LAUNCH_MIN.checked_sub(started.elapsed()) {
            tokio::time::sleep(left).await;
        }
    }

    pub async fn terminate_app(&mut self, bundle_id: &str) -> Result<()> {
        self.post("/wda/apps/terminate", json!({"bundleId": bundle_id})).await.map(drop)
    }

    /// 打开网址或 URL Scheme，等打开它的 App 到前台、画出来再返回（最多约 8 秒），同 [`Self::launch_app`]。
    /// 不等的话 App 冷启动时还是黑屏，紧接着截图会把两帧黑屏当成「画面已静止」（iPad Safari 真机）。
    /// 返回结果和前台 App；在原来的前台 App 里打开（Safari 里开网址）时等 1 秒没换就算打开了
    pub async fn open_url(&mut self, url: &str) -> Result<(Launch, String)> {
        let started = tokio::time::Instant::now();
        let before = self.active_app().await.unwrap_or_default();
        self.in_session("POST", "/url", Some(json!({"url": url})), Duration::from_secs(60)).await?;
        let deadline = started + Duration::from_secs(8);
        // 前台要连续两次是同一个才算：Safari 冷启动的一瞬间前台是 `com.apple.DocumentManager.DockFolderViewService`（iPad 真机）
        let mut last = String::new();
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let waited = started.elapsed() >= Duration::from_secs(1);
            let now = self.active_app().await;
            let steady = now.as_ref().is_ok_and(|a| *a == last);
            if let Ok(a) = &now {
                last.clone_from(a);
            }
            match now {
                Ok(a) if a != before && a != SPRINGBOARD && steady => {
                    self.settle_launched(started).await;
                    return Ok((Launch::Foreground, a));
                }
                Ok(a) if a == before && a != SPRINGBOARD && waited => {
                    self.settle_launched(started).await;
                    return Ok((Launch::Foreground, a));
                }
                // 「要在某 App 中打开吗」之类的系统弹窗挡着时前台一直是 SpringBoard
                Ok(a) if a == SPRINGBOARD && waited => {
                    if let Ok(Some(text)) = self.alert_text().await {
                        return Ok((Launch::Blocked(text), a));
                    }
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                let now = self.active_app().await.unwrap_or_default();
                return Ok((Launch::Slow, now));
            }
        }
    }
}
