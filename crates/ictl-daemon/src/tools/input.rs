//! Acting at a point or on an element: taps, touches, long presses, swipes, typing

use super::*;
use crate::protocol::MAX_TAP_COUNT;

/// 一次滑动（[`Daemon::swipe`]）：从 `from` 用 `seconds` 秒移到 `to`；`hold` 秒大于 0 时先在起点按住再移动
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Stroke {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub seconds: f64,
    pub hold: f64,
}

/// 边滑边找时每次滑多久：1 秒的慢滑停下时几乎没有惯性，内容移动和手指移动差不多（iPhone 12 mini 设置首页：
/// 手指移 300 点，0.3 秒滚了 618 点，0.6 秒 420 点，1 秒 282 点；W3C 在抬起前停一下也去不掉惯性）
pub(super) const SEARCH_SWIPE_SECONDS: f64 = 1.0;

/// 边滑边找时算找到了：有匹配，而且要点的那个没有被整个盖住（停在底部工具栏、悬浮栏下面的还要再滑一下）。
/// index 超出匹配个数也算找到：滑下去不会让序号变得更有意义，交给后面照常报
fn reachable_in(tree: &Tree, matches: &[&elements::Element], index: usize) -> bool {
    match matches.get(index) {
        Some(e) => !matches!(tree.tap_point(e), TapPoint::Covered { .. }),
        None => !matches.is_empty(),
    }
}

/// 有能点到的匹配，按 [`elements::find`]：没有文字完全相同的就算包含它的
pub(super) fn reachable(tree: &Tree, text: &str, index: usize) -> bool {
    reachable_in(tree, &elements::find(&tree.elements, text), index)
}

/// 有能点到的、文字（标签或名字）完全相同的匹配。边滑边找只在这时停下：当前屏只有包含它的（找「App」时的
/// 「Apple账户」）不算，要找的多半在后面
pub(super) fn reachable_exact(tree: &Tree, text: &str, index: usize) -> bool {
    let exact: Vec<_> = tree.elements.iter().filter(|e| e.label == text || e.name == text).collect();
    reachable_in(tree, &exact, index)
}

/// 边滑边找的一次滑动：屏幕正中，竖着滑 35% 屏高（前后两屏有重叠，不会漏过一行）。`down` 看下面的内容，手指往上
pub(super) fn search_stroke(width: f64, height: f64, down: bool) -> ((f64, f64), (f64, f64)) {
    let x = (width / 2.0).round();
    let (low, high) = ((height * 0.68).round(), (height * 0.33).round());
    if down { ((x, low), (x, high)) } else { ((x, high), (x, low)) }
}

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

    /// 点击 `count` 下（1 = 普通点击）。连点时 WDA 发一个手势（[`WdaClient::multi_tap`]），HID 在同一个会话里连发
    pub(crate) async fn tap(
        &self,
        g: &mut Inner,
        x: f64,
        y: f64,
        count: u32,
        view: Option<&str>,
    ) -> crate::error::Result<String> {
        if !(1..=MAX_TAP_COUNT).contains(&count) {
            return Err(Error::preflight(ictl_l10n::trf!(
                "count 要是 1 到 {MAX_TAP_COUNT} 的整数",
                "count must be a whole number from 1 to {MAX_TAP_COUNT}"
            )));
        }
        self.prepare_coordinates(g, view, true).await.map_err(Error::preflight)?;
        let pos = format!("({},{})", fmt_g(x), fmt_g(y));
        let times = if count > 1 { ictl_l10n::trf!(",连点 {count} 下", ", {count} times") } else { String::new() };
        if let Some(w) = self.wda(g) {
            // 只有连不上才退到 HID；超时说明请求已发出，再用 HID 做一遍可能点两次
            let r = if count > 1 { w.multi_tap(x, y, count).await } else { w.tap(x, y).await };
            match r {
                Ok(()) => return Ok(ictl_l10n::trf!("已点击 {pos}{times}", "Tapped {pos}{times}")),
                Err(WdaError::Unavailable(_)) if view.is_none() => {}
                Err(e) => return Err(Error::wda_input(e)),
            }
        }
        // natively HID is the way, not a fallback: the page's view was checked above as through WDA
        let native = self.native(g);
        if !native && view.is_some() {
            return Err(Error::preflight(wda_down(&g.state, g.info.as_ref())));
        }
        let rot = self.hid_touch_rotation(g).await.map_err(Error::preflight)?;
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                for i in 0..count {
                    if i > 0 {
                        tokio::time::sleep(Duration::from_millis(ictl_wda::MULTI_TAP_GAP_MS)).await;
                    }
                    h.tap(x, y).await?;
                }
                Ok(())
            })
        })
        .await?;
        if native {
            return Ok(ictl_l10n::trf!("已点击 {pos}{times}", "Tapped {pos}{times}"));
        }
        Ok(ictl_l10n::trf!("已点击 {pos}{times}(经 HID)", "Tapped {pos}{times} (via HID)"))
    }

    /// 依次点几个点（`iphone_batch` 的 `one_gesture`）：WDA 发一个手势（[`WdaClient::tap_sequence`]），HID 在同一个会话里连发。
    /// 和 [`Self::tap`] 一样只有连不上 WDA 才退到 HID
    pub(crate) async fn tap_sequence(&self, g: &mut Inner, points: &[(f64, f64)]) -> crate::error::Result<String> {
        if points.is_empty() || points.len() > crate::protocol::MAX_SEQUENCE_POINTS {
            return Err(ictl_l10n::trf!(
                "一个手势要点 1 到 {} 个点",
                "a gesture takes 1 to {} points",
                crate::protocol::MAX_SEQUENCE_POINTS
            )
            .into());
        }
        self.prepare_coordinates(g, None, true).await?;
        let n = points.len();
        let list: Vec<String> = points.iter().map(|(x, y)| format!("({},{})", fmt_g(*x), fmt_g(*y))).collect();
        let list = list.join(" ");
        if let Some(w) = self.wda(g) {
            match w.tap_sequence(points).await {
                Ok(()) => {
                    return Ok(ictl_l10n::trf!(
                        "在一个手势里依次点了 {n} 个点:{list}",
                        "Tapped {n} points in one gesture: {list}"
                    ));
                }
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let native = self.native(g);
        let rot = self.hid_touch_rotation(g).await?;
        let pts = points.to_vec();
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                for (i, (x, y)) in pts.into_iter().enumerate() {
                    if i > 0 {
                        tokio::time::sleep(Duration::from_millis(ictl_wda::SEQUENCE_GAP_MS)).await;
                    }
                    h.tap(x, y).await?;
                }
                Ok(())
            })
        })
        .await?;
        if native {
            return Ok(ictl_l10n::trf!("依次点了 {n} 个点:{list}", "Tapped {n} points: {list}"));
        }
        Ok(ictl_l10n::trf!("依次点了 {n} 个点(经 HID):{list}", "Tapped {n} points (via HID): {list}"))
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
                    // no HID fallback below iOS 27: as `send_touch` says it, so the page sends the whole gesture
                    None if !hid_supported(g.info.as_ref()) => {
                        return Err(Response::Error {
                            message: wda_down(&g.state, g.info.as_ref()),
                            retry_as_gesture: true,
                            failure: None,
                        });
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
                        failure: None,
                    });
                }
            },
        };
        h.rotation = rot;
        let result = Box::pin(h.touch(contact, x, y)).await;
        g.hid_used = Some(Instant::now());
        match result {
            // the finger is up once that was sent, not before: an up refused below leaves it down, to be lifted
            Ok(()) if !contact => g.dev.touch_rotation = None,
            Ok(()) => {}
            // refused before anything was sent (a point that isn't a number, an unknown screen size): the tunnel and
            // its sessions are fine; a finger that is down stays where it was
            Err(e @ ictl_device::DeviceError::InvalidInput(_)) => {
                if phase == TouchPhase::Down {
                    g.dev.touch_rotation = None;
                }
                return Err(Response::error(e.to_string()).with_failure(Delivery::NotSent, FailureStage::Preflight));
            }
            Err(e) => {
                // A failed write may still have reached the phone.
                g.dev.touch_rotation = None;
                drop_tunnel(g).await;
                return Err(Response::error(ictl_l10n::trf!("HID 命令失败: {e}", "HID command failed: {e}")));
            }
        }
        Ok(())
    }

    /// 边滑边找的一次慢滑（[`search_stroke`]），等停下后读一棵新树。`x`：在这个横坐标上滑（iPad 分栏界面的左栏），
    /// 不给就是屏幕正中
    pub(super) async fn search_scroll(&self, g: &mut Inner, down: bool, x: Option<f64>) -> crate::error::Result<Tree> {
        let screen = self.screen(g).await.map_err(Error::preflight)?;
        let (mut from, mut to) = search_stroke(screen.width, screen.height, down);
        if let Some(x) = x {
            (from.0, to.0) = (x, x);
        }
        Box::pin(self.swipe(g, Stroke { from, to, seconds: SEARCH_SWIPE_SECONDS, hold: 0.0 }, None, false)).await?;
        // the read that saw the list stop is the screen now: not read again (about 0.3 s a swipe, 8 to 15 a search)
        if let Some(tree) = self.swiped(g).await {
            g.elements_cache = Some((Instant::now(), tree.clone()));
            g.dev.seen.saw_tree(&tree);
            return Ok(tree);
        }
        self.tree(g, Duration::ZERO).await
    }

    /// The tree read as the last swipe's list stopped, if still fresh ([`SWIPED_TREE_FRESH`]); taken. Under native
    /// control that read was of the app alone: what SpringBoard shows over it (an alert) is read now
    async fn swiped(&self, g: &mut Inner) -> Option<Tree> {
        let fresh = |at: &Instant| at.elapsed() < SWIPED_TREE_FRESH;
        if let Some((_, tree)) = g.swiped_tree.take().filter(|(at, _)| fresh(at)) {
            return Some(tree);
        }
        let (_, snap) = g.swiped_snap.take().filter(|(at, _)| fresh(at))?;
        Box::pin(self.with_overlays(g, snap)).await.ok()
    }

    pub(crate) async fn tap_element(
        &self,
        g: &mut Inner,
        text: &str,
        index: usize,
        max_age: Duration,
        scroll: Option<crate::protocol::ScrollSearch>,
        may_continue: &mut dyn FnMut() -> bool,
    ) -> crate::error::Result<(String, bool)> {
        self.ensure_unlocked(g).await.map_err(Error::preflight)?;
        let mut tree = self.tree(g, max_age).await.map_err(Error::preflight)?;
        // 屏幕上没有、又要边滑边找：慢滑一小段（不带惯性，不会把要找的甩过去）、等停下再读树，
        // 找到文字完全相同的或滑到底为止。一路上记下第一次看到包含它的是滑了几次的时候
        let mut scrolled = 0;
        let mut back = 0;
        let mut at_end = false;
        if let Some(s) = scroll {
            let failed_scroll = |e: Error, completed: u32| {
                let message = ictl_l10n::trf!(
                    "查找目标时滚动失败（此前已完成 {completed} 次滑动），尚未点击目标；请查看屏幕：{e}",
                    "Scroll search failed after {completed} completed swipes; the target was not tapped. Inspect the screen: {e}"
                );
                e.with_message(message)
            };
            let mut partial_at = None;
            loop {
                if reachable_exact(&tree, text, index) {
                    break;
                }
                if partial_at.is_none() && reachable(&tree, text, index) {
                    partial_at = Some(scrolled);
                }
                if at_end || scrolled >= s.max {
                    break;
                }
                // the task was paused or the caller went away: no more swiping, and nothing tapped
                if !may_continue() {
                    return Err(Error::preflight(stopped_searching(scrolled)));
                }
                let next =
                    Box::pin(self.search_scroll(g, s.down, None)).await.map_err(|e| failed_scroll(e, scrolled))?;
                scrolled += 1;
                at_end = super::scroll::unchanged_viewport(&tree, &next);
                tree = next;
            }
            // 没有文字完全相同的，包含它的又已经滑过去了：往回滑到看到它的地方（多滑一次，前后两屏有重叠）
            if !reachable_exact(&tree, text, index)
                && !reachable(&tree, text, index)
                && let Some(k) = partial_at
            {
                while back <= scrolled - k && !reachable(&tree, text, index) {
                    if !may_continue() {
                        return Err(Error::preflight(stopped_searching(scrolled + back)));
                    }
                    tree = Box::pin(self.search_scroll(g, !s.down, None))
                        .await
                        .map_err(|e| failed_scroll(e, scrolled + back))?;
                    back += 1;
                }
            }
        }
        let searched = match (scrolled, at_end, back) {
            (0, ..) => String::new(),
            (n, _, b @ 1..) => ictl_l10n::trf!(
                "(滑了 {n} 次没有文字完全相同的,又往回滑了 {b} 次)",
                " (scrolled {n} times without an exact match, then {b} back)"
            ),
            (n, true, 0) => ictl_l10n::trf!("(滑了 {n} 次,已到尽头)", " (scrolled {n} times, reached the end)"),
            (n, false, 0) => ictl_l10n::trf!("(滑了 {n} 次)", " (scrolled {n} times)"),
        };
        // 边滑边找了、最后点的却不是文字完全相同的：明说，Agent 看得出这和它要找的不一样
        let partial_only = scroll.is_some() && !reachable_exact(&tree, text, index);
        // 点之前刚读的树就是操作前的界面（比操作开始前记下的更新）
        g.dev.before_action = Some(std::mem::take(&mut g.dev.seen));
        let matches = elements::find(&tree.elements, text);
        if matches.is_empty() {
            let limited = self.snapshot_note(g);
            let hint = if scroll.is_some() {
                String::new()
            } else {
                ictl_l10n::tr!(
                    ";可能在屏幕外,可带 scroll 边滑边找",
                    "; it may be off screen: pass scroll to look further"
                )
                .to_owned()
            };
            return Ok((
                ictl_l10n::trf!(
                    "{}包含 \"{text}\" 的元素{searched},先用 iphone_list_elements 或截图看看屏幕{limited}{hint}",
                    "{}: no element contains \"{text}\"{searched}; look at the screen with iphone_list_elements or a screenshot first{limited}{hint}",
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
        // paused, taken over or left while the tree was read or the list moved: nothing is tapped
        if !may_continue() {
            return Err(Error::preflight(stopped_searching(scrolled + back)));
        }
        self.tap_at(g, x, y).await?;
        let note = if matches.len() > 1 {
            ictl_l10n::trf!("(共匹配 {} 个,点的是第 {index} 个)", " ({} matches, tapped number {index})", matches.len())
        } else {
            String::new()
        };
        let found = match (scrolled, partial_only) {
            (0, false) => String::new(),
            (0, true) => {
                ictl_l10n::tr!("(没有文字完全相同的,点的是包含它的)", " (no exact match, tapped one that contains it)")
                    .to_owned()
            }
            (_, true) => ictl_l10n::trf!(
                "{searched}(没有文字完全相同的,点的是包含它的)",
                "{searched} (no exact match, tapped one that contains it)"
            ),
            (n, false) => ictl_l10n::trf!("(滑了 {n} 次后找到)", " (found after {n} scrolls)"),
        };
        Ok((ictl_l10n::trf!("已点击 {}{found}{note}{moved}", "Tapped {}{found}{note}{moved}", e.describe()), false))
    }

    pub(crate) async fn long_press(
        &self,
        g: &mut Inner,
        x: f64,
        y: f64,
        seconds: f64,
        view: Option<&str>,
    ) -> crate::error::Result<String> {
        self.prepare_coordinates(g, view, true).await.map_err(Error::preflight)?;
        // as for a tap: without WDA (or when it can't be reached) by HID where the phone has it, unless the point was
        // measured on the page's view through WDA
        let by_hid = match self.route(g) {
            Route::Wda(w) => match w.touch_and_hold(x, y, seconds).await {
                Ok(()) => false,
                Err(WdaError::Unavailable(_)) if view.is_none() && hid_supported(g.info.as_ref()) => true,
                Err(e) => return Err(Error::wda_input(e)),
            },
            Route::Native => true,
            Route::Hid if view.is_none() => true,
            Route::Hid | Route::Unavailable => {
                return Err(Error::preflight(wda_down(&g.state, g.info.as_ref())));
            }
        };
        if by_hid {
            Box::pin(self.native_long_press(g, x, y, seconds)).await?;
        }
        let (px, py, s) = (fmt_g(x), fmt_g(y), fmt_g(seconds));
        // said like the other gestures when HID stood in for WDA (natively it is the way, not a fallback)
        if by_hid && !self.native(g) {
            return Ok(ictl_l10n::trf!(
                "已长按 ({px},{py}) {s} 秒(经 HID)",
                "Long-pressed ({px},{py}) for {s} s (via HID)"
            ));
        }
        Ok(ictl_l10n::trf!("已长按 ({px},{py}) {s} 秒", "Long-pressed ({px},{py}) for {s} s"))
    }

    pub(crate) async fn swipe(
        &self,
        g: &mut Inner,
        stroke: Stroke,
        view: Option<&str>,
        watched: bool,
    ) -> crate::error::Result<String> {
        let Stroke { from, to, seconds, hold } = stroke;
        self.prepare_coordinates(g, view, true).await.map_err(Error::preflight)?;
        let mut desc = format!("({},{}) → ({},{})", fmt_g(from.0), fmt_g(from.1), fmt_g(to.0), fmt_g(to.1));
        if hold > 0.0 {
            desc = ictl_l10n::trf!("{desc},先按住 {} 秒", "{desc}, held {} s first", fmt_g(hold));
        }
        if let Some(w) = self.wda(g) {
            match w.swipe_into_polling(from, to, (hold * 1000.0) as u64, (seconds * 1000.0) as u64).await {
                // 滑动请求不等惯性滚动（等的话设置不对能卡十几秒），这里等列表停下再返回：
                // 列表还在滚时下一次点击只会让它停住，点不到东西
                // someone watches the screen ([`crate::protocol::WATCHED`]): they see when it stops
                Ok(()) if watched => {
                    w.end_polling().await;
                    g.swiped_tree = None;
                    g.swiped_snap = None;
                    return Ok(ictl_l10n::trf!("已滑动 {desc}", "Swiped {desc}"));
                }
                Ok(()) => {
                    let vertical = (to.1 - from.1).abs() >= (to.0 - from.0).abs();
                    let (note, tree) = wait_scrolled(w, vertical).await;
                    g.swiped_tree = tree.map(|t| (Instant::now(), t));
                    g.swiped_snap = None;
                    return Ok(ictl_l10n::trf!("已滑动 {desc}{note}", "Swiped {desc}{note}"));
                }
                Err(WdaError::Unavailable(_)) if view.is_none() => {}
                Err(e) => return Err(Error::wda_input(e)),
            }
        }
        let native = self.native(g);
        if view.is_some() && !native {
            return Err(Error::preflight(wda_down(&g.state, g.info.as_ref())));
        }
        let rot = self.hid_touch_rotation(g).await.map_err(Error::preflight)?;
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                if hold > 0.0 { h.held_drag(from, to, hold, seconds).await } else { h.drag(from, to, seconds).await }
            })
        })
        .await?;
        if !native {
            return Ok(ictl_l10n::trf!("已滑动 {desc}(经 HID)", "Swiped {desc} (via HID)"));
        }
        // as through WDA: a list still scrolling only stops at the next tap; someone watching sees when it stops
        g.swiped_tree = None;
        g.swiped_snap = None;
        let note = if watched {
            String::new()
        } else {
            let vertical = (to.1 - from.1).abs() >= (to.0 - from.0).abs();
            let (note, snap) = Box::pin(self.native_scrolled(g, vertical)).await;
            g.swiped_snap = snap.map(|s| (Instant::now(), s));
            note
        };
        Ok(ictl_l10n::trf!("已滑动 {desc}{note}", "Swiped {desc}{note}"))
    }

    pub(crate) async fn type_text(
        &self,
        g: &mut Inner,
        text: &str,
        submit: bool,
        field_verified: &mut Option<bool>,
    ) -> crate::error::Result<String> {
        self.ensure_unlocked(g).await.map_err(Error::preflight)?;
        let n = text.chars().count();
        *field_verified = (!text.is_empty()).then_some(false);
        if let Some(w) = self.wda(g) {
            // /wda/keys 没有输入框获得焦点时也返回成功，其实什么都没输：先看屏幕上有没有键盘
            let mut before = None;
            let typed = match w.keyboard_shown().await {
                Ok(false) => {
                    return Err(Error::preflight(ictl_l10n::tr!(
                        "屏幕上没有键盘(没有输入框获得焦点),什么都没输;先点一下输入框,截图确认键盘出来了再输入",
                        "No keyboard on screen (no text field has focus), nothing typed; tap a text field first and check with a screenshot that the keyboard is up"
                    )));
                }
                Ok(true) if text.is_empty() => Ok(()),
                Ok(true) => {
                    if text.chars().any(|c| !c.is_control()) {
                        before = readable_focus(w).await;
                    }
                    w.keys(text).await
                }
                Err(e @ WdaError::Unavailable(_)) => Err(e),
                Err(e) => return Err(Error::preflight(e)),
            };
            match typed {
                Ok(()) => {
                    // 有的 App 边输入边刷新输入框，会吞掉一部分字（高德搜索框「故宫博物院」只剩「故宫博」），WDA 照样报成功：
                    // 输完读回输入框核对，少了结尾几个字就补上；对不上就不按回车
                    let field = verify_typed(w, before.as_ref(), text).await;
                    if !text.is_empty() {
                        *field_verified = Some(matches!(field, Typed::Ok | Typed::Fixed(_)));
                    }
                    let note = match field {
                        Typed::Ok => String::new(),
                        Typed::Unknown if text.chars().any(|c| !c.is_control()) => ictl_l10n::tr!(
                            "(无法读回输入框，请查看屏幕确认)",
                            " (field contents could not be verified; inspect the screen)"
                        )
                        .into(),
                        Typed::Unknown => String::new(),
                        Typed::Fixed(k) => ictl_l10n::trf!(
                            "(第一次有 {k} 个字没输进去,已补上)",
                            " ({k} characters were dropped the first time and typed again)"
                        ),
                        Typed::FocusChanged => {
                            return Err(Error::readback(ictl_l10n::tr!(
                                "输入后焦点已改变，请查看屏幕确认；未再输入或按回车",
                                "Focus changed after typing. Inspect the screen; no further keys or Return were sent."
                            )));
                        }
                        Typed::Mismatch(now) => {
                            return Err(Error::readback(ictl_l10n::trf!(
                                "输入了 {n} 个字符,但输入框里现在是「{now}」,和要输入的「{text}」对不上(App 可能吞掉了部分字符){};截图确认后再处理",
                                "Typed {n} characters, but the field now contains “{now}”, which doesn't match “{text}” (the app may have swallowed some characters){}; check with a screenshot before going on",
                                if submit { ictl_l10n::tr!(",没有按回车", ", Return not pressed") } else { "" }
                            )));
                        }
                    };
                    if submit {
                        w.keys("\n").await.map_err(|e| {
                            if text.is_empty() {
                                return Error::wda_input(e);
                            }
                            Error::action(
                                Delivery::Sent,
                                FailureStage::Send,
                                ictl_l10n::trf!(
                                    "文字已输入{note},按回车失败:{e}",
                                    "Text typed{note}, pressing Return failed: {e}"
                                ),
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
                Err(e) => return Err(Error::wda_input(e)),
            }
        }
        if self.native(g) {
            return Box::pin(self.native_type(g, text, submit, field_verified)).await;
        }
        let payload = if submit { format!("{text}\n") } else { text.to_owned() };
        // 美式键盘按得出来的直接按键，不动剪贴板
        if payload.chars().all(|c| ascii_to_usage(c).is_some()) {
            self.hid_do(g, move |h| Box::pin(async move { h.type_ascii(&payload).await })).await?;
            return Ok(ictl_l10n::trf!("已输入 {n} 个字符(经 HID)", "Typed {n} characters (via HID)"));
        }
        self.hid_paste(g, text, submit).await
    }

    /// Replaces the text of the focused field (`replace: true`; empty `text` empties it), and presses Return only once
    /// the field reads back as exactly `text`: through WDA ([`wda_replace`]), or natively ([`Daemon::native_replace`])
    pub(crate) async fn replace_text(
        &self,
        g: &mut Inner,
        text: &str,
        submit: bool,
    ) -> crate::error::Result<(String, Delivery)> {
        self.ensure_unlocked(g).await.map_err(Error::preflight)?;
        let native = self.native(g);
        if let Some(w) = self.wda(g) {
            match wda_replace(w, text, submit).await {
                Ok(m) => return Ok(m),
                Err(WdaReplace::Failed(e)) => return Err(e),
                // WDA went away before anything was changed
                Err(WdaReplace::Untouched(e)) if !native => return Err(Error::wda_input(e)),
                Err(WdaReplace::Untouched(_)) => {}
            }
        }
        if native {
            return Box::pin(self.native_replace(g, text, submit)).await;
        }
        // no HID way: the field can't be read without WDA
        Err(Error::preflight(wda_down(&g.state, g.info.as_ref())))
    }

    /// HID 输入中文等按不出来的文字：经剪贴板粘贴（[`ictl_device::Tunnel::paste_text`]）。
    ///
    /// 手机会弹「允许粘贴」，WDA 不可用时读不到界面、点不了，交给调用方截图去点；剪贴板一改弹窗就消失，所以原来的内容
    /// 先存在守护进程里，[`PASTE_RESTORE_AFTER`] 后由巡检写回（下一次粘贴前、换设备前也会写回，见 [`Daemon::restore_pasteboard`]）。
    /// 弹窗还在时按回车会选中「不允许粘贴」，所以要回车也不按，让调用方粘上以后再按
    async fn hid_paste(&self, g: &mut Inner, text: &str, submit: bool) -> crate::error::Result<String> {
        let n = text.chars().count();
        self.restore_pasteboard(g, false).await;
        self.hid(g).await.map_err(|e| Error::action(Delivery::NotSent, FailureStage::Connect, e))?;
        let r = Box::pin(g.tunnel.as_mut().expect("hid() 刚建好隧道").paste_text(text)).await;
        g.hid_used = Some(Instant::now());
        let pending = match r {
            Ok(p) => p,
            Err(e @ (DeviceError::InvalidInput(_) | DeviceError::PasteNotSent(_))) => return Err(Error::preflight(e)),
            Err(e) => {
                drop_tunnel(g).await;
                return Err(Error::action(
                    Delivery::Unknown,
                    FailureStage::Send,
                    ictl_l10n::trf!("HID 粘贴失败:{e}", "HID paste failed: {e}"),
                ));
            }
        };
        let saved = pending.is_some();
        g.dev.pending_paste = pending.map(|p| (Instant::now(), p));
        let secs = PASTE_RESTORE_AFTER.as_secs();
        let clipboard = if saved {
            ictl_l10n::trf!(
                "剪贴板原来的内容 {secs} 秒后还原,还原后这个弹窗会消失(那时还没点就要重新输入)",
                "the pasteboard's old contents are put back in {secs} s, which closes that prompt (if it is still up then, type the text again)"
            )
        } else {
            ictl_l10n::tr!(
                "剪贴板原来的内容读不出来或不能原样写回,之后剪贴板里留着这段文字",
                "the pasteboard's old contents couldn't be read or written back as they were, so it keeps this text"
            )
            .into()
        };
        let enter = if submit {
            ictl_l10n::tr!(
                "。没有按回车(弹窗还在时回车会选中「不允许粘贴」):粘上以后用 iphone_type_text 输入 \"\\n\" 按回车",
                ". Return wasn't pressed (with the prompt up it would choose “Don't Allow Paste”): once the text is in, type \"\\n\" with iphone_type_text"
            )
        } else {
            ""
        };
        Ok(ictl_l10n::trf!(
            "已按 Cmd-V 粘贴 {n} 个字符(经 HID 和剪贴板;WDA 不可用,读不到界面)。手机会弹「…想从“dtpasteboardd”粘贴」:截图,点「允许粘贴」文字才会进去(点「不允许粘贴」就不粘),再截图确认;{clipboard}{enter}",
            "Pressed Cmd-V to paste {n} characters (via HID and the pasteboard; WDA is unavailable, so the screen can't be read). The phone asks “… would like to paste from dtpasteboardd”: take a screenshot and tap “Allow Paste” to let the text in (“Don't Allow Paste” keeps it out), then check with another screenshot; {clipboard}{enter}"
        ))
    }

    /// 把 HID 粘贴前存下的剪贴板写回去（[`Self::hid_paste`]）。`due_only`：只在粘贴过了 [`PASTE_RESTORE_AFTER`] 时写
    /// （巡检用）。写回失败不重试：剪贴板不归我们管，留着粘贴的文字比反复改它好
    pub(crate) async fn restore_pasteboard(&self, g: &mut Inner, due_only: bool) {
        let Some((at, _)) = &g.dev.pending_paste else { return };
        if due_only && at.elapsed() < PASTE_RESTORE_AFTER {
            return;
        }
        let Some((_, p)) = g.dev.pending_paste.take() else { return };
        let Some(device) = g.device.clone() else { return };
        let r = match self.ensure_tunnel(g, &device).await {
            Ok(t) => Box::pin(t.restore_pasteboard(p)).await,
            Err(e) => ClipboardRestore::Failed(e.to_string()),
        };
        match r {
            ClipboardRestore::Restored => tracing::info!(
                "{}",
                ictl_l10n::log_tr!("HID 粘贴前的剪贴板已还原", "Pasteboard from before the HID paste restored")
            ),
            ClipboardRestore::ChangedMeanwhile => tracing::info!(
                "{}",
                ictl_l10n::log_tr!(
                    "HID 粘贴后剪贴板被改过，没有还原",
                    "The pasteboard changed after the HID paste; not restored"
                )
            ),
            ClipboardRestore::Failed(e) => tracing::warn!(
                "{}",
                ictl_l10n::log_trf!(
                    "还原 HID 粘贴前的剪贴板失败：{e}",
                    "Restoring the pasteboard from before the HID paste failed: {e}"
                )
            ),
        }
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

/// Why a replacement through WDA ([`wda_replace`]) stopped
pub(super) enum WdaReplace {
    /// WDA didn't take a request before anything was changed: natively it can still be done
    Untouched(WdaError),
    Failed(crate::error::Error),
}

/// After a replacement through WDA, the field is read back this many times, [`READBACK_EVERY`] apart, until it holds
/// the text (the tree may lag the field a little)
const READBACK_TRIES: u32 = 5;
const READBACK_EVERY: Duration = Duration::from_millis(300);

/// Field types that can be replaced: readable text. A password field can't be read back, so it is refused
const REPLACEABLE: [&str; 3] = ["TextField", "SearchField", "TextView"];

/// Replaces the focused field's text through WDA: the focused element (`/element/active`), WDA's clear, the new
/// text as keys, and the same element read back; Return only once it holds exactly `text`. Unlike native control
/// (select all, then paste over the selection), the old text is gone once cleared: a failure after that says so
pub(super) async fn wda_replace<T: ictl_wda::http::Transport>(
    w: &mut WdaClient<T>,
    text: &str,
    submit: bool,
) -> Result<(String, Delivery), WdaReplace> {
    use WdaReplace::{Failed, Untouched};
    // not sent: nothing changed
    let untouched = |e: WdaError| match e {
        WdaError::Unavailable(_) => Untouched(e),
        e => Failed(Error::preflight(e)),
    };
    if !w.keyboard_shown().await.map_err(untouched)? {
        return Err(Failed(Error::preflight(ictl_l10n::tr!(
            "屏幕上没有键盘，先点一下要替换的输入框",
            "No keyboard on screen; tap the field you want to replace first"
        ))));
    }
    let field = match w.focused_field().await.map_err(untouched)? {
        Some(f) if REPLACEABLE.contains(&f.kind.as_str()) => f,
        Some(f) if f.kind == "SecureTextField" => {
            return Err(Failed(Error::preflight(ictl_l10n::tr!(
                "获得焦点的是密码框，读不出内容、核对不了，没有替换",
                "The focused field is a password field: it can't be read back, so nothing was replaced"
            ))));
        }
        Some(f) => {
            return Err(Failed(Error::preflight(ictl_l10n::trf!(
                "获得焦点的是 {}，不是普通输入框，没有替换",
                "The focused element is a {}, not a text field; nothing was replaced",
                f.kind
            ))));
        }
        None => {
            return Err(Failed(Error::preflight(ictl_l10n::tr!(
                "无法确认当前聚焦的是可读的普通输入框，没有替换文字",
                "Couldn't identify a readable focused text field; no text was replaced"
            ))));
        }
    };
    let old = w.field_text(&field).await.map_err(untouched)?;
    let mut note = String::new();
    if old != text {
        let gone = ictl_l10n::tr!("原来的文字已经删掉了", "the old text is gone");
        let not_submitted = if submit { ictl_l10n::tr!("，没有按回车", ", Return not pressed") } else { "" };
        if !old.is_empty() {
            w.clear(&field.id).await.map_err(|e| match e {
                WdaError::Unavailable(_) => Untouched(e),
                e => {
                    let message = ictl_l10n::trf!(
                        "清空输入框失败（{e}），原来的文字可能已经删掉了一部分{not_submitted}；截图确认",
                        "Emptying the field failed ({e}); some of the old text may be gone{not_submitted}; check with a screenshot"
                    );
                    Failed(Error::wda_input(e).with_message(message))
                }
            })?;
        }
        if !text.is_empty() {
            w.keys(text).await.map_err(|e| {
                let message = ictl_l10n::trf!(
                    "{gone}，输入新文字失败（{e}）{not_submitted}；截图确认",
                    "{gone}, typing the new text failed ({e}){not_submitted}; check with a screenshot"
                );
                let cause = Error::wda_input(e);
                Failed(if old.is_empty() {
                    cause.with_message(message)
                } else {
                    Error::action(Delivery::Unknown, FailureStage::Send, message)
                })
            })?;
        }
        let unread = |e: WdaError| {
            Failed(Error::readback(ictl_l10n::trf!(
                "{gone}，替换后读不回输入框（{e}）{not_submitted}；截图确认",
                "{gone}, and the field couldn't be read back after the replacement ({e}){not_submitted}; check with a screenshot"
            )))
        };
        let mut now = String::new();
        for attempt in 0..READBACK_TRIES {
            if attempt > 0 {
                tokio::time::sleep(READBACK_EVERY).await;
            }
            now = w.field_text(&field).await.map_err(unread)?;
            if now == text {
                break;
            }
        }
        // the app dropped the last characters (as typing sees, `verify_typed`): typed again one by one
        if now != text
            && let Some(rest) = repairable_suffix(text, &now)
        {
            for c in rest.chars() {
                w.keys(&c.to_string()).await.map_err(unread)?;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let k = rest.chars().count();
            note = ictl_l10n::trf!(
                "（第一次有 {k} 个字没输进去，已补上）",
                " ({k} characters were dropped the first time and typed again)"
            );
            now = w.field_text(&field).await.map_err(unread)?;
        }
        if now != text {
            return Err(Failed(Error::readback(ictl_l10n::trf!(
                "{gone}，但输入框里现在是「{now}」，和要替换成的「{text}」对不上{not_submitted}；截图确认后再处理",
                "{gone}, but the field now holds “{now}”, which doesn't match “{text}”{not_submitted}; check with a screenshot before going on"
            ))));
        }
    }
    if submit {
        w.keys("\n").await.map_err(|e| {
            let message = ictl_l10n::trf!(
                "已替换为「{text}」{note}，按回车失败：{e}",
                "Replaced the field with “{text}”{note}, pressing Return failed: {e}"
            );
            Failed(if old == text {
                Error::wda_input(e).with_message(message)
            } else {
                Error::action(Delivery::Sent, FailureStage::Send, message)
            })
        })?;
    }
    let delivery = if old != text || submit { Delivery::Sent } else { Delivery::NotSent };
    Ok((replaced(text, submit, &note), delivery))
}

/// What a replacement that went through says ([`Daemon::replace_text`])
pub(super) fn replaced(text: &str, submit: bool, note: &str) -> String {
    let n = text.chars().count();
    match (n, submit) {
        (0, true) => ictl_l10n::trf!("已清空输入框{note}，并按了回车", "Emptied the field{note} and pressed Return"),
        (0, false) => ictl_l10n::trf!("已清空输入框{note}", "Emptied the field{note}"),
        (_, true) => ictl_l10n::trf!(
            "已替换为 {n} 个字符{note}，并按了回车",
            "Replaced the field with {n} characters{note} and pressed Return"
        ),
        (_, false) => ictl_l10n::trf!("已替换为 {n} 个字符{note}", "Replaced the field with {n} characters{note}"),
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
    FocusChanged,
}

/// Verification requires the entire requested insertion in the observed field. Existing digits or
/// an unrelated field changing cannot supply part of the evidence. Phone numbers may gain display spaces.
pub(super) fn newly_typed(before: &str, now: &str, text: &str) -> bool {
    fn inserted(before: &str, now: &str, text: &str) -> bool {
        if text.is_empty() || now.len().checked_sub(before.len()) != Some(text.len()) {
            return false;
        }
        now.match_indices(text)
            .any(|(i, _)| before.get(..i) == Some(&now[..i]) && before.get(i..) == Some(&now[i + text.len()..]))
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        let digits = |s: &str| s.split_whitespace().collect::<String>();
        inserted(&digits(before), &digits(now), text)
    } else {
        inserted(before, now, text)
    }
}

/// The text arrived, but not the whitespace it was sent with: a single-line field turns a pasted newline into a
/// space («北京\n» reads «北京 », «北京\n上海» reads «北京 上海»; Spotlight, iPhone 15 Pro Max, iOS 27.2.1), and leading
/// or trailing whitespace can be dropped. [`newly_typed`] with all whitespace left out
pub(super) fn typed_but_whitespace(before: &str, now: &str, text: &str) -> bool {
    let text = bare(text);
    !text.is_empty() && newly_typed(&bare(before), &bare(now), &text)
}

/// `s` without its whitespace
pub(super) fn bare(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Repair only an observed append of the exact leading characters, stable across two fresh reads.
pub(super) fn missing_suffix<'a>(before: &str, first: &str, second: &str, text: &'a str) -> Option<&'a str> {
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        let strip_spaces = |s: &str| s.split_whitespace().collect::<String>();
        let first = strip_spaces(first);
        let second = strip_spaces(second);
        if first != second {
            return None;
        }
        return repairable_suffix(text, second.strip_prefix(&strip_spaces(before))?);
    }
    if first != second {
        return None;
    }
    repairable_suffix(text, second.strip_prefix(before)?)
}

/// A missing Return or Tab can mean the first one already submitted or moved focus.
fn repairable_suffix<'a>(text: &'a str, observed: &str) -> Option<&'a str> {
    if observed.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    text.strip_prefix(observed).filter(|rest| !rest.is_empty() && !rest.chars().any(char::is_whitespace))
}

/// The focused field before typing: which element it is, its placeholder, and its exact value, whitespace included
pub(super) struct Focus {
    field: ictl_wda::client::FocusedField,
    value: String,
}

/// Four requests: the element, its type and placeholder ([`WdaClient::focused_field`]), its value. [`reread`] takes two
async fn readable_focus<T: ictl_wda::http::Transport>(w: &mut WdaClient<T>) -> Option<Focus> {
    let field = w.focused_field().await.ok()??;
    if !REPLACEABLE.contains(&field.kind.as_str()) {
        return None;
    }
    let value = w.field_text(&field).await.ok()?;
    Some(Focus { field, value })
}

/// What [`reread`] found
enum Reread {
    Value(String),
    /// Another readable field has focus now
    Moved,
    /// No readable field has focus, or WDA didn't answer
    Unreadable,
}

/// The field [`readable_focus`] found, once more. Its type and placeholder don't change, so only which element has
/// focus and its value are asked: two requests instead of four (each about 0.15 s through WDA on an iPad mini 5)
async fn reread<T: ictl_wda::http::Transport>(w: &mut WdaClient<T>, f: &Focus) -> Reread {
    match w.active_element().await {
        Ok(Some(id)) if id == f.field.id => {}
        // as `readable_focus` judges it: another text field (focus changed) or anything else (can't tell)
        Ok(Some(_)) => {
            return match w.focused_field().await {
                Ok(Some(o)) if REPLACEABLE.contains(&o.kind.as_str()) => Reread::Moved,
                _ => Reread::Unreadable,
            };
        }
        Ok(None) | Err(_) => return Reread::Unreadable,
    }
    w.field_text(&f.field).await.map_or(Reread::Unreadable, Reread::Value)
}

async fn verify_typed<T: ictl_wda::http::Transport>(w: &mut WdaClient<T>, before: Option<&Focus>, text: &str) -> Typed {
    let Some(f) = before else { return Typed::Unknown };
    let before = f.value.as_str();
    let mut reads = Vec::with_capacity(2);
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let now = match reread(w, f).await {
            Reread::Value(now) => now,
            Reread::Moved => return Typed::FocusChanged,
            Reread::Unreadable => return Typed::Unknown,
        };
        if newly_typed(before, &now, text) {
            return Typed::Ok;
        }
        if typed_but_whitespace(before, &now, text) {
            // The visible text arrived, but the field cannot confirm the whitespace it was sent with.
            return Typed::Unknown;
        }
        if now.contains(text) && (now == before || !now.starts_with(before)) {
            // Replacing a selection (a search field keeps its last search, selected) can't be told apart from the
            // text having been there, and reading again can't tell either: answer now. Reading twice more and waiting
            // in between made typing into Spotlight about 1.8 s slower through WDA (iPad mini 5)
            return Typed::Unknown;
        }
        reads.push(now);
    }
    let Some(rest) = missing_suffix(before, &reads[0], &reads[1], text) else {
        return Typed::Mismatch(reads.pop().unwrap());
    };
    for (i, c) in rest.chars().enumerate() {
        if i > 0 && !w.active_element().await.ok().flatten().is_some_and(|id| id == f.field.id) {
            return Typed::FocusChanged;
        }
        if w.keys(&c.to_string()).await.is_err() {
            return Typed::Mismatch(reads.pop().unwrap());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    match reread(w, f).await {
        Reread::Moved => Typed::FocusChanged,
        Reread::Value(now) if newly_typed(before, &now, text) => Typed::Fixed(rest.chars().count()),
        Reread::Value(now) => Typed::Mismatch(now),
        Reread::Unreadable => Typed::Unknown,
    }
}

/// [`Daemon::tap_element`] stopped before tapping: the task was paused or taken over, or the caller went away
fn stopped_searching(completed: u32) -> String {
    ictl_l10n::trf!(
        "点击前被打断（任务暂停、被接管或调用方已离开），此前已完成 {completed} 次滑动，没有点击；请先查看屏幕",
        "Interrupted before tapping (the task was paused or taken over, or the caller went away) after {completed} swipes; nothing was tapped. Look at the screen first"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A WDA with one focused field
    struct Fake(Mutex<Field>);

    struct Field {
        keyboard: bool,
        kind: &'static str,
        placeholder: &'static str,
        value: String,
        /// characters the app drops from the end of the next keys
        drop_next: usize,
        /// what the app makes of typed text
        mangle: fn(&str) -> String,
        /// the field doesn't answer at all (WDA gone)
        gone: bool,
        /// its text is selected: the next keys replace it (a search field keeps its last search, selected)
        selected: bool,
        /// requests as `METHOD path`, and the text of every `/wda/keys`
        seen: Vec<String>,
        keys: Vec<String>,
    }

    impl Fake {
        fn new(kind: &'static str, value: &str) -> Self {
            Self(Mutex::new(Field {
                keyboard: true,
                kind,
                placeholder: "搜索",
                value: value.into(),
                drop_next: 0,
                mangle: str::to_owned,
                gone: false,
                selected: false,
                seen: vec![],
                keys: vec![],
            }))
        }
    }

    impl ictl_wda::http::Transport for &Fake {
        async fn send(
            &self,
            method: &str,
            path: &str,
            body: Option<Vec<u8>>,
            _: Duration,
        ) -> std::io::Result<(u16, Vec<u8>)> {
            let mut f = self.0.lock().unwrap();
            if f.gone {
                return Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"));
            }
            f.seen.push(format!("{method} {path}"));
            let value = |v: serde_json::Value| Ok((200, serde_json::json!({ "value": v }).to_string().into_bytes()));
            let path = path.strip_prefix("/session/S").unwrap_or(path);
            match (method, path) {
                ("POST", "/session") => Ok((200, br#"{"sessionId":"S"}"#.to_vec())),
                ("POST", "/elements") => {
                    value(if f.keyboard { serde_json::json!([{"ELEMENT": "K"}]) } else { serde_json::json!([]) })
                }
                ("GET", "/element/active") => value(serde_json::json!({"ELEMENT": "F"})),
                ("GET", "/element/F/attribute/type") => value(format!("XCUIElementType{}", f.kind).into()),
                ("GET", "/element/F/attribute/placeholderValue") => value(f.placeholder.into()),
                // an empty field reads its placeholder
                ("GET", "/element/F/attribute/value") => {
                    value(if f.value.is_empty() { f.placeholder.into() } else { f.value.clone().into() })
                }
                ("POST", "/element/F/clear") => {
                    f.value.clear();
                    value(serde_json::Value::Null)
                }
                ("POST", "/wda/keys") => {
                    let body: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
                    let typed: String = body["value"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
                    f.keys.push(typed.clone());
                    if typed != "\n" {
                        if std::mem::take(&mut f.selected) {
                            f.value.clear();
                        }
                        let keep = typed.chars().count().saturating_sub(std::mem::take(&mut f.drop_next));
                        let typed: String = typed.chars().take(keep).collect();
                        let typed = (f.mangle)(&typed);
                        f.value.push_str(&typed);
                    }
                    value(serde_json::Value::Null)
                }
                _ => Ok((404, br#"{"value":{"error":"unknown command","message":"no route"}}"#.to_vec())),
            }
        }
    }

    async fn replace(f: &Fake, text: &str, submit: bool) -> Result<String, String> {
        let mut w = WdaClient::new(f);
        wda_replace(&mut w, text, submit).await.map(|(message, _)| message).map_err(|e| match e {
            WdaReplace::Untouched(e) => format!("untouched: {e}"),
            WdaReplace::Failed(e) => e.to_string(),
        })
    }

    #[tokio::test]
    async fn a_single_line_field_never_receives_a_second_return_during_repair() {
        // a newline dropped, or turned into a space (Spotlight does that with pasted text)
        let mangles: [fn(&str) -> String; 2] = [|s| s.trim_end().to_owned(), |s| s.replace('\n', " ")];
        for mangle in mangles {
            let f = Fake::new("TextField", "");
            f.0.lock().unwrap().mangle = mangle;
            let mut w = WdaClient::new(&f);
            let before = readable_focus(&mut w).await.unwrap();
            w.keys("hi\n").await.unwrap();
            assert_eq!(verify_typed(&mut w, Some(&before), "hi\n").await, Typed::Unknown);
            assert_eq!(f.0.lock().unwrap().keys, ["hi\n"]);
        }

        let f = Fake::new("TextField", "");
        f.0.lock().unwrap().mangle = |s| s.trim_end().to_owned();
        assert!(replace(&f, "hi\n", false).await.is_err());
        assert_eq!(f.0.lock().unwrap().keys, ["hi\n"]);
    }

    /// Typing over a selection can't be told apart from the text having been there, and reading again can't tell
    /// either: one reread of the field already known (which element has focus, its value) answers, with no wait and no
    /// second read (they made typing into Spotlight about 1.8 s slower through WDA on an iPad mini 5)
    #[tokio::test]
    async fn typing_over_a_selection_is_judged_after_one_reread() {
        for (old, text) in [("设置", "设置"), ("wifi", "设置")] {
            let f = Fake::new("SearchField", old);
            f.0.lock().unwrap().selected = true;
            let mut w = WdaClient::new(&f);
            let before = readable_focus(&mut w).await.unwrap();
            w.keys(text).await.unwrap();
            let asked = f.0.lock().unwrap().seen.len();
            assert_eq!(verify_typed(&mut w, Some(&before), text).await, Typed::Unknown);
            let field = f.0.into_inner().unwrap();
            assert_eq!(field.value, text);
            assert_eq!(
                field.seen[asked..],
                ["GET /session/S/element/active", "GET /session/S/element/F/attribute/value"]
            );
        }
    }

    #[tokio::test]
    async fn replaces_the_focused_field_then_presses_return() {
        let f = Fake::new("SearchField", "wifi");
        assert_eq!(replace(&f, "蓝牙", true).await.unwrap(), "已替换为 2 个字符，并按了回车");
        let f = f.0.into_inner().unwrap();
        assert_eq!(f.value, "蓝牙");
        assert_eq!(f.keys, ["蓝牙", "\n"]);
        assert!(f.seen.contains(&"POST /session/S/element/F/clear".to_owned()), "{:?}", f.seen);
    }

    #[tokio::test]
    async fn empty_text_empties_the_field() {
        let f = Fake::new("TextField", "wifi");
        assert_eq!(replace(&f, "", false).await.unwrap(), "已清空输入框");
        let f = f.0.into_inner().unwrap();
        assert_eq!((f.value.as_str(), f.keys.len()), ("", 0));
    }

    #[tokio::test]
    async fn replacement_reports_noop_and_field_mismatch_separately() {
        let f = Fake::new("TextField", "same");
        let mut w = WdaClient::new(&f);
        for (text, submit, expected) in
            [("same", false, Delivery::NotSent), ("same", true, Delivery::Sent), ("", false, Delivery::Sent)]
        {
            let Ok((_, delivery)) = wda_replace(&mut w, text, submit).await else { panic!("replacement failed") };
            assert_eq!(delivery, expected);
        }
        f.0.lock().unwrap().mangle = |s| s.to_uppercase();
        let Err(WdaReplace::Failed(e)) = wda_replace(&mut w, "mismatch", true).await else {
            panic!("expected a verification failure")
        };
        let response = e.response();
        let req = crate::protocol::Request::ReplaceText { text: "mismatch".into(), submit: true };
        assert_eq!(response.delivery(&req), Delivery::Sent);
        assert_eq!(response.field_readback(&req), Some("failed"));
    }

    /// The field already holds the text (or, emptying, reads its placeholder): nothing is cleared or typed
    #[tokio::test]
    async fn a_field_holding_the_text_is_left_alone() {
        for (value, text) in [("wifi", "wifi"), ("", "")] {
            let f = Fake::new("SearchField", value);
            replace(&f, text, true).await.unwrap();
            let f = f.0.into_inner().unwrap();
            assert!(!f.seen.iter().any(|s| s.ends_with("/clear")), "{:?}", f.seen);
            assert_eq!(f.keys, ["\n"]);
        }
    }

    #[tokio::test]
    async fn dropped_last_characters_are_typed_again() {
        let f = Fake::new("TextField", "old");
        f.0.lock().unwrap().drop_next = 2;
        let r = replace(&f, "故宫博物院", false).await.unwrap();
        assert!(r.contains("2 个字没输进去"), "{r}");
        assert_eq!(f.0.into_inner().unwrap().value, "故宫博物院");
    }

    /// A field that doesn't read back as the text: Return isn't pressed, and the old text is said to be gone
    #[tokio::test]
    async fn a_mismatch_is_refused_without_return() {
        let f = Fake::new("TextField", "wifi");
        f.0.lock().unwrap().mangle = |s| s.to_uppercase();
        let e = replace(&f, "bluetooth", true).await.unwrap_err();
        assert!(e.contains("原来的文字已经删掉了") && e.contains("BLUETOOTH") && e.contains("没有按回车"), "{e}");
        assert!(!f.0.into_inner().unwrap().keys.contains(&"\n".to_owned()));
    }

    #[tokio::test]
    async fn password_fields_and_missing_keyboards_are_refused_untouched() {
        let f = Fake::new("SecureTextField", "secret");
        assert!(replace(&f, "x", true).await.unwrap_err().contains("密码框"));
        let f = f.0.into_inner().unwrap();
        assert_eq!((f.value.as_str(), f.keys.len()), ("secret", 0));

        let f = Fake::new("TextField", "wifi");
        f.0.lock().unwrap().keyboard = false;
        assert!(replace(&f, "x", false).await.unwrap_err().starts_with("屏幕上没有键盘"));
        assert!(!f.0.into_inner().unwrap().seen.iter().any(|s| s.contains("/element/active")));
    }

    /// WDA unreachable before anything was done: another way may still replace the text
    #[tokio::test]
    async fn an_unreachable_wda_leaves_the_field_untouched() {
        let f = Fake::new("TextField", "wifi");
        f.0.lock().unwrap().gone = true;
        assert!(replace(&f, "x", false).await.unwrap_err().starts_with("untouched"));
    }
}
