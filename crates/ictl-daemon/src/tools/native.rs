//! Native control of iOS 27 and later, without WebDriverAgent (`ICTL_IOS_BACKEND`, docs/dev/ios27-native.md):
//! the screen through remoteAXService (`ictl_ax`, turned into WDA's XML so `ictl_wda::elements` reads it), touches,
//! keys and Home through HID, text through the pasteboard, apps and links through CoreDevice AppService, the lock
//! state through lockdown.
//!
//! The tools keep their WDA code; where WDA isn't there they come here when [`Daemon::native`] says so.

use ictl_device::DeviceInfo;
use ictl_unlock::Screen;
use ictl_wda::client::LockCheck;
use ictl_wda::elements::View;

use super::*;
use crate::config::IosBackend;

/// A read of the screen waits at most this long for a tree (the first one of a session comes in about a second)
pub(crate) const AX_WAIT: Duration = Duration::from_secs(5);
/// 无障碍服务不应答时，同一台设备这么久内最多重启一次 axauditd：重启了还不行就照常报错，不反复结束它
pub(crate) const AX_RESTART_EVERY: Duration = Duration::from_secs(60);
/// After an action, the screen is first read this long after it started: an app reacts to a tap within about
/// 0.1 s, a page slides in within about 0.35 s (the second read, which confirms, comes about 0.2 s later)
const REACT: Duration = Duration::from_millis(300);
/// After Cmd-V: how long for the text to show in the field, answering the paste prompt meanwhile
const PASTE_WAIT: Duration = Duration::from_secs(5);
/// After the paste prompt was answered, a field that already showed the text and doesn't change for this long took
/// the paste as it was: the pasted text replaced the same text, selected (Spotlight keeps the last search selected).
/// A paste shows in the field within about half a second of the answer
const PASTE_SAME_SETTLE: Duration = Duration::from_millis(1500);
/// How long an app may take to come to the front
const LAUNCH_WAIT: Duration = Duration::from_secs(8);
/// How long a killed app's processes may take to be gone
const TERMINATE_WAIT: Duration = Duration::from_secs(2);
/// Home is answered no sooner than this after it was pressed: what it closes on the home screen animates meanwhile
const HOME_ANIMATION: Duration = Duration::from_millis(600);
/// After Home, how long the screen takes to wake before the swipe up (WDA's 1 s covers its slow queries)
const NATIVE_WAKE_SETTLE: Duration = Duration::from_millis(500);
/// After unlocking, the lock screen slides away for about half a second
const UNLOCK_SETTLE: Duration = Duration::from_millis(700);
/// The app's zoom-in takes about half a second; a tap during it is lost (the same as WDA's `LAUNCH_MIN`)
const LAUNCH_SETTLE: Duration = Duration::from_millis(800);

/// See [`IosBackend::drives_natively`]
fn native_for(backend: IosBackend, info: Option<&DeviceInfo>) -> bool {
    backend.drives_natively(info)
}

enum PasteTarget<'a> {
    Append(Option<&'a [String]>),
    Replace(ictl_ax::ObjectRef),
}

/// What a paste did, as far as the fields show
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pasted {
    /// A field changed and now holds the text
    Typed,
    /// A field held the text before and still does, unchanged: the paste replaced the same text, or didn't go in
    Unchanged,
    /// A password field is on screen: its value can't be read
    Unreadable,
    /// No field holds the text
    Missing,
}

/// One read of the fields after a paste, against `old`, theirs before it (None: a password field)
fn paste_seen(now: Option<&[String]>, old: Option<&[String]>, wanted: &str) -> Pasted {
    let (Some(now), Some(old)) = (now, old) else { return Pasted::Unreadable };
    if !now.iter().any(|v| v.contains(wanted)) {
        Pasted::Missing
    } else if now != old {
        Pasted::Typed
    } else {
        Pasted::Unchanged
    }
}

/// The on-screen keyboard is up: a Keyboard element, or its keys (the keyboard's process reads as the app's own
/// elements at times, then no Keyboard is made of them, `ictl_ax::xml`)
fn keyboard_up(tree: &Tree) -> bool {
    tree.elements.iter().any(|e| e.kind == "Keyboard") || tree.elements.iter().filter(|e| e.kind == "Key").count() >= 8
}

/// The paste prompt's "Allow Paste" button ("“Notes” would like to paste from “dtpasteboardd”"), when it is up
fn paste_allow(tree: &Tree) -> Option<(f64, f64)> {
    tree.alert().filter(|a| a.label.contains("dtpasteboardd"))?;
    tree.elements
        .iter()
        .find(|e| e.kind == "Button" && matches!(e.label.as_str(), "允许粘贴" | "Allow Paste" | "允許貼上"))
        .map(|e| (e.center.0 as f64, e.center.1 as f64))
}

impl Daemon {
    /// The current phone is driven natively (no WDA): see [`native_for`]
    pub(crate) fn native(&self, g: &Inner) -> bool {
        native_for(self.cfg.ios_backend, g.info.as_ref())
    }

    /// One read of the screen through remoteAXService, as WDA-style XML: the frontmost app's tree as it is now and
    /// SpringBoard's elements over the app. Returns the XML, the screen (as turned now) and the frontmost app's
    /// process
    pub(crate) async fn ax_read(
        &self,
        g: &mut Inner,
        wait: Duration,
    ) -> crate::error::Result<(String, ScreenSize, i64)> {
        self.ax_read_with(g, wait, true).await
    }

    /// [`Self::ax_read`], SpringBoard's elements over the app left out unless `overlays` (they don't tell whether a
    /// list has stopped, and reading them takes as long again)
    async fn ax_read_with(
        &self,
        g: &mut Inner,
        wait: Duration,
        overlays: bool,
    ) -> crate::error::Result<(String, ScreenSize, i64)> {
        let s = self.ax_snapshot(g, wait, overlays).await?;
        self.ax_render(g, &s).await
    }

    /// One read of the screen as the phone gives it ([`Self::ax_read_with`] before it becomes XML)
    async fn ax_snapshot(
        &self,
        g: &mut Inner,
        wait: Duration,
        overlays: bool,
    ) -> crate::error::Result<ictl_device::ax::AxScreen> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let mut restarted = false;
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
            if let Err(ictl_device::DeviceError::AxUnanswered(e)) = &r
                && !restarted
                && Box::pin(self.restart_stuck_ax(g, &device, e)).await
            {
                restarted = true;
                continue;
            }
            return Ok(r.map_err(|e| ictl_l10n::trf!("读界面失败：{e}", "Reading the screen failed: {e}"))?);
        }
    }

    /// remoteAXService didn't answer: unless that was already tried within [`AX_RESTART_EVERY`], end axauditd on the
    /// phone ([`ictl_device::Tunnel::restart_ax_daemon`]). Whether it was ended, so the read is worth repeating
    async fn restart_stuck_ax(&self, g: &mut Inner, device: &ictl_device::Device, why: &str) -> bool {
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

    /// A read as WDA-style XML, the screen (as turned now) and the frontmost app's process
    async fn ax_render(
        &self,
        g: &mut Inner,
        s: &ictl_device::ax::AxScreen,
    ) -> crate::error::Result<(String, ScreenSize, i64)> {
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
            ictl_ax::xml::to_xml(&s.app, &s.overlays, size.width, size.height, &web, s.springboard_front),
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
        if pids.iter().any(|p| !g.dev.web_pids.contains_key(p))
            && let Some(device) = g.device.clone()
            && let Ok(t) = self.ensure_tunnel(g, &device).await
            && let Ok(procs) = t.processes().await
        {
            if g.dev.web_pids.len() > 200 {
                g.dev.web_pids.clear();
            }
            for p in &pids {
                let exe = procs.iter().find(|(q, _)| *q as i64 == *p).map(|(_, e)| e.as_str()).unwrap_or("");
                let name = exe.rsplit('/').next().unwrap_or("");
                g.dev.web_pids.insert(*p, name.starts_with("com.apple.WebKit.WebContent"));
            }
        }
        pids.into_iter().filter(|p| g.dev.web_pids.get(p) == Some(&true)).collect()
    }

    /// The screen's elements through remoteAXService (see [`Self::ax_read`])
    pub(crate) async fn ax_tree(&self, g: &mut Inner, wait: Duration) -> crate::error::Result<Tree> {
        let screen = self.ax_snapshot(g, wait, true).await?;
        self.snapshot_tree(g, &screen).await
    }

    /// The frontmost app's bundle ID (one request, no walk)
    pub(crate) async fn native_foreground(&self, g: &mut Inner) -> crate::error::Result<String> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let mut restarted = false;
        let r = loop {
            let t = self.ensure_tunnel(g, &device).await?;
            let r = t.ax_frontmost_pid(AX_WAIT).await;
            if r.is_err() {
                t.close_ax().await;
            }
            g.ax_used = Some(Instant::now());
            if let Err(ictl_device::DeviceError::AxUnanswered(e)) = &r
                && !restarted
                && Box::pin(self.restart_stuck_ax(g, &device, e)).await
            {
                restarted = true;
                continue;
            }
            break r;
        };
        let pid = r.map_err(|e| ictl_l10n::trf!("读前台 App 失败：{e}", "Reading the foreground app failed: {e}"))?;
        self.bundle_of_pid(g, pid).await
    }

    async fn bundle_of_pid(&self, g: &mut Inner, pid: i64) -> crate::error::Result<String> {
        if let Some(b) = g.dev.bundles.get(&pid) {
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
                if g.dev.bundles.len() > 200 {
                    g.dev.bundles.clear();
                }
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

    /// Whether the phone is locked (lockdown's `PasswordProtected`)
    pub(crate) async fn native_locked(&self, g: &mut Inner) -> crate::error::Result<bool> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        Ok(device.passcode_locked().await?)
    }

    /// The interface's rotation, from SpringBoard (a lockdown service: no tunnel, no screenshot)
    pub(crate) async fn native_rotation(&self, g: &Inner) -> Option<Rotation> {
        Box::pin(g.device.as_ref()?.interface_rotation()).await.ok().flatten()
    }

    /// Waits for the screen to settle after an action, as through WDA: the tree read again until two reads in a
    /// row are alike and nothing is loading, at most `max`. The first read waits a moment after the action (the
    /// app's reaction, a page's slide in), or two reads of the old screen would count as settled. The reads meanwhile
    /// are of the app alone; what SpringBoard shows over it is read once, for the tree returned
    pub(crate) async fn native_settle(&self, g: &mut Inner, max: Duration) -> crate::error::Result<(String, Tree)> {
        let t0 = Instant::now();
        if let Some(left) = g.dev.acted_at.and_then(|t| REACT.checked_sub(t.elapsed())) {
            tokio::time::sleep(left).await;
        }
        let mut snap = self.ax_snapshot(g, AX_WAIT, false).await?;
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

    async fn snapshot_tree(&self, g: &mut Inner, s: &ictl_device::ax::AxScreen) -> crate::error::Result<Tree> {
        let (xml, size, _) = self.ax_render(g, s).await?;
        let mut tree = elements::parse_tree(&xml, size);
        tree.incomplete |= !s.app_complete || !s.overlays_complete;
        Ok(tree)
    }

    /// `s`, a read of the app alone, with what SpringBoard shows over it, as a tree
    async fn with_overlays(&self, g: &mut Inner, mut s: ictl_device::ax::AxScreen) -> crate::error::Result<Tree> {
        if let Some(t) = g.tunnel.as_mut() {
            (s.overlays, s.overlays_complete) = t.ax_overlays(s.app_pid).await;
        }
        self.snapshot_tree(g, &s).await
    }

    /// The frontmost app's elements alone ([`Self::ax_read_with`])
    async fn ax_tree_app(&self, g: &mut Inner) -> crate::error::Result<Tree> {
        let (xml, size, _) = self.ax_read_with(g, AX_WAIT, false).await?;
        Ok(elements::parse_tree(&xml, size))
    }

    /// After a swipe, waits for the list to stop (at most [`SCROLL_SETTLE`]): two reads in a row with the elements
    /// in the same place along the swipe. Says when it was still moving; a screen with few elements isn't waited for
    pub(crate) async fn native_scrolled(&self, g: &mut Inner, vertical: bool) -> String {
        let t0 = Instant::now();

        let Ok(mut prev) = self.ax_tree_app(g).await else { return String::new() };
        if prev.elements.len() < VISUAL_SETTLE_BELOW {
            return String::new();
        }
        while t0.elapsed() < SCROLL_SETTLE {
            let Ok(cur) = self.ax_tree_app(g).await else { return String::new() };
            let still = cur.still_along_common(&prev, vertical, STILL_TOLERANCE);
            tracing::debug!(
                "scroll wait at {:.2} s: {} then {} elements, still: {still}",
                t0.elapsed().as_secs_f64(),
                prev.elements.len(),
                cur.elements.len()
            );
            if still {
                return String::new();
            }
            prev = cur;
        }
        ictl_l10n::trf!(
            "({:.1} 秒后列表还在动,可能还在滚动)",
            " (the list was still moving after {:.1} s, it may still be scrolling)",
            SCROLL_SETTLE.as_secs_f64()
        )
    }

    /// Text appearing on screen, natively: the tree read again every 0.3 s
    pub(crate) async fn native_wait_for(
        &self,
        g: &mut Inner,
        text: &str,
        timeout: Duration,
        disappeared: bool,
    ) -> crate::error::Result<String> {
        let t0 = Instant::now();
        loop {
            let screen = self.ax_snapshot(g, AX_WAIT, true).await?;
            let (xml, size, _) = self.ax_render(g, &screen).await?;
            let mut tree = elements::parse_tree(&xml, size);
            tree.incomplete |= !screen.app_complete || !screen.overlays_complete;
            if let Some(result) = super::observe::wait_match(&tree, text, disappeared, t0.elapsed()) {
                if disappeared
                    && (self.native_locked(g).await?
                        || (screen.springboard_front && !unlocked_springboard(&xml, &tree)))
                {
                    return Err(super::observe::wait_locked());
                }
                return Ok(result);
            }
            if t0.elapsed() >= timeout {
                return Err(super::observe::wait_timeout(text, timeout, disappeared));
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

    /// Home, natively: not pressed under a system alert (it would do nothing), and confirmed by the home screen
    /// coming to the front; when the key does nothing, a swipe up from the bottom edge does it
    pub(crate) async fn native_home(&self, g: &mut Inner, watched: bool) -> crate::error::Result<String> {
        if !watched && let (_, Some(text)) = self.native_alert(g).await? {
            return Ok(ictl_l10n::trf!(
                "{},按 Home 不会生效,没有按;{TAP_ALERT}",
                "{}, pressing Home would have no effect, not pressed; {TAP_ALERT_EN}",
                blocking(&text)
            ));
        }
        // the HID session opened first, the wait below counted from the press: opening it (the screen stream, about
        // 0.6 s after the daemon left it idle) used up the whole wait, and an app launched right after a Home on the
        // home screen was sent back there (7 times in 8; 0 in 8 with the session already open)
        self.hid(g).await?;
        let pressed = Instant::now();
        self.hid_do(g, |h| Box::pin(h.press(Button::Home))).await?;
        if self.native_reached_home(g).await {
            // already on the home screen (Home closed a menu, a folder): what it closes is still animating, and an
            // app launched meanwhile was sent back to the home screen once it ended (3 times in 3)
            if let Some(left) = HOME_ANIMATION.checked_sub(pressed.elapsed()) {
                tokio::time::sleep(left).await;
            }
            return Ok(ictl_l10n::tr!("已按 home", "Pressed home").into());
        }
        let s = self.screen(g).await?;
        let rot = self.hid_touch_rotation(g).await?;
        let (from, to) = ((s.width / 2.0, s.height - 2.0), (s.width / 2.0, s.height * 0.55));
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                h.drag(from, to, 0.25).await
            })
        })
        .await?;
        if self.native_reached_home(g).await {
            return Ok(ictl_l10n::tr!(
                "已按 home(Home 键没反应,改为从屏幕底部上滑)",
                "Pressed home (the key did nothing, swiped up from the bottom edge instead)"
            )
            .into());
        }
        if let (_, Some(text)) = self.native_alert(g).await? {
            return Ok(ictl_l10n::trf!(
                "{},按 Home 没有生效,先点弹窗上的按钮",
                "{}, pressing Home had no effect; first tap a button on the alert",
                blocking(&text)
            ));
        }
        let app = self.native_foreground(g).await.unwrap_or_default();
        Ok(ictl_l10n::trf!(
            "已按 home,但前台还是 {app},先截图看看",
            "Pressed home, but the foreground is still {app}; take a screenshot"
        ))
    }

    /// Whether the home screen comes to the front within [`HOME_SETTLE`]
    async fn native_reached_home(&self, g: &mut Inner) -> bool {
        let deadline = Instant::now() + HOME_SETTLE;
        loop {
            if self.native_foreground(g).await.is_ok_and(|b| b == SPRINGBOARD) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// A tap at a point through HID, the interface's rotation read first
    pub(crate) async fn native_tap(&self, g: &mut Inner, x: f64, y: f64) -> crate::error::Result<()> {
        let rot = self.hid_touch_rotation(g).await?;
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                h.tap(x, y).await
            })
        })
        .await
    }

    pub(crate) async fn native_long_press(
        &self,
        g: &mut Inner,
        x: f64,
        y: f64,
        seconds: f64,
    ) -> crate::error::Result<()> {
        let rot = self.hid_touch_rotation(g).await?;
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                h.long_press(x, y, seconds).await
            })
        })
        .await
    }

    /// Typing, natively: all text is pasted, the paste prompt answered, and the field read back before the
    /// pasteboard is put back and Return pressed. Not as keys: HID keys go through the hardware keyboard's input
    /// source, which composed them as pinyin with the on-screen keyboard showing English («wifi» became marked
    /// pinyin «w i f i»), and the "next keyboard" button names the keyboard it switches to, not the one shown
    pub(crate) async fn native_type(&self, g: &mut Inner, text: &str, submit: bool) -> crate::error::Result<String> {
        let n = text.chars().count();
        let before = self.ax_tree_app(g).await?;
        if !keyboard_up(&before) {
            return Err(ictl_l10n::tr!(
                "屏幕上没有键盘(没有输入框获得焦点),什么都没输;先点一下输入框,截图确认键盘出来了再输入",
                "No keyboard on screen (no text field has focus), nothing typed; tap a text field first and check with a screenshot that the keyboard is up"
            )
            .into());
        }
        let old = elements::field_values(&before.elements);
        let mut note = String::new();
        if !text.is_empty() {
            let (typed, prompt) = self.native_paste(g, text, PasteTarget::Append(old.as_deref())).await?;
            if prompt {
                note += ictl_l10n::tr!("(已替你点了「允许粘贴」)", " (“Allow Paste” was tapped for you)");
            }
            match typed {
                Pasted::Typed => {}
                Pasted::Unreadable => {
                    note += ictl_l10n::tr!(
                        "(这是密码框,读不出内容,截图确认输进去了)",
                        " (a password field: its contents can't be read back, check with a screenshot)"
                    )
                }
                // the same text, selected, replaced by the paste (a search field keeping the last search)
                Pasted::Unchanged => {
                    note += ictl_l10n::tr!(
                        "(输入框里原来就是这段文字,粘贴后看不出变化,截图确认)",
                        " (the field already held this text, so the paste shows no change; check with a screenshot)"
                    )
                }
                Pasted::Missing => {
                    return Err(ictl_l10n::trf!(
                        "粘贴了 {n} 个字符,但 {} 秒内输入框里没有这段文字{};截图看看:输入框可能没有焦点,或 App 不接受粘贴",
                        "Pasted {n} characters, but the field didn't show the text within {} s{}; take a screenshot: the field may not have focus, or the app doesn't take pastes",
                        PASTE_WAIT.as_secs(),
                        if submit { ictl_l10n::tr!(",没有按回车", ", Return not pressed") } else { "" }
                    )
                    .into());
                }
            }
        }
        if submit {
            self.hid_do(g, |h| Box::pin(async move { h.type_ascii("\n").await })).await.map_err(|e| {
                ictl_l10n::trf!("文字已输入{note},按回车失败:{e}", "Text typed{note}, pressing Return failed: {e}")
            })?;
        }
        Ok(ictl_l10n::trf!(
            "已输入 {n} 个字符{note}{}",
            "Typed {n} characters{note}{}",
            if submit { ictl_l10n::tr!(",并按了回车", " and pressed Return") } else { "" }
        ))
    }

    /// Replace a single focused, readable field. Verify the full value before sending Return.
    pub(crate) async fn replace_text(&self, g: &mut Inner, text: &str, submit: bool) -> crate::error::Result<String> {
        if !self.native(g) || self.wda(g).is_some() {
            return Err(ictl_l10n::tr!("替换全文需要原生控制", "Replacing text requires native control").into());
        }
        self.ensure_unlocked(g).await?;
        let before = self.ax_snapshot(g, AX_WAIT, false).await?;
        if !keyboard_up(&self.snapshot_tree(g, &before).await?) {
            return Err(ictl_l10n::tr!(
                "屏幕上没有键盘，先点一下要替换的输入框",
                "No keyboard on screen; tap the field you want to replace first"
            )
            .into());
        }
        let (target, old) =
            focused_text_field(&before).and_then(|f| Some((f.object?, f.value.as_str()))).ok_or_else(|| {
                ictl_l10n::tr!(
                    "无法确认当前聚焦的是可读的普通输入框，没有替换文字",
                    "Couldn't identify a readable focused text field; no text was replaced"
                )
            })?;
        if old != text {
            let result = async {
                self.hid_do(g, |h| Box::pin(h.select_all())).await?;
                let (seen, _) = self.native_paste(g, text, PasteTarget::Replace(target)).await?;
                if seen != Pasted::Typed {
                    return Err(ictl_l10n::tr!(
                        "替换后无法核对输入框全文，未按回车；请查看手机界面",
                        "Couldn't verify the full field value after the replacement; Return was not pressed"
                    )
                    .into());
                }
                Ok(())
            }
            .await;
            if let Err(err) = result {
                if let Err(cause) = self.collapse_field_selection(g, target).await {
                    return Err(ictl_l10n::trf!(
                        "{err}（无法收起原输入框的选区，请在重试前查看屏幕：{cause}）",
                        "{err} (couldn't collapse the original selection; inspect the screen before retrying: {cause})"
                    )
                    .into());
                }
                return Err(err);
            }
        }
        if submit {
            self.hid_do(g, |h| Box::pin(h.type_ascii("\n"))).await?;
        }
        let n = text.chars().count();
        Ok(if submit {
            ictl_l10n::trf!(
                "已替换为 {n} 个字符，并按了回车",
                "Replaced the field with {n} characters and pressed Return"
            )
        } else {
            ictl_l10n::trf!("已替换为 {n} 个字符", "Replaced the field with {n} characters")
        })
    }

    async fn collapse_field_selection(&self, g: &mut Inner, target: ictl_ax::ObjectRef) -> crate::error::Result<()> {
        let screen = self.ax_snapshot(g, AX_WAIT, true).await?;
        if focused_text_field(&screen).and_then(|f| f.object) != Some(target)
            || self.snapshot_tree(g, &screen).await?.alert().is_some()
        {
            return Err(ictl_l10n::tr!("输入焦点已改变或被弹窗遮挡", "Focus changed or an alert is in front").into());
        }
        self.hid_do(g, |h| Box::pin(h.collapse_selection())).await
    }

    /// Answer the system paste prompt and verify the requested edit before restoring the clipboard.
    async fn native_paste(
        &self,
        g: &mut Inner,
        text: &str,
        target: PasteTarget<'_>,
    ) -> crate::error::Result<(Pasted, bool)> {
        // a paste through the WDA-down path may still wait to be put back
        self.restore_pasteboard(g, false).await;
        self.hid(g).await?;
        let r = Box::pin(g.tunnel.as_mut().expect("hid() 刚建好隧道").paste_text(text)).await;
        g.hid_used = Some(Instant::now());
        let pending = match r {
            Ok(p) => p,
            Err(e @ DeviceError::InvalidInput(_)) => return Err(e.into()),
            Err(e) => {
                drop_tunnel(g).await;
                return Err(ictl_l10n::trf!("粘贴失败:{e}", "Paste failed: {e}").into());
            }
        };
        g.dev.pending_paste = pending.map(|p| (Instant::now(), p));
        let result = async {
            let deadline = Instant::now() + PASTE_WAIT;
            // when the paste prompt was last answered
            let mut answered: Option<Instant> = None;
            let typed = loop {
                let screen = self.ax_snapshot(g, AX_WAIT, true).await?;
                let tree = self.snapshot_tree(g, &screen).await?;
                if let Some((x, y)) = paste_allow(&tree) {
                    self.native_tap(g, x, y).await?;
                    answered = Some(Instant::now());
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if Instant::now() >= deadline {
                        break Pasted::Missing;
                    }
                    continue;
                }
                let seen = match target {
                    PasteTarget::Append(old) => {
                        paste_seen(elements::field_values(&tree.elements).as_deref(), old, text.trim())
                    }
                    PasteTarget::Replace(object) => replacement_seen(&screen, object, text),
                };
                match seen {
                    Pasted::Typed => break seen,
                    // only once the prompt was answered: before that, the paste hasn't gone in yet
                    Pasted::Unchanged if answered.is_some_and(|t| t.elapsed() >= PASTE_SAME_SETTLE) => break seen,
                    // never asked: iOS asks for every paste of ours, so nothing may have been pasted at all
                    Pasted::Unchanged if Instant::now() >= deadline => break Pasted::Missing,
                    _ if Instant::now() >= deadline => break seen,
                    _ => {}
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            };
            Ok((typed, answered.is_some()))
        }
        .await;
        self.restore_pasteboard(g, false).await;
        result
    }

    /// Launches `bundle_id` in front (with `url`, the link for it to open) and waits for it to be there
    pub(crate) async fn native_launch(
        &self,
        g: &mut Inner,
        bundle_id: &str,
        url: Option<&str>,
    ) -> crate::error::Result<ictl_wda::Launch> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let t0 = Instant::now();
        self.ensure_tunnel(g, &device).await?.launch_foreground(bundle_id, url).await?;
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let (xml, size, pid) = self.ax_read(g, AX_WAIT).await?;
            let tree = elements::parse_tree(&xml, size);
            if let Some(a) = tree.alert().filter(|a| !a.label.is_empty()) {
                return Ok(ictl_wda::Launch::Blocked(a.label.clone()));
            }
            if self.bundle_of_pid(g, pid).await.is_ok_and(|b| b == bundle_id) {
                if let Some(left) = LAUNCH_SETTLE.checked_sub(t0.elapsed()) {
                    tokio::time::sleep(left).await;
                }
                return Ok(ictl_wda::Launch::Foreground);
            }
            if t0.elapsed() >= LAUNCH_WAIT {
                return Ok(ictl_wda::Launch::Slow);
            }
        }
    }

    /// Ends `bundle_id`'s process (SIGKILL through CoreDevice); Ok(false) when it wasn't running
    pub(crate) async fn native_terminate(&self, g: &mut Inner, bundle_id: &str) -> crate::error::Result<bool> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let procs = self.ensure_tunnel(g, &device).await?.processes().await?;
        let mut apps = self.list_apps(g, false).await?;
        apps.extend(self.list_apps(g, true).await?);
        let pids: Vec<u64> = procs
            .iter()
            .filter(|(_, exe)| apps::bundle_of_process(exe, &apps).as_deref() == Some(bundle_id))
            .map(|(p, _)| *p)
            .collect();
        for pid in &pids {
            self.ensure_tunnel(g, &device).await?.signal(*pid, 9).await?;
        }
        // gone before this says so: a launch right after one killed failed with «The process identifier of the
        // launched application could not be determined. It may have already terminated.»
        let deadline = Instant::now() + TERMINATE_WAIT;
        while !pids.is_empty() && Instant::now() < deadline {
            let procs = self.ensure_tunnel(g, &device).await?.processes().await?;
            if !procs.iter().any(|(p, _)| pids.contains(p)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(!pids.is_empty())
    }

    /// The app that opens `url`
    pub(crate) async fn native_app_for_url(&self, g: &mut Inner, url: &str) -> crate::error::Result<String> {
        let mut apps = self.list_apps(g, false).await?;
        apps.extend(self.list_apps(g, true).await?);
        apps::app_for_url(url, &apps)
            .ok_or_else(|| ictl_l10n::trf!("手机上没有 App 能打开 {url}", "No app on the phone opens {url}").into())
    }

    /// Unlocks natively, with the same rules and guards as through WDA (`ictl_unlock`)
    pub(crate) async fn native_unlock(
        &self,
        g: &mut Inner,
        cfg: &ictl_unlock::UnlockConfig,
    ) -> ictl_unlock::Result<ictl_unlock::Outcome> {
        // Home wakes a dark screen within about 0.3 s; the keypad is looked for as it comes (`reads_cheaply`)
        let cfg = ictl_unlock::UnlockConfig {
            passcode_file: cfg.passcode_file.clone(),
            disabled_flag: cfg.disabled_flag.clone(),
            timing: ictl_unlock::Timing { wake_settle: NATIVE_WAKE_SETTLE, ..cfg.timing },
        };
        let mut screen = NativeScreen { d: self, g };
        let r = Box::pin(ictl_unlock::ensure_unlocked(&mut screen, &cfg)).await;
        if matches!(r, Ok(ictl_unlock::Outcome::Unlocked)) {
            // the lock screen is still sliding away: a tap now is lost (a tap by text right after unlocking did
            // nothing on the phone)
            tokio::time::sleep(UNLOCK_SETTLE).await;
        }
        r
    }
}

// PasswordProtected stays false on a phone without a passcode. Only accept a recognisable unlocked
// SpringBoard surface: a cover sheet or an unidentified system screen cannot establish absence.
fn unlocked_springboard(xml: &str, tree: &Tree) -> bool {
    elements::cover_sheet(xml) == elements::CoverSheet::None
        && tree.elements.iter().any(|e| e.kind == "Icon" || e.name == "SpotlightSearchField")
}

fn focused_text_field(screen: &ictl_device::ax::AxScreen) -> Option<&ictl_ax::Element> {
    let mut fields = screen.app.iter().filter(|e| {
        // Raw AX retains focus and exact text, including whitespace. The shared XML has been normalized for display.
        e.focused && matches!(ictl_ax::xml::kind(e), "TextField" | "TextView" | "SearchField" | "SecureTextField")
    });
    let f = fields.next()?;
    (fields.next().is_none() && f.enabled && f.visible && ictl_ax::xml::kind(f) != "SecureTextField").then_some(f)
}

fn replacement_seen(screen: &ictl_device::ax::AxScreen, target: ictl_ax::ObjectRef, text: &str) -> Pasted {
    if focused_text_field(screen).is_some_and(|f| f.object == Some(target) && f.value == text) {
        Pasted::Typed
    } else {
        Pasted::Missing
    }
}

/// The phone as [`ictl_unlock`] sees it, driven natively
struct NativeScreen<'a> {
    d: &'a Daemon,
    g: &'a mut Inner,
}

/// Reads the lock screen: with the HID session's screen stream open, the phone hides the lock screen and its
/// passcode keypad from accessibility (one element, the app's, for every session; iPhone 15 Pro Max, iOS 27.2.1), so
/// the session is closed first, and the elements come back a moment after. The next tap opens it again (about 0.5 s).
/// Not before the last touch has landed ([`TOUCH_LANDS`])
async fn read_lock_screen(d: &Daemon, g: &mut Inner) -> crate::error::Result<(String, ScreenSize, i64)> {
    let was_open = g.tunnel.as_ref().is_some_and(ictl_device::Tunnel::hid_open);
    if was_open && let Some(left) = touch_lands_in(g.hid_used.map(|t| t.elapsed())) {
        tokio::time::sleep(left).await;
    }
    crate::core::close_hid(g).await;
    let t0 = Instant::now();
    loop {
        let read = Box::pin(d.ax_read(g, Duration::from_secs(3))).await?;
        let shown = read.0.matches("<XCUIElementType").count() > 1;
        if shown || !was_open || t0.elapsed() >= LOCK_SCREEN_BACK {
            tracing::debug!(
                "lock screen read {:.0} ms after closing HID, elements shown: {shown}",
                t0.elapsed().as_secs_f64() * 1000.0
            );
            return Ok(read);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// A touch sent over HID doesn't count when its session closes right after it: the passcode keypad came up, but no
/// key went down and the field stayed at 0 digits, so unlocking failed with "couldn't bring up the keypad" (or
/// stopped at the 2nd or 3rd digit). Not every time: the same phone unlocked with no wait for hours, then lost
/// every key. Unlocks while it lost them, by how long the session stayed open after the touch (iPhone19,7,
/// iOS 27.2.1, one daemon, the waits taken in turn): 0 ms 0 of 9, 50 ms 0 of 3, 100 ms 0 of 3, 150 ms 1 of 3,
/// 200 ms 3 of 4, 250 ms 5 of 5, 350 ms 3 of 3. An unlock takes about 1.5 s longer for it (8.3 s without)
const TOUCH_LANDS: Duration = Duration::from_millis(350);

/// How much longer the HID session has to stay open for the last touch sent over it to count, `since` it was sent
fn touch_lands_in(since: Option<Duration>) -> Option<Duration> {
    TOUCH_LANDS.checked_sub(since?).filter(|d| !d.is_zero())
}

/// How long after the HID session closes the lock screen's elements may take to come back
const LOCK_SCREEN_BACK: Duration = Duration::from_secs(3);

/// A step that failed: unlocking tries another round
fn step(e: impl std::fmt::Display) -> WdaError {
    WdaError::Remote { kind: "native".into(), message: e.to_string() }
}

impl Screen for NativeScreen<'_> {
    async fn reports_locked(&mut self) -> std::result::Result<bool, WdaError> {
        self.d.native_locked(self.g).await.map_err(|e| WdaError::Unavailable(e.to_string()))
    }

    fn lock_state_trusted(&self) -> bool {
        // lockdown knows: no lock-screen-looking notification center to tell apart
        true
    }

    async fn check_lock(&mut self, _reported: Option<bool>, _strict: bool) -> std::result::Result<LockCheck, WdaError> {
        let locked = self.d.native_locked(self.g).await.map_err(|e| WdaError::Unavailable(e.to_string()))?;
        let tree = if locked { read_lock_screen(self.d, self.g).await.ok().map(|(x, _, _)| x) } else { None };
        Ok(LockCheck { locked, uncertain: false, tree })
    }

    async fn quick_waits(&mut self) -> std::result::Result<Option<ictl_wda::IdleSettings>, WdaError> {
        Ok(None)
    }

    async fn restore_waits(&mut self, _: ictl_wda::IdleSettings) -> std::result::Result<(), WdaError> {
        Ok(())
    }

    async fn press_home(&mut self, _round: u32) -> std::result::Result<(), WdaError> {
        self.d.hid_do(self.g, |h| Box::pin(h.press(Button::Home))).await.map_err(step)
    }

    async fn view(&mut self) -> std::result::Result<View, WdaError> {
        self.d.screen(self.g).await.map(View::from).map_err(|e| WdaError::Unavailable(e.to_string()))
    }

    async fn swipe(&mut self, from: (f64, f64), to: (f64, f64), ms: u64) -> std::result::Result<(), WdaError> {
        let rot = self.d.hid_touch_rotation(self.g).await.map_err(step)?;
        let seconds = ms as f64 / 1000.0;
        self.d
            .hid_do(self.g, move |h| {
                Box::pin(async move {
                    h.rotation = rot;
                    h.drag(from, to, seconds).await
                })
            })
            .await
            .map_err(step)
    }

    async fn tap(&mut self, x: f64, y: f64, _press_ms: u64) -> std::result::Result<(), WdaError> {
        self.d.native_tap(self.g, x, y).await.map_err(step)
    }

    async fn elements(&mut self) -> std::result::Result<Vec<elements::Element>, WdaError> {
        let (xml, size, _) = read_lock_screen(self.d, self.g).await.map_err(step)?;
        Ok(elements::parse(&xml, size))
    }

    async fn passcode_field(&mut self) -> std::result::Result<Option<(u32, u32)>, WdaError> {
        Ok(elements::passcode_field(&self.elements().await?))
    }

    fn reads_cheaply(&self) -> bool {
        // a read of the lock screen takes about 0.1 s
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_recognisable_unlocked_springboard_can_confirm_absence() {
        let check = |markers: &str| {
            let xml = format!(r#"<App>{markers}</App>"#);
            let tree = elements::parse_tree(&xml, ScreenSize { width: 390.0, height: 844.0 });
            unlocked_springboard(&xml, &tree)
        };
        let icon = r#"<XCUIElementTypeIcon type="Icon" label="Clock" x="10" y="10" width="54" height="54"/>"#;
        assert!(check(icon));
        assert!(!check(&format!(r#"{icon}<Other name="lockscreen-date-view"/>"#)));
        assert!(!check(r#"<Other name="proud-lock"/>"#));
        assert!(!check(r#"<StaticText label="12:25"/>"#));
    }

    #[test]
    fn replacing_text_requires_one_readable_focused_field() {
        let mut screen = ictl_device::ax::AxScreen {
            app: vec![
                ictl_ax::Element {
                    xc_type: 49,
                    value: "old".into(),
                    visible: true,
                    enabled: true,
                    ..Default::default()
                },
                ictl_ax::Element {
                    xc_type: 49,
                    value: " 蓝牙 ".into(),
                    focused: true,
                    visible: true,
                    enabled: true,
                    ..Default::default()
                },
            ],
            app_complete: true,
            overlays: Vec::new(),
            overlays_complete: true,
            app_pid: 1,
            springboard_front: false,
            at: Instant::now(),
        };
        assert_eq!(focused_text_field(&screen).map(|f| f.value.as_str()), Some(" 蓝牙 "));
        let target = ictl_ax::ObjectRef { pid: 1, id: 2, is_app: false };
        screen.app[1].object = Some(target);
        screen.app[1].value = "wifi".into();
        screen.app[0].value = "wi".into();
        screen.app_complete = false;
        assert_eq!(replacement_seen(&screen, target, "wi"), Pasted::Missing);
        screen.app[1].value = "wi".into();
        assert_eq!(replacement_seen(&screen, target, "wi"), Pasted::Typed);
        assert_eq!(replacement_seen(&screen, ictl_ax::ObjectRef { id: 3, ..target }, "wi"), Pasted::Missing);
        screen.app[0].focused = true;
        assert_eq!(focused_text_field(&screen).map(|f| f.value.as_str()), None);
        screen.app[0].focused = false;
        screen.app[1].value = "••••".into();
        assert_eq!(focused_text_field(&screen).map(|f| f.value.as_str()), Some("••••"));
        screen.app[1].xc_type = 50; // SecureTextField
        assert_eq!(focused_text_field(&screen).map(|f| f.value.as_str()), None);
    }

    /// The HID session isn't closed right after a touch (the phone takes the touch back): the rest of
    /// `TOUCH_LANDS` is waited out first, and nothing when no touch was sent or it was long enough ago
    #[test]
    fn the_hid_session_stays_open_until_the_last_touch_has_landed() {
        let ms = Duration::from_millis;
        assert_eq!(touch_lands_in(Some(ms(0))), Some(TOUCH_LANDS));
        assert_eq!(touch_lands_in(Some(ms(100))), Some(TOUCH_LANDS - ms(100)));
        assert_eq!(touch_lands_in(Some(TOUCH_LANDS)), None);
        assert_eq!(touch_lands_in(Some(ms(5000))), None);
        assert_eq!(touch_lands_in(None), None);
    }

    fn info(version: &str) -> DeviceInfo {
        DeviceInfo {
            udid: "u".into(),
            name: "n".into(),
            product_type: "iPhone16,2".into(),
            product_version: version.into(),
        }
    }

    #[test]
    fn ios_27_and_later_are_native_unless_wda_is_asked_for() {
        assert!(native_for(IosBackend::Auto, Some(&info("27.2.1"))));
        assert!(native_for(IosBackend::Native, Some(&info("28.0"))));
        assert!(!native_for(IosBackend::Wda, Some(&info("27.2.1"))));
        // no HID below 27
        assert!(!native_for(IosBackend::Native, Some(&info("26.7.3"))));
        assert!(!native_for(IosBackend::Auto, None));
    }

    #[test]
    fn the_paste_prompt_is_answered_only_for_our_paste() {
        let xml = |alert: &str| {
            format!(
                r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" label="备忘录" x="0" y="0" width="430" height="932"><XCUIElementTypeAlert type="XCUIElementTypeAlert" label="{alert}" x="55" y="390" width="320" height="175"><XCUIElementTypeButton type="XCUIElementTypeButton" label="允许粘贴" x="71" y="501" width="140" height="49"/><XCUIElementTypeButton type="XCUIElementTypeButton" label="不允许粘贴" x="219" y="501" width="140" height="49"/></XCUIElementTypeAlert></XCUIElementTypeApplication>"#
            )
        };
        let screen = ScreenSize { width: 430.0, height: 932.0 };
        let ours = elements::parse_tree(&xml("“备忘录”想从“dtpasteboardd”粘贴"), screen);
        assert_eq!(paste_allow(&ours), Some((141.0, 526.0)));
        let other = elements::parse_tree(&xml("“备忘录”想从“微信”粘贴"), screen);
        assert_eq!(paste_allow(&other), None);
    }

    #[test]
    fn a_paste_that_replaced_the_same_text_is_told_apart_from_one_that_missed() {
        let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // into an empty field, and after text already there
        assert_eq!(paste_seen(Some(&v(&["设置"])), Some(&v(&[""])), "设置"), Pasted::Typed);
        assert_eq!(paste_seen(Some(&v(&["设置设置"])), Some(&v(&["设置"])), "设置"), Pasted::Typed);
        // Spotlight's last search, selected, replaced by the same text: nothing changes
        assert_eq!(paste_seen(Some(&v(&["设置"])), Some(&v(&["设置"])), "设置"), Pasted::Unchanged);
        assert_eq!(paste_seen(Some(&v(&["时钟"])), Some(&v(&["时钟"])), "设置"), Pasted::Missing);
        assert_eq!(paste_seen(Some(&v(&[""])), Some(&v(&[""])), "设置"), Pasted::Missing);
        assert_eq!(paste_seen(None, Some(&v(&[""])), "设置"), Pasted::Unreadable);
        assert_eq!(paste_seen(Some(&v(&["设置"])), None, "设置"), Pasted::Unreadable);
    }
}
