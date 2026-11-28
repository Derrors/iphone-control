use super::*;

/// 服务说明（发给客户端的 instructions）：toolkit 的那段，加上自动解锁范围、多设备的说明
pub fn instructions() -> &'static str {
    ictl_l10n::tr!(
        "控制一台通过 USB 连接的真实 iPhone(或 iPad)。坐标单位是点(pt),与 iphone_screenshot 图像像素一致。\
操作前先用 iphone_screenshot 或 iphone_list_elements 看清屏幕;能按名字点就优先用 iphone_tap_element。\
游戏、地图等界面没有元素树,只能看截图按坐标操作;小图标、小字看不清时用 iphone_screenshot 的区域放大。\
配置了锁屏密码时,截图、读元素和操作类工具会先自动解锁(iphone_status、列 App、列设备、切换设备和开发者工具不会,按 lock 键也不会)。每次操作后再截图确认结果。\
涉及付款、删除、发送消息、修改账号或隐私设置的操作,先向用户确认。\
省时间:操作类工具可带 then=elements 或 then=screenshot,等界面稳定后顺带返回操作后的界面,不用再单独调一次;\
几步确定的连续操作(按键、逐级点菜单、填表)用 iphone_batch 一次发完;等页面加载用 iphone_wait_for,不要反复截图。\
接着多台 iPhone/iPad 时同一时间只控制一台:用 iphone_list_devices 查看,iphone_select_device 切换。",
        "Controls a real iPhone (or iPad) connected over USB. Coordinates are in points (pt), the same as the pixels of an iphone_screenshot image. Before acting, look at the screen with iphone_screenshot or iphone_list_elements; prefer iphone_tap_element whenever the target has text. Games, maps and similar screens have no element tree: work from screenshots and coordinates, and zoom into small icons or text with an iphone_screenshot region. If a passcode is configured, screenshots, reading elements and the action tools unlock the phone first (iphone_status, listing apps or devices, switching devices and the developer tools don't, nor does pressing lock). Check the result with a screenshot after each action. Ask the user before anything that pays, deletes, sends a message, or changes account or privacy settings. Save time: action tools accept then=elements or then=screenshot to return the screen once it has settled, without a separate call; send a few predictable steps in a row (keys, menu levels, forms) as one iphone_batch; wait for a page with iphone_wait_for instead of repeated screenshots. With several iPhones/iPads connected only one is controlled at a time: see them with iphone_list_devices, switch with iphone_select_device."
    )
}

pub(crate) fn num(desc: &str) -> Value {
    json!({"type": "number", "description": desc})
}

pub(crate) fn schema(props: Value, required: &[&str]) -> Map<String, Value> {
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
pub(crate) fn then_props() -> Value {
    json!({
        "then": {"type": "string", "enum": ["none", "elements", "screenshot"], "default": "none",
                 "description": ictl_l10n::tr!("操作后等界面稳定,再顺带返回界面:elements=元素列表(最多 80 个),screenshot=截图。省一次单独的调用。操作前看过界面(截图、读元素)时,还会说明和操作前相比界面变没变;没变说明操作可能没生效", "After the action, wait for the screen to settle and return it: elements = element list (up to 80), screenshot = screenshot. Saves a separate call. If the screen was looked at before the action (screenshot, element list), it also says whether the screen changed since; no change means the action may not have taken effect")},
        "then_filter": {"type": "string", "default": "", "description": ictl_l10n::tr!("then=elements 时只列包含这段文字的元素", "With then=elements: only list elements containing this text")},
        "then_region": {"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4,
                        "description": ictl_l10n::tr!("then=screenshot 时只返回这块区域的放大图 [x, y, width, height](点),例如游戏棋盘", "With then=screenshot: only return this region, enlarged, as [x, y, width, height] (points), e.g. a game board")},
        "then_settle": {"type": "number", "default": 1.5,
                        "description": ictl_l10n::tr!("顺带返回界面前最多等几秒让界面稳定(不超过 10)。游戏、地图按画面是否静止判断,消除连锁等长动画可调大,画面一静止就返回", "Maximum seconds to wait for the screen to settle before returning it (up to 10). Games and maps are judged by whether the picture is still; raise it for long animations such as chain reactions, it returns as soon as the picture is still")}
    })
}

/// 在参数 schema 里加上 then / then_filter
pub(crate) fn with_then(props: Value) -> Value {
    let mut p = props;
    if let (Some(o), Value::Object(extra)) = (p.as_object_mut(), then_props()) {
        o.extend(extra);
    }
    p
}

/// 操作类工具描述的共同结尾：返回什么、then 能做什么、自动解锁。
/// `alert`：不带 then 时会不会查系统弹窗（见 [`wants_alert_check`]）
pub(crate) fn action_desc(body: &str, example: &str, alert: bool) -> String {
    let mut s = ictl_l10n::trf!(
        "{body}\n\n返回:一行结果文字,例如「{example}」。",
        "{body}\n\nReturns: one line of result text, e.g. “{example}”."
    );
    if alert {
        s += ictl_l10n::tr!(
            "不带 then 时,如果系统弹窗挡着屏幕,结果里会提醒(这时操作落在了弹窗上)。",
            " Without then, the result warns if a system alert covers the screen (the action then landed on the alert)."
        );
    }
    s += ictl_l10n::tr!(
        "传 then=elements 或 then=screenshot 会等界面稳定后附上操作后的元素列表或截图,省一次单独的调用。\
配置了锁屏密码时会先自动解锁。",
        " With then=elements or then=screenshot it waits for the screen to settle and attaches the element list or screenshot after the action, saving a separate call. If a passcode is configured, the phone is unlocked first."
    );
    s
}

/// 只看不改的工具
pub(crate) fn looking(title: &str) -> ToolAnnotations {
    ToolAnnotations::with_title(title).read_only(true).open_world(false)
}

/// 改变手机状态的工具。open_world：点击、输入可能经 App 触达外部（发消息、下单），所以都标 true
pub(crate) fn acting(title: &str, destructive: bool, idempotent: bool) -> ToolAnnotations {
    ToolAnnotations::with_title(title).read_only(false).destructive(destructive).idempotent(idempotent).open_world(true)
}

/// Names of the developer tools, listed only with [`dev_tools_enabled`]
pub const DEV_TOOLS: [&str; 6] = [
    "iphone_app_logs",
    "iphone_crash_reports",
    "iphone_install_app",
    "iphone_uninstall_app",
    "iphone_web_targets",
    "iphone_web_eval",
];

/// `IPHONE_CONTROL_DEV_TOOLS=1` (or true/yes/on) adds the developer tools; off by default, so agents that don't
/// need them don't pay for their descriptions
pub fn dev_tools_enabled() -> bool {
    std::env::var(ictl_protocol::env::DEV_TOOLS)
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// The tools this server lists: the 18 standard ones, plus the developer tools when switched on
pub fn tools() -> Vec<Tool> {
    tools_with(dev_tools_enabled())
}

pub fn tools_with(dev: bool) -> Vec<Tool> {
    let mut v = standard_tools();
    if dev {
        v.extend(dev_tools());
    }
    v
}

/// [`task::CONFIRM_TOOL`], listed only while working for a task (see [`task`])
pub fn confirm_tool() -> Tool {
    Tool::new(
        task::CONFIRM_TOOL,
        ictl_l10n::tr!(
            "请用户确认一步操作(在控制台页面或发起任务的地方),一直等到用户回答。\n\n何时用:付款、购买、下单、转账;删除或清空数据;发送消息、邮件、评论、帖子;\
修改账号、密码、隐私或安全设置;其他不可撤销的操作。在真正点下那一步之前调用。\n\n\
返回:用户同意时是普通结果,接着执行;用户拒绝时是错误结果,这时不要执行这一步,也不要换个方式绕过,停下来说明情况。",
            "Ask the user to confirm one action (on the control page or wherever the task was started), and wait for the answer.\n\nWhen to use: payments, purchases, orders, transfers; deleting or clearing data; sending messages, emails, comments, posts; changing account, password, privacy or security settings; anything else that can't be undone. Call it right before the step that does it.\n\nReturns: a normal result when the user approves, then go ahead; an error when the user rejects, then don't do that step or work around it: stop and explain."
        )
        .to_owned(),
        schema(
            json!({
                "action": {"type": "string", "description": ictl_l10n::tr!("要做的事,一句话,例如「发送消息」「删除照片」", "what is about to happen, in a few words, e.g. “Send message”, “Delete photo”")},
                "detail": {"type": "string", "description": ictl_l10n::tr!("具体内容:发给谁、发什么、删哪几项、付多少钱", "the specifics: to whom, what, which items, how much")}
            }),
            &["action", "detail"],
        ),
    )
    .with_annotations(ToolAnnotations::with_title(ictl_l10n::tr!("请用户确认", "Ask the user")).read_only(true).open_world(false))
}

pub(crate) fn dev_tools() -> Vec<Tool> {
    let t = |name: &'static str, desc: &str, s: Map<String, Value>, a: ToolAnnotations| {
        Tool::new(name, desc.to_owned(), s).with_annotations(a)
    };
    vec![
        t(
            "iphone_app_logs",
            ictl_l10n::tr!(
                "读手机的系统日志(os_log,默认、Error、Fault 级别;Debug、Info 不记录,记录它们会拖慢手机),可按 App 或进程、关键词、时间筛选,每行:多少秒前、进程[pid]、级别、子系统、内容。\n\n\
何时用:App 崩溃、报错、请求失败、界面没反应时,不靠截图找原因。第一次调用才开始记录(之前的拿不到):先调一次,复现问题,再调一次。\
记录在 15 分钟没有调用后自动停止。只读。",
                "Read the phone's system log (os_log, default, Error and Fault levels; Debug and Info aren't recorded, since streaming them slows the phone down), filtered by app or process, keyword and time; one line per entry: seconds ago, process[pid], level, subsystem, message.\n\nWhen to use: when an app crashes, shows an error, a request fails or the UI doesn't respond, to find the cause without screenshots. Recording starts with the first call (earlier lines aren't available): call it once, reproduce the problem, then call it again. Recording stops by itself after 15 minutes without a call. Read-only."
            ),
            schema(
                json!({
                    "process": {"type": "string", "default": "", "description": ictl_l10n::tr!("App 的显示名或 Bundle ID,或进程名(如 SpringBoard);空为全部", "app display name or bundle ID, or a process name (e.g. SpringBoard); empty for all")},
                    "grep": {"type": "string", "default": "", "description": ictl_l10n::tr!("只要内容或子系统包含这段文字的行(不分大小写)", "only lines whose message or subsystem contains this text (case-insensitive)")},
                    "since": {"type": "number", "default": 60, "description": ictl_l10n::tr!("只看最近多少秒,最多 3600", "only the last this many seconds, at most 3600")},
                    "limit": {"type": "integer", "default": 100, "description": ictl_l10n::tr!("最多返回多少行(最新的),1 到 1000", "at most this many lines (the newest), 1 to 1000")},
                    "errors_only": {"type": "boolean", "default": false, "description": ictl_l10n::tr!("只看 Error 和 Fault 级别", "only Error and Fault levels")}
                }),
                &[],
            ),
            looking(ictl_l10n::tr!("读日志", "Read logs")),
        ),
        t(
            "iphone_crash_reports",
            ictl_l10n::tr!(
                "列出手机上的崩溃报告(新的在前,最多 50 个),或用 name 读取其中一份(.ips,开头是异常类型、崩溃的线程和 App 版本)。\n\n\
何时用:App 闪退后找原因。只读。",
                "List the crash reports on the phone (newest first, at most 50), or read one with name (.ips; the start has the exception type, the crashed thread and the app version).\n\nWhen to use: to find out why an app quit unexpectedly. Read-only."
            ),
            schema(
                json!({
                    "filter": {"type": "string", "default": "", "description": ictl_l10n::tr!("只列文件名包含这段文字的(如进程名)", "only list file names containing this text (e.g. the process name)")},
                    "name": {"type": "string", "default": "", "description": ictl_l10n::tr!("要读取的报告文件名(来自列表);给了就返回内容", "file name of the report to read (from the list); if given, returns its content")}
                }),
                &[],
            ),
            looking(ictl_l10n::tr!("崩溃报告", "Crash reports")),
        ),
        t(
            "iphone_install_app",
            ictl_l10n::tr!(
                "安装签好名的 .ipa 或 .app(覆盖已装的同一 App)。path 是运行守护进程的机器上的路径。\
大的包要一两分钟。",
                "Install a signed .ipa or .app (replacing the same app if installed). path is on the machine that runs the daemon. Large packages take a minute or two."
            ),
            schema(
                json!({"path": {"type": "string", "description": ictl_l10n::tr!(".ipa 文件或 .app 目录的路径", "path of the .ipa file or .app directory")}}),
                &["path"],
            ),
            acting(ictl_l10n::tr!("安装 App", "Install app"), true, true),
        ),
        t(
            "iphone_uninstall_app",
            ictl_l10n::tr!(
                "卸载 App,连同它的数据。不会卸载 WebDriverAgent(iphone-control 靠它控制手机)。卸载前先问用户。",
                "Uninstall an app, together with its data. Refuses to remove WebDriverAgent, which iphone-control needs to control the phone. Ask the user before uninstalling."
            ),
            schema(
                json!({"app": {"type": "string", "description": ictl_l10n::tr!("Bundle ID 或显示名", "bundle ID or display name")}}),
                &["app"],
            ),
            acting(ictl_l10n::tr!("卸载 App", "Uninstall app"), true, true),
        ),
        t(
            "iphone_web_targets",
            ictl_l10n::tr!(
                "列出可检查的网页和 WebView(Web Inspector):Safari 标签页、App 里可检查的 WebView(Capacitor、Cordova、React Native 等的调试构建)。每行:id、类型、所属 App、标题和网址。\n\n\
何时用:页面内容在网页里时,用 iphone_web_eval 直接读 DOM,比截图和元素树快、省 token、也更准。需要手机上 设置 → Safari → 高级 → 网页检查器 打开。只读。",
                "List inspectable web pages and WebViews (Web Inspector): Safari tabs and inspectable WebViews in apps (debug builds of Capacitor, Cordova, React Native and similar). One line each: id, type, app, title and URL.\n\nWhen to use: when the content is a web page, read the DOM with iphone_web_eval instead; that is faster, cheaper in tokens and more accurate than screenshots or the element tree. Needs Settings → Safari → Advanced → Web Inspector on. Read-only."
            ),
            schema(json!({}), &[]),
            looking(ictl_l10n::tr!("列出网页", "List web pages")),
        ),
        t(
            "iphone_web_eval",
            ictl_l10n::tr!(
                "在网页或 WebView 里执行 JavaScript,返回结果的 JSON(Promise 会等它完成)。例如 document.title、\
[...document.querySelectorAll('button')].map(b => b.innerText)、document.querySelector('#email').value = 'a@b.c'。\n\n\
何时用:读网页内容、表单状态、控制台变量,或直接操作页面。只取需要的部分,结果超过 5 万字会被截断。\
脚本能改页面(提交表单、发请求),和点击一样要小心。",
                "Run JavaScript in a web page or WebView and return the result as JSON (a promise is awaited). For example document.title, [...document.querySelectorAll('button')].map(b => b.innerText), document.querySelector('#email').value = 'a@b.c'.\n\nWhen to use: to read page content, form state or JavaScript variables, or to act on the page directly. Select just what you need; results over 50,000 characters are cut. A script can change the page (submit forms, send requests), so take the same care as with taps."
            ),
            schema(
                json!({
                    "expression": {"type": "string", "description": ictl_l10n::tr!("要执行的 JavaScript 表达式", "the JavaScript expression to run")},
                    "target": {"type": "string", "default": "", "description": ictl_l10n::tr!("iphone_web_targets 列出的 id,或网址、标题里的一段文字;只有一个页面时可以不填", "an id from iphone_web_targets, or text in the page's URL or title; may be left out when there is only one page")},
                    "timeout": {"type": "number", "default": 10, "description": ictl_l10n::tr!("最多等多少秒,最多 60", "maximum seconds to wait, at most 60")}
                }),
                &["expression"],
            ),
            acting(ictl_l10n::tr!("执行 JavaScript", "Run JavaScript"), true, false),
        ),
    ]
}

/// 18 个工具的定义（toolkit 的 13 个 + iphone_wait_for、iphone_batch、iphone_list_devices、iphone_select_device、iphone_scroll_collect）。
/// 描述可以改写，但工具名、参数、默认返回不改（docs/decisions.md D10）
pub(crate) fn standard_tools() -> Vec<Tool> {
    let t = |name: &'static str, desc: String, s: Map<String, Value>, a: ToolAnnotations| {
        Tool::new(name, desc, s).with_annotations(a)
    };
    let app = || {
        json!({"app": {"type": "string",
            "description": ictl_l10n::tr!("Bundle ID(如 com.apple.Preferences)或显示名(如「设置」「微信」)", "Bundle ID (e.g. com.apple.Preferences) or display name (e.g. “Settings”)")}})
    };
    vec![
        t(
            "iphone_status",
            ictl_l10n::tr!("查看手机和服务的状态:WDA(WebDriverAgent)是否就绪、是否锁屏、能否自动解锁、当前前台 App 的 Bundle ID、\
屏幕尺寸(点)、守护进程状态。\n\n何时用:开始操作前确认手机可用;操作报错、怀疑 WDA 掉线或手机锁屏时排查。\
WDA 不可用时会说明哪些功能受影响(读元素、按名字点击、启动 App)。只读,不改变手机状态。", "Show the state of the phone and the service: whether WDA (WebDriverAgent) is ready, whether the phone is locked and can be auto-unlocked, the bundle ID of the foreground app, the screen size (points) and the daemon state.\n\nWhen to use: to check that the phone is usable before starting; to investigate when actions fail or WDA or the lock screen seem to be in the way. If WDA is unavailable it says which features are affected (reading elements, tapping by text, launching apps). Read-only, doesn't change the phone.")
                .into(),
            schema(json!({}), &[]),
            looking(ictl_l10n::tr!("查看状态", "Show status")),
        ),
        t(
            "iphone_screenshot",
            ictl_l10n::tr!("截取当前屏幕,返回 JPEG 图像和一行说明。图像已缩放到点尺寸,图中像素坐标可直接用于 iphone_tap、iphone_swipe。\n\n\
看不清小图标、小字时,传 x/y/width/height(点,四个要一起给)放大这块区域:从原始高清截图裁剪,小区域再额外放大。\
返回文字里有放大倍数,图中坐标换算回屏幕点:x + 像素/倍数。\n\n\
何时用:游戏、地图、图片等没有元素树的界面,或要确认画面效果时。只想知道有哪些按钮、文字,iphone_list_elements 更快更省上下文。\
操作后要看结果时,优先给操作工具带 then=screenshot。WDA 不可用时改用 CoreDevice 截图(iOS 27 起,说明里注明「HID」)。\
只读:不改变界面,但配置了锁屏密码时会先解锁。", "Capture the current screen and return a JPEG image plus a line of explanation. The image is scaled to points, so its pixel coordinates can be used directly with iphone_tap and iphone_swipe.\n\nTo read small icons or text, pass x/y/width/height (points, all four together) to zoom into that region: it is cropped from the full-resolution capture, and small regions are enlarged further. The text gives the zoom factor; convert image coordinates back to screen points with x + pixel / factor.\n\nWhen to use: screens without an element tree (games, maps, images), or to check how something looks. To find out which buttons and texts are there, iphone_list_elements is faster and uses less context. To see the result of an action, prefer then=screenshot on the action tool. If WDA is unavailable it uses a CoreDevice screenshot instead (iOS 27+, marked “HID”). Read-only: doesn't change the screen, but unlocks the phone first if a passcode is configured.")
                .into(),
            schema(
                json!({"x": num(ictl_l10n::tr!("区域左上角 x(点)", "left edge x of the region (points)")), "y": num(ictl_l10n::tr!("区域左上角 y(点)", "top edge y of the region (points)")), "width": num(ictl_l10n::tr!("区域宽(点)", "region width (points)")), "height": num(ictl_l10n::tr!("区域高(点)", "region height (points)"))}),
                &[],
            ),
            looking(ictl_l10n::tr!("截图", "Screenshot")),
        ),
        t(
            "iphone_list_elements",
            ictl_l10n::tr!("列出屏幕上带文字的界面元素,每行一个:类型、标签、名字、值、中心点坐标、尺寸(点)。\n\n\
何时用:找按钮、输入框、列表项的文字和位置,比截图快、省上下文;找到后用 iphone_tap_element 按文字点。\
游戏、地图等没有元素树的界面这里是空的,改用 iphone_screenshot。\n\n\
注意:按坐标判断是否在屏幕内,被弹窗遮住的元素也可能列出来,不确定时截图核对。需要 WDA。\
只读:不改变界面,但配置了锁屏密码时会先解锁。", "List the UI elements on screen that carry text, one per line: type, label, name, value, center point and size (points).\n\nWhen to use: to find buttons, text fields and list items with their text and position; faster than a screenshot and uses less context. Then tap one by its text with iphone_tap_element. Games, maps and other screens without an element tree return nothing here; use iphone_screenshot.\n\nNote: visibility is judged by position, so elements hidden behind an alert can still be listed; check with a screenshot when in doubt. Needs WDA. Read-only: doesn't change the screen, but unlocks the phone first if a passcode is configured.")
                .into(),
            schema(
                json!({
                    "filter": {"type": "string", "default": "",
                        "description": ictl_l10n::tr!("只返回类型/标签/名字/值里包含这段文字的元素(不区分大小写),例如 SearchField、Button、登录", "Only return elements whose type/label/name/value contains this text (case-insensitive), e.g. SearchField, Button, Login")},
                    "limit": {"type": "integer", "default": 150, "description": ictl_l10n::tr!("最多列几个,结构化结果包含匹配总数和截断标记", "Maximum number of elements to return; structured output reports the count and whether it was truncated")}
                }),
                &[],
            ),
            looking(ictl_l10n::tr!("列出界面元素", "List UI elements")),
        )
        .with_raw_output_schema(std::sync::Arc::new(elements_output_schema())),
        t(
            "iphone_scroll_collect",
            ictl_l10n::tr!("收集连续几屏里可见的文字行，合并相邻屏的重叠内容。可用 until 找到文字就停在当前屏。只滑动，不点击。返回 rows、scrolls、stop_reason，以及找到时当前屏上的 matched。",
                "Collect visible text rows across overlapping viewports. Use until to stop with a matching element on screen. Only scrolls, never taps. Returns rows, scrolls, stop_reason and the matched element on the final viewport.").into(),
            schema(json!({
                "scroll": {"type": "string", "enum": ["down", "up"], "default": "down", "description": ictl_l10n::tr!("往下或往上找", "look further down or up the list")},
                "until": {"type": "string", "description": ictl_l10n::tr!("可见元素的文字、标识或值包含这段文字时停止，不区分大小写", "stop at a visible label, name or value containing this text, case-insensitive")},
                "filter": {"type": "string", "description": ictl_l10n::tr!("只收集类型或文字包含它的行。与 until 独立", "collect rows whose type or text contains this string, independent of until")},
                "limit": {"type": "integer", "minimum": 1, "maximum": protocol::MAX_SCROLL_ROWS, "default": protocol::MAX_SCROLL_ROWS, "description": ictl_l10n::tr!("最多收集几行", "stop after collecting this many rows")},
                "max_scrolls": {"type": "integer", "minimum": 1, "maximum": protocol::MAX_SEARCH_SCROLLS, "default": 8, "description": ictl_l10n::tr!("最多滚动几次", "maximum number of scrolls")},
                "x": {"type": "number", "minimum": 0, "description": ictl_l10n::tr!("在哪个横坐标上下滑（点），默认屏幕正中。iPad 分栏界面（设置、邮件）只滚动手指所在的那一栏：要滚左栏，给左栏里的横坐标", "where to swipe across, in points from the left; the middle of the screen by default. An iPad split view (Settings, Mail) scrolls only the column swiped in: for the sidebar, an x inside it")}
            }), &[]),
            acting(ictl_l10n::tr!("滚动收集文字", "Scroll and collect text"), false, false),
        ),
        t(
            "iphone_tap",
            action_desc(
                ictl_l10n::tr!("点击屏幕上的一个坐标(单位是点,与 iphone_screenshot 全屏图的像素一致,左上角为原点)。\n\n\
何时用:目标没有文字(图标、图片)、在游戏或地图里,或 iphone_list_elements 里找不到它时。\
带文字的按钮、列表项优先用 iphone_tap_element,界面布局变了也点得准。按住用 iphone_long_press,滚动用 iphone_swipe。\n\n\
行为:单指按下约 0.05 秒后抬起,点一次(count 大于 1 时在同一点连点)。不检查坐标上是什么,点之前先截图确认位置。\
WDA 不可用时改用 CoreDevice HID(iOS 27 起;结果里注明「经 HID」)。", "Tap a point on the screen (in points, the same as the pixels of a full-screen iphone_screenshot, origin at the top left).\n\nWhen to use: the target has no text (icon, image), is in a game or map, or can't be found with iphone_list_elements. For buttons and list items with text prefer iphone_tap_element, which keeps working when the layout changes. Use iphone_long_press to hold and iphone_swipe to scroll.\n\nBehavior: one finger down for about 0.05 s, then up; a single tap (several at the same point with count above 1). It doesn't check what is at that point, so look at a screenshot first. If WDA is unavailable it uses CoreDevice HID instead (iOS 27+; the result says “via HID”)."),
                ictl_l10n::tr!("已点击 (120,300)", "Tapped (120,300)"),
                true,
            ),
            schema(
                with_then(json!({
                    "x": num(ictl_l10n::tr!("x(点)", "x (points)")),
                    "y": num(ictl_l10n::tr!("y(点)", "y (points)")),
                    "count": {"type": "integer", "default": 1, "minimum": 1, "maximum": MAX_TAP_COUNT,
                        "description": ictl_l10n::tr!("在这一点连点几下,一个手势里点完(每下间隔约 0.1 秒):2 是双击;也用于要在短时间内连点几下的入口。分开调用赶不上这类时间窗", "how many times to tap this point, all in one gesture (about 0.1 s apart): 2 is a double tap; also for entries that need several taps in a short time, which separate calls are too slow for")}
                })),
                &["x", "y"],
            ),
            acting(ictl_l10n::tr!("点击坐标", "Tap point"), true, false),
        ),
        t(
            "iphone_tap_element",
            action_desc(
                ictl_l10n::tr!("按文字点击界面元素:先找标签/名字与 text 完全相同的,找不到再找包含 text 的。\
中心被悬浮栏、键盘等盖住时改点没被盖住的部分,整个被盖住就不点。\n\n\
何时用:按钮、列表项、标签页等带文字的元素,优先用它而不是按坐标点。不确定文字怎么写时先 iphone_list_elements。\
没有文字的图标、游戏、地图用 iphone_tap。\n\n\
行为:没找到、index 超出匹配个数或整个被盖住时不点,返回原因(不算出错;在 iphone_batch 里会让批量停下)。需要 WDA。", "Tap a UI element by its text: first an exact match of label or name with text, otherwise one that contains text. If its center is covered by a floating bar, the keyboard or similar, it taps an uncovered part; if it is completely covered, it doesn't tap.\n\nWhen to use: buttons, list items, tabs and other elements with text; prefer this over coordinates. If unsure how the text is written, use iphone_list_elements first. For icons without text, games and maps use iphone_tap.\n\nBehavior: when nothing matches, index is beyond the number of matches, or the element is completely covered, it doesn't tap and says why (not an error; inside iphone_batch it stops the batch). Needs WDA."),
                ictl_l10n::tr!("已点击 Button \"通用\"", "Tapped Button \"General\""),
                true,
            ),
            schema(
                with_then(json!({
                    "text": {"type": "string", "description": ictl_l10n::tr!("元素的文字(标签或名字),默认先精确匹配再按包含匹配;已给 kind 或 id 时可传空字符串", "the element's text (label or name); exact match first then substring by default; may be empty if kind or id is given")},
                    "kind": {"type": "string", "description": ictl_l10n::tr!("限定控件类型(如 Button、TextField、SearchField、Cell、Switch,不区分大小写);同屏有多个同名元素时用", "restrict to this element type (e.g. Button, TextField, SearchField, Cell, Switch; case-insensitive), useful when several elements share the same text")},
                    "id": {"type": "string", "description": ictl_l10n::tr!("限定无障碍标识(元素列表里 id=\"...\" 的精确值)", "restrict to this exact accessibility identifier (the id=\"...\" shown by iphone_list_elements)")},
                    "match": {"type": "string", "enum": ["auto", "exact", "contains"], "default": "auto",
                        "description": ictl_l10n::tr!("text 的匹配方式:auto=先精确再包含(默认),exact=仅精确匹配标签或名字,contains=仅按包含匹配", "how text is matched: auto = exact first then substring (default), exact = exact label or name only, contains = substring match only")},
                    "index": {"type": "integer", "default": 0,
                        "description": ictl_l10n::tr!("匹配到多个时点第几个(从 0 开始,顺序同 iphone_list_elements)", "which match to tap if there are several (from 0, same order as iphone_list_elements)")},
                    "scroll": {"type": "string", "enum": ["down", "up"],
                        "description": ictl_l10n::tr!("当前屏幕上没有时边滑边找:down 往下找(看下面的内容),up 往上找。每次慢滑约三分之一屏、不带惯性,找到文字完全相同的就点(当前屏只有包含它的不算,接着滑;滑完都没有完全相同的才点包含它的),滑到底(界面不再变)或滑满 max_scrolls 次还没有就停下说明。用于长列表、设置页里不在当前屏的项;不给就只在当前屏幕上找", "If it isn't on the current screen, scroll and look again: down looks further down (the content below), up further up. Each scroll is a slow third of the screen without momentum; it taps as soon as the exact text is found (a row that only contains it doesn't stop the search; if no exact match turns up, one that contains it is tapped), and stops and says so at the end of the list (the screen no longer changes) or after max_scrolls. For items of long lists and settings pages that are off screen; without it only the current screen is searched")},
                    "max_scrolls": {"type": "integer", "default": 8, "minimum": 1, "maximum": ictl_protocol::MAX_SEARCH_SCROLLS,
                        "description": ictl_l10n::tr!("带 scroll 时最多滑几次,默认 8", "with scroll: at most this many scrolls, default 8")}
                })),
                &["text"],
            ),
            acting(ictl_l10n::tr!("按文字点击", "Tap by text"), true, false),
        ),
        t(
            "iphone_long_press",
            action_desc(
                ictl_l10n::tr!("在屏幕坐标(点)上单指按住 seconds 秒再抬起,手指不移动。\n\n\
何时用:要长按才出现的操作,例如主屏图标的快捷菜单、消息或图片的上下文菜单、选中文字。\
普通点击用 iphone_tap 或 iphone_tap_element;要移动手指(滚动、拖滑块)用 iphone_swipe。\n\n\
行为:只按一次。按得太短(不到约 0.5 秒)系统可能当成普通点击。长按弹出的菜单不会自己消失,\
下一步先看屏幕(或直接带 then=screenshot)。需要 WDA。", "Hold one finger still at a screen point (points) for seconds, then lift.\n\nWhen to use: actions that need a long press, such as home-screen quick actions, context menus on messages or images, or selecting text. For normal taps use iphone_tap or iphone_tap_element; to move the finger (scroll, drag a slider) use iphone_swipe.\n\nBehavior: a single press. Too short a press (under about 0.5 s) may count as a normal tap. Menus opened by a long press stay open, so look at the screen next (or pass then=screenshot). Needs WDA."),
                ictl_l10n::tr!("已长按 (200,400) 1 秒", "Long-pressed (200,400) for 1 s"),
                true,
            ),
            schema(
                with_then(json!({
                    "x": num(ictl_l10n::tr!("x(点)", "x (points)")),
                    "y": num(ictl_l10n::tr!("y(点)", "y (points)")),
                    "seconds": {"type": "number", "default": 1.0, "description": ictl_l10n::tr!("按住几秒,默认 1", "seconds to hold, default 1")}
                })),
                &["x", "y"],
            ),
            acting(ictl_l10n::tr!("长按", "Long press"), true, false),
        ),
        t(
            "iphone_swipe",
            action_desc(
                ictl_l10n::tr!("单指从 (from_x, from_y) 按下,用 seconds 秒移到 (to_x, to_y) 后抬起(坐标单位是点)。\n\n\
何时用:滚动列表或页面、左右翻页、拖动滑块。向上滑看下面的内容:from_y 大、to_y 小(例如 y 从 700 滑到 300);\
向左滑看右边的内容:from_x 大、to_x 小。点击用 iphone_tap,原地按住用 iphone_long_press。\n\n\
行为:seconds 越小滑得越快,快滑会带惯性多滚一段;要精确移动(滑块)把 seconds 调到 1 秒左右。\
滑完会等列表停止滚动(最多 2.5 秒)再返回,还在滚就在结果里说明。WDA 不可用时改用 CoreDevice HID(iOS 27 起)。", "One finger down at (from_x, from_y), moving to (to_x, to_y) over seconds, then up (coordinates in points).\n\nWhen to use: scroll a list or page, page left or right, drag a slider. To see content further down, swipe up: from_y large, to_y small (e.g. y from 700 to 300); to see content to the right, swipe left: from_x large, to_x small. Use iphone_tap to tap and iphone_long_press to hold in place.\n\nBehavior: the smaller seconds, the faster the swipe; a fast swipe keeps scrolling with momentum. For precise moves (sliders) use about 1 second. It waits until the list stops scrolling (up to 2.5 s) before returning and says so if it is still moving. If WDA is unavailable it uses CoreDevice HID instead (iOS 27+)."),
                ictl_l10n::tr!("已滑动 (215,700) → (215,300)", "Swiped (215,700) → (215,300)"),
                true,
            ),
            schema(
                with_then(json!({
                    "from_x": num(ictl_l10n::tr!("起点 x(点)", "start x (points)")),
                    "from_y": num(ictl_l10n::tr!("起点 y(点)", "start y (points)")),
                    "to_x": num(ictl_l10n::tr!("终点 x(点)", "end x (points)")),
                    "to_y": num(ictl_l10n::tr!("终点 y(点)", "end y (points)")),
                    "seconds": {"type": "number", "default": 0.3, "description": ictl_l10n::tr!("滑动用时(秒),默认 0.3;越小越快、惯性越大", "swipe duration (seconds), default 0.3; smaller is faster with more momentum")},
                    "hold": {"type": "number", "default": 0, "minimum": 0, "description": ictl_l10n::tr!("先在起点按住几秒再移动,默认 0(普通滑动)。拖动要先长按才拿得起来的东西时用,例如主屏幕图标、可排序的列表项、文件:hold 0.6~1,seconds 0.7 左右。少于 0.5 秒通常拿不起来,只会滑动或翻页", "seconds to hold still at the start before moving, default 0 (a plain swipe). For dragging things that have to be long-pressed before they can be picked up, such as home-screen icons, reorderable list rows or files: hold 0.6-1, seconds about 0.7. Under 0.5 s usually doesn't pick anything up and just swipes or pages")}
                })),
                &["from_x", "from_y", "to_x", "to_y"],
            ),
            acting(ictl_l10n::tr!("滑动", "Swipe"), false, false),
        ),
        t(
            "iphone_type_text",
            action_desc(
                ictl_l10n::tr!("在当前获得焦点的输入框里输入文字(支持中文),默认从光标处输入。replace=true 替换当前聚焦的普通输入框的全文(text 为空时清空),读回全文一致才按回车。submit=true 时末尾按回车。\n\n\
何时用:搜索框、表单、聊天框等。要先点中输入框让键盘弹出;屏幕上没有键盘时报错,什么都不输。\
回车可能会发送消息或提交表单,发送前先向用户确认。\n\n\
行为:输完会读回输入框核对,少了结尾几个字会自动补上;对不上就报错,也不按回车。\
WDA 不可用时改用 HID(iOS 27 起):美式键盘上的英文和符号直接按键;中文等经剪贴板粘贴,手机会弹「允许粘贴」,要截图后点它才粘得进去(剪贴板 45 秒后还原)。", "Type text into the focused text field (any language, including Chinese) at the cursor by default. replace=true replaces the contents of a readable focused field (empty text empties it), and presses Return only once the field reads back as exactly the text. With submit=true it presses Return at the end.\n\nWhen to use: search boxes, forms, chat fields. Tap the field first so the keyboard comes up; with no keyboard on screen it fails and types nothing. Return may send a message or submit a form, so ask the user before sending.\n\nBehavior: it reads the field back afterwards; dropped trailing characters are typed again. If it still doesn't match, it reports an error and doesn't press Return. If WDA is unavailable it uses HID instead (iOS 27+): English letters and symbols of a US keyboard are typed as keys, anything else is pasted through the pasteboard, and the phone asks to “Allow Paste”: take a screenshot and tap it to let the text in (the pasteboard is put back after 45 s)."),
                ictl_l10n::tr!("已输入 4 个字符,并按了回车", "Typed 4 characters and pressed Return"),
                true,
            ),
            schema(
                with_then(json!({
                    "text": {"type": "string", "description": ictl_l10n::tr!("要输入的文字", "the text to type")},
                    "replace": {"type": "boolean", "default": false, "description": ictl_l10n::tr!("替换当前聚焦输入框的全文,text 为空时清空输入框。仅普通可读输入框(不含密码框);经 WDA 时先清空再输入,失败时原来的文字已删掉;默认在光标处输入", "replace all text in the focused field; with empty text, empty it. Readable fields only (not password fields); through WDA the field is emptied first, so after a failure the old text is gone; defaults to typing at the cursor")},
                    "submit": {"type": "boolean", "default": false, "description": ictl_l10n::tr!("输完后按回车(搜索、发送、提交)", "press Return afterwards (search, send, submit)")}
                })),
                &["text"],
            ),
            acting(ictl_l10n::tr!("输入文字", "Type text"), false, false),
        ),
        t(
            "iphone_press_button",
            action_desc(
                ictl_l10n::tr!("按硬件键:home(回主屏幕)、volume_up、volume_down、lock(锁屏)。\n\n\
何时用:离开当前 App 回主屏幕用 home(App 留在后台,要彻底关掉用 iphone_terminate_app);调音量;锁屏。\
锁屏后配置了密码的话,下一个操作会先自动解锁。\n\n\
行为:系统弹窗挡着时按 home 不会生效,这时不按,并提示先点弹窗上的按钮。按 home 会等主屏真的到前台再返回。", "Press a hardware button: home (go to the home screen), volume_up, volume_down, lock (lock the screen).\n\nWhen to use: leave the current app with home (the app stays in the background; use iphone_terminate_app to quit it); change the volume; lock the phone. After locking, if a passcode is configured, the next action unlocks first.\n\nBehavior: while a system alert covers the screen home has no effect, so it isn't pressed and the result says to tap a button on the alert first. home waits until the home screen is really in front before returning."),
                ictl_l10n::tr!("已按 home", "Pressed home"),
                false,
            ),
            schema(
                with_then(json!({"button": {"type": "string", "enum": ["home", "volume_up", "volume_down", "lock"],
                    "description": ictl_l10n::tr!("home、volume_up、volume_down 或 lock", "home, volume_up, volume_down or lock")}})),
                &["button"],
            ),
            acting(ictl_l10n::tr!("按硬件键", "Press button"), false, false),
        ),
        t(
            "iphone_list_apps",
            ictl_l10n::tr!("列出已安装的 App,每行一个:显示名和 Bundle ID。\n\n\
何时用:iphone_launch_app、iphone_terminate_app 报「找不到名为…的 App」时,查准确的名字或 Bundle ID。\
默认只列用户装的 App;设置、相机等系统 App 要 include_system=true(要多花几秒)。结果有缓存。只读。", "List installed apps, one per line: display name and bundle ID.\n\nWhen to use: when iphone_launch_app or iphone_terminate_app report “No app named …”, to look up the exact name or bundle ID. By default only user-installed apps; system apps such as Settings or Camera need include_system=true (takes a few seconds longer). Results are cached. Read-only.")
                .into(),
            schema(
                json!({"include_system": {"type": "boolean", "default": false, "description": ictl_l10n::tr!("同时列出系统 App", "also list system apps")}}),
                &[],
            ),
            looking(ictl_l10n::tr!("列出 App", "List apps")),
        ),
        t(
            "iphone_launch_app",
            action_desc(
                ictl_l10n::tr!("启动 App 并切到前台。app 是显示名时按已安装的 App 查找,找不到会报错,提示用 iphone_list_apps 查看。\n\n\
何时用:打开某个 App。要直接进网页或 App 里的某个页面用 iphone_open_url。\
想从首页重新开始(而不是回到上次的页面),先用 iphone_terminate_app 结束它再启动。\n\n\
行为:App 已在后台运行时只切回前台,停在上次的页面;没在运行就冷启动。等 App 真的到前台才返回(最多约 8 秒);\
被系统弹窗(如权限请求)挡住或启动慢时,结果里会说明。需要 WDA。", "Launch an app and bring it to the foreground. A display name is looked up among the installed apps; if not found it fails and suggests iphone_list_apps.\n\nWhen to use: to open an app. To go straight to a web page or a screen inside an app, use iphone_open_url. To start from the app's first screen (instead of where it was), quit it with iphone_terminate_app first.\n\nBehavior: an app already running in the background is only brought back to the front, on the screen it was on; otherwise it cold-starts. It returns once the app is really in front (up to about 8 s); if a system alert (such as a permission prompt) blocks it or it starts slowly, the result says so. Needs WDA."),
                ictl_l10n::tr!("已启动 com.apple.Preferences", "Launched com.apple.Preferences"),
                true,
            ),
            schema(with_then(app()), &["app"]),
            acting(ictl_l10n::tr!("启动 App", "Launch app"), false, true),
        ),
        t(
            "iphone_terminate_app",
            action_desc(
                ictl_l10n::tr!("强制结束 App 进程,相当于在多任务界面里把它划掉。app 是显示名时按已安装的 App 查找,找不到会报错。\n\n\
何时用:App 卡住或状态不对,或者想让下次 iphone_launch_app 从首页冷启动。只想离开 App 用 iphone_press_button 按 home,App 留在后台。\n\n\
行为:App 里没保存的输入、正在播放的内容会丢失。App 本来没在运行也不报错。结束的是前台 App 时会回到主屏幕。需要 WDA。", "Force-quit an app, like swiping it away in the app switcher. A display name is looked up among the installed apps; if not found it fails.\n\nWhen to use: the app is stuck or in a wrong state, or the next iphone_launch_app should cold-start it on its first screen. To just leave the app, press home with iphone_press_button; the app stays in the background.\n\nBehavior: unsaved input and anything playing is lost. No error if the app wasn't running. Quitting the foreground app returns to the home screen. Needs WDA."),
                ictl_l10n::tr!("已结束 com.apple.Preferences", "Terminated com.apple.Preferences"),
                false,
            ),
            schema(with_then(app()), &["app"]),
            acting(ictl_l10n::tr!("结束 App", "Quit app"), true, true),
        ),
        t(
            "iphone_open_url",
            action_desc(
                ictl_l10n::tr!("用系统默认方式打开网址或 URL Scheme:http/https 网页在 Safari 打开,URL Scheme 打开对应的 App。\n\n\
何时用:直接进入某个网页或 App 里的某个页面,比启动 App 再一级级点快。只想打开 App 用 iphone_launch_app。\n\n\
行为:有的 URL Scheme 系统会先弹窗问要不要打开,带 then=screenshot 确认。需要 WDA。", "Open a web address or URL scheme the way the system does: http/https pages open in the default browser, URL schemes open their app.\n\nWhen to use: to go straight to a web page or a screen inside an app, faster than launching the app and tapping through. To just open an app use iphone_launch_app.\n\nBehavior: for some URL schemes the system first asks whether to open them; check with then=screenshot. Needs WDA."),
                ictl_l10n::tr!("已打开 https://example.com", "Opened https://example.com"),
                true,
            ),
            schema(
                with_then(json!({"url": {"type": "string", "description": ictl_l10n::tr!("完整网址(含 https://)或 URL Scheme", "full address (including https://) or URL scheme")}})),
                &["url"],
            ),
            acting(ictl_l10n::tr!("打开链接", "Open URL"), false, false),
        ),
        t(
            "iphone_wait_for",
            ictl_l10n::tr!("等含指定文字的元素出现或消失。默认等出现；state=disappeared 时等当前界面没有匹配项。超时报错。\n\n\
何时用:操作后要等加载时代替反复截图轮询,每 0.3 秒查一次,不占上下文。输入框里的字不算(刚输入的搜索词不会被当成结果)。\
只读:不改变界面,但配置了锁屏密码时会先解锁。", "Wait for visible UI text to appear (the default), or use state=disappeared to wait for a screen without a match. Returns an error on timeout.\n\nWhen to use: after an action, to wait for loading instead of polling with screenshots; it checks every 0.3 s and uses no context meanwhile. Text inside input fields doesn't count (a search term just typed isn't taken for a result). Read-only: doesn't change the screen, but unlocks the phone first if a passcode is configured.")
                .into(),
            schema(
                json!({
                    "text": {"type": "string", "description": ictl_l10n::tr!("要等的文字,元素的标签/名字/值包含它就算出现;已给 kind 或 id 时可传空字符串", "the text to wait for; it counts as appeared when an element's label/name/value contains it; may be empty if kind or id is given")},
                    "kind": {"type": "string", "description": ictl_l10n::tr!("限定控件类型(如 Button、StaticText、Alert,不区分大小写)", "restrict to this element type (e.g. Button, StaticText, Alert; case-insensitive)")},
                    "id": {"type": "string", "description": ictl_l10n::tr!("限定无障碍标识(精确值)", "restrict to this exact accessibility identifier")},
                    "match": {"type": "string", "enum": ["auto", "exact", "contains"], "default": "auto",
                        "description": ictl_l10n::tr!("text 的匹配方式:auto=先精确再包含(默认),exact=仅精确匹配,contains=仅包含匹配", "how text is matched: auto (default), exact, or contains")},
                    "timeout": {"type": "number", "default": 5, "description": ictl_l10n::tr!("最多等几秒(不超过 30)", "maximum seconds to wait (up to 30)")},
                    "state": {"type": "string", "enum": ["appeared", "disappeared"], "default": "appeared", "description": ictl_l10n::tr!("等文字出现或从当前界面消失", "wait for the text to appear or disappear from the visible screen")}
                }),
                &["text"],
            ),
            looking(ictl_l10n::tr!("等文字出现", "Wait for text")),
        ),
        t(
            "iphone_list_devices",
            ictl_l10n::tr!("列出 USB 上接着的全部 iPhone/iPad,每行一台:设备名、型号、iOS 版本、UDID,标出正在控制哪台、有没有设自动解锁密码。\n\n\
何时用:接着多台设备、要确认操作落在哪台,或切换前查设备名和 UDID。同一时间只控制一台,其他工具都作用在正在控制的那台上。只读。", "List all iPhones/iPads connected over USB, one per line: device name, model, iOS version, UDID; marks the one being controlled and whether it has an auto-unlock passcode.\n\nWhen to use: with several devices connected, to check which one actions go to, or to look up names and UDIDs before switching. Only one device is controlled at a time; all other tools act on it. Read-only.")
                .into(),
            schema(json!({}), &[]),
            looking(ictl_l10n::tr!("列出设备", "List devices")),
        ),
        t(
            "iphone_select_device",
            ictl_l10n::tr!("改为控制另一台设备,之后所有工具都作用在它上面,守护进程重启后还记得。device 是 UDID 或设备名(设备名包含这段文字且只有一台也行),\
auto 表示取消选定、改回自动选(正在控制的那台断开才换)。\n\n\
何时用:接着多台设备、要操作的不是正在控制的那台。先用 iphone_list_devices 看有哪几台。\n\n\
行为:切换后要在新设备上拉起 WDA(10~60 秒),期间读界面、按名字点击等用不了,先用 iphone_status 确认就绪;\
这台从没装过 WDA 时起不来,要请用户运行 iphone-control setup。\
自动解锁按设备分开存密码,新设备没设就不会自动解锁。设了 IPHONE_UDID 时不能切换。\n\n\
返回:一行结果文字,例如「已切换到 Derrors 的 iPad(iPad11,1 iOS 26.6.1,00008020-…)」。", "Control another device from now on: all tools act on it, and the daemon remembers it across restarts. device is a UDID or device name (part of the name works if only one device matches); auto clears the selection and goes back to automatic (switches only when the controlled device disconnects).\n\nWhen to use: with several devices connected, when the one to operate isn't the one being controlled. Look at them with iphone_list_devices first.\n\nBehavior: after switching, WDA has to start on the new device (10-60 s); until then reading the screen, tapping by text etc. don't work, so check readiness with iphone_status. If WDA was never installed on that device it won't start; ask the user to run iphone-control setup. Auto-unlock passcodes are stored per device, so a new device without one isn't auto-unlocked. Not possible while IPHONE_UDID is set.\n\nReturns: one line of result text, e.g. “Switched to Derrors's iPad (iPad11,1 iOS 26.6.1, 00008020-…)”.")
                .into(),
            schema(
                json!({"device": {"type": "string", "description": ictl_l10n::tr!("UDID、设备名,或 auto(改回自动选)", "UDID, device name, or auto (back to automatic)")}}),
                &["device"],
            ),
            ToolAnnotations::with_title(ictl_l10n::tr!("切换设备", "Switch device")).read_only(false).destructive(false).idempotent(true).open_world(false),
        ),
        t(
            "iphone_batch",
            action_desc(
                ictl_l10n::tr!("按顺序执行几步操作,一次调用完成(最多 20 步),某一步出错就停下,说明停在第几步、后面几步没执行。\n\n\
steps 每项是 {\"tool\": 工具名, 其余是该工具的参数},可用:iphone_tap、iphone_tap_element、iphone_long_press、\
iphone_swipe、iphone_type_text、iphone_press_button、iphone_launch_app、iphone_terminate_app、iphone_open_url、\
iphone_wait_for,以及 {\"tool\": \"wait\", \"seconds\": 秒数}(不超过 5)。iphone_tap_element 没点到也会停下。\
执行前先检查全部参数,有错一步都不执行。\n\n\
何时用:结果确定的连续操作(按键、逐级点菜单、填表),每步省一轮往返。每步的结果不确定(点了以后不知道会跳到哪)时,还是一步步来。\
then 在全部做完(或停下)后返回一次界面。", "Run several steps in order in one call (up to 20); stops at the first error and says which step it stopped at and how many didn't run.\n\nEach item of steps is {\"tool\": tool name, plus that tool's parameters}; allowed: iphone_tap, iphone_tap_element, iphone_long_press, iphone_swipe, iphone_type_text, iphone_press_button, iphone_launch_app, iphone_terminate_app, iphone_open_url, iphone_wait_for, and {\"tool\": \"wait\", \"seconds\": n} (up to 5). An iphone_tap_element that taps nothing also stops it. All parameters are checked first; if any is wrong, no step runs.\n\nWhen to use: sequences with predictable results (keys, menu levels, forms), saving a round trip per step. When the outcome of a step is uncertain (you don't know where a tap leads), go step by step. then returns the screen once, after the last step (or where it stopped)."),
                ictl_l10n::tr!("1. iphone_launch_app → 已启动 com.apple.Preferences(每步一行)", "1. iphone_launch_app → Launched com.apple.Preferences (one line per step)"),
                true,
            ),
            schema(
                with_then(json!({
                    "steps": {"type": "array", "items": {"type": "object"},
                        "description": ictl_l10n::tr!("要执行的步骤,1 到 20 个,例如 [{\"tool\": \"iphone_tap_element\", \"text\": \"通用\"}]", "the steps to run, 1 to 20, e.g. [{\"tool\": \"iphone_tap_element\", \"text\": \"General\"}]")},
                    "reuse_elements": {"type": "boolean", "default": false,
                        "description": ictl_l10n::tr!("这批操作不改变界面布局时(例如在同一个键盘上连按)设为 true:按名字点击复用最近读到的元素树,每步省 0.3~0.8 秒。会跳转页面的不要用,否则可能点到旧位置", "Set to true when the steps don't change the layout (e.g. pressing keys on the same keypad): tapping by text reuses the most recent element tree, saving 0.3-0.8 s per step. Don't use it for steps that navigate, or it may tap where things used to be")},
                    "one_gesture": {"type": "boolean", "default": false,
                        "description": ictl_l10n::tr!("步骤全是 iphone_tap 时可设为 true:这些点在一个手势里依次点完(每个点一根手指,间隔约 0.04 秒),中间不等界面空闲。用于在同一个键盘上连按,例如输入框不接受 iphone_type_text 时逐个点数字键:7 个键约 1.9 秒,分开点约 3.6 秒。点的过程中不看界面,中途跳了页面后面的点也照样点下去;点完截图或读元素核对", "Only when every step is iphone_tap: set to true to tap the points one after another in a single gesture (one finger per point, about 0.04 s apart) without waiting for the interface in between. For pressing keys on the same keyboard, e.g. digit keys one by one when a field doesn't take iphone_type_text: 7 keys in about 1.9 s instead of about 3.6 s. Nothing is checked while it taps, so if a tap navigates, the remaining points still land; check the result with a screenshot or elements afterwards")}
                })),
                &["steps"],
            ),
            acting(ictl_l10n::tr!("批量操作", "Batch"), true, false),
        ),
    ]
}
