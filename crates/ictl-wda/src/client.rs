//! WDA HTTP 客户端。会话失效（WDA 重启后）自动重建一次。

use std::time::Duration;

use base64::Engine;
use ictl_device::ScreenSize;
use serde_json::{Value, json};

use crate::http::Transport;
use crate::{Result, WdaError, elements};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// 普通点击按下的时长（毫秒）。太短可能不被识别成点击；解锁按键另有自己的时长
const TAP_PRESS_MS: u64 = 50;

/// W3C pointer 动作的一步
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerStep {
    Move { x: f64, y: f64, ms: u64 },
    Down,
    Up,
    Pause { ms: u64 },
}

/// 单指点一下：按下停 `press_ms` 再抬起
pub fn tap_steps(x: f64, y: f64, press_ms: u64) -> Vec<PointerStep> {
    vec![PointerStep::Move { x, y, ms: 0 }, PointerStep::Down, PointerStep::Pause { ms: press_ms }, PointerStep::Up]
}

/// 滑动：按下、停 50ms、在 `ms` 毫秒内移到终点、抬起（与 toolkit 一致）
pub fn swipe_steps(from: (f64, f64), to: (f64, f64), ms: u64) -> Vec<PointerStep> {
    vec![
        PointerStep::Move { x: from.0, y: from.1, ms: 0 },
        PointerStep::Down,
        PointerStep::Pause { ms: 50 },
        PointerStep::Move { x: to.0, y: to.1, ms },
        PointerStep::Up,
    ]
}

/// `POST /session/{id}/actions` 的请求体
pub fn actions_body(steps: &[PointerStep]) -> Value {
    let actions: Vec<Value> = steps
        .iter()
        .map(|s| match *s {
            PointerStep::Move { x, y, ms } => json!({"type": "pointerMove", "duration": ms, "x": x, "y": y}),
            PointerStep::Down => json!({"type": "pointerDown", "button": 0}),
            PointerStep::Up => json!({"type": "pointerUp", "button": 0}),
            PointerStep::Pause { ms } => json!({"type": "pause", "duration": ms}),
        })
        .collect();
    json!({"actions": [{"type": "pointer", "id": "finger1", "parameters": {"pointerType": "touch"}, "actions": actions}]})
}

/// appium settings 中与等待有关的两项。锁屏解锁、点击期间要临时调小。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdleSettings {
    pub wait_for_idle_timeout: f64,
    pub animation_cool_off_timeout: f64,
}

impl IdleSettings {
    /// WDA 的默认值
    pub const DEFAULT: Self = Self { wait_for_idle_timeout: 10.0, animation_cool_off_timeout: 2.0 };
    /// 临时调小的值（0 会导致锁屏唤醒失败，所以不是 0）
    pub const QUICK: Self = Self { wait_for_idle_timeout: 1.0, animation_cool_off_timeout: 0.2 };
}

const SPRINGBOARD: &str = "com.apple.springboard";

/// [`WdaClient::check_lock`] 的结果
#[derive(Debug)]
pub struct LockCheck {
    pub locked: bool,
    /// 核对时取到的 SpringBoard 元素树（没核对或前台不是 SpringBoard 时为 None）
    pub tree: Option<String>,
}

pub struct WdaClient<T> {
    transport: T,
    session: Option<String>,
    screen: Option<ScreenSize>,
    /// 本客户端（对应一次 WDA 启动）是否确认过一次「确实没锁屏」，确认后 `/wda/locked` 就可信了
    lock_state_trusted: bool,
    /// 点击前后要恢复的等待设置（第一次点击时读一次）
    idle_baseline: Option<IdleSettings>,
}

impl<T: Transport> WdaClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport, session: None, screen: None, lock_state_trusted: false, idle_baseline: None }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        let body = body.map(|b| b.to_string().into_bytes());
        let (status, raw) = self.transport.send(method, path, body, timeout).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::TimedOut => WdaError::Timeout(e.to_string()),
            _ => WdaError::Unavailable(e.to_string()),
        })?;
        let v: Value = match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(_) if status >= 400 => {
                return Err(WdaError::Remote { kind: "http".into(), message: status.to_string() });
            }
            Err(e) => return Err(WdaError::Unavailable(format!("响应不是 JSON：{e}"))),
        };
        if let Some(err) = v.get("value").and_then(|x| x.get("error")).and_then(|e| e.as_str()) {
            let msg = v["value"].get("message").and_then(|m| m.as_str()).unwrap_or_default();
            return Err(WdaError::Remote {
                kind: err.to_owned(),
                message: msg.lines().next().unwrap_or_default().to_owned(),
            });
        }
        Ok(v)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        Ok(self.call("GET", path, None, DEFAULT_TIMEOUT).await?["value"].take())
    }

    async fn session_id(&mut self) -> Result<String> {
        if let Some(s) = &self.session {
            return Ok(s.clone());
        }
        let r =
            self.call("POST", "/session", Some(json!({"capabilities": {"alwaysMatch": {}}})), DEFAULT_TIMEOUT).await?;
        let id = r
            .get("sessionId")
            .and_then(|s| s.as_str())
            .or_else(|| r["value"].get("sessionId").and_then(|s| s.as_str()))
            .ok_or_else(|| WdaError::Unavailable("创建会话没有返回 sessionId".into()))?
            .to_owned();
        self.session = Some(id.clone());
        Ok(id)
    }

    /// 带会话的请求；会话失效时重建一次再试
    pub async fn in_session(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        for attempt in 0..2 {
            let sid = self.session_id().await?;
            match self.call(method, &format!("/session/{sid}{path}"), body.clone(), timeout).await {
                Err(WdaError::Remote { kind, .. }) if attempt == 0 && kind.contains("session") => self.session = None,
                other => return other.map(|mut v| v["value"].take()),
            }
        }
        unreachable!()
    }

    async fn post(&mut self, path: &str, body: Value) -> Result<Value> {
        self.in_session("POST", path, Some(body), DEFAULT_TIMEOUT).await
    }

    pub async fn ready(&self) -> Result<bool> {
        Ok(self.call("GET", "/status", None, Duration::from_secs(5)).await?["value"]["ready"] == true)
    }

    /// 屏幕点尺寸，拿到后缓存
    pub async fn screen_size(&mut self) -> Result<ScreenSize> {
        if let Some(s) = self.screen {
            return Ok(s);
        }
        let v = self.get("/wda/screen").await?;
        let s = &v["screenSize"];
        let size = match (s["width"].as_f64(), s["height"].as_f64()) {
            (Some(width), Some(height)) => ScreenSize { width, height },
            _ => return Err(WdaError::Unavailable("/wda/screen 没有 screenSize".into())),
        };
        self.screen = Some(size);
        Ok(size)
    }

    /// 是否锁屏。WDA 的 `/wda/locked` 只靠 `com.apple.springboard.lockstate` 的变化通知维护
    /// （WebDriverAgent `XCUIDevice+FBHelpers.m`），有两处误报，用元素树纠正（[`elements::cover_sheet`]）：
    /// - 初值为 false：WDA 在手机锁着时启动就一直报未锁屏，直到下一次锁屏/解锁。确认过一次「确实没锁屏」之后
    ///   WDA 就跟上了，此后它说没锁就直接信，不多发请求
    /// - 下拉通知中心时报 true（和锁屏同一个界面）：WDA 说锁了时核对一次，是通知中心就算没锁
    ///
    /// 核对失败时按 WDA 的结果；需要「拿不准就算锁着」时用 [`Self::check_lock`] 的 strict 模式
    pub async fn locked(&mut self) -> Result<bool> {
        Ok(self.check_lock(None, false).await?.locked)
    }

    /// 锁屏判断，并带回核对时取到的元素树（解锁流程接着用，省一次 `/source`）。
    /// - `reported`：调用方刚问过的 `/wda/locked`，省一次请求
    /// - `strict`：拿不准就算锁着。确认整串密码是否解开时用：把仍锁着误判成已解锁，下一次会把同一个错误密码再输一遍
    pub async fn check_lock(&mut self, reported: Option<bool>, strict: bool) -> Result<LockCheck> {
        let wda_locked = match reported {
            Some(v) => v,
            None => self.wda_reports_locked().await?,
        };
        if self.lock_state_trusted && (!wda_locked || strict) {
            // 已同步过：WDA 说没锁就是没锁；strict 场景在密码键盘上，WDA 说锁了也不会是通知中心
            return Ok(LockCheck { locked: wda_locked, tree: None });
        }
        let (sheet, tree) = match self.cover_tree().await {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!("核对锁屏界面失败，按{}处理：{e}", if strict { "锁着" } else { " WDA 的结果" });
                return Ok(LockCheck { locked: strict || wda_locked, tree: None });
            }
        };
        let locked = match (wda_locked, sheet) {
            (true, elements::CoverSheet::NotificationCenter) => false,
            (true, _) => true,
            (false, elements::CoverSheet::Locked) => {
                tracing::info!("WDA 报未锁屏，但屏幕上是锁屏界面（WDA 在锁屏时启动，状态未同步）");
                true
            }
            (false, _) => {
                self.lock_state_trusted = true;
                false
            }
        };
        Ok(LockCheck { locked, tree })
    }

    /// WDA 自己报的锁屏状态，不核对。可能误报，见 [`Self::locked`]；配合 [`Self::lock_state_trusted`] 做快速判断
    pub async fn wda_reports_locked(&self) -> Result<bool> {
        Ok(self.call("GET", "/wda/locked", None, Duration::from_secs(10)).await?["value"] == true)
    }

    /// 本次 WDA 启动后是否确认过一次没锁屏。确认过且 WDA 说没锁，就确定没锁
    pub fn lock_state_trusted(&self) -> bool {
        self.lock_state_trusted
    }

    /// 前台是 SpringBoard 时看元素树是锁屏、通知中心还是普通界面，并带回元素树；前台是别的 App 就是普通界面
    async fn cover_tree(&self) -> Result<(elements::CoverSheet, Option<String>)> {
        if self.active_app().await? != SPRINGBOARD {
            return Ok((elements::CoverSheet::None, None));
        }
        let xml = self.source_xml().await?;
        Ok((elements::cover_sheet(&xml), Some(xml)))
    }

    pub async fn active_app(&self) -> Result<String> {
        Ok(self.get("/wda/activeAppInfo").await?["bundleId"].as_str().unwrap_or_default().to_owned())
    }

    pub async fn screenshot_png(&self) -> Result<Vec<u8>> {
        // base64 约 11MB：从 JSON 里直接拿走字符串，原地去掉换行，不再另外复制
        let Value::String(mut b64) = self.get("/screenshot").await? else {
            return Err(WdaError::Unavailable("截图响应不是字符串".into()));
        };
        b64.retain(|c| !c.is_whitespace());
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| WdaError::Unavailable(format!("截图不是有效的 base64：{e}")))
    }

    /// 元素树 XML，排除可见性等慢属性（完整树 7～8 秒，排除后约 0.3 秒）
    pub async fn source_xml(&self) -> Result<String> {
        let path = format!("/source?format=xml&excluded_attributes={}", elements::SOURCE_EXCLUDED);
        let v = self.call("GET", &path, None, Duration::from_secs(60)).await?;
        Ok(v["value"].as_str().unwrap_or_default().to_owned())
    }

    pub async fn elements(&mut self) -> Result<Vec<elements::Element>> {
        let screen = self.screen_size().await?;
        Ok(elements::parse(&self.source_xml().await?, screen))
    }

    /// 点击：一个 W3C actions 单指手势（按下 50ms），期间把等待设置临时调小。真机计时（iOS 27）：
    /// - `/wda/tap` 先等界面空闲：主屏点图标约 1.7 秒（调小等待也没用），App 里约 0.6 秒，锁屏上约 2.9 秒
    /// - actions 手势做完要等动画冷却（`animationCoolOffTimeout`，默认 2 秒）：App 里切标签约 1.9 秒
    /// - actions + 临时调小等待：App 里约 0.7 秒，主屏点图标约 0.9 秒
    ///
    /// 和 `/wda/tap` 一样不等点击引起的动画结束，调用方要看结果就再截图
    pub async fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        let baseline = match self.idle_baseline {
            Some(b) => b,
            None => {
                // 上次没恢复成功的话读到的就是调小的值：按 WDA 默认值算
                let b = match self.idle_settings().await? {
                    s if s == IdleSettings::QUICK => IdleSettings::DEFAULT,
                    s => s,
                };
                self.idle_baseline = Some(b);
                b
            }
        };
        self.set_idle_settings(IdleSettings::QUICK).await?;
        let r = self.actions(&tap_steps(x, y, TAP_PRESS_MS)).await;
        if let Err(e) = self.set_idle_settings(baseline).await {
            tracing::warn!("点击后恢复 WDA 等待设置失败：{e}");
        }
        r
    }

    pub async fn touch_and_hold(&mut self, x: f64, y: f64, seconds: f64) -> Result<()> {
        self.post("/wda/touchAndHold", json!({"x": x, "y": y, "duration": seconds})).await.map(drop)
    }

    /// 一次 W3C actions 请求。注意：多次点击合并成一个请求会漏键，解锁时每位单独发
    pub async fn actions(&mut self, steps: &[PointerStep]) -> Result<()> {
        self.post("/actions", actions_body(steps)).await.map(drop)
    }

    pub async fn keys(&mut self, text: &str) -> Result<()> {
        let chars: Vec<String> = text.chars().map(String::from).collect();
        self.in_session("POST", "/wda/keys", Some(json!({"value": chars})), Duration::from_secs(120)).await.map(drop)
    }

    /// home / volumeUp / volumeDown
    pub async fn press_button(&mut self, name: &str) -> Result<()> {
        self.post("/wda/pressButton", json!({"name": name})).await.map(drop)
    }

    pub async fn lock(&mut self) -> Result<()> {
        self.post("/wda/lock", json!({})).await.map(drop)
    }

    /// 启动并等到前台后再停 1 秒（冷启动后立即点击会被吞）。返回 8 秒内是否到了前台；
    /// 等待期间查前台出错不算失败（启动命令已经成功了）
    pub async fn launch_app(&mut self, bundle_id: &str) -> Result<bool> {
        self.in_session("POST", "/wda/apps/launch", Some(json!({"bundleId": bundle_id})), Duration::from_secs(60))
            .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let mut foreground = false;
        while tokio::time::Instant::now() < deadline {
            if self.active_app().await.is_ok_and(|a| a == bundle_id) {
                foreground = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok(foreground)
    }

    pub async fn terminate_app(&mut self, bundle_id: &str) -> Result<()> {
        self.post("/wda/apps/terminate", json!({"bundleId": bundle_id})).await.map(drop)
    }

    pub async fn open_url(&mut self, url: &str) -> Result<()> {
        self.in_session("POST", "/url", Some(json!({"url": url})), Duration::from_secs(60)).await.map(drop)
    }

    pub async fn idle_settings(&mut self) -> Result<IdleSettings> {
        let v = self.in_session("GET", "/appium/settings", None, DEFAULT_TIMEOUT).await?;
        Ok(IdleSettings {
            wait_for_idle_timeout: v["waitForIdleTimeout"].as_f64().unwrap_or(10.0),
            animation_cool_off_timeout: v["animationCoolOffTimeout"].as_f64().unwrap_or(2.0),
        })
    }

    pub async fn set_idle_settings(&mut self, s: IdleSettings) -> Result<()> {
        let body = json!({"settings": {"waitForIdleTimeout": s.wait_for_idle_timeout, "animationCoolOffTimeout": s.animation_cool_off_timeout}});
        self.post("/appium/settings", body).await.map(drop)
    }

    /// 锁屏密码栏的 (总位数, 已输入位数)；找不到密码栏返回 None
    pub async fn passcode_field(&mut self) -> Result<Option<(u32, u32)>> {
        let els =
            self.post("/elements", json!({"using": "class name", "value": "XCUIElementTypeSecureTextField"})).await?;
        for e in els.as_array().into_iter().flatten() {
            let Some(eid) = e.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_str()) else { continue };
            let v = self.in_session("GET", &format!("/element/{eid}/attribute/value"), None, DEFAULT_TIMEOUT).await?;
            if let Some(p) = v.as_str().and_then(elements::parse_passcode_value) {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 按顺序返回预设响应，并记录收到的请求
    #[derive(Default)]
    struct Mock {
        replies: Mutex<VecDeque<(u16, String)>>,
        seen: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl Mock {
        fn with(replies: &[(u16, &str)]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().map(|(s, b)| (*s, b.to_string())).collect()),
                ..Default::default()
            }
        }
    }

    impl Transport for &Mock {
        async fn send(
            &self,
            method: &str,
            path: &str,
            body: Option<Vec<u8>>,
            _: Duration,
        ) -> std::io::Result<(u16, Vec<u8>)> {
            self.seen.lock().unwrap().push((method.into(), path.into(), body.map(|b| String::from_utf8(b).unwrap())));
            let (s, b) = self.replies.lock().unwrap().pop_front().ok_or_else(|| std::io::Error::other("连接被拒绝"))?;
            Ok((s, b.into_bytes()))
        }
    }

    #[tokio::test]
    async fn session_is_recreated_once_after_invalid_session() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A","value":{}}"#),
            (404, r#"{"value":{"error":"invalid session id","message":"gone\nmore"}}"#),
            (200, r#"{"value":{"sessionId":"B"}}"#),
            (200, r#"{"value":null}"#),
        ]);
        let mut c = WdaClient::new(&m);
        c.press_button("home").await.unwrap();
        let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
        assert_eq!(paths, ["/session", "/session/A/wda/pressButton", "/session", "/session/B/wda/pressButton"]);
    }

    #[tokio::test]
    async fn tap_is_one_gesture_with_quick_waits_then_restores_baseline() {
        let ok = r#"{"value":null}"#;
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (200, r#"{"value":{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}}"#),
            (200, ok),
            (200, ok),
            (200, ok),
            // 第二次点击不再读设置
            (200, ok),
            (200, ok),
            (200, ok),
        ]);
        let mut c = WdaClient::new(&m);
        c.tap(10.0, 20.0).await.unwrap();
        c.tap(30.0, 40.0).await.unwrap();
        let seen = m.seen.lock().unwrap().clone();
        let paths: Vec<_> = seen.iter().map(|s| s.1.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/session",
                "/session/A/appium/settings",
                "/session/A/appium/settings",
                "/session/A/actions",
                "/session/A/appium/settings",
                "/session/A/appium/settings",
                "/session/A/actions",
                "/session/A/appium/settings",
            ]
        );
        let body = |i: usize| serde_json::from_str::<Value>(seen[i].2.as_deref().unwrap()).unwrap();
        assert_eq!(body(2)["settings"]["animationCoolOffTimeout"], 0.2);
        assert_eq!(body(4)["settings"]["animationCoolOffTimeout"], 2.0);
        let a = body(3);
        let steps = a["actions"][0]["actions"].as_array().unwrap();
        assert_eq!(steps.iter().filter(|s| s["type"] == "pointerDown").count(), 1);
    }

    #[tokio::test]
    async fn remote_error_keeps_first_line_and_unreachable_maps_to_unavailable() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (500, r#"{"value":{"error":"unknown error","message":"boom\nstack"}}"#),
        ]);
        let mut c = WdaClient::new(&m);
        match c.press_button("home").await {
            Err(WdaError::Remote { kind, message }) => {
                assert_eq!((kind.as_str(), message.as_str()), ("unknown error", "boom"))
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(c.active_app().await, Err(WdaError::Unavailable(_))));
    }

    #[tokio::test]
    async fn screen_size_is_cached_and_status_ready() {
        let m = Mock::with(&[
            (200, r#"{"value":{"screenSize":{"width":430,"height":932},"scale":3}}"#),
            (200, r#"{"value":{"ready":true}}"#),
        ]);
        let mut c = WdaClient::new(&m);
        assert_eq!(c.screen_size().await.unwrap(), ScreenSize { width: 430.0, height: 932.0 });
        assert_eq!(c.screen_size().await.unwrap().width, 430.0);
        assert!(c.ready().await.unwrap());
    }

    #[tokio::test]
    async fn source_request_excludes_slow_attributes() {
        let m = Mock::with(&[(200, r#"{"value":"<x/>"}"#)]);
        let c = WdaClient::new(&m);
        c.source_xml().await.unwrap();
        let path = m.seen.lock().unwrap()[0].1.clone();
        assert!(path.starts_with("/source?format=xml&excluded_attributes=visible,"), "{path}");
    }

    #[tokio::test]
    async fn passcode_field_reads_secure_text_field_value() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":[{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}]}"#),
            (200, r#"{"value":"共6位数，已输入2位"}"#),
        ]);
        let mut c = WdaClient::new(&m);
        assert_eq!(c.passcode_field().await.unwrap(), Some((6, 2)));
        assert_eq!(m.seen.lock().unwrap()[2].1, "/session/S/element/E1/attribute/value");
    }

    #[test]
    fn actions_body_is_one_pointer_with_steps() {
        let b = actions_body(&tap_steps(10.0, 20.0, 80));
        let a = &b["actions"][0];
        assert_eq!(a["parameters"]["pointerType"], "touch");
        let types: Vec<_> = a["actions"].as_array().unwrap().iter().map(|s| s["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["pointerMove", "pointerDown", "pause", "pointerUp"]);
        assert_eq!(a["actions"][2]["duration"], 80);
        let s = swipe_steps((1.0, 2.0), (3.0, 4.0), 300);
        assert_eq!(s[3], PointerStep::Move { x: 3.0, y: 4.0, ms: 300 });
    }

    #[tokio::test]
    async fn locked_checks_tree_until_unlocked_once_then_trusts_wda() {
        let lock_xml = serde_json::to_string(include_str!("../tests/fixtures/lockscreen-ios27.xml")).unwrap();
        let home_xml = serde_json::to_string(include_str!("../tests/fixtures/home.xml")).unwrap();
        let sb = r#"{"value":{"bundleId":"com.apple.springboard"}}"#;
        let no = r#"{"value":false}"#;
        let (lock_src, home_src) = (format!(r#"{{"value":{lock_xml}}}"#), format!(r#"{{"value":{home_xml}}}"#));
        let m = Mock::with(&[
            // 1. WDA 在锁屏时启动：/wda/locked 误报 false，元素树是锁屏
            (200, no),
            (200, sb),
            (200, &lock_src),
            // 2. 人工解锁后：主屏幕，确认一次
            (200, no),
            (200, sb),
            (200, &home_src),
            // 3. 之后 WDA 说没锁就直接信
            (200, no),
            // 4. WDA 说锁了：核对一次，确实是锁屏
            (200, r#"{"value":true}"#),
            (200, sb),
            (200, &lock_src),
        ]);
        let mut c = WdaClient::new(&m);
        assert!(c.locked().await.unwrap());
        assert!(!c.locked().await.unwrap());
        assert!(!c.locked().await.unwrap());
        assert!(c.locked().await.unwrap());
        let paths: Vec<String> = m.seen.lock().unwrap().iter().map(|(_, p, _)| p.clone()).collect();
        assert_eq!(paths.iter().filter(|p| p.starts_with("/source")).count(), 3, "{paths:?}");
    }

    #[tokio::test]
    async fn notification_center_is_not_locked_even_if_wda_says_so() {
        let nc = serde_json::to_string(include_str!("../tests/fixtures/notification-center-ios27.xml")).unwrap();
        let pin = serde_json::to_string(include_str!("../tests/fixtures/passcode-view-ios27.xml")).unwrap();
        let sb = r#"{"value":{"bundleId":"com.apple.springboard"}}"#;
        let yes = r#"{"value":true}"#;
        let (nc, pin) = (format!(r#"{{"value":{nc}}}"#), format!(r#"{{"value":{pin}}}"#));
        let m = Mock::with(&[(200, yes), (200, sb), (200, &nc), (200, yes), (200, sb), (200, &pin), (200, yes)]);
        let mut c = WdaClient::new(&m);
        assert!(!c.locked().await.unwrap(), "通知中心");
        assert!(c.locked().await.unwrap(), "密码键盘界面");
        // 核对失败时按 WDA 的结果
        assert!(c.locked().await.unwrap());
    }

    #[tokio::test]
    async fn locked_falls_back_to_wda_when_tree_check_fails() {
        // 核对失败（例如取元素树超时）不能把可用的手机误判成锁屏，也不能就此信任 WDA
        let m = Mock::with(&[(200, r#"{"value":false}"#)]);
        let mut c = WdaClient::new(&m);
        assert!(!c.locked().await.unwrap());
        assert!(!c.lock_state_trusted);
    }
}
