//! 各工具请求的实现：优先 WDA，WDA 连不上时坐标类操作退到 HID。调用方持有设备锁。

mod input;
mod keys;
mod launch;
mod native;
mod observe;
mod scroll;

pub(crate) use input::Stroke;

use std::time::{Duration, Instant};

use base64::Engine;
use ictl_device::{
    AppInfo, Button, ClipboardRestore, DeviceError, HidSession, Rotation, ScreenSize, apps, hid::ascii_to_usage,
};
use ictl_unlock::UnlockError;
use ictl_wda::client::SPRINGBOARD;
use ictl_wda::elements::{self, TapPoint, Tree};
use ictl_wda::http::DeviceTransport;
use ictl_wda::{Launch, WdaClient, WdaError};

use crate::core::{Daemon, Inner, drop_tunnel, hid_supported, wda_down};
use crate::error::Error;
use crate::imaging::{self, fmt_g};
use crate::notify::Event;
use crate::protocol::{ALERT_BLOCKING, Response, TAP_MISSED, TouchPhase};
use crate::protocol::{Delivery, FailureStage};
use crate::protocol::{ElementData, StatusData};
pub(crate) use native::AX_WAIT;
use observe::*;
pub(crate) use observe::{Repeats, Seen};

/// 锁屏后至少等这么久再自动解锁：锁屏过渡没走完就唤醒，WDA 第一下上滑卡约 16 秒后报 `point.x != INFINITY`
pub(crate) const LOCK_SETTLE: Duration = Duration::from_secs(3);
/// 元素少于这么多个就按画面判断是否稳定（游戏只有几个状态栏元素；普通 App 页面通常几十个）
const VISUAL_SETTLE_BELOW: usize = 12;
/// 两帧缩略图平均逐像素差低于这个值算静止（0～255）。游戏背景的轻微摆动约 1～2，消除动画几十
const VISUAL_STILL: f64 = 3.0;
/// 整体静止时，某一小块的平均差超过这个值算「还有一小块在动」（加载圈、小动画）。完全没动的两帧差是 0。
/// 只在画面大部分是空白时才提醒（[`imaging::mostly_blank`]），游戏、地图的待机动画不算
const LOCAL_MOTION: f64 = 4.0;
/// 等界面稳定时，前后两次元素位置差不超过这么多点就算没动（惯性滚动的尾巴）
const STILL_TOLERANCE: i64 = 3;
/// 滑动后最多等这么久让列表停下（快速滑动的惯性滚动约 2 秒）
const SCROLL_SETTLE: Duration = Duration::from_millis(2500);
/// 滑动时读到的树这么久之内还能当 settle 比较的起点
const SWIPED_TREE_FRESH: Duration = Duration::from_secs(1);
/// WDA 不可用时 status 里说明哪些功能用不了
const NEEDS_WDA: &str =
    "读界面元素、按名字点击、启动 App 要等 WDA 恢复；输入中文改经剪贴板粘贴，要点手机弹出的「允许粘贴」";
const NEEDS_WDA_EN: &str = "reading elements, tapping by text and launching apps wait until WDA is back; non-ASCII text is pasted through the pasteboard instead, which needs a tap on the phone's “Allow Paste” prompt";
/// HID 粘贴后过这么久还原剪贴板：留时间给调用方截图、点「允许粘贴」（Poki 等 45 秒）
pub(crate) const PASTE_RESTORE_AFTER: Duration = Duration::from_secs(45);
/// 按 Home 后最多等这么久，确认主屏到了前台（iPhone 12 mini 上 IOHID 的 Home 约 1.3～1.5 秒）
const HOME_SETTLE: Duration = Duration::from_secs(2);
/// 按电源键后最多等这么久，等 WDA 报锁屏（iPhone 12 mini 实测约 1 秒）
const LOCK_WAIT: Duration = Duration::from_secs(3);

impl Daemon {
    /// 配置了密码就在锁屏时自动解锁；没配置而手机锁着则报错（与 toolkit 一致）
    pub(crate) async fn ensure_unlocked(&self, g: &mut Inner) -> crate::error::Result<()> {
        let cfg = self.unlock_cfg(g.device.as_ref().map(|d| d.udid.as_str()));
        let t0 = Instant::now();
        if let Some(left) = g.dev.last_lock.and_then(|t| LOCK_SETTLE.checked_sub(t.elapsed())) {
            tokio::time::sleep(left).await;
        }
        let native = self.native(g);
        let r = match self.wda(g) {
            Some(wda) => ictl_unlock::ensure_unlocked(wda, &cfg).await,
            None if native => Box::pin(self.native_unlock(g, &cfg)).await,
            None => return Ok(()),
        };
        match r {
            Ok(ictl_unlock::Outcome::Unlocked) => {
                let took = t0.elapsed();
                // the answer says so (`dispatch`), and the event log: the action took seconds longer than usual
                g.dev.unlocked = Some(took);
                self.log.log(&ictl_l10n::log_trf!(
                    "已自动解锁(用时 {:.1} 秒)",
                    "Unlocked automatically ({:.1} s)",
                    took.as_secs_f64()
                ));
                Ok(())
            }
            Ok(ictl_unlock::Outcome::NotLocked) => Ok(()),
            Err(e @ (UnlockError::WrongPasscode | UnlockError::Uncertain(_))) => {
                self.notify(g, Event::UnlockDisabled);
                Err(ictl_l10n::trf!("{e},已推送通知", "{e}, notification sent").into())
            }
            Err(UnlockError::NoPasscode) if self.cfg.legacy_passcode_file().is_file() => {
                Err(ictl_l10n::tr!("手机已锁屏,需要人工解锁:旧版本保存的锁屏密码没记是哪台设备的,同时接着几台时不会用它;\
在这台设备连着时运行 iphone-control passcode set 重新设置", "The phone is locked and has to be unlocked by hand: the passcode saved by an older version has no device, and it isn't used while several devices are connected; run iphone-control passcode set while this device is connected to set it again")
                    .into())
            }
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) async fn screen(&self, g: &mut Inner) -> crate::error::Result<ScreenSize> {
        if let Some(w) = self.wda(g)
            && let Ok(s) = w.screen_size().await
        {
            return Ok(s);
        }
        // 原生控制每次读界面都记下尺寸（[`Self::ax_read`]）：按现在的方向给
        if self.native(g)
            && let Some(s) = g.persisted.screen_for(g.device.as_ref().map(|d| d.udid.as_str()))
        {
            let landscape = g.dev.last_rotation.is_some_and(|(_, r)| r.is_landscape());
            return Ok(if landscape { ScreenSize { width: s.height, height: s.width } } else { s });
        }
        // 这台设备记下的尺寸（别的设备的不用；旧版本没记是哪台，照用）
        g.persisted
            .screen_for(g.device.as_ref().map(|d| d.udid.as_str()))
            .or(self.cfg.screen_fallback)
            .ok_or_else(|| ictl_l10n::tr!("WDA 不可用,也不知道屏幕尺寸;等 WDA 起来一次,或在 ~/.config/iphone-control/env 里设 SCREEN_W/SCREEN_H", "WDA is unavailable and the screen size is unknown; wait for WDA to start once, or set SCREEN_W/SCREEN_H in ~/.config/iphone-control/env").into())
    }

    /// 每次 HID 触摸前重新截图核对横竖，不依赖上次截图（转屏、回主屏、解锁都可能改变方向）。
    /// 截图只给宽高，不能分辨横屏左右；短暂掉线时借用近期实测方向，过期或不匹配就不触摸。
    /// A device below iOS 27 has no HID fallback: said at once, like every other action, not after a screenshot
    /// whose orientation doesn't matter (it used to answer "landscape without a fresh WDA rotation… or turn it")
    async fn hid_touch_rotation(&self, g: &mut Inner) -> crate::error::Result<Rotation> {
        if !hid_supported(g.info.as_ref()) {
            return Err(wda_down(&g.state, g.info.as_ref()).into());
        }
        // 原生控制：SpringBoard 直接给出四个方向（lockdown 服务，不用截图）
        if self.native(g)
            && let Some(r) = self.native_rotation(g).await
        {
            g.dev.last_rotation = Some((Instant::now(), r));
            return Ok(r);
        }
        self.sample_display(g, false).await;
        let png = self.hid_screenshot(g).await.map_err(|e| {
            ictl_l10n::trf!(
                "核对 HID 方向失败，没有发送触摸：{e}",
                "Checking the HID orientation failed, no touch sent: {e}"
            )
        })?;
        let (w, h) = imaging::png_size(&png)
            .ok_or(ictl_l10n::tr!("无法读取截图尺寸，没有发送触摸", "Can't read the screenshot size, no touch sent"))?;
        let known = g.dev.last_rotation.map(|(at, r)| (at.elapsed(), r));
        Ok(hid_rotation(w, h, known)?)
    }

    /// A screenshot as PNG for comparing pictures: through WDA, or natively through the tunnel; otherwise WDA's
    /// "unavailable"
    async fn raw_png(&self, g: &mut Inner) -> crate::error::Result<Vec<u8>> {
        if let Some(w) = self.wda(g) {
            return Ok(w.screenshot_png().await?);
        }
        if self.native(g) {
            return self.hid_screenshot(g).await;
        }
        Ok(self.require_wda(g)?.screenshot_png().await?)
    }

    /// A tap at a point: through WDA, or natively through HID
    async fn tap_at(&self, g: &mut Inner, x: f64, y: f64) -> crate::error::Result<()> {
        if let Some(w) = self.wda(g) {
            return w.tap(x, y).await.map_err(Error::wda_input);
        }
        if self.native(g) {
            return Box::pin(self.native_tap(g, x, y)).await;
        }
        self.require_wda(g).map_err(Error::preflight)?.tap(x, y).await.map_err(Error::wda_input)
    }

    /// 经隧道截图（WDA 不可用时）。失败就丢掉隧道，下次重建
    async fn hid_screenshot(&self, g: &mut Inner) -> crate::error::Result<Vec<u8>> {
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let t = self.ensure_tunnel(g, &device).await?;
        // boxed: the screenshot service's future is large (tens of KB), and every tool that may take one carries it
        match Box::pin(t.screenshot_png()).await {
            Ok(p) => Ok(p),
            Err(e) => {
                drop_tunnel(g).await;
                Err(e.into())
            }
        }
    }

    /// HID 会话（WDA 不可用时的后备路线）。开不起来就丢掉隧道，下次重建
    pub(crate) async fn hid<'a>(&self, g: &'a mut Inner) -> crate::error::Result<&'a mut HidSession> {
        let rate = video_bitrate(g);
        self.hid_at(g, rate).await
    }

    /// [`Self::hid`], its screen video capped at `max_bitrate` if it has to be opened
    pub(crate) async fn hid_at<'a>(
        &self,
        g: &'a mut Inner,
        max_bitrate: u32,
    ) -> crate::error::Result<&'a mut HidSession> {
        // 低版本建隧道只会报 service not found：直接说清楚
        if !hid_supported(g.info.as_ref()) {
            return Err(wda_down(&g.state, g.info.as_ref()).into());
        }
        let screen = self.screen(g).await?;
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        // HID 触摸按设备竖放的坐标归一化
        let screen = screen.portrait();
        let t = self
            .ensure_tunnel(g, &device)
            .await
            .map_err(|e| ictl_l10n::trf!("HID 后备路线不可用：{e}", "HID fallback unavailable: {e}"))?;
        // boxed, as the screenshot above: opening the session is a large future
        if let Err(e) = Box::pin(t.hid(screen, max_bitrate)).await {
            drop_tunnel(g).await;
            return Err(ictl_l10n::trf!("HID 后备路线不可用：{e}", "HID fallback unavailable: {e}").into());
        }
        // 第二次调用直接返回已开好的会话（这样写是为了绕开借用检查对条件返回的限制）
        Ok(Box::pin(g.tunnel.as_mut().expect("刚建好").hid(screen, max_bitrate)).await?)
    }

    async fn hid_do<F>(&self, g: &mut Inner, f: F) -> crate::error::Result<()>
    where
        F: for<'h> FnOnce(
            &'h mut HidSession,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ictl_device::Result<()>> + Send + 'h>>,
    {
        let r = f(self.hid(g).await.map_err(|e| Error::action(Delivery::NotSent, FailureStage::Connect, e))?).await;
        g.hid_used = Some(Instant::now());
        match r {
            Ok(()) => Ok(()),
            // 参数问题（例如不能输入的字符），连接没坏，不用重建
            Err(e @ DeviceError::InvalidInput(_)) => Err(Error::preflight(e)),
            Err(e) => {
                drop_tunnel(g).await;
                Err(Error::action(
                    Delivery::Unknown,
                    FailureStage::Send,
                    ictl_l10n::trf!("HID 命令失败: {e}", "HID command failed: {e}"),
                ))
            }
        }
    }

    /// 接着多台、或选定的那台没接时，说明在控制哪台、怎么切换；只接着一台时为空（输出和原来一样）
    pub(crate) fn devices_line(&self, g: &Inner) -> Option<String> {
        let pinned = self.pinned(g);
        let current = g.device.as_ref().map(|d| d.udid.as_str());
        let others: Vec<String> = self
            .connected_infos(g)
            .iter()
            .filter(|i| Some(i.udid.as_str()) != current)
            .map(crate::core::device_label)
            .collect();
        let how = if self.cfg.udid.is_some() {
            ictl_l10n::tr!(
                "(IPHONE_UDID 固定了设备,要换先改 ~/.config/iphone-control/env)",
                " (IPHONE_UDID pins the device; change ~/.config/iphone-control/env to switch)"
            )
        } else {
            ictl_l10n::tr!(
                ";用 iphone-control select 或 iphone_select_device 工具切换",
                "; switch with iphone-control select or the iphone_select_device tool"
            )
        };
        match (current, &pinned) {
            (None, Some(p)) => Some(ictl_l10n::trf!(
                "设备: 选定的 {p} 没连接{}{how}",
                "Device: the selected {p} is not connected{}{how}",
                if others.is_empty() {
                    String::new()
                } else {
                    ictl_l10n::trf!(",接着的有 {}", ", connected are {}", others.join(ictl_l10n::tr!("、", ", ")))
                }
            )),
            (Some(_), _) if !others.is_empty() => {
                let me = g.info.as_ref().map(crate::core::device_label).unwrap_or_default();
                let mode = if pinned.is_some() {
                    ictl_l10n::tr!("选定", "selected")
                } else {
                    ictl_l10n::tr!("自动选", "automatic")
                };
                Some(ictl_l10n::trf!(
                    "设备: 正在控制 {me}({mode});另外接着 {}{how}",
                    "Device: controlling {me} ({mode}); also connected {}{how}",
                    others.join(ictl_l10n::tr!("、", ", "))
                ))
            }
            _ => None,
        }
    }

    /// The status for people and agents, and the same as data for programs (the control page)
    pub(crate) async fn status(&self, g: &mut Inner) -> (String, StatusData) {
        let mut daemon_line = ictl_l10n::trf!("守护进程: {}", "Daemon: {}", g.state.describe());
        if let Some(l) = self.devices_line(g) {
            daemon_line = format!("{l}\n{daemon_line}");
        }
        let unlock = self.unlock_cfg(g.device.as_ref().map(|d| d.udid.as_str()));
        let Some(w) = self.wda(g) else {
            if self.native(g) {
                if g.state == crate::machine::DeviceState::Ready {
                    let (text, data) = Box::pin(self.native_status(g, &unlock)).await;
                    return (format!("{text}\n{daemon_line}"), data);
                }
                let text = format!("{}\n{daemon_line}", wda_down(&g.state, g.info.as_ref()));
                return (text, StatusData { native: true, ..Default::default() });
            }
            let text = format!("{}\n{}\n{daemon_line}", wda_down(&g.state, g.info.as_ref()), needs_wda());
            return (text, StatusData::default());
        };
        let r: Result<(String, StatusData), WdaError> = async {
            let ready = w.ready().await?;
            let app = w.active_app().await?;
            let s = w.screen_size().await?;
            let check = w.check_lock(None, false).await?;
            let locked = check.locked;
            let lock = lock_text(locked, check.uncertain, &unlock);
            let data = StatusData {
                wda_ready: ready,
                locked: Some(locked || check.uncertain),
                possibly_locked: check.uncertain,
                foreground: Some(app.clone()).filter(|a| !a.is_empty()),
                width: Some(s.width),
                height: Some(s.height),
                native: false,
            };
            let ready = if ready { "True" } else { "False" };
            let text = ictl_l10n::trf!(
                "WDA 就绪: {ready}\n锁屏: {lock}\n前台 App: {app}\n屏幕: {}x{} 点",
                "WDA ready: {ready}\nLocked: {lock}\nForeground app: {app}\nScreen: {}x{} points",
                fmt_g(s.width),
                fmt_g(s.height)
            );
            Ok((text, data))
        }
        .await;
        match r {
            Ok((t, data)) => (format!("{t}\n{daemon_line}"), data),
            Err(e) => (format!("{e}\n{}\n{daemon_line}", needs_wda()), StatusData::default()),
        }
    }

    pub(crate) async fn screenshot(
        &self,
        g: &mut Inner,
        region: Option<(f64, f64, f64, f64)>,
    ) -> crate::error::Result<Response> {
        self.ensure_unlocked(g).await?;
        let fetched = Instant::now();
        let wda_png = match self.wda(g) {
            Some(w) => w.screenshot_png().await.ok(),
            None => None,
        };
        // 给短暂 WDA 掉线时的 HID 留一份近期方向；不把方向永久缓存到磁盘。
        if wda_png.is_some() {
            self.sample_display(g, false).await;
        }
        let (png, via) = match wda_png {
            Some(p) => (p, "WDA"),
            None => {
                if !hid_supported(g.info.as_ref()) {
                    return Err(wda_down(&g.state, g.info.as_ref()).into());
                }
                (
                    self.hid_screenshot(g)
                        .await
                        .map_err(|e| ictl_l10n::trf!("HID 命令失败: {e}", "HID command failed: {e}"))?,
                    "HID",
                )
            }
        };
        let screen = self.screen(g).await?;
        // 解码、缩放、JPEG 编码是 CPU 活（1290×2796），放到阻塞线程，不卡住主循环
        // 说明文字在阻塞线程里生成：带上这个请求的语言
        let lang = ictl_l10n::lang();
        let (png_len, processed) = (png.len(), Instant::now());
        let fetch_ms = processed.duration_since(fetched).as_millis();
        let (jpeg, caption, thumb) = tokio::task::spawn_blocking(move || {
            ictl_l10n::with_lang(lang, || imaging::answer(&png, screen, region, via))
        })
        .await
        .map_err(|e| ictl_l10n::trf!("图像处理任务异常：{e}", "Image processing task failed: {e}"))??;
        tracing::debug!(
            "screenshot via {via}: {png_len} bytes of PNG in {fetch_ms} ms, processed in {} ms",
            processed.elapsed().as_millis()
        );
        // 区域放大也记整屏：下一个操作做完后比的是整个画面
        g.dev.seen.saw_thumb(thumb);
        Ok(Response::Image { jpeg_base64: base64::engine::general_purpose::STANDARD.encode(jpeg), caption })
    }
}

/// 只给 WDA 短暂掉线留窗口。缓存不能跨进程重启、跨设备或长期代替方向查询。
const HID_ROTATION_MAX_AGE: Duration = Duration::from_secs(30);

/// 当前截图必须能确定横竖；横屏还需近期 WDA 旋转角。仅凭宽高无法识别 WDA 离线期间的 180° 翻转，
/// 所以后备操作期间不要翻转设备；恢复 WDA 后重新确认方向。
pub(crate) fn hid_rotation(w: u32, h: u32, known: Option<(Duration, Rotation)>) -> Result<Rotation, String> {
    if w == 0 || h == 0 || w == h {
        return Err(ictl_l10n::tr!(
            "截图尺寸无效，无法核对 HID 方向，没有发送触摸",
            "Invalid screenshot size, can't check the HID orientation, no touch sent"
        )
        .into());
    }
    let recent = known.filter(|(age, _)| *age <= HID_ROTATION_MAX_AGE).map(|(_, r)| r);
    if w > h {
        return recent.filter(|r| r.is_landscape()).ok_or_else(|| {
            ictl_l10n::tr!("当前为横屏，但没有近期且匹配的 WDA 旋转角，没有发送 HID 触摸；等 WDA 恢复后重试，或转回竖屏", "The screen is landscape but there is no recent matching WDA rotation, no HID touch sent; retry once WDA is back, or rotate to portrait").into()
        });
    }
    Ok(if recent == Some(Rotation::UpsideDown) { Rotation::UpsideDown } else { Rotation::Portrait })
}

/// 判断画面动没动用的缩略图。解码是 CPU 活，放到阻塞线程
async fn thumbnail(png: Vec<u8>) -> crate::error::Result<Vec<u8>> {
    // 阻塞线程上没有请求的语言，带过去（解码失败的报错会返回给调用方）
    let lang = ictl_l10n::lang();
    tokio::task::spawn_blocking(move || ictl_l10n::with_lang(lang, || imaging::thumbnail(&png)))
        .await
        .map_err(|e| ictl_l10n::trf!("图像处理任务异常：{e}", "Image processing task failed: {e}"))?
        .map_err(Into::into)
}

/// The status's lock line
pub(super) fn lock_text(locked: bool, uncertain: bool, unlock: &ictl_unlock::UnlockConfig) -> String {
    let mut lock = if uncertain {
        ictl_l10n::tr!("可能已锁屏(屏幕上是锁屏样式的界面,WDA 说没锁;配置了自动解锁时操作前会先尝试解锁)", "possibly locked (the screen looks like the lock screen though WDA says unlocked; with auto-unlock configured, actions try to unlock first)").to_owned()
    } else if !locked {
        ictl_l10n::tr!("未锁屏", "unlocked").to_owned()
    } else if unlock.disabled_by_failure() {
        ictl_l10n::tr!("已锁屏(自动解锁之前失败过已停用,需要人工解锁,核对密码后运行 iphone-control unlock-reset)", "locked (auto-unlock was disabled after an earlier failure; unlock by hand, check the passcode, then run iphone-control unlock-reset)").to_owned()
    } else if unlock.enabled() {
        ictl_l10n::tr!("已锁屏(操作时会自动解锁)", "locked (actions unlock automatically)").to_owned()
    } else {
        ictl_l10n::tr!("已锁屏(未配置自动解锁,需要人工解锁)", "locked (no auto-unlock configured; unlock by hand)")
            .to_owned()
    };
    if !locked && unlock.disabled_by_failure() {
        lock += ictl_l10n::tr!(";自动解锁已停用", "; auto-unlock disabled");
    }
    lock
}

fn needs_wda() -> &'static str {
    if ictl_l10n::is_en() { NEEDS_WDA_EN } else { NEEDS_WDA }
}

/// The screen video's bitrate cap for a HID session opened now: low while a page in low-bandwidth mode watches
/// (`crate::screen` opens the session again when that changes)
pub(crate) fn video_bitrate(g: &Inner) -> u32 {
    if g.low_bandwidth_viewers > 0 { ictl_device::LOW_BANDWIDTH_BITRATE } else { ictl_device::FULL_BITRATE }
}

#[cfg(test)]
mod tests {
    use super::input::*;
    use super::keys::*;
    use super::*;

    /// 操作前后的界面：树比树，没有树比画面，什么都没有不说
    #[test]
    fn screen_change_is_judged_by_tree_then_picture() {
        use super::observe::{Change, Seen, compare};
        let tree = |label: &str, y: i64| {
            let xml = format!(
                r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" label="A" x="0" y="0" width="430" height="932"><XCUIElementTypeButton type="XCUIElementTypeButton" label="{label}" x="20" y="{y}" width="100" height="44"/></XCUIElementTypeApplication>"#
            );
            elements::parse_tree(
                &xml,
                elements::View { screen: ScreenSize { width: 430.0, height: 932.0 }, orientation: None },
            )
        };
        let mut before = Seen::default();
        before.saw_tree(&tree("确定", 100));
        assert_eq!(compare(&before, Some(&tree("确定", 102)), None), Change::Same, "moved within the tolerance");
        assert_eq!(compare(&before, Some(&tree("确定", 300)), None), Change::Changed);
        assert_eq!(compare(&before, Some(&tree("取消", 100)), None), Change::Changed);
        // no tree after (a game, a map): the picture, if one was taken before
        assert_eq!(compare(&before, None, Some(&[0u8; 10])), Change::Unknown);
        let n = (imaging::THUMB_W * imaging::THUMB_H) as usize;
        let mut pic = Seen::default();
        pic.saw_thumb(vec![200; n]);
        assert_eq!(compare(&pic, None, Some(&vec![200; n])), Change::Same);
        // one small area changed (a switch flipped): the mean hardly moves, the tile does
        let mut flipped = vec![200u8; n];
        for y in 0..8 {
            for x in 0..8 {
                flipped[y * imaging::THUMB_W as usize + x] = 40;
            }
        }
        assert_eq!(compare(&pic, None, Some(&flipped)), Change::Changed);
        assert_eq!(compare(&Seen::default(), Some(&tree("确定", 100)), None), Change::Unknown);
        assert!(super::observe::change_text(Change::Unknown).is_empty());
    }

    /// 边滑边找只在文字完全相同时停下：找「App」时屏幕上只有「Apple账户」，包含它但不算找到，要接着滑
    #[test]
    fn a_scroll_search_stops_only_on_an_exact_match() {
        let row = |label: &str, y: u32| {
            format!(
                r#"<XCUIElementTypeButton type="XCUIElementTypeButton" label="{label}" name="{label}" x="20" y="{y}" width="390" height="52"/>"#
            )
        };
        let xml = |rows: &str| {
            format!(
                r#"<XCUIElementTypeApplication type="XCUIElementTypeApplication" x="0" y="0" width="430" height="932"><XCUIElementTypeWindow type="XCUIElementTypeWindow" x="0" y="0" width="430" height="932">{rows}</XCUIElementTypeWindow></XCUIElementTypeApplication>"#
            )
        };
        let screen = ictl_device::ScreenSize { width: 430.0, height: 932.0 };
        let top = elements::parse_tree(&xml(&row("Apple账户、iCloud+等", 200)), screen);
        assert!(input::reachable(&top, "App", 0), "a row that contains it");
        assert!(!input::reachable_exact(&top, "App", 0), "but not the one asked for");
        let bottom = elements::parse_tree(&xml(&(row("钱包与 Apple Pay", 300) + &row("App", 600))), screen);
        assert!(input::reachable_exact(&bottom, "App", 0));
    }

    /// 边滑边找的一次滑动在屏幕正中竖着滑，往下找时手指往上；滑的距离不到半屏，前后两屏有重叠
    #[test]
    fn a_search_scroll_is_a_short_vertical_stroke_in_the_middle() {
        let (from, to) = input::search_stroke(375.0, 812.0, true);
        assert_eq!(from, (188.0, 552.0));
        assert_eq!(to, (188.0, 268.0));
        assert!(from.1 - to.1 < 812.0 / 2.0);
        assert_eq!(input::search_stroke(375.0, 812.0, false), (to, from));
    }

    /// 同一个操作原样重做、界面还是没变，从第二次起说是第几次；换了操作、参数不同、界面变了都重新数，
    /// 没法比（操作前没看过界面）的那次不数也不清零
    #[test]
    fn the_same_action_leaving_the_screen_unchanged_is_counted() {
        use super::observe::{Change, Repeats, repeat_text};
        use crate::protocol::Request;
        let tap = |x| Request::Tap { x, y: 640.0, count: 1 };
        let mut r = Repeats::default();
        let mut run = |req: &Request, c| {
            r.start(req);
            r.settled(c)
        };
        assert_eq!(run(&tap(232.0), Change::Same), 1);
        assert_eq!(run(&tap(232.0), Change::Same), 2);
        assert_eq!(run(&tap(232.0), Change::Unknown), 0);
        assert_eq!(run(&tap(232.0), Change::Same), 3);
        // another point is another action
        assert_eq!(run(&tap(100.0), Change::Same), 1);
        assert_eq!(run(&tap(100.0), Change::Changed), 0);
        assert_eq!(run(&tap(100.0), Change::Same), 1);
        // something else in between starts the count again
        assert_eq!(run(&Request::PressButton { button: "home".into() }, Change::Unknown), 0);
        assert_eq!(run(&tap(100.0), Change::Same), 1);
        // a settle without an action counts nothing
        assert_eq!(r.settled(Change::Same), 0);
        assert!(repeat_text(0).is_empty() && repeat_text(1).is_empty());
        assert!(repeat_text(3).contains('3'));
    }

    /// 太久以前看过的界面不拿来比；拿走以后就没了
    #[test]
    fn seen_screens_are_taken_once() {
        use super::observe::Seen;
        let mut s = Seen::default();
        assert!(s.take_fresh().is_none());
        s.saw_thumb(vec![1]);
        assert!(s.take_fresh().is_some());
        assert!(s.take_fresh().is_none());
    }

    /// 默认先短按电源键；记过这台设备上它不灵，就先用 WDA 的。两种都会试
    #[test]
    fn a_failed_lock_says_why_not_that_wda_is_down() {
        let m = lock_failed(&[
            "still not locked 3 s after pressing the power key via IOHID".into(),
            "WDA's lock returned an error: x".into(),
        ]);
        assert!(m.contains("3 s") && m.contains("WDA's lock returned an error: x"), "{m}");
        assert!(!m.contains("unavailable") && !m.contains("不可用") && !m.contains("掉线"), "{m}");
    }

    #[test]
    fn lock_order_follows_what_worked_on_this_device() {
        assert_eq!(lock_order(false), [LockWay::PowerKey, LockWay::Wda]);
        assert_eq!(lock_order(true), [LockWay::Wda, LockWay::PowerKey]);
        let mut p = crate::core::Persisted::default();
        assert!(!p.ways_for(Some("A")).lock_via_wda, "a new device starts with the power key");
        assert!(p.record_ways("A", |w| w.lock_via_wda = true));
        assert!(!p.record_ways("a", |w| w.lock_via_wda = true), "the same device, nothing new");
        assert!(
            p.ways_for(Some("a")).lock_via_wda && !p.ways_for(Some("B")).lock_via_wda && !p.ways_for(None).lock_via_wda
        );
        // kept in state.json; an older one without it reads as nothing known
        let back: crate::core::Persisted = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back.ways_for(Some("A")), p.ways_for(Some("A")));
        let old: crate::core::Persisted = serde_json::from_str(r#"{"team_id":null,"selected_udid":null}"#).unwrap();
        assert!(old.button_ways.is_empty());
    }

    #[tokio::test]
    async fn missing_hid_session_allows_retry_for_down_only() {
        let (d, dir) = crate::core::tests::test_daemon("touch-no-hid");
        let mut g = d.inner.lock().await;
        // Older devices cannot open a HID session. Only a Down that already passed
        // coordinate preparation can opt in to the whole-gesture fallback.
        g.info = Some(ictl_device::DeviceInfo { product_version: "18.0".into(), ..Default::default() });
        for phase in [TouchPhase::Down, TouchPhase::Move, TouchPhase::Up] {
            g.dev.touch_rotation = Some(Rotation::Portrait);
            let err = d.send_touch(&mut g, phase, 20., 50.).await.unwrap_err();
            assert!(
                matches!(err, Response::Error { retry_as_gesture, .. } if retry_as_gesture == (phase == TouchPhase::Down))
            );
            assert!(g.dev.touch_rotation.is_none());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Without WDA, a device below iOS 27 gets the usual "no HID fallback" for taps, swipes and a finger, at once:
    /// not an orientation error after a screenshot (iPad mini 5, iPadOS 26: "turn it to portrait")
    #[tokio::test]
    async fn no_hid_fallback_is_said_before_any_orientation_check() {
        let (d, dir) = crate::core::tests::test_daemon("no-hid-touch");
        let mut g = d.inner.lock().await;
        g.info = Some(ictl_device::DeviceInfo { product_version: "26.7.3".into(), ..Default::default() });
        g.state = crate::machine::DeviceState::WdaStarting { last_error: None };
        let down = |e: &str| e.contains("没有 HID 后备路线") || e.contains("no HID fallback");
        let tap = d.tap(&mut g, 20., 50., 1, None).await.unwrap_err().to_string();
        assert!(down(&tap), "{tap}");
        let swipe = d
            .swipe(&mut g, Stroke { from: (20., 50.), to: (20., 300.), seconds: 0.3, hold: 0.0 }, None, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(down(&swipe), "{swipe}");
        // a finger: the same, and the page may still send the whole gesture once WDA is back
        match d.touch(&mut g, TouchPhase::Down, 20., 50., None).await {
            Err(Response::Error { message, retry_as_gesture: true, .. }) => assert!(down(&message), "{message}"),
            other => panic!("{other:?}"),
        }
        assert!(g.dev.touch_rotation.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hid_rotation_requires_fresh_matching_landscape() {
        use Rotation::*;
        let fresh = |r| Some((Duration::ZERO, r));
        assert_eq!(hid_rotation(2796, 1290, fresh(TopToLeft)), Ok(TopToLeft));
        assert_eq!(hid_rotation(2796, 1290, fresh(TopToRight)), Ok(TopToRight));
        assert!(hid_rotation(2796, 1290, None).is_err());
        assert!(hid_rotation(2796, 1290, fresh(Portrait)).is_err());
        assert!(hid_rotation(2796, 1290, Some((Duration::from_secs(31), TopToLeft))).is_err());
        assert_eq!(hid_rotation(1290, 2796, fresh(TopToLeft)), Ok(Portrait));
        assert_eq!(hid_rotation(1290, 2796, fresh(UpsideDown)), Ok(UpsideDown));
        assert_eq!(hid_rotation(1290, 2796, None), Ok(Portrait));
        assert!(hid_rotation(0, 2796, fresh(Portrait)).is_err());
        assert!(hid_rotation(100, 100, fresh(Portrait)).is_err());
    }

    #[test]
    fn no_match_with_filter_names_the_filter() {
        assert_eq!(no_elements_text("", 0), "没有找到带文字的元素");
        assert_eq!(no_elements_text("通用", 0), "没有找到带文字的元素");
        let t = no_elements_text("通用", 12);
        assert!(t.contains("\"通用\"") && t.contains("12 个"), "{t}");
    }

    #[test]
    fn input_readback_requires_new_evidence_in_the_same_field() {
        assert!(newly_typed("", "130 1234 5678", "13012345678"));
        assert!(!newly_typed("", "helloworld", "hello world"));
        assert!(!newly_typed("", "hello", "hello "));
        assert!(newly_typed("", "hello world", "hello world"));
        assert!(!newly_typed("1", "11", "11"));
        assert!(newly_typed("ab", "a测b", "测"));
        assert!(newly_typed("abc", "abcabc", "abc"));
        assert!(!newly_typed("abc", "abc", "abc"));
        assert!(!newly_typed("", "故宫博", "故宫博物院"));
    }

    #[test]
    fn input_repair_requires_a_stable_new_prefix() {
        assert_eq!(missing_suffix("abc", "abc故宫", "abc故宫", "故宫博物院"), Some("博物院"));
        assert_eq!(missing_suffix("abc故宫", "abc故宫", "abc故宫", "故宫博物院"), None);
        assert_eq!(missing_suffix("", "故", "故宫", "故宫博物院"), None);
        assert_eq!(missing_suffix("abc", "def故宫", "def故宫", "故宫博物院"), None);
        for text in ["hi\n", "hi\t", "hi ", "hi\nmore"] {
            assert_eq!(missing_suffix("", "hi", "hi", text), None);
        }
        assert_eq!(missing_suffix("", "138 0013 8", "13800138", "13800138000"), Some("000"));
        assert_eq!(missing_suffix("138 00", "138 0013 8", "13800138", "138000"), Some("000"));
        assert_eq!(missing_suffix("", "138 00", "138001", "13800138"), None);
    }
}
