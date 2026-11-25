//! MCP stdio 服务。工具名、参数、返回文本与 iphone-control-toolkit 一致（docs/decisions.md D10），
//! 切换时 agent 侧不用改。本 crate 不直接碰设备，全部经守护进程控制接口（Unix socket）。
//!
//! 手写 `ServerHandler`（不用宏）：参数 schema 直接写 JSON，省掉 schemars；
//! 参数到请求的转换（[`to_request`]）是纯函数，可单测。
//!
//! - `schema`：服务说明、工具定义（名字、说明、参数 schema、注解）
//! - `args`：参数校验和到请求的转换（[`to_request`]、[`parse_then`]、[`check_args`]）
//! - `batch`：`iphone_batch` 的步骤（[`batch_steps`]）
//! - `result`：守护进程的回答到工具结果，输入的投递状态
//! - `server`：[`McpServer`]（stdio 和 HTTP），工具调用的执行

use std::path::PathBuf;
use std::time::Duration;

use ictl_protocol::{self as protocol, Delivery, FailureStage, MAX_TAP_COUNT, Request, Response};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, InitializeResult, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use serde_json::{Map, Value, json};

pub mod task;

mod args;
mod batch;
mod result;
mod schema;
mod server;
pub use args::*;
pub use batch::*;
pub use result::*;
pub use schema::*;
pub use server::*;

#[cfg(test)]
mod tests;
