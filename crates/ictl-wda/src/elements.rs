//! 元素树解析（纯函数）。行为与 toolkit 的 `src/iphone_mcp/elements.py` 一致：
//!
//! - 只保留有 label/name/value 的元素
//! - 按坐标判断是否在屏幕内（请求时排除了 WDA 的可见性计算：完整树 7～8 秒，排除后约 0.3 秒）
//! - 铺满全屏的 Application/Window 之类去掉；父子重复（同文字同位置）去重
//! - 点击中心取元素在屏幕内的部分
//! - 锁屏密码键盘的 `Key` 元素不显示时也在树里，**不能**据此判断键盘已出现

use ictl_device::ScreenSize;
use quick_xml::{XmlVersion, events::Event};

/// `/source?format=xml&excluded_attributes=` 要排除的属性
pub const SOURCE_EXCLUDED: &str =
    "visible,accessible,focused,enabled,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";

#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub kind: String,
    pub label: String,
    pub name: String,
    pub value: String,
    pub center: (i64, i64),
    pub size: (i64, i64),
}

impl Element {
    fn haystack(&self) -> String {
        format!("{} {} {}", self.label, self.name, self.value).to_lowercase()
    }

    /// 与 toolkit 相同的一行描述：`Button "显示与亮度" id="..." @(215,676) 390x53`
    pub fn describe(&self) -> String {
        let text = [&self.label, &self.name, &self.value].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
        let mut s = format!("{} \"{}\"", self.kind, text);
        if !self.name.is_empty() && !self.label.is_empty() && self.name != self.label {
            s += &format!(" id=\"{}\"", self.name);
        }
        if !self.value.is_empty() && self.value != self.label && self.value != self.name {
            let v: String = self.value.chars().take(60).collect();
            s += &format!(" value=\"{v}\"");
        }
        s + &format!(" @({},{}) {}x{}", self.center.0, self.center.1, self.size.0, self.size.1)
    }
}

/// Python `round()` 是银行家舍入；坐标这里差半点无所谓，但为了与 toolkit 输出逐字一致，照它来
fn py_round(v: f64) -> i64 {
    let r = v.round();
    if (v - v.trunc()).abs() == 0.5 && (r as i64) % 2 != 0 { (r - v.signum()) as i64 } else { r as i64 }
}

pub fn parse(xml: &str, screen: ScreenSize) -> Vec<Element> {
    let (sw, sh) = (screen.width, screen.height);
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    loop {
        let e = match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => e,
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => continue,
        };
        let tag = e.name().as_ref().to_owned();
        let mut attr = std::collections::HashMap::new();
        for a in e.attributes().flatten() {
            let k = a.key.as_ref().to_owned();
            let v = a.normalized_value(XmlVersion::Implicit1_0).map(|c| c.into_owned()).unwrap_or_default();
            attr.insert(k, v);
        }
        let get = |k: &str| attr.get(k).map(|s| s.trim().to_owned()).unwrap_or_default();
        let (label, name, value) = (get("label"), get("name"), get("value"));
        let text = [&label, &name, &value].into_iter().find(|s| !s.is_empty()).cloned();
        let Some(text) = text else { continue };
        let num = |k: &str| attr.get(k).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let (x, y, w, h) = (num("x"), num("y"), num("width"), num("height"));
        if w <= 0.0 || h <= 0.0 || x >= sw || y >= sh || x + w <= 0.0 || y + h <= 0.0 {
            continue;
        }
        if w >= sw && h >= sh {
            continue;
        }
        if !seen.insert((text, py_round(x), py_round(y), py_round(w), py_round(h))) {
            continue;
        }
        let (vx1, vy1, vx2, vy2) = (x.max(0.0), y.max(0.0), (x + w).min(sw), (y + h).min(sh));
        let kind = attr.get("type").cloned().unwrap_or(tag);
        out.push(Element {
            kind: kind.strip_prefix("XCUIElementType").unwrap_or(&kind).to_owned(),
            label,
            name,
            value,
            center: (py_round((vx1 + vx2) / 2.0), py_round((vy1 + vy2) / 2.0)),
            size: (py_round(w), py_round(h)),
        });
    }
    out
}

/// 标签/名字/值里包含 `text` 的元素（不区分大小写）
pub fn filter<'a>(elements: &'a [Element], text: &str) -> Vec<&'a Element> {
    let t = text.to_lowercase();
    elements.iter().filter(|e| e.haystack().contains(&t)).collect()
}

/// 按文字找元素：先找标签或名字完全相同的，没有再找包含该文字的
pub fn find<'a>(elements: &'a [Element], text: &str) -> Vec<&'a Element> {
    let exact: Vec<_> = elements.iter().filter(|e| e.label == text || e.name == text).collect();
    if exact.is_empty() { filter(elements, text) } else { exact }
}

/// 锁屏密码键盘上 0～9 各键的中心点；没找齐 10 个返回 None。
/// 找到了也不代表键盘在屏幕上——按键后要核对密码栏的已输入位数。
pub fn keypad(elements: &[Element]) -> Option<[(i64, i64); 10]> {
    let mut keys: [Option<(i64, i64)>; 10] = [None; 10];
    for e in elements.iter().filter(|e| e.kind == "Button" || e.kind == "Key") {
        for t in [e.label.trim(), e.name.trim()] {
            let mut chars = t.chars();
            let (Some(first), second) = (chars.next(), chars.next()) else { continue };
            // 键的标签可能是 "2" 或 "2 ABC"，但不能是 "10"
            if let Some(d) = first.to_digit(10)
                && !second.is_some_and(|c| c.is_ascii_digit())
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
    Some(out)
}

/// 锁屏密码栏的值形如「共6位数，已输入2位」或英文「6-digit passcode, 2 entered」，返回 (总位数, 已输入位数)
pub fn parse_passcode_value(value: &str) -> Option<(u32, u32)> {
    let nums: Vec<u32> = regex::Regex::new(r"\d+").ok()?.find_iter(value).filter_map(|m| m.as_str().parse().ok()).collect();
    match nums[..] {
        [total, entered] => Some((total, entered)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: ScreenSize = ScreenSize { width: 430.0, height: 932.0 };
    const HOME: &str = include_str!("../tests/fixtures/home.xml");
    const LOCK: &str = include_str!("../tests/fixtures/lockscreen.xml");

    fn labels(els: &[Element]) -> Vec<String> {
        els.iter().map(|e| if e.label.is_empty() { e.name.clone() } else { e.label.clone() }).collect()
    }

    fn home() -> Vec<Element> {
        parse(HOME, SCREEN)
    }

    #[test]
    fn full_screen_containers_are_dropped() {
        let h = home();
        assert!(!labels(&h).contains(&"Home screen icons".to_owned()));
        assert!(h.iter().all(|e| e.kind != "Application" && e.kind != "Window"));
    }

    #[test]
    fn child_repeating_parent_label_and_rect_is_deduped() {
        let h = home();
        let l = labels(&h);
        assert_eq!(l.iter().filter(|s| *s == "设置").count(), 1);
        assert_eq!(h[l.iter().position(|s| s == "设置").unwrap()].kind, "Icon");
    }

    #[test]
    fn offscreen_page_and_degenerate_elements_are_dropped() {
        let l = labels(&home());
        assert!(!l.contains(&"第二页的 App".to_owned()));
        assert!(!l.contains(&"零宽".to_owned()));
    }

    #[test]
    fn unlabeled_elements_are_dropped() {
        assert!(home().iter().all(|e| !(e.label.is_empty() && e.name.is_empty() && e.value.is_empty())));
    }

    #[test]
    fn center_is_clipped_to_visible_part() {
        let h = home();
        assert_eq!(h.iter().find(|e| e.label == "被顶出屏幕一半").unwrap().center, (150, 10));
        assert_eq!(h.iter().find(|e| e.label.starts_with("垂直滚动条")).unwrap().center.0, 421);
    }

    #[test]
    fn type_prefix_is_removed() {
        let kinds: Vec<_> = home().iter().map(|e| e.kind.clone()).collect();
        for k in ["Icon", "Button", "StaticText"] {
            assert!(kinds.contains(&k.to_owned()));
        }
    }

    #[test]
    fn describe_matches_toolkit_format() {
        let h = home();
        let d = |l: &str| h.iter().find(|e| e.label == l).unwrap().describe();
        assert_eq!(d("显示与亮度"), r#"Button "显示与亮度" id="com.apple.settings.displayAndBrightness" @(215,676) 390x53"#);
        assert!(d("设置").contains(r#"value="1个新项目""#));
    }

    #[test]
    fn find_prefers_exact_over_substring() {
        let h = home();
        let names = |v: Vec<&Element>| v.into_iter().map(|e| e.label.clone()).collect::<Vec<_>>();
        assert_eq!(names(find(&h, "时钟")), ["时钟"]);
        assert_eq!(names(find(&h, "亮度")), ["显示与亮度"]);
        assert!(find(&h, "不存在").is_empty());
        assert_eq!(names(find(&h, "com.apple.settings.displayAndBrightness")), ["显示与亮度"]);
    }

    #[test]
    fn filter_is_case_insensitive_and_checks_value() {
        let h = home();
        assert_eq!(filter(&h, "新项目").iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["设置"]);
    }

    fn key(label: &str, x: i64, kind: &str) -> Element {
        Element { kind: kind.into(), label: label.into(), name: label.into(), value: String::new(), center: (x, 100), size: (80, 80) }
    }

    #[test]
    fn keypad_needs_all_ten_digits() {
        let keys: Vec<_> = (0..10).map(|d| key(&d.to_string(), d * 10, "Key")).collect();
        let pad = keypad(&keys).unwrap();
        assert_eq!(pad[0], (0, 100));
        assert_eq!(pad[9], (90, 100));
        assert!(keypad(&keys[..9]).is_none());
    }

    #[test]
    fn keypad_accepts_letters_after_digit_but_not_multi_digit_labels() {
        let keys: Vec<_> = (0..10).map(|d| key(&format!("{d} ABC"), d, "Button")).collect();
        assert!(keypad(&keys).is_some());
        let mut keys: Vec<_> = (0..9).map(|d| key(&d.to_string(), d, "Key")).collect();
        keys.push(key("10", 0, "Key"));
        assert!(keypad(&keys).is_none());
    }

    #[test]
    fn keypad_ignores_non_key_types() {
        let keys: Vec<_> = (0..10).map(|d| key(&d.to_string(), d, "StaticText")).collect();
        assert!(keypad(&keys).is_none());
    }

    /// 回归用例 #1：键盘收起时 Key 元素仍在树里。只能证明"树里有"，解锁逻辑不能拿它判断键盘已显示
    #[test]
    fn hidden_keypad_still_present_in_tree() {
        let els = parse(LOCK, SCREEN);
        let pad = keypad(&els).expect("隐藏的键盘仍能解析出 10 个键");
        assert_eq!(pad[1], (103, 371));
        assert_eq!(pad[0], (215, 695));
        let field = els.iter().find(|e| e.kind == "SecureTextField").unwrap();
        assert_eq!(parse_passcode_value(&field.value), Some((6, 0)));
    }

    #[test]
    fn passcode_value_variants() {
        assert_eq!(parse_passcode_value("共6位数，已输入0位"), Some((6, 0)));
        assert_eq!(parse_passcode_value("共6位数，已输入2位"), Some((6, 2)));
        assert_eq!(parse_passcode_value("6-digit passcode, 3 entered"), Some((6, 3)));
        assert_eq!(parse_passcode_value("共4位数"), None);
        assert_eq!(parse_passcode_value(""), None);
    }
}
