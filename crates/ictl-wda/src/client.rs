//! WDA HTTP 客户端。会话失效（WDA 重启后）自动重建一次。

use std::time::Duration;

use base64::Engine;
use ictl_device::ScreenSize;
use serde_json::{Value, json};

use crate::http::Transport;
use crate::{Result, WdaError, elements};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

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

/// appium settings 中与等待有关的两项。锁屏解锁期间要临时调小。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdleSettings {
    pub wait_for_idle_timeout: f64,
    pub animation_cool_off_timeout: f64,
}

const SPRINGBOARD: &str = "com.apple.springboard";

pub struct WdaClient<T> {
    transport: T,
    session: Option<String>,
    screen: Option<ScreenSize>,
    /// 本客户端（对应一次 WDA 启动）是否确认过一次「确实没锁屏」，确认后 `/wda/locked` 就可信了
    lock_state_trusted: bool,
}

impl<T: Transport> WdaClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport, session: None, screen: None, lock_state_trusted: false }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        let body = body.map(|b| b.to_string().into_bytes());
        let (status, raw) =
            self.transport.send(method, path, body, timeout).await.map_err(|e| WdaError::Unavailable(e.to_string()))?;
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

    pub async fn status(&self) -> Result<Value> {
        self.get("/status").await
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
    pub async fn locked(&mut self) -> Result<bool> {
        let wda_locked = self.call("GET", "/wda/locked", None, Duration::from_secs(10)).await?["value"] == true;
        if !wda_locked && self.lock_state_trusted {
            return Ok(false);
        }
        let sheet = match self.cover_sheet().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("核对锁屏界面失败，按 WDA 的结果：{e}");
                return Ok(wda_locked);
            }
        };
        match (wda_locked, sheet) {
            (true, elements::CoverSheet::NotificationCenter) => Ok(false),
            (true, _) => Ok(true),
            (false, elements::CoverSheet::Locked) => {
                tracing::info!("WDA 报未锁屏，但屏幕上是锁屏界面（WDA 在锁屏时启动，状态未同步）");
                Ok(true)
            }
            (false, _) => {
                self.lock_state_trusted = true;
                Ok(false)
            }
        }
    }

    /// 前台是 SpringBoard 时看元素树是锁屏、通知中心还是普通界面；前台是别的 App 就是普通界面
    async fn cover_sheet(&self) -> Result<elements::CoverSheet> {
        if self.active_app().await? != SPRINGBOARD {
            return Ok(elements::CoverSheet::None);
        }
        Ok(elements::cover_sheet(&self.source_xml().await?))
    }

    pub async fn active_app(&self) -> Result<String> {
        Ok(self.get("/wda/activeAppInfo").await?["bundleId"].as_str().unwrap_or_default().to_owned())
    }

    pub async fn screenshot_png(&self) -> Result<Vec<u8>> {
        let v = self.get("/screenshot").await?;
        let b64: String = v.as_str().unwrap_or_default().chars().filter(|c| !c.is_whitespace()).collect();
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

    pub async fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        self.post("/wda/tap", json!({"x": x, "y": y})).await.map(drop)
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

    /// 启动并等到前台后再停 1 秒（冷启动后立即点击会被吞）
    pub async fn launch_app(&mut self, bundle_id: &str) -> Result<()> {
        self.in_session("POST", "/wda/apps/launch", Some(json!({"bundleId": bundle_id})), Duration::from_secs(60))
            .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while tokio::time::Instant::now() < deadline && self.active_app().await? != bundle_id {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok(())
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
        c.tap(10.0, 20.0).await.unwrap();
        let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
        assert_eq!(paths, ["/session", "/session/A/wda/tap", "/session", "/session/B/wda/tap"]);
    }

    #[tokio::test]
    async fn remote_error_keeps_first_line_and_unreachable_maps_to_unavailable() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (500, r#"{"value":{"error":"unknown error","message":"boom\nstack"}}"#),
        ]);
        let mut c = WdaClient::new(&m);
        match c.tap(1.0, 1.0).await {
            Err(WdaError::Remote { kind, message }) => {
                assert_eq!((kind.as_str(), message.as_str()), ("unknown error", "boom"))
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(c.status().await, Err(WdaError::Unavailable(_))));
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
