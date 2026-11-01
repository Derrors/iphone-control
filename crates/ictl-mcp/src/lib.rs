//! MCP stdio 服务。工具名、参数、返回文本与 iphone-control-toolkit 一致（docs/decisions.md D10），
//! 切换时 agent 侧不用改。本 crate 不直接碰设备，全部经守护进程控制接口（Unix socket）。
//!
//! 手写 `ServerHandler`（不用宏）：参数 schema 直接写 JSON，省掉 schemars；
//! 参数到请求的转换（[`to_request`]）是纯函数，可单测。

use std::path::PathBuf;
use std::time::Duration;

use ictl_protocol::{self as protocol, Request, Response};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, InitializeResult, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, Tool,
    },
    service::RequestContext,
};
use serde_json::{Map, Value, json};

/// 服务说明（发给客户端的 instructions），与 toolkit 一致
pub const INSTRUCTIONS: &str = "控制一台通过 USB 连接的真实 iPhone。坐标单位是点(pt),与 iphone_screenshot 图像像素一致。\
操作前先用 iphone_screenshot 或 iphone_list_elements 看清屏幕;能按名字点就优先用 iphone_tap_element。\
游戏、地图等界面没有元素树,只能看截图按坐标操作;小图标、小字看不清时用 iphone_screenshot 的区域放大。\
配置了锁屏密码时,各操作工具会先自动解锁。每次操作后再截图确认结果。\
涉及付款、删除、发送消息、修改账号或隐私设置的操作,先向用户确认。\
省时间:操作类工具可带 then=elements 或 then=screenshot,等界面稳定后顺带返回操作后的界面,不用再单独调一次;\
几步确定的连续操作(按键、逐级点菜单、填表)用 iphone_batch 一次发完;等页面加载用 iphone_wait_for,不要反复截图。";

fn num(desc: &str) -> Value {
    json!({"type": "number", "description": desc})
}

fn schema(props: Value, required: &[&str]) -> Map<String, Value> {
    let v = json!({"type": "object", "properties": props, "required": required});
    v.as_object().cloned().unwrap_or_default()
}

/// 操作后顺带返回界面时最多列多少个元素（每步都带太多会占满上下文）
pub const THEN_LIMIT: usize = 80;
/// 顺带返回界面前等界面稳定的上限
pub const THEN_SETTLE_MS: u64 = 1500;
/// 一次批量最多几步
pub const BATCH_MAX_STEPS: usize = 20;
/// reuse_elements 时按名字点击复用多旧的元素树（毫秒）
pub const BATCH_REUSE_MS: u64 = 10_000;
/// 批量里能用的工具：改变界面的操作和等待；看界面用 then
pub const BATCH_TOOLS: [&str; 11] = [
    "iphone_tap",
    "iphone_tap_element",
    "iphone_long_press",
    "iphone_swipe",
    "iphone_type_text",
    "iphone_press_button",
    "iphone_launch_app",
    "iphone_terminate_app",
    "iphone_open_url",
    "iphone_wait_for",
    "wait",
];

/// 操作类工具共有的可选参数：操作后顺带返回界面
fn then_props() -> Value {
    json!({
        "then": {"type": "string", "enum": ["none", "elements", "screenshot"], "default": "none",
                 "description": "操作后等界面稳定,再顺带返回界面:elements=元素列表(最多 80 个),screenshot=截图。省一次单独的调用"},
        "then_filter": {"type": "string", "default": "", "description": "then=elements 时只列包含这段文字的元素"},
        "then_region": {"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4,
                        "description": "then=screenshot 时只返回这块区域的放大图 [x, y, width, height](点),例如游戏棋盘"},
        "then_settle": {"type": "number", "default": 1.5,
                        "description": "顺带返回界面前最多等几秒让界面稳定(不超过 10)。游戏、地图按画面是否静止判断,消除连锁等长动画可调大,画面一静止就返回"}
    })
}

/// 在参数 schema 里加上 then / then_filter
fn with_then(props: Value) -> Value {
    let mut p = props;
    if let (Some(o), Value::Object(extra)) = (p.as_object_mut(), then_props()) {
        o.extend(extra);
    }
    p
}

/// 15 个工具的定义（toolkit 的 13 个 + iphone_wait_for、iphone_batch）
pub fn tools() -> Vec<Tool> {
    let t = |name: &'static str, desc: &'static str, s: Map<String, Value>| Tool::new(name, desc, s);
    vec![
        t("iphone_status", "查看连接状态:WDA 是否就绪、是否锁屏、当前前台 App、屏幕尺寸。", schema(json!({}), &[])),
        t(
            "iphone_screenshot",
            "截取当前屏幕。图像已缩放到点尺寸,像素坐标可直接用于点击和滑动。\n\n看不清小图标、小字时,传 x/y/width/height(点)放大这块区域:从原始高清截图裁剪,\n小区域再额外放大。返回文字里有放大倍数,图中坐标换算回屏幕点:x + 像素/倍数。",
            schema(
                json!({"x": num("区域左上角 x(点)"), "y": num("区域左上角 y(点)"), "width": num("区域宽(点)"), "height": num("区域高(点)")}),
                &[],
            ),
        ),
        t(
            "iphone_list_elements",
            "列出屏幕上带文字的界面元素:类型、标签、中心点坐标、尺寸。\n\nfilter: 只返回标签/名字/值里包含该文字的元素(不区分大小写)。\n注意:按坐标判断是否在屏幕内,被弹窗遮住的元素也可能列出来,不确定时截图核对。",
            schema(
                json!({"filter": {"type": "string", "default": ""}, "limit": {"type": "integer", "default": 150}}),
                &[],
            ),
        ),
        t(
            "iphone_tap",
            "点击屏幕坐标(点)。",
            schema(with_then(json!({"x": num("x(点)"), "y": num("y(点)")})), &["x", "y"]),
        ),
        t(
            "iphone_tap_element",
            "按文字点击元素:先找标签/名字完全相同的,找不到再找包含该文字的。\n\nindex: 匹配到多个时点第几个(从 0 开始,顺序同 iphone_list_elements)。",
            schema(
                with_then(json!({"text": {"type": "string"}, "index": {"type": "integer", "default": 0}})),
                &["text"],
            ),
        ),
        t(
            "iphone_long_press",
            "长按屏幕坐标(点)。",
            schema(
                with_then(json!({"x": num("x(点)"), "y": num("y(点)"), "seconds": {"type": "number", "default": 1.0}})),
                &["x", "y"],
            ),
        ),
        t(
            "iphone_swipe",
            "从一点滑到另一点(点坐标)。向上滑看下面的内容:from_y 大、to_y 小。",
            schema(
                with_then(
                    json!({"from_x": num(""), "from_y": num(""), "to_x": num(""), "to_y": num(""), "seconds": {"type": "number", "default": 0.3}}),
                ),
                &["from_x", "from_y", "to_x", "to_y"],
            ),
        ),
        t(
            "iphone_type_text",
            "在当前获得焦点的输入框里输入文字(支持中文)。需要先点中输入框。submit=True 时末尾按回车。",
            schema(
                with_then(json!({"text": {"type": "string"}, "submit": {"type": "boolean", "default": false}})),
                &["text"],
            ),
        ),
        t(
            "iphone_press_button",
            "按硬件键:home(回主屏幕)、volume_up、volume_down、lock(锁屏;配置了密码时之后的操作会自动解锁)。",
            schema(
                with_then(json!({"button": {"type": "string", "enum": ["home", "volume_up", "volume_down", "lock"]}})),
                &["button"],
            ),
        ),
        t(
            "iphone_list_apps",
            "列出已安装的 App(名字和 Bundle ID)。include_system=True 时列出系统 App。",
            schema(json!({"include_system": {"type": "boolean", "default": false}}), &[]),
        ),
        t(
            "iphone_launch_app",
            "启动 App 并切到前台。app 可以是 Bundle ID,也可以是显示名(如「设置」「微信」)。",
            schema(with_then(json!({"app": {"type": "string"}})), &["app"]),
        ),
        t(
            "iphone_terminate_app",
            "结束 App 进程。app 可以是 Bundle ID 或显示名。",
            schema(with_then(json!({"app": {"type": "string"}})), &["app"]),
        ),
        t(
            "iphone_open_url",
            "用系统默认方式打开链接或 URL Scheme(网页会在 Safari 打开)。",
            schema(with_then(json!({"url": {"type": "string"}})), &["url"]),
        ),
        t(
            "iphone_wait_for",
            "等包含某段文字的界面元素出现(例如页面加载完、弹窗出现),出现就立即返回;超时报错。代替反复截图轮询。",
            schema(
                json!({"text": {"type": "string"}, "timeout": {"type": "number", "default": 5, "description": "最多等几秒(不超过 30)"}}),
                &["text"],
            ),
        ),
        t(
            "iphone_batch",
            "按顺序执行几步操作,一次调用完成(最多 20 步),某一步出错就停下并说明是第几步。\n\n\
steps 每项是 {\"tool\": 工具名, 其余是该工具的参数},可用:iphone_tap、iphone_tap_element、iphone_long_press、\
iphone_swipe、iphone_type_text、iphone_press_button、iphone_launch_app、iphone_terminate_app、iphone_open_url、\
iphone_wait_for,以及 {\"tool\": \"wait\", \"seconds\": 秒数}(不超过 5)。\n\
适合确定的连续操作;每步的结果不确定(例如点了以后不知道会跳到哪)时,还是一步步来。\n\
then 同其他操作工具:全部做完后顺带返回界面。",
            schema(
                with_then(json!({
                    "steps": {"type": "array", "items": {"type": "object"}, "description": "要执行的步骤"},
                    "reuse_elements": {"type": "boolean", "default": false,
                        "description": "这批操作不改变界面布局时(例如在同一个键盘上连按)设为 true:按名字点击复用最近读到的元素树,每步省 0.3~0.8 秒。会跳转页面的不要用,否则可能点到旧位置"}
                })),
                &["steps"],
            ),
        ),
    ]
}

struct Args<'a>(&'a Map<String, Value>);

/// 数字，或写成字符串的数字（toolkit 的 pydantic 也接受 "3"）
fn number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()).filter(|x: &f64| x.is_finite())
}

/// 参数给了但类型不对就报错，不再悄悄用默认值（例如 index: -1 原来会点第 0 个）
impl Args<'_> {
    fn get(&self, k: &str) -> Option<&Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }
    fn f(&self, k: &str) -> Result<f64, String> {
        self.opt_f(k)?.ok_or_else(|| format!("缺少数字参数 {k}"))
    }
    fn f_or(&self, k: &str, d: f64) -> Result<f64, String> {
        Ok(self.opt_f(k)?.unwrap_or(d))
    }
    fn opt_f(&self, k: &str) -> Result<Option<f64>, String> {
        self.get(k).map(|v| number(v).ok_or_else(|| format!("参数 {k} 要是数字"))).transpose()
    }
    fn s(&self, k: &str) -> Result<String, String> {
        self.get(k).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("缺少文字参数 {k}"))
    }
    fn s_or(&self, k: &str, d: &str) -> Result<String, String> {
        match self.get(k) {
            None => Ok(d.to_owned()),
            Some(v) => v.as_str().map(str::to_owned).ok_or_else(|| format!("参数 {k} 要是文字")),
        }
    }
    fn u_or(&self, k: &str, d: usize) -> Result<usize, String> {
        match self.get(k) {
            None => Ok(d),
            Some(v) => number(v)
                .filter(|x| *x >= 0.0 && x.fract() == 0.0)
                .map(|x| x as usize)
                .ok_or_else(|| format!("参数 {k} 要是非负整数")),
        }
    }
    fn b_or(&self, k: &str, d: bool) -> Result<bool, String> {
        match self.get(k) {
            None => Ok(d),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(true),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => Ok(false),
            Some(_) => Err(format!("参数 {k} 要是 true 或 false")),
        }
    }
}

/// 工具调用 → 守护进程请求
pub fn to_request(name: &str, args: &Map<String, Value>) -> Result<Request, String> {
    let a = Args(args);
    Ok(match name {
        "iphone_status" => Request::Status,
        "iphone_screenshot" => {
            let r = [a.opt_f("x")?, a.opt_f("y")?, a.opt_f("width")?, a.opt_f("height")?];
            let region = match r {
                [None, None, None, None] => None,
                [Some(x), Some(y), Some(w), Some(h)] => Some((x, y, w, h)),
                _ => return Err("放大区域要同时给出 x、y、width、height".into()),
            };
            Request::Screenshot { region }
        }
        "iphone_list_elements" => {
            Request::ListElements { filter: a.s_or("filter", "")?, limit: a.u_or("limit", 150)?, max_age_ms: 0 }
        }
        "iphone_tap" => Request::Tap { x: a.f("x")?, y: a.f("y")? },
        "iphone_tap_element" => Request::TapElement { text: a.s("text")?, index: a.u_or("index", 0)?, max_age_ms: 0 },
        "iphone_long_press" => Request::LongPress { x: a.f("x")?, y: a.f("y")?, seconds: a.f_or("seconds", 1.0)? },
        "iphone_swipe" => Request::Swipe {
            from: (a.f("from_x")?, a.f("from_y")?),
            to: (a.f("to_x")?, a.f("to_y")?),
            seconds: a.f_or("seconds", 0.3)?,
        },
        "iphone_type_text" => Request::TypeText { text: a.s("text")?, submit: a.b_or("submit", false)? },
        "iphone_press_button" => Request::PressButton { button: a.s("button")? },
        "iphone_list_apps" => Request::ListApps { include_system: a.b_or("include_system", false)? },
        "iphone_launch_app" => Request::LaunchApp { app: a.s("app")? },
        "iphone_terminate_app" => Request::TerminateApp { app: a.s("app")? },
        "iphone_open_url" => Request::OpenUrl { url: a.s("url")? },
        "iphone_wait_for" => {
            let secs = a.f_or("timeout", 5.0)?;
            if !(0.0..=30.0).contains(&secs) {
                return Err("timeout 要在 0 到 30 秒之间".into());
            }
            Request::WaitFor { text: a.s("text")?, timeout_ms: (secs * 1000.0) as u64 }
        }
        other => return Err(format!("未知工具 {other}")),
    })
}

/// 操作后顺带返回什么
#[derive(Debug, Clone, PartialEq)]
pub enum Then {
    None,
    Elements(String),
    Screenshot(Option<(f64, f64, f64, f64)>),
}

/// then 带的选项：返回什么 + 最多等多久让界面稳定
#[derive(Debug, Clone, PartialEq)]
pub struct ThenOpts {
    pub then: Then,
    pub settle_ms: u64,
}

pub fn parse_then(args: &Map<String, Value>) -> Result<ThenOpts, String> {
    let a = Args(args);
    let then = match a.s_or("then", "none")?.as_str() {
        "none" | "" => Then::None,
        "elements" => Then::Elements(a.s_or("then_filter", "")?),
        "screenshot" => {
            let region = match a.get("then_region") {
                None => None,
                Some(v) => {
                    let xs: Option<Vec<f64>> = v.as_array().map(|arr| arr.iter().filter_map(number).collect());
                    match xs.as_deref() {
                        Some([x, y, w, h]) if *w > 0.0 && *h > 0.0 => Some((*x, *y, *w, *h)),
                        _ => return Err("then_region 要是 [x, y, width, height] 四个数,宽高大于 0".into()),
                    }
                }
            };
            Then::Screenshot(region)
        }
        other => return Err(format!("then 只能是 none / elements / screenshot,不是 {other}")),
    };
    let secs = a.f_or("then_settle", THEN_SETTLE_MS as f64 / 1000.0)?;
    if !(0.0..=10.0).contains(&secs) {
        return Err("then_settle 要在 0 到 10 秒之间".into());
    }
    Ok(ThenOpts { then, settle_ms: (secs * 1000.0) as u64 })
}

/// 批量里的一步
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Request { tool: String, req: Request },
    Wait(std::time::Duration),
}

/// 检查并转换批量步骤（一步都不执行就先把参数错误全挡掉）
pub fn batch_steps(args: &Map<String, Value>) -> Result<Vec<Step>, String> {
    let steps = args.get("steps").and_then(Value::as_array).ok_or("缺少 steps(数组)")?;
    let reuse = Args(args).b_or("reuse_elements", false)?;
    if steps.is_empty() || steps.len() > BATCH_MAX_STEPS {
        return Err(format!("steps 要有 1 到 {BATCH_MAX_STEPS} 步"));
    }
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let n = i + 1;
            let obj = s.as_object().ok_or_else(|| format!("第 {n} 步不是对象"))?;
            let tool = obj.get("tool").and_then(Value::as_str).ok_or_else(|| format!("第 {n} 步缺少 tool"))?;
            if !BATCH_TOOLS.contains(&tool) {
                return Err(format!("第 {n} 步:批量里不能用 {tool},可用:{}", BATCH_TOOLS.join("、")));
            }
            if tool == "wait" {
                let secs = Args(obj).f("seconds").map_err(|e| format!("第 {n} 步:{e}"))?;
                if !(0.0..=5.0).contains(&secs) {
                    return Err(format!("第 {n} 步:wait 的 seconds 要在 0 到 5 之间"));
                }
                return Ok(Step::Wait(std::time::Duration::from_secs_f64(secs)));
            }
            let req = match to_request(tool, obj).map_err(|e| format!("第 {n} 步({tool}):{e}"))? {
                Request::TapElement { text, index, .. } if reuse => {
                    Request::TapElement { text, index, max_age_ms: BATCH_REUSE_MS }
                }
                other => other,
            };
            Ok(Step::Request { tool: tool.to_owned(), req })
        })
        .collect()
}

/// tap_element 的结果是不是「没点到」（没找到、或 index 超出匹配个数）
pub fn tap_missed(text: &str) -> bool {
    text.starts_with("没找到") || text.starts_with("只匹配到")
}

/// 守护进程响应 → MCP 结果。错误走工具错误（is_error），文字原样给客户端
pub fn to_result(r: Response) -> CallToolResult {
    match r {
        Response::Text { text } => CallToolResult::success(vec![ContentBlock::text(text)]),
        Response::Image { jpeg_base64, caption } => {
            CallToolResult::success(vec![ContentBlock::image(jpeg_base64, "image/jpeg"), ContentBlock::text(caption)])
        }
        Response::Error { message } => CallToolResult::error(vec![ContentBlock::text(message)]),
    }
}

#[derive(Debug, Clone)]
pub struct McpServer {
    pub socket: PathBuf,
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let then = match parse_then(&args) {
            Ok(t) => t,
            Err(e) => return Ok(to_result(Response::error(e)).into()),
        };
        if request.name == "iphone_batch" {
            return Ok(self.batch(&args, then).await.into());
        }
        let req = match to_request(&request.name, &args) {
            Ok(r) => r,
            Err(e) => return Ok(to_result(Response::error(e)).into()),
        };
        // then 只对操作类工具有意义；看界面的工具本身就是返回界面
        let resp = self.call(&req).await;
        Ok(match resp {
            Response::Text { text } if req.is_device_action() && then.then != Then::None => {
                self.finish(text, false, then).await
            }
            resp => to_result(resp),
        }
        .into())
    }
}

impl McpServer {
    async fn call(&self, req: &Request) -> Response {
        // 守护进程正忙（例如在自动解锁）时会排队；最多等 150 秒，免得 agent 一直挂着
        protocol::call(&self.socket, req, Some(Duration::from_secs(150))).await.unwrap_or_else(|e| {
            Response::error(format!("连不上守护进程({}):{e};确认 iphone-control 容器在运行", self.socket.display()))
        })
    }

    /// 操作完成后：按 then 等界面稳定，再附上元素列表或截图
    async fn finish(&self, mut text: String, failed: bool, opts: ThenOpts) -> CallToolResult {
        let done = |blocks: Vec<ContentBlock>| {
            if failed { CallToolResult::error(blocks) } else { CallToolResult::success(blocks) }
        };
        let then = opts.then;
        if then == Then::None {
            return done(vec![ContentBlock::text(text)]);
        }
        match self.call(&Request::Settle { max_ms: opts.settle_ms }).await {
            Response::Text { text: t } => text += &format!("\n{t}"),
            Response::Error { message } => text += &format!("\n(等界面稳定失败:{message})"),
            Response::Image { .. } => {}
        }
        match then {
            Then::Elements(filter) => {
                // Settle 刚读过元素树，直接用它
                match self.call(&Request::ListElements { filter, limit: THEN_LIMIT, max_age_ms: THEN_SETTLE_MS }).await
                {
                    Response::Text { text: t } => text += &format!("\n--- 操作后的界面 ---\n{t}"),
                    Response::Error { message } => text += &format!("\n(读界面失败:{message})"),
                    Response::Image { .. } => {}
                }
                done(vec![ContentBlock::text(text)])
            }
            Then::Screenshot(region) => match self.call(&Request::Screenshot { region }).await {
                Response::Image { jpeg_base64, caption } => done(vec![
                    ContentBlock::text(text),
                    ContentBlock::image(jpeg_base64, "image/jpeg"),
                    ContentBlock::text(caption),
                ]),
                Response::Error { message } => done(vec![ContentBlock::text(format!("{text}\n(截图失败:{message})"))]),
                Response::Text { text: t } => done(vec![ContentBlock::text(format!("{text}\n{t}"))]),
            },
            Then::None => unreachable!("上面已返回"),
        }
    }

    /// 按顺序执行；某一步出错就停，说明是第几步、后面几步没执行。出错时也按 then 附上当前界面，方便看停在哪
    async fn batch(&self, args: &Map<String, Value>, then: ThenOpts) -> CallToolResult {
        let steps = match batch_steps(args) {
            Ok(s) => s,
            Err(e) => return to_result(Response::error(e)),
        };
        let total = steps.len();
        let mut lines = Vec::new();
        for (i, step) in steps.into_iter().enumerate() {
            let n = i + 1;
            match step {
                Step::Wait(d) => {
                    tokio::time::sleep(d).await;
                    lines.push(format!("{n}. wait → 等了 {} 秒", d.as_secs_f64()));
                }
                Step::Request { tool, req } => match self.call(&req).await {
                    // 按名字没点到（toolkit 兼容：单独调用时是普通结果）在批量里要停下：后面的步骤都建立在它点中的前提上
                    Response::Text { text } if tool == "iphone_tap_element" && tap_missed(&text) => {
                        lines.push(format!("{n}. {tool} → {text}"));
                        if n < total {
                            lines.push(format!("第 {n} 步没点到,后面 {} 步没有执行", total - n));
                        }
                        return self.finish(lines.join("\n"), true, then).await;
                    }
                    Response::Text { text } => lines.push(format!("{n}. {tool} → {text}")),
                    Response::Image { caption, .. } => lines.push(format!("{n}. {tool} → {caption}")),
                    Response::Error { message } => {
                        lines.push(format!("{n}. {tool} → 出错:{message}"));
                        if n < total {
                            lines.push(format!("第 {n} 步出错,后面 {} 步没有执行", total - n));
                        }
                        return self.finish(lines.join("\n"), true, then).await;
                    }
                },
            }
        }
        self.finish(lines.join("\n"), false, then).await
    }
}

/// 在 stdin/stdout 上运行 MCP 服务直到客户端断开
pub async fn serve_stdio(socket: PathBuf) -> anyhow::Result<()> {
    let running = McpServer { socket }.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn toolkit_tools_plus_wait_for_and_batch() {
        let names: Vec<_> = tools().iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names.len(), 15);
        for n in ["iphone_status", "iphone_screenshot", "iphone_tap_element", "iphone_open_url"] {
            assert!(names.contains(&n.to_owned()), "{n}");
        }
        for t in tools().into_iter().filter(|t| t.name != "iphone_batch") {
            let no_required = t.input_schema.get("required").unwrap().as_array().unwrap().is_empty();
            assert_eq!(to_request(&t.name, &Map::new()).is_ok(), no_required, "{}", t.name);
        }
    }

    #[test]
    fn defaults_match_toolkit() {
        assert_eq!(
            to_request("iphone_list_elements", &Map::new()).unwrap(),
            Request::ListElements { filter: String::new(), limit: 150, max_age_ms: 0 }
        );
        assert_eq!(
            to_request("iphone_swipe", &args(json!({"from_x": 1, "from_y": 2, "to_x": 3, "to_y": 4}))).unwrap(),
            Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.3 }
        );
        assert_eq!(
            to_request("iphone_long_press", &args(json!({"x": 1, "y": 2}))).unwrap(),
            Request::LongPress { x: 1.0, y: 2.0, seconds: 1.0 }
        );
        assert_eq!(
            to_request("iphone_type_text", &args(json!({"text": "设置"}))).unwrap(),
            Request::TypeText { text: "设置".into(), submit: false }
        );
    }

    #[test]
    fn then_is_offered_on_actions_only() {
        let has_then = |name: &str| {
            let t = tools().into_iter().find(|t| t.name == name).unwrap();
            t.input_schema["properties"].get("then").is_some()
        };
        for n in [
            "iphone_tap",
            "iphone_tap_element",
            "iphone_swipe",
            "iphone_type_text",
            "iphone_launch_app",
            "iphone_batch",
        ] {
            assert!(has_then(n), "{n}");
        }
        for n in ["iphone_status", "iphone_screenshot", "iphone_list_elements", "iphone_wait_for"] {
            assert!(!has_then(n), "{n}");
        }
        let then = |v: Value| parse_then(&args(v)).map(|o| o.then);
        assert_eq!(parse_then(&Map::new()).unwrap(), ThenOpts { then: Then::None, settle_ms: THEN_SETTLE_MS });
        assert_eq!(then(json!({"then": "elements", "then_filter": "设置"})).unwrap(), Then::Elements("设置".into()));
        assert_eq!(then(json!({"then": "screenshot"})).unwrap(), Then::Screenshot(None));
        assert_eq!(
            then(json!({"then": "screenshot", "then_region": [0, 295, 430, 460]})).unwrap(),
            Then::Screenshot(Some((0.0, 295.0, 430.0, 460.0)))
        );
        assert!(then(json!({"then": "screenshot", "then_region": [0, 1, 2]})).is_err());
        assert!(then(json!({"then": "screenshot", "then_region": [0, 1, 0, 5]})).is_err());
        assert!(then(json!({"then": "video"})).is_err());
        assert_eq!(parse_then(&args(json!({"then": "screenshot", "then_settle": 5}))).unwrap().settle_ms, 5000);
        assert!(parse_then(&args(json!({"then_settle": 30}))).is_err());
    }

    #[test]
    fn batch_steps_are_checked_before_anything_runs() {
        let steps = batch_steps(&args(json!({"steps": [
            {"tool": "iphone_tap_element", "text": "One"},
            {"tool": "wait", "seconds": 0.5},
            {"tool": "iphone_tap", "x": 1, "y": 2},
            {"tool": "iphone_wait_for", "text": "完成", "timeout": 3}
        ]})))
        .unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[1], Step::Wait(std::time::Duration::from_millis(500)));
        assert_eq!(
            steps[3],
            Step::Request {
                tool: "iphone_wait_for".into(),
                req: Request::WaitFor { text: "完成".into(), timeout_ms: 3000 }
            }
        );
        // 第 2 步参数错：一步都不执行，报出是第几步
        let e = batch_steps(&args(
            json!({"steps": [{"tool": "iphone_tap", "x": 1, "y": 2}, {"tool": "iphone_tap", "x": 1}]}),
        ))
        .unwrap_err();
        assert!(e.contains("第 2 步"), "{e}");
        // 看界面的工具、未知工具、批量套批量都不行
        for bad in ["iphone_screenshot", "iphone_batch", "rm"] {
            assert!(batch_steps(&args(json!({"steps": [{"tool": bad}]}))).is_err(), "{bad}");
        }
        assert!(batch_steps(&args(json!({"steps": [{"tool": "wait", "seconds": 9}]}))).is_err());
        assert!(batch_steps(&args(json!({"steps": []}))).is_err());
        let many: Vec<Value> = (0..21).map(|_| json!({"tool": "wait", "seconds": 0})).collect();
        assert!(batch_steps(&args(json!({"steps": many}))).is_err());
    }

    #[test]
    fn reuse_elements_only_affects_tap_element_steps() {
        let steps = |reuse: bool| {
            batch_steps(&args(json!({"reuse_elements": reuse, "steps": [
                {"tool": "iphone_tap_element", "text": "One"},
                {"tool": "iphone_tap", "x": 1, "y": 2}
            ]})))
            .unwrap()
        };
        assert_eq!(
            steps(true)[0],
            Step::Request {
                tool: "iphone_tap_element".into(),
                req: Request::TapElement { text: "One".into(), index: 0, max_age_ms: BATCH_REUSE_MS }
            }
        );
        assert!(matches!(&steps(false)[0], Step::Request { req: Request::TapElement { max_age_ms: 0, .. }, .. }));
        assert_eq!(steps(true)[1], steps(false)[1]);
    }

    #[test]
    fn missed_tap_texts_are_recognised() {
        assert!(tap_missed("没找到包含 \"x\" 的元素,先用 iphone_list_elements 或截图看看屏幕"));
        assert!(tap_missed("只匹配到 1 个:\nButton \"x\""));
        assert!(!tap_missed("已点击 Button \"x\""));
    }

    #[test]
    fn wait_for_timeout_is_bounded() {
        assert_eq!(
            to_request("iphone_wait_for", &args(json!({"text": "x"}))).unwrap(),
            Request::WaitFor { text: "x".into(), timeout_ms: 5000 }
        );
        assert!(to_request("iphone_wait_for", &args(json!({"text": "x", "timeout": 60}))).is_err());
    }

    #[test]
    fn wrong_typed_parameters_are_errors_not_defaults() {
        let args = |v: Value| v.as_object().unwrap().clone();
        assert!(to_request("iphone_tap_element", &args(json!({"text": "设置", "index": -1}))).is_err());
        assert!(to_request("iphone_list_elements", &args(json!({"limit": 1.5}))).is_err());
        assert!(to_request("iphone_swipe", &args(json!({"from_x": 1, "from_y": 2, "to_x": "x", "to_y": 4}))).is_err());
        // 写成字符串的数字照收（toolkit 的 pydantic 也收）
        assert_eq!(
            to_request("iphone_tap_element", &args(json!({"text": "设置", "index": "2"}))).unwrap(),
            Request::TapElement { text: "设置".into(), index: 2, max_age_ms: 0 }
        );
        assert_eq!(
            to_request("iphone_list_elements", &args(json!({"limit": null}))).unwrap(),
            Request::ListElements { filter: String::new(), limit: 150, max_age_ms: 0 }
        );
    }

    #[test]
    fn screenshot_region_needs_all_four() {
        assert_eq!(to_request("iphone_screenshot", &Map::new()).unwrap(), Request::Screenshot { region: None });
        assert_eq!(
            to_request("iphone_screenshot", &args(json!({"x": 0, "y": 55, "width": 430, "height": 40}))).unwrap(),
            Request::Screenshot { region: Some((0.0, 55.0, 430.0, 40.0)) }
        );
        assert_eq!(
            to_request("iphone_screenshot", &args(json!({"x": 10, "y": 10}))).unwrap_err(),
            "放大区域要同时给出 x、y、width、height"
        );
    }

    /// 回归用例 #17：预期内的错误要作为工具错误、带可读文字返回
    #[test]
    fn errors_become_tool_errors_with_text() {
        let r = to_result(Response::error("button 只能是 home / volume_up / volume_down / lock"));
        assert_eq!(r.is_error, Some(true));
        assert!(matches!(&r.content[0], ContentBlock::Text(t) if t.text.contains("button 只能是")));
        let img = to_result(Response::Image { jpeg_base64: "AAAA".into(), caption: "430x932 点".into() });
        assert_eq!(img.content.len(), 2);
        assert!(to_request("iphone_nope", &Map::new()).is_err());
    }
}
