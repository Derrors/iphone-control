//! Acting at a point or on an element: taps, touches, long presses, swipes, typing

use super::*;

impl Daemon {
    /// A stale pointer must neither unlock nor act on a new display. Unlock can itself
    /// rotate the foreground app, so take the final measurement after it completes.
    pub(super) async fn prepare_coordinates(
        &self,
        g: &mut Inner,
        view: Option<&str>,
        start: bool,
    ) -> crate::error::Result<Option<Rotation>> {
        let changed = || {
            ictl_l10n::tr!(
                "画面已变化或尺寸尚未确认，请在新画面上重试",
                "The display changed or its geometry is unavailable. Retry on the new picture."
            )
            .to_owned()
        };
        if let Some(id) = view
            && g.dev.display.as_ref().is_none_or(|v| v.id != id)
        {
            return Err(changed().into());
        }
        // Move and Up use the finger's orientation from Down. A known display change
        // has already invalidated the token and released that finger in update_display.
        if !start {
            return Ok(None);
        }
        self.ensure_unlocked(g).await?;
        let Some(id) = view else { return Ok(None) };
        let (rotation, _) = self.sample_display(g, true).await;
        if g.dev.display.as_ref().is_none_or(|v| v.id != id) {
            return Err(changed().into());
        }
        Ok(rotation)
    }

    pub(crate) async fn tap(&self, g: &mut Inner, x: f64, y: f64, view: Option<&str>) -> crate::error::Result<String> {
        self.prepare_coordinates(g, view, true).await?;
        let pos = format!("({},{})", fmt_g(x), fmt_g(y));
        if let Some(w) = self.wda(g) {
            // 只有连不上才退到 HID；超时说明请求已发出，再用 HID 做一遍可能点两次
            match w.tap(x, y).await {
                Ok(()) => return Ok(ictl_l10n::trf!("已点击 {pos}", "Tapped {pos}")),
                Err(WdaError::Unavailable(_)) if view.is_none() => {}
                Err(e) => return Err(e.into()),
            }
        }
        if view.is_some() {
            return Err(wda_down(&g.state, g.info.as_ref()).into());
        }
        let rot = self.hid_touch_rotation(g).await?;
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                h.tap(x, y).await
            })
        })
        .await?;
        Ok(ictl_l10n::trf!("已点击 {pos}(经 HID)", "Tapped {pos} (via HID)"))
    }

    /// Follow the viewer pointer with one HID report per call. The first event prepares the device.
    /// Later events reuse the open HID session and reject a change in display orientation.
    pub(crate) async fn touch(
        &self,
        g: &mut Inner,
        phase: TouchPhase,
        x: f64,
        y: f64,
        view: Option<&str>,
    ) -> Result<(), Response> {
        if phase == TouchPhase::Prepare {
            self.hid(g).await.map_err(Response::error)?;
            g.hid_used = Some(Instant::now());
            return Ok(());
        }
        let measured = self.prepare_coordinates(g, view, phase == TouchPhase::Down).await.map_err(Response::error)?;
        if phase == TouchPhase::Down {
            let rotation = if let Some(rotation) = measured {
                rotation
            } else {
                // Clients without a view id can use the HID fallback.
                let fresh = match self.wda(g) {
                    Some(wda) => wda.rotation().await.ok(),
                    None => None,
                };
                match fresh {
                    Some(r) => {
                        g.dev.last_rotation = Some((Instant::now(), r));
                        r
                    }
                    None => self.hid_touch_rotation(g).await.map_err(Response::error)?,
                }
            };
            g.dev.touch_rotation = Some(rotation);
        }
        self.send_touch(g, phase, x, y).await
    }

    pub(super) async fn send_touch(&self, g: &mut Inner, phase: TouchPhase, x: f64, y: f64) -> Result<(), Response> {
        let contact = phase != TouchPhase::Up;
        let Some(rot) = g.dev.touch_rotation else {
            return Err(Response::error(ictl_l10n::tr!(
                "没有按下的手指,先发 down",
                "No finger is down; send down first"
            )));
        };
        if !contact {
            g.dev.touch_rotation = None;
        }
        // Reuse the open session without another WDA screen-size lookup on every move.
        let h = match g.tunnel.as_mut().and_then(|t| t.open_hid()) {
            Some(h) => h,
            None => match self.hid(g).await {
                Ok(h) => h,
                Err(message) => {
                    g.dev.touch_rotation = None;
                    return Err(Response::Error {
                        message: message.into(),
                        retry_as_gesture: phase == TouchPhase::Down,
                    });
                }
            },
        };
        h.rotation = rot;
        let result = h.touch(contact, x, y).await;
        g.hid_used = Some(Instant::now());
        if let Err(e) = result {
            // A failed write may still have reached the phone.
            g.dev.touch_rotation = None;
            drop_tunnel(g).await;
            return Err(Response::error(ictl_l10n::trf!("HID 命令失败: {e}", "HID command failed: {e}")));
        }
        Ok(())
    }

    pub(crate) async fn tap_element(
        &self,
        g: &mut Inner,
        text: &str,
        index: usize,
        max_age: Duration,
    ) -> crate::error::Result<(String, bool)> {
        self.ensure_unlocked(g).await?;
        let tree = self.tree(g, max_age).await?;
        let matches = elements::find(&tree.elements, text);
        if matches.is_empty() {
            return Ok((
                ictl_l10n::trf!(
                    "{}包含 \"{text}\" 的元素,先用 iphone_list_elements 或截图看看屏幕",
                    "{}: no element contains \"{text}\"; look at the screen with iphone_list_elements or a screenshot first",
                    tap_missed_prefixes()[0]
                ),
                true,
            ));
        }
        if index >= matches.len() {
            let list: Vec<_> = matches.iter().map(|e| e.describe()).collect();
            return Ok((
                ictl_l10n::trf!(
                    "{} {} 个:\n{}",
                    "{} {} (index out of range):\n{}",
                    tap_missed_prefixes()[1],
                    matches.len(),
                    list.join("\n")
                ),
                true,
            ));
        }
        let e = matches[index];
        // 中心被悬浮栏、键盘、浮层之类盖住时，点上去会落在盖着的东西上
        let ((x, y), moved) = match tree.tap_point(e) {
            TapPoint::Center(p) => (p, String::new()),
            TapPoint::Moved { at, by } => (
                at,
                ictl_l10n::trf!(
                    ";它的中心被「{by}」盖住,改点没被盖住的 ({},{})",
                    "; its center is covered by “{by}”, tapped an uncovered part at ({},{}) instead",
                    fmt_g(at.0),
                    fmt_g(at.1)
                ),
            ),
            TapPoint::Covered { by } => {
                return Ok((
                    ictl_l10n::trf!(
                        "{}:{} 整个被「{by}」盖住,没有点;先滑动让它露出来,或截图后按坐标点",
                        "{}: {} is completely covered by “{by}”, not tapped; scroll to reveal it first, or tap by coordinates from a screenshot",
                        tap_missed_prefixes()[2],
                        e.describe()
                    ),
                    true,
                ));
            }
        };
        self.require_wda(g)?.tap(x, y).await?;
        let note = if matches.len() > 1 {
            ictl_l10n::trf!("(共匹配 {} 个,点的是第 {index} 个)", " ({} matches, tapped number {index})", matches.len())
        } else {
            String::new()
        };
        Ok((ictl_l10n::trf!("已点击 {}{note}{moved}", "Tapped {}{note}{moved}", e.describe()), false))
    }

    pub(crate) async fn long_press(
        &self,
        g: &mut Inner,
        x: f64,
        y: f64,
        seconds: f64,
        view: Option<&str>,
    ) -> crate::error::Result<String> {
        self.prepare_coordinates(g, view, true).await?;
        self.require_wda(g)?.touch_and_hold(x, y, seconds).await?;
        Ok(ictl_l10n::trf!("已长按 ({},{}) {} 秒", "Long-pressed ({},{}) for {} s", fmt_g(x), fmt_g(y), fmt_g(seconds)))
    }

    pub(crate) async fn swipe(
        &self,
        g: &mut Inner,
        from: (f64, f64),
        to: (f64, f64),
        seconds: f64,
        view: Option<&str>,
    ) -> crate::error::Result<String> {
        self.prepare_coordinates(g, view, true).await?;
        let desc = format!("({},{}) → ({},{})", fmt_g(from.0), fmt_g(from.1), fmt_g(to.0), fmt_g(to.1));
        if let Some(w) = self.wda(g) {
            match w.swipe_into_polling(from, to, (seconds * 1000.0) as u64).await {
                // 滑动请求不等惯性滚动（等的话设置不对能卡十几秒），这里等列表停下再返回：
                // 列表还在滚时下一次点击只会让它停住，点不到东西
                Ok(()) => {
                    let vertical = (to.1 - from.1).abs() >= (to.0 - from.0).abs();
                    let (note, tree) = wait_scrolled(w, vertical).await;
                    g.swiped_tree = tree.map(|t| (Instant::now(), t));
                    return Ok(ictl_l10n::trf!("已滑动 {desc}{note}", "Swiped {desc}{note}"));
                }
                Err(WdaError::Unavailable(_)) if view.is_none() => {}
                Err(e) => return Err(e.into()),
            }
        }
        if view.is_some() {
            return Err(wda_down(&g.state, g.info.as_ref()).into());
        }
        let rot = self.hid_touch_rotation(g).await?;
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                h.drag(from, to, seconds).await
            })
        })
        .await?;
        Ok(ictl_l10n::trf!("已滑动 {desc}(经 HID)", "Swiped {desc} (via HID)"))
    }

    pub(crate) async fn type_text(&self, g: &mut Inner, text: &str, submit: bool) -> crate::error::Result<String> {
        self.ensure_unlocked(g).await?;
        let n = text.chars().count();
        if let Some(w) = self.wda(g) {
            // /wda/keys 没有输入框获得焦点时也返回成功，其实什么都没输：先看屏幕上有没有键盘
            let typed = match w.keyboard_shown().await {
                Ok(false) => {
                    return Err(
                        ictl_l10n::tr!("屏幕上没有键盘(没有输入框获得焦点),什么都没输;先点一下输入框,截图确认键盘出来了再输入", "No keyboard on screen (no text field has focus), nothing typed; tap a text field first and check with a screenshot that the keyboard is up").into(),
                    );
                }
                Ok(true) if text.is_empty() => Ok(()),
                Ok(true) => w.keys(text).await,
                Err(e) => Err(e),
            };
            match typed {
                Ok(()) => {
                    // 有的 App 边输入边刷新输入框，会吞掉一部分字（高德搜索框「故宫博物院」只剩「故宫博」），WDA 照样报成功：
                    // 输完读回输入框核对，少了结尾几个字就补上；对不上就不按回车
                    let note = match verify_typed(w, text).await {
                        Typed::Ok | Typed::Unknown => String::new(),
                        Typed::Fixed(k) => ictl_l10n::trf!(
                            "(第一次有 {k} 个字没输进去,已补上)",
                            " ({k} characters were dropped the first time and typed again)"
                        ),
                        Typed::Mismatch(now) => {
                            return Err(ictl_l10n::trf!(
                                "输入了 {n} 个字符,但输入框里现在是「{now}」,和要输入的「{text}」对不上(App 可能吞掉了部分字符){};截图确认后再处理",
                                "Typed {n} characters, but the field now contains “{now}”, which doesn't match “{text}” (the app may have swallowed some characters){}; check with a screenshot before going on",
                                if submit { ictl_l10n::tr!(",没有按回车", ", Return not pressed") } else { "" }
                            ).into());
                        }
                    };
                    if submit {
                        w.keys("\n").await.map_err(|e| {
                            ictl_l10n::trf!(
                                "文字已输入{note},按回车失败:{e}",
                                "Text typed{note}, pressing Return failed: {e}"
                            )
                        })?;
                    }
                    return Ok(ictl_l10n::trf!(
                        "已输入 {n} 个字符{note}{}",
                        "Typed {n} characters{note}{}",
                        if submit { ictl_l10n::tr!(",并按了回车", " and pressed Return") } else { "" }
                    ));
                }
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let payload = if submit { format!("{text}\n") } else { text.to_owned() };
        // 先检查，一个都不输；不打开 HID 会话
        if let Some(c) = payload.chars().find(|c| ascii_to_usage(*c).is_none()) {
            return Err(ictl_l10n::trf!(
                "WDA 不可用,HID 只能输入美式键盘上的英文和符号,不能输入 {c:?}",
                "WDA unavailable; HID can only type English letters and symbols of a US keyboard, not {c:?}"
            )
            .into());
        }
        self.hid_do(g, move |h| Box::pin(async move { h.type_ascii(&payload).await })).await?;
        Ok(ictl_l10n::trf!("已输入 {n} 个字符(经 HID)", "Typed {n} characters (via HID)"))
    }
}

/// 滑动后等列表停下（最多 [`SCROLL_SETTLE`]）：沿滑动方向的位置连续两次差不超过 [`STILL_TOLERANCE`]。
/// 没停下返回提醒；元素很少（游戏、地图）看不出来，不等。
/// 同时返回停下时读到的树，紧接着的 settle（then=…）从它开始比较，少读一次（[`Daemon::settle`]）。
/// 调用前已在轮询模式（[`WdaClient::swipe_into_polling`]）
pub(super) async fn wait_scrolled(w: &mut WdaClient<DeviceTransport>, vertical: bool) -> (String, Option<Tree>) {
    let t0 = Instant::now();
    let r: Result<(bool, Tree), WdaError> = async {
        let mut prev = w.tree().await?;
        if prev.elements.len() < VISUAL_SETTLE_BELOW {
            return Ok((true, prev));
        }
        while t0.elapsed() < SCROLL_SETTLE {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let cur = w.tree().await?;
            if cur.still_along(&prev, vertical, STILL_TOLERANCE) {
                return Ok((true, cur));
            }
            prev = cur;
        }
        Ok((false, prev))
    }
    .await;
    w.end_polling().await;
    match r {
        Ok((true, tree)) => (String::new(), Some(tree)),
        Ok((false, _)) => (
            ictl_l10n::trf!(
                "({:.1} 秒后列表还在动,可能还在滚动)",
                " (the list was still moving after {:.1} s, it may still be scrolling)",
                SCROLL_SETTLE.as_secs_f64()
            ),
            None,
        ),
        Err(_) => (String::new(), None),
    }
}

/// How a missed `iphone_tap_element` result starts: the Chinese prefixes are fixed because MCP servers before 0.1.5
/// recognise them; new ones read [`crate::protocol::Outcome`] instead. Goes in 0.3.0
/// ([`crate::protocol::TEXT_FALLBACK_UNTIL`])
pub(super) fn tap_missed_prefixes() -> [&'static str; 3] {
    if ictl_l10n::is_en() { ["Not found", "Only matched", "Covered"] } else { TAP_MISSED }
}

/// 输入后核对的结果
#[derive(Debug, PartialEq)]
pub(super) enum Typed {
    Ok,
    /// 核对不了：没读到输入框、有密码框、或读树失败
    Unknown,
    /// 结尾有几个字没输进去，已补上
    Fixed(usize),
    /// 对不上（输入框里现在的内容）
    Mismatch(String),
}

pub(super) fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 输入框里的内容有没有包含刚输入的文字（忽略空白：手机号之类会被自动加空格）
pub(super) fn typed_into(values: &[String], text: &str) -> bool {
    let want = squash(text);
    values.iter().any(|v| squash(v).contains(&want))
}

/// 输入框结尾是 `text` 的前几个字（后面的丢了）：返回输进去了几个字
pub(super) fn typed_prefix(values: &[String], text: &str) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    values
        .iter()
        .filter_map(|v| {
            let v = squash(v);
            (1..chars.len()).rev().find(|&k| {
                let p = squash(&chars[..k].iter().collect::<String>());
                !p.is_empty() && v.ends_with(&p)
            })
        })
        .max()
}

pub(super) async fn verify_typed(w: &mut WdaClient<DeviceTransport>, text: &str) -> Typed {
    if squash(text).is_empty() {
        return Typed::Unknown;
    }
    let mut values = None;
    // 树可能比输入框慢一拍：第一次对不上隔一下再读一次，确认真的少了再补，免得补重
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let Some(v) = w.elements().await.ok().and_then(|e| elements::field_values(&e)) else { return Typed::Unknown };
        if v.is_empty() {
            return Typed::Unknown;
        }
        if typed_into(&v, text) {
            return Typed::Ok;
        }
        values = Some(v);
    }
    let values = values.unwrap_or_default();
    let shown = || values.iter().find(|v| !v.is_empty()).cloned().unwrap_or_default();
    let Some(k) = typed_prefix(&values, text) else { return Typed::Mismatch(shown()) };
    // 剩下的一个字一个字慢慢补
    let rest: Vec<char> = text.chars().skip(k).collect();
    for c in &rest {
        if w.keys(&c.to_string()).await.is_err() {
            return Typed::Mismatch(shown());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    match w.elements().await.ok().and_then(|e| elements::field_values(&e)) {
        Some(v) if typed_into(&v, text) => Typed::Fixed(rest.len()),
        Some(v) => Typed::Mismatch(v.into_iter().find(|v| !v.is_empty()).unwrap_or_default()),
        None => Typed::Unknown,
    }
}
