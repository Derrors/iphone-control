//! Looking at the screen: the element tree, waiting for it to settle or for a text, system alerts

use super::*;

impl Daemon {
    /// 当前界面的元素树。缓存不超过 `max_age` 就直接用（0 = 总是重读）；重读的结果写回缓存
    pub(super) async fn tree(&self, g: &mut Inner, max_age: Duration) -> crate::error::Result<Tree> {
        if !max_age.is_zero()
            && let Some((t, tree)) = &g.elements_cache
            && t.elapsed() <= max_age
        {
            let tree = tree.clone();
            g.dev.seen.saw_tree(&tree);
            return Ok(tree);
        }
        let tree = match self.route(g) {
            Route::Wda(w) => w.tree().await?,
            Route::Native => Box::pin(self.ax_tree(g, super::native::AX_WAIT)).await?,
            Route::Hid | Route::Unavailable => return Err(wda_unavailable(g)),
        };
        g.elements_cache = Some((Instant::now(), tree.clone()));
        g.dev.seen.saw_tree(&tree);
        Ok(tree)
    }

    pub(crate) async fn list_elements(
        &self,
        g: &mut Inner,
        filter: &str,
        limit: usize,
        max_age: Duration,
        keep_locked: bool,
    ) -> crate::error::Result<(String, crate::protocol::Data)> {
        if keep_locked {
            // only looking (a read-only viewer): the passcode isn't typed for it
            let locked = match self.route(g) {
                Route::Wda(w) => w.locked().await?,
                Route::Native => self.native_locked(g).await?,
                Route::Hid | Route::Unavailable => false,
            };
            if locked {
                return Err(ictl_l10n::tr!(
                    "手机锁着：只看不操作时不会替你解锁",
                    "The phone is locked: it isn't unlocked for someone who only watches"
                )
                .into());
            }
        } else {
            self.ensure_unlocked(g).await?;
        }
        let els = self.tree(g, max_age).await?.elements;
        let shown: Vec<&elements::Element> =
            if filter.is_empty() { els.iter().collect() } else { elements::filter(&els, filter) };
        let lines: Vec<_> = shown.iter().take(limit).map(|e| e.describe()).collect();
        let matched_count = shown.len();
        let truncated = matched_count > limit;
        let data = crate::protocol::Data::Elements {
            elements: shown.iter().take(limit).map(|e| element_data(e)).collect(),
            matched_count: Some(matched_count),
            truncated: Some(truncated),
        };
        let more = if truncated {
            ictl_l10n::trf!(
                "\n……另有 {} 个未列出,可用 filter 缩小范围",
                "\n…{} more not listed, narrow it down with filter",
                shown.len() - limit
            )
        } else {
            String::new()
        };
        let limited = self.snapshot_note(g);
        Ok((
            if lines.is_empty() { no_elements_text(filter, els.len()) } else { lines.join("\n") } + &more + &limited,
            data,
        ))
    }

    /// 上一次读的元素树受了快照上限的限制时，接在结果后面的说明（含换行）；没有限制时为空
    pub(super) fn snapshot_note(&self, g: &mut Inner) -> String {
        self.wda(g).and_then(|w| w.snapshot_note()).map(|n| format!("\n{n}")).unwrap_or_default()
    }

    /// 等界面稳定，最多等 `max`：
    /// - 有元素树的界面：连续两次读到的元素一样，并且没有加载圈。轮询期间让 WDA 读树时不等界面空闲、动画冷却
    ///   （否则滑完第一次读要等惯性滚动停下再冷却 2 秒，页面加载时一次能卡好几秒，超过上限很多）
    /// - 元素很少的界面（游戏、地图，画面变化不反映在元素树里）：改看画面，连续两帧缩略图几乎一样。
    ///   截图约 0.2 秒一张，两帧之间再隔一点，避免两帧落在同一段静止的过渡里
    ///
    /// 屏幕上有弹窗时在结果里提醒（读树时顺带看，不多花时间）
    pub(crate) async fn settle(&self, g: &mut Inner, max: Duration) -> crate::error::Result<String> {
        let t0 = Instant::now();
        // 刚滑完读到的树：只作比较的起点，是不是真稳定还要看下一次读到的
        let seed = g.swiped_tree.take().filter(|(at, _)| at.elapsed() < SWIPED_TREE_FRESH).map(|(_, t)| t);
        let w = match self.route(g) {
            Route::Wda(w) => w,
            Route::Native => return Box::pin(self.settle_native(g, t0, max)).await,
            Route::Hid | Route::Unavailable => {
                tokio::time::sleep(max.min(Duration::from_millis(500))).await;
                return Ok(ictl_l10n::tr!("WDA 不可用,等了 0.5 秒", "WDA unavailable, waited 0.5 s").into());
            }
        };
        w.begin_polling().await?;
        let r = poll_stable(w, t0, max, seed).await;
        w.end_polling().await;
        let (msg, settled, tree) = match r? {
            Polled::FewElements(first) => {
                // 元素很少：改看画面
                let note = alert_note(&first);
                let msg = self.settle_visually(g, t0, max).await?;
                let change = self.change_note(g, None).await;
                return Ok(msg + &change + &note);
            }
            Polled::Done { msg, settled, tree } => (msg, settled, tree),
        };
        let note = alert_note(&tree);
        // 自己画的加载圈（不是 ActivityIndicator）不在树里，树稳定了页面可能还在加载。
        // 加载中的页面中间是空的：只在这种时候多看一眼画面（约 0.5 秒），平常不多花时间
        let screen = self.screen(g).await?;
        let msg = if settled && middle_is_empty(&tree, screen.height) { msg + &self.motion_note(g).await } else { msg };
        let change = self.change_note(g, Some(&tree)).await;
        g.dev.seen.saw_tree(&tree);
        // 紧接着的 then=elements 直接用这棵树
        g.elements_cache = Some((Instant::now(), tree));
        Ok(msg + &change + &note)
    }

    /// [`Self::settle`] without WDA: the trees the phone pushes say when the screen has settled; a screen with few
    /// elements (a game, a map) is judged by its picture, as through WDA
    async fn settle_native(&self, g: &mut Inner, t0: Instant, max: Duration) -> crate::error::Result<String> {
        let (msg, tree) = Box::pin(self.native_settle(g, max)).await?;
        let note = alert_note(&tree);
        // no word from it: a screen with few elements that stay the same (a game, a map), judged by the picture
        if msg.is_empty() {
            let left = max.saturating_sub(t0.elapsed()).max(Duration::from_millis(600));
            let msg = self.settle_visually(g, Instant::now(), left).await?;
            let change = self.change_note(g, None).await;
            return Ok(msg + &change + &note);
        }
        let change = self.change_note(g, Some(&tree)).await;
        g.dev.seen.saw_tree(&tree);
        g.elements_cache = Some((Instant::now(), tree));
        Ok(msg + &change + &note)
    }

    /// 操作做完、界面稳定后，和操作前（[`PerDevice::before_action`](crate::core::PerDevice)）比有没有变化，
    /// 接在 settle 结果后面的说明。`tree`：稳定时读到的元素树（看画面判断稳定时为 None，用记下的画面指纹）。
    /// 操作前只截过图、没读过元素树时，补截一张图来比（约 0.3 秒）；操作前什么都没看过就不说
    async fn change_note(&self, g: &mut Inner, tree: Option<&Tree>) -> String {
        let Some(before) = g.dev.before_action.take() else { return String::new() };
        let mut thumb = g.dev.seen.thumb.clone();
        if before.tree.as_ref().zip(tree).is_none()
            && before.thumb.is_some()
            && thumb.is_none()
            && let Ok(png) = self.raw_png(g).await
            && let Ok(t) = thumbnail(png).await
        {
            g.dev.seen.saw_thumb(t.clone());
            thumb = Some(t);
        }
        let change = compare(&before, tree, thumb.as_deref());
        let n = g.dev.repeats.settled(change);
        change_text(change) + &repeat_text(n)
    }

    pub(super) async fn settle_visually(
        &self,
        g: &mut Inner,
        t0: Instant,
        max: Duration,
    ) -> crate::error::Result<String> {
        let deadline = t0 + max;
        let mut prev: Option<Vec<u8>> = None;
        loop {
            let png = self.raw_png(g).await?;
            let cur = thumbnail(png).await?;
            // 最后一帧就是操作后的画面：和操作前比（[`Self::change_note`]），也是下一个操作的「操作前」
            g.dev.seen.saw_thumb(cur.clone());
            if let Some(p) = &prev {
                let m = imaging::thumb_motion(p, &cur);
                if m.mean < VISUAL_STILL {
                    let local = if m.max_tile >= LOCAL_MOTION && imaging::mostly_blank(&cur) {
                        ictl_l10n::tr!(
                            ";但有一小块还在动(可能是加载圈或小动画),内容可能还没加载完,要等结果用 iphone_wait_for 或稍后再截图",
                            "; but a small area is still moving (perhaps a spinner or a small animation), content may still be loading; to wait for a result use iphone_wait_for or take a screenshot later"
                        )
                    } else {
                        ""
                    };
                    return Ok(ictl_l10n::trf!(
                        "画面已静止({:.1} 秒){local}",
                        "Picture still ({:.1} s){local}",
                        t0.elapsed().as_secs_f64()
                    ));
                }
            }
            // 不够再截一张（约 0.2 秒）、隔一下再比的时间了
            if deadline.saturating_duration_since(Instant::now()) < Duration::from_millis(300) {
                return Ok(ictl_l10n::trf!(
                    "{:.1} 秒内画面一直在动,按最后的状态返回",
                    "The picture kept moving for {:.1} s, returning the last state",
                    max.as_secs_f64()
                ));
            }
            prev = Some(cur);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// 隔一下截两张图，画面还在动就返回提醒，否则返回空串
    pub(super) async fn motion_note(&self, g: &mut Inner) -> String {
        let mut thumbs = Vec::new();
        for i in 0..2 {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let Ok(png) = self.raw_png(g).await else { return String::new() };
            let Ok(t) = thumbnail(png).await else { return String::new() };
            thumbs.push(t);
        }
        let m = imaging::thumb_motion(&thumbs[0], &thumbs[1]);
        if (m.mean >= VISUAL_STILL || m.max_tile >= LOCAL_MOTION) && imaging::mostly_blank(&thumbs[1]) {
            ictl_l10n::tr!(";但画面中间是空的、还有东西在动(可能还在加载),要等结果用 iphone_wait_for 或稍后再截图", "; but the middle of the screen is empty and something is still moving (perhaps still loading); to wait for a result use iphone_wait_for or take a screenshot later").into()
        } else {
            String::new()
        }
    }

    /// 前台是 SpringBoard 时查有没有系统弹窗（权限请求等），有就返回提醒，没有返回空串。
    /// 不带 then 的操作做完后调：弹窗挡着时点击、滑动都会落在弹窗上，WDA 却照样报成功。
    /// 前台是 App 时只多一次约 0.13 秒的查询；在主屏上再多约 0.4 秒
    /// A system alert in front: the note for the agent, and the alert's own text
    pub(crate) async fn alert_check(&self, g: &mut Inner) -> crate::error::Result<(String, Option<String>)> {
        let w = match self.route(g) {
            Route::Wda(w) => w,
            Route::Native => return Box::pin(self.native_alert(g)).await,
            Route::Hid | Route::Unavailable => return Ok((String::new(), None)),
        };
        if w.active_app().await? != SPRINGBOARD {
            return Ok((String::new(), None));
        }
        Ok(match w.alert_text().await {
            Ok(Some(text)) => (alert_msg(&text), Some(text)),
            _ => (String::new(), None),
        })
    }

    /// Wait for text to appear or to be absent from a successfully read screen.
    pub(crate) async fn wait_for(
        &self,
        g: &mut Inner,
        text: &str,
        timeout: Duration,
        disappeared: bool,
    ) -> crate::error::Result<String> {
        self.ensure_unlocked(g).await?;
        let t0 = Instant::now();
        let w = match self.route(g) {
            Route::Wda(w) => w,
            Route::Native => return Box::pin(self.native_wait_for(g, text, timeout, disappeared)).await,
            Route::Hid | Route::Unavailable => return Err(wda_unavailable(g)),
        };
        // 轮询时不等界面空闲：页面还在加载、播放动画时每次读树也只要约 0.2 秒
        w.begin_polling().await?;
        let r = async {
            loop {
                let tree = w.tree().await?;
                let why = match wait_match(&tree, text, disappeared, t0.elapsed()) {
                    Some(result) if !disappeared => return Ok(result),
                    // the lock screen lacks the text too: asked only when it would count as gone
                    Some(result) => match w.check_lock(None, true).await {
                        Ok(c) if c.locked => return Err(wait_locked()),
                        Ok(c) if !c.uncertain => return Ok(result),
                        Ok(_) => Waited::Covered,
                        // not known whether locked: not gone yet, asked again on the next read
                        Err(e) => {
                            tracing::debug!("lock state while waiting for {text:?} to disappear: {e}");
                            Waited::Covered
                        }
                    },
                    None => Waited::after(&tree, text, disappeared),
                };
                if t0.elapsed() >= timeout {
                    return Err(wait_timeout(text, timeout, why));
                }
                tokio::time::sleep(Duration::from_millis(300).min(timeout.saturating_sub(t0.elapsed()))).await;
            }
        }
        .await;
        w.end_polling().await;
        r
    }
}

pub(super) fn wait_match(tree: &Tree, text: &str, disappeared: bool, elapsed: Duration) -> Option<String> {
    let matches = elements::find_shown(&tree.elements, text);
    let elapsed = elapsed.as_secs_f64();
    if disappeared {
        if matches.is_empty() && !tree.incomplete && !tree.elements.is_empty() {
            return Some(ictl_l10n::trf!(
                "已消失「{text}」({elapsed:.1} 秒)",
                "Disappeared: \"{text}\" ({elapsed:.1} s)"
            ));
        }
        return None;
    }
    matches
        .first()
        .map(|e| ictl_l10n::trf!("已出现 {}({elapsed:.1} 秒)", "Appeared: {} ({elapsed:.1} s)", e.describe()))
}

/// Why [`Daemon::wait_for`] isn't done after the last read, for its timeout to say
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Waited {
    /// Waiting for text to appear: it hasn't
    NotAppeared,
    /// Waiting for it to disappear: it was read, still there
    Present,
    /// Not seen, but the read can't tell it is gone: nothing read, or cut short ([`wait_match`])
    Unread,
    /// Not seen, but something is over the screen (notification center, control center, a lock screen not known
    /// by its marks), or whether the phone is locked couldn't be found out
    Covered,
}

impl Waited {
    /// A read where the text didn't count as appeared, or as gone ([`wait_match`] gave nothing)
    pub(super) fn after(tree: &Tree, text: &str, disappeared: bool) -> Self {
        if !disappeared {
            Self::NotAppeared
        } else if elements::find_shown(&tree.elements, text).is_empty() {
            Self::Unread
        } else {
            Self::Present
        }
    }
}

pub(super) fn wait_timeout(text: &str, timeout: Duration, why: Waited) -> crate::error::Error {
    let seconds = timeout.as_secs_f64();
    match why {
        Waited::NotAppeared => {}
        Waited::Present => {
            return ictl_l10n::trf!(
                "{seconds:.1} 秒内「{text}」没消失,截图看看屏幕",
                "\"{text}\" did not disappear within {seconds:.1} s; take a screenshot"
            )
            .into();
        }
        Waited::Unread => {
            return ictl_l10n::trf!(
                "{seconds:.1} 秒内无法确认「{text}」已消失:界面没读到或没读全(元素太多、读得太慢),截图看看屏幕",
                "Couldn't confirm within {seconds:.1} s that \"{text}\" disappeared: the screen wasn't read, or not in full (too many elements, a slow read); take a screenshot"
            )
            .into();
        }
        Waited::Covered => {
            return ictl_l10n::trf!(
                "{seconds:.1} 秒内无法确认「{text}」已消失:屏幕被通知中心、控制中心或锁屏挡着,或查不到是否锁屏,截图看看屏幕",
                "Couldn't confirm within {seconds:.1} s that \"{text}\" disappeared: notification center, control center or the lock screen is over the screen, or the lock state couldn't be found out; take a screenshot"
            )
            .into();
        }
    }
    ictl_l10n::trf!(
        "{seconds:.0} 秒内没出现包含 \"{text}\" 的元素,截图看看屏幕",
        "No element containing \"{text}\" appeared within {seconds:.0} s; take a screenshot to look at the screen"
    )
    .into()
}

pub(super) fn wait_locked() -> crate::error::Error {
    ictl_l10n::tr!(
        "等待时手机锁屏了,无法确认文字已消失;解锁后再等",
        "The phone locked while waiting, so disappearance can't be confirmed; wait again once it is unlocked"
    )
    .into()
}

#[cfg(test)]
mod wait_tests {
    use super::*;

    #[test]
    fn disappearance_requires_a_readable_complete_screen_without_the_target() {
        let screen = ScreenSize { width: 390.0, height: 844.0 };
        let mut tree = elements::parse_tree(r#"<Button label="Clock" x="0" y="0" width="80" height="40"/>"#, screen);
        assert!(wait_match(&tree, "Clock", false, Duration::ZERO).is_some());
        assert!(wait_match(&tree, "Clock", true, Duration::ZERO).is_none());
        assert!(wait_match(&tree, "Timer", true, Duration::ZERO).is_some());
        tree.incomplete = true;
        assert!(wait_match(&tree, "Timer", true, Duration::ZERO).is_none());
        assert!(wait_match(&Tree::default(), "Timer", true, Duration::ZERO).is_none());
    }

    /// The timeout says why: still there, not read in full, or not appeared
    #[test]
    fn a_timeout_tells_present_from_unread() {
        let screen = ScreenSize { width: 390.0, height: 844.0 };
        let mut tree = elements::parse_tree(r#"<Button label="Clock" x="0" y="0" width="80" height="40"/>"#, screen);
        assert_eq!(Waited::after(&tree, "Clock", true), Waited::Present);
        assert_eq!(Waited::after(&tree, "Clock", false), Waited::NotAppeared);
        tree.incomplete = true;
        assert_eq!(Waited::after(&tree, "Timer", true), Waited::Unread);
        // read even though cut short: it is still there
        assert_eq!(Waited::after(&tree, "Clock", true), Waited::Present);
        assert_eq!(Waited::after(&Tree::default(), "Timer", true), Waited::Unread);
        let msg = |w| wait_timeout("Clock", Duration::from_secs(2), w).to_string();
        let all = [Waited::NotAppeared, Waited::Present, Waited::Unread, Waited::Covered].map(msg);
        assert!(all.iter().collect::<std::collections::HashSet<_>>().len() == 4, "{all:?}");
    }
}

/// 屏幕中间（上下各去掉一段，留给导航栏、标签栏）几乎没有元素：加载中、空白页
pub(super) fn middle_is_empty(tree: &Tree, screen_h: f64) -> bool {
    let (top, bottom) = ((screen_h * 0.25) as i64, (screen_h * 0.85) as i64);
    tree.elements.iter().filter(|e| e.center.1 > top && e.center.1 < bottom).count() <= 1
}

/// 轮询元素树直到连续两次一样（位置差不超过 [`STILL_TOLERANCE`]，且没有加载圈）或到 `max`。返回第一次读到的树，以及 (结果说明, 最后的树)；
/// 第一次读到的元素很少（游戏、地图）时第二项是 None，由调用方改看画面。
/// 不中途丢弃请求：丢掉的请求手机上仍在执行，后面的请求要排在它后面
pub(super) async fn poll_stable(
    w: &mut WdaClient<DeviceTransport>,
    t0: Instant,
    max: Duration,
    seed: Option<Tree>,
) -> crate::error::Result<Polled> {
    let first = match seed {
        Some(t) => t,
        None => w.tree().await?,
    };
    if first.elements.len() < VISUAL_SETTLE_BELOW {
        return Ok(Polled::FewElements(first));
    }
    let mut prev = first;
    let (msg, settled) = loop {
        if t0.elapsed() >= max {
            let msg = if prev.loading {
                ictl_l10n::trf!(
                    "{:.1} 秒内页面一直在加载(有加载圈),按最后的状态返回;要等结果出来用 iphone_wait_for",
                    "The page kept loading for {:.1} s (spinner shown), returning the last state; to wait for the result use iphone_wait_for",
                    max.as_secs_f64()
                )
            } else {
                ictl_l10n::trf!(
                    "{:.1} 秒内界面一直在变(可能有动画),按最后的状态返回",
                    "The screen kept changing for {:.1} s (perhaps an animation), returning the last state",
                    max.as_secs_f64()
                )
            };
            break (msg, false);
        }
        // 一次读树本身约 0.2 秒，两次之间不用再多等
        tokio::time::sleep(Duration::from_millis(30)).await;
        let cur = w.tree().await?;
        let same = cur.close_to(&prev, STILL_TOLERANCE);
        prev = cur;
        if same && !prev.loading {
            let msg = ictl_l10n::trf!("界面已稳定({:.1} 秒)", "Screen settled ({:.1} s)", t0.elapsed().as_secs_f64());
            break (msg, true);
        }
    };
    Ok(Polled::Done { msg, settled, tree: prev })
}

/// 轮询元素树的结果
pub(super) enum Polled {
    /// 元素太少（游戏、地图），元素树看不出界面变化，要改看画面
    FewElements(Tree),
    Done {
        msg: String,
        /// 真的稳定了（不是等到上限）
        settled: bool,
        tree: Tree,
    },
}

/// 列元素一个都没列出来时的说明。带 filter 没匹配上时要说清楚，不然 agent 会以为屏幕上什么都没有
pub(super) fn no_elements_text(filter: &str, total: usize) -> String {
    if filter.is_empty() || total == 0 {
        // 与 toolkit 的返回文字一致
        ictl_l10n::tr!("没有找到带文字的元素", "No elements with text found").into()
    } else {
        ictl_l10n::trf!(
            "没有找到包含 \"{filter}\" 的元素(屏幕上另有 {total} 个带文字的元素,不带 filter 可全部列出)",
            "No element containing \"{filter}\" ({total} other elements with text on screen; leave out filter to list them all)"
        )
    }
}

/// 有人读过的界面：元素树、画面指纹（截图时记下），从上一个操作做完以后算。
/// 下一个操作开始时它就是「操作前的界面」（[`PerDevice::before_action`](crate::core::PerDevice)）
#[derive(Debug, Clone, Default)]
pub(crate) struct Seen {
    at: Option<Instant>,
    pub tree: Option<Tree>,
    pub thumb: Option<Vec<u8>>,
}

/// 操作前看过的界面隔了这么久就不拿来比：中间画面可能自己变了（视频、轮播、来消息）
const SEEN_FRESH: Duration = Duration::from_secs(120);

impl Seen {
    pub fn saw_tree(&mut self, tree: &Tree) {
        self.at = Some(Instant::now());
        self.tree = Some(tree.clone());
    }

    pub fn saw_thumb(&mut self, thumb: Vec<u8>) {
        self.at = Some(Instant::now());
        self.thumb = Some(thumb);
    }

    /// 拿走记下的界面；太旧或什么都没有时为 None
    pub fn take_fresh(&mut self) -> Option<Seen> {
        let s = std::mem::take(self);
        s.at.is_some_and(|t| t.elapsed() < SEEN_FRESH).then_some(s)
    }
}

/// 操作前后界面比较的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Change {
    Changed,
    Same,
    /// 没有能比的：操作前没读过树也没截过图，或者前后一个是树一个是画面
    Unknown,
}

/// 先比元素树（文字、值、大小一样，位置差不超过 [`STILL_TOLERANCE`]），没有树再比画面指纹
/// （整体和每一小块都几乎没变才算一样：只有一个开关翻过来时整体平均差很小）
pub(super) fn compare(before: &Seen, tree: Option<&Tree>, thumb: Option<&[u8]>) -> Change {
    if let (Some(a), Some(b)) = (&before.tree, tree) {
        return if a.close_to(b, STILL_TOLERANCE) { Change::Same } else { Change::Changed };
    }
    if let (Some(a), Some(b)) = (&before.thumb, thumb) {
        let m = imaging::thumb_motion(a, b);
        return if m.mean < VISUAL_STILL && m.max_tile < LOCAL_MOTION { Change::Same } else { Change::Changed };
    }
    Change::Unknown
}

pub(super) fn change_text(c: Change) -> String {
    match c {
        Change::Changed => ictl_l10n::tr!(";和操作前相比界面有变化", "; the screen changed since before the action").into(),
        Change::Same => ictl_l10n::tr!(
            ";但界面和操作前一样:操作可能没生效(点在了空白处、被别的东西挡住,或 App 还没反应过来),先截图看清再决定,不要原样再做一遍",
            "; but the screen is the same as before the action: it may not have taken effect (it hit an empty spot, something covered the target, or the app hasn't reacted yet); take a screenshot before deciding, rather than repeating the same action"
        )
        .into(),
        Change::Unknown => String::new(),
    }
}

/// 同一个操作原样连续做了几次、界面都没变。操作开始时 [`Self::start`]，等界面稳定比较完 [`Self::settled`]
#[derive(Debug, Default)]
pub(crate) struct Repeats {
    /// 正在做的操作
    acting: Option<String>,
    /// 上一个做完界面没变的操作，和它连续没变的次数
    same: Option<(String, u32)>,
}

impl Repeats {
    /// 一个操作开始：换了操作（参数不同也算）就重新数
    pub(crate) fn start(&mut self, req: &crate::protocol::Request) {
        let sig = serde_json::to_string(req).unwrap_or_default();
        if self.same.as_ref().is_some_and(|(s, _)| *s != sig) {
            self.same = None;
        }
        self.acting = Some(sig);
    }

    /// 操作做完、和操作前比过：界面没变时返回这是连续第几次（1 起），变了或没法比时为 0
    pub(super) fn settled(&mut self, change: Change) -> u32 {
        let Some(sig) = self.acting.take() else { return 0 };
        match change {
            Change::Same => {
                let n = match &self.same {
                    Some((s, n)) if *s == sig => n + 1,
                    _ => 1,
                };
                self.same = Some((sig, n));
                n
            }
            Change::Changed => {
                self.same = None;
                0
            }
            // 没法比（操作前没看过界面）：不数这一次，也不清零
            Change::Unknown => 0,
        }
    }
}

/// 同一个操作原样做了 `n` 次界面都没变（第二次起）：明说在原地打转，要换做法
pub(super) fn repeat_text(n: u32) -> String {
    if n < 2 {
        return String::new();
    }
    ictl_l10n::trf!(
        ";这已经是连续第 {n} 次原样做这个操作、界面都没变:别再重复了,换个做法(截图看清目标在哪、换个位置或改用按名字点、先关掉挡住的东西),还不行就告诉用户卡在了哪一步",
        "; this exact action has now left the screen unchanged {n} times in a row: stop repeating it and try something else (look at a screenshot to find the target, tap elsewhere or by text, close whatever is in the way), or tell the user where it is stuck"
    )
}

/// An element as data: the text it shows (its label, else its name, else its value) and its box
pub(super) fn element_data(e: &elements::Element) -> ElementData {
    let text = [&e.label, &e.name, &e.value].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
    let (w, h) = (e.size.0 as f64, e.size.1 as f64);
    ElementData {
        kind: e.kind.clone(),
        label: text.chars().take(200).collect(),
        x: e.center.0 as f64 - w / 2.0,
        y: e.center.1 as f64 - h / 2.0,
        width: w,
        height: h,
    }
}

pub(super) fn alert_msg(text: &str) -> String {
    ictl_l10n::trf!(
        "\n注意:{},点击、滑动都会落在弹窗上;{TAP_ALERT}",
        "\nNote: {}, taps and swipes land on the alert; {TAP_ALERT_EN}",
        blocking(text)
    )
}

pub(super) const TAP_ALERT: &str = "先用 iphone_tap_element 点弹窗上的按钮";

pub(super) const TAP_ALERT_EN: &str = "first tap a button on the alert with iphone_tap_element";

/// 「屏幕上有系统弹窗「…」挡着」：旧版本 MCP 端靠固定的开头认出结果里已经报过弹窗（新版本看 Outcome）
pub(super) fn blocking(text: &str) -> String {
    ictl_l10n::trf!("{ALERT_BLOCKING}「{text}」挡着", "a system alert is on screen: “{text}”")
}

/// 树里有弹窗就返回提醒
pub(super) fn alert_note(tree: &Tree) -> String {
    match tree.alert() {
        Some(a) => {
            let title = [&a.label, &a.name].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
            ictl_l10n::trf!(
                "\n注意:屏幕上有弹窗「{title}」,先点弹窗上的按钮(见元素列表或截图)",
                "\nNote: an alert “{title}” is on screen; tap a button on it first (see the element list or the screenshot)"
            )
        }
        None => String::new(),
    }
}
