//! Element tapping by name and scroll-to-find helpers.

use super::super::*;
use super::Stroke;

/// 边滑边找时每次滑多久：1 秒的慢滑停下时几乎没有惯性，内容移动和手指移动差不多（iPhone 12 mini 设置首页：
/// 手指移 300 点，0.3 秒滚了 618 点，0.6 秒 420 点，1 秒 282 点；W3C 在抬起前停一下也去不掉惯性）
const SEARCH_SWIPE_SECONDS: f64 = 1.0;

/// 边滑边找找到之后、点之前，隔这么久再读一次，看要点的那个是不是还在原处（[`target_at_rest`]）
const TARGET_REST_GAP: Duration = Duration::from_millis(250);
/// 最多等这么久让它停在原处；到时照样点最后一次读到的位置
const TARGET_REST_MAX: Duration = Duration::from_millis(2000);

/// 要点的那个匹配（按 [`elements::find`] 的第 `index` 个）在哪里：中心和大小
pub(super) fn target_place(tree: &Tree, text: &str, index: usize) -> Option<((i64, i64), (i64, i64))> {
    elements::find(&tree.elements, text).get(index).map(|e| (e.center, e.size))
}

/// 边滑边找停下时用的是 [`STILL_TOLERANCE`]（两次读之间动了不超过 3 点就算停了），列表减速的尾巴还在挪的时候也算；
/// 这时点下去只让列表停住，点不到要点的那个，却照样报「已点击」（真机 iOS 27.2.1，设置里带 scroll 点「隐私与安全」：
/// 6 次只进去 1 次；先停 2 秒再点 4 次进去 3 次）。所以点之前再读（`read`，隔 `gap`，用的是 [`TARGET_REST_GAP`]），
/// 直到它连着两次在同一个地方，最多 `max`（[`TARGET_REST_MAX`]）；返回最后读到的树。读失败了、它不见了就不再等，
/// 用手上最新的那棵
pub(super) async fn target_at_rest(
    mut tree: Tree,
    (text, index): (&str, usize),
    (gap, max): (Duration, Duration),
    mut read: impl AsyncFnMut() -> Option<Tree>,
) -> Tree {
    let t0 = Instant::now();
    while t0.elapsed() < max {
        let Some(was) = target_place(&tree, text, index) else { break };
        tokio::time::sleep(gap).await;
        let Some(next) = read().await else { break };
        let still = target_place(&next, text, index) == Some(was);
        tree = next;
        if still {
            break;
        }
    }
    tree
}

/// 边滑边找时算找到了：有匹配，而且要点的那个没有被整个盖住（停在底部工具栏、悬浮栏下面的还要再滑一下）。
/// index 超出匹配个数也算找到：滑下去不会让序号变得更有意义，交给后面照常报
fn reachable_in(tree: &Tree, matches: &[&elements::Element], index: usize) -> bool {
    match matches.get(index) {
        Some(e) => !matches!(tree.tap_point(e), TapPoint::Covered { .. }),
        None => !matches.is_empty(),
    }
}

/// 有能点到的匹配，按 [`elements::find`]：没有文字完全相同的就算包含它的
pub(in crate::tools) fn reachable(tree: &Tree, text: &str, index: usize) -> bool {
    reachable_in(tree, &elements::find(&tree.elements, text), index)
}

/// 有能点到的、文字（标签或名字）完全相同的匹配。边滑边找只在这时停下：当前屏只有包含它的（找「App」时的
/// 「Apple账户」）不算，要找的多半在后面
pub(in crate::tools) fn reachable_exact(tree: &Tree, text: &str, index: usize) -> bool {
    let exact: Vec<_> = tree.elements.iter().filter(|e| e.label == text || e.name == text).collect();
    reachable_in(tree, &exact, index)
}

/// 边滑边找的一次滑动：屏幕正中，竖着滑 35% 屏高（前后两屏有重叠，不会漏过一行）。`down` 看下面的内容，手指往上
pub(in crate::tools) fn search_stroke(width: f64, height: f64, down: bool) -> ((f64, f64), (f64, f64)) {
    let x = (width / 2.0).round();
    let (low, high) = ((height * 0.68).round(), (height * 0.33).round());
    if down { ((x, low), (x, high)) } else { ((x, high), (x, low)) }
}

impl Daemon {
    /// 边滑边找的一次慢滑（[`search_stroke`]），等停下后读一棵新树。`x`：在这个横坐标上滑（iPad 分栏界面的左栏），
    /// 不给就是屏幕正中
    pub(in crate::tools) async fn search_scroll(
        &self,
        g: &mut Inner,
        down: bool,
        x: Option<f64>,
    ) -> crate::error::Result<Tree> {
        let screen = self.screen(g).await.map_err(Error::preflight)?;
        let (mut from, mut to) = search_stroke(screen.width, screen.height, down);
        if let Some(x) = x {
            (from.0, to.0) = (x, x);
        }
        Box::pin(self.swipe(g, Stroke { from, to, seconds: SEARCH_SWIPE_SECONDS, hold: 0.0 }, None, false)).await?;
        // the read that saw the list stop is the screen now: not read again (about 0.3 s a swipe, 8 to 15 a search)
        if let Some(tree) = self.swiped(g).await {
            g.elements_cache = Some((Instant::now(), tree.clone()));
            g.dev.seen.saw_tree(&tree);
            return Ok(tree);
        }
        self.tree(g, Duration::ZERO).await
    }

    /// The tree read as the last swipe's list stopped, if still fresh ([`SWIPED_TREE_FRESH`]) and confirmed still by
    /// two reads (a screen with few elements isn't); taken. Under native control that read was of the app alone: what
    /// SpringBoard shows over it (an alert) is read now
    async fn swiped(&self, g: &mut Inner) -> Option<Tree> {
        let usable = |at: &Instant, still: bool| still && at.elapsed() < SWIPED_TREE_FRESH;
        if let Some(s) = g.swiped_tree.take().filter(|s| usable(&s.at, s.still)) {
            return Some(s.read);
        }
        let s = g.swiped_snap.take().filter(|s| usable(&s.at, s.still))?;
        Box::pin(self.with_overlays(g, s.read)).await.ok()
    }

    pub(crate) async fn tap_element(
        &self,
        g: &mut Inner,
        text: &str,
        index: usize,
        max_age: Duration,
        scroll: Option<crate::protocol::ScrollSearch>,
        may_continue: &mut dyn FnMut() -> bool,
    ) -> crate::error::Result<(String, bool)> {
        self.ensure_unlocked(g).await.map_err(Error::preflight)?;
        let mut tree = self.tree(g, max_age).await.map_err(Error::preflight)?;
        // 屏幕上没有、又要边滑边找：慢滑一小段（不带惯性，不会把要找的甩过去）、等停下再读树，
        // 找到文字完全相同的或滑到底为止。一路上记下第一次看到包含它的是滑了几次的时候
        let mut scrolled = 0;
        let mut back = 0;
        let mut at_end = false;
        if let Some(s) = scroll {
            let failed_scroll = |e: Error, completed: u32| {
                let message = ictl_l10n::trf!(
                    "查找目标时滚动失败（此前已完成 {completed} 次滑动），尚未点击目标；请查看屏幕：{e}",
                    "Scroll search failed after {completed} completed swipes; the target was not tapped. Inspect the screen: {e}"
                );
                e.with_message(message)
            };
            let mut partial_at = None;
            loop {
                if reachable_exact(&tree, text, index) {
                    break;
                }
                if partial_at.is_none() && reachable(&tree, text, index) {
                    partial_at = Some(scrolled);
                }
                if at_end || scrolled >= s.max {
                    break;
                }
                // the task was paused or the caller went away: no more swiping, and nothing tapped
                if !may_continue() {
                    return Err(Error::preflight(stopped_searching(scrolled)));
                }
                let next =
                    Box::pin(self.search_scroll(g, s.down, None)).await.map_err(|e| failed_scroll(e, scrolled))?;
                scrolled += 1;
                at_end = super::scroll::unchanged_viewport(&tree, &next);
                tree = next;
            }
            // 没有文字完全相同的，包含它的又已经滑过去了：往回滑到看到它的地方（多滑一次，前后两屏有重叠）
            if !reachable_exact(&tree, text, index)
                && !reachable(&tree, text, index)
                && let Some(k) = partial_at
            {
                while back <= scrolled - k && !reachable(&tree, text, index) {
                    if !may_continue() {
                        return Err(Error::preflight(stopped_searching(scrolled + back)));
                    }
                    tree = Box::pin(self.search_scroll(g, !s.down, None))
                        .await
                        .map_err(|e| failed_scroll(e, scrolled + back))?;
                    back += 1;
                }
            }
        }
        if scrolled + back > 0 {
            let pace = (TARGET_REST_GAP, TARGET_REST_MAX);
            tree = target_at_rest(tree, (text, index), pace, async || self.tree(g, Duration::ZERO).await.ok()).await;
        }
        let searched = match (scrolled, at_end, back) {
            (0, ..) => String::new(),
            (n, _, b @ 1..) => ictl_l10n::trf!(
                "(滑了 {n} 次没有文字完全相同的,又往回滑了 {b} 次)",
                " (scrolled {n} times without an exact match, then {b} back)"
            ),
            (n, true, 0) => ictl_l10n::trf!("(滑了 {n} 次,已到尽头)", " (scrolled {n} times, reached the end)"),
            (n, false, 0) => ictl_l10n::trf!("(滑了 {n} 次)", " (scrolled {n} times)"),
        };
        // 边滑边找了、最后点的却不是文字完全相同的：明说，Agent 看得出这和它要找的不一样
        let partial_only = scroll.is_some() && !reachable_exact(&tree, text, index);
        // 点之前刚读的树就是操作前的界面（比操作开始前记下的更新）
        g.dev.before_action = Some(std::mem::take(&mut g.dev.seen));
        let matches = elements::find(&tree.elements, text);
        if matches.is_empty() {
            let limited = self.snapshot_note(g);
            let hint = if scroll.is_some() {
                String::new()
            } else {
                ictl_l10n::tr!(
                    ";可能在屏幕外,可带 scroll 边滑边找",
                    "; it may be off screen: pass scroll to look further"
                )
                .to_owned()
            };
            return Ok((
                ictl_l10n::trf!(
                    "没找到包含 \"{text}\" 的元素{searched},先用 iphone_list_elements 或截图看看屏幕{limited}{hint}",
                    "Not found: no element contains \"{text}\"{searched}; look at the screen with iphone_list_elements or a screenshot first{limited}{hint}",
                ),
                true,
            ));
        }
        if index >= matches.len() {
            let list: Vec<_> = matches.iter().map(|e| e.describe()).collect();
            return Ok((
                ictl_l10n::trf!(
                    "只匹配到 {} 个:\n{}",
                    "Only matched {} (index out of range):\n{}",
                    matches.len(),
                    list.join("\n")
                ),
                true,
            ));
        }
        let e = matches[index];
        // 中心被悬浮栏、键盘、浮层之类盖住时，点上去会落在盖着的东西上
        let ((x, y), moved) = match tree.tap_point(e) {
            TapPoint::Center(p) => (p, String::new()),
            TapPoint::Moved { at, by } => (
                at,
                ictl_l10n::trf!(
                    ";它的中心被「{by}」盖住,改点没被盖住的 ({},{})",
                    "; its center is covered by “{by}”, tapped an uncovered part at ({},{}) instead",
                    fmt_g(at.0),
                    fmt_g(at.1)
                ),
            ),
            TapPoint::Covered { by } => {
                return Ok((
                    ictl_l10n::trf!(
                        "被挡住:{} 整个被「{by}」盖住,没有点;先滑动让它露出来,或截图后按坐标点",
                        "Covered: {} is completely covered by “{by}”, not tapped; scroll to reveal it first, or tap by coordinates from a screenshot",
                        e.describe()
                    ),
                    true,
                ));
            }
        };
        // paused, taken over or left while the tree was read or the list moved: nothing is tapped
        if !may_continue() {
            return Err(Error::preflight(stopped_searching(scrolled + back)));
        }
        self.tap_at(g, x, y).await?;
        let note = if matches.len() > 1 {
            ictl_l10n::trf!("(共匹配 {} 个,点的是第 {index} 个)", " ({} matches, tapped number {index})", matches.len())
        } else {
            String::new()
        };
        let found = match (scrolled, partial_only) {
            (0, false) => String::new(),
            (0, true) => {
                ictl_l10n::tr!("(没有文字完全相同的,点的是包含它的)", " (no exact match, tapped one that contains it)")
                    .to_owned()
            }
            (_, true) => ictl_l10n::trf!(
                "{searched}(没有文字完全相同的,点的是包含它的)",
                "{searched} (no exact match, tapped one that contains it)"
            ),
            (n, false) => ictl_l10n::trf!("(滑了 {n} 次后找到)", " (found after {n} scrolls)"),
        };
        Ok((ictl_l10n::trf!("已点击 {}{found}{note}{moved}", "Tapped {}{found}{note}{moved}", e.describe()), false))
    }
}

/// [`Daemon::tap_element`] stopped before tapping: the task was paused or taken over, or the caller went away
fn stopped_searching(completed: u32) -> String {
    ictl_l10n::trf!(
        "点击前被打断（任务暂停、被接管或调用方已离开），此前已完成 {completed} 次滑动，没有点击；请先查看屏幕",
        "Interrupted before tapping (the task was paused or taken over, or the caller went away) after {completed} swipes; nothing was tapped. Look at the screen first"
    )
}
