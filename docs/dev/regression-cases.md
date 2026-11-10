# 必须覆盖的回归用例

#1～#34 来自 toolkit 和本项目早期实际踩过的坑，#35～#45 是多设备和 HID 横屏，#46～#58 是实时画面、开发者工具和英文界面，#59～#63 是 iPhone 18 Pro Max（iOS 27.2.1）真机全量测试发现的问题，#64 起是 viewer 的 AI 任务和新页面。能单测的写单测，不能单测的进真机回归清单：改了设备、时序、WDA 相关的代码，要在真机上重跑相关的用例，并在 PR 里写明跑了哪些、用什么设备和 iOS 版本。

| # | 坑 | 正确行为 | 测试方式 |
|---|---|---|---|
| 1 | 锁屏密码键盘不显示时，`Key` 元素仍留在元素树里（`visible=false`） | 不以"树里有键盘"判断键盘已出现；固定先唤醒再上滑 | 单测（脱敏 XML 样本）+ 真机 |
| 2 | `displayed` 接口在锁屏上返回 false，即使键盘可见 | 以密码栏「已输入 N 位」为准 | 真机 |
| 3 | 锁屏界面 Face ID 动画使 WDA 每请求等 idle 至 10 秒，键盘随后自动收起 | 解锁期间 `waitForIdleTimeout=1`、`animationCoolOffTimeout=0.2`，结束恢复；0 会导致唤醒失败 | 真机 |
| 4 | 多位密码合成一个 W3C 手势会漏键 | 每位单独一个手势并逐位核对 | 真机 |
| 5 | 锁屏上 `/wda/tap` 每次约 2.9 秒 | 解锁用 W3C 单手势（约 0.7 秒） | 真机计时 |
| 6 | 同名「设置」有 5 个 `SBAppTags` 含 `hidden` 的后台服务 | 按名字解析 App 时跳过 hidden | 单测 |
| 7 | 容器无 udev，手机重启/重插后 usbmuxd 看不到设备 | sysfs 里 iPhone/iPad/iPod 的台数连续 3 次多于 usbmuxd 认到的台数 → 重启 usbmuxd（只在自管 usbmuxd 时） | 单测（`usbmuxd_restart_after_three_misses`）+ 真机（杀 usbmuxd 模拟） |
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
| 30 | `.env` 里留空的 `ICTL_LOG=` 生成一个什么都不放行的过滤器，日志全没了 | 空值当作没设 | 真机（`docker logs`） |
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
| 41 | 已接着一台时再插一台，没有 udev 的 usbmuxd 认不到第二台 | 比台数，不只看「有没有设备」 | 单测（`usbmuxd_restart_when_a_second_device_is_missed`）+ 真机 |
| 42 | 自动选设备时 `iphone_batch` 执行到一半换了设备（拔线后换到另一台），剩下的步骤落到另一台上 | 控制的设备变了就停下报错 | 真机（batch 里放 `wait`，中途拔线） |
| 43 | Apple 键盘等配件被当成手机：`doctor` 误报 USB 上有设备，usbmuxd 被反复重启 | 只算 iPhone/iPad/iPod（厂商号 05ac 且产品名匹配） | 单测 + 真机 |
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
| 65 | viewer 已经停了、任务已经停了，agent 的 MCP 服务还在继续操作手机 | 闸门连不上或任务已结束一律拒绝，并告诉 agent 停下 | 单测（`a_gate_nobody_listens_on_refuses`、`a_stopped_task_refuses_everything`）+ 真机（任务中途停止，之后没有新步骤，agent 进程组已结束） |
| 66 | 暂停只挡住了界面上的按钮，agent 照样继续点 | 暂停、接管、等确认时，下一个工具调用在闸门处挂住，恢复后才放行 | 单测（`a_paused_task_holds_the_call_until_resumed`）+ 真机（暂停 8 秒没有新步骤） |
| 67 | 用户接管时动过手机，交还后 agent 按旧的界面接着点 | 交还后第一个不是看屏幕的调用被拒，让它先截图 | 单测（`after_a_takeover_the_agent_looks_again_before_acting`）+ 真机（交还后先截图） |
| 68 | 用户拒绝确认后 agent 正常结束，页面显示「已完成」 | 有被拒绝的一步就按「已停止」结束 | 单测（`a_rejected_confirmation_tells_the_agent_not_to_do_it`）+ 真机（拒绝删除世界时钟城市，没有删） |
| 69 | 按结果文字里的 `@(x,y)` 给所有步骤画点击标记，读元素的一步也被标成点击 | 只给 `iphone_tap_element` 的结果解析点击位置 | 单测（`only_a_tap_by_text_gets_a_mark_from_its_result`） |
| 70 | 其他网站用 `<img src="http://127.0.0.1:8200/api/elements">` 触发读元素，顺带自动解锁手机 | 浏览器标明 `Sec-Fetch-Site` 不是 same-origin / none 的读请求一律 403 | 单测（`reads_from_other_sites_are_refused`）+ curl |
| 71 | AI 任务执行中，用户在页面上点击和 agent 同时操作 | 没有接管时 viewer 拒绝手动操作（409 并说明），页面也锁住 | 真机（任务中 `/api/tap` 被拒，接管后按 Home 生效） |
| 72 | 设计稿在 1024×768 下横向溢出 26px：中间栏各行按内容最小宽度把栏撑出窗口 | 中间栏各行 `min-width: 0`；两栏区间「设备面板」只留图标；状态标签先让出空间 | Playwright：360×800 到 1920×1080 共 11 个视口横向溢出都是 0 |
| 73 | viewer 的手动操作走 WDA：点击约 530 毫秒，滑动要等列表停下（1.8～3 秒），松开鼠标后才滑，画面不跟手 | iOS 27 起经 HID 实时发按下、移动、松开（`Request::Touch`），移动只发最新位置；滚轮换成手指拖动 | 单测（`page_actions_map_to_daemon_requests`）+ 真机（iPhone 18 Pro Max / iOS 27.2.1：移动约 2 毫秒，点击约 20 毫秒；拖动、滚轮滚动设置列表，点击进入「通用」，按住 1 秒出主屏菜单） |
| 74 | 每次 HID 触摸前截图核对方向（几百毫秒），实时移动跟不上 | 只在 `down` 时核对一次（有 WDA 时读它的旋转角），移动、抬起直接用已开的会话 | 真机计时 |
| 75 | WDA 的 MJPEG 默认每秒 10 帧，画面比操作慢 | 每秒 60 帧、75% 尺寸（`fast_stream`）；只在有客户端时采集 | 真机：约 48 fps、34 KB/帧，读元素树不变慢 |
| 76 | viewer 页面只有中文 | 页面文字中英两套（`tr()`），默认跟着 `IPHONE_CONTROL_LANG`，可切换；请求带 `X-Ictl-Lang`，守护进程的提示和 AI 任务用同一种语言 | 单测（`the_page_says_its_default_language`）+ Playwright（英文页面除切换按钮和手机上的数据外没有中文；360×800 到 1920×1080 中英文都没有横向溢出）+ 真机（英文 AI 任务） |
