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
//!
//! 分在几个文件里：`geometry`（矩形、方向、坐标换算）、`parse`（解析 XML）、`query`（在树里找、点哪里）、`lock`（锁屏的
//! 键盘、密码栏、封面）。类型都在这里，各文件里是它们的实现

mod geometry;
mod lock;
mod parse;
mod query;
#[cfg(test)]
mod tests;

pub use lock::{
    CoverSheet, cover_sheet, delete_key, keypad, parse_passcode_value, passcode_field, use_passcode_button,
};
pub use parse::{parse, parse_tree};
pub use query::{field_values, filter, find, find_shown};

use std::sync::LazyLock;

use ictl_device::ScreenSize;
use regex::Regex;

/// The patterns are compile-time constants, only checked by the tests that use them
fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid pattern")
}

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

/// 界面方向，来自 WDA 的 `GET /session/:id/orientation`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Portrait,
    PortraitUpsideDown,
    /// WDA 报 `LANDSCAPE`
    LandscapeLeft,
    /// WDA 报 `UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT`
    LandscapeRight,
}

/// 解析元素树要知道的：当前方向下的屏幕尺寸，和方向（不知道时为 None，不做坐标换算）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub screen: ScreenSize,
    pub orientation: Option<Orientation>,
}

/// 元素树坐标到屏幕坐标的换算。iPad 横屏时 SpringBoard（主屏、长按菜单、锁屏）给的是竖屏坐标：
/// 窗口 768x1024，屏幕 1024x768；App 里给的是屏幕坐标（iPadOS 26.6.1 真机）。点击用的是屏幕坐标。
/// 往哪边转看状态栏：它在竖屏坐标里是一条贴边的竖条，贴哪边，哪边就是屏幕顶边。
/// WDA 报的方向靠不住：iPad mini 5 两个横屏方向在主屏上都报 `LANDSCAPE`（rotation z=90）
#[derive(Debug, Clone, Copy, PartialEq)]
enum Rotate {
    None,
    /// 竖屏的顶边转到了屏幕左边（Home 键在右）：(x, y) → (y, 屏幕高 - x)
    TopToLeft,
    /// 竖屏的顶边转到了屏幕右边（Home 键在左）：(x, y) → (屏幕宽 - y, x)
    TopToRight,
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
    /// 所在的可滚动区域（屏幕坐标）；不在可滚动内容里时为 None
    scroller: Option<Rect>,
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

/// 可能盖住别的元素的东西，包括没有文字的容器（例如悬浮搜索栏的底板、键盘）。
/// 可滚动内容里的节点不算（单元格的背景层、滚到导航栏底下的行）
#[derive(Debug, Clone)]
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
    /// Some nodes may not have been observed. Missing text on this tree does not prove absence.
    pub incomplete: bool,
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
