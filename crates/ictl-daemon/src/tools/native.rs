//! Native control of iOS 27 and later, without WebDriverAgent (`ICTL_IOS_BACKEND`, docs/dev/ios27-native.md):
//! the screen through remoteAXService (`ictl_ax`, turned into WDA's XML so `ictl_wda::elements` reads it), touches,
//! keys and Home through HID, text through the pasteboard, apps and links through CoreDevice AppService, the lock
//! state through lockdown.
//!
//! The tools keep their WDA code; where WDA isn't there they come here when [`Daemon::native`] says so.

use ictl_device::{ClipboardRestore, DeviceInfo};
use ictl_unlock::Screen;
use ictl_wda::client::LockCheck;
use ictl_wda::elements::View;

use super::*;
use crate::config::IosBackend;

/// A read of the screen waits at most this long for a tree (the first one of a session comes in about a second)
pub(crate) const AX_WAIT: Duration = Duration::from_secs(5);
/// After an action, the screen is first read this long after it started: an app reacts to a tap within about
/// 0.1 s, a page slides in within about 0.35 s (the second read, which confirms, comes about 0.2 s later)
const REACT: Duration = Duration::from_millis(300);
/// After Cmd-V: how long for the text to show in the field, answering the paste prompt meanwhile
const PASTE_WAIT: Duration = Duration::from_secs(5);
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

/// The on-screen keyboard is up: a Keyboard element, or its keys (the keyboard's process reads as the app's own
/// elements at times, then no Keyboard is made of them, `ictl_ax::xml`)
fn keyboard_up(tree: &Tree) -> bool {
    tree.elements.iter().any(|e| e.kind == "Keyboard") || tree.elements.iter().filter(|e| e.kind == "Key").count() >= 8
}

/// The keyboard's input mode, as its "next keyboard" button says it («简体拼音», "English (US)")
fn keyboard_mode(tree: &Tree) -> Option<&str> {
    tree.elements
        .iter()
        .find(|e| matches!(e.label.as_str(), "下一个键盘" | "下一個鍵盤" | "Next keyboard" | "Next Keyboard"))
        .map(|e| e.value.as_str())
        .filter(|v| !v.is_empty())
}

/// An input mode that takes keys as they are: English. Anything else (Pinyin, Zhuyin, Kana, Hangul and the
/// like compose; other layouts may map keys elsewhere) is pasted
fn types_keys_as_they_are(mode: &str) -> bool {
    let m = mode.to_lowercase();
    m.contains("english") || m.contains("英语") || m.contains("英文") || m.contains("英語")
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
        let t = self
            .ensure_tunnel(g, &device)
            .await
            .map_err(|e| ictl_l10n::trf!("读界面失败（建隧道）：{e}", "Reading the screen failed (tunnel): {e}"))?;
        let r = t.ax_screen(wait, overlays).await;
        if r.is_err() {
            t.close_ax().await;
        }
        g.ax_used = Some(Instant::now());
        Ok(r.map_err(|e| ictl_l10n::trf!("读界面失败：{e}", "Reading the screen failed: {e}"))?)
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
        let (xml, size, _) = self.ax_read(g, wait).await?;
        Ok(elements::parse_tree(&xml, size))
    }

    /// The frontmost app's bundle ID (one request, no walk)
    pub(crate) async fn native_foreground(&self, g: &mut Inner) -> crate::error::Result<String> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let t = self.ensure_tunnel(g, &device).await?;
        let r = t.ax_frontmost_pid(AX_WAIT).await;
        if r.is_err() {
            t.close_ax().await;
        }
        g.ax_used = Some(Instant::now());
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
            // between two pages (a slide back) the tree can be nearly empty for a moment: once more
            tokio::time::sleep(Duration::from_millis(300)).await;
            snap = self.ax_snapshot(g, AX_WAIT, false).await?;
            prev = self.snapshot_tree(g, &snap).await?;
        }
        if prev.elements.len() < VISUAL_SETTLE_BELOW {
            // too few elements to tell (a game, a map): the caller looks at the picture
            return Ok((String::new(), self.with_overlays(g, snap).await?));
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
        Ok(elements::parse_tree(&xml, size))
    }

    /// `s`, a read of the app alone, with what SpringBoard shows over it, as a tree
    async fn with_overlays(&self, g: &mut Inner, mut s: ictl_device::ax::AxScreen) -> crate::error::Result<Tree> {
        if let Some(t) = g.tunnel.as_mut() {
            s.overlays = t.ax_overlays(s.app_pid).await;
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
    ) -> crate::error::Result<String> {
        let t0 = Instant::now();
        loop {
            let tree = self.ax_tree(g, AX_WAIT).await?;
            // 不算输入框里的字：刚输入的搜索词不能算「结果出来了」
            if let Some(e) = elements::find_shown(&tree.elements, text).first() {
                return Ok(ictl_l10n::trf!(
                    "已出现 {}({:.1} 秒)",
                    "Appeared: {} ({:.1} s)",
                    e.describe(),
                    t0.elapsed().as_secs_f64()
                ));
            }
            if t0.elapsed() >= timeout {
                return Err(ictl_l10n::trf!(
                    "{:.0} 秒内没出现包含 \"{text}\" 的元素,截图看看屏幕",
                    "No element containing \"{text}\" appeared within {:.0} s; take a screenshot to look at the screen",
                    timeout.as_secs_f64()
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
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

    /// Typing, natively: all text is pasted (keys on a Pinyin keyboard would be taken as pinyin), the paste prompt
    /// answered, and the field read back before the pasteboard is put back and Return pressed
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
        // an English keyboard takes keys as they are: no paste, no prompt (Pinyin would compose them)
        if !text.is_empty()
            && keyboard_mode(&before).is_some_and(types_keys_as_they_are)
            && text.chars().all(|c| ascii_to_usage(c).is_some())
        {
            let payload = if submit { format!("{text}\n") } else { text.to_owned() };
            self.hid_do(g, move |h| Box::pin(async move { h.type_ascii(&payload).await })).await?;
            if old.is_none() {
                note += ictl_l10n::tr!(
                    "(这是密码框,读不出内容,截图确认输进去了)",
                    " (a password field: its contents can't be read back, check with a screenshot)"
                );
            } else if !submit && let Some(now) = self.native_field_shows(g, text).await? {
                return Err(ictl_l10n::trf!(
                    "按键输入了 {n} 个字符,但输入框里现在是「{now}」,和要输入的「{text}」对不上(可能被自动更正或吞掉了);截图确认后再处理",
                    "Typed {n} characters as keys, but the field now contains “{now}”, which doesn't match “{text}” (autocorrected or swallowed); check with a screenshot before going on"
                )
                .into());
            }
            return Ok(ictl_l10n::trf!(
                "已输入 {n} 个字符{note}{}",
                "Typed {n} characters{note}{}",
                if submit { ictl_l10n::tr!(",并按了回车", " and pressed Return") } else { "" }
            ));
        }
        if !text.is_empty() {
            let (typed, prompt) = self.native_paste(g, text, old.as_deref()).await?;
            if prompt {
                note += ictl_l10n::tr!("(已替你点了「允许粘贴」)", " (“Allow Paste” was tapped for you)");
            }
            match typed {
                Some(true) => {}
                // a password field: its value can't be read
                None => {
                    note += ictl_l10n::tr!(
                        "(这是密码框,读不出内容,截图确认输进去了)",
                        " (a password field: its contents can't be read back, check with a screenshot)"
                    )
                }
                Some(false) => {
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

    /// After typing `text` as keys: None once a field shows it (within about a second; the tree may lag a key
    /// behind), else what the field shows
    async fn native_field_shows(&self, g: &mut Inner, text: &str) -> crate::error::Result<Option<String>> {
        let deadline = Instant::now() + Duration::from_millis(1200);
        loop {
            let values = elements::field_values(&self.ax_tree_app(g).await?.elements).unwrap_or_default();
            if values.is_empty() || super::input::typed_into(&values, text) {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                return Ok(Some(values.into_iter().find(|v| !v.is_empty()).unwrap_or_default()));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Pastes `text` and waits for it to show in a field (`old`: the fields' values before; None: a password field
    /// on screen). Returns whether it showed (None: can't be read) and whether the paste prompt was answered. The
    /// pasteboard is put back before returning
    async fn native_paste(
        &self,
        g: &mut Inner,
        text: &str,
        old: Option<&[String]>,
    ) -> crate::error::Result<(Option<bool>, bool)> {
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
        let deadline = Instant::now() + PASTE_WAIT;
        let mut prompt = false;
        let wanted = text.trim();
        let typed = loop {
            // the prompt brings SpringBoard to the front: the app in front alone tells both
            let tree = self.ax_tree_app(g).await?;
            if let Some((x, y)) = paste_allow(&tree) {
                self.native_tap(g, x, y).await?;
                prompt = true;
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
            let now = elements::field_values(&tree.elements);
            match (&now, old) {
                (Some(now), Some(old)) if now.as_slice() != old && now.iter().any(|v| v.contains(wanted)) => {
                    break Some(true);
                }
                (None, _) | (_, None) if Instant::now() >= deadline => break None,
                _ if Instant::now() >= deadline => break Some(false),
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        if let Some(p) = pending {
            let restored = match g.tunnel.as_mut() {
                Some(t) => Box::pin(t.restore_pasteboard(p)).await,
                None => ClipboardRestore::Failed("no tunnel".into()),
            };
            if let ClipboardRestore::Failed(e) = restored {
                tracing::warn!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "粘贴后还原剪贴板失败：{e}",
                        "Restoring the pasteboard after the paste failed: {e}"
                    )
                );
            }
        }
        Ok((typed, prompt))
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

/// The phone as [`ictl_unlock`] sees it, driven natively
struct NativeScreen<'a> {
    d: &'a Daemon,
    g: &'a mut Inner,
}

/// Reads the lock screen: with the HID session's screen stream open, the phone hides the lock screen and its
/// passcode keypad from accessibility (one element, the app's, for every session; iPhone 15 Pro Max, iOS 27.2.1), so
/// the session is closed first, and the elements come back a moment after. The next tap opens it again (about 0.5 s)
async fn read_lock_screen(d: &Daemon, g: &mut Inner) -> crate::error::Result<(String, ScreenSize, i64)> {
    let was_open = g.tunnel.as_ref().is_some_and(ictl_device::Tunnel::hid_open);
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
}
