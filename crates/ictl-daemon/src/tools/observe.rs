//! Looking at the screen: the element tree, waiting for it to settle or for a text, system alerts

use super::*;

impl Daemon {
    /// 当前界面的元素树。缓存不超过 `max_age` 就直接用（0 = 总是重读）；重读的结果写回缓存
    pub(super) async fn tree(&self, g: &mut Inner, max_age: Duration) -> Result<Tree, String> {
        if !max_age.is_zero()
            && let Some((t, tree)) = &g.elements_cache
            && t.elapsed() <= max_age
        {
            return Ok(tree.clone());
        }
        let tree = self.require_wda(g)?.tree().await.map_err(|e| e.to_string())?;
        g.elements_cache = Some((Instant::now(), tree.clone()));
        Ok(tree)
    }

    pub(crate) async fn list_elements(
        &self,
        g: &mut Inner,
        filter: &str,
        limit: usize,
        max_age: Duration,
        keep_locked: bool,
    ) -> Result<(String, Vec<ElementData>), String> {
        if keep_locked {
            // only looking (a read-only viewer): the passcode isn't typed for it
            if let Some(w) = self.wda(g)
                && w.locked().await.map_err(|e| e.to_string())?
            {
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
        let data = shown.iter().take(limit).map(|e| element_data(e)).collect();
        let more = if shown.len() > limit {
            ictl_l10n::trf!(
                "\n……另有 {} 个未列出,可用 filter 缩小范围",
                "\n…{} more not listed, narrow it down with filter",
                shown.len() - limit
            )
        } else {
            String::new()
        };
        Ok((if lines.is_empty() { no_elements_text(filter, els.len()) } else { lines.join("\n") } + &more, data))
    }

    /// 等界面稳定，最多等 `max`：
    /// - 有元素树的界面：连续两次读到的元素一样，并且没有加载圈。轮询期间让 WDA 读树时不等界面空闲、动画冷却
    ///   （否则滑完第一次读要等惯性滚动停下再冷却 2 秒，页面加载时一次能卡好几秒，超过上限很多）
    /// - 元素很少的界面（游戏、地图，画面变化不反映在元素树里）：改看画面，连续两帧缩略图几乎一样。
    ///   截图约 0.2 秒一张，两帧之间再隔一点，避免两帧落在同一段静止的过渡里
    ///
    /// 屏幕上有弹窗时在结果里提醒（读树时顺带看，不多花时间）
    pub(crate) async fn settle(&self, g: &mut Inner, max: Duration) -> Result<String, String> {
        let t0 = Instant::now();
        // 刚滑完读到的树：只作比较的起点，是不是真稳定还要看下一次读到的
        let seed = g.swiped_tree.take().filter(|(at, _)| at.elapsed() < SWIPED_TREE_FRESH).map(|(_, t)| t);
        let Some(w) = self.wda(g) else {
            tokio::time::sleep(max.min(Duration::from_millis(500))).await;
            return Ok(ictl_l10n::tr!("WDA 不可用,等了 0.5 秒", "WDA unavailable, waited 0.5 s").into());
        };
        w.begin_polling().await.map_err(|e| e.to_string())?;
        let r = poll_stable(w, t0, max, seed).await;
        w.end_polling().await;
        let (msg, settled, tree) = match r? {
            Polled::FewElements(first) => {
                // 元素很少：改看画面
                let note = alert_note(&first);
                return Ok(self.settle_visually(g, t0, max).await? + &note);
            }
            Polled::Done { msg, settled, tree } => (msg, settled, tree),
        };
        let note = alert_note(&tree);
        // 自己画的加载圈（不是 ActivityIndicator）不在树里，树稳定了页面可能还在加载。
        // 加载中的页面中间是空的：只在这种时候多看一眼画面（约 0.5 秒），平常不多花时间
        let screen = self.screen(g).await?;
        let msg = if settled && middle_is_empty(&tree, screen.height) { msg + &self.motion_note(g).await } else { msg };
        // 紧接着的 then=elements 直接用这棵树
        g.elements_cache = Some((Instant::now(), tree));
        Ok(msg + &note)
    }

    pub(super) async fn settle_visually(&self, g: &mut Inner, t0: Instant, max: Duration) -> Result<String, String> {
        let deadline = t0 + max;
        let w = self.require_wda(g)?;
        let mut prev: Option<Vec<u8>> = None;
        loop {
            let png = w.screenshot_png().await.map_err(|e| e.to_string())?;
            let cur = thumbnail(png).await?;
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
        let Some(w) = self.wda(g) else { return String::new() };
        let mut thumbs = Vec::new();
        for i in 0..2 {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let Ok(png) = w.screenshot_png().await else { return String::new() };
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
    pub(crate) async fn alert_check(&self, g: &mut Inner) -> Result<(String, Option<String>), String> {
        let Some(w) = self.wda(g) else { return Ok((String::new(), None)) };
        if w.active_app().await.map_err(|e| e.to_string())? != SPRINGBOARD {
            return Ok((String::new(), None));
        }
        Ok(match w.alert_text().await {
            Ok(Some(text)) => (alert_msg(&text), Some(text)),
            _ => (String::new(), None),
        })
    }

    /// 等包含 `text` 的元素出现
    pub(crate) async fn wait_for(&self, g: &mut Inner, text: &str, timeout: Duration) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let t0 = Instant::now();
        let w = self.require_wda(g)?;
        // 轮询时不等界面空闲：页面还在加载、播放动画时每次读树也只要约 0.2 秒
        w.begin_polling().await.map_err(|e| e.to_string())?;
        let r = async {
            loop {
                let els = w.elements().await.map_err(|e| e.to_string())?;
                // 不算输入框里的字：刚输入的搜索词不能算「结果出来了」
                if let Some(e) = elements::find_shown(&els, text).first() {
                    return Ok(ictl_l10n::trf!("已出现 {}({:.1} 秒)", "Appeared: {} ({:.1} s)", e.describe(), t0.elapsed().as_secs_f64()));
                }
                if t0.elapsed() >= timeout {
                    return Err(ictl_l10n::trf!("{:.0} 秒内没出现包含 \"{text}\" 的元素,截图看看屏幕", "No element containing \"{text}\" appeared within {:.0} s; take a screenshot to look at the screen", timeout.as_secs_f64()));
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        .await;
        w.end_polling().await;
        r
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
) -> Result<Polled, String> {
    let first = match seed {
        Some(t) => t,
        None => w.tree().await.map_err(|e| e.to_string())?,
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
        let cur = w.tree().await.map_err(|e| e.to_string())?;
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
