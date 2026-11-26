use super::*;

#[derive(Debug, Clone)]
pub struct McpServer {
    pub socket: PathBuf,
    /// This process works for a task (`IPHONE_CONTROL_TASK_TOKEN`, see [`task`]); over HTTP a request may instead
    /// carry a task's token in [`protocol::TASK_TOKEN_HEADER`] (`request_token`)
    pub task: bool,
    /// Held for the length of each tool call ([`serve_stdio`]); `None` where the process holds one of its own
    pub lease: Option<Lease>,
}

/// Takes the host's shared maintenance lease (`ictl_daemon::maintenance::shared`): an update waits while it is held
pub type Lease = fn() -> std::io::Result<std::fs::File>;

/// How long a tool call waits for an update that holds the maintenance lock before it goes on without the lease
/// (the daemon then answers, or `protocol::connect` waits for it to be back)
pub(crate) const LEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// The lease for one tool call: taken, waited for while an update runs, or `None` (going on without)
pub(crate) async fn hold(take: Lease) -> Option<std::fs::File> {
    let started = std::time::Instant::now();
    loop {
        match take() {
            Ok(f) => return Some(f),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && started.elapsed() < LEASE_WAIT => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(_) => return None,
        }
    }
}

/// The task token an HTTP request carries ([`protocol::TASK_TOKEN_HEADER`]); `None` over stdio
pub(crate) fn request_token(context: &RequestContext<RoleServer>) -> Option<String> {
    let parts = context.extensions.get::<http::request::Parts>()?;
    let t = parts.headers.get(protocol::TASK_TOKEN_HEADER)?.to_str().ok()?.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// MCP over HTTP (Streamable HTTP, one session per client) for the viewer's `/mcp`
pub type HttpService = rmcp::transport::streamable_http_server::StreamableHttpService<
    McpServer,
    rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
>;

/// The service behind `/mcp`. It doesn't check `Host` or `Origin`: the viewer does, with the names of its page
pub fn http_service(socket: PathBuf) -> HttpService {
    use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(Vec::<String>::new());
    StreamableHttpService::new(
        move || Ok(McpServer { socket: socket.clone(), task: false, lease: None }),
        std::sync::Arc::default(),
        config,
    )
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(instructions())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut t = tools();
        if self.task || request_token(&context).is_some() {
            t.push(confirm_tool());
        }
        Ok(ListToolsResult::with_all_items(t))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // an idle server holds nothing (an update can go ahead between calls); a call holds the lease to its end
        let _lease = match self.lease {
            Some(take) => hold(take).await,
            None => None,
        };
        let token = request_token(&context);
        if !self.task && token.is_none() {
            return self.call_tool_cancellable(request, &context).await;
        }
        // every request of this call goes out with the HTTP request's token (over stdio: the process's)
        protocol::as_task(token, None, self.call_tool_in_task(request, context)).await
    }
}

impl McpServer {
    /// [`ServerHandler::call_tool`] for a task's agent
    async fn call_tool_in_task(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // working for a task: the daemon holds or refuses the calls; announce each so the page shows the tool
        let name = request.name.clone();
        let args = request.arguments.clone().unwrap_or_default();
        let work = async {
            if request.name == task::CONFIRM_TOOL {
                if let Err(e) = check_args(&request.name, &args, &[], false) {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(e)]).into());
                }
                let r = match task::confirm(&self.socket, &args).await {
                    Ok(m) => CallToolResult::success(vec![ContentBlock::text(m)]),
                    Err(m) => CallToolResult::error(vec![ContentBlock::text(m)]),
                };
                return Ok(r.into());
            }
            match task::before(&self.socket, &request.name, &args).await {
                task::Before::Go(step) => {
                    let t0 = std::time::Instant::now();
                    let r = protocol::as_task(None, Some(step), self.call_tool_inner(request)).await;
                    if let Ok(CallToolResponse::Complete(c)) = &r {
                        let v = serde_json::to_value(c).unwrap_or_default();
                        task::after(&self.socket, step, &v, t0.elapsed().as_millis() as u64).await;
                    }
                    r
                }
                task::Before::Refuse(m) => {
                    Ok(input_error(m, &name, Delivery::NotSent, Some(FailureStage::Preflight)).into())
                }
            }
        };
        tokio::select! {
            r = work => r,
            _ = context.ct.cancelled() => Ok(input_error(ictl_l10n::tr!("调用已取消", "Call cancelled"), &name, Delivery::Unknown, None).into()),
        }
    }

    async fn call_tool_cancellable(
        &self,
        request: CallToolRequestParams,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // rmcp 收到取消通知只是置这个令牌，不会中止处理：自己停下。丢掉进行中的调用会断开到守护进程的连接，
        // 守护进程那边还在排队的请求随之放弃（protocol::call），iphone_batch 剩下的步骤也不再执行
        let name = request.name.clone();
        tokio::select! {
            r = self.call_tool_inner(request) => r,
            _ = context.ct.cancelled() => Ok(input_error(ictl_l10n::tr!("调用已取消", "Call cancelled"), &name, Delivery::Unknown, None).into()),
        }
    }

    pub(crate) async fn call_tool_inner(&self, request: CallToolRequestParams) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        if let Err(e) = check_args(&request.name, &args, &[], true) {
            return Ok(input_error(e, &request.name, Delivery::NotSent, Some(FailureStage::Preflight)).into());
        }
        let then = match parse_then(&args) {
            Ok(t) => t,
            Err(e) => return Ok(input_error(e, &request.name, Delivery::NotSent, Some(FailureStage::Preflight)).into()),
        };
        if request.name == "iphone_batch" {
            return Ok(self.batch(&args, then).await.into());
        }
        if DEV_TOOLS.contains(&&*request.name) && !dev_tools_enabled() {
            let name = &request.name;
            return Ok(to_result(Response::error(ictl_l10n::trf!(
                "{name} 是开发者工具,要先设 IPHONE_CONTROL_DEV_TOOLS=1",
                "{name} is a developer tool; set IPHONE_CONTROL_DEV_TOOLS=1 first"
            )))
            .into());
        }
        let req = match to_request(&request.name, &args) {
            Ok(r) => r,
            Err(e) => return Ok(input_error(e, &request.name, Delivery::NotSent, Some(FailureStage::Preflight)).into()),
        };
        // then 只对操作类工具有意义；看界面的工具本身就是返回界面
        // A single input and its readbacks must all apply to the same device. Tasks already pin theirs.
        let udid = if req.reports_delivery() && !protocol::in_task() {
            match self.controlled_udid().await {
                Ok(id) => Some(id),
                Err((message, stage)) => {
                    return Ok(input_error(message, &request.name, Delivery::NotSent, Some(stage)).into());
                }
            }
        } else {
            None
        };
        let resp = self.call_on(&req, udid.as_deref()).await;
        let input = req.reports_delivery().then(|| InputResult {
            delivery: resp.delivery(&req),
            stage: resp.failure_stage(),
            field: resp.field_readback(&req),
            udid: udid.as_deref(),
        });
        if matches!(req, Request::ScrollCollect(_)) {
            return Ok(to_result(resp).into());
        }
        Ok(match (resp, input) {
            (Response::Error { mut message, .. }, Some(input)) => {
                add_delivery_hint(&mut message, input.delivery);
                self.finish(message, true, then, false, false, Some(input)).await
            }
            (Response::Text { text, outcome, .. }, input) if req.is_device_action() => {
                // 结果里已经说了有弹窗（启动 App 被挡）就不再查
                let check = wants_alert_check(&req) && !protocol::alert_reported(&text, outcome);
                // 按名字没点到东西：界面没动过，不用等它稳定，直接给界面（按名字点击刚读过的树）
                let settle = !tap_missed(&text, outcome);
                self.finish(text, false, then, check, settle, input).await
            }
            (resp, _) => to_result(resp),
        }
        .into())
    }

    async fn call(&self, req: &Request) -> Response {
        self.call_on(req, None).await
    }

    /// `expect_udid`：守护进程正在控制的不是这台就不执行（见 [`protocol::EXPECT_UDID`]）
    async fn call_on(&self, req: &Request, expect_udid: Option<&str>) -> Response {
        // 守护进程正忙（例如在自动解锁）时会排队；最多等 150 秒，免得 agent 一直挂着。An install uploads the package and
        // can take several minutes on its own
        let wait = if matches!(req, Request::InstallApp { .. }) { 660 } else { 150 };
        // in a task the daemon holds calls while the user has paused or taken over, as long as that takes
        let wait = (!protocol::in_task()).then(|| Duration::from_secs(wait));
        protocol::exchange_on(&self.socket, req, wait, expect_udid, None, None).await.unwrap_or_else(|e| {
            let reason = e.source;
            let message = if e.failure.stage == FailureStage::Connect {
                ictl_l10n::trf!(
                    "连不上守护进程({}):{reason};确认守护进程在运行(iphone-control status)",
                    "Can't reach the daemon ({}): {reason}; make sure the daemon is running (iphone-control status)",
                    self.socket.display()
                )
            } else {
                ictl_l10n::trf!("守护进程请求失败: {reason}", "Daemon request failed: {reason}")
            };
            Response::error(message).with_failure(e.failure.delivery, e.failure.stage)
        })
    }

    /// 操作完成后：按 then 等界面稳定，再附上元素列表或截图（等稳定时顺带提醒弹窗）。
    /// 不带 then 时，`alert_check` 为真就查一下有没有系统弹窗挡着。`settle` 为假（什么都没点）时不等稳定
    async fn finish(
        &self,
        mut text: String,
        failed: bool,
        opts: ThenOpts,
        alert_check: bool,
        settle: bool,
        input: Option<InputResult<'_>>,
    ) -> CallToolResult {
        let expect_udid = input.as_ref().and_then(|i| i.udid);
        let mut elements = None;
        let mut readback = json!({"settle": "not_requested", "screen": "not_requested"});
        let mut image = None;
        if opts.then == Then::None {
            if alert_check {
                match self.call_on(&Request::AlertCheck, expect_udid).await {
                    Response::Text { text: t, .. } => {
                        text += &t;
                        readback["alert"] = "ok".into();
                    }
                    Response::Error { message, .. } if input.is_some() => {
                        text += &ictl_l10n::trf!("\n(检查弹窗失败:{message})", "\n(alert check failed: {message})");
                        readback["alert"] = "failed".into();
                    }
                    _ => readback["alert"] = "failed".into(),
                }
            }
        } else {
            if settle {
                match self.call_on(&Request::Settle { max_ms: opts.settle_ms }, expect_udid).await {
                    Response::Text { text: t, .. } => {
                        text += &format!("\n{t}");
                        readback["settle"] = "ok".into();
                    }
                    Response::Error { message, .. } => {
                        text += &ictl_l10n::trf!(
                            "\n(等界面稳定失败:{message})",
                            "\n(waiting for the screen to settle failed: {message})"
                        );
                        readback["settle"] = "failed".into();
                    }
                    _ => readback["settle"] = "failed".into(),
                }
            }
            let read = match opts.then {
                Then::Elements(filter) => {
                    Request::ListElements { filter, limit: THEN_LIMIT, max_age_ms: THEN_SETTLE_MS, keep_locked: false }
                }
                Then::Screenshot(region) => Request::Screenshot { region },
                Then::None => unreachable!(),
            };
            let screen = match self.call_on(&read, expect_udid).await {
                Response::Text { text: t, data, .. } if matches!(read, Request::ListElements { .. }) => {
                    elements = data;
                    text += &ictl_l10n::trf!("\n--- 操作后的界面 ---\n{t}", "\n--- Screen after the action ---\n{t}");
                    "ok"
                }
                Response::Image { jpeg_base64, caption } if matches!(read, Request::Screenshot { .. }) => {
                    image = Some((jpeg_base64, caption));
                    "ok"
                }
                Response::Error { message, .. } => {
                    text += &ictl_l10n::trf!("\n(读界面失败:{message})", "\n(reading the screen failed: {message})");
                    "failed"
                }
                _ => {
                    text += ictl_l10n::tr!("\n(未收到请求的界面)", "\n(The requested screen was not returned.)");
                    "failed"
                }
            };
            readback["screen"] = screen.into();
        }
        let read_failed = readback.as_object().unwrap().values().any(|v| v == "failed");
        if input.is_some() && read_failed && !failed {
            text += ictl_l10n::tr!(
                "\n请单独读取屏幕确认结果，避免重复之前的输入。",
                "\nRead the screen separately before repeating any input."
            );
        }
        let mut blocks = vec![ContentBlock::text(text)];
        if let Some((jpeg, caption)) = image {
            blocks.push(ContentBlock::image(jpeg, "image/jpeg"));
            blocks.push(ContentBlock::text(caption));
        }
        let mut result = if failed { CallToolResult::error(blocks) } else { CallToolResult::success(blocks) };
        result = with_element_data(result, elements);
        if let Some(input) = input {
            let data = result.structured_content.get_or_insert_with(|| json!({}));
            data["delivery"] = json!(input.delivery);
            data["readback"] = readback;
            if let Some(field) = input.field {
                data["readback"]["field"] = field.into();
            }
            if let Some(stage) = input.stage.or_else(|| (read_failed && !failed).then_some(FailureStage::Readback)) {
                data["stage"] = json!(stage);
            }
        }
        result
    }

    /// The cached daemon snapshot includes device.udid on older daemons as well.
    async fn controlled_udid(&self) -> Result<String, (String, FailureStage)> {
        match self.call(&Request::Daemon).await {
            Response::Text { text, .. } => {
                if let Ok(snapshot) = serde_json::from_str::<Value>(&text)
                    && let Some(id) = snapshot["device"]["udid"].as_str().filter(|id| !id.is_empty())
                {
                    return Ok(id.to_owned());
                }
            }
            Response::Error { message, failure, .. } => {
                return Err((message, failure.map_or(FailureStage::Preflight, |f| f.stage)));
            }
            Response::Image { .. } => {}
        }
        Err((
            ictl_l10n::tr!(
                "无法确定当前控制的设备，操作未执行",
                "Cannot identify the controlled device; action not sent"
            )
            .into(),
            FailureStage::Preflight,
        ))
    }

    /// 按顺序执行；某一步出错就停，说明是第几步、后面几步没执行。出错时也按 then 附上当前界面，方便看停在哪
    async fn batch(&self, args: &Map<String, Value>, then: ThenOpts) -> CallToolResult {
        let steps = match batch_steps(args) {
            Ok(s) => s,
            Err(e) => return to_result(Response::error(e)),
        };
        let total = steps.len();
        // 每一步都要求还是开始时控制的那台：自动选设备时拔掉它会改控另一台，剩下的步骤不能落到那台上
        let udid = self.controlled_udid().await.ok();
        let mut lines = Vec::new();
        // 最后一步是不是需要查弹窗的操作
        let mut check = false;
        for (i, step) in steps.into_iter().enumerate() {
            let n = i + 1;
            match step {
                Step::Wait(d) => {
                    tokio::time::sleep(d).await;
                    lines.push(ictl_l10n::trf!("{n}. wait → 等了 {} 秒", "{n}. wait → waited {} s", d.as_secs_f64()));
                }
                Step::Request { tool, req } => match self.call_on(&req, udid.as_deref()).await {
                    // 按名字没点到（toolkit 兼容：单独调用时是普通结果）在批量里要停下：后面的步骤都建立在它点中的前提上
                    Response::Text { text, outcome, .. }
                        if tool == "iphone_tap_element" && tap_missed(&text, outcome) =>
                    {
                        lines.push(format!("{n}. {tool} → {text}"));
                        if n < total {
                            lines.push(ictl_l10n::trf!(
                                "第 {n} 步没点到,后面 {} 步没有执行",
                                "Step {n} tapped nothing, the remaining {} step(s) didn't run",
                                total - n
                            ));
                        }
                        return self.finish(lines.join("\n"), true, then, false, true, None).await;
                    }
                    Response::Text { text, outcome, .. } => {
                        check = wants_alert_check(&req) && !protocol::alert_reported(&text, outcome);
                        lines.push(format!("{n}. {tool} → {text}"));
                    }
                    Response::Image { caption, .. } => lines.push(format!("{n}. {tool} → {caption}")),
                    Response::Error { message, .. } => {
                        lines.push(ictl_l10n::trf!("{n}. {tool} → 出错:{message}", "{n}. {tool} → error: {message}"));
                        if n < total {
                            lines.push(ictl_l10n::trf!(
                                "第 {n} 步出错,后面 {} 步没有执行",
                                "Step {n} failed, the remaining {} step(s) didn't run",
                                total - n
                            ));
                        }
                        return self.finish(lines.join("\n"), true, then, true, true, None).await;
                    }
                },
            }
        }
        self.finish(lines.join("\n"), false, then, check, true, None).await
    }
}

/// 在 stdin/stdout 上运行 MCP 服务直到客户端断开
/// The MCP server on stdin/stdout. `lease` is taken for each tool call, not for the life of the process: agents keep
/// this process running for hours, and an update has to wait only for calls under way
/// `task_token`: the task to work for, given by the caller; without it the environment's
/// ([`crate::task::join_from_env`])
pub async fn serve_stdio(socket: PathBuf, lease: Option<Lease>, task_token: Option<String>) -> anyhow::Result<()> {
    let task = match task_token {
        Some(t) => {
            protocol::join_task(t.trim());
            true
        }
        None => task::join_from_env(),
    };
    let running = McpServer { socket, task, lease }.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
