//! 守护进程控制接口：Unix socket（默认 /run/iphone-control/ctl.sock，0600），每行一个 JSON。
//! MCP 进程和 CLI 都通过它访问设备；本机不开 TCP 端口。

use serde::{Deserialize, Serialize};

pub const DEFAULT_SOCKET: &str = "/run/iphone-control/ctl.sock";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// 区域单位为点；None 表示整屏
    Screenshot { region: Option<(f64, f64, f64, f64)> },
    ListElements { filter: String, limit: usize },
    Tap { x: f64, y: f64 },
    TapElement { text: String, index: usize },
    LongPress { x: f64, y: f64, seconds: f64 },
    Swipe { from: (f64, f64), to: (f64, f64), seconds: f64 },
    TypeText { text: String, submit: bool },
    PressButton { button: String },
    ListApps { include_system: bool },
    LaunchApp { app: String },
    TerminateApp { app: String },
    OpenUrl { url: String },
    Resign { force: bool },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Text { text: String },
    /// JPEG base64 + 说明文字（尺寸、放大倍数、坐标换算式）
    Image { jpeg_base64: String, caption: String },
    Error { message: String },
}
