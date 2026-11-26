//! 在解析好的树里找：按文字找元素、点哪里、和上一次读的比

use super::parse::EDITABLE;
use super::*;

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

    /// [`Self::still_along`], judged on the elements both reads have: a list that scrolls moves cells in and out at its
    /// edges, so the two lists differ while it moves and right after it stops. Matched by type, label and name, in
    /// order of appearance; at least half of the smaller read (and 3) must match, and all of them stand still
    pub fn still_along_common(&self, o: &Tree, vertical: bool, tol: i64) -> bool {
        fn key(e: &Element) -> (&str, &str, &str) {
            (e.kind.as_str(), e.label.as_str(), e.name.as_str())
        }
        let mut theirs: std::collections::HashMap<(&str, &str, &str), std::collections::VecDeque<&Element>> =
            std::collections::HashMap::new();
        for e in &o.elements {
            theirs.entry(key(e)).or_default().push_back(e);
        }
        let mut matched = 0usize;
        for a in &self.elements {
            let Some(b) = theirs.get_mut(&key(a)).and_then(|q| q.pop_front()) else { continue };
            let (pa, pb) = if vertical { (a.center.1, b.center.1) } else { (a.center.0, b.center.0) };
            if (pa - pb).abs() > tol {
                return false;
            }
            matched += 1;
        }
        let smaller = self.elements.len().min(o.elements.len());
        matched >= 3 && matched * 2 >= smaller
    }

    /// 弹窗（系统权限弹窗、App 自己的提示框）
    pub fn alert(&self) -> Option<&Element> {
        self.elements.iter().find(|e| e.kind == "Alert")
    }

    /// 点 `e` 时落在哪。WDA 不给可见性（太慢），只能按位置推：树里在它后面出现、又不是它后代、并且能盖住它的东西
    /// 画在它上面。单靠树里的先后会误判（单元格里的背景层排在按钮后面；导航栏排在滚到它底下的内容前面），所以按层限定：
    /// - 栏里的元素只会被键盘、弹窗、面板盖住
    /// - 可滚动内容里的元素会被内容外面的任何东西盖住（悬浮栏、浮层横幅），但和整个滚动区一样大或更大的非模态层不算：
    ///   那是滚动区的背景、边缘效果（iPadOS 26 浮动侧栏的列表后面跟着几层和侧栏一样大的空容器），不是盖在上面的东西
    /// - 其他元素只会被浮在上面的层（工具栏、标签栏、键盘、弹窗、面板）盖住
    pub fn tap_point(&self, e: &Element) -> TapPoint {
        let can_cover = |b: &Blocker| {
            (e.group == 0 || b.group != e.group)
                && match e.layer {
                    Layer::Bar => b.modal,
                    Layer::Scroll => b.modal || !e.scroller.is_some_and(|s| b.rect.covers(&s)),
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
        let by = if first.name.is_empty() {
            ictl_l10n::tr!("没有文字的浮层", "an overlay without text").to_owned()
        } else {
            first.name.clone()
        };
        match points.into_iter().find(|p| cover(*p).is_none()) {
            Some(p) => TapPoint::Moved { at: (p.0.round(), p.1.round()), by },
            None => TapPoint::Covered { by },
        }
    }
}

/// 超长文字截断显示（筛选、按名字点仍按全文）
pub(super) fn clip(s: &str, max: usize, keep: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_owned()
    } else {
        ictl_l10n::trf!("{}…(共 {n} 字)", "{}…({n} characters)", s.chars().take(keep).collect::<String>())
    }
}

impl Element {
    /// Inside a scroll area's content (a list's rows, a page): what a swipe moves. Bars, a scroll area itself and
    /// what lies outside one don't move with it. A tree read natively has no scroll areas: nothing is
    pub fn in_scroll(&self) -> bool {
        self.layer == Layer::Scroll
    }

    /// `needle` is already lowercase so a page of elements can share it.
    pub fn matches_filter(&self, needle: &str) -> bool {
        self.contains_text(needle) || self.kind.to_lowercase().contains(needle)
    }

    pub fn contains_text(&self, needle: &str) -> bool {
        needle.is_empty() || self.haystack().contains(needle)
    }

    fn haystack(&self) -> String {
        format!("{} {} {}", self.label, self.name, self.value).to_lowercase()
    }

    /// 与 toolkit 相同的一行描述：`Button "显示与亮度" id="..." @(215,676) 390x53`。超长的文字和标识截断
    pub fn describe(&self) -> String {
        format!("{} @({},{}) {}x{}", self.describe_text(), self.center.0, self.center.1, self.size.0, self.size.1)
    }

    /// Text and identifier without a position. Suitable for rows collected from earlier viewports.
    pub fn describe_text(&self) -> String {
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
        s
    }
}

/// 类型/标签/名字/值里包含 `text` 的元素（不区分大小写）。列元素时筛选用，可以按类型筛（如 `SearchField`）
pub fn filter<'a>(elements: &'a [Element], text: &str) -> Vec<&'a Element> {
    let t = text.to_lowercase();
    elements.iter().filter(|e| e.matches_filter(&t)).collect()
}

/// 按文字找元素：先找标签或名字完全相同的，没有再找标签/名字/值包含该文字的（不看类型）
pub fn find<'a>(elements: &'a [Element], text: &str) -> Vec<&'a Element> {
    let exact: Vec<_> = elements.iter().filter(|e| e.label == text || e.name == text).collect();
    if !exact.is_empty() {
        return exact;
    }
    let t = text.to_lowercase();
    elements.iter().filter(|e| e.contains_text(&t)).collect()
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
