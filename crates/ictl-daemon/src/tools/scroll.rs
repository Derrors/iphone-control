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
            tree = Box::pin(self.search_scroll(g, scan.down)).await?;
            scrolls += 1;
        }
    }
}

/// A truncated read can omit the rows that moved. Only two complete, populated reads can show the end.
fn unchanged_viewport(before: &Tree, after: &Tree) -> bool {
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

fn visible_page(tree: &Tree, down: bool) -> Vec<&Element> {
    let mut page: Vec<_> =
        tree.elements.iter().filter(|e| !matches!(tree.tap_point(e), TapPoint::Covered { .. })).collect();
    page.sort_by_key(|e| (if down { e.center.1 } else { -e.center.1 }, e.center.0));
    page
}

fn visible_match<'a>(page: &[&'a Element], until: &str) -> Option<&'a Element> {
    if until.is_empty() {
        return None;
    }
    page.iter().copied().find(|e| e.contains_text(until))
}

/// Fixed headers and footers recur at the same location. After removing those, consecutive viewports
/// share a suffix/prefix in travel order. Comparing sequences preserves identical labels in distinct rows.
fn new_rows<'a>(before: &[&Element], page: &[&'a Element]) -> Vec<&'a Element> {
    fn key(e: &Element) -> (&str, &str, &str, &str) {
        (&e.kind, &e.name, &e.label, &e.value)
    }
    let fixed = |(a, b): (&&Element, &&Element)| key(a) == key(b) && a.center == b.center;
    let head = before.iter().zip(page).take_while(|pair| fixed(*pair)).count();
    let (before, page) = (&before[head..], &page[head..]);
    let tail = before.iter().rev().zip(page.iter().rev()).take_while(|pair| fixed(*pair)).count();
    let (before, page) = (&before[..before.len() - tail], &page[..page.len() - tail]);
    let overlap = (1..=before.len().min(page.len()))
        .rev()
        .find(|&n| before[before.len() - n..].iter().zip(&page[..n]).all(|(a, b)| key(a) == key(b)))
        .unwrap_or(0);
    page[overlap..].to_vec()
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
