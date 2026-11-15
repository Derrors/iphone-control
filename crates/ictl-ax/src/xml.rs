//! An accessibility tree as the XML WebDriverAgent's `/source` gives (`XCUIElementType…` tags with `type`,
//! `label`, `name`, `value`, `x`, `y`, `width`, `height`), so `ictl_wda::elements::parse_tree` reads it unchanged:
//! finding by text, what covers what, the keyboard, alerts, the lock screen and the passcode pad.
//!
//! Elements of other processes (a keyboard, an extension, a remote view) come with frames relative to the view
//! hosting them, which carries [`trait_bit::REMOTE_HOST`] and comes right before them; they are moved to screen
//! points by that host. Those without a host (home screen widgets) can't be placed and are left out. A process
//! whose elements are mostly single characters is the on-screen keyboard: it becomes one `Keyboard` element.
//! The same rules as Pinky (DIYgod/pinky `observe_rax.go`, Apache-2.0).

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use crate::tree::{Element, trait_bit};

/// x, y, width, height in screen points
type Frame = (f64, f64, f64, f64);

/// XCUIElementType names, by the enum's value (Apple's public order)
pub const TYPES: [&str; 83] = [
    "Any",
    "Other",
    "Application",
    "Group",
    "Window",
    "Sheet",
    "Drawer",
    "Alert",
    "Dialog",
    "Button",
    "RadioButton",
    "RadioGroup",
    "CheckBox",
    "DisclosureTriangle",
    "PopUpButton",
    "ComboBox",
    "MenuButton",
    "ToolbarButton",
    "Popover",
    "Keyboard",
    "Key",
    "NavigationBar",
    "TabBar",
    "TabGroup",
    "Toolbar",
    "StatusBar",
    "Table",
    "TableRow",
    "TableColumn",
    "Outline",
    "OutlineRow",
    "Browser",
    "CollectionView",
    "Slider",
    "PageIndicator",
    "ProgressIndicator",
    "ActivityIndicator",
    "SegmentedControl",
    "Picker",
    "PickerWheel",
    "Switch",
    "Toggle",
    "Link",
    "Image",
    "Icon",
    "SearchField",
    "ScrollView",
    "ScrollBar",
    "StaticText",
    "TextField",
    "SecureTextField",
    "DatePicker",
    "TextView",
    "Menu",
    "MenuItem",
    "MenuBar",
    "MenuBarItem",
    "Map",
    "WebView",
    "IncrementArrow",
    "DecrementArrow",
    "Timeline",
    "RatingIndicator",
    "ValueIndicator",
    "SplitGroup",
    "Splitter",
    "RelevanceIndicator",
    "ColorWell",
    "HelpTag",
    "Matte",
    "DockItem",
    "Ruler",
    "RulerMarker",
    "Grid",
    "LevelIndicator",
    "Cell",
    "LayoutArea",
    "LayoutItem",
    "Handle",
    "Stepper",
    "Tab",
    "TouchBar",
    "StatusItem",
];

/// The element's type name: the XCUIElementType, or, in a first pushed tree that lacks it, a guess from the traits
pub fn kind(e: &Element) -> &'static str {
    let k = typed_kind(e);
    // the lock screen's passcode field reads as a plain text field here (typed or not); WDA calls it a secure one,
    // which is what finds it when unlocking (`ictl_wda::elements::passcode_field`, `cover_sheet`)
    if k == "TextField" && passcode_value(&e.value) { "SecureTextField" } else { k }
}

fn typed_kind(e: &Element) -> &'static str {
    if let Some(t) = usize::try_from(e.xc_type).ok().filter(|t| (1..TYPES.len()).contains(t)) {
        return TYPES[t];
    }
    let t = e.traits;
    let has = |bit| t & bit != 0;
    if has(trait_bit::KEYBOARD_KEY) {
        "Key"
    } else if has(trait_bit::SEARCH_FIELD) {
        "SearchField"
    } else if has(trait_bit::TEXT_FIELD) {
        "TextField"
    } else if has(trait_bit::SWITCH) {
        "Switch"
    } else if has(trait_bit::ADJUSTABLE) {
        "Slider"
    } else if has(trait_bit::LINK) {
        "Link"
    } else if has(trait_bit::BUTTON) {
        "Button"
    } else if has(trait_bit::IMAGE) {
        "Image"
    } else if has(trait_bit::STATIC_TEXT) {
        "StaticText"
    } else {
        "Other"
    }
}

/// A passcode field's value: 「共6位数，已输入2位」, "6-digit passcode, 2 entered" (two numbers, and the words for it)
fn passcode_value(v: &str) -> bool {
    let numbers = v.split(|c: char| !c.is_ascii_digit()).filter(|n| !n.is_empty()).count();
    let lower = v.to_lowercase();
    numbers == 2 && (v.contains("位") || lower.contains("passcode") || lower.contains("digit"))
}

/// At least this many one-character elements make a process the keyboard
const KEYBOARD_KEYS: usize = 8;

fn is_key_char(label: &str) -> bool {
    let mut c = label.chars();
    c.next().is_some() && c.next().is_none()
}

/// Where each element goes, in screen points; None = left out
fn place(els: &[Element], keyboard: &HashSet<i64>, web: &HashSet<i64>) -> Vec<Option<Frame>> {
    let app_pid = els.first().map_or(0, Element::pid);
    let mut offset: HashMap<i64, (f64, f64)> = HashMap::new();
    let mut host: Option<Frame> = None;
    let mut out = Vec::with_capacity(els.len());
    for (i, e) in els.iter().enumerate() {
        let parent_out = e.parent.map(|p| out.get(p).copied().flatten().is_some());
        if parent_out == Some(false) {
            // under something left out
            out.push(None);
            continue;
        }
        if e.traits & trait_bit::REMOTE_HOST != 0 && e.frame.is_some() {
            host = e.frame;
        } else if e.pid() == app_pid {
            host = None;
        }
        let Some((x, y, w, h)) = e.frame else {
            out.push(Some((0.0, 0.0, 0.0, 0.0)));
            continue;
        };
        if i == 0 || e.pid() == app_pid {
            out.push(Some((x, y, w, h)));
            continue;
        }
        if let Some(hf) = host
            && !offset.contains_key(&e.pid())
            && w <= hf.2 + 1.0
            && h <= hf.3 + 1.0
        {
            offset.insert(e.pid(), (hf.0, hf.1));
        }
        match offset.get(&e.pid()) {
            Some((dx, dy)) => out.push(Some((x + dx, y + dy, w, h))),
            // the keyboard is kept (it marks the keyboard as shown) even when its keys can't be placed
            None if keyboard.contains(&e.pid()) => out.push(Some((0.0, 0.0, 0.0, 0.0))),
            // a page (WebKit's web content process) is listed without a host, in screen points (Pinky
            // `webcontent.go`)
            None if web.contains(&e.pid()) => out.push(Some((x, y, w, h))),
            None => out.push(None),
        }
    }
    out
}

fn esc(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
}

fn open_tag(out: &mut String, kind: &str, e: &Element, (x, y, w, h): Frame, close: bool) {
    let _ = write!(out, "<XCUIElementType{kind} type=\"XCUIElementType{kind}\"");
    let mut field = |name: &str, v: &str| {
        if !v.is_empty() {
            let _ = write!(out, " {name}=\"");
            esc(v, out);
            out.push('"');
        }
    };
    field("label", &e.label);
    field("name", if e.identifier.is_empty() { &e.label } else { &e.identifier });
    field("value", if e.value.is_empty() { &e.placeholder } else { &e.value });
    field("placeholderValue", &e.placeholder);
    let _ = write!(
        out,
        " enabled=\"{}\" selected=\"{}\" focused=\"{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"{}>",
        e.enabled,
        e.selected,
        e.focused,
        x.round() as i64,
        y.round() as i64,
        w.round() as i64,
        h.round() as i64,
        if close { "/" } else { "" }
    );
}

/// The XML for `app` (the frontmost application's tree, its element first) with `overlays` (what SpringBoard
/// draws over it: alerts, banners; in screen points) after it, on a `width`×`height` screen. `web`: the processes
/// that are WebKit's web content (a page's elements), placed as they come. `springboard_front`: SpringBoard is the
/// app in front, as when it asks something over an app (see [`looks_like_dialog`])
pub fn to_xml(
    app: &[Element],
    overlays: &[Element],
    width: f64,
    height: f64,
    web: &HashSet<i64>,
    springboard_front: bool,
) -> String {
    let app_pid = app.first().map_or(0, Element::pid);
    let mut keys: HashMap<i64, usize> = HashMap::new();
    // a page's own one-letter links are no keys
    for e in app.iter().filter(|e| e.pid() != app_pid && !web.contains(&e.pid()) && is_key_char(&e.label)) {
        *keys.entry(e.pid()).or_default() += 1;
    }
    let keyboard: HashSet<i64> = keys.into_iter().filter(|(_, n)| *n >= KEYBOARD_KEYS).map(|(p, _)| p).collect();
    let placed = place(app, &keyboard, web);

    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    let root = app.first().cloned().unwrap_or_default();
    open_tag(&mut out, "Application", &root, (0.0, 0.0, width, height), false);
    // the keyboard's keys, gathered under one Keyboard element at the end (drawn over the app)
    let mut kb_keys: Vec<(usize, Frame)> = Vec::new();
    let dialog = springboard_front && looks_like_dialog(app);
    if dialog {
        let title = app.iter().skip(1).find(|e| kind(e) == "StaticText" && !e.label.is_empty());
        let a = Element {
            label: title.map(|e| e.label.clone()).unwrap_or_default(),
            enabled: true,
            visible: true,
            ..Default::default()
        };
        let boxes: Vec<(usize, Frame)> =
            placed.iter().enumerate().skip(1).filter_map(|(i, f)| f.map(|f| (i, f))).collect();
        open_tag(&mut out, "Alert", &a, keys_box(&boxes).unwrap_or((0.0, 0.0, width, height)), false);
    }
    write_children(&mut out, app, &placed, Some(0), 0, &keyboard, &mut kb_keys);
    if dialog {
        out.push_str("</XCUIElementTypeAlert>");
    }
    let has_keyboard_type = app.iter().zip(&placed).any(|(e, p)| p.is_some() && kind(e) == "Keyboard");
    if !keyboard.is_empty() && !has_keyboard_type {
        let frame = keys_box(&kb_keys).unwrap_or((0.0, height * 0.6, width, height * 0.4));
        let kb = Element { enabled: true, visible: true, ..Default::default() };
        open_tag(&mut out, "Keyboard", &kb, frame, kb_keys.is_empty());
        if !kb_keys.is_empty() {
            for (i, f) in &kb_keys {
                open_tag(&mut out, "Key", &app[*i], *f, true);
            }
            out.push_str("</XCUIElementTypeKeyboard>");
        }
    }
    // SpringBoard over the app, but not its status bar
    let shown: Vec<Option<Frame>> = overlays
        .iter()
        .map(|e| (e.traits & trait_bit::STATUS_BAR == 0).then_some(e.frame.unwrap_or_default()))
        .collect();
    let shown = inherit_skips(overlays, shown);
    write_children(&mut out, overlays, &shown, None, 0, &HashSet::new(), &mut Vec::new());
    out.push_str("</XCUIElementTypeApplication>");
    out
}

/// SpringBoard in front with a few texts and two buttons or more, and none of the home screen's icons, a
/// keyboard, the passcode field or list cells: a question it asks over an app («“设置”想从“dtpasteboardd”粘贴»,
/// permission requests). Read through WDA it is an Alert; here it comes without one (iPhone 15 Pro Max, iOS 27.2.1)
fn looks_like_dialog(app: &[Element]) -> bool {
    let kinds: Vec<&str> = app.iter().skip(1).map(kind).collect();
    app.len() <= 13
        && kinds.iter().filter(|k| **k == "Button").count() >= 2
        && kinds.contains(&"StaticText")
        && !kinds.iter().any(|k| matches!(*k, "Icon" | "Key" | "SecureTextField" | "Cell" | "Alert"))
}

/// Elements under a left-out element are left out too
fn inherit_skips(els: &[Element], mut placed: Vec<Option<Frame>>) -> Vec<Option<Frame>> {
    for i in 0..els.len() {
        if let Some(p) = els[i].parent
            && p < i
            && placed[p].is_none()
        {
            placed[i] = None;
        }
    }
    placed
}

fn keys_box(keys: &[(usize, Frame)]) -> Option<Frame> {
    let real: Vec<_> = keys.iter().map(|(_, f)| *f).filter(|f| f.2 > 0.0 && f.3 > 0.0).collect();
    let x1 = real.iter().map(|f| f.0).reduce(f64::min)?;
    let y1 = real.iter().map(|f| f.1).reduce(f64::min)?;
    let x2 = real.iter().map(|f| f.0 + f.2).reduce(f64::max)?;
    let y2 = real.iter().map(|f| f.1 + f.3).reduce(f64::max)?;
    Some((x1, y1, x2 - x1, y2 - y1))
}

/// Writes the elements whose parent is `parent` (None: the roots of a list, as SpringBoard's overlays have), each
/// with its own children
fn write_children(
    out: &mut String,
    els: &[Element],
    placed: &[Option<Frame>],
    parent: Option<usize>,
    depth: usize,
    keyboard: &HashSet<i64>,
    kb_keys: &mut Vec<(usize, Frame)>,
) {
    for (i, e) in els.iter().enumerate() {
        if e.parent != parent || depth > 80 {
            continue;
        }
        let Some(frame) = placed[i] else { continue };
        if keyboard.contains(&e.pid()) {
            if is_key_char(&e.label) && frame.2 > 0.0 && frame.3 > 0.0 {
                kb_keys.push((i, frame));
            }
            write_children(out, els, placed, Some(i), depth + 1, keyboard, kb_keys);
            continue;
        }
        let has_children = els.iter().any(|c| c.parent == Some(i));
        open_tag(out, kind(e), e, frame, !has_children);
        if has_children {
            write_children(out, els, placed, Some(i), depth + 1, keyboard, kb_keys);
            let _ = write!(out, "</XCUIElementType{}>", kind(e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ObjectRef;

    fn el(pid: i64, id: u64, parent: Option<usize>, label: &str, xc: i64, frame: Frame) -> Element {
        Element {
            object: Some(ObjectRef { pid, id, is_app: parent.is_none() }),
            parent,
            label: label.into(),
            xc_type: xc,
            frame: Some(frame),
            enabled: true,
            visible: true,
            ..Default::default()
        }
    }

    #[test]
    fn the_app_and_its_elements_nest_like_wda_source() {
        let els = vec![
            el(10, 1, None, "设置", 2, (0.0, 0.0, 430.0, 932.0)),
            el(10, 2, Some(0), "", 1, (0.0, 0.0, 430.0, 932.0)),
            el(10, 3, Some(1), "通用", 9, (20.0, 745.3, 390.0, 52.3)),
            el(10, 4, Some(0), "a & <b>", 48, (1.0, 2.0, 3.0, 4.0)),
        ];
        let xml = to_xml(&els, &[], 430.0, 932.0, &HashSet::new(), false);
        assert!(xml.contains(r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" label="设置""#), "{xml}");
        assert!(xml.contains(r#"<XCUIElementTypeOther type="XCUIElementTypeOther" enabled"#), "{xml}");
        assert!(
            xml.contains(r#"label="通用" name="通用" enabled="true" selected="false" focused="false" x="20" y="745" width="390" height="52"/>"#),
            "{xml}"
        );
        assert!(xml.contains("</XCUIElementTypeOther>"));
        assert!(xml.contains("a &amp; &lt;b&gt;"));
        assert!(xml.ends_with("</XCUIElementTypeApplication>"));
        // the button sits inside the Other, which is inside the app
        let (o, b) = (xml.find("XCUIElementTypeOther ").unwrap(), xml.find("通用").unwrap());
        assert!(o < b && b < xml.find("</XCUIElementTypeOther>").unwrap());
    }

    #[test]
    fn another_process_is_placed_by_its_host_or_left_out() {
        let mut host = el(10, 2, Some(0), "", 1, (27.0, 81.0, 179.0, 203.0));
        host.traits = trait_bit::REMOTE_HOST;
        let els = vec![
            el(10, 1, None, "主屏", 2, (0.0, 0.0, 430.0, 932.0)),
            host,
            el(77, 1, Some(1), "上海市", 48, (19.0, 19.0, 52.0, 13.0)),
            el(10, 3, Some(0), "时钟", 44, (228.0, 402.0, 72.0, 95.0)),
            // a widget with no host before it
            el(88, 1, Some(0), "今天无日程", 48, (19.0, 97.0, 85.0, 21.0)),
        ];
        let xml = to_xml(&els, &[], 430.0, 932.0, &HashSet::new(), false);
        assert!(
            xml.contains(
                r#"label="上海市" name="上海市" enabled="true" selected="false" focused="false" x="46" y="100""#
            ),
            "{xml}"
        );
        assert!(!xml.contains("今天无日程"), "{xml}");
        assert!(xml.contains("时钟"));
    }

    #[test]
    fn a_process_of_single_characters_is_the_keyboard() {
        let mut els = vec![el(10, 1, None, "备忘录", 2, (0.0, 0.0, 430.0, 932.0))];
        for (i, c) in "qwertyuiop".chars().enumerate() {
            els.push(el(55, i as u64 + 1, Some(0), &c.to_string(), 0, (i as f64 * 40.0, 600.0, 40.0, 50.0)));
        }
        let xml = to_xml(&els, &[], 430.0, 932.0, &HashSet::new(), false);
        // the keys can't be placed (no host): one Keyboard over the lower part of the screen
        assert!(xml.contains(r#"<XCUIElementTypeKeyboard type="XCUIElementTypeKeyboard" enabled="true" selected="false" focused="false" x="0" y="559" width="430" height="373"/>"#), "{xml}");
        assert!(!xml.contains(r#"label="q""#));
    }

    #[test]
    fn springboard_overlays_go_last_without_the_status_bar() {
        let app = vec![el(10, 1, None, "备忘录", 2, (0.0, 0.0, 430.0, 932.0))];
        let mut time = el(39, 2, None, "14:03", 48, (0.0, 0.0, 80.0, 30.0));
        time.traits = trait_bit::STATUS_BAR;
        let overlays = vec![
            time,
            el(39, 3, None, "“备忘录”想从“dtpasteboardd”粘贴", 7, (55.0, 390.0, 320.0, 175.0)),
            el(39, 4, Some(1), "允许粘贴", 9, (71.0, 501.0, 140.0, 49.0)),
        ];
        let xml = to_xml(&app, &overlays, 430.0, 932.0, &HashSet::new(), false);
        assert!(!xml.contains("14:03"));
        let alert = xml.find("XCUIElementTypeAlert ").unwrap();
        assert!(alert < xml.find("允许粘贴").unwrap());
        assert!(xml.contains("</XCUIElementTypeAlert>"));
    }

    #[test]
    fn a_first_tree_without_types_is_typed_by_traits() {
        let mut e = el(1, 1, Some(0), "x", 0, (0.0, 0.0, 1.0, 1.0));
        e.traits = trait_bit::BUTTON;
        assert_eq!(kind(&e), "Button");
        e.traits = trait_bit::SWITCH | trait_bit::BUTTON;
        assert_eq!(kind(&e), "Switch");
        e.xc_type = 48;
        assert_eq!(kind(&e), "StaticText");
    }

    #[test]
    fn the_lock_screen_passcode_field_reads_as_secure() {
        let field = |value: &str| Element { xc_type: 49, value: value.into(), ..Default::default() };
        assert_eq!(TYPES[49], "TextField");
        assert_eq!(kind(&field("共6位数，已输入0位")), "SecureTextField");
        assert_eq!(kind(&field("6-digit passcode, 2 entered")), "SecureTextField");
        assert_eq!(kind(&field("2 杯咖啡 3 份")), "TextField");
        assert_eq!(kind(&field("hello")), "TextField");
        // untyped, a text field by its traits
        let untyped =
            Element { traits: trait_bit::TEXT_FIELD, value: "共6位数，已输入1位".into(), ..Default::default() };
        assert_eq!(kind(&untyped), "SecureTextField");
    }

    #[test]
    fn a_page_is_placed_as_it_comes_and_a_widget_without_host_is_left_out() {
        let obj = |pid: i64, id: u64| Some(crate::ObjectRef { pid, id, is_app: false });
        let el = |pid, id, parent, label: &str, y| Element {
            object: obj(pid, id),
            parent,
            label: label.into(),
            frame: Some((20.0, y, 200.0, 40.0)),
            xc_type: 48,
            is_element: true,
            enabled: true,
            visible: true,
            ..Default::default()
        };
        let mut app = el(10, 1, None, "Safari", 0.0);
        app.frame = Some((0.0, 0.0, 430.0, 932.0));
        let els = vec![app, el(20, 2, Some(0), "This domain is for use", 300.0), el(30, 3, Some(0), "Widget", 500.0)];
        let web: HashSet<i64> = [20].into();
        let xml = to_xml(&els, &[], 430.0, 932.0, &web, false);
        assert!(xml.contains(r#"label="This domain is for use""#) && xml.contains(r#"y="300""#), "{xml}");
        assert!(!xml.contains("Widget"), "{xml}");
    }

    #[test]
    fn a_question_springboard_asks_reads_as_an_alert() {
        let f = |y| (40.0, y, 350.0, 40.0);
        let els = vec![
            el(39, 1, None, " ", 2, (0.0, 0.0, 430.0, 932.0)),
            el(39, 2, Some(0), "“设置”想从“dtpasteboardd”粘贴", 48, f(380.0)),
            el(39, 3, Some(0), "你允许这样做吗？", 48, f(420.0)),
            el(39, 4, Some(0), "允许粘贴", 9, f(480.0)),
            el(39, 5, Some(0), "不允许粘贴", 9, f(530.0)),
        ];
        let xml = to_xml(&els, &[], 430.0, 932.0, &HashSet::new(), true);
        assert!(
            xml.contains(r#"<XCUIElementTypeAlert type="XCUIElementTypeAlert" label="“设置”想从“dtpasteboardd”粘贴""#),
            "{xml}"
        );
        // the same in an app, or the lock screen's single button: no alert
        assert!(!to_xml(&els, &[], 430.0, 932.0, &HashSet::new(), false).contains("Alert"));
        assert!(!to_xml(&els[..4], &[], 430.0, 932.0, &HashSet::new(), true).contains("Alert"));
    }
}
