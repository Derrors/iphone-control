//! AX screen reading, rendering, self-healing, settle/scroll/wait observation, and status.

use super::super::*;

/// A read of the screen waits at most this long for a tree (the first one of a session comes in about a second)
pub(crate) const AX_WAIT: Duration = Duration::from_secs(5);
/// 无障碍服务不应答时，同一台设备这么久内最多重启一次 axauditd：重启了还不行就照常报错，不反复结束它
pub(crate) const AX_RESTART_EVERY: Duration = Duration::from_secs(60);
/// After an action, the screen is first read this long after it started: an app reacts to a tap within about
/// 0.1 s, a page slides in within about 0.35 s (the second read, which confirms, comes about 0.2 s later)
const REACT: Duration = Duration::from_millis(300);

/// How often a wait for text to disappear asks lockdown whether the phone is locked (a new connection each time)
const LOCK_ASK_EVERY: Duration = Duration::from_secs(1);

impl Daemon {
    /// One read of the screen through remoteAXService, as an element tree like WDA's `/source`: the frontmost app's
    /// tree as it is now and SpringBoard's elements over the app. Returns the tree, the screen (as turned now) and the
    /// frontmost app's process
    pub(crate) async fn ax_read(
        &self,
        g: &mut Inner,
        wait: Duration,
    ) -> crate::error::Result<(elements::Source, ScreenSize, i64)> {
        self.ax_read_with(g, wait, true).await
    }

    /// [`Self::ax_read`], SpringBoard's elements over the app left out unless `overlays` (they don't tell whether a
    /// list has stopped, and reading them takes as long again)
    async fn ax_read_with(
        &self,
        g: &mut Inner,
        wait: Duration,
        overlays: bool,
    ) -> crate::error::Result<(elements::Source, ScreenSize, i64)> {
        let s = self.ax_snapshot(g, wait, overlays).await?;
        self.ax_render(g, &s).await
    }

    /// One read of the screen as the phone gives it ([`Self::ax_read_with`] before it becomes a tree)
    pub(super) async fn ax_snapshot(
        &self,
        g: &mut Inner,
        wait: Duration,
        overlays: bool,
    ) -> crate::error::Result<ictl_device::ax::AxScreen> {
        self.ax_snapshot_with(g, wait, overlays, true).await
    }

    /// [`Self::ax_snapshot`]; `restart_stuck`: an unanswered read may restart axauditd ([`Self::restart_stuck_ax`]).
    /// Not while pasting: an app waiting for the paste prompt's answer doesn't answer either, and a restarted
    /// axauditd gives the elements new references, so the field typed into isn't found again
    pub(super) async fn ax_snapshot_with(
        &self,
        g: &mut Inner,
        wait: Duration,
        overlays: bool,
        restart_stuck: bool,
    ) -> crate::error::Result<ictl_device::ax::AxScreen> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let (mut restarted, mut rebuilt) = (false, false);
        loop {
            let t = self
                .ensure_tunnel(g, &device)
                .await
                .map_err(|e| ictl_l10n::trf!("读界面失败（建隧道）：{e}", "Reading the screen failed (tunnel): {e}"))?;
            let r = t.ax_screen(wait, overlays).await;
            if r.is_err() {
                t.close_ax().await;
            }
            g.ax_used = Some(Instant::now());
            if tunnel_timed_out(&r) {
                drop_tunnel(g).await;
                if !std::mem::replace(&mut rebuilt, true) {
                    continue;
                }
            }
            if let Err(ictl_device::DeviceError::AxUnanswered(e)) = &r
                && restart_stuck
                && !restarted
                && Box::pin(self.restart_stuck_ax(g, &device, &e.to_string())).await
            {
                restarted = true;
                continue;
            }
            return Ok(r.map_err(|e| ictl_l10n::trf!("读界面失败：{e}", "Reading the screen failed: {e}"))?);
        }
    }

    /// remoteAXService didn't answer: unless that was already tried within [`AX_RESTART_EVERY`], end axauditd on the
    /// phone ([`ictl_device::Tunnel::restart_ax_daemon`]). Whether it was ended, so the read is worth repeating
    pub(crate) async fn restart_stuck_ax(&self, g: &mut Inner, device: &ictl_device::Device, why: &str) -> bool {
        // a background check (the page's alert and status polls) doesn't restart it: it may be dropped halfway for an
        // action, and the next action finds out for itself
        if crate::core::in_background() {
            return false;
        }
        if g.dev.ax_restarted.is_some_and(|t| t.elapsed() < AX_RESTART_EVERY) {
            return false;
        }
        g.dev.ax_restarted = Some(Instant::now());
        let Ok(t) = self.ensure_tunnel(g, device).await else { return false };
        match t.restart_ax_daemon().await {
            Ok(n) if n > 0 => {
                self.log.log(&ictl_l10n::log_trf!(
                    "无障碍服务没有应答（{why}），已重启手机上的 axauditd",
                    "The accessibility service didn't answer ({why}), restarted axauditd on the phone"
                ));
                true
            }
            Ok(_) => false,
            Err(e) => {
                tracing::info!("{}", ictl_l10n::log_trf!("重启 axauditd 失败：{e}", "Restarting axauditd failed: {e}"));
                false
            }
        }
    }

    /// A read as an element tree like WDA's, the screen (as turned now) and the frontmost app's process
    async fn ax_render(
        &self,
        g: &mut Inner,
        s: &ictl_device::ax::AxScreen,
    ) -> crate::error::Result<(elements::Source, ScreenSize, i64)> {
        // the application element covers the screen: its frame is the screen in points, as turned now
        let root = s.app.first().and_then(|e| e.frame).map(|f| ScreenSize { width: f.2, height: f.3 });
        let size = match root.filter(|r| r.width > 1.0 && r.height > 1.0) {
            Some(r) => {
                if let Some(udid) = g.device.as_ref().map(|d| d.udid.clone())
                    && g.persisted.record_screen(&udid, r.portrait())
                {
                    g.persisted.save(&self.cfg.state_file());
                }
                r
            }
            None => self.screen(g).await?,
        };
        tracing::debug!(
            "native read: app pid {} with {} elements, {} over it, app frame {:?}, screen {}x{}",
            s.app_pid,
            s.app.len(),
            s.overlays.len(),
            s.app.first().and_then(|e| e.frame),
            size.width,
            size.height
        );
        let web = self.web_processes(g, &s.app).await;
        Ok((
            ictl_ax::node::to_node(&s.app, &s.overlays, size.width, size.height, &web, s.springboard_front),
            size,
            s.app_pid,
        ))
    }

    /// Which of the processes in a read are WebKit's web content (a page's elements, listed without a host): from
    /// the process list, read again only for a process not seen yet
    async fn web_processes(&self, g: &mut Inner, app: &[ictl_ax::Element]) -> std::collections::HashSet<i64> {
        let app_pid = app.first().map_or(0, ictl_ax::Element::pid);
        let mut pids: Vec<i64> = app.iter().map(ictl_ax::Element::pid).filter(|p| *p != app_pid).collect();
        pids.sort_unstable();
        pids.dedup();
        if pids.iter().any(|p| g.dev.web_pids.get(*p).is_none())
            && let Some(device) = g.device.clone()
            && let Ok(t) = self.ensure_tunnel(g, &device).await
            && let Ok(procs) = t.processes().await
        {
            for p in &pids {
                let exe = procs.iter().find(|(q, _)| *q as i64 == *p).map(|(_, e)| e.as_str()).unwrap_or("");
                let name = exe.rsplit('/').next().unwrap_or("");
                g.dev.web_pids.insert(*p, name.starts_with("com.apple.WebKit.WebContent"));
            }
        }
        pids.into_iter().filter(|p| g.dev.web_pids.get(*p) == Some(&true)).collect()
    }

    /// The screen's elements through remoteAXService (see [`Self::ax_read`])
    pub(crate) async fn ax_tree(&self, g: &mut Inner, wait: Duration) -> crate::error::Result<Tree> {
        let screen = self.ax_snapshot(g, wait, true).await?;
        self.snapshot_tree(g, &screen).await
    }

    /// The frontmost app's bundle ID (one request, no walk)
    pub(crate) async fn native_foreground(&self, g: &mut Inner) -> crate::error::Result<String> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let (mut restarted, mut rebuilt) = (false, false);
        let r = loop {
            let t = self.ensure_tunnel(g, &device).await?;
            let r = t.ax_frontmost_pid(AX_WAIT).await;
            if r.is_err() {
                t.close_ax().await;
            }
            g.ax_used = Some(Instant::now());
            if tunnel_timed_out(&r) {
                drop_tunnel(g).await;
                if !std::mem::replace(&mut rebuilt, true) {
                    continue;
                }
            }
            if let Err(ictl_device::DeviceError::AxUnanswered(e)) = &r
                && !restarted
                && Box::pin(self.restart_stuck_ax(g, &device, &e.to_string())).await
            {
                restarted = true;
                continue;
            }
            break r;
        };
        let pid = r.map_err(|e| ictl_l10n::trf!("读前台 App 失败：{e}", "Reading the foreground app failed: {e}"))?;
        self.bundle_of_pid(g, pid).await
    }

    pub(super) async fn bundle_of_pid(&self, g: &mut Inner, pid: i64) -> crate::error::Result<String> {
        if let Some(b) = g.dev.bundles.get(pid) {
            return Ok(b.clone());
        }
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let procs = self.ensure_tunnel(g, &device).await?.processes().await?;
        let exe = procs
            .into_iter()
            .find(|(p, _)| *p as i64 == pid)
            .map(|(_, e)| e)
            .ok_or_else(|| ictl_l10n::trf!("前台进程 {pid} 已经不在了", "The foreground process {pid} is gone"))?;
        for attempt in 0..2 {
            // the user's apps first: a system app never shares their paths, but this keeps the order of resolve
            let mut apps = self.list_apps(g, false).await?;
            apps.extend(self.list_apps(g, true).await?);
            if let Some(b) = apps::bundle_of_process(&exe, &apps) {
                g.dev.bundles.insert(pid, b.clone());
                return Ok(b);
            }
            // installed since the list was read
            if attempt == 0 {
                g.dev.apps.clear();
            }
        }
        Err(ictl_l10n::trf!("看不出进程 {pid}（{exe}）是哪个 App", "Can't tell which app process {pid} ({exe}) is")
            .into())
    }

    /// [`Self::status`] for a phone driven natively
    pub(crate) async fn native_status(
        &self,
        g: &mut Inner,
        unlock: &ictl_unlock::UnlockConfig,
    ) -> (String, StatusData) {
        let mut data = StatusData { native: true, ..Default::default() };
        let mut lines = vec![ictl_l10n::tr!(
            "控制方式: 原生(iOS 27 起不用 WDA:读界面经无障碍服务,触摸和按键经 HID)",
            "Control: native (no WDA from iOS 27: the screen through the accessibility service, touches and keys through HID)"
        )
        .to_owned()];
        match self.native_locked(g).await {
            Ok(locked) => {
                data.locked = Some(locked);
                lines.push(ictl_l10n::trf!("锁屏: {}", "Locked: {}", lock_text(locked, false, unlock)));
            }
            Err(e) => lines.push(ictl_l10n::trf!("锁屏: 查不到({e})", "Locked: unknown ({e})")),
        }
        match self.native_foreground(g).await {
            Ok(app) => {
                lines.push(ictl_l10n::trf!("前台 App: {app}", "Foreground app: {app}"));
                data.foreground = Some(app);
            }
            Err(e) => lines.push(ictl_l10n::trf!("前台 App: 查不到({e})", "Foreground app: unknown ({e})")),
        }
        if let Ok(s) = self.screen(g).await {
            lines.push(ictl_l10n::trf!("屏幕: {}x{} 点", "Screen: {}x{} points", fmt_g(s.width), fmt_g(s.height)));
            (data.width, data.height) = (Some(s.width), Some(s.height));
        }
        (lines.join("\n"), data)
    }

    /// The interface's rotation, from SpringBoard (a lockdown service: no tunnel, no screenshot)
    pub(crate) async fn native_rotation(&self, g: &Inner) -> Option<Rotation> {
        Box::pin(g.device.as_ref()?.interface_rotation()).await.ok().flatten()
    }

    /// Waits for the screen to settle after an action, as through WDA: the tree read again until two reads in a
    /// row are alike and nothing is loading, at most `max`. The first read waits a moment after the action (the
    /// app's reaction, a page's slide in), or two reads of the old screen would count as settled. The reads meanwhile
    /// are of the app alone; what SpringBoard shows over it is read once, for the tree returned
    /// `seed`: the read that just saw a swiped list stop, taken as the first one (it still has to be read the same
    /// again to count as settled)
    pub(crate) async fn native_settle(
        &self,
        g: &mut Inner,
        max: Duration,
        seed: Option<ictl_device::ax::AxScreen>,
    ) -> crate::error::Result<(String, Tree)> {
        let t0 = Instant::now();
        if let Some(left) = g.dev.acted_at.and_then(|t| REACT.checked_sub(t.elapsed())) {
            tokio::time::sleep(left).await;
        }
        let mut snap = match seed {
            Some(s) => s,
            None => self.ax_snapshot(g, AX_WAIT, false).await?,
        };
        let mut prev = self.snapshot_tree(g, &snap).await?;
        if prev.elements.len() < VISUAL_SETTLE_BELOW {
            // between two pages (a slide in or back) the tree can be nearly empty for a moment: once more. Few
            // elements twice, and the same: a screen whose changes the tree doesn't show (a game, a map), the caller
            // looks at the picture. Otherwise the tree goes on telling (Settings → General has 12 or 13 elements,
            // fewer while it slides in)
            tokio::time::sleep(Duration::from_millis(300)).await;
            let again = self.ax_snapshot(g, AX_WAIT, false).await?;
            let cur = self.snapshot_tree(g, &again).await?;
            let sparse = cur.elements.len() < VISUAL_SETTLE_BELOW && cur.close_to(&prev, STILL_TOLERANCE);
            (snap, prev) = (again, cur);
            if sparse {
                return Ok((String::new(), self.with_overlays(g, snap).await?));
            }
        }
        loop {
            if t0.elapsed() >= max {
                let msg = if prev.loading {
                    ictl_l10n::trf!(
                        "{:.1} 秒内页面一直在加载(有加载圈),按最后的状态返回;要等结果出来用 iphone_wait_for",
                        "The page kept loading for {:.1} s (spinner shown), returning the last state; to wait for the result use iphone_wait_for",
                        max.as_secs_f64()
                    )
                } else {
                    ictl_l10n::trf!(
                        "{:.1} 秒内界面一直在变(可能有动画),按最后的状态返回",
                        "The screen kept changing for {:.1} s (perhaps an animation), returning the last state",
                        max.as_secs_f64()
                    )
                };
                return Ok((msg, self.with_overlays(g, snap).await?));
            }
            snap = self.ax_snapshot(g, AX_WAIT, false).await?;
            let cur = self.snapshot_tree(g, &snap).await?;
            let same = cur.close_to(&prev, STILL_TOLERANCE);
            prev = cur;
            if same && !prev.loading {
                let tree = self.with_overlays(g, snap).await?;
                let msg =
                    ictl_l10n::trf!("界面已稳定({:.1} 秒)", "Screen settled ({:.1} s)", t0.elapsed().as_secs_f64());
                return Ok((msg, tree));
            }
        }
    }

    pub(super) async fn snapshot_tree(
        &self,
        g: &mut Inner,
        s: &ictl_device::ax::AxScreen,
    ) -> crate::error::Result<Tree> {
        let (src, size, _) = self.ax_render(g, s).await?;
        let mut tree = Tree::of(&src, size);
        apply_ax_completeness(&mut tree, s);
        Ok(tree)
    }

    /// `s`, a read of the app alone, with what SpringBoard shows over it, as a tree
    pub(in crate::tools) async fn with_overlays(
        &self,
        g: &mut Inner,
        mut s: ictl_device::ax::AxScreen,
    ) -> crate::error::Result<Tree> {
        if let Some(t) = g.tunnel.as_mut() {
            let (overlays, overlays_cutoff) = t.ax_overlays_report(s.app_pid).await;
            s.overlays = overlays;
            s.overlays_complete = overlays_cutoff.is_none();
            s.overlays_cutoff = overlays_cutoff;
        }
        self.snapshot_tree(g, &s).await
    }

    /// The frontmost app's elements alone, as a read and as a tree
    async fn ax_app_read(&self, g: &mut Inner) -> crate::error::Result<(ictl_device::ax::AxScreen, Tree)> {
        let s = self.ax_snapshot(g, AX_WAIT, false).await?;
        let (src, size, _) = self.ax_render(g, &s).await?;
        Ok((s, Tree::of(&src, size)))
    }

    /// After a swipe, waits for the list to stop (at most [`SCROLL_SETTLE`]): two reads in a row with the elements
    /// in the same place along the swipe. Says when it was still moving; a screen with few elements isn't waited for
    /// Also returns the read that saw it stop, confirmed by two reads, or the one read of a screen with few elements
    /// (not confirmed), for the next look at the screen ([`crate::core::Inner::swiped_snap`]); none when it was still
    /// moving
    pub(crate) async fn native_scrolled(
        &self,
        g: &mut Inner,
        vertical: bool,
    ) -> (String, Option<(ictl_device::ax::AxScreen, bool)>) {
        let t0 = Instant::now();

        let Ok((snap, mut prev)) = self.ax_app_read(g).await else { return (String::new(), None) };
        if prev.elements.len() < VISUAL_SETTLE_BELOW {
            return (String::new(), Some((snap, false)));
        }
        while t0.elapsed() < SCROLL_SETTLE {
            let Ok((snap, cur)) = self.ax_app_read(g).await else { return (String::new(), None) };
            let still = cur.still_along_common(&prev, vertical, STILL_TOLERANCE);
            tracing::debug!(
                "scroll wait at {:.2} s: {} then {} elements, still: {still}",
                t0.elapsed().as_secs_f64(),
                prev.elements.len(),
                cur.elements.len()
            );
            if still {
                return (String::new(), Some((snap, true)));
            }
            prev = cur;
        }
        let note = ictl_l10n::trf!(
            "({:.1} 秒后列表还在动,可能还在滚动)",
            " (the list was still moving after {:.1} s, it may still be scrolling)",
            SCROLL_SETTLE.as_secs_f64()
        );
        (note, None)
    }

    /// Text (or selector match) appearing on screen, natively: the tree read again every 0.3 s
    pub(crate) async fn native_wait_for(
        &self,
        g: &mut Inner,
        text: &str,
        selector: &crate::protocol::ElementSelector,
        timeout: Duration,
        disappeared: bool,
        may_continue: &mut dyn FnMut() -> bool,
    ) -> crate::error::Result<String> {
        use super::observe::Waited;
        let t0 = Instant::now();
        let sel = ui_selector(text, selector);
        let desc = describe_selector(text, selector);
        // lockdown's last answer, and when (None: it failed)
        let mut lock: Option<(Instant, Option<bool>)> = None;
        loop {
            let screen = self.ax_snapshot(g, AX_WAIT, true).await?;
            let (src, size, _) = self.ax_render(g, &screen).await?;
            let mut tree = Tree::of(&src, size);
            apply_ax_completeness(&mut tree, &screen);
            let why = match super::observe::wait_match_matching(&tree, sel, &desc, disappeared, t0.elapsed()) {
                Some(result) if !disappeared => return Ok(result),
                Some(result) => {
                    // the read first (costs nothing more), then lockdown: it says locked before the read shows the
                    // lock screen, but only on a phone with a passcode, so it can't be the only check. Asked at most
                    // every LOCK_ASK_EVERY (a new connection each time); a failure leaves it unknown, not the wait
                    let cover = native_cover(&src, &screen, &tree);
                    if cover == Cover::Locked {
                        return Err(super::observe::wait_locked());
                    }
                    let locked = match lock {
                        Some((at, answer)) if at.elapsed() < LOCK_ASK_EVERY => answer,
                        _ => {
                            let answer = self.native_locked(g).await.inspect_err(|e| {
                                tracing::debug!("lock state while waiting for {desc:?} to disappear: {e}")
                            });
                            let answer = answer.ok();
                            lock = Some((Instant::now(), answer));
                            answer
                        }
                    };
                    match (cover, locked) {
                        (_, Some(true)) => return Err(super::observe::wait_locked()),
                        (Cover::None, Some(false)) => return Ok(result),
                        _ => Waited::Covered,
                    }
                }
                None => Waited::after_matching(&tree, sel, disappeared),
            };
            if t0.elapsed() >= timeout {
                return Err(super::observe::wait_timeout(&desc, timeout, why));
            }
            if !may_continue() {
                return Err(super::observe::wait_interrupted(&desc, t0.elapsed()));
            }
            tokio::time::sleep(Duration::from_millis(300).min(timeout.saturating_sub(t0.elapsed()))).await;
        }
    }

    /// A system alert over the screen, natively: SpringBoard's elements are part of every read
    pub(crate) async fn native_alert(&self, g: &mut Inner) -> crate::error::Result<(String, Option<String>)> {
        let tree = self.ax_tree(g, AX_WAIT).await?;
        Ok(match tree.alert().map(|a| a.label.clone()).filter(|l| !l.is_empty()) {
            Some(text) => (alert_msg(&text), Some(text)),
            None => (String::new(), None),
        })
    }
}

pub(super) fn apply_ax_completeness(tree: &mut Tree, s: &ictl_device::ax::AxScreen) {
    tree.app_pid = Some(s.app_pid);
    tree.incomplete |= !s.app_complete || !s.overlays_complete;
    if tree.incomplete_reason.is_none() {
        tree.incomplete_reason = s
            .app_cutoff
            .map(cutoff_reason)
            .or_else(|| (!s.app_complete).then_some(elements::IncompleteReason::Unreadable))
            .or_else(|| (!s.overlays_complete).then_some(elements::IncompleteReason::OverlaysIncomplete));
    }
}

fn cutoff_reason(c: ictl_ax::WalkCutoff) -> elements::IncompleteReason {
    match c {
        ictl_ax::WalkCutoff::ElementLimit => elements::IncompleteReason::ElementLimit,
        ictl_ax::WalkCutoff::Timeout => elements::IncompleteReason::Timeout,
        ictl_ax::WalkCutoff::DepthLimit => elements::IncompleteReason::DepthLimit,
        ictl_ax::WalkCutoff::Unreadable => elements::IncompleteReason::Unreadable,
    }
}

/// What is over the screen in a native read, for a wait for text to disappear
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cover {
    /// An app, or SpringBoard's home screen or Spotlight: what it shows is the screen
    None,
    /// The lock screen, known by its marks ([`elements::cover_sheet`])
    Locked,
    /// Notification center, control center, or the lock screen without the marks [`elements::cover_sheet`] knows it
    /// by, as it often reads natively: SpringBoard in front without app icons (6-7 elements: date, time,
    /// notifications, flashlight, camera; iPhone19,7, iOS 27.2.1), or another process in front with the lock
    /// screen's buttons ([`cover_mark`]). Lockdown says locked only on a phone with a passcode
    Other,
}

pub(super) fn native_cover(src: &elements::Source, screen: &ictl_device::ax::AxScreen, tree: &Tree) -> Cover {
    match elements::cover_sheet(src) {
        elements::CoverSheet::Locked => Cover::Locked,
        elements::CoverSheet::NotificationCenter => Cover::Other,
        elements::CoverSheet::None if screen.springboard_front && !unlocked_springboard(tree) => Cover::Other,
        elements::CoverSheet::None if tree.elements.iter().any(cover_mark) => Cover::Other,
        elements::CoverSheet::None => Cover::None,
    }
}

/// An element only the lock screen and notification center show, whichever process is in front: the flashlight
/// and camera buttons at the bottom, and the sleep lock screen's own elements. During a sleep focus the lock screen
/// is another process (`SleepLockScreen`), not SpringBoard, with SpringBoard's elements over it (iPhone19,7, iOS
/// 27.2.1, 2026-11-19 00:04)
fn cover_mark(e: &elements::Element) -> bool {
    e.name == "flashlight-orb-button"
        || e.name.starts_with("system-orb-button-")
        || e.name.starts_with("UIA.SleepLockScreen")
}

/// SpringBoard in front, recognisably unlocked: the home screen (app icons) or Spotlight
fn unlocked_springboard(tree: &Tree) -> bool {
    tree.elements.iter().any(|e| e.kind == "Icon" || e.name == "SpotlightSearchField")
}

/// A read that timed out in the tunnel itself (connecting to the service's port, a query through it): the tunnel
/// is stuck, and keeping it would time out every read after it. It is dropped (and the read tried once more on a new
/// one). Not [`ictl_device::DeviceError::AxUnanswered`]: that is the accessibility daemon, restarted instead
pub(super) fn tunnel_timed_out<T>(r: &ictl_device::Result<T>) -> bool {
    matches!(r, Err(ictl_device::DeviceError::Timeout(_)))
}
