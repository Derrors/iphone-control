//! 锁屏：密码键盘的键位、密码栏、「使用密码」按钮、是哪种封面界面

use super::*;

/// 锁屏密码键盘上 0～9 各键的中心点；没找齐 10 个返回 None。
/// 找到了也不代表键盘在屏幕上——按键后要核对密码栏的已输入位数。
/// 键位不像电话键盘时也返回 None（见 [`keypad_layout_ok`]）。
pub fn keypad(elements: &[Element]) -> Option<[(i64, i64); 10]> {
    let mut keys: [Option<(i64, i64)>; 10] = [None; 10];
    for e in elements.iter().filter(|e| e.kind == "Button" || e.kind == "Key") {
        for t in [e.label.trim(), e.name.trim()] {
            let mut chars = t.chars();
            let (Some(first), second) = (chars.next(), chars.next()) else { continue };
            // 键的标签可能是 "2" 或 "2 ABC"；"10"、"3条通知" 这类锁屏上别的按钮不算
            if let Some(d) = first.to_digit(10)
                && second.is_none_or(char::is_whitespace)
            {
                keys[d as usize].get_or_insert(e.center);
                break;
            }
        }
    }
    let mut out = [(0, 0); 10];
    for (i, k) in keys.into_iter().enumerate() {
        out[i] = k?;
    }
    if !keypad_layout_ok(&out) {
        tracing::warn!(
            "{}",
            ictl_l10n::log_trf!(
                "锁屏数字键的位置不像电话键盘（坐标可能没换算对），不按：{out:?}",
                "The lock screen keypad doesn't look like a phone keypad (coordinates may be converted wrongly), not pressing: {out:?}"
            )
        );
        return None;
    }
    Some(out)
}

/// 键位像电话键盘：1 2 3、4 5 6、7 8 9 三行各自同高、从左到右、自上而下，三列上下对齐，0 在 8 正下方。
/// 坐标换算错了（例如横屏时没把竖屏坐标转过来，1 2 3 会竖着排）就过不了：宁可调不出键盘，也不能按错键
fn keypad_layout_ok(k: &[(i64, i64); 10]) -> bool {
    const TOL: i64 = 10;
    let near = |a: i64, b: i64| (a - b).abs() <= TOL;
    let rows = [[1, 2, 3], [4, 5, 6], [7, 8, 9]];
    let rows_ok = rows.iter().all(|r| {
        near(k[r[0]].1, k[r[1]].1) && near(k[r[0]].1, k[r[2]].1) && k[r[0]].0 < k[r[1]].0 && k[r[1]].0 < k[r[2]].0
    });
    let cols_ok = (1..=3).all(|c| near(k[c].0, k[c + 3].0) && near(k[c].0, k[c + 6].0));
    rows_ok && cols_ok && k[1].1 < k[4].1 && k[4].1 < k[7].1 && near(k[0].0, k[8].0) && k[0].1 > k[8].1
}

/// 密码键盘右下角的按钮：有输入时是「删除」（每按一下删一位），没输入时是「取消」。
/// 标签随语言变，按位置认：和 9 同一列、在 9 下面的 Button（iPhone 上在 0 的右下，iPad 上和 0 同一行）
pub fn delete_key(elements: &[Element], pad: &[(i64, i64); 10]) -> Option<(i64, i64)> {
    let (col, row9) = (pad[9].0, pad[9].1);
    elements
        .iter()
        .filter(|e| e.kind == "Button" && (e.center.0 - col).abs() <= 10 && e.center.1 > row9 + 50)
        .map(|e| e.center)
        .next()
}

/// 有 Touch ID 的机型（iPad mini 5，iPadOS 26.6.1 真机）上滑后先是「触控 ID」提示，只有「使用密码」和「取消」两个按钮，
/// 树里没有数字键和密码栏，点「使用密码」才出键盘。返回它的中心。英文标签没在真机上核对过。
/// 别的语言认不出文字时按结构找：整棵树只有这两个按钮、上下叠在同一条竖线上，没有锁屏本身的标识（「按下主屏幕按钮
/// 以解锁」也是个按钮）和密码栏，「使用密码」在上（三个真机样本都是）。万一点成了「取消」，只是回到锁屏，不算输错密码
pub fn use_passcode_button(elements: &[Element]) -> Option<(i64, i64)> {
    let by_label = elements.iter().find(|e| {
        e.kind == "Button"
            && (USE_PASSCODE_LABELS.contains(&e.label.as_str()) || USE_PASSCODE_LABELS.contains(&e.name.as_str()))
    });
    if let Some(e) = by_label {
        return Some(e.center);
    }
    let buttons: Vec<&Element> = elements.iter().filter(|e| e.kind == "Button").collect();
    let stacked = |a: &Element, b: &Element| (a.center.0 - b.center.0).abs() <= 20 && a.center.1 != b.center.1;
    // the lock screen itself (its "press Home to unlock" is a button too, with a notification above it)
    let lock_screen = elements
        .iter()
        .any(|e| ["call-to-action-label", "proud-lock", "lockscreen-date-view"].contains(&e.name.as_str()));
    // the upper one: Use Passcode is above Cancel in every sample, whatever order the tree lists them in
    (buttons.len() == 2 && stacked(buttons[0], buttons[1]) && !lock_screen && passcode_field(elements).is_none())
        .then(|| buttons.iter().map(|b| b.center).min_by_key(|c| c.1))
        .flatten()
}

/// 「触控 ID」提示的结构，和界面语言无关：SpringBoard 的 `SBCoverSheetWindow` 里没有锁屏日期（不是锁屏本身，
/// 也不是通知中心），只有两个按钮。按钮上的字只认得中文和英文（[`USE_PASSCODE_LABELS`]），繁中、日、德、法……
/// 的系统上靠这个认出锁屏：认不出时把锁着的 iPad 当成了解锁的，之后不再查锁屏
pub(super) fn touch_id_prompt(xml: &str) -> bool {
    let Some(at) = xml.find("name=\"SBCoverSheetWindow\"") else { return false };
    let start = xml[..at].rfind('<').unwrap_or(0);
    // the window itself: an element of another kind with that name would be matched to a window's end tag
    if !xml[start..].starts_with("<XCUIElementTypeWindow ") {
        return false;
    }
    let Some(open_end) = xml[at..].find('>').map(|i| at + i) else { return false };
    if xml[..open_end].ends_with('/') {
        return false;
    }
    let Some(len) = xml[open_end..].find("</XCUIElementTypeWindow>") else { return false };
    let window = &xml[start..open_end + len];
    !window.contains("\"lockscreen-date-view\"") && window.matches("<XCUIElementTypeButton ").count() == 2
}

/// 「触控 ID」提示上「使用密码」按钮的标签（英文的没在真机上核对过）
const USE_PASSCODE_LABELS: [&str; 3] = ["使用密码", "Use Passcode", "Enter Passcode"];

/// 元素树里锁屏密码栏的 (总位数, 已输入位数)
pub fn passcode_field(elements: &[Element]) -> Option<(u32, u32)> {
    elements.iter().filter(|e| e.kind == "SecureTextField").find_map(|e| parse_passcode_value(&e.value))
}

/// 锁屏密码栏的值形如「共6位数，已输入2位」或英文「0 of 6 values entered」，返回 (总位数, 已输入位数)。
/// 中文先说总位数，英、法、德、西语先说已输入几位（#174）；已输入位数不会超过总位数，所以大的是总位数，与语言无关
pub fn parse_passcode_value(value: &str) -> Option<(u32, u32)> {
    static DIGITS: LazyLock<Regex> = LazyLock::new(|| regex(r"\d+"));
    let nums: Vec<u32> = DIGITS.find_iter(value).filter_map(|m| m.as_str().parse().ok()).collect();
    match nums[..] {
        [a, b] if a.max(b) > 0 => Some((a.max(b), a.min(b))),
        _ => None,
    }
}

/// SpringBoard 元素树看到的是哪种「封面」界面。用来纠正 `/wda/locked`（见 [`crate::WdaClient::locked`]）。
/// 锁屏的标识（iOS 27 iPhone 和 iPadOS 26.6.1 iPad mini 5 真机样本）：
/// - Face ID 机型锁屏有锁图标 `proud-lock`（灵动岛里还有 `systemApertureElementIdentifierLock`），上滑后的密码键盘有 `passcode-view`
/// - Touch ID 机型没有锁图标，锁屏有 `call-to-action-label`（「按下主屏幕按钮以解锁」）；上滑后的「触控 ID」提示和数字键盘
///   都没有与语言无关的标识，只能认「使用密码」按钮和锁屏密码栏（值是「共6位数，已输入0位」这样的）
/// - 通知中心和锁屏是同一个界面（都有 `lockscreen-date-view`），但没有上面这些
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverSheet {
    Locked,
    NotificationCenter,
    None,
}

pub fn cover_sheet(xml: &str) -> CoverSheet {
    let lock_glyph =
        ["\"proud-lock\"", "systemApertureElementIdentifierLock", "\"passcode-view\"", "\"call-to-action-label\""]
            .iter()
            .any(|m| xml.contains(m));
    let use_passcode = USE_PASSCODE_LABELS.iter().any(|l| xml.contains(&format!("label=\"{l}\"")));
    static PASSCODE: LazyLock<Regex> =
        LazyLock::new(|| regex(r#"<XCUIElementTypeSecureTextField [^>]*value="([^"]*)""#));
    let passcode_field = PASSCODE.captures_iter(xml).any(|c| parse_passcode_value(&c[1]).is_some());
    if lock_glyph || use_passcode || passcode_field || touch_id_prompt(xml) {
        CoverSheet::Locked
    } else if xml.contains("\"lockscreen-date-view\"") {
        CoverSheet::NotificationCenter
    } else {
        CoverSheet::None
    }
}
