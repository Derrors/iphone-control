//! 解析 WDA 的元素树（XML）：留下哪些元素、在哪一层、谁能盖住谁

use super::query::clip;
use super::*;
use quick_xml::{XmlVersion, events::Event};

/// 底部悬浮工具栏/标签栏（iOS 26 起的玻璃栏）实际接收点击的范围比元素树给的框往外多约 20 点。
/// 真机：设置首页搜索栏底板顶边 856，点 834 还能进下面那一行，838 就被栏吞掉（什么都不发生）；
/// 左右两边按同样的余量算
const BAR_SLOP: f64 = 24.0;
/// 原生控制读到的底部悬浮搜索栏（只有输入框，见 `floating_search`）往上吞掉多远的点击。真机（iPhone 15 Pro Max，
/// iOS 27.2.1，设置首页）：输入框顶 866，点 806 还能进下面那一行，812、820、826 都被栏吞掉（什么都不发生）
const SEARCH_BAR_REACH: f64 = 58.0;
/// 同上，左右和往下：玻璃底板比输入框大约 10 点，再加 [`BAR_SLOP`]
const SEARCH_BAR_SIDE: f64 = 10.0 + BAR_SLOP;

/// 可滚动的内容区
const SCROLLERS: [&str; 5] = ["ScrollView", "Table", "CollectionView", "WebView", "TextView"];
/// 盖在一切之上的层
const MODALS: [&str; 4] = ["Keyboard", "Alert", "Sheet", "Popover"];
/// 自成一组的层：组里的东西互相不算盖住（弹窗按钮后面排着弹窗自己的背景层）
const GROUPS: [&str; 7] = ["Toolbar", "TabBar", "NavigationBar", "Keyboard", "Alert", "Sheet", "Popover"];

/// 输入框类元素
pub(super) const EDITABLE: [&str; 4] = ["TextField", "SearchField", "TextView", "SecureTextField"];
/// 子元素的文字已经包含在它们的文字里时，子元素不再单列
const CONTAINERS: [&str; 10] =
    ["Button", "Cell", "Link", "Icon", "MenuItem", "SearchField", "TextField", "Switch", "Tab", "Key"];

/// 只有开发者起的标识、没有给人看的文字：`label-view`、`launch_logo`、UUID 之类
fn looks_like_identifier(s: &str) -> bool {
    s.len() >= 3 && s.is_ascii() && !s.contains(char::is_whitespace) && s.contains(['_', '-', '.', ':'])
}

/// A scroll bar: an `Other` whose value is the scroll position as a percentage, about 30 points across and running
/// most of the way along what it scrolls (`within`: the scroll view's visible size, or the screen when there is none,
/// as on the home screen). In the device samples the bars are 25-31 points thick and at least 68% as long as that, but
/// aren't flush with its edge. A custom progress strip reporting a percentage is thinner or shorter, so it stays.
/// The label depends on the system language ("垂直滚动条, 1页", "Vertikaler Rollbalken, 1 Seite", …), so it is only an
/// extra hint for Chinese and English
pub(super) fn is_scroll_bar(kind: &str, label: &str, value: &str, size: (f64, f64), within: (f64, f64)) -> bool {
    if kind != "Other" {
        return false;
    }
    if label.contains("滚动条") || label.to_lowercase().contains("scroll bar") {
        return true;
    }
    let (thick, long, room) = if size.1 >= size.0 { (size.0, size.1, within.1) } else { (size.1, size.0, within.0) };
    is_percentage(value) && (24.0..=32.0).contains(&thick) && long >= room * 0.6
}

/// "0%", "73 %" (with a no-break space in German and French), "%40" (Turkish)
pub(super) fn is_percentage(v: &str) -> bool {
    let v = v.trim();
    let n = v.strip_suffix('%').or_else(|| v.strip_prefix('%')).map(str::trim);
    n.is_some_and(|n| !n.is_empty() && n.chars().all(char::is_numeric))
}

/// Python `round()` 是银行家舍入；坐标这里差半点无所谓，但为了与 toolkit 输出逐字一致，照它来
fn py_round(v: f64) -> i64 {
    let r = v.round();
    if (v - v.trunc()).abs() == 0.5 && (r as i64) % 2 != 0 { (r - v.signum()) as i64 } else { r as i64 }
}

pub fn parse(xml: &str, view: impl Into<View>) -> Vec<Element> {
    parse_tree(xml, view).elements
}

/// 解析时每个还没闭合的节点
struct Open {
    /// 元素树坐标（小组件偏移已加上，还没换算到屏幕）
    abs: Rect,
    /// 在工具栏/标签栏里
    in_bar: bool,
    /// 在导航栏/工具栏/标签栏里
    on_bar: bool,
    /// 在可滚动的内容里
    in_scroll: bool,
    /// 自己是滚动区就是自己，否则同父节点（屏幕坐标）
    scroller: Option<Rect>,
    /// 在键盘、弹窗、面板里
    modal: bool,
    group: usize,
    /// 这一层子节点的原始坐标要加上的偏移（小组件内部用的是相对小组件的坐标）
    offset: (f64, f64),
    /// 自己是 WebView 或在 WebView 里
    in_web: bool,
    in_status_bar: bool,
    kind: String,
    kept: Option<usize>,
    blocker: Option<usize>,
}

pub fn parse_tree(xml: &str, view: impl Into<View>) -> Tree {
    let view = view.into();
    let screen = view.screen;
    let (sw, sh) = (screen.width, screen.height);
    // 看到 Application 下第一个窗口时定下来
    let mut rotate: Option<Rotate> = None;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut seen = std::collections::HashSet::new();
    let mut tree = Tree::default();
    let mut stack: Vec<Open> = Vec::new();
    // 节点序号（文档顺序）
    let mut n = 0usize;
    let close = |tree: &mut Tree, o: &Open, end: usize| {
        if let Some(i) = o.kept {
            tree.elements[i].span.1 = end;
        }
        if let Some(i) = o.blocker {
            tree.blockers[i].span.1 = end;
        }
    };
    loop {
        let (e, empty) = match reader.read_event() {
            Ok(Event::Start(e)) => (e, false),
            Ok(Event::Empty(e)) => (e, true),
            Ok(Event::End(_)) => {
                if let Some(o) = stack.pop() {
                    close(&mut tree, &o, n);
                }
                continue;
            }
            Ok(Event::Eof) => {
                tree.incomplete = !stack.is_empty();
                break;
            }
            Err(e) => {
                // 解析到一半出错：已解析的照常返回，但要留痕，否则「没找到元素」看起来像真结果
                tracing::warn!(
                    "{}",
                    ictl_l10n::log_trf!(
                        "元素树 XML 解析出错（位置 {}），只用了前面的部分：{e}",
                        "Error parsing the element tree XML (at {}), only the part before it is used: {e}",
                        reader.buffer_position()
                    )
                );
                tree.incomplete = true;
                break;
            }
            Ok(_) => continue,
        };
        n += 1;
        let tag = e.name().as_ref().to_owned();
        let mut attr = std::collections::HashMap::new();
        for a in e.attributes().flatten() {
            let k = a.key.as_ref().to_owned();
            let v = a.normalized_value(XmlVersion::Implicit1_0).map(|c| c.into_owned()).unwrap_or_default();
            attr.insert(k, v);
        }
        let get = |k: &str| attr.get(k).map(|s| s.trim().to_owned()).unwrap_or_default();
        let (label, name, value) = (get("label"), get("name"), get("value"));
        let num = |k: &str| attr.get(k).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let raw = Rect { x: num("x"), y: num("y"), w: num("width"), h: num("height") };
        let full_kind = attr.get("type").cloned().unwrap_or(tag);
        let kind = full_kind.strip_prefix("XCUIElementType").unwrap_or(&full_kind).to_owned();

        // 小组件的内容：某个节点在 (0,0) 且和父节点一样大，而父节点不在 (0,0)——从它开始坐标是相对父节点的。
        // 网页里不算：WKWebView 内部有一层节点也报 (0,0)、和 WebView 一样大，但网页元素用的是屏幕坐标，
        // 加上偏移会把整页错开 WebView 的 y（App 内浏览器、小程序在导航栏下面时约 100 点）
        let parent = stack.last();
        let in_web = kind == "WebView" || parent.is_some_and(|p| p.in_web);
        let mut offset = parent.map_or((0.0, 0.0), |p| p.offset);
        if let Some(p) = parent
            && !p.in_web
            && raw.x == 0.0
            && raw.y == 0.0
            && (raw.w - p.abs.w).abs() < 1.0
            && (raw.h - p.abs.h).abs() < 1.0
            && (p.abs.x != 0.0 || p.abs.y != 0.0)
        {
            offset = (p.abs.x, p.abs.y);
        }
        let abs = Rect { x: raw.x + offset.0, y: raw.y + offset.1, ..raw };
        if rotate.is_none() && kind == "Window" && stack.len() == 1 && abs.w > 0.0 && abs.h > 0.0 {
            rotate = Some(Rotate::detect(abs, view, Rotate::status_bar(xml)));
        }
        // 下面判断在不在屏幕内、挡没挡住、点哪里，都用屏幕坐标
        let scr = rotate.unwrap_or(Rotate::None).apply(abs, screen);
        let in_status_bar = kind == "StatusBar" || parent.is_some_and(|p| p.in_status_bar);
        // 原生控制读到的树是扁平的：设置首页底部悬浮的搜索栏只是一个直接挂在 Application 下的 SearchField，外面没有
        // WDA 树里那层 Toolbar。屏幕下部的这种搜索框也按悬浮栏算：盖得住下面的行，接收点击的范围也多算 BAR_SLOP。
        // 不然被压在栏下的行按名字点会点它的中心，打开的是搜索（iPhone 15 Pro Max，iOS 27.2.1）
        let floating_search =
            kind == "SearchField" && parent.is_some_and(|p| p.kind == "Application") && scr.y > sh * 0.6;
        let in_bar =
            matches!(kind.as_str(), "Toolbar" | "TabBar") || floating_search || parent.is_some_and(|p| p.in_bar);
        let on_bar = in_bar || kind == "NavigationBar" || parent.is_some_and(|p| p.on_bar);
        // 子节点在可滚动内容里（容器自己不算：它就是那块区域）
        let in_scroll = parent.is_some_and(|p| p.in_scroll || SCROLLERS.contains(&p.kind.as_str()));
        let modal = MODALS.contains(&kind.as_str()) || parent.is_some_and(|p| p.modal);
        let group = if GROUPS.contains(&kind.as_str()) { n } else { parent.map_or(0, |p| p.group) };
        let onscreen =
            scr.w > 0.0 && scr.h > 0.0 && scr.x < sw && scr.y < sh && scr.x + scr.w > 0.0 && scr.y + scr.h > 0.0;
        let (vx1, vy1) = (scr.x.max(0.0), scr.y.max(0.0));
        let visible = Rect { x: vx1, y: vy1, w: (scr.x + scr.w).min(sw) - vx1, h: (scr.y + scr.h).min(sh) - vy1 };
        let text = [&label, &name, &value].into_iter().find(|s| !s.is_empty()).cloned();
        let scroll_area = parent.and_then(|p| p.scroller).map_or((sw, sh), |s| (s.w, s.h));
        let scroll_bar = is_scroll_bar(&kind, &label, &value, (scr.w, scr.h), scroll_area);
        if kind == "ActivityIndicator" && onscreen {
            tree.loading = true;
        }

        // 能挡住别人的：在屏幕内、不是铺满大半屏的透明容器、不是细线和滚动条
        let blocker = (onscreen
            && !in_scroll
            && !SCROLLERS.contains(&kind.as_str())
            && !scroll_bar
            && !matches!(kind.as_str(), "Application" | "Window")
            && visible.w >= 8.0
            && visible.h >= 8.0
            && visible.w * visible.h < sw * sh * 0.5)
            .then(|| {
                let name = text.as_deref().map(|t| clip(t, 20, 20)).unwrap_or_default();
                let mut rect = visible;
                // 原生的树里只有栏里的输入框，没有它外面那块玻璃底板：按真机量的范围算它吞掉的点击
                if floating_search {
                    rect = Rect {
                        x: rect.x - SEARCH_BAR_SIDE,
                        y: rect.y - SEARCH_BAR_REACH,
                        w: rect.w + 2.0 * SEARCH_BAR_SIDE,
                        h: rect.h + SEARCH_BAR_REACH + SEARCH_BAR_SIDE,
                    };
                } else if in_bar && rect.y > sh * 0.6 {
                    rect = Rect {
                        x: rect.x - BAR_SLOP,
                        y: rect.y - BAR_SLOP,
                        w: rect.w + 2.0 * BAR_SLOP,
                        h: rect.h + BAR_SLOP,
                    };
                }
                tree.blockers.push(Blocker { span: (n, n), rect, floating: in_bar || modal, modal, group, name });
                tree.blockers.len() - 1
            });
        if let Some(t) = &text {
            for o in &stack {
                if let Some(i) = o.blocker
                    && tree.blockers[i].name.is_empty()
                {
                    tree.blockers[i].name = clip(t, 20, 20);
                }
            }
        }

        let scroller = if SCROLLERS.contains(&kind.as_str()) { Some(visible) } else { parent.and_then(|p| p.scroller) };
        let kind_for_children = kind.clone();
        let kept = text.filter(|text| {
            let noise = !onscreen
                || (scr.w >= sw && scr.h >= sh)
                || scroll_bar
                || (in_status_bar && kind != "Button")
                || (matches!(kind.as_str(), "Other" | "Image")
                    && value.is_empty()
                    && (label.is_empty() || label == name)
                    && looks_like_identifier(&name))
                // 按钮、单元格里的图标，和重复按钮文字的子文字
                || (matches!(kind.as_str(), "StaticText" | "Image")
                    && stack.iter().filter_map(|o| o.kept).map(|i| &tree.elements[i]).any(|a| {
                        CONTAINERS.contains(&a.kind.as_str())
                            && !a.label.is_empty()
                            && (kind == "Image" || a.label.contains(text.as_str()))
                    }));
            !noise && seen.insert((text.clone(), py_round(scr.x), py_round(scr.y), py_round(scr.w), py_round(scr.h)))
        });
        let kept = kept.map(|_| {
            tree.elements.push(Element {
                kind,
                label,
                name,
                value,
                center: (py_round(visible.x + visible.w / 2.0), py_round(visible.y + visible.h / 2.0)),
                size: (py_round(scr.w), py_round(scr.h)),
                visible,
                span: (n, n),
                layer: if on_bar {
                    Layer::Bar
                } else if in_scroll {
                    Layer::Scroll
                } else {
                    Layer::Plain
                },
                group,
                scroller: parent.and_then(|p| p.scroller),
            });
            tree.elements.len() - 1
        });
        let o = Open {
            abs,
            in_bar,
            on_bar,
            in_scroll,
            scroller,
            modal,
            group,
            kind: kind_for_children,
            offset,
            in_web,
            in_status_bar,
            kept,
            blocker,
        };
        if empty {
            close(&mut tree, &o, n);
        } else {
            stack.push(o);
        }
    }
    tree
}
