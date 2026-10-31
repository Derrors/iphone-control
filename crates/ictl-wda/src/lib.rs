//! WebDriverAgent：启动、HTTP 客户端、元素树解析。
//!
//! - 启动：idevice `XCUITestService::run_until_wda_ready`，就绪判定 = 本进程持有的 XCTest 会话存活 **且** `/status` ready
//!   （手机上残留的 WDA 在 runner 退出后还会响应几十秒）
//! - 访问：经 idevice provider 直连设备 8100 端口，本机不监听任何端口
//! - 元素树：`/source?format=xml&excluded_attributes=visible,...`（约 0.3 秒；完整树 7～8 秒），可见性按坐标自己判断

use std::time::Duration;

pub mod elements;

#[derive(Debug, thiserror::Error)]
pub enum WdaError {
    #[error("WDA 不可用：{0}")]
    Unavailable(String),
    /// 手机重启后首次启用 UI 自动化，需要用户在手机上输入锁屏密码
    #[error("WDA 卡在「Enable UI Automation」密码框，请在手机上输入锁屏密码")]
    NeedsAutomationPin,
    #[error("WDA 返回错误 {kind}: {message}")]
    Remote { kind: String, message: String },
}

pub type Result<T> = std::result::Result<T, WdaError>;

/// 识别 XCTest 启动失败是否是"等待自动化密码"
pub fn is_automation_pin_error(message: &str) -> bool {
    message.contains("enabling automation mode")
}

/// 一个运行中的 WDA runner
pub struct WdaRunner {
    pub bundle_id: String,
}

impl WdaRunner {
    pub async fn start(tunnel: &mut ictl_device::Tunnel, bundle_id: &str, timeout: Duration) -> Result<Self> {
        let _ = (tunnel, bundle_id, timeout);
        todo!("TestConfig::from_installation_proxy + XCUITestService::run_until_wda_ready")
    }

    pub fn alive(&self) -> bool {
        todo!("WdaRunHandle 任务是否仍在运行")
    }

    pub async fn stop(self) -> Result<()> {
        todo!("WdaRunHandle::abort")
    }
}

/// W3C pointer 动作的一步
#[derive(Debug, Clone)]
pub enum PointerStep {
    Move { x: f64, y: f64, ms: u64 },
    Down,
    Up,
    Pause { ms: u64 },
}

/// appium settings 中与等待有关的两项。锁屏解锁期间要临时调小。
#[derive(Debug, Clone, Copy)]
pub struct IdleSettings {
    pub wait_for_idle_timeout: f64,
    pub animation_cool_off_timeout: f64,
}

/// WDA HTTP 客户端，会话失效时自动重建一次
pub struct WdaClient {
    session: Option<String>,
}

impl WdaClient {
    pub fn new() -> Self {
        Self { session: None }
    }

    pub async fn ready(&mut self) -> Result<bool> { todo!("GET /status") }
    pub async fn screen_size(&mut self) -> Result<ictl_device::ScreenSize> { todo!("GET /wda/screen") }
    pub async fn locked(&mut self) -> Result<bool> { todo!("GET /wda/locked") }
    pub async fn active_app(&mut self) -> Result<String> { todo!("GET /wda/activeAppInfo") }
    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> { todo!("GET /screenshot") }
    pub async fn source_xml(&mut self) -> Result<String> { todo!("GET /source?format=xml&excluded_attributes=...") }
    pub async fn tap(&mut self, x: f64, y: f64) -> Result<()> { let _ = (x, y); todo!("POST /session/{{id}}/wda/tap") }
    pub async fn touch_and_hold(&mut self, x: f64, y: f64, seconds: f64) -> Result<()> { let _ = (x, y, seconds); todo!() }
    /// 一次 W3C actions 请求。注意：多次点击合并成一个请求会漏键，解锁时每位单独发
    pub async fn actions(&mut self, steps: &[PointerStep]) -> Result<()> { let _ = steps; todo!("POST /session/{{id}}/actions") }
    pub async fn keys(&mut self, text: &str) -> Result<()> { let _ = text; todo!("POST /session/{{id}}/wda/keys") }
    pub async fn press_button(&mut self, name: &str) -> Result<()> { let _ = name; todo!() }
    pub async fn lock(&mut self) -> Result<()> { todo!("POST /session/{{id}}/wda/lock") }
    /// 启动并等到前台后再停 1 秒（冷启动立即点击会被吞）
    pub async fn launch_app(&mut self, bundle_id: &str) -> Result<()> { let _ = bundle_id; todo!() }
    pub async fn terminate_app(&mut self, bundle_id: &str) -> Result<()> { let _ = bundle_id; todo!() }
    pub async fn open_url(&mut self, url: &str) -> Result<()> { let _ = url; todo!("POST /session/{{id}}/url") }
    pub async fn idle_settings(&mut self) -> Result<IdleSettings> { todo!("GET /session/{{id}}/appium/settings") }
    pub async fn set_idle_settings(&mut self, s: IdleSettings) -> Result<()> { let _ = s; todo!() }
    /// 锁屏密码栏的值（形如「共6位数，已输入2位」）
    pub async fn passcode_field_value(&mut self) -> Result<Option<String>> { todo!("查 XCUIElementTypeSecureTextField 的 value") }
}

impl Default for WdaClient {
    fn default() -> Self {
        Self::new()
    }
}
