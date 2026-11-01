//! 守护进程控制接口：Unix socket（默认 /run/iphone-control/ctl.sock，0600），每行一个 JSON。
//! MCP 进程和 CLI 都通过它访问设备；本机不开 TCP 端口。

use serde::{Deserialize, Serialize};

pub const DEFAULT_SOCKET: &str = "/run/iphone-control/ctl.sock";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// MCP 的 iphone_status：与 toolkit 相同的文字
    Status,
    /// 区域单位为点；None 表示整屏
    Screenshot {
        region: Option<(f64, f64, f64, f64)>,
    },
    ListElements {
        filter: String,
        limit: usize,
    },
    Tap {
        x: f64,
        y: f64,
    },
    TapElement {
        text: String,
        index: usize,
    },
    LongPress {
        x: f64,
        y: f64,
        seconds: f64,
    },
    Swipe {
        from: (f64, f64),
        to: (f64, f64),
        seconds: f64,
    },
    TypeText {
        text: String,
        submit: bool,
    },
    PressButton {
        button: String,
    },
    ListApps {
        include_system: bool,
    },
    LaunchApp {
        app: String,
    },
    TerminateApp {
        app: String,
    },
    OpenUrl {
        url: String,
    },
    /// 运维：守护进程和设备的状态快照（JSON 文本）
    Daemon,
    /// 运维：逐层诊断，返回多行文字
    Doctor,
    Resign {
        force: bool,
    },
    UnlockReset,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Text {
        text: String,
    },
    /// JPEG base64 + 说明文字（尺寸、放大倍数、坐标换算式）
    Image {
        jpeg_base64: String,
        caption: String,
    },
    Error {
        message: String,
    },
}

impl Response {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }
    pub fn error(s: impl Into<String>) -> Self {
        Self::Error { message: s.into() }
    }
}

/// 发一个请求、读一个响应（客户端用）
pub async fn call(socket: &std::path::Path, req: &Request) -> std::io::Result<Response> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::UnixStream::connect(socket).await?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.shutdown().await?;
    let mut resp = String::new();
    BufReader::new(r).read_line(&mut resp).await?;
    serde_json::from_str(&resp).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let r = Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.3 };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.starts_with(r#"{"op":"swipe""#), "{s}");
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        let e: Response = serde_json::from_str(r#"{"kind":"error","message":"x"}"#).unwrap();
        assert_eq!(e, Response::error("x"));
    }
}
