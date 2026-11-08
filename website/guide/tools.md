# 工具一览

MCP 服务提供 17 个工具。坐标单位是**点**，和整屏截图的像素一一对应（iPhone 15 Pro Max 是 430×932），agent 从截图上读到的位置可以直接点。坐标随屏幕方向变化：转屏后同一个位置的坐标不同，要重新截图。不同设备屏幕尺寸不同，iPad mini 5 竖放是 768×1024。

## 看屏幕

| 工具 | 作用 |
|---|---|
| `iphone_status` | WDA 是否就绪、是否锁屏、前台 App、屏幕尺寸 |
| `iphone_screenshot` | 截图；给出 `x`、`y`、`width`、`height` 时只返回这块区域的放大图，看清小字和小图标 |
| `iphone_list_elements` | 列出屏幕上带文字的元素：类型、文字、中心点、尺寸；`filter` 可按文字或类型筛选 |
| `iphone_wait_for` | 等某段文字出现（页面加载完、弹窗出现），出现就返回；输入框里的字不算。`timeout` 默认 5 秒，最多 30 秒 |
| `iphone_list_apps` | 已安装的 App；只读，没有 `then` |

开启[自动解锁](./auto-unlock)时，手机锁着的话，`iphone_screenshot`、`iphone_list_elements`、`iphone_wait_for` 也会先解锁。这些工具标为「只读」，指的是不改界面内容。`iphone_status`、`iphone_list_apps` 不会解锁。

## 操作

| 工具 | 作用 |
|---|---|
| `iphone_tap` | 按坐标点击 |
| `iphone_tap_element` | 按文字点击；中心被悬浮栏、键盘挡住时改点露出来的部分，整个被挡住就不点 |
| `iphone_long_press` | 长按；`seconds` 默认 1，最多 10 |
| `iphone_swipe` | 滑动；列表停稳后才返回。`seconds` 默认 0.3，最多 10 |
| `iphone_type_text` | 在当前输入框输入文字（支持中文），输完读回核对，丢字会补上；`submit` 时按回车 |
| `iphone_press_button` | `home`、`volume_up`、`volume_down`、`lock` |
| `iphone_launch_app` / `iphone_terminate_app` | 启动 / 结束 App，可以用名字（如「设置」）或 Bundle ID |
| `iphone_open_url` | 打开链接或 URL Scheme |
| `iphone_batch` | 一次执行最多 20 步，出错就停。MCP 客户端取消调用时，剩下的步骤不再执行；没选定设备时，执行中控制的设备变了也会停下报错 |

## 接着多台设备

同一时间只控制一台，其他工具都作用在它上面。只接着一台时不用管这两个工具。

| 工具 | 作用 |
|---|---|
| `iphone_list_devices` | 列出 USB 上接着的 iPhone/iPad：名字、型号、iOS 版本、UDID，标出正在控制哪台、有没有设自动解锁密码 |
| `iphone_select_device` | 改为控制另一台（UDID 或设备名），重启后还记得；`auto` 改回自动选。新设备上的 WDA 起来要 10~60 秒。设了 `IPHONE_UDID` 时不能切换 |

详见[多台设备](./multi-device)。

## 开发者工具（显式开启）

给 MCP 服务设 `IPHONE_CONTROL_DEV_TOOLS=1` 后多出六个工具。默认关闭，用不到的 agent 不必多读这些工具说明。

| 工具 | 作用 |
|---|---|
| `iphone_app_logs` | 读手机的系统日志（默认、Error、Fault 级别；Debug、Info 会拖慢手机，不记录），可按 App 或进程、文字、时间筛选。第一次调用才开始记录：先调一次，复现问题，再调一次 |
| `iphone_crash_reports` | 列出崩溃报告，或读取其中一份 |
| `iphone_install_app` | 安装签好名的 `.ipa` 或 `.app`（路径在运行守护进程的机器上） |
| `iphone_uninstall_app` | 卸载 App 及其数据；不会卸载 WebDriverAgent |
| `iphone_web_targets` | 列出可检查的网页和 WebView（Safari 标签页、混合 App 的调试构建）；页面所在的 App 要在前台 |
| `iphone_web_eval` | 在页面里执行 JavaScript，以 JSON 返回结果：读网页内容比截图快、省 token、也更准 |

## 操作后顺带看界面：`then`

上面「操作」一节的工具都可以带 `then`，等界面稳定后顺带返回操作后的样子，省一次往返：

| 参数 | 含义 |
|---|---|
| `then` | `elements`（元素列表）或 `screenshot`（截图） |
| `then_filter` | 只列包含这段文字（或这种类型）的元素 |
| `then_region` | 只返回截图里 `[x, y, width, height]` 这块的放大图，例如游戏棋盘 |
| `then_settle` | 最多等几秒让界面稳定，默认 1.5，最多 10 |

结果里会说明等了多久、页面是否可能还在加载、有没有弹窗挡着。不带 `then` 时，如果有系统弹窗挡着也会提醒。

## WDA 不可用时：HID 后备

连不上 WDA 时，截图、点击、滑动、按键改走 CoreDevice HID。有这些限制：

- 要 iOS 27 及以上。更早的 iOS 没有这条路线，WDA 重启期间操作会失败。
- **横屏**：每次触摸前都重新截图核对方向，还要 30 秒内从 WDA 拿到过方向并且和当前一致，否则拒绝触摸，不会猜（守护进程刚重启后就是这样）。截图和按键不受影响。WDA 掉线期间不要把设备翻到另一侧，等它恢复再翻。
- **屏幕尺寸**：WDA 不可用、又没有这台设备的尺寸记录时，点击和滑动报错。等 WDA 起来一次，或设 [`SCREEN_W`/`SCREEN_H`](./configuration#高级)。
- **不会重做**：请求已经发给 WDA、连接才断开或响应不完整时，不退到 HID，因为动作可能已经做了，重做会点两次、输两遍。工具会报错，agent 要先截图确认再决定。

完整参数见仓库里的 [docs/tools.md](https://github.com/Derrors/iphone-control/blob/main/docs/tools.md)（英文）。
