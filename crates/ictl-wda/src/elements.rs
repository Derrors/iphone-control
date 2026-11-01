//! 元素树解析（纯函数）。行为与 toolkit 的 `src/iphone_mcp/elements.py` 一致：
//!
//! - 只保留有 label/name/value 的元素
//! - 按坐标判断是否在屏幕内（请求时排除了 WDA 的可见性计算：完整树 7～8 秒，排除后约 0.3 秒）
//! - 铺满全屏的 Application/Window 之类去掉；父子重复（同文字同位置）去重
//! - 按钮、单元格里重复按钮文字的子文字、按钮里的图标、滚动条、状态栏、只有开发者标识的容器和图片去掉
//!   （只占 token，按名字点按钮本身就行）
//! - 小组件里的元素 WDA 给的是相对小组件的坐标，换算成屏幕坐标
//! - 点击中心取元素在屏幕内的部分；中心被后面（上层）的东西盖住时另找没被盖住的点（[`Tree::tap_point`]）
//! - 锁屏密码键盘的 `Key` 元素不显示时也在树里，**不能**据此判断键盘已出现

use ictl_device::ScreenSize;
use quick_xml::{XmlVersion, events::Event};

/// `/source?format=xml&excluded_attributes=` 要排除的属性
pub const SOURCE_EXCLUDED: &str =
    "visible,accessible,focused,enabled,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame";

/// 屏幕坐标下的矩形（点）
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    fn contains(&self, (px, py): (f64, f64)) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

#[derive(Debug, Clone, Default)]
pub struct Element {
    pub kind: String,
    pub label: String,
    pub name: String,
    pub value: String,
    pub center: (i64, i64),
    pub size: (i64, i64),
    /// 在屏幕内的部分（屏幕坐标）
    pub visible: Rect,
    /// 在整棵树里的先后序号：(自己, 最后一个后代)。后出现的画在上层
    span: (usize, usize),
    /// 在哪一层，判断谁能盖住它（见 [`Tree::tap_point`]）
    layer: Layer,
    /// 最近的栏、键盘、弹窗、面板祖先（含自己）的序号；0 = 没有。同一个栏/弹窗里的东西互相不算盖住
    group: usize,
}

/// 元素所在的层
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Layer {
    /// 普通界面
    #[default]
    Plain,
    /// 可滚动的内容里（列表、网页）
    Scroll,
    /// 导航栏、工具栏、标签栏里
    Bar,
}

/// 只比看得见的内容：等界面稳定时比较前后两棵树，不因没有文字的节点增减而算「变了」
impl PartialEq for Element {
    fn eq(&self, o: &Self) -> bool {
        (&self.kind, &self.label, &self.name, &self.value, self.center, self.size)
            == (&o.kind, &o.label, &o.name, &o.value, o.center, o.size)
    }
}

/// 可能盖住别的元素的东西，包括没有文字的容器（例如悬浮搜索栏的底板、键盘）。
/// 可滚动内容里的节点不算（单元格的背景层、滚到导航栏底下的行）
#[derive(Debug, Clone, PartialEq)]
struct Blocker {
    span: (usize, usize),
    rect: Rect,
    /// 在工具栏、标签栏、键盘、弹窗、面板里（浮在内容上面的层）
    floating: bool,
    /// 在键盘、弹窗、面板里（能盖住导航栏的）
    modal: bool,
    /// 同 [`Element::group`]
    group: usize,
    /// 自己或第一个有文字的后代的文字，用来告诉调用方是什么挡着
    name: String,
}

/// 解析后的元素树
#[derive(Debug, Clone, Default)]
pub struct Tree {
    pub elements: Vec<Element>,
    blockers: Vec<Blocker>,
    /// 屏幕上有转圈的加载指示（ActivityIndicator，有没有文字都算）
    pub loading: bool,
}

impl PartialEq for Tree {
    fn eq(&self, o: &Self) -> bool {
        self.elements == o.elements && self.loading == o.loading
    }
}

/// 按名字点击时实际点哪里
#[derive(Debug, Clone, PartialEq)]
pub enum TapPoint {
    Center((f64, f64)),
    /// 中心被 `by` 盖住，改点元素里没被盖住的 `at`
    Moved {
        at: (f64, f64),
        by: String,
    },
    /// 整个都被 `by` 盖住
    Covered {
        by: String,
    },
}

impl Tree {
    /// 和另一棵树看起来一样：元素一一对应、文字相同，位置最多差 `tol` 点。
    /// 列表惯性滚动的尾巴每次只挪一两点，按完全相同判断要多等一两秒
    pub fn close_to(&self, o: &Tree, tol: i64) -> bool {
        self.loading == o.loading
            && self.elements.len() == o.elements.len()
            && self.elements.iter().zip(&o.elements).all(|(a, b)| {
                (&a.kind, &a.label, &a.name, &a.value, a.size) == (&b.kind, &b.label, &b.name, &b.value, b.size)
                    && (a.center.0 - b.center.0).abs() <= tol
                    && (a.center.1 - b.center.1).abs() <= tol
            })
    }

    /// 滑动后判断列表停没停：只看沿滑动方向（`vertical`）的位置，不看值和另一个方向。
    /// 网页里自动轮播的图、播放进度一直在变，但列表已经停了
    pub fn still_along(&self, o: &Tree, vertical: bool, tol: i64) -> bool {
        self.elements.len() == o.elements.len()
            && self.elements.iter().zip(&o.elements).all(|(a, b)| {
                let (pa, pb) = if vertical { (a.center.1, b.center.1) } else { (a.center.0, b.center.0) };
                (&a.kind, &a.label, &a.name) == (&b.kind, &b.label, &b.name) && (pa - pb).abs() <= tol
            })
    }

    /// 弹窗（系统权限弹窗、App 自己的提示框）
    pub fn alert(&self) -> Option<&Element> {
        self.elements.iter().find(|e| e.kind == "Alert")
    }

    /// 点 `e` 时落在哪。WDA 不给可见性（太慢），只能按位置推：树里在它后面出现、又不是它后代、并且能盖住它的东西
    /// 画在它上面。单靠树里的先后会误判（单元格里的背景层排在按钮后面；导航栏排在滚到它底下的内容前面），所以按层限定：
    /// - 栏里的元素只会被键盘、弹窗、面板盖住
    /// - 可滚动内容里的元素会被内容外面的任何东西盖住（悬浮栏、浮层横幅）
    /// - 其他元素只会被浮在上面的层（工具栏、标签栏、键盘、弹窗、面板）盖住
    pub fn tap_point(&self, e: &Element) -> TapPoint {
        let can_cover = |b: &Blocker| {
            (e.group == 0 || b.group != e.group)
                && match e.layer {
                    Layer::Bar => b.modal,
                    Layer::Scroll => true,
                    Layer::Plain => b.floating,
                }
        };
        let cover =
            |p: (f64, f64)| self.blockers.iter().find(|b| b.span.0 > e.span.1 && can_cover(b) && b.rect.contains(p));
        let center = (e.center.0 as f64, e.center.1 as f64);
        let Some(first) = cover(center) else { return TapPoint::Center(center) };
        let v = e.visible;
        let inset = (v.w / 4.0).min(v.h / 4.0).min(4.0);
        let along =
            |start: f64, len: f64, n: usize, i: usize| start + inset + (len - 2.0 * inset) * i as f64 / (n - 1) as f64;
        let mut points: Vec<(f64, f64)> = (0..7)
            .flat_map(|i| (0..5).map(move |j| (i, j)))
            .map(|(i, j)| (along(v.x, v.w, 7, i), along(v.y, v.h, 5, j)))
            .collect();
        let d = |p: &(f64, f64)| (p.0 - center.0).powi(2) + (p.1 - center.1).powi(2);
        points.sort_by(|a, b| d(a).total_cmp(&d(b)));
        let by = if first.name.is_empty() { "没有文字的浮层".to_owned() } else { first.name.clone() };
        match points.into_iter().find(|p| cover(*p).is_none()) {
            Some(p) => TapPoint::Moved { at: (p.0.round(), p.1.round()), by },
            None => TapPoint::Covered { by },
        }
    }
}

/// 底部悬浮工具栏/标签栏（iOS 26 起的玻璃栏）实际接收点击的范围比元素树给的框往外多约 20 点。
/// 真机：设置首页搜索栏底板顶边 856，点 834 还能进下面那一行，838 就被栏吞掉（什么都不发生）；
/// 左右两边按同样的余量算
const BAR_SLOP: f64 = 24.0;

/// 可滚动的内容区
const SCROLLERS: [&str; 5] = ["ScrollView", "Table", "CollectionView", "WebView", "TextView"];
/// 盖在一切之上的层
const MODALS: [&str; 4] = ["Keyboard", "Alert", "Sheet", "Popover"];
/// 自成一组的层：组里的东西互相不算盖住（弹窗按钮后面排着弹窗自己的背景层）
const GROUPS: [&str; 7] = ["Toolbar", "TabBar", "NavigationBar", "Keyboard", "Alert", "Sheet", "Popover"];

/// 输入框类元素
const EDITABLE: [&str; 4] = ["TextField", "SearchField", "TextView", "SecureTextField"];
/// 子元素的文字已经包含在它们的文字里时，子元素不再单列
const CONTAINERS: [&str; 10] =
    ["Button", "Cell", "Link", "Icon", "MenuItem", "SearchField", "TextField", "Switch", "Tab", "Key"];

/// 只有开发者起的标识、没有给人看的文字：`label-view`、`launch_logo`、UUID 之类
fn looks_like_identifier(s: &str) -> bool {
    s.len() >= 3 && s.is_ascii() && !s.contains(char::is_whitespace) && s.contains(['_', '-', '.', ':'])
}

fn is_scroll_bar(kind: &str, label: &str) -> bool {
    kind == "Other" && (label.contains("滚动条") || label.to_lowercase().contains("scroll bar"))
}

/// 超长文字截断显示（筛选、按名字点仍按全文）
fn clip(s: &str, max: usize, keep: usize) -> String {
    let n = s.chars().count();
    if n <= max { s.to_owned() } else { format!("{}…(共 {n} 字)", s.chars().take(keep).collect::<String>()) }
}

impl Element {
    fn haystack(&self) -> String {
        format!("{} {} {}", self.label, self.name, self.value).to_lowercase()
    }

    /// 与 toolkit 相同的一行描述：`Button "显示与亮度" id="..." @(215,676) 390x53`。超长的文字和标识截断
    pub fn describe(&self) -> String {
        let text =
            [&self.label, &self.name, &self.value].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
        let mut s = format!("{} \"{}\"", self.kind, clip(&text, 80, 60));
        if !self.name.is_empty() && !self.label.is_empty() && self.name != self.label {
            let id = if self.name.chars().count() > 48 {
                format!("{}…", self.name.chars().take(40).collect::<String>())
            } else {
                self.name.clone()
            };
            s += &format!(" id=\"{id}\"");
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
    parse_tree(xml, screen).elements
}

/// 解析时每个还没闭合的节点
struct Open {
    abs: Rect,
    /// 在工具栏/标签栏里
    in_bar: bool,
    /// 在导航栏/工具栏/标签栏里
    on_bar: bool,
    /// 在可滚动的内容里
    in_scroll: bool,
    /// 在键盘、弹窗、面板里
    modal: bool,
    group: usize,
    /// 这一层子节点的原始坐标要加上的偏移（小组件内部用的是相对小组件的坐标）
    offset: (f64, f64),
    in_status_bar: bool,
    kind: String,
    kept: Option<usize>,
    blocker: Option<usize>,
}

pub fn parse_tree(xml: &str, screen: ScreenSize) -> Tree {
    let (sw, sh) = (screen.width, screen.height);
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
            Ok(Event::Eof) => break,
            Err(e) => {
                // 解析到一半出错：已解析的照常返回，但要留痕，否则「没找到元素」看起来像真结果
                tracing::warn!("元素树 XML 解析出错（位置 {}），只用了前面的部分：{e}", reader.buffer_position());
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

        // 小组件的内容：某个节点在 (0,0) 且和父节点一样大，而父节点不在 (0,0)——从它开始坐标是相对父节点的
        let parent = stack.last();
        let mut offset = parent.map_or((0.0, 0.0), |p| p.offset);
        if let Some(p) = parent
            && raw.x == 0.0
            && raw.y == 0.0
            && (raw.w - p.abs.w).abs() < 1.0
            && (raw.h - p.abs.h).abs() < 1.0
            && (p.abs.x != 0.0 || p.abs.y != 0.0)
        {
            offset = (p.abs.x, p.abs.y);
        }
        let abs = Rect { x: raw.x + offset.0, y: raw.y + offset.1, ..raw };
        let in_status_bar = kind == "StatusBar" || parent.is_some_and(|p| p.in_status_bar);
        let in_bar = matches!(kind.as_str(), "Toolbar" | "TabBar") || parent.is_some_and(|p| p.in_bar);
        let on_bar = in_bar || kind == "NavigationBar" || parent.is_some_and(|p| p.on_bar);
        // 子节点在可滚动内容里（容器自己不算：它就是那块区域）
        let in_scroll = parent.is_some_and(|p| p.in_scroll || SCROLLERS.contains(&p.kind.as_str()));
        let modal = MODALS.contains(&kind.as_str()) || parent.is_some_and(|p| p.modal);
        let group = if GROUPS.contains(&kind.as_str()) { n } else { parent.map_or(0, |p| p.group) };
        let onscreen =
            abs.w > 0.0 && abs.h > 0.0 && abs.x < sw && abs.y < sh && abs.x + abs.w > 0.0 && abs.y + abs.h > 0.0;
        let (vx1, vy1) = (abs.x.max(0.0), abs.y.max(0.0));
        let visible = Rect { x: vx1, y: vy1, w: (abs.x + abs.w).min(sw) - vx1, h: (abs.y + abs.h).min(sh) - vy1 };
        let text = [&label, &name, &value].into_iter().find(|s| !s.is_empty()).cloned();
        let scroll_bar = is_scroll_bar(&kind, &label);
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
                if in_bar && rect.y > sh * 0.6 {
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

        let kind_for_children = kind.clone();
        let kept = text.filter(|text| {
            let noise = !onscreen
                || (abs.w >= sw && abs.h >= sh)
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
            !noise && seen.insert((text.clone(), py_round(abs.x), py_round(abs.y), py_round(abs.w), py_round(abs.h)))
        });
        let kept = kept.map(|_| {
            tree.elements.push(Element {
                kind,
                label,
                name,
                value,
                center: (py_round(visible.x + visible.w / 2.0), py_round(visible.y + visible.h / 2.0)),
                size: (py_round(abs.w), py_round(abs.h)),
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
            });
            tree.elements.len() - 1
        });
        let o = Open {
            abs,
            in_bar,
            on_bar,
            in_scroll,
            modal,
            group,
            kind: kind_for_children,
            offset,
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

/// 类型/标签/名字/值里包含 `text` 的元素（不区分大小写）。列元素时筛选用，可以按类型筛（如 `SearchField`）
pub fn filter<'a>(elements: &'a [Element], text: &str) -> Vec<&'a Element> {
    let t = text.to_lowercase();
    elements.iter().filter(|e| e.haystack().contains(&t) || e.kind.to_lowercase().contains(&t)).collect()
}

/// 按文字找元素：先找标签或名字完全相同的，没有再找标签/名字/值包含该文字的（不看类型）
pub fn find<'a>(elements: &'a [Element], text: &str) -> Vec<&'a Element> {
    let exact: Vec<_> = elements.iter().filter(|e| e.label == text || e.name == text).collect();
    if !exact.is_empty() {
        return exact;
    }
    let t = text.to_lowercase();
    elements.iter().filter(|e| e.haystack().contains(&t)).collect()
}

/// 等某段文字出现时用：同 [`find`]，但不算输入框里的内容（刚输入的搜索词不能算「结果出来了」）
pub fn find_shown<'a>(elements: &'a [Element], text: &str) -> Vec<&'a Element> {
    let shown = |e: &Element| -> Vec<String> {
        if !EDITABLE.contains(&e.kind.as_str()) {
            return vec![e.label.clone(), e.name.clone(), e.value.clone()];
        }
        [&e.label, &e.name].into_iter().filter(|s| **s != e.value).cloned().collect()
    };
    let exact: Vec<_> = elements.iter().filter(|e| shown(e).iter().any(|s| s == text)).collect();
    if !exact.is_empty() {
        return exact;
    }
    let t = text.to_lowercase();
    elements.iter().filter(|e| shown(e).iter().any(|s| s.to_lowercase().contains(&t))).collect()
}

/// 屏幕上输入框里的内容，用来核对输入有没有丢字。有密码框时返回 None（值是圆点，核对不了）
pub fn field_values(elements: &[Element]) -> Option<Vec<String>> {
    if elements.iter().any(|e| e.kind == "SecureTextField") {
        return None;
    }
    Some(elements.iter().filter(|e| EDITABLE.contains(&e.kind.as_str())).map(|e| e.value.clone()).collect())
}

/// 锁屏密码键盘上 0～9 各键的中心点；没找齐 10 个返回 None。
/// 找到了也不代表键盘在屏幕上——按键后要核对密码栏的已输入位数。
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
    Some(out)
}

/// 密码键盘右下角的按钮：有输入时是「删除」（每按一下删一位），没输入时是「取消」。
/// 标签随语言变，按位置认：和 9 同一列、在 0 下面的 Button
pub fn delete_key(elements: &[Element], pad: &[(i64, i64); 10]) -> Option<(i64, i64)> {
    let (col, row0) = (pad[9].0, pad[0].1);
    elements
        .iter()
        .filter(|e| e.kind == "Button" && (e.center.0 - col).abs() <= 10 && e.center.1 > row0 + 50)
        .map(|e| e.center)
        .next()
}

/// 元素树里锁屏密码栏的 (总位数, 已输入位数)
pub fn passcode_field(elements: &[Element]) -> Option<(u32, u32)> {
    elements.iter().filter(|e| e.kind == "SecureTextField").find_map(|e| parse_passcode_value(&e.value))
}

/// 锁屏密码栏的值形如「共6位数，已输入2位」或英文「6-digit passcode, 2 entered」，返回 (总位数, 已输入位数)
pub fn parse_passcode_value(value: &str) -> Option<(u32, u32)> {
    let nums: Vec<u32> =
        regex::Regex::new(r"\d+").ok()?.find_iter(value).filter_map(|m| m.as_str().parse().ok()).collect();
    match nums[..] {
        [total, entered] => Some((total, entered)),
        _ => None,
    }
}

/// SpringBoard 元素树看到的是哪种「封面」界面。用来纠正 `/wda/locked`（见 [`crate::WdaClient::locked`]）。
/// 标识都与语言无关（iOS 27 真机样本）：
/// - 锁屏有锁图标 `proud-lock`（灵动岛里还有 `systemApertureElementIdentifierLock`），上滑后的密码键盘有 `passcode-view`
/// - 通知中心和锁屏是同一个界面（都有 `lockscreen-date-view`），但没有锁图标
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverSheet {
    Locked,
    NotificationCenter,
    None,
}

pub fn cover_sheet(xml: &str) -> CoverSheet {
    let lock_glyph =
        ["\"proud-lock\"", "systemApertureElementIdentifierLock", "\"passcode-view\""].iter().any(|m| xml.contains(m));
    if lock_glyph {
        CoverSheet::Locked
    } else if xml.contains("\"lockscreen-date-view\"") {
        CoverSheet::NotificationCenter
    } else {
        CoverSheet::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: ScreenSize = ScreenSize { width: 430.0, height: 932.0 };
    const HOME: &str = include_str!("../tests/fixtures/home.xml");
    const LOCK: &str = include_str!("../tests/fixtures/lockscreen.xml");
    const LOCK_IOS27: &str = include_str!("../tests/fixtures/lockscreen-ios27.xml");
    const PIN_IOS27: &str = include_str!("../tests/fixtures/passcode-view-ios27.xml");
    const NC_IOS27: &str = include_str!("../tests/fixtures/notification-center-ios27.xml");
    const PARTIAL_IOS27: &str = include_str!("../tests/fixtures/passcode-partial-ios27.xml");
    const SETTINGS_IOS27: &str = include_str!("../tests/fixtures/settings-floating-search-ios27.xml");
    const HOME_WIDGETS_IOS27: &str = include_str!("../tests/fixtures/home-widgets-ios27.xml");
    const WALLET_IOS27: &str = include_str!("../tests/fixtures/settings-wallet-ios27.xml");

    #[test]
    fn delete_key_found_by_position_on_real_keypad() {
        let els = parse(PARTIAL_IOS27, SCREEN);
        let pad = keypad(&els).unwrap();
        assert_eq!(delete_key(&els, &pad), Some((327, 857)));
        assert_eq!(passcode_field(&els), Some((6, 2)));
        // 「紧急情况」在左下，不能认成删除
        assert!(els.iter().any(|e| e.label == "紧急情况"));
    }

    #[test]
    fn cover_sheet_kinds_from_real_trees() {
        assert_eq!(cover_sheet(LOCK_IOS27), CoverSheet::Locked);
        // 密码键盘界面没有 lockscreen-date-view，也必须算锁屏（自动解锁靠它判断是否解开了）
        assert_eq!(cover_sheet(PIN_IOS27), CoverSheet::Locked);
        assert_eq!(cover_sheet(NC_IOS27), CoverSheet::NotificationCenter);
        assert_eq!(cover_sheet(HOME), CoverSheet::None);
        // 真机样本也要能正常解析
        for xml in [LOCK_IOS27, PIN_IOS27, NC_IOS27] {
            assert!(!parse(xml, SCREEN).is_empty());
        }
    }

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
    }

    #[test]
    fn scroll_bars_are_dropped() {
        assert!(!labels(&home()).iter().any(|l| l.contains("滚动条")));
        assert!(!labels(&parse(SETTINGS_IOS27, SCREEN)).iter().any(|l| l.contains("滚动条")));
    }

    /// 真机：设置首页往上滑后，「钱包与Apple Pay」那一行（832～886）被底部悬浮搜索栏盖住：
    /// 栏的底板从 856 起，实际吞点击的范围再往上约 20 点，露出来的部分点不中
    #[test]
    fn tap_point_avoids_floating_search_bar() {
        let t = parse_tree(SETTINGS_IOS27, SCREEN);
        let wallet = t.elements.iter().find(|e| e.label == "钱包与Apple Pay").unwrap();
        assert_eq!(wallet.center, (215, 859));
        assert_eq!(t.tap_point(wallet), TapPoint::Covered { by: "搜索".into() });
        // 中心在栏上方、下半截被盖住的：改点上半截
        let mut half = wallet.clone();
        half.visible = Rect { y: 810.0, ..half.visible };
        half.center = (215, 837);
        match t.tap_point(&half) {
            TapPoint::Moved { at, by } => {
                assert!(at.1 < 832.0 && at.1 >= 810.0, "{at:?}");
                assert_eq!(by, "搜索");
            }
            other => panic!("{other:?}"),
        }
        // 没被挡的照常点中心
        let wallpaper = t.elements.iter().find(|e| e.label == "相机").unwrap();
        assert_eq!(t.tap_point(wallpaper), TapPoint::Center((wallpaper.center.0 as f64, wallpaper.center.1 as f64)));
        // 搜索栏自己（在最上层）不算被挡
        let search = t.elements.iter().find(|e| e.kind == "SearchField").unwrap();
        assert!(matches!(t.tap_point(search), TapPoint::Center(_)));
    }

    /// 真机：设置 › 钱包与Apple Pay。单元格里的背景层排在按钮后面、滚到导航栏底下的行排在导航栏后面，都不算盖住
    #[test]
    fn decorations_and_content_under_nav_bar_do_not_cover() {
        let t = parse_tree(WALLET_IOS27, SCREEN);
        for label in ["通知、隐式推送", "设置"] {
            let e = t.elements.iter().find(|e| e.label == label).unwrap();
            assert!(matches!(t.tap_point(e), TapPoint::Center(_)), "{label}");
        }
    }

    /// 真机：天气的定位服务弹窗，按钮后面排着弹窗自己的背景层（没有文字）
    #[test]
    fn alert_buttons_are_not_covered_by_their_own_alert() {
        let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="430" height="932">
            <XCUIElementTypeAlert type="XCUIElementTypeAlert" label="打开定位服务" x="55" y="404" width="320" height="149">
              <XCUIElementTypeButton type="XCUIElementTypeButton" label="取消" x="219" y="488" width="140" height="49"/>
              <XCUIElementTypeOther type="XCUIElementTypeOther" x="55" y="404" width="320" height="149"/>
            </XCUIElementTypeAlert>
        </XCUIElementTypeApplication>"#;
        let t = parse_tree(xml, SCREEN);
        let cancel = t.elements.iter().find(|e| e.label == "取消").unwrap();
        assert!(matches!(t.tap_point(cancel), TapPoint::Center(_)));
    }

    #[test]
    fn tap_point_reports_fully_covered() {
        let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="430" height="932">
            <XCUIElementTypeScrollView type="XCUIElementTypeScrollView" x="0" y="0" width="430" height="932">
              <XCUIElementTypeButton type="XCUIElementTypeButton" label="底下的按钮" x="100" y="500" width="100" height="40"/>
            </XCUIElementTypeScrollView>
            <XCUIElementTypeOther type="XCUIElementTypeOther" x="0" y="450" width="430" height="200">
              <XCUIElementTypeStaticText type="XCUIElementTypeStaticText" label="浮层" x="10" y="460" width="60" height="20"/>
            </XCUIElementTypeOther>
        </XCUIElementTypeApplication>"#;
        let t = parse_tree(xml, SCREEN);
        let b = t.elements.iter().find(|e| e.label == "底下的按钮").unwrap();
        assert_eq!(t.tap_point(b), TapPoint::Covered { by: "浮层".into() });
    }

    /// 真机：主屏天气小组件里的文字，WDA 给的是相对小组件的坐标
    #[test]
    fn widget_contents_are_moved_to_screen_coordinates() {
        let els = parse(HOME_WIDGETS_IOS27, SCREEN);
        let city = els.iter().find(|e| e.label == "上海市").unwrap();
        // 小组件内容区在 (29,84)，文字在其中 (19,19) 起 52x13
        assert_eq!(city.center, (74, 110));
        // 主屏图标本来就是屏幕坐标，不受影响
        let clock = els.iter().find(|e| e.label == "时钟" && e.kind == "Icon").unwrap();
        assert!(clock.center.1 > 400);
    }

    #[test]
    fn close_to_tolerates_small_drift_only() {
        let t = parse_tree(SETTINGS_IOS27, SCREEN);
        let mut moved = t.clone();
        for e in &mut moved.elements {
            e.center.1 += 2;
        }
        assert!(t.close_to(&moved, 2));
        assert!(!t.close_to(&moved, 1));
        moved.elements.pop();
        assert!(!t.close_to(&moved, 2));
    }

    #[test]
    fn still_along_ignores_values_and_the_other_axis() {
        let t = parse_tree(SETTINGS_IOS27, SCREEN);
        let mut carousel = t.clone();
        carousel.elements[0].value = "第 2 页".into();
        carousel.elements[0].center.0 += 100;
        assert!(t.still_along(&carousel, true, 3));
        assert!(!t.still_along(&carousel, false, 3));
        let mut scrolled = t.clone();
        scrolled.elements[0].center.1 += 10;
        assert!(!t.still_along(&scrolled, true, 3));
    }

    #[test]
    fn noise_is_dropped_but_tap_targets_stay() {
        let h = parse(HOME_WIDGETS_IOS27, SCREEN);
        let l = labels(&h);
        assert!(!l.iter().any(|s| s == "label-view"));
        // 状态栏（时间、信号、电量）
        assert!(!h.iter().any(|e| e.label.contains("电池电量")));
        assert!(h.iter().any(|e| e.kind == "Icon" && e.label == "设置"));
        let s = parse(SETTINGS_IOS27, SCREEN);
        // 按钮里重复按钮文字的 StaticText 和右边的箭头图标不再单列
        assert_eq!(s.iter().filter(|e| e.label == "钱包与Apple Pay").count(), 1);
        assert!(!s.iter().any(|e| e.label == "chevron.forward"));
        assert!(s.iter().any(|e| e.kind == "Button" && e.label == "通知"));
    }

    #[test]
    fn long_text_and_ids_are_clipped_in_description() {
        let e = Element {
            kind: "Button".into(),
            label: "很长".repeat(100),
            name: format!("Open.{}", "X".repeat(100)),
            ..Default::default()
        };
        let d = e.describe();
        assert!(d.contains("…(共 200 字)") && d.chars().count() < 160, "{d}");
        assert!(filter(std::slice::from_ref(&e), "很长很长").len() == 1, "筛选仍按全文");
    }

    #[test]
    fn filter_matches_type_but_find_does_not() {
        let s = parse(SETTINGS_IOS27, SCREEN);
        assert_eq!(filter(&s, "searchfield").len(), 1);
        assert!(find(&s, "SearchField").is_empty());
    }

    #[test]
    fn find_shown_ignores_text_typed_into_fields() {
        let els = vec![
            Element {
                kind: "TextView".into(),
                label: "红烧肉做法".into(),
                value: "红烧肉做法".into(),
                ..Default::default()
            },
            Element {
                kind: "SearchField".into(), label: "搜索".into(), value: "红烧肉".into(), ..Default::default()
            },
        ];
        assert!(find_shown(&els, "红烧肉").is_empty());
        assert_eq!(find_shown(&els, "搜索").len(), 1);
        let mut more = els.clone();
        more.push(Element { kind: "StaticText".into(), label: "家常红烧肉".into(), ..Default::default() });
        assert_eq!(find_shown(&more, "红烧肉")[0].label, "家常红烧肉");
    }

    #[test]
    fn field_values_skip_password_forms() {
        let field = |kind: &str, v: &str| Element { kind: kind.into(), value: v.into(), ..Default::default() };
        assert_eq!(field_values(&[field("SearchField", "故宫博")]), Some(vec!["故宫博".to_owned()]));
        assert_eq!(field_values(&[field("TextField", "me"), field("SecureTextField", "••")]), None);
    }

    #[test]
    fn loading_indicator_and_alert_are_reported() {
        let xml = r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="430" height="932">
            <XCUIElementTypeActivityIndicator type="XCUIElementTypeActivityIndicator" x="200" y="450" width="30" height="30"/>
            <XCUIElementTypeAlert type="XCUIElementTypeAlert" label="“微信”想给你发送通知" x="55" y="390" width="320" height="173"/>
        </XCUIElementTypeApplication>"#;
        let t = parse_tree(xml, SCREEN);
        assert!(t.loading);
        assert_eq!(t.alert().unwrap().label, "“微信”想给你发送通知");
        assert!(!parse_tree(HOME, SCREEN).loading);
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
        assert_eq!(
            d("显示与亮度"),
            r#"Button "显示与亮度" id="com.apple.settings.displayAndBrightness" @(215,676) 390x53"#
        );
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
        Element {
            kind: kind.into(),
            label: label.into(),
            name: label.into(),
            center: (x, 100),
            size: (80, 80),
            ..Default::default()
        }
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
        // 锁屏通知上的「3条通知」不能占掉 3 的位置
        let mut keys = vec![key("3条通知", 99, "Button")];
        keys.extend((0..10).map(|d| key(&d.to_string(), d, "Key")));
        assert_eq!(keypad(&keys).unwrap()[3], (3, 100));
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
