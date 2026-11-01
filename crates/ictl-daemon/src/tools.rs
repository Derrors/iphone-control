//! 各工具请求的实现：优先 WDA，WDA 连不上时坐标类操作退到 HID。调用方持有设备锁。

use std::time::{Duration, Instant};

use base64::Engine;
use ictl_device::{AppInfo, Button, DeviceError, HidSession, ScreenSize, apps, hid::ascii_to_usage};
use ictl_unlock::UnlockError;
use ictl_wda::{WdaError, elements, swipe_steps};

use crate::core::{Daemon, Inner, drop_tunnel, wda_down};
use crate::imaging::{self, fmt_g};
use crate::notify::Event;
use crate::protocol::Response;

/// 锁屏后至少等这么久再自动解锁：锁屏过渡没走完就唤醒，WDA 第一下上滑卡约 16 秒后报 `point.x != INFINITY`
pub(crate) const LOCK_SETTLE: Duration = Duration::from_secs(3);

impl Daemon {
    /// 配置了密码就在锁屏时自动解锁；没配置而手机锁着则报错（与 toolkit 一致）
    pub(crate) async fn ensure_unlocked(&self, g: &mut Inner) -> Result<(), String> {
        let cfg = self.unlock_cfg();
        if let Some(left) = g.last_lock.and_then(|t| LOCK_SETTLE.checked_sub(t.elapsed())) {
            tokio::time::sleep(left).await;
        }
        let Some(wda) = self.wda(g) else { return Ok(()) };
        match ictl_unlock::ensure_unlocked(wda, &cfg).await {
            Ok(_) => Ok(()),
            Err(e @ (UnlockError::WrongPasscode | UnlockError::Uncertain(_))) => {
                self.notify(g, Event::UnlockDisabled);
                Err(format!("{e},已推送通知"))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    async fn screen(&self, g: &mut Inner) -> Result<ScreenSize, String> {
        if let Some(w) = self.wda(g)
            && let Ok(s) = w.screen_size().await
        {
            return Ok(s);
        }
        g.persisted
            .screen
            .or(self.cfg.screen_fallback)
            .ok_or_else(|| "WDA 不可用,也不知道屏幕尺寸;等 WDA 起来一次,或在 .env 里设 SCREEN_W/SCREEN_H".into())
    }

    /// HID 会话（WDA 不可用时的后备路线）。开不起来就丢掉隧道，下次重建
    async fn hid<'a>(&self, g: &'a mut Inner) -> Result<&'a mut HidSession, String> {
        let screen = self.screen(g).await?;
        let device = g.device.clone().ok_or("手机未连接")?;
        let t = self.ensure_tunnel(g, &device).await.map_err(|e| format!("HID 后备路线不可用：{e}"))?;
        if let Err(e) = t.hid(screen).await {
            drop_tunnel(g).await;
            return Err(format!("HID 后备路线不可用：{e}"));
        }
        // 第二次调用直接返回已开好的会话（这样写是为了绕开借用检查对条件返回的限制）
        g.tunnel.as_mut().expect("刚建好").hid(screen).await.map_err(|e| e.to_string())
    }

    async fn hid_do<F>(&self, g: &mut Inner, f: F) -> Result<(), String>
    where
        F: for<'h> FnOnce(
            &'h mut HidSession,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ictl_device::Result<()>> + Send + 'h>>,
    {
        let r = f(self.hid(g).await?).await;
        g.hid_used = Some(Instant::now());
        match r {
            Ok(()) => Ok(()),
            // 参数问题（例如不能输入的字符），连接没坏，不用重建
            Err(e @ DeviceError::InvalidInput(_)) => Err(e.to_string()),
            Err(e) => {
                drop_tunnel(g).await;
                Err(format!("HID 命令失败: {e}"))
            }
        }
    }

    pub(crate) async fn status_text(&self, g: &mut Inner) -> String {
        let daemon_line = format!("守护进程: {}", g.state.describe());
        let unlock = self.unlock_cfg();
        let Some(w) = self.wda(g) else {
            return format!(
                "{}\n读界面元素、按名字点击、启动 App、输入中文要等 WDA 恢复\n{daemon_line}",
                wda_down(&g.state)
            );
        };
        let r: Result<String, WdaError> = async {
            let ready = w.ready().await?;
            let app = w.active_app().await?;
            let s = w.screen_size().await?;
            let locked = w.locked().await?;
            let mut lock = if !locked {
                "未锁屏".to_owned()
            } else if unlock.disabled_by_failure() {
                "已锁屏(自动解锁之前失败过已停用,需要人工解锁,核对密码后运行 iphone-control unlock-reset)".to_owned()
            } else if unlock.enabled() {
                "已锁屏(操作时会自动解锁)".to_owned()
            } else {
                "已锁屏(未配置自动解锁,需要人工解锁)".to_owned()
            };
            if !locked && unlock.disabled_by_failure() {
                lock += ";自动解锁已停用";
            }
            let ready = if ready { "True" } else { "False" };
            Ok(format!(
                "WDA 就绪: {ready}\n锁屏: {lock}\n前台 App: {app}\n屏幕: {}x{} 点",
                fmt_g(s.width),
                fmt_g(s.height)
            ))
        }
        .await;
        match r {
            Ok(t) => format!("{t}\n{daemon_line}"),
            Err(e) => format!("{e}\n读界面元素、按名字点击、启动 App、输入中文要等 WDA 恢复\n{daemon_line}"),
        }
    }

    pub(crate) async fn screenshot(
        &self,
        g: &mut Inner,
        region: Option<(f64, f64, f64, f64)>,
    ) -> Result<Response, String> {
        self.ensure_unlocked(g).await?;
        let wda_png = match self.wda(g) {
            Some(w) => w.screenshot_png().await.ok(),
            None => None,
        };
        let (png, via) = match wda_png {
            Some(p) => (p, "WDA"),
            None => {
                let device = g.device.clone().ok_or("手机未连接")?;
                let t = self.ensure_tunnel(g, &device).await.map_err(|e| format!("HID 命令失败: {e}"))?;
                match t.screenshot_png().await {
                    Ok(p) => (p, "HID"),
                    Err(e) => {
                        drop_tunnel(g).await;
                        return Err(format!("HID 命令失败: {e}"));
                    }
                }
            }
        };
        let screen = self.screen(g).await?;
        // 解码、缩放、JPEG 编码是 CPU 活（1290×2796），放到阻塞线程，不卡住主循环
        let (jpeg, caption) = tokio::task::spawn_blocking(move || match region {
            None => imaging::full_screen(&png, screen, via),
            Some(r) => imaging::region(&png, screen, r, via),
        })
        .await
        .map_err(|e| format!("图像处理任务异常：{e}"))??;
        Ok(Response::Image { jpeg_base64: base64::engine::general_purpose::STANDARD.encode(jpeg), caption })
    }

    pub(crate) async fn list_elements(&self, g: &mut Inner, filter: &str, limit: usize) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let els = self.require_wda(g)?.elements().await.map_err(|e| e.to_string())?;
        let shown: Vec<&elements::Element> =
            if filter.is_empty() { els.iter().collect() } else { elements::filter(&els, filter) };
        let lines: Vec<_> = shown.iter().take(limit).map(|e| e.describe()).collect();
        let more = if shown.len() > limit {
            format!("\n……另有 {} 个未列出,可用 filter 缩小范围", shown.len() - limit)
        } else {
            String::new()
        };
        Ok(if lines.is_empty() { "没有找到带文字的元素".into() } else { lines.join("\n") } + &more)
    }

    pub(crate) async fn tap(&self, g: &mut Inner, x: f64, y: f64) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let pos = format!("({},{})", fmt_g(x), fmt_g(y));
        if let Some(w) = self.wda(g) {
            // 只有连不上才退到 HID；超时说明请求已发出，再用 HID 做一遍可能点两次
            match w.tap(x, y).await {
                Ok(()) => return Ok(format!("已点击 {pos}")),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        self.hid_do(g, |h| Box::pin(h.tap(x, y))).await?;
        Ok(format!("已点击 {pos}(经 HID)"))
    }

    pub(crate) async fn tap_element(&self, g: &mut Inner, text: &str, index: usize) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let w = self.require_wda(g)?;
        let els = w.elements().await.map_err(|e| e.to_string())?;
        let matches = elements::find(&els, text);
        if matches.is_empty() {
            return Ok(format!("没找到包含 \"{text}\" 的元素,先用 iphone_list_elements 或截图看看屏幕"));
        }
        if index >= matches.len() {
            let list: Vec<_> = matches.iter().map(|e| e.describe()).collect();
            return Ok(format!("只匹配到 {} 个:\n{}", matches.len(), list.join("\n")));
        }
        let e = matches[index];
        w.tap(e.center.0 as f64, e.center.1 as f64).await.map_err(|e| e.to_string())?;
        let note = if matches.len() > 1 {
            format!("(共匹配 {} 个,点的是第 {index} 个)", matches.len())
        } else {
            String::new()
        };
        Ok(format!("已点击 {}{note}", e.describe()))
    }

    pub(crate) async fn long_press(&self, g: &mut Inner, x: f64, y: f64, seconds: f64) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.touch_and_hold(x, y, seconds).await.map_err(|e| e.to_string())?;
        Ok(format!("已长按 ({},{}) {} 秒", fmt_g(x), fmt_g(y), fmt_g(seconds)))
    }

    pub(crate) async fn swipe(
        &self,
        g: &mut Inner,
        from: (f64, f64),
        to: (f64, f64),
        seconds: f64,
    ) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let desc = format!("({},{}) → ({},{})", fmt_g(from.0), fmt_g(from.1), fmt_g(to.0), fmt_g(to.1));
        if let Some(w) = self.wda(g) {
            match w.actions(&swipe_steps(from, to, (seconds * 1000.0) as u64)).await {
                Ok(()) => return Ok(format!("已滑动 {desc}")),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        self.hid_do(g, |h| Box::pin(h.drag(from, to, seconds))).await?;
        Ok(format!("已滑动 {desc}(经 HID)"))
    }

    pub(crate) async fn type_text(&self, g: &mut Inner, text: &str, submit: bool) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let payload = if submit { format!("{text}\n") } else { text.to_owned() };
        let n = text.chars().count();
        if let Some(w) = self.wda(g) {
            match w.keys(&payload).await {
                Ok(()) => return Ok(format!("已输入 {n} 个字符{}", if submit { ",并按了回车" } else { "" })),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        // 先检查，一个都不输；不打开 HID 会话
        if let Some(c) = payload.chars().find(|c| ascii_to_usage(*c).is_none()) {
            return Err(format!("WDA 不可用,HID 只能输入美式键盘上的英文和符号,不能输入 {c:?}"));
        }
        let p = payload.clone();
        self.hid_do(g, move |h| Box::pin(async move { h.type_ascii(&p).await })).await?;
        Ok(format!("已输入 {n} 个字符(经 HID)"))
    }

    pub(crate) async fn press_button(&self, g: &mut Inner, button: &str) -> Result<String, String> {
        let hid_button = Button::parse(button).ok_or("button 只能是 home / volume_up / volume_down / lock")?;
        let wda_name = match hid_button {
            Button::Lock => return self.lock_screen(g).await,
            Button::Home => "home",
            Button::VolumeUp => "volumeUp",
            Button::VolumeDown => "volumeDown",
        };
        self.ensure_unlocked(g).await?;
        if let Some(w) = self.wda(g) {
            match w.press_button(wda_name).await {
                Ok(()) => return Ok(format!("已按 {button}")),
                Err(WdaError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        self.hid_do(g, |h| Box::pin(h.press(hid_button))).await?;
        Ok(format!("已按 {button}"))
    }

    async fn lock_screen(&self, g: &mut Inner) -> Result<String, String> {
        if let Some(w) = self.wda(g) {
            // WDA 锁屏偶尔等 5 秒报 500（手机其实已锁）：确认锁了就不退到 HID（HID 会开屏幕媒体流）
            let locked = match w.lock().await {
                Ok(()) => true,
                Err(e) => {
                    tracing::info!("WDA 锁屏报错：{e}");
                    w.wda_reports_locked().await.unwrap_or(false)
                }
            };
            if locked {
                g.last_lock = Some(Instant::now());
                return Ok("已锁屏".into());
            }
        }
        self.hid_do(g, |h| Box::pin(h.press(Button::Lock))).await?;
        g.last_lock = Some(Instant::now());
        Ok("已锁屏".into())
    }

    pub(crate) async fn list_apps(&self, g: &mut Inner, include_system: bool) -> Result<Vec<AppInfo>, String> {
        if let Some(a) = g.apps.get(&include_system) {
            return Ok(a.clone());
        }
        let device = g.device.clone().ok_or("手机未连接")?;
        let a = device.list_apps(include_system).await.map_err(|e| e.to_string())?;
        g.apps.insert(include_system, a.clone());
        Ok(a)
    }

    async fn resolve_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        if apps::looks_like_bundle_id(app) {
            return Ok(app.to_owned());
        }
        if let Some(b) = self.resolve_cached(g, app).await? {
            return Ok(b);
        }
        // 守护进程启动后新装的 App 不在缓存里：刷新一次再找
        g.apps.clear();
        self.resolve_cached(g, app)
            .await?
            .ok_or_else(|| format!("找不到名为 \"{app}\" 的 App,用 iphone_list_apps 查看"))
    }

    async fn resolve_cached(&self, g: &mut Inner, app: &str) -> Result<Option<String>, String> {
        // 先只查用户 App；列系统 App 要好几秒，只在找不到时才去取
        let user = self.list_apps(g, false).await?;
        if let Some(b) = user.iter().find(|a| a.name == app) {
            return Ok(Some(b.bundle_id.clone()));
        }
        let system = self.list_apps(g, true).await?;
        Ok(apps::resolve(app, [&user[..], &system[..]]))
    }

    pub(crate) async fn launch_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        let bid = self.resolve_app(g, app).await?;
        self.ensure_unlocked(g).await?;
        if self.require_wda(g)?.launch_app(&bid).await.map_err(|e| e.to_string())? {
            Ok(format!("已启动 {bid}"))
        } else {
            Ok(format!("已启动 {bid},但 8 秒内没到前台(可能有弹窗或启动慢),先截图确认"))
        }
    }

    pub(crate) async fn terminate_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        let bid = self.resolve_app(g, app).await?;
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.terminate_app(&bid).await.map_err(|e| e.to_string())?;
        Ok(format!("已结束 {bid}"))
    }

    pub(crate) async fn open_url(&self, g: &mut Inner, url: &str) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.open_url(url).await.map_err(|e| e.to_string())?;
        Ok(format!("已打开 {url}"))
    }
}
