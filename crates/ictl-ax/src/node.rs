//! An accessibility tree as the element tree WebDriverAgent's `/source` gives (an [`ictl_ui::Source`] of
//! `XCUIElementType` kinds with labels, names, values and frames), so `ictl_ui::Tree::of` reads it as it reads WDA's:
//! finding by text, what covers what, the keyboard, alerts, the lock screen and the passcode pad.
//!
//! Elements of other processes (a keyboard, an extension, a remote view) come with frames relative to the view
//! hosting them, which carries [`trait_bit::REMOTE_HOST`] and comes right before them; they are moved to screen
//! points by that host. Those without a host (home screen widgets) can't be placed and are left out. A process
//! whose elements are mostly single characters is the on-screen keyboard: it becomes one `Keyboard` element.

use std::collections::{HashMap, HashSet};

use ictl_ui::{Node, Rect, Source};

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
    // which is what finds it when unlocking (`ictl_ui::passcode_field`, `cover_sheet`)
    if k == "TextField" && is_passcode_field(e) { "SecureTextField" } else { k }
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

/// The lock screen's passcode field. Its value holds two numbers, how many digits were entered and how many there are,
/// in either order: 「共6位数，已输入2位」, "0 of 6 values entered", "0 valeur saisie sur 6", "0 von 6 Werten eingegeben".
/// Known words in the label or the value say so; with none (a language not listed), a 4- or 6-digit total still does
/// when words stand between the numbers: a field showing a time, a fraction or a decimal ("6:00", "2/4", "1.6") is
/// no passcode field, and taking it for one made the screen read as locked
fn is_passcode_field(e: &Element) -> bool {
    let runs = digit_runs(&e.value);
    let [(a, _), (b, sep)] = runs[..] else { return false };
    let total = a.max(b);
    if total == 0 {
        return false;
    }
    let label = e.label.to_lowercase();
    let value = e.value.to_lowercase();
    PASSCODE_LABEL_WORDS.iter().any(|w| label.contains(w))
        || PASSCODE_VALUE_WORDS.iter().any(|w| value.contains(w))
        || (matches!(total, 4 | 6) && sep.chars().any(char::is_alphabetic))
}

/// The numbers in `s`, each with the text before it (since the previous number)
fn digit_runs(s: &str) -> Vec<(u32, &str)> {
    let mut out = Vec::new();
    let (mut after, mut i) = (0, 0);
    let b = s.as_bytes();
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            out.push((s[start..i].parse().unwrap_or(u32::MAX), &s[after..start]));
            after = i;
        } else {
            i += 1;
        }
    }
    out
}

/// Words in the passcode field's label (lowercase): 「密码栏」, "Passcode field", "Champ de code"
const PASSCODE_LABEL_WORDS: [&str; 3] = ["密码", "passcode", "champ de code"];

/// Words in the passcode field's value (lowercase)
const PASSCODE_VALUE_WORDS: [&str; 7] = ["位", "passcode", "digit", "entered", "saisie", "eingegeben", "introducido"];

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
            // a page (WebKit's web content process) is listed without a host, in screen points
            None if web.contains(&e.pid()) => out.push(Some((x, y, w, h))),
            None => out.push(None),
        }
    }
    out
}

/// Text as WDA's XML gave it: no control characters but line breaks, no whitespace around it
fn text(s: &str) -> String {
    s.chars().filter(|c| *c == '\n' || u32::from(*c) >= 0x20).collect::<String>().trim().to_owned()
}

/// `e` as a node of `kind` at `frame` (whole points), without children. Its name is its identifier, or its label;
/// its value is its value, or its placeholder
fn node(kind: &str, e: &Element, (x, y, w, h): Frame) -> Node {
    let whole = |v: f64| v.round() as i64 as f64;
    Node {
        kind: kind.to_owned(),
        label: text(&e.label),
        name: text(if e.identifier.is_empty() { &e.label } else { &e.identifier }),
        value: text(if e.value.is_empty() { &e.placeholder } else { &e.value }),
        frame: Rect { x: whole(x), y: whole(y), w: whole(w), h: whole(h) },
        children: Vec::new(),
    }
}

/// The tree for `app` (the frontmost application's tree, its element first) with `overlays` (what SpringBoard
/// draws over it: alerts, banners; in screen points) after it, on a `width`×`height` screen. `web`: the processes
/// that are WebKit's web content (a page's elements), placed as they come. `springboard_front`: SpringBoard is the
/// app in front, as when it asks something over an app (see `looks_like_dialog`)
pub fn to_node(
    app: &[Element],
    overlays: &[Element],
    width: f64,
    height: f64,
    web: &HashSet<i64>,
    springboard_front: bool,
) -> Source {
    let app_pid = app.first().map_or(0, Element::pid);
    let mut keys: HashMap<i64, usize> = HashMap::new();
    // a page's own one-letter links are no keys
    for e in app.iter().filter(|e| e.pid() != app_pid && !web.contains(&e.pid()) && is_key_char(&e.label)) {
        *keys.entry(e.pid()).or_default() += 1;
    }
    let keyboard: HashSet<i64> = keys.into_iter().filter(|(_, n)| *n >= KEYBOARD_KEYS).map(|(p, _)| p).collect();
    let placed = place(app, &keyboard, web);

    let root = app.first().cloned().unwrap_or_default();
    let mut out = node("Application", &root, (0.0, 0.0, width, height));
    // the keyboard's keys, gathered under one Keyboard element at the end (drawn over the app)
    let mut kb_keys: Vec<(usize, Frame)> = Vec::new();
    let children = children(app, &placed, Some(0), 0, &keyboard, &mut kb_keys);
    if springboard_front && looks_like_dialog(app) {
        let title = app
            .iter()
            .skip(1)
            .find(|e| kind(e) == "StaticText" && !e.label.is_empty() && e.traits & trait_bit::STATUS_BAR == 0);
        let a = Element {
            label: title.map(|e| e.label.clone()).unwrap_or_default(),
            enabled: true,
            visible: true,
            ..Default::default()
        };
        let boxes: Vec<(usize, Frame)> =
            placed.iter().enumerate().skip(1).filter_map(|(i, f)| f.map(|f| (i, f))).collect();
        let mut alert = node("Alert", &a, keys_box(&boxes).unwrap_or((0.0, 0.0, width, height)));
        alert.children = children;
        out.children.push(alert);
    } else {
        out.children = children;
    }
    let has_keyboard_type = app.iter().zip(&placed).any(|(e, p)| p.is_some() && kind(e) == "Keyboard");
    if !keyboard.is_empty() && !has_keyboard_type {
        let frame = keys_box(&kb_keys).unwrap_or((0.0, height * 0.6, width, height * 0.4));
        let mut kb = node("Keyboard", &Element::default(), frame);
        kb.children = kb_keys.iter().map(|(i, f)| node("Key", &app[*i], *f)).collect();
        out.children.push(kb);
    }
    // SpringBoard over the app, but not its status bar
    let shown: Vec<Option<Frame>> = overlays
        .iter()
        .map(|e| (e.traits & trait_bit::STATUS_BAR == 0).then_some(e.frame.unwrap_or_default()))
        .collect();
    let shown = inherit_skips(overlays, shown);
    out.children.extend(children_of(overlays, &shown));
    Source { roots: vec![out], incomplete: false }
}

/// The roots of `els` (SpringBoard's overlays have no parent), each with its own children
fn children_of(els: &[Element], placed: &[Option<Frame>]) -> Vec<Node> {
    children(els, placed, None, 0, &HashSet::new(), &mut Vec::new())
}

/// The elements whose parent is `parent` (None: the roots of a list), each with its own children. A keyboard
/// process's elements aren't nodes: their keys go to `kb_keys`, what is under them takes their place
fn children(
    els: &[Element],
    placed: &[Option<Frame>],
    parent: Option<usize>,
    depth: usize,
    keyboard: &HashSet<i64>,
    kb_keys: &mut Vec<(usize, Frame)>,
) -> Vec<Node> {
    let mut out = Vec::new();
    for (i, e) in els.iter().enumerate() {
        if e.parent != parent || depth > 80 {
            continue;
        }
        let Some(frame) = placed[i] else { continue };
        if keyboard.contains(&e.pid()) {
            if is_key_char(&e.label) && frame.2 > 0.0 && frame.3 > 0.0 {
                kb_keys.push((i, frame));
            }
            out.extend(children(els, placed, Some(i), depth + 1, keyboard, kb_keys));
            continue;
        }
        let mut n = node(kind(e), e, frame);
        n.children = children(els, placed, Some(i), depth + 1, keyboard, kb_keys);
        out.push(n);
    }
    out
}

/// SpringBoard in front with a few texts and two buttons or more, and none of the home screen's icons, a
/// keyboard, the passcode field or list cells: a question it asks over an app («“设置”想从“dtpasteboardd”粘贴»,
/// permission requests). Read through WDA it is an Alert; here it comes without one (iPhone 15 Pro Max, iOS 27.2.1)
fn looks_like_dialog(app: &[Element]) -> bool {
    // the status bar's items don't count (with SpringBoard in front they are its own: the lock screen, Face ID's
    // prompt read as an alert titled with the time)
    let shown: Vec<&Element> = app.iter().skip(1).filter(|e| e.traits & trait_bit::STATUS_BAR == 0).collect();
    let kinds: Vec<&str> = shown.iter().map(|e| kind(e)).collect();
    shown.len() <= 12
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

    fn tree(app: &[Element]) -> Source {
        to_node(app, &[], 430.0, 932.0, &HashSet::new(), false)
    }

    /// The first node labelled `label`
    fn find<'a>(s: &'a Source, label: &str) -> Option<&'a Node> {
        s.nodes().find(|n| n.label == label)
    }

    fn frame(n: &Node) -> (f64, f64, f64, f64) {
        (n.frame.x, n.frame.y, n.frame.w, n.frame.h)
    }

    #[test]
    fn the_app_and_its_elements_nest_like_wda_source() {
        let els = vec![
            el(10, 1, None, "设置", 2, (0.0, 0.0, 430.0, 932.0)),
            el(10, 2, Some(0), "", 1, (0.0, 0.0, 430.0, 932.0)),
            el(10, 3, Some(1), "通用", 9, (20.0, 745.3, 390.0, 52.3)),
            el(10, 4, Some(0), "a & <b>", 48, (1.0, 2.0, 3.0, 4.0)),
        ];
        let s = tree(&els);
        assert_eq!(s.roots.len(), 1);
        let app = &s.roots[0];
        assert_eq!((app.kind.as_str(), app.label.as_str()), ("Application", "设置"));
        assert_eq!(app.children.iter().map(|c| c.kind.as_str()).collect::<Vec<_>>(), ["Other", "StaticText"]);
        // the button sits inside the Other, which is inside the app; its name is its label, in whole points
        let b = &app.children[0].children[0];
        assert_eq!((b.kind.as_str(), b.label.as_str(), b.name.as_str()), ("Button", "通用", "通用"));
        assert_eq!(frame(b), (20.0, 745.0, 390.0, 52.0));
        assert_eq!(app.children[1].label, "a & <b>");
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
        let s = tree(&els);
        assert_eq!(frame(find(&s, "上海市").unwrap()), (46.0, 100.0, 52.0, 13.0));
        assert!(find(&s, "今天无日程").is_none());
        assert!(find(&s, "时钟").is_some());
    }

    #[test]
    fn a_process_of_single_characters_is_the_keyboard() {
        let mut els = vec![el(10, 1, None, "备忘录", 2, (0.0, 0.0, 430.0, 932.0))];
        for (i, c) in "qwertyuiop".chars().enumerate() {
            els.push(el(55, i as u64 + 1, Some(0), &c.to_string(), 0, (i as f64 * 40.0, 600.0, 40.0, 50.0)));
        }
        let s = tree(&els);
        // the keys can't be placed (no host): one Keyboard over the lower part of the screen
        let kb = s.nodes().find(|n| n.kind == "Keyboard").expect("a keyboard");
        assert_eq!(frame(kb), (0.0, 559.0, 430.0, 373.0));
        assert!(kb.children.is_empty());
        assert!(find(&s, "q").is_none());
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
        let s = to_node(&app, &overlays, 430.0, 932.0, &HashSet::new(), false);
        assert!(find(&s, "14:03").is_none());
        let alert = s.roots[0].children.last().unwrap();
        assert_eq!(alert.kind, "Alert");
        assert_eq!(alert.children[0].label, "允许粘贴");
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
        // English, French, German and Spanish put the entered digits first (#174)
        assert_eq!(kind(&field("0 of 6 values entered")), "SecureTextField");
        assert_eq!(kind(&field("0 valeur saisie sur 6")), "SecureTextField");
        assert_eq!(kind(&field("0 von 6 Werten eingegeben")), "SecureTextField");
        assert_eq!(kind(&field("0 de 6 valores introducidos")), "SecureTextField");
        // a language with no known words: a 4- or 6-digit total is enough
        assert_eq!(kind(&field("6 中 3")), "SecureTextField");
        // the label alone says so, whatever the total
        let labelled = Element { label: "Passcode field".into(), ..field("2 / 8") };
        assert_eq!(kind(&labelled), "SecureTextField");
        assert_eq!(kind(&field("2 / 8")), "TextField");
        assert_eq!(kind(&field("2 杯咖啡 3 份")), "TextField");
        assert_eq!(kind(&field("0 0")), "TextField");
        assert_eq!(kind(&field("1 2 3")), "TextField");
        assert_eq!(kind(&field("hello")), "TextField");
        // a time, a fraction, a decimal: punctuation between the numbers, no words
        for v in ["6:00", "2/4", "1.6", "4-6", "06:04"] {
            assert_eq!(kind(&field(v)), "TextField", "{v}");
        }
        // untyped, a text field by its traits
        let untyped =
            Element { traits: trait_bit::TEXT_FIELD, value: "共6位数，已输入1位".into(), ..Default::default() };
        assert_eq!(kind(&untyped), "SecureTextField");
    }

    #[test]
    fn a_page_is_placed_as_it_comes_and_a_widget_without_host_is_left_out() {
        let s = to_node(&page(), &[], 430.0, 932.0, &[20].into(), false);
        assert_eq!(frame(find(&s, "This domain is for use").unwrap()).1, 300.0);
        assert!(find(&s, "Widget").is_none());
    }

    fn page() -> Vec<Element> {
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
        vec![app, el(20, 2, Some(0), "This domain is for use", 300.0), el(30, 3, Some(0), "Widget", 500.0)]
    }

    fn question() -> Vec<Element> {
        let f = |y| (40.0, y, 350.0, 40.0);
        vec![
            el(39, 1, None, " ", 2, (0.0, 0.0, 430.0, 932.0)),
            el(39, 2, Some(0), "“设置”想从“dtpasteboardd”粘贴", 48, f(380.0)),
            el(39, 3, Some(0), "你允许这样做吗？", 48, f(420.0)),
            el(39, 4, Some(0), "允许粘贴", 9, f(480.0)),
            el(39, 5, Some(0), "不允许粘贴", 9, f(530.0)),
        ]
    }

    #[test]
    fn a_question_springboard_asks_reads_as_an_alert() {
        let els = question();
        let alert = |els: &[Element], front| {
            to_node(els, &[], 430.0, 932.0, &HashSet::new(), front).nodes().find(|n| n.kind == "Alert").cloned()
        };
        let a = alert(&els, true).expect("an alert");
        assert_eq!(a.label, "“设置”想从“dtpasteboardd”粘贴");
        assert_eq!(a.children.len(), 4);
        // the same in an app, or the lock screen's single button: no alert
        assert!(alert(&els, false).is_none());
        assert!(alert(&els[..4], true).is_none());
        // the status bar's time and buttons alone are no question
        let f = (40.0, 10.0, 350.0, 40.0);
        let bar = |id, label: &str, xc| Element { traits: trait_bit::STATUS_BAR, ..el(39, id, Some(0), label, xc, f) };
        let status_only = vec![els[0].clone(), bar(6, "18:36", 48), bar(7, "Wi-Fi", 9), bar(8, "电池", 9)];
        assert!(alert(&status_only, true).is_none());
    }

    /// A read: the app's elements, SpringBoard's over it, the web content processes, SpringBoard in front
    type Read = (Vec<Element>, Vec<Element>, HashSet<i64>, bool);

    /// Reads of every kind the tests above make, and text XML has to escape or drop
    fn reads() -> Vec<Read> {
        let mut odd = vec![el(10, 1, None, " 设置 ", 2, (0.0, 0.0, 430.0, 932.0))];
        let mut e = el(10, 2, Some(0), "two\nlines\t\u{1}and \"quotes\" & <tags>", 9, (10.4, 20.5, 30.6, 40.5));
        e.identifier = "row-1".into();
        e.placeholder = "  搜索  ".into();
        odd.push(e);
        let mut field = el(10, 3, Some(0), "", 49, (0.0, 100.0, 430.0, 40.0));
        field.value = "共6位数，已输入2位".into();
        odd.push(field);
        let mut keyboard = vec![el(10, 1, None, "备忘录", 2, (0.0, 0.0, 430.0, 932.0))];
        let mut host = el(10, 2, Some(0), "", 1, (0.0, 560.0, 430.0, 372.0));
        host.traits = trait_bit::REMOTE_HOST;
        keyboard.push(host);
        for (i, c) in "asdfghjkl".chars().enumerate() {
            keyboard.push(el(55, i as u64 + 1, Some(1), &c.to_string(), 0, (i as f64 * 43.0, 10.0, 40.0, 50.0)));
        }
        let mut time = el(39, 2, None, "14:03", 48, (0.0, 0.0, 80.0, 30.0));
        time.traits = trait_bit::STATUS_BAR;
        let overlays = vec![
            time,
            el(39, 3, None, "“备忘录”想从“dtpasteboardd”粘贴", 7, (55.0, 390.0, 320.0, 175.0)),
            el(39, 4, Some(1), "允许粘贴", 9, (71.0, 501.0, 140.0, 49.0)),
        ];
        let mut host = el(10, 2, Some(0), "", 1, (27.0, 81.0, 179.0, 203.0));
        host.traits = trait_bit::REMOTE_HOST;
        let widgets = vec![
            el(10, 1, None, "主屏", 2, (0.0, 0.0, 430.0, 932.0)),
            host,
            el(77, 1, Some(1), "上海市", 48, (19.0, 19.0, 52.0, 13.0)),
            el(88, 1, Some(0), "今天无日程", 48, (19.0, 97.0, 85.0, 21.0)),
        ];
        let none = HashSet::new;
        vec![
            (odd, vec![], none(), false),
            (keyboard.clone(), overlays.clone(), none(), false),
            (keyboard[..3].to_vec(), vec![], none(), false),
            (widgets, vec![], none(), false),
            (page(), vec![], [20].into(), false),
            (question(), vec![], none(), true),
            (question()[..4].to_vec(), overlays, none(), true),
        ]
    }

    /// Written out (`debug … --out`) and read back, a tree is the same
    #[test]
    fn a_tree_written_as_xml_reads_back_the_same() {
        for (i, (app, overlays, web, front)) in reads().iter().enumerate() {
            let t = to_node(app, overlays, 430.0, 932.0, web, *front);
            assert_eq!(Source::from_xml(&t.to_xml()), t, "read {i}");
        }
    }
}
