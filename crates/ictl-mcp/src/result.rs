use super::*;

/// tap_element 的结果是不是「没点到」（没找到、index 超出匹配个数、或整个被别的东西盖住）
pub fn tap_missed(outcome: Option<protocol::Outcome>) -> bool {
    outcome.is_some_and(|o| o.tapped_nothing)
}

/// 不带 then 的操作做完后要不要查系统弹窗（弹窗挡着时操作都落在弹窗上，WDA 照样报成功）。
/// 按键自己处理（Home 先查弹窗；锁屏后不用查），结束 App 后前台是主屏，不会冒出新弹窗
/// Every variant is listed (no `_`), so a new request type has to be decided here
pub fn wants_alert_check(req: &Request) -> bool {
    match req {
        Request::Tap { .. }
        | Request::TapSequence { .. }
        | Request::TapElement { .. }
        | Request::LongPress { .. }
        | Request::Swipe { .. }
        | Request::TypeText { .. }
        | Request::ReplaceText { .. }
        | Request::LaunchApp { .. }
        | Request::OpenUrl { .. } => true,
        // 按键见 PressButton 自己的弹窗处理；切换设备后新设备的 WDA 还没起来，查不了弹窗；
        // 安装、卸载、Web Inspector 不碰屏幕（Web Inspector 也不该为它排队等设备锁）
        // the web viewer's finger isn't an MCP tool; the person watching sees any alert
        Request::PressButton { .. }
        | Request::Touch { .. }
        | Request::TerminateApp { .. }
        | Request::SelectDevice { .. }
        | Request::InstallApp { .. }
        | Request::UninstallApp { .. }
        | Request::WebEval { .. }
        | Request::Status
        | Request::ScreenStream
        | Request::MjpegStream
        | Request::Screenshot { .. }
        | Request::ScrollCollect(_)
        | Request::ListElements { .. }
        | Request::ListApps { .. }
        | Request::Settle { .. }
        | Request::AlertCheck
        | Request::WaitFor { .. }
        | Request::WaitForDisappearance { .. }
        | Request::Daemon
        | Request::Doctor
        | Request::Resign { .. }
        | Request::UnlockReset
        | Request::UnlockResetDevice { .. }
        | Request::SetupStep { .. }
        | Request::WdaForward { .. }
        | Request::PinCurrent { .. }
        | Request::ListDevices
        | Request::AppLogs { .. }
        | Request::CrashReports { .. }
        | Request::WebTargets
        // tasks: not tools, nothing on the screen
        | Request::TaskOpen { .. }
        | Request::TaskEvents { .. }
        | Request::TaskList
        | Request::TaskControl { .. }
        | Request::TaskAgent
        | Request::TaskCheck
        | Request::TaskStep { .. }
        | Request::TaskStepDone { .. }
        | Request::TaskConfirm { .. }
        | Request::TaskNote { .. }
        | Request::TaskFinish { .. } => false,
    }
}

/// The shape of [`with_element_data`]' structured content, declared as `iphone_list_elements`' output schema (action
/// tools give it with then=elements, but not always: a declared schema would oblige every result to carry it).
/// `matched_count` and `truncated` are left out by a daemon older than them
pub(crate) fn elements_output_schema() -> Map<String, Value> {
    let element = json!({
        "type": "object",
        "properties": {
            "type": {"type": "string"}, "label": {"type": "string"},
            "x": {"type": "number"}, "y": {"type": "number"}, "width": {"type": "number"}, "height": {"type": "number"}
        }
    });
    let v = json!({
        "type": "object",
        "properties": {
            "type": {"const": "elements"},
            "elements": {"type": "array", "items": element,
                "description": "the elements listed, at most limit; x/y the top left corner in points"},
            "matched_count": {"type": "integer", "description": "how many elements matched filter in this read, before limit"},
            "truncated": {"type": "boolean", "description": "matched_count is more than limit: some weren't listed"},
            "incomplete": {"type": "boolean", "description": "the accessibility walk stopped before reading the whole screen"},
            "incomplete_reason": {"type": "string", "description": "why the accessibility walk was incomplete (element_limit, timeout, depth_limit, unreadable, overlays_incomplete)"}
        },
        "required": ["elements"]
    });
    v.as_object().cloned().unwrap_or_default()
}

pub(crate) fn with_element_data(mut result: CallToolResult, data: Option<protocol::Data>) -> CallToolResult {
    if let Some(data @ (protocol::Data::Elements { .. } | protocol::Data::ScrollCollection { .. })) = data {
        result.structured_content = serde_json::to_value(data).ok();
    }
    result
}

/// 守护进程响应 → MCP 结果。错误走工具错误（is_error），文字原样给客户端
pub fn to_result(r: Response) -> CallToolResult {
    match r {
        Response::Text { text, data, .. } => {
            with_element_data(CallToolResult::success(vec![ContentBlock::text(text)]), data)
        }
        Response::Image { jpeg_base64, caption, geometry } => {
            let mut result = CallToolResult::success(vec![
                ContentBlock::image(jpeg_base64, "image/jpeg"),
                ContentBlock::text(caption),
            ]);
            if let Some(g) = geometry {
                result.structured_content = Some(json!({
                    "type": "screenshot",
                    "width": g.width,
                    "height": g.height,
                    "point_width": g.point_width,
                    "point_height": g.point_height,
                    "region": g.region,
                    "coordinate_scale": g.coordinate_scale,
                }));
            }
            result
        }
        Response::Error { message, .. } => CallToolResult::error(vec![ContentBlock::text(message)]),
    }
}

/// Tools that expose delivery of their primary tap, gesture, or text input.
pub fn reports_input(name: &str) -> bool {
    matches!(name, "iphone_tap" | "iphone_tap_element" | "iphone_type_text" | "iphone_swipe" | "iphone_long_press")
}

pub(crate) fn input_error(
    message: impl Into<String>,
    name: &str,
    delivery: Delivery,
    stage: Option<FailureStage>,
) -> CallToolResult {
    let mut message = message.into();
    if !reports_input(name) {
        return to_result(Response::error(message));
    }
    if delivery == Delivery::Unknown {
        message += ictl_l10n::tr!(
            "。输入可能已送达，请先查看屏幕再决定下一步。",
            ". Input may have reached the phone. Inspect the screen before retrying."
        );
    }
    let mut result = to_result(Response::error(message));
    result.structured_content =
        Some(json!({"delivery": delivery, "readback": {"screen": "not_requested", "settle": "not_requested"}}));
    if let Some(stage) = stage {
        result.structured_content.as_mut().unwrap()["stage"] = json!(stage);
    }
    result
}

pub(crate) fn add_delivery_hint(message: &mut String, delivery: Delivery) {
    let lower = message.to_ascii_lowercase();
    let already_guided =
        ["inspect the screen", "take a screenshot", "check with a screenshot", "查看屏幕", "截图确认", "截图看看"]
            .iter()
            .any(|hint| lower.contains(hint));
    message.truncate(message.trim_end().trim_end_matches(['.', '。']).len());
    message.push_str(match delivery {
        Delivery::NotSent => ictl_l10n::tr!("。目标输入未发出。", ". The requested input was not sent."),
        Delivery::Unknown => ictl_l10n::tr!("。输入可能已送达。", ". Input may have reached the phone."),
        Delivery::Sent => ictl_l10n::tr!("。输入已发出。", ". Input was sent."),
    });
    if !already_guided && delivery != Delivery::NotSent {
        message.push_str(ictl_l10n::tr!("请先查看屏幕再决定下一步。", " Inspect the screen before retrying."));
    }
}

pub(crate) struct InputResult<'a> {
    pub(crate) delivery: Delivery,
    pub(crate) stage: Option<FailureStage>,
    pub(crate) field: Option<&'static str>,
    pub(crate) udid: Option<&'a str>,
}
