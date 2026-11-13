# 必须覆盖的回归用例

#1～#34 来自 toolkit 和本项目早期实际踩过的坑，#35～#45 是多设备和 HID 横屏，#46～#58 是实时画面、开发者工具和英文界面，#59～#63 是 iPhone 18 Pro Max（iOS 27.2.1）真机全量测试发现的问题，#64 起是 viewer 的 AI 任务和新页面。能单测的写单测，不能单测的进真机回归清单。**每个 PR 都必须保持这里的全部用例通过**：单测的由 `cargo test` 覆盖；改到的范围里测试方式是真机、无头 Chrome 或手动测量的，要重跑并在 PR 里列出编号，写明设备和 iOS 版本；修了清单里没有的问题，要加一行。完整要求见 [CONTRIBUTING.md 的 Required tests](../../CONTRIBUTING.md#required-tests)。

| # | 坑 | 正确行为 | 测试方式 |
|---|---|---|---|
| 1 | 锁屏密码键盘不显示时，`Key` 元素仍留在元素树里（`visible=false`） | 不以"树里有键盘"判断键盘已出现；固定先唤醒再上滑 | 单测（脱敏 XML 样本）+ 真机 |
| 2 | `displayed` 接口在锁屏上返回 false，即使键盘可见 | 以密码栏「已输入 N 位」为准 | 真机 |
| 3 | 锁屏界面 Face ID 动画使 WDA 每请求等 idle 至 10 秒，键盘随后自动收起 | 解锁期间 `waitForIdleTimeout=1`、`animationCoolOffTimeout=0.2`，结束恢复；0 会导致唤醒失败 | 真机 |
| 4 | 多位密码合成一个 W3C 手势会漏键 | 每位单独一个手势并逐位核对 | 真机 |
| 5 | 锁屏上 `/wda/tap` 每次约 2.9 秒 | 解锁用 W3C 单手势（约 0.7 秒） | 真机计时 |
| 6 | 同名「设置」有 5 个 `SBAppTags` 含 `hidden` 的后台服务 | 按名字解析 App 时跳过 hidden | 单测 |
| 7 | 容器无 udev，手机重启/重插后 usbmuxd 看不到设备 | 不再适用：2026-11-10 起不发布 Docker，守护进程不再自管 usbmuxd，热插拔由系统的 usbmuxd 和 udev 处理（D5、D14） | — |
| 8 | 手机重启后 XCTest 弹「Enable UI Automation」密码框 | 识别错误、只推送一次、60 秒重试、恢复后再推送 | 单测（错误识别）+ 真机 |
| 9 | plumesign 不签 `.xctest`，WDA 报 XCTest 103 | `.xctest` 签名、无 entitlements | 单测（签完检查 `_CodeSignature`）+ 真机 |
| 10 | `ios forward` 监听所有网卡，WDA 无认证，局域网可直接控制手机 | 默认不监听任何端口；bridge 只绑 127.0.0.1 | 集成测试（检查监听地址） |
| 11 | 依赖大版本漂移（mcp 2.x 把 FastMCP 改名、idevice 0.1.x 每版破坏性变更） | 所有依赖锁定精确版本 + `Cargo.lock` 入库；升级单独提交 | CI |
| 12 | host 侧 runner 退出后手机上残留 WDA 仍响应几十秒 | 就绪 = 本进程 XCTest 会话存活 且 `/status` ready | 真机 |
| 13 | App 冷启动后立即点击会被吞 | `launch_app` 等到前台再停 1 秒 | 真机 |
| 14 | 完整元素树 7～8 秒 | `excluded_attributes` 排除可见性等属性（约 0.3 秒），可见性按坐标判断 | 单测 + 计时 |
| 15 | 铺满全屏的 Application/Window/Toolbar 等元素没有定位价值 | 列表中过滤 | 单测 |
| 16 | 区域放大倍数只保留 2 位有效数字导致边缘约 1.4 点误差 | 保留 3 位 | 单测 |
| 17 | 只有 ToolError 的消息能透传给 MCP 客户端（Python SDK） | 所有预期错误都转成工具错误文本 | 单测 |
| 18 | 锁屏（未首次解锁）时隧道报 `PasswordProtected` | 进入 NeedsUnlock，提示人工 | 真机 |
| 19 | Apple ID 会话过期只在重签时才发现 | 每日独立验证会话 | 单测（模拟过期） |
| 20 | WDA 在锁屏时启动，`/wda/locked` 一直报 false（只靠变化通知维护，初值 false） | 本次 WDA 启动后确认过一次未锁屏之前，用元素树核对（锁图标 `proud-lock` / `passcode-view`） | 单测（iOS 27 真机样本）+ 真机 |
| 21 | 下拉通知中心时 `/wda/locked` 报 true | WDA 说锁了时核对元素树：只有 `lockscreen-date-view`、没有锁图标的是通知中心 | 单测（真机样本）+ 真机 |
| 22 | iOS 27 上锁屏密码键盘有残留输入时，按 Home 不清空（键隐藏但仍可按） | 用右下角删除键逐位删到 0（按位置认，不依赖语言），每删一位核对 | 单测（真机样本 + 假锁屏）+ 真机 |
| 23 | 熄屏锁屏时第一个无障碍查询等约 8 秒；锁屏 0.5 秒内唤醒，第一下上滑卡约 16 秒后报 `point.x != INFINITY` | 能自动解锁时先按 Home 再核对；本进程锁屏后 3 秒内不解锁；唤醒/上滑报错算这轮落空重来 | 单测 + 真机计时 |
| 24 | HID 会话的屏幕媒体流没正常停止时，手机端一直挂着：灵动岛「配对的电脑」图标常驻、不自动锁屏，进程退出和拔插都清不掉 | HID 空闲 30 秒关闭；所有丢弃路径（含退出）先停流；WDA 锁屏报错时确认已锁就不退到 HID | 真机 |
| 25 | 自动解锁第 2 位之后出错（点击报错、核对锁屏报错、进程被杀）会跳过写停用标记，下次可能把同一个错误密码再输一遍 | 输第 2 位之前先写停用标记，只有确认解开或确认没凑满整串才撤掉；出错返回 `UnlockError::Uncertain` 并推送通知 | 单测（`ictl-unlock`：`error_after_second_digit_keeps_auto_unlock_disabled`、`double_press_that_fills_the_entry_disables`、`dropped_last_digit_is_not_a_wrong_attempt`） |
| 26 | 最后一位输完后用 WDA 的锁屏状态确认结果：WDA 在锁屏时启动会报 false，把输错当成解开 | 确认时用严格判断 `WdaClient::check_lock(_, strict = true)`：核对出错一律算仍锁着 | 单测（`stale_wda_lock_state_cannot_turn_a_wrong_entry_into_unlocked`） |
| 27 | HID 会话在屏幕媒体流起来之后才失败，媒体流没停（症状同 #24） | `HidSession::open`、`media::start` 出错时先停流；HID 会话归 `Tunnel` 所有，关隧道必先停流 | 真机 |
| 28 | 一个长操作（启动 WDA 等自动化密码最长 120 秒）挡住所有工具、`status` 和 SIGTERM，排队的点击几分钟后才执行 | 启动 WDA、重签、会话检查不持设备锁；`status` 读快照；改变手机状态的请求排队超过 30 秒就放弃；停止信号优先处理（architecture §2 并发模型） | 真机 |
| 29 | WDA 请求超时或中途断开后退到 HID，同一个动作做了两遍 | 传输层区分连不上（`WdaError::Unavailable`，可以退到 HID）和请求已发出之后的超时、断开、响应不完整（不退，报错让 agent 先截图确认） | 单测 + 代码审查 |
| 30 | 设置文件里留空的 `ICTL_LOG=` 生成一个什么都不放行的过滤器，日志全没了 | 空值当作没设 | 真机（看守护进程的输出） |
| 31 | `/wda/tap` 每次先等界面空闲：主屏点图标约 1.7 秒（调小等待设置也没用）；W3C actions 手势做完又要等动画冷却（默认 2 秒），App 里切标签约 1.9 秒 | 点击用单个 actions 手势，期间临时把 waitForIdleTimeout/animationCoolOffTimeout 调成 1/0.2，做完恢复（原值只读一次）。真机：主屏点图标 1.70→0.91 秒（到前台 1.80→1.01 秒），App 里 0.62→0.72 秒 | 单测（请求顺序、恢复原值）+ 真机计时 |
| 32 | `/wda/keys` 没有输入框获得焦点时也返回成功，什么都没输（真机：Safari 地址栏没点中、主屏上直接输入） | 输入前查屏幕上有没有软键盘（`XCUIElementTypeKeyboard`，约 0.2 秒），没有就报错让先点输入框 | 真机 |
| 33 | 系统弹窗（定位、通知权限）挡着时 Home 不生效、App 到不了前台，WDA 照样报成功；弹窗属于 SpringBoard，看前台 App 发现不了 | 按 Home 前查 `/alert/text`（App 静止时约 0.4 秒；按下后动画中查要好几秒）；有弹窗就说明并不按。launch_app 没到前台时同样带上弹窗文字。按 Home 后等主屏到前台再返回 | 真机计时；弹窗路径待弹窗再出现时复测 |
| 34 | `/wda/touchAndHold` 前后都等界面空闲：主屏长按图标 1.2 秒要约 12 秒 | 长按和点击一样用单个 actions 手势、临时调小等待：约 2 秒 | 真机计时 |
| 35 | 接着多台时，`devices` / `iphone_list_devices` 漏设备或标错正在控制的那台 | 列出 USB 上每一台的名字、型号、iOS 版本、UDID，标出正在控制哪台、有没有设自动解锁密码 | 真机（两台） |
| 36 | 按 usbmuxd 的列表顺序选设备，列表顺序一变就换了台 | 选定了只认那一台，没接就是离线、不换；没选定时正在控制的那台一直用到它断开 | 单测（`device_pick_is_pinned_or_sticky`）+ 真机（拔掉选定的那台） |
| 37 | `select` 按名字选到了别的设备 | 先比 UDID，再比设备名，最后是名字包含这段文字且只有一台；对上多台就报错，让用 UDID 指定 | 单测（`device_is_found_by_udid_then_name`） |
| 38 | 选定的设备重启守护进程后丢了；设了 `IPHONE_UDID` 还能切走 | 选定的写进 state.json；`IPHONE_UDID` 优先，设了就拒绝切到别的设备 | 真机（`select` 后重启；设 `IPHONE_UDID` 后 `select` 另一台） |
| 39 | 切换后操作还落在上一台，或用了上一台的 App 列表、方向、屏幕尺寸 | 切换时拆掉上一台的会话，清掉 App 列表和方向；屏幕尺寸记录是别的设备的就不用。切换后等 `iphone_status` 就绪，截图、`iphone_list_apps`、点击都要作用在新设备上 | 真机（iPhone + iPad，各做一遍 `iphone_select_device` → 截图 → 点击） |
| 40 | 一台的密码输进另一台 | 密码按设备存（`secrets/passcodes/<UDID>`）；旧的 `secrets/passcode` 只在 USB 上只接着一台时归到那台，接着多台时不用 | 单测（`passcode_file_is_per_device_and_stays_in_its_dir`）+ 真机 |
| 41 | 已接着一台时再插一台，没有 udev 的 usbmuxd 认不到第二台 | 不再适用：同 #7，守护进程不再自管 usbmuxd | — |
| 42 | 自动选设备时 `iphone_batch` 执行到一半换了设备（拔线后换到另一台），剩下的步骤落到另一台上 | 控制的设备变了就停下报错 | 真机（batch 里放 `wait`，中途拔线） |
| 43 | Apple 键盘等配件被当成手机：`doctor` 误报 USB 上有设备（以前自管 usbmuxd 时还会被反复重启） | 只算 iPhone/iPad/iPod（厂商号 05ac 且产品名匹配） | 单测 + 真机 |
| 44 | 横屏时 WDA 掉线，HID 触摸按竖屏坐标换算，点错位置 | 每次 HID 触摸前重新截图核对横竖；横屏要有 30 秒内且匹配的 WDA 旋转角才发送，否则拒绝，不猜方向；截图和按键不受影响 | 单测（`hid_rotation_requires_fresh_matching_landscape`）+ 真机（iOS 27 横屏下让 WDA 掉线，点击落点正确；守护进程重启后横屏触摸被拒绝） |
| 45 | WDA 在横屏时就绪，记下的是横屏尺寸，之后 HID 换算错 | state.json 只存竖放尺寸和对应的 UDID，换算时再按当前方向转 | 真机（iOS 27，横屏下启动 WDA，再让它掉线后点击） |
| 46 | 浏览器里的其他网页向 `viewer` 发请求，借它操作手机 | 操作要求 `Origin` 是页面本身、内容类型是 JSON；`Host` 必须是回环地址 | 单测（`actions_need_own_origin_and_json`） |
| 47 | `viewer` 开着时 `select` 到别的设备，点击落到用户没在看的手机上 | 每个操作带启动时的 UDID，守护进程控制的不是这台就拒绝 | 单测（`request_for_another_device_is_refused`）+ 真机（`viewer` 开着时 `select` 另一台，再点击） |
| 48 | 不需要开发者工具的 agent 多读工具说明，或没开时也能调用 | 开关关闭时工具列表还是 17 个，调用这些工具报错并说明要设 `IPHONE_CONTROL_DEV_TOOLS=1` | 单测（工具列表：`toolkit_tools_plus_wait_for_batch_and_devices`）；调用被拒：代码审查（`call_tool` 里的判断，还没有单测） |
| 49 | os_log 开着 Debug 时手机上每个进程都产生 Debug 消息（约 650 行/秒），手势变慢、失败 | 只要默认、Error、Fault 级别（约 80 行/秒） | 真机（`iphone_app_logs` 采集时跑回归脚本默认步骤，和不采集时对比耗时） |
| 50 | 日志采集的缓冲把内存撑大（全部进程，每秒几十行） | 行数和总量都封顶（20,000 行、8 MB），单行截断，结果说明最早一行是多久前 | 单测（`buffer_is_bounded_by_size`） |
| 51 | 每次调用重开 Web Inspector 连接：约 8 秒一次，几轮后手机不再报告 Safari，要重启 Safari | 复用一条连接；只有连接层的错误才重开，页面超时或页面报错不断开 | 单测（`only_a_page_timeout_keeps_the_connection`）+ 真机（连续多次 `iphone_web_targets`、`iphone_web_eval` 都快且找得到页面；重启 Safari 后仍能找到） |
| 52 | `iphone_web_eval` 失败后换新连接重试，脚本执行两遍 | 只有列页面（不改手机状态）时才换连接重试，执行脚本不重试 | 代码审查（`web_eval`） |
| 53 | 卸载 App 按模糊匹配，删错 App 连同数据；卸载 WDA 后无法再控制手机 | 只接受 Bundle ID 或完整显示名（不分大小写），空的、部分匹配、同名多个都拒绝并列出候选；拒绝卸载 WDA | 单测（`uninstall_needs_an_exact_name`、`wda_runner_cannot_be_uninstalled`） |
| 54 | 滚动条的标签随系统语言变化，只认中英文时其他语言的滚动条出现在元素列表里 | 按百分比的值和细长的形状认，不看标签 | 单测（`scroll_bars_are_dropped_in_any_language`） |
| 55 | 升级时新旧版本混用：旧客户端按中文开头认结果，旧守护进程不带 `outcome` | 不带语言的请求照旧回中文；`outcome` 可选，没有时按中文开头认 | 单测（`outcome_is_optional_on_the_wire`） |
| 56 | 安装和重签同时进行：idevice 把每个 .ipa 上传到手机上同一个文件（`PublicStaging/idevice.ipa`），互相覆盖 | 安装（`iphone_install_app`、重签、`sign`）互相排队 | 真机（安装和重签同时进行，两个都成功） |
| 57 | iPhone 横屏时 viewer 画面还是竖的、内容是歪的，点击也错位 | iPhone 上 WDA 每个会话开 `mjpegFixOrientation`，帧随界面转正 | 真机：Safari 转横屏，`/stream` 的帧是 2796×1290 且内容正立 |
| 58 | 持设备锁的调用不返回：之后的请求全部排队到过期，进程却还活着，服务管理器不会重启 | 完成巡检或请求时记心跳，约 10 分钟没有心跳就退出（`watchdog.rs`），巡检因为锁被占而跳过时不算 | 单测 a_beat_ends_the_silence |
| 59 | iPhone 18 Pro Max（iOS 27.2.1）上按住 0.5 秒的 IOHID 电源键没反应，每次锁屏都退到 WDA 的锁屏：约 10 秒，日志里还记一次超时 | 电源键按 0.1 秒（0.05～0.15 秒都约 0.7 秒锁屏） | 单测（`hid_buttons_send_consumer_page_events`）+ 真机计时（锁屏约 0.9 秒） |
| 60 | IOHID 的 AC Home 在 iPhone 18 Pro Max（iOS 27.2.1）上基本不起作用，按住多久都一样；XCTest 的 home 在 iPhone 12 mini 上不起作用，正相反。只按固定顺序的话，前者每次 Home 都先白等 2 秒 | 按设备记住上次让主屏回到前台的按法，下次先用它；不起作用再换另一种，换成功了就改记 | 真机计时（iPhone 18 Pro Max：第一次约 3.7 秒，之后约 1.3 秒）；iPhone 12 mini 待复测 |
| 61 | 点击、长按用 0.2 秒的动画冷却：界面上一直有动画时（键盘弹着）WDA 要约 26 秒才返回（同 `NO_COOL_OFF` 注释里滑动的情况） | 点击、长按和滑动一样用冷却 0（`IdleSettings::NO_COOL_OFF`）；解锁仍用 `QUICK` | 单测（`tap_is_one_gesture_with_quick_waits_then_restores_baseline`）+ 真机计时（设置里搜索后点结果：26 秒 → 1.4 秒；主屏点图标、App 里切标签不变慢） |
| 62 | 网页检查器没开时手机直接断开 Web Inspector 连接，工具只报一句 TLS 错误 | 连接被手机断开时提示打开 设置 → Safari → 高级 → 网页检查器 | 单测（`a_closed_connection_points_at_the_web_inspector_setting`）+ 真机（开关关着时调 `iphone_web_targets`） |
| 63 | 不带调试消息的日志流里仍有 Info 级别的条目，和 `iphone_app_logs` 的说明不符 | 采集时丢掉 Info、Debug | 单测（`info_and_debug_entries_are_not_kept`）+ 真机 |
| 64 | Codex 对标了 destructiveHint 的 MCP 工具默认要审批，`codex exec` 非交互运行时直接拒绝（「requires approval, but approval policy is never」），点击、滑动全都做不了 | 给 iphone 这个 MCP 服务设 `default_tools_approval_mode="approve"`，由 `iphone_confirm` 把关 | 真机（Codex 任务里点击、按名字点都执行了） |
| 65 | viewer 已经停了、任务已经停了，agent 的 MCP 服务还在继续操作手机 | 连不上守护进程或任务已结束一律拒绝，并告诉 agent 停下；开任务的程序断开，任务就结束（abandoned） | 单测（`without_the_daemon_a_task_call_is_refused`、`an_ended_or_unknown_token_is_refused_for_everything`、`an_opened_task_holds_the_phone_until_its_lease_goes`）+ 真机（任务中途停止，之后没有新步骤，agent 进程组已结束） |
| 66 | 暂停只挡住了界面上的按钮，agent 照样继续点 | 暂停、接管、等确认时，任务的请求在守护进程里挂住（包括一个工具调用中间的请求，如 `iphone_batch` 的下一步），恢复后才放行 | 单测（`a_paused_task_holds_its_requests_until_resumed`、`a_held_request_whose_caller_left_is_dropped`）+ 真机（暂停 8 秒没有新步骤） |
| 67 | 用户接管时动过手机，交还后 agent 按旧的界面接着点 | 交还后、或别人动过手机后，任务的操作一直被拒，直到它截图或读过元素 | 单测（`after_someone_else_acted_the_agent_looks_before_acting`）+ 真机（交还后先截图） |
| 68 | 用户拒绝确认后 agent 正常结束，页面显示「已完成」 | 有被拒绝的一步就按「已停止」结束 | 单测（`a_rejected_confirmation_tells_the_agent_not_to_do_it`）+ 真机（拒绝删除世界时钟城市，没有删） |
| 69 | 按结果文字里的 `@(x,y)` 给所有步骤画点击标记，读元素的一步也被标成点击 | 只给 `iphone_tap_element` 的结果解析点击位置 | 单测（`only_a_tap_by_text_gets_a_mark_from_its_result`） |
| 70 | 其他网站用 `<img src="http://127.0.0.1:8200/api/elements">` 触发读元素，顺带自动解锁手机 | 浏览器标明 `Sec-Fetch-Site` 不是 same-origin / none 的读请求一律 403 | 单测（`reads_from_other_sites_are_refused`）+ curl |
| 71 | AI 任务执行中，用户在页面上点击和 agent 同时操作 | 没有接管时 viewer 拒绝手动操作（409 并说明），页面也锁住 | 真机（任务中 `/api/tap` 被拒，接管后按 Home 生效） |
| 72 | 设计稿在 1024×768 下横向溢出 26px：中间栏各行按内容最小宽度把栏撑出窗口 | 中间栏各行 `min-width: 0`；两栏区间「设备面板」只留图标；状态标签先让出空间 | Playwright：360×800 到 1920×1080 共 11 个视口横向溢出都是 0 |
| 73 | viewer 的手动操作走 WDA：点击约 530 毫秒，滑动要等列表停下（1.8～3 秒），松开鼠标后才滑，画面不跟手 | iOS 27 起经 HID 实时发按下、移动、松开（`Request::Touch`），移动只发最新位置；滚轮换成手指拖动 | 单测（`page_actions_map_to_daemon_requests`）+ 真机（iPhone 18 Pro Max / iOS 27.2.1：移动约 2 毫秒，点击约 20 毫秒；拖动、滚轮滚动设置列表，点击进入「通用」，按住 1 秒出主屏菜单） |
| 74 | 每次 HID 触摸前截图核对方向（几百毫秒），实时移动跟不上 | 只在 `down` 时核对一次（有 WDA 时读它的旋转角），移动、抬起直接用已开的会话 | 真机计时 |
| 75 | WDA 的 MJPEG 默认每秒 10 帧，画面比操作慢 | 每秒 60 帧、75% 尺寸（`fast_stream`）；只在有客户端时采集 | 真机：约 48 fps、34 KB/帧，读元素树不变慢 |
| 76 | viewer 页面只有中文 | 页面文字中英两套（`tr()`），默认跟着 `IPHONE_CONTROL_LANG`，可切换；请求带 `X-Ictl-Lang`，守护进程的提示和 AI 任务用同一种语言 | 单测（`the_page_says_its_default_language`）+ Playwright（英文页面除切换按钮和手机上的数据外没有中文；360×800 到 1920×1080 中英文都没有横向溢出）+ 真机（英文 AI 任务） |
| 77 | 控制台画面每秒 4～10 MB，经隧道在外网看会卡 | 装了 ffmpeg 时改用 H.264（`/video`），显卡编码优先，约为 MJPEG 的 1/15 | 真机：设置里来回滚动，`/video` 约 250～430 KB/s，静止时为零 |
| 78 | ffmpeg 扣住最新一帧，画面静止时最后一次变化一直出不来 | 每帧后面紧跟 K 份副本，K 在探测时测出 | 单测 encodes_with_the_local_ffmpeg；真机：每帧延迟 16 毫秒 |
| 79 | 裸 H.264 流要等下一帧开头才知道这一帧结束 | ffmpeg 输出 FLV，每帧带长度 | 单测 tags_come_out_whole_however_the_bytes_arrive |
| 80 | 链路慢时系统和 SSH 隧道的缓冲吞下几 MB，画面越来越滞后（每秒读 30 KB 落后 40 秒） | 页面每 100 毫秒回报收到多少；落后时新画面先不编码，追上后只编最新的 | 单测 behind_means_late_beyond_the_usual_round_trip、no_more_on_the_way_than_the_link_carries_in_the_slack；手动 slow_page_stays_close_to_live；真机：每秒 200 KB 时滚动停下后约 0.6 秒追上 |
| 81 | 一次回报丢了，服务端一直等，画面卡住 | 页面收到确认才算报过；服务端 3 秒没收到回报就不再等 | 单测 behind_means_late_beyond_the_usual_round_trip |
| 82 | 转屏后帧的尺寸变了 | 尺寸一变就开新的编码会话，页面按新的解码配置重新配置 | 真机：Safari 横竖屏切换，解码配置发 3 次，ffmpeg 解码无错 |
| 83 | 浏览器没有 WebCodecs 或解不了 H.264 | 这一页从没出过画面就改用 MJPEG | 无头 Chrome：删掉 VideoDecoder 后画面是 MJPEG |
| 84 | MJPEG 去重后画面静止时不显示（Chrome 要看到下一部分的头部才显示上一帧） | 每帧后面紧跟下一部分的分隔线和头部，不带 Content-Length | 无头 Chrome：静止画面下 MJPEG 有图 |
| 85 | macOS 上要装 ffmpeg 才有 H.264 | 本程序直接调 VideoToolbox：JPEG 由它的硬件解码器解成 NV12，直接交给 H.264 编码器；ffmpeg 的 VideoToolbox 作备用 | 单测 encodes_natively_without_holding_frames、encodes_with_the_local_ffmpeg（没装 ffmpeg 也选中原生的）；真机（Apple 芯片）：从送进到出帧 3.5～6 毫秒；手机真实画面（990×2151）中位 17 毫秒，qp 20 时流量为 MJPEG 的 11.4%，SSIM 中位 0.9976（手动测试 measure_recording） |
| 86 | VideoToolbox 把奇数尺寸的最后一列、一行裁掉（481×1041 编成 480×1040） | JPEG 解码时就拉成偶数尺寸（482×1042），和 ffmpeg 的 `scale` 一样 | 单测 odd_sizes_grow_to_even；无头 Chrome 用 WebCodecs 解出 482×1042 |
| 87 | 比 macOS 11 新的 VideoToolbox 键直接链接的话，旧系统上程序起不来 | `MinAllowedFrameQP` 等运行时用 `dlsym` 找，没有就按 `Quality` 近似 qp | 代码审查 |
| 88 | 页面画面滞后 4 帧，手势结束后停在动作中间那一帧（Chrome 的硬件解码器扣着帧等后面的帧来挤） | VideoToolbox 的 SPS 没有 VUI，解码器按 Level 4.2 的缓冲最坏情况扣 4 帧；发给页面前改写 avcC 里的 SPS，写明 `max_num_reorder_frames=0`（`video/sps.rs`），已经写明的不动 | 单测 videotoolbox_sps_gets_no_reordering、vui_without_restriction_keeps_its_fields；无头 Chrome 逐帧喂：硬件解码器从扣 4 帧变为 0 帧 |
| 89 | 控制台开着时 WDA 每隔几分钟被 iOS 杀掉（`cpu_resource_fatal`：一分钟平均 84%，限额 80%），守护进程跟着记「WDA 不可用」 | iOS 27 及以上改用手机自己的视频（`ScreenStream`），WDA 不再截图 | 真机：页面看了 70 秒，WDA 的 CPU 平均 3%、最高 11%（原来 110～120%），没有再重启 |
| 90 | 新页面一直黑屏：刚开流时要的关键帧手机不理，流自带的第一个关键帧在订阅前就过去了 | 没收到关键帧时每 250 毫秒再要一次 | 真机：刚开流 1.6～1.8 秒出第一帧 |
| 91 | 手机按请求补发的关键帧不带参数集，新页面从它开始解不了 | 组帧时记下 VPS/SPS/PPS，补到这样的关键帧前面 | 单测 keyframes_get_the_parameter_sets |
| 92 | 横屏时页面上的画面上下颠倒（转正方向写反） | 按「竖放顶边顺时针转过的角度」同向转回来；方向每秒问一次 | 真机：向左、向右横屏的画面和手机截图一致，1.1～2.7 秒跟上；横屏点地址栏点中 |
| 93 | 在本机运行 `iphone-control update` 只出现子命令帮助，还要再查该用哪种升级命令 | 先检查当前版本。已是最新则提示并结束，有可升级版本时按核实的安装渠道处理，固定版本仍需显式解除 | 单测（`user_update_checks_the_release_before_choosing_an_installer`） |
| 94 | Debian 升级只对照同一发布页上的 `.sha256` 就用 sudo 以 root 安装：能替换发布附件的人可以同时换掉包和校验值 | 发布流程给每个 .deb 签名清单（`release.py sign-deb`）；没有签名清单不提供升级，下载后大小和 SHA-256 要和签名清单一致才交给 APT | 单测 deb_manifest_is_bound_to_its_package、deb_upgrade_waits_for_its_signed_manifest；`packaging/test_installer.py` test_debian_package_is_signed_for_its_own_name |
| 95 | `iphone-control update` 不问就开始安装，受管服务会重启，打断正在执行的任务 | 先说明要做什么并询问，没有终端时要 `--yes` | 单测 update_accepts_no_subcommand_and_preserves_read_only_check；手动：终端里运行，回答 n 不升级 |
| 96 | Homebrew、Debian 升级后后台服务还在跑旧版本 | 升级后服务在运行就重启，并确认守护进程报告的是新版本；确认不了时写出手动重启命令 | 单测 brew_upgrades_the_exact_active_formula_only_after_the_tap_is_ready（已是最新时不重启）；真机：deb 安装下升级 |
| 97 | APT 锁定（hold）的包仍提示可直接升级，运行后才报错 | hold 当作“已固定”：升级命令带上 `sudo apt-mark unhold`，直接运行时先提示解除锁定 | 单测 a_held_deb_is_reported_like_a_pinned_installation |
| 98 | 发布清单只允许从声明过的某一个版本升级，其他版本一律“需要手动迁移” | 去掉回滚兼容标记和 `update rollback`；任何更新的签名稳定版都能升级，新版本起不来时仍自动换回旧版本；清单保留两个旧字段让 0.2.2 还能升级 | 单测 verifies_publisher_signed_bytes_before_staging_an_executable、unhealthy_daemon_restores_last_verified_executable_and_service；`test_installer.py` test_archive_manifest_still_lets_0_2_2_upgrade |
| 99 | `iphone-control status \| head -1`：读者先退出，后面的 `println!` 遇到 EPIPE 就 panic（退出码 101，打出崩溃信息）。Rust 启动时把 SIGPIPE 设成了忽略 | 只打印结果就结束的命令恢复 SIGPIPE 默认行为，安静退出；守护进程、MCP、viewer、wda-bridge 和跨守护进程重启的 update、service、setup 保持忽略 | 单测 sigpipe_default_only_for_commands_that_print_and_end、集成测试 a_closed_stdout_ends_printing_commands_quietly |
| 100 | 0.2.2 起控制台每次按下都先请求 `/wda/screen`（约 0.1～0.2 秒），HID 会话空闲 30 秒关掉后，鼠标一直停在画面里时第一下还要等约 0.5 秒重开 | 按下只问方向，尺寸在转屏时和距上次测量满 60 秒后才重量；鼠标在画面上移动时每 20 秒最多预热一次 HID 会话 | 单测 display_poll_remeasures_size_only_when_needed；页面测试 hovering keeps the HID session warm at most every 20 seconds；真机：控制台连按，按下往返在几十毫秒内 |
| 101 | 控制台开着、页面走 WDA 截图的画面（浏览器解不了 HEVC，或低于 iOS 27）时，WDA 每 1.5～2.5 分钟被 iOS 杀掉（`cpu_resource_fatal`：60 秒内平均 86%～93% CPU，上限 80%）。MJPEG 缩到 75% 让 WDA 逐帧解码、缩放（CoreGraphics）、重新编码；横屏时 `mjpegFixOrientation` 让它逐帧旋转、重新编码，同样超限 | MJPEG 原尺寸、不转正（`MJPEG_SCALE` = 100，`mjpegFixOrientation` = false）：WDA 直接转发 testmanagerd 硬件编好的 JPEG，帧率不变（约 34 fps）。横屏的竖帧由页面转正（H.264 按 `display.rotation`，MJPEG 的 img 由浏览器按 EXIF） | 真机（iPhone 15 Pro Max，iOS 27.0.1）：拉本机编码的 `/video`，竖屏 25 分钟、横屏 5 分钟不再被杀（开着转正时横屏 2 分钟被杀）；无头 Chrome：横屏下 H.264 和 MJPEG 画面方向正确、坐标换算正确 |
| 102 | iPhone 15 Pro Max（iOS 27.0.1）每次锁屏约 10 秒：经 IOHID 短按电源键（0.05～0.1 秒）调用卡约 5.3 秒且不锁，再等 3 秒才退回 `/wda/lock`（约 1.9 秒）。iPhone 18 Pro Max（iOS 27.2.1）正相反 | 按设备记住哪种锁屏有效（`PerDevice::lock_via_wda`），之后先用它；先用的不灵仍会试另一种 | 单测 lock_order_follows_what_worked_on_this_device；真机：同一台连续锁屏，第一次约 10 秒、之后约 2 秒 |
| 103 | WDA 半死：`/status` 照常 200，前台 App、截图、读元素、`/wda/locked` 全部超时（iPad mini 5，iPadOS 26.7.3，后台启动的 WDA 跑 20～40 分钟后）。守护进程只看 `/status`，一直显示「就绪」，不重启；后台模式下守护进程重启还会把它接回来。只靠巡检探测也不够：卡死时工具调用一个接一个等满超时（每个 10～30 秒），设备被占好几分钟，巡检抢不到锁 | WDA 客户端数经 XCTest 的请求（`/status` 之外都是，工具调用和巡检探测 `/wda/activeAppInfo` 一样算）连续几次送到了却没有应答，有应答（哪怕是错误）就清零；连续 3 次就算卡死：之后经 XCTest 的请求直接报不可用，巡检和每个设备请求开头都会忘掉记下的后台 WDA、丢掉会话重新启动 | 单测 three_unanswered_requests_mean_stalled、xctest_probe_counts_only_missing_answers；真机：绕过守护进程让 WDA 长按（期间 `/status` 5 毫秒 200，`/wda/activeAppInfo` 超时）。空闲时 0.2.3 一直「就绪」，修复后约 28 秒判定卡死、34 秒恢复；同时经守护进程背靠背发工具调用时，main（#60）180 秒里 8 次调用各等 10～30 秒、不重启，修复后第 3 次超时（约 50 秒）就重启，13 秒后就绪，其间的调用立即返回；经守护进程连续长按 6×10 秒不误判 |
| 104 | iOS 27 以下（iPad mini 5，iPadOS 26.7.3）锁屏没锁上：经 IOHID 按电源键 3 秒后未锁、`/wda/lock` 报「Timed out while waiting until the screen gets locked」，之后退到 HID 后备路线，而低版本没有 HID，报的是「WDA 不可用（刚掉线）」，WDA 其实正常 | WDA 在、两种办法都没锁上、又没有 HID 后备时，报「没锁上屏」和两种办法各自的原因；有 HID 后备时照旧退到 HID | 单测 a_failed_lock_says_why_not_that_wda_is_down；真机：见 PR |
| 105 | iPad mini 5（iPadOS 26.7.3）一直锁不上屏：经 IOHID 按电源键 0.1 秒调用卡约 5.4 秒、不锁；`/wda/lock`（iOS 27 以下是 XCTest 的 `pressLockButton`）10 秒后报「Timed out while waiting until the screen gets locked」。iPhone 15 Pro Max（iOS 27.0.1）上 0.1 秒同样不认，每台第一次锁屏要退到 `/wda/lock`，约 10 秒 | 电源键按 0.15 秒（`POWER_PRESS_SECS`）：iPad mini 5、iPhone 15 Pro Max 上 0.15～0.3 秒都能锁，iPhone 18 Pro Max（iOS 27.2.1）上 0.05～0.15 秒能锁、0.5 秒不行 | 单测 hid_buttons_send_consumer_page_events；真机：经 `wda-bridge` 每次先解锁再按不同时长，看 `/wda/locked`；经守护进程连续锁屏 3 次 |
| 106 | AI 任务进行中，另一个 MCP 客户端或命令行照样能点手机，和 agent 抢着操作（监管只在 viewer 里） | 任务由守护进程管：不带任务口令的请求可以看，操作一律拒绝并说明是哪个任务，用户接管后才放行；切换设备一律拒绝 | 单测（`others_may_look_but_not_act_until_the_user_takes_over`、`an_opened_task_holds_the_phone_until_its_lease_goes`）+ 真机（任务进行中用 `iphone-control` 另开的 MCP 点击被拒，截图照常） |
| 107 | 任务进行中在别处 `select` 或自动选设备换了一台，任务接着操作另一台手机 | 任务的请求固定发往开任务时的设备；控制的设备变成另一台时任务中断（interrupted） | 单测（`the_task_goes_to_its_own_device`、`another_device_interrupts_the_task`） |
| 108 | 守护进程重启后任务和步骤记录都没了，也说不清任务怎么结束的 | 任务事件逐条落盘（`tasks/<id>.jsonl`，最多 50 个），重启后没结束的记成 interrupted，旧口令失效 | 单测（`logs_survive_a_restart_and_unfinished_tasks_are_interrupted`、`tasks_keep_their_order_across_a_restart_even_within_a_millisecond`、`only_the_newest_logs_are_kept`） |
| 109 | 控制台刷新后不再跟踪正在进行的任务；在别处开的任务页面上看不到 | 页面接着跟刷新前的任务（事件从头重放），也接上在别处开的任务；守护进程不记得的才标成已停止 | 单测（`tests/viewer/tasks.test.mjs`）+ 无头 Chrome 或手动（任务进行中刷新页面） |
| 110 | AI 任务只该改设置，却点进了微信、打开了 `tel:` 链接或别的 App，没人拦 | 限定了 App 范围的任务，每个操作前看它碰到哪个 App（启动目标、链接、当时的前台），超出范围就停下来等人确认；`tel:` 这类链接、安装和卸载一律先问；看不出前台时不操作；拒绝只拒这一步，任务可以回到范围内继续 | 单测（`what_is_outside_the_scope`、`what_each_request_checks_against_the_scope`、`an_approved_app_joins_the_scope_for_the_rest_of_the_task`、`a_rejected_app_refuses_that_step_but_the_task_can_go_back`、`a_question_nobody_waits_for_leaves_the_page`、`the_scope_survives_a_restart`）+ 真机（`task run --app 设置` 的任务启动「信息」时页面出现确认） |
| 111 | 控制台上 AI 任务的总结直接显示 Markdown 原文（`**iOS 27.0.1**`、`- **设置**：…`），换行也被合并成一行 | 总结先整体转义，再按一小部分 Markdown 显示（粗体、斜体、行内代码、列表、标题、分隔线、换行、http(s) 链接），原始 HTML 一律当文字 | 页面测试（`tests/viewer/summary.test.mjs`）+ 真机（Claude Code、Codex 任务的总结） |
| 112 | iOS 27 上控制台用手机自己的视频时，系统权限弹窗（「允许“日历”使用你的位置？」）在画面里只剩变暗的屏幕，看不到弹窗本身，接管的人不知道该点哪里；agent 的截图里有 | 画面是手机自己的视频时，页面问守护进程有没有系统弹窗（`AlertCheck`；在 App 里每 2 秒，主屏幕上查一次约 0.6 秒，没弹窗时放慢到 6 秒，刚触摸后 3 秒内不问），有就照 iOS 弹窗的样子把它画在画面里它所在的位置（文字、按钮）；按钮按坐标点（按名字点「允许」会点到「不允许」）；AI 任务进行中按钮要先接管 | 单测（`a_system_alert_with_its_own_buttons`）+ 页面测试（`tests/viewer/alert.test.mjs`）+ 真机（弹出权限请求时页面显示弹窗和按钮，点按钮生效） |
| 113 | 在控制台画面上拖动、滑动时，触控板双指一碰就变成了缩放画面（双指捏合在浏览器里是按住 Ctrl 的滚轮） | 画面只用上方的加减号缩放；画面区域里的 Ctrl/⌘ + 滚轮、Safari 的 gesture 事件都拦下，既不缩放画面也不缩放整页；触屏上画面周围不允许双指缩放（`touch-action: pan-x pan-y`） | 页面测试（`tests/viewer/zoom.test.mjs`）+ 真机（在画面上双指捏合，缩放比例不变） |
| 114 | 设置菜单不能用方向键，取消后可能留下终端输入模式 | 方向键和编号选择一致，Esc/Ctrl+C 不提交当前选择并恢复终端；基础终端保留逐行输入 | 菜单状态单测 + PTY 回归（不连接设备） |
| 115 | 生成 Shell 补全触发配置读取或设备维护锁，脚本与 CLI 定义不同步 | Bash/Zsh/Fish 从实际命令树生成，生成路径不读取设置、不连接设备、不申请维护锁 | 补全单测、CLI 子进程回归、Bash/Fish 候选断言及 Zsh 交互验证 |
