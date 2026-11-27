//! 守护进程控制接口：Unix socket（0600，默认位置见 [`default_socket`]），每行一个 JSON。
//! MCP 进程和 CLI 都通过它访问设备；本机不开 TCP 端口。

mod client;
pub mod env;
mod paths;
mod request;
mod response;

#[cfg(test)]
mod tests;

pub use client::*;
pub use paths::*;
pub use request::*;
pub use response::*;
