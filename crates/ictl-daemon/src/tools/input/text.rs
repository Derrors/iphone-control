//! Typing, text replacement, HID clipboard paste fallback, and field readback verification.

use super::super::*;

impl Daemon {
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
pub(in crate::tools) fn replaced(text: &str, submit: bool, note: &str) -> String {
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
pub(in crate::tools) fn newly_typed(before: &str, now: &str, text: &str) -> bool {
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
pub(in crate::tools) fn typed_but_whitespace(before: &str, now: &str, text: &str) -> bool {
    let text = bare(text);
    !text.is_empty() && newly_typed(&bare(before), &bare(now), &text)
}

/// `s` without its whitespace
pub(in crate::tools) fn bare(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Repair only an observed append of the exact leading characters, stable across two fresh reads.
pub(in crate::tools) fn missing_suffix<'a>(before: &str, first: &str, second: &str, text: &'a str) -> Option<&'a str> {
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
pub(super) async fn readable_focus<T: ictl_wda::http::Transport>(w: &mut WdaClient<T>) -> Option<Focus> {
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

pub(super) async fn verify_typed<T: ictl_wda::http::Transport>(
    w: &mut WdaClient<T>,
    before: Option<&Focus>,
    text: &str,
) -> Typed {
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
