//! Collect visible text as a list scrolls, retaining the final viewport for a follow-up action.

use super::*;
use crate::protocol::{Data, ScrollCollect, ScrollMatch, ScrollRow, ScrollStop};
use elements::Element;

impl Daemon {
    pub(crate) async fn scroll_collect(
        &self,
        g: &mut Inner,
        scan: &ScrollCollect,
        mut may_continue: impl FnMut() -> bool,
    ) -> crate::error::Result<(String, Data)> {
        self.ensure_unlocked(g).await?;
        if let Some(x) = scan.x {
            let width = self.screen(g).await.map_err(Error::preflight)?.width;
            if x > width {
                return Err(Error::preflight(ictl_l10n::trf!(
                    "x 超出了屏幕宽度（{} 点），什么都没做",
                    "x is past the screen's width ({} points); nothing was done",
                    width
                )));
            }
        }
        let filter = scan.filter.to_lowercase();
        let until = scan.until.to_lowercase();
        let limit = scan.limit;
        let mut tree = self.tree(g, Duration::ZERO).await?;
        let mut previous = None;
        let mut rows = Vec::new();
        let mut lines = Vec::new();
        let mut scrolls = 0;
        loop {
            let page = visible_page(&tree, scan.down);
            let matched = visible_match(&page, &until);
            let before = previous.as_ref().map(|t| visible_page(t, scan.down)).unwrap_or_default();
            for e in new_rows(&before, &page).into_iter().filter(|e| e.matches_filter(&filter)).take(limit - rows.len())
            {
                lines.push(e.describe_text());
                rows.push(ScrollRow {
                    kind: e.kind.clone(),
                    label: e.label.clone(),
                    name: e.name.clone(),
                    value: e.value.clone(),
                });
            }
            let end = previous.as_ref().is_some_and(|p| unchanged_viewport(p, &tree));
            if let Some(reason) = stop_reason(matched.is_some(), rows.len() == limit, end, scrolls == scan.max_scrolls)
                .or_else(|| (!may_continue()).then_some(ScrollStop::Interrupted))
            {
                let why = match reason {
                    ScrollStop::Interrupted => ictl_l10n::tr!("滚动已中断", "collection interrupted"),
                    ScrollStop::Found => ictl_l10n::tr!("找到目标", "found the target"),
                    ScrollStop::End => {
                        ictl_l10n::tr!("滚动后可见内容没有变化", "visible content stayed the same after scrolling")
                    }
                    ScrollStop::Limit => ictl_l10n::tr!("已达到行数上限", "row limit reached"),
                    ScrollStop::MaxScrolls => ictl_l10n::tr!("已达到滚动次数上限", "scroll limit reached"),
                };
                let n = rows.len();
                let mut text = ictl_l10n::trf!(
                    "滑动 {scrolls} 次，收集 {n} 行，{why}",
                    "Scrolled {scrolls} times, collected {n} rows, {why}"
                );
                let matched = matched.map(|e| {
                    let point = match tree.tap_point(e) {
                        TapPoint::Center(p) | TapPoint::Moved { at: p, .. } => p,
                        TapPoint::Covered { .. } => unreachable!("visible_page excludes covered rows"),
                    };
                    text += &format!("\n{} @({},{})", e.describe_text(), point.0, point.1);
                    let r = e.visible;
                    let element = ElementData { x: r.x, y: r.y, width: r.w, height: r.h, ..element_data(e) };
                    Box::new(ScrollMatch { element, point })
                });
                for line in lines {
                    text.push('\n');
                    text.push_str(&line);
                }
                text += &self.snapshot_note(g);
                return Ok((text, Data::ScrollCollection { stop_reason: reason, scrolls, rows, matched }));
            }
            previous = Some(tree);
            tree = Box::pin(self.search_scroll(g, scan.down, scan.x)).await?;
            scrolls += 1;
        }
    }
}

/// A truncated read can omit the rows that moved. Only two complete, populated reads can show the end.
pub(super) fn unchanged_viewport(before: &Tree, after: &Tree) -> bool {
    !before.incomplete && !after.incomplete && !after.elements.is_empty() && after.close_to(before, STILL_TOLERANCE)
}

fn stop_reason(found: bool, row_limit: bool, end: bool, scroll_limit: bool) -> Option<ScrollStop> {
    if found {
        Some(ScrollStop::Found)
    } else if row_limit {
        Some(ScrollStop::Limit)
    } else if end {
        Some(ScrollStop::End)
    } else if scroll_limit {
        Some(ScrollStop::MaxScrolls)
    } else {
        None
    }
}

/// The visible elements in travel order: row by row, each row left to right
fn visible_page(tree: &Tree, down: bool) -> Vec<&Element> {
    let along = |e: &Element| if down { row_y(e) } else { -row_y(e) };
    let mut page: Vec<_> =
        tree.elements.iter().filter(|e| !matches!(tree.tap_point(e), TapPoint::Covered { .. })).collect();
    page.sort_by(|a, b| along(a).total_cmp(&along(b)).then(a.center.0.cmp(&b.center.0)));
    // One row's elements (an iPad's Cell, its label and its chevron) lie a point apart along the swipe, in an order
    // that changed as the list moved, so no two viewports overlapped (Settings, iPad mini 5): they're one row
    let mut rows: Vec<Vec<&Element>> = Vec::new();
    for e in page {
        match rows.last_mut() {
            Some(row) if along(e) - along(row[0]) <= ROW_SLOP => row.push(e),
            _ => rows.push(vec![e]),
        }
    }
    rows.into_iter()
        .flat_map(|mut row| {
            row.sort_by_key(|e| e.center.0);
            row
        })
        .collect()
}

/// How far apart along a swipe the elements of one row may lie ([`visible_page`])
const ROW_SLOP: f64 = 4.0;

/// The middle of an element's whole frame along a swipe: one the screen's edge cuts off still lines up with the rest of
/// its row
fn row_y(e: &Element) -> f64 {
    let (v, h) = (e.visible, e.size.1 as f64);
    if v.y <= 0.0 && v.h < h { v.y + v.h - h / 2.0 } else { v.y + h / 2.0 }
}

fn visible_match<'a>(page: &[&'a Element], until: &str) -> Option<&'a Element> {
    if until.is_empty() {
        return None;
    }
    page.iter().copied().find(|e| e.contains_text(until))
}

fn key(e: &Element) -> (&str, &str, &str, &str) {
    (&e.kind, &e.name, &e.label, &e.value)
}

/// How many elements may lie between the overlap of two viewports and the ends it is looked for at ([`new_rows`])
const SKIP: usize = 3;

/// Only a scroll area's content moves with a swipe. Bars and the rest (a navigation bar and the search field in it, the
/// list's own container, a dimming overlay) stay or change on their own anywhere in travel order: an iPad sidebar's
/// rows pass under its search field, so the viewports' rows didn't line up. The rows are matched without them, and
/// they're new only when the previous viewport had nothing like them. A tree read natively has no scroll areas: it's
/// matched whole
fn new_rows<'a>(before: &[&Element], page: &[&'a Element]) -> Vec<&'a Element> {
    if !page.iter().any(|e| e.in_scroll()) {
        return overlap_rows(before, page);
    }
    let rows = overlap_rows(
        &before.iter().copied().filter(|e| e.in_scroll()).collect::<Vec<_>>(),
        &page.iter().copied().filter(|e| e.in_scroll()).collect::<Vec<_>>(),
    );
    page.iter()
        .copied()
        .filter(|e| {
            if e.in_scroll() {
                rows.iter().any(|r| std::ptr::eq(*r, *e))
            } else {
                !before.iter().any(|b| key(b) == key(e))
            }
        })
        .collect()
}

/// Fixed headers and footers recur at the same location. After removing those, consecutive viewports
/// share a suffix/prefix in travel order. Comparing sequences preserves identical labels in distinct rows.
fn overlap_rows<'a>(before: &[&Element], page: &[&'a Element]) -> Vec<&'a Element> {
    let (before, page) = without_still_column(before, page);
    let fixed = |(a, b): (&&Element, &&Element)| key(a) == key(b) && a.center == b.center;
    let head = before.iter().zip(&page).take_while(|pair| fixed(*pair)).count();
    let (before, page) = (&before[head..], &page[head..]);
    let tail = before.iter().rev().zip(page.iter().rev()).take_while(|pair| fixed(*pair)).count();
    let (before, page) = (&before[..before.len() - tail], &page[..page.len() - tail]);
    // The longest run of the previous viewport's end at the start of this one. A few elements may lie outside it: a
    // large title collapses into the navigation bar as a list moves (Settings, iOS 27), so the next viewport starts
    // with the title; scrolling back up to the top, the previous viewport ends with it
    let mut best = (0, 0, 0); // the overlap, skipped at the end of `before`, skipped at the start of `page`
    for j in 0..=SKIP.min(before.len()) {
        let b = &before[..before.len() - j];
        for k in 0..=SKIP.min(page.len()) {
            let p = &page[k..];
            let n = (1..=b.len().min(p.len()))
                .rev()
                .find(|&n| b[b.len() - n..].iter().zip(&p[..n]).all(|(x, y)| key(x) == key(y)))
                .unwrap_or(0);
            if n > best.0 {
                best = (n, j, k);
            }
        }
    }
    let (n, j, k) = best;
    if n == 0 {
        return page.to_vec();
    }
    // skipped at the start of this viewport, above the overlap: new only if the previous one had nothing like it
    let lead = page[..k].iter().copied().filter(|e| !before.iter().any(|b| key(b) == key(e)));
    // skipped at the end of the previous viewport: the same element moved (the title), not a new one, when it turns up
    let mut moved: Vec<_> = before[before.len() - j..].iter().map(|e| key(e)).collect();
    let rest = page[k + n..].iter().copied().filter(|e| match moved.iter().position(|m| *m == key(e)) {
        Some(i) => {
            moved.swap_remove(i);
            false
        }
        None => true,
    });
    lead.chain(rest).collect()
}

/// An iPad split view scrolls one column; the other keeps its rows where they were, interleaved with the scrolled
/// column's in travel order, so the two viewports share no suffix and prefix and everything read as new (Settings,
/// iPad mini 5 in landscape: the 150-row limit after two scrolls). Leaves out of both the elements that kept their
/// place outside every column that moved: where elements seen in both viewports are at another place now
fn without_still_column<'b, 'a>(before: &[&'b Element], page: &[&'a Element]) -> (Vec<&'b Element>, Vec<&'a Element>) {
    fn span(e: &Element) -> (f64, f64) {
        (e.visible.x, e.visible.x + e.visible.w)
    }
    fn stayed(e: &Element, other: &[&Element]) -> bool {
        other.iter().any(|o| key(o) == key(e) && o.center == e.center)
    }
    let moved: Vec<(f64, f64)> = page
        .iter()
        .filter(|e| before.iter().any(|b| key(b) == key(e)) && !stayed(e, before))
        .map(|e| span(e))
        .collect();
    if moved.is_empty() {
        return (before.to_vec(), page.to_vec());
    }
    let still = |e: &Element, other: &[&Element]| {
        let (left, right) = span(e);
        stayed(e, other) && !moved.iter().any(|&(l, r)| left < r && l < right)
    };
    (
        before.iter().copied().filter(|e| !still(e, page)).collect(),
        page.iter().copied().filter(|e| !still(e, before)).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(labels: &[(&str, i64)]) -> Tree {
        let mut xml = String::from("<App>");
        for (label, y) in labels {
            xml += &format!(
                r#"<XCUIElementTypeCell type="XCUIElementTypeCell" label="{label}" x="10" y="{y}" width="200" height="30"/>"#
            );
        }
        xml += "</App>";
        elements::parse_tree(&xml, ScreenSize { width: 430.0, height: 932.0 })
    }

    #[test]
    fn overlap_preserves_separate_rows_in_both_directions_and_skips_fixed_content() {
        let a = tree(&[("Header", 0), ("A", 100), ("Repeat", 180), ("B", 260), ("Footer", 800)]);
        let b = tree(&[("Header", 0), ("Repeat", 100), ("B", 180), ("Repeat", 260), ("C", 340), ("Footer", 800)]);
        let down = new_rows(&visible_page(&a, true), &visible_page(&b, true));
        assert_eq!(down.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["Repeat", "C"]);
        let up = new_rows(&visible_page(&b, false), &visible_page(&a, false));
        assert_eq!(up.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["A"]);
        assert!(new_rows(&visible_page(&a, true), &visible_page(&a, true)).is_empty());
    }

    /// Elements as `(type, label, x, y, width)`, 30 high, on a screen of `size`
    fn screen(size: (f64, f64), items: &[(&str, &str, i64, i64, i64)]) -> Tree {
        let mut xml = String::from("<App>");
        for (kind, label, x, y, w) in items {
            xml += &format!(
                r#"<XCUIElementType{kind} type="XCUIElementType{kind}" label="{label}" x="{x}" y="{y}" width="{w}" height="30"/>"#
            );
        }
        xml += "</App>";
        elements::parse_tree(&xml, ScreenSize { width: size.0, height: size.1 })
    }

    fn labels<'a>(rows: &[&'a Element]) -> Vec<&'a str> {
        rows.iter().map(|e| e.label.as_str()).collect()
    }

    /// Settings' large title collapses into the navigation bar as the list moves (iPhone 15 Pro Max, iOS 27.2.1): the
    /// next viewport starts with the title somewhere else, then the rows the previous one ended with. Those were
    /// collected again (个人热点, 电池, VPN, 通用 and 无障碍 twice); scrolling back up, the title opens out again
    #[test]
    fn a_title_collapsing_into_the_navigation_bar_does_not_repeat_the_overlap() {
        let phone = (430.0, 932.0);
        let row = |label, y| ("Button", label, 10, y, 410);
        let top = screen(
            phone,
            &[
                ("StaticText", "设置", 20, 100, 129),
                row("A", 200),
                row("B", 260),
                row("C", 320),
                row("D", 380),
                row("E", 440),
                row("F", 500),
                ("SearchField", "搜索", 10, 850, 410),
            ],
        );
        let next = screen(
            phone,
            &[
                ("StaticText", "设置", 150, 50, 129),
                row("D", 140),
                row("E", 200),
                row("F", 260),
                row("G", 320),
                row("H", 380),
                ("SearchField", "搜索", 10, 850, 410),
            ],
        );
        assert_eq!(labels(&new_rows(&visible_page(&top, true), &visible_page(&next, true))), ["G", "H"]);
        assert_eq!(labels(&new_rows(&visible_page(&next, false), &visible_page(&top, false))), ["C", "B", "A"]);
    }

    /// An iPad split view scrolls the column swiped in; the sidebar's rows stay put, interleaved with the moving ones
    /// in travel order (Settings, iPad mini 5 in landscape: the 150-row limit after two scrolls, everything twice)
    #[test]
    fn a_column_that_did_not_scroll_is_not_collected_again() {
        let ipad = (1024.0, 768.0);
        let side =
            |i: i64| ("Button", ["S1", "S2", "S3", "S4", "S5", "S6", "S7", "S8"][i as usize], 20, 100 + 50 * i, 290);
        let pane = |n: i64, y| {
            (
                "Button",
                ["R0", "R1", "R2", "R3", "R4", "R5", "R6", "R7", "R8", "R9", "R10", "R11"][n as usize],
                340,
                y,
                660,
            )
        };
        let mut first: Vec<_> = (0..8).map(side).collect();
        first.extend((1..=8).map(|n| pane(n, 50 + 50 * n)));
        let mut second: Vec<_> = (0..8).map(side).collect();
        second.extend((4..=11).map(|n| pane(n, 50 * (n - 2))));
        let (first, second) = (screen(ipad, &first), screen(ipad, &second));
        assert_eq!(labels(&new_rows(&visible_page(&first, true), &visible_page(&second, true))), ["R9", "R10", "R11"]);
        // the sidebar scrolled instead (`x` inside it), the pane stayed
        let mut first: Vec<_> = (1..=8).map(|n| pane(n, 50 + 50 * n)).collect();
        first.extend((0..5).map(|i| ("Button", ["S1", "S2", "S3", "S4", "S5"][i as usize], 20, 100 + 50 * i, 290)));
        let mut second: Vec<_> = (1..=8).map(|n| pane(n, 50 + 50 * n)).collect();
        second.extend((0..5).map(|i| ("Button", ["S3", "S4", "S5", "S6", "S7"][i as usize], 20, 100 + 50 * i, 290)));
        let (first, second) = (screen(ipad, &first), screen(ipad, &second));
        assert_eq!(labels(&new_rows(&visible_page(&first, true), &visible_page(&second, true))), ["S6", "S7"]);
    }

    /// An iPad split view as WDA reads it (Settings, iPad mini 5, iPadOS 26.7.3, landscape), in its document order: the
    /// sidebar's navigation bar with the search field, the sidebar's rows in a CollectionView, the detail pane's
    /// navigation bar, its rows in a Table, each a Cell with its label and a chevron. The rows are `(label, top)`
    fn split_view(side: &[(String, f64)], pane: &[(String, f64)]) -> Tree {
        fn el(kind: &str, label: &str, (x, y, w, h): (f64, f64, f64, f64), inner: &str) -> String {
            format!(
                r#"<XCUIElementType{kind} type="XCUIElementType{kind}" name="{label}" label="{label}" x="{x}" y="{y}" width="{w}" height="{h}">{inner}</XCUIElementType{kind}>"#
            )
        }
        let side: String = side.iter().map(|(label, y)| el("Button", label, (26.0, *y, 288.0, 50.0), "")).collect();
        let pane: String = pane
            .iter()
            .map(|(label, y)| {
                let inner = el("Button", label, (362.0, y + 12.0, 97.0, 28.0), "")
                    + &el("Button", "chevron", (979.0, y + 19.0, 9.0, 13.0), "");
                el("Cell", label, (346.0, *y, 662.0, 51.0), &inner)
            })
            .collect();
        let search = el("SearchField", "搜索", (26.0, 87.0, 288.0, 38.0), "");
        let screen = [
            el("NavigationBar", "设置", (10.0, 32.0, 320.0, 108.0), &search),
            el("CollectionView", "边栏", (10.0, 32.0, 320.0, 726.0), &side),
            el("NavigationBar", "通用", (0.0, 32.0, 1024.0, 54.0), ""),
            el("Table", "", (346.0, 0.0, 678.0, 768.0), &pane),
        ]
        .concat();
        let xml = el("Application", "设置", (0.0, 0.0, 1024.0, 768.0), &screen);
        elements::parse_tree(&xml, ScreenSize { width: 1024.0, height: 768.0 })
    }

    /// Rows `{prefix}{n}` for `n` in `ns`, the first at `top`, `pitch` apart
    fn numbered(prefix: &str, ns: std::ops::RangeInclusive<usize>, top: f64, pitch: f64) -> Vec<(String, f64)> {
        let first = *ns.start();
        ns.map(|n| (format!("{prefix}{n}"), top + pitch * (n - first) as f64)).collect()
    }

    fn described(rows: &[&Element]) -> Vec<String> {
        rows.iter().map(|e| format!("{} {}", e.kind, e.label)).collect()
    }

    /// Through WDA an iPad split view has scroll areas (Settings, iPad mini 5, iPadOS 26.7.3, landscape). A detail
    /// row's Cell, label and chevron have middles a point apart, in an order that turned round as the list moved; the
    /// sidebar's navigation bar and search field stay put while its rows pass under them, between the rows in travel
    /// order. No two viewports overlapped: every swipe collected all it saw again (the 150-row limit after three)
    #[test]
    fn rows_are_matched_in_the_scroll_area_one_row_at_a_time() {
        // swiped in the middle: the detail pane moves 251 points, the sidebar stays
        let side = numbered("S", 1..=12, 140.0, 50.0);
        let first = split_view(&side, &numbered("R", 1..=9, 297.0, 51.0));
        let second = split_view(&side, &numbered("R", 1..=14, 46.0, 51.0));
        let want: Vec<_> =
            (10..=14).flat_map(|n| [format!("Button R{n}"), format!("Cell R{n}"), "Button chevron".into()]).collect();
        assert_eq!(described(&new_rows(&visible_page(&first, true), &visible_page(&second, true))), want);
        assert!(new_rows(&visible_page(&second, false), &visible_page(&first, false)).is_empty());
        // swiped in the sidebar (`x` inside it): its rows move 253 points, two of them above its search field
        let pane = numbered("R", 1..=9, 297.0, 51.0);
        let first = split_view(&numbered("S", 1..=12, 140.0, 50.0), &pane);
        let second = split_view(&numbered("S", 1..=17, -113.0, 50.0), &pane);
        let want: Vec<_> = (13..=17).map(|n| format!("Button S{n}")).collect();
        assert_eq!(described(&new_rows(&visible_page(&first, true), &visible_page(&second, true))), want);
    }

    #[test]
    fn identical_rows_that_scroll_into_the_same_position_are_kept() {
        let a = tree(&[("A", 100), ("Repeat", 200), ("B", 300)]);
        let b = tree(&[("B", 100), ("Repeat", 200), ("C", 300)]);
        let added = new_rows(&visible_page(&a, true), &visible_page(&b, true));
        assert_eq!(added.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(), ["Repeat", "C"]);
    }

    #[test]
    fn a_covered_row_cannot_stop_the_search() {
        let xml = include_str!("../../../ictl-wda/tests/fixtures/settings-floating-search-native-ios27.xml");
        let tree = elements::parse_tree(xml, ScreenSize { width: 430.0, height: 932.0 });
        let page = visible_page(&tree, true);
        assert!(visible_match(&page, "资源库").is_none());
        assert!(visible_match(&page, "相机").is_some());
        assert!(visible_match(&page, "").is_none());
    }

    #[test]
    fn truncated_or_empty_viewports_do_not_establish_the_end() {
        let complete = tree(&[("Header", 0)]);
        let mut partial = complete.clone();
        partial.incomplete = true;
        assert!(unchanged_viewport(&complete, &complete));
        assert!(!unchanged_viewport(&complete, &partial));
        assert!(!unchanged_viewport(&partial, &complete));
        assert!(!unchanged_viewport(&Tree::default(), &Tree::default()));
    }

    #[test]
    fn a_visible_match_wins_even_on_the_last_viewport() {
        assert_eq!(stop_reason(true, true, true, true), Some(ScrollStop::Found));
        assert_eq!(stop_reason(false, true, false, false), Some(ScrollStop::Limit));
        assert_eq!(stop_reason(false, false, true, true), Some(ScrollStop::End));
        assert_eq!(stop_reason(false, false, false, true), Some(ScrollStop::MaxScrolls));
        assert_eq!(stop_reason(false, false, false, false), None);
    }
}
