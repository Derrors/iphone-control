//! [`WdaClient`]: What is on the screen: size, rotation, lock state, the element tree, screenshots, alerts, the keyboard

use super::*;

impl<T: Transport> WdaClient<T> {
    /// Read the WDA screen size again, including changes to Display Zoom.
    pub async fn measure_screen(&mut self) -> Result<ScreenSize> {
        let v = self.get("/wda/screen").await?;
        let s = &v["screenSize"];
        let positive = |key: &str| s[key].as_f64().filter(|n| n.is_finite() && *n > 0.0);
        let size =
            positive("width").zip(positive("height")).map(|(width, height)| ScreenSize { width, height }).ok_or_else(
                || WdaError::Unavailable(ictl_l10n::text!("/wda/screen 尺寸无效", "Invalid /wda/screen dimensions")),
            )?;
        self.screen = Some(size);
        self.screen_at = Some(Instant::now());
        Ok(size)
    }

    pub async fn base_screen_size(&mut self) -> Result<ScreenSize> {
        match self.screen {
            Some(s) => Ok(s),
            None => self.measure_screen().await,
        }
    }

    /// 当前方向下的屏幕点尺寸（转屏后宽高跟着变）
    pub async fn screen_size(&mut self) -> Result<ScreenSize> {
        Ok(self.view().await?.screen)
    }

    /// 当前方向下的屏幕尺寸和方向。尺寸只问一次 `/wda/screen`（约 0.3 秒），方向每次问（约 20 毫秒）：
    /// 尺寸缓存的是问的那一刻的方向，转屏后不换算的话截图会被压扁、屏幕下半截的元素被当成屏幕外。
    /// 问不到方向（旧版 WDA 等）就按缓存的尺寸、方向未知（不换算元素坐标）
    pub async fn view(&mut self) -> Result<elements::View> {
        let base = self.base_screen_size().await?;
        Ok(match self.orientation().await {
            Ok(o) => elements::View { screen: o.orient(base), rotation: Some(o) },
            Err(e) => {
                tracing::debug!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "读屏幕方向失败，按缓存的屏幕尺寸算：{e}",
                        "Reading the screen orientation failed, using the cached screen size: {e}"
                    )
                );
                elements::View { screen: base, rotation: None }
            }
        })
    }

    /// 界面相对设备竖放转了多少（`GET /rotation` 的 z）。能分出两个横屏方向，`/orientation` 分不出
    pub async fn rotation(&mut self) -> Result<ictl_device::Rotation> {
        let v = self.in_session("GET", "/rotation", None, Duration::from_secs(10)).await?;
        v["z"].as_f64().and_then(ictl_device::Rotation::from_degrees).ok_or_else(|| {
            WdaError::Unavailable(ictl_l10n::textf!(
                "看不懂 WDA 给的旋转角：{v}",
                "Can't make sense of the rotation WDA reported: {v}"
            ))
        })
    }

    /// Older WDA versions lack /rotation. Remember an unsupported route for this runner;
    /// the regular /orientation endpoint folds opposite directions into the same string.
    pub(super) async fn interface_rotation(
        &mut self,
        fallback: impl Future<Output = Option<Rotation>>,
    ) -> Option<Rotation> {
        if !self.rotation_unsupported {
            match self.rotation().await {
                Ok(r) => return Some(r),
                Err(WdaError::Remote { kind, message })
                    if matches!(kind.as_str(), "unknown command" | "unknown method" | "unsupported operation")
                        || (kind == "http" && matches!(message.as_str(), "404" | "405")) =>
                {
                    self.rotation_unsupported = true;
                }
                Err(_) => return None,
            }
        }
        fallback.await
    }

    /// Poll rotation to detect a changed view. The screen size (a slower request) is measured on acquisition and on
    /// rotation, and again once the last measurement is older than [`SCREEN_RECHECK`] (Display Zoom) when
    /// `refresh_stale`: a poll while nobody acts, never a gesture's start, which costs only the rotation request.
    pub async fn display_sample(
        &mut self,
        previous: Option<u16>,
        refresh_stale: bool,
        fallback: impl Future<Output = Option<Rotation>>,
    ) -> (Option<ScreenSize>, Option<Rotation>) {
        let rotation = self.interface_rotation(fallback).await;
        let stale = self.screen_at.is_none_or(|t| t.elapsed() >= SCREEN_RECHECK);
        let rotated = rotation.is_some_and(|r| Some(r.degrees()) != previous);
        let size = if rotated || (refresh_stale && stale) {
            self.measure_screen().await
        } else {
            self.base_screen_size().await
        };
        (size.ok().map(ScreenSize::portrait), rotation)
    }

    /// 前台界面的方向
    pub async fn orientation(&mut self) -> Result<ictl_device::Rotation> {
        let v = self.in_session("GET", "/orientation", None, Duration::from_secs(10)).await?;
        v.as_str().and_then(ictl_device::Rotation::from_wda_orientation).ok_or_else(|| {
            WdaError::Unavailable(ictl_l10n::textf!(
                "看不懂 WDA 给的方向：{v}",
                "Can't make sense of the orientation WDA reported: {v}"
            ))
        })
    }

    /// 是否锁屏。WDA 的 `/wda/locked` 只靠 `com.apple.springboard.lockstate` 的变化通知维护
    /// （WebDriverAgent `XCUIDevice+FBHelpers.m`），有两处误报，用元素树纠正（[`elements::cover_sheet`]）：
    /// - 初值为 false：WDA 在手机锁着时启动就一直报未锁屏，直到下一次锁屏/解锁。确认过一次「确实没锁屏」之后
    ///   WDA 就跟上了，此后它说没锁就直接信，不多发请求
    /// - 下拉通知中心时报 true（和锁屏同一个界面）：WDA 说锁了时核对一次，是通知中心就算没锁
    ///
    /// 核对失败时按 WDA 的结果；需要「拿不准就算锁着」时用 [`Self::check_lock`] 的 strict 模式
    pub async fn locked(&mut self) -> Result<bool> {
        Ok(self.check_lock(None, false).await?.locked)
    }

    /// 锁屏判断，并带回核对时取到的元素树（解锁流程接着用，省一次 `/source`）。
    /// - `reported`：调用方刚问过的 `/wda/locked`，省一次请求
    /// - `strict`：拿不准就算锁着。确认整串密码是否解开时用：把仍锁着误判成已解锁，下一次会把同一个错误密码再输一遍
    pub async fn check_lock(&mut self, reported: Option<bool>, strict: bool) -> Result<LockCheck> {
        let wda_locked = match reported {
            Some(v) => v,
            None => self.wda_reports_locked().await?,
        };
        if self.lock_state_trusted && (!wda_locked || strict) {
            // 已同步过：WDA 说没锁就是没锁；strict 场景在密码键盘上，WDA 说锁了也不会是通知中心
            return Ok(LockCheck { locked: wda_locked, uncertain: false, tree: None });
        }
        let (sheet, tree) = match self.cover_tree().await {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "核对锁屏界面失败，按{}处理：{e}",
                        "Checking the lock screen failed, going by {}: {e}",
                        if strict {
                            ictl_l10n::tr!("锁着", "locked")
                        } else {
                            ictl_l10n::tr!(" WDA 的结果", "what WDA reported")
                        }
                    )
                );
                return Ok(LockCheck { locked: strict || wda_locked, uncertain: false, tree: None });
            }
        };
        let (locked, uncertain) = match (wda_locked, sheet) {
            (true, elements::CoverSheet::NotificationCenter) => (false, false),
            (true, _) => (true, false),
            (false, elements::CoverSheet::Locked) => {
                tracing::info!(
                    "{}",
                    ictl_l10n::log_tr!(
                        "WDA 报未锁屏，但屏幕上是锁屏界面（WDA 在锁屏时启动，状态未同步）",
                        "WDA says unlocked, but the screen shows the lock screen (WDA started while locked and its state is stale)"
                    )
                );
                (true, false)
            }
            // 锁屏样式、却没有锁的标记：可能是通知中心，也可能是 Touch ID 机型熄屏、有通知时的锁屏
            // （iPad mini 5 / iPadOS 26.6.1 真机：没有「按下主屏幕按钮以解锁」那行）。WDA 在锁屏时启动的话一直报 false，
            // 不能在这里认定它可信，否则之后再也不核对、永远不会自动解锁
            // 严格核对时拿不准就算锁着（同 `strict` 的说明）
            (false, elements::CoverSheet::NotificationCenter) => (strict, !strict),
            // 普通界面上确认过一次：WDA 的状态是同步的
            (false, elements::CoverSheet::None) => {
                self.lock_state_trusted = true;
                (false, false)
            }
        };
        Ok(LockCheck { locked, uncertain, tree })
    }

    /// WDA 自己报的锁屏状态，不核对。可能误报，见 [`Self::locked`]；配合 [`Self::lock_state_trusted`] 做快速判断
    pub async fn wda_reports_locked(&self) -> Result<bool> {
        Ok(self.call("GET", "/wda/locked", None, Duration::from_secs(10)).await?["value"] == true)
    }

    /// 本次 WDA 启动后是否确认过一次没锁屏。确认过且 WDA 说没锁，就确定没锁
    pub fn lock_state_trusted(&self) -> bool {
        self.lock_state_trusted
    }

    /// 前台是 SpringBoard 时看元素树是锁屏、通知中心还是普通界面，并带回元素树；前台是别的 App 就是普通界面
    pub(super) async fn cover_tree(&mut self) -> Result<(elements::CoverSheet, Option<elements::Source>)> {
        if self.active_app().await? != SPRINGBOARD {
            return Ok((elements::CoverSheet::None, None));
        }
        let src = elements::Source::from_xml(&self.source_xml().await?);
        Ok((elements::cover_sheet(&src), Some(src)))
    }

    pub async fn active_app(&self) -> Result<String> {
        Ok(self.get("/wda/activeAppInfo").await?["bundleId"].as_str().unwrap_or_default().to_owned())
    }

    pub async fn screenshot_png(&self) -> Result<Vec<u8>> {
        // base64 约 11MB：从 JSON 里直接拿走字符串，原地去掉换行，不再另外复制
        let Value::String(mut b64) = self.get("/screenshot").await? else {
            return Err(WdaError::Unavailable(ictl_l10n::text!(
                "截图响应不是字符串",
                "screenshot response is not a string"
            )));
        };
        b64.retain(|c| !c.is_whitespace());
        base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| {
            WdaError::Unavailable(ictl_l10n::textf!(
                "截图不是有效的 base64：{e}",
                "screenshot is not valid base64: {e}"
            ))
        })
    }

    /// 元素树 XML，排除可见性等慢属性（完整树 7～8 秒，排除后约 0.3 秒）。读得慢就收紧快照上限（[`SnapshotLimits`]）
    pub async fn source_xml(&mut self) -> Result<String> {
        let level = self.snapshot_level;
        if self.snapshot_applied.unwrap_or(0) != level {
            let l = SNAPSHOT_LEVELS[level];
            let body = json!({"settings": {
                "snapshotMaxDepth": l.depth,
                "snapshotMaxChildren": l.children.unwrap_or(i32::MAX as u32),
            }});
            // 设不上照样读，下次再设
            match self.post("/appium/settings", body).await {
                Ok(_) => self.snapshot_applied = Some(level),
                Err(e) => tracing::debug!("snapshot limits: {e}"),
            }
        }
        let applied = self.snapshot_applied.unwrap_or(0);
        let path = format!("/source?format=xml&excluded_attributes={}", elements::SOURCE_EXCLUDED);
        let t0 = Instant::now();
        let r = self.call("GET", &path, None, SOURCE_TIMEOUT).await;
        let took = match r {
            Ok(_) => Some(t0.elapsed()),
            Err(WdaError::Timeout(_)) => None,
            Err(e) => return Err(e),
        };
        let next = next_snapshot_level(applied, took);
        if next != applied {
            tracing::info!(
                "{}",
                ictl_l10n::log_trf!(
                    "读元素树{}，快照上限改为 {:?}",
                    "Reading the element tree {}; snapshot limits now {:?}",
                    took.map_or_else(
                        || ictl_l10n::log_tr!("超时", "timed out").to_owned(),
                        |t| ictl_l10n::log_trf!("用了 {:.1} 秒", "took {:.1} s", t.as_secs_f64())
                    ),
                    SNAPSHOT_LEVELS[next]
                )
            );
        }
        self.snapshot_level = next;
        let v = r?;
        self.snapshot_read = applied;
        Ok(v["value"].as_str().unwrap_or_default().to_owned())
    }

    /// 上一次读到的元素树受了快照上限（[`SnapshotLimits`]）的限制时，给人和 agent 看的说明
    pub fn snapshot_note(&self) -> Option<String> {
        let l = SNAPSHOT_LEVELS[self.snapshot_read];
        let children = l.children?;
        Some(ictl_l10n::trf!(
            "(界面元素太多、读得慢,这次只读了 {} 层、每个元素最多 {children} 个子元素:长列表靠后的行、很深的元素可能没列出;读得快了会自动放宽)",
            "(This screen has so many elements that reading them was slow, so this read went {} levels deep with at most {children} children per element: rows far down a long list or deeply nested elements may be missing; the limits loosen again once reads are fast)",
            l.depth
        ))
    }

    pub async fn elements(&mut self) -> Result<Vec<elements::Element>> {
        Ok(self.tree().await?.elements)
    }

    /// 元素树，带上「谁盖住谁」「有没有加载圈」等信息
    pub async fn tree(&mut self) -> Result<elements::Tree> {
        let view = self.view().await?;
        self.move_detection_point(view.screen).await;
        let mut tree = elements::parse_tree(&self.source_xml().await?, view);
        tree.incomplete |= SNAPSHOT_LEVELS[self.snapshot_read].children.is_some();
        Ok(tree)
    }

    /// 把 WDA 判断前台 App 的点（appium setting `activeAppDetectionPoint`）挪到 [`detection_point`]，每个客户端做一次。
    ///
    /// WDA 读元素树、问前台 App 时，看屏幕上这个点下面的元素属于哪个进程。默认点在离左上角短边 20% 的地方
    /// （WDA 16.15.3 `FBActiveAppDetectionPoint`，430 点宽的 iPhone 上是 (86, 86)），正好在通知横幅底下：横幅弹着时
    /// 读到的是 SpringBoard 的树，按名字点 App 里的按钮就找不到（iPhone16,2 / iOS 27.2.1 的备忘录，「Device Hub」横幅）。
    /// 设不上不影响读树，只是还用默认的点；连不上 WDA 时下次再试
    pub(super) async fn move_detection_point(&mut self, screen: ScreenSize) {
        if self.detection_point_set {
            return;
        }
        let (x, y) = detection_point(screen);
        let body = json!({"settings": {"activeAppDetectionPoint": format!("{x},{y}")}});
        match self.post("/appium/settings", body).await {
            Ok(_) => self.detection_point_set = true,
            Err(e) => {
                tracing::debug!("activeAppDetectionPoint: {e}");
                self.detection_point_set = !matches!(e, WdaError::Unavailable(_));
            }
        }
    }

    /// 屏幕上的系统弹窗（定位权限、通知权限等）的文字；没有弹窗返回 None。
    /// 弹窗挡着时 Home 不生效、App 也到不了前台，WDA 却照样报成功
    pub async fn alert_text(&mut self) -> Result<Option<String>> {
        match self.in_session("GET", "/alert/text", None, DEFAULT_TIMEOUT).await {
            Ok(v) => Ok(v.as_str().map(str::to_owned).filter(|t| !t.is_empty())),
            Err(WdaError::Remote { kind, .. }) if kind.contains("alert") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 屏幕上有没有软键盘（有输入框获得了焦点）。`/wda/keys` 在没有焦点时也返回成功，什么都没输进去
    pub async fn keyboard_shown(&mut self) -> Result<bool> {
        let v = self.post("/elements", json!({"using": "class name", "value": "XCUIElementTypeKeyboard"})).await?;
        Ok(v.as_array().is_some_and(|a| !a.is_empty()))
    }

    /// 锁屏密码栏的 (总位数, 已输入位数)；找不到密码栏返回 None
    pub async fn passcode_field(&mut self) -> Result<Option<(u32, u32)>> {
        let els =
            self.post("/elements", json!({"using": "class name", "value": "XCUIElementTypeSecureTextField"})).await?;
        for e in els.as_array().into_iter().flatten() {
            let Some(eid) = e.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_str()) else { continue };
            let v = self.in_session("GET", &format!("/element/{eid}/attribute/value"), None, DEFAULT_TIMEOUT).await?;
            if let Some(p) = v.as_str().and_then(elements::parse_passcode_value) {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }
}
