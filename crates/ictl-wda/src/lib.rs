//! WebDriverAgent：启动 runner、HTTP 客户端、元素树解析。
//!
//! - 启动：[`runner::WdaRunner`]，就绪判定 = 本进程持有的 runner 任务存活 **且** `/status` ready
//! - 访问：[`http::DeviceTransport`] 经 usbmuxd 直连设备 8100 端口，本机不监听任何端口
//! - 元素树：[`elements`]，纯函数，可见性按坐标判断

pub mod client;
pub mod elements;
pub mod http;
pub mod runner;

pub use client::{IdleSettings, PointerStep, WdaClient, actions_body, swipe_steps, tap_steps};
pub use elements::Element;
pub use runner::WdaRunner;

#[derive(Debug, thiserror::Error)]
pub enum WdaError {
    #[error("WDA 不可用：{0}")]
    Unavailable(String),
    /// 手机重启后首次启用 UI 自动化，需要用户在手机上输入锁屏密码
    #[error("WDA 卡在「Enable UI Automation」密码框，请在手机上输入锁屏密码")]
    NeedsAutomationPin,
    #[error("手机上没有安装 WDA（{0}），先运行 iphone-control sign")]
    NotInstalled(String),
    #[error("WDA 启动失败：{0}")]
    StartFailed(String),
    #[error("{kind}: {message}")]
    Remote { kind: String, message: String },
}

pub type Result<T> = std::result::Result<T, WdaError>;

/// 识别 XCTest 启动失败是否是「等待自动化密码」：密码框 60 秒没输（toolkit 和 iOS 27 真机同一文字），
/// 或手机锁屏时密码框弹不出来、被系统直接取消（LocalAuthentication 错误，如 Code=-4「认证已取消」）
pub fn is_automation_pin_error(message: &str) -> bool {
    message.contains("enabling automation mode") || message.contains("com.apple.LocalAuthentication")
}

#[cfg(test)]
mod tests {
    #[test]
    fn automation_pin_error_detection() {
        assert!(super::is_automation_pin_error(
            "Timed out while enabling automation mode. (Error code: 1000, Domain: com.apple.dt.XCTest.XCTFuture)"
        ));
        // iOS 27.0.1 锁屏时的真机报错
        assert!(super::is_automation_pin_error(
            "Failed to initialize for UI testing: Error Domain=com.apple.LocalAuthentication Code=-4 \"认证已取消。\""
        ));
        assert!(!super::is_automation_pin_error("Failed to load the test bundle (Error code: 103)"));
    }
}
