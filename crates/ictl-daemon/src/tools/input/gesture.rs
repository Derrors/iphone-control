//! Coordinate preparation, taps, touches, long presses, and swipes.

use super::super::*;
use crate::protocol::MAX_TAP_COUNT;

/// 一次滑动（[`Daemon::swipe`]）：从 `from` 用 `seconds` 秒移到 `to`；`hold` 秒大于 0 时先在起点按住再移动
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Stroke {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub seconds: f64,
    pub hold: f64,
}

impl Daemon {
    /// A stale pointer must neither unlock nor act on a new display. Unlock can itself
    /// rotate the foreground app, so take the final measurement after it completes.
    pub(in crate::tools) async fn prepare_coordinates(
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

    pub(in crate::tools) async fn send_touch(
        &self,
        g: &mut Inner,
        phase: TouchPhase,
        x: f64,
        y: f64,
    ) -> Result<(), Response> {
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
                    g.swiped_tree = tree.map(|(t, still)| crate::core::Swiped::now(t, still));
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
            g.swiped_snap = snap.map(|(s, still)| crate::core::Swiped::now(s, still));
            note
        };
        Ok(ictl_l10n::trf!("已滑动 {desc}{note}", "Swiped {desc}{note}"))
    }
}

/// 滑动后等列表停下（最多 [`SCROLL_SETTLE`]）：沿滑动方向的位置连续两次差不超过 [`STILL_TOLERANCE`]。
/// 没停下返回提醒；元素很少（游戏、地图）看不出来，不等。
/// 同时返回停下时读到的树，紧接着的 settle（then=…）从它开始比较，少读一次（[`Daemon::settle`]）。
/// 调用前已在轮询模式（[`WdaClient::swipe_into_polling`]）
/// Also returns the last read, with whether two reads agreed it stood still (a screen with few elements isn't waited
/// for: its one read comes back unconfirmed)
async fn wait_scrolled(w: &mut WdaClient<DeviceTransport>, vertical: bool) -> (String, Option<(Tree, bool)>) {
    let t0 = Instant::now();
    // (stopped, confirmed by two reads, the last read)
    let r: Result<(bool, bool, Tree), WdaError> = async {
        let mut prev = w.tree().await?;
        if prev.elements.len() < VISUAL_SETTLE_BELOW {
            return Ok((true, false, prev));
        }
        while t0.elapsed() < SCROLL_SETTLE {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let cur = w.tree().await?;
            if cur.still_along(&prev, vertical, STILL_TOLERANCE) {
                return Ok((true, true, cur));
            }
            prev = cur;
        }
        Ok((false, false, prev))
    }
    .await;
    w.end_polling().await;
    match r {
        Ok((true, still, tree)) => (String::new(), Some((tree, still))),
        Ok((false, _, _)) => (
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
