//! Native HID gestures, Home, text typing/replacing, and paste prompt handling.

use super::super::*;
use super::AX_WAIT;

/// After Cmd-V: how long for the text to show in the field, answering the paste prompt meanwhile
const PASTE_WAIT: Duration = Duration::from_secs(5);

/// How long a field emptied by [`Daemon::native_replace`] may take to read as empty
const CLEAR_WAIT: Duration = Duration::from_millis(1500);

/// After the paste prompt was answered, a field that already showed the text and doesn't change for this long took
/// the paste as it was: the pasted text replaced the same text, selected (Spotlight keeps the last search selected).
/// A paste shows in the field within about half a second of the answer
pub(super) const PASTE_SAME_SETTLE: Duration = Duration::from_millis(1500);
/// After Cmd-V, how long only SpringBoard is looked at for the paste prompt before the app is read too: an app pasting
/// into a web page waits for the prompt's answer, and reading it meanwhile times out and can leave remoteAXService
/// stuck for a while (Safari, iOS 27.2.1: the prompt came 1.2 s after Cmd-V; read at 1 s, the read timed out and
/// the next session's handshake took 16 s). iOS asks for every paste of ours, so this is rarely waited out
const PROMPT_GRACE: Duration = Duration::from_millis(2500);
/// After the paste prompt was answered, at least this long for the text to show in the field (the 5 s of
/// [`PASTE_WAIT`] may be mostly gone by then)
const PASTE_AFTER_ANSWER: Duration = Duration::from_millis(2500);
/// The paste prompt takes taps only about a second after it can be read (Safari, iOS 27.2.1: read 0.27 s after Cmd-V,
/// taps at 0.27 and 0.76 s did nothing, the one at 1.23 s answered it): tapped once it was seen this long ago. One that
/// is still there after a tap is tapped again
const PROMPT_SHOWN: Duration = Duration::from_millis(800);

/// Home is answered no sooner than this after it was pressed: what it closes on the home screen animates meanwhile
const HOME_ANIMATION: Duration = Duration::from_millis(600);

enum PasteTarget<'a> {
    Append(Option<(ictl_ax::ObjectRef, &'a str)>),
    Replace(ictl_ax::ObjectRef),
}

/// What a paste did, as far as the fields show
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pasted {
    /// A field changed and now holds the text
    Typed,
    /// The field shows the insertion with surrounding whitespace removed.
    Trimmed,
    /// A field held the text before and still does, unchanged: the paste replaced the same text, or didn't go in
    Unchanged,
    /// A password field is on screen: its value can't be read
    Unreadable,
    /// No field holds the text
    Missing,
}

/// What a look at the field after pasting means: done with this outcome, or look again (None). `answered`: how long
/// ago the paste prompt was answered; `late`: the time for pasting is up
pub(super) fn paste_outcome(seen: Pasted, answered: Option<Duration>, late: bool) -> Option<Pasted> {
    let settled = answered.is_some_and(|t| t >= PASTE_SAME_SETTLE);
    match seen {
        Pasted::Typed | Pasted::Trimmed => Some(seen),
        // only once the prompt was answered: before that, the paste hasn't gone in yet. A field that can't be read
        // won't tell more by waiting (it used to be looked at for all of the 5 s, the phone held meanwhile)
        Pasted::Unchanged | Pasted::Unreadable if settled => Some(seen),
        // never asked: iOS asks for every paste of ours, so nothing may have been pasted at all
        Pasted::Unchanged if late => Some(Pasted::Missing),
        _ if late => Some(seen),
        _ => None,
    }
}

/// Compare only the original focused field's raw value, preserving whitespace. Whitespace the field changed (a
/// single-line field turns a pasted newline into a space) makes it [`Pasted::Trimmed`], not missing
pub(super) fn paste_seen(now: Option<&str>, old: &str, wanted: &str) -> Pasted {
    let Some(now) = now else { return Pasted::Unreadable };
    let shown = input::bare(wanted);
    if input::newly_typed(old, now, wanted) {
        Pasted::Typed
    } else if input::typed_but_whitespace(old, now, wanted) {
        Pasted::Trimmed
    } else if now == old && !shown.is_empty() && input::bare(old).contains(&shown) {
        Pasted::Unchanged
    } else if !shown.is_empty() && input::bare(now).contains(&shown) && !now.starts_with(old) {
        Pasted::Unreadable // a selection may have been replaced
    } else {
        Pasted::Missing
    }
}

pub(super) fn append_seen(
    field: Option<&ictl_ax::Element>,
    object: ictl_ax::ObjectRef,
    old: &str,
    wanted: &str,
) -> Pasted {
    match field {
        Some(f) if f.object == Some(object) => paste_seen(Some(shown_text(f)), old, wanted),
        Some(_) => Pasted::Missing,
        None => Pasted::Unreadable,
    }
}

/// The on-screen keyboard is up: a Keyboard element, or its keys (the keyboard's process reads as the app's own
/// elements at times, then no Keyboard is made of them, `ictl_ax::node`)
fn keyboard_up(tree: &Tree) -> bool {
    tree.elements.iter().any(|e| e.kind == "Keyboard") || tree.elements.iter().filter(|e| e.kind == "Key").count() >= 8
}

/// The paste prompt's "Allow Paste" button ("“Notes” would like to paste from “dtpasteboardd”"), when it is up. Known
/// by its text in Chinese and English; in any other language by its place: the prompt's two buttons are Allow and
/// Don't Allow, Allow first (on the left side by side, on top stacked). The prompt is known by the process name in it
pub(super) fn paste_allow(tree: &Tree) -> Option<(f64, f64)> {
    let at = tree.elements.iter().position(|e| e.kind == "Alert")?;
    let alert = &tree.elements[at];
    if !alert.label.contains("dtpasteboardd") {
        return None;
    }
    let r = alert.visible;
    let inside = |e: &&elements::Element| {
        let (x, y) = (e.center.0 as f64, e.center.1 as f64);
        x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h
    };
    let buttons: Vec<_> = tree.elements[at + 1..].iter().filter(|e| e.kind == "Button").filter(inside).collect();
    let named = buttons.iter().find(|e| matches!(e.label.as_str(), "允许粘贴" | "Allow Paste" | "允許貼上"));
    let first = || (buttons.len() == 2).then(|| *buttons.iter().min_by_key(|e| (e.center.1, e.center.0)).unwrap());
    named.copied().or_else(first).map(|e| (e.center.0 as f64, e.center.1 as f64))
}

impl Daemon {
    /// Home, natively: not pressed under a system alert (it would do nothing), and confirmed by the home screen
    /// coming to the front; when the key does nothing, a swipe up from the bottom edge does it. The bool: the text
    /// reports a system alert ([`crate::protocol::Outcome::alert_reported`])
    pub(crate) async fn native_home(&self, g: &mut Inner, watched: bool) -> crate::error::Result<(String, bool)> {
        if !watched && let (_, Some(text)) = self.native_alert(g).await? {
            return Ok((
                ictl_l10n::trf!(
                    "{},按 Home 不会生效,没有按;{TAP_ALERT}",
                    "{}, pressing Home would have no effect, not pressed; {TAP_ALERT_EN}",
                    blocking(&text)
                ),
                true,
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
            return Ok((ictl_l10n::tr!("已按 home", "Pressed home").into(), false));
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
            let text = ictl_l10n::tr!(
                "已按 home(Home 键没反应,改为从屏幕底部上滑)",
                "Pressed home (the key did nothing, swiped up from the bottom edge instead)"
            );
            return Ok((text.into(), false));
        }
        if let (_, Some(text)) = self.native_alert(g).await? {
            return Ok((
                ictl_l10n::trf!(
                    "{},按 Home 没有生效,先点弹窗上的按钮",
                    "{}, pressing Home had no effect; first tap a button on the alert",
                    blocking(&text)
                ),
                true,
            ));
        }
        let app = self.native_foreground(g).await.unwrap_or_default();
        let text = ictl_l10n::trf!(
            "已按 home,但前台还是 {app},先截图看看",
            "Pressed home, but the foreground is still {app}; take a screenshot"
        );
        Ok((text, false))
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
        let rot = self.hid_touch_rotation(g).await.map_err(Error::preflight)?;
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
        let rot = self.hid_touch_rotation(g).await.map_err(Error::preflight)?;
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
    pub(crate) async fn native_type(
        &self,
        g: &mut Inner,
        text: &str,
        submit: bool,
        field_verified: &mut Option<bool>,
    ) -> crate::error::Result<String> {
        let n = text.chars().count();
        let before = self.ax_snapshot(g, AX_WAIT, false).await.map_err(Error::preflight)?;
        if !keyboard_up(&self.snapshot_tree(g, &before).await.map_err(Error::preflight)?) {
            return Err(Error::preflight(ictl_l10n::tr!(
                "屏幕上没有键盘(没有输入框获得焦点),什么都没输;先点一下输入框,截图确认键盘出来了再输入",
                "No keyboard on screen (no text field has focus), nothing typed; tap a text field first and check with a screenshot that the keyboard is up"
            )));
        }
        let old = focused_text_field(&before).and_then(|f| Some((f.object?, shown_text(f))));
        let mut note = String::new();
        if !text.is_empty() {
            let (typed, prompt) = self.native_paste(g, text, &before, PasteTarget::Append(old)).await?;
            if prompt {
                note += ictl_l10n::tr!("(已替你点了「允许粘贴」)", " (“Allow Paste” was tapped for you)");
            }
            match typed {
                Pasted::Typed => *field_verified = Some(true),
                Pasted::Trimmed | Pasted::Unreadable => {
                    note += ictl_l10n::tr!(
                        "(无法核对输入框内容，请查看屏幕确认)",
                        " (field contents could not be verified; inspect the screen)"
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
                    return Err(Error::readback(ictl_l10n::trf!(
                        "粘贴了 {n} 个字符,但 {} 秒内输入框里没有这段文字{};截图看看:输入框可能没有焦点,或 App 不接受粘贴",
                        "Pasted {n} characters, but the field didn't show the text within {} s{}; take a screenshot: the field may not have focus, or the app doesn't take pastes",
                        PASTE_WAIT.as_secs(),
                        if submit { ictl_l10n::tr!(",没有按回车", ", Return not pressed") } else { "" }
                    )));
                }
            }
        }
        if submit {
            self.hid_do(g, |h| Box::pin(async move { h.type_ascii("\n").await })).await.map_err(|e| {
                if text.is_empty() {
                    return e;
                }
                Error::action(
                    Delivery::Sent,
                    FailureStage::Send,
                    ictl_l10n::trf!("文字已输入{note},按回车失败:{e}", "Text typed{note}, pressing Return failed: {e}"),
                )
            })?;
        }
        Ok(ictl_l10n::trf!(
            "已输入 {n} 个字符{note}{}",
            "Typed {n} characters{note}{}",
            if submit { ictl_l10n::tr!(",并按了回车", " and pressed Return") } else { "" }
        ))
    }

    /// Replaces a single focused, readable field natively ([`Daemon::replace_text`]). Verifies the full value before
    /// sending Return
    pub(crate) async fn native_replace(
        &self,
        g: &mut Inner,
        text: &str,
        submit: bool,
    ) -> crate::error::Result<(String, Delivery)> {
        let before = self.ax_snapshot(g, AX_WAIT, false).await.map_err(Error::preflight)?;
        if !keyboard_up(&self.snapshot_tree(g, &before).await.map_err(Error::preflight)?) {
            return Err(Error::preflight(ictl_l10n::tr!(
                "屏幕上没有键盘，先点一下要替换的输入框",
                "No keyboard on screen; tap the field you want to replace first"
            )));
        }
        let (target, old) = focused_text_field(&before)
            .and_then(|f| Some((f.object?, shown_text(f))))
            .ok_or_else(|| {
                ictl_l10n::tr!(
                    "无法确认当前聚焦的是可读的普通输入框，没有替换文字",
                    "Couldn't identify a readable focused text field; no text was replaced"
                )
            })
            .map_err(Error::preflight)?;
        if old != text {
            let result = async {
                self.hid_do(g, |h| Box::pin(h.select_all())).await?;
                let seen = if text.is_empty() {
                    // emptying: Delete takes the selection, nothing is pasted (no paste prompt)
                    self.hid_do(g, |h| Box::pin(h.delete_backward())).await?;
                    self.native_field_emptied(g, target).await.map_err(Error::readback)?
                } else {
                    self.native_paste(g, text, &before, PasteTarget::Replace(target)).await?.0
                };
                if seen != Pasted::Typed {
                    return Err(Error::readback(ictl_l10n::tr!(
                        "替换后无法核对输入框全文，未按回车；请查看手机界面",
                        "Couldn't verify the full field value after the replacement; Return was not pressed"
                    )));
                }
                Ok(())
            }
            .await;
            if let Err(err) = result {
                if let Err(cause) = self.collapse_field_selection(g, target).await {
                    let message = ictl_l10n::trf!(
                        "{err}（无法收起原输入框的选区，请在重试前查看屏幕：{cause}）",
                        "{err} (couldn't collapse the original selection; inspect the screen before retrying: {cause})"
                    );
                    return Err(err.with_message(message));
                }
                return Err(err);
            }
        }
        if submit {
            self.hid_do(g, |h| Box::pin(h.type_ascii("\n")))
                .await
                .map_err(|e| if old == text { e } else { Error::action(Delivery::Sent, FailureStage::Send, e) })?;
        }
        let delivery = if old != text || submit { Delivery::Sent } else { Delivery::NotSent };
        Ok((input::replaced(text, submit, ""), delivery))
    }

    /// After select all and Delete: `target` reads as empty within [`CLEAR_WAIT`]
    async fn native_field_emptied(&self, g: &mut Inner, target: ictl_ax::ObjectRef) -> crate::error::Result<Pasted> {
        let deadline = Instant::now() + CLEAR_WAIT;
        loop {
            let screen = self.ax_snapshot(g, AX_WAIT, false).await?;
            let seen = replacement_seen(&screen, target, "");
            if seen == Pasted::Typed || Instant::now() >= deadline {
                return Ok(seen);
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
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
    /// `before`: the screen read before pasting (its app, which SpringBoard's elements are put over when looking for
    /// the prompt alone: [`Self::paste_prompt`])
    async fn native_paste(
        &self,
        g: &mut Inner,
        text: &str,
        before: &ictl_device::ax::AxScreen,
        target: PasteTarget<'_>,
    ) -> crate::error::Result<(Pasted, bool)> {
        // a paste through the WDA-down path may still wait to be put back
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
                    ictl_l10n::trf!("粘贴失败:{e}", "Paste failed: {e}"),
                ));
            }
        };
        g.dev.pending_paste = pending.map(|p| (Instant::now(), p));
        let result = async {
            let pasted = Instant::now();
            let mut deadline = pasted + PASTE_WAIT;
            // when the paste prompt was last answered, and when it was first seen
            let mut answered: Option<Instant> = None;
            let mut seen: Option<Instant> = None;
            // axauditd was restarted meanwhile: the elements have new references, the field is the focused one
            let mut renewed = false;
            let typed = loop {
                let late = Instant::now() >= deadline;
                // the prompt is SpringBoard's: looked for there first. Until it is answered, an app pasting into a web
                // page waits and doesn't answer reads (they timed out, and the prompt was never reached)
                let mut prompt = match self.paste_prompt(g, before).await {
                    Ok(p) => p,
                    Err(e) => {
                        if !self.unstick_ax_while_pasting(g, &mut renewed, &e).await && late {
                            return Err(Error::readback(e));
                        }
                        continue;
                    }
                };
                if prompt.is_none() && answered.is_none() && pasted.elapsed() < PROMPT_GRACE && !late {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    continue;
                }
                let mut screen = None;
                if prompt.is_none() {
                    match self.ax_snapshot_with(g, AX_WAIT, true, false).await {
                        Ok(s) => {
                            let tree = self.snapshot_tree(g, &s).await.map_err(Error::readback)?;
                            prompt = paste_allow(&tree);
                            screen = Some(s);
                        }
                        // still waiting for the prompt, or it is gone and the app catches up: look again
                        Err(e) => {
                            tracing::debug!("reading the field after pasting: {e}");
                            if !self.unstick_ax_while_pasting(g, &mut renewed, &e.to_string()).await && late {
                                return Err(Error::readback(e));
                            }
                            continue;
                        }
                    }
                }
                if prompt.is_some() && seen.get_or_insert_with(Instant::now).elapsed() < PROMPT_SHOWN && !late {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    continue;
                }
                if let Some((x, y)) = prompt {
                    tracing::debug!("paste prompt answered {:.2} s after pasting", pasted.elapsed().as_secs_f64());
                    self.native_tap(g, x, y).await.map_err(|e| Error::action(Delivery::Sent, FailureStage::Send, e))?;
                    answered = Some(Instant::now());
                    // pushed back once at most: a prompt that comes back each time it is answered can't hold it forever
                    let latest = pasted + PASTE_WAIT + PASTE_AFTER_ANSWER;
                    deadline = deadline.max((Instant::now() + PASTE_AFTER_ANSWER).min(latest));
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if Instant::now() >= deadline {
                        break Pasted::Missing;
                    }
                    continue;
                }
                let Some(screen) = screen else { continue };
                let focused = focused_text_field(&screen);
                let seen = match target {
                    PasteTarget::Append(Some((_, old))) if renewed => paste_seen(focused.map(shown_text), old, text),
                    PasteTarget::Append(Some((object, old))) => append_seen(focused, object, old, text),
                    PasteTarget::Append(None) => Pasted::Unreadable,
                    PasteTarget::Replace(_) if renewed => {
                        if focused.is_some_and(|f| shown_text(f) == text) {
                            Pasted::Typed
                        } else {
                            Pasted::Missing
                        }
                    }
                    PasteTarget::Replace(object) => replacement_seen(&screen, object, text),
                };
                if let Some(done) = paste_outcome(seen, answered.map(|t| t.elapsed()), Instant::now() >= deadline) {
                    break done;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            };
            Ok((typed, answered.is_some()))
        }
        .await;
        self.restore_pasteboard(g, false).await;
        result
    }

    /// The paste prompt's "Allow Paste" ([`paste_allow`]) without reading the app pasted into, which may be stuck
    /// waiting for the answer (a web page). Another app in front (Safari: the prompt puts SpringBoard there) is read
    /// as the app, the only way SpringBoard's question reads as an alert; with the same app in front, SpringBoard's
    /// elements over it, put over `before`'s (Notes: the prompt is one of them). `Err`: remoteAXService didn't answer
    async fn paste_prompt(
        &self,
        g: &mut Inner,
        before: &ictl_device::ax::AxScreen,
    ) -> Result<Option<(f64, f64)>, String> {
        let Some(t) = g.tunnel.as_mut() else { return Ok(None) };
        let s = match t.ax_frontmost_pid(Duration::from_secs(1)).await {
            Ok(pid) if pid != before.app_pid => {
                self.ax_snapshot_with(g, AX_WAIT, false, false).await.map_err(|e| e.to_string())?
            }
            Ok(_) => {
                let mut s = before.clone();
                (s.overlays, s.overlays_complete) = t.ax_overlays(before.app_pid).await;
                s
            }
            Err(e @ (DeviceError::AxUnanswered(_) | DeviceError::Timeout(_))) => {
                t.close_ax().await;
                return Err(e.to_string());
            }
            Err(e) => {
                tracing::debug!("paste prompt: the app in front: {e}");
                return Ok(None);
            }
        };
        Ok(self.snapshot_tree(g, &s).await.ok().as_ref().and_then(paste_allow))
    }

    /// remoteAXService stopped answering after a paste (Safari, iOS 27.2.1, the first paste after the phone sat idle
    /// for minutes: from Cmd-V on, not even the app in front is answered, so the prompt can't be found and the paste
    /// never goes in). axauditd is restarted once ([`Self::restart_stuck_ax`]), after which the prompt reads at once;
    /// its elements get new references, so the field is then known as the focused one (`renewed`). Whether it was
    /// restarted now
    async fn unstick_ax_while_pasting(&self, g: &mut Inner, renewed: &mut bool, why: &str) -> bool {
        tracing::debug!("reading the screen after pasting: {why}");
        if *renewed {
            return false;
        }
        let Some(device) = g.device.clone() else { return false };
        *renewed = Box::pin(self.restart_stuck_ax(g, &device, why)).await;
        *renewed
    }
}

pub(super) fn focused_text_field(screen: &ictl_device::ax::AxScreen) -> Option<&ictl_ax::Element> {
    let mut fields = screen.app.iter().filter(|e| {
        // Raw AX retains focus and exact text, including whitespace. The element tree has been normalized for display.
        e.focused && matches!(ictl_ax::node::kind(e), "TextField" | "TextView" | "SearchField" | "SecureTextField")
    });
    let f = fields.next()?;
    (fields.next().is_none() && f.enabled && f.visible && ictl_ax::node::kind(f) != "SecureTextField").then_some(f)
}

/// What a field holds: an empty field reads its placeholder as its value natively (Spotlight's empty field reads
/// «搜索»), so that counts as empty. A field holding exactly its placeholder's words reads as empty too: rare, and
/// replacing it with other text still works (only emptying it is then taken as done already)
fn shown_text(f: &ictl_ax::Element) -> &str {
    if !f.placeholder.is_empty() && f.value == f.placeholder { "" } else { &f.value }
}

pub(super) fn replacement_seen(screen: &ictl_device::ax::AxScreen, target: ictl_ax::ObjectRef, text: &str) -> Pasted {
    if focused_text_field(screen).is_some_and(|f| f.object == Some(target) && shown_text(f) == text) {
        Pasted::Typed
    } else {
        Pasted::Missing
    }
}
