# 工具一览

MCP 服务提供 15 个工具。坐标单位是**点**，和整屏截图的像素一一对应（iPhone 15 Pro Max 是 430×932），agent 从截图上读到的位置可以直接点。

## 看屏幕

| 工具 | 作用 |
|---|---|
| `iphone_status` | WDA 是否就绪、是否锁屏、前台 App、屏幕尺寸 |
| `iphone_screenshot` | 截图；给出 `x`、`y`、`width`、`height` 时只返回这块区域的放大图，看清小字和小图标 |
| `iphone_list_elements` | 列出屏幕上带文字的元素：类型、文字、中心点、尺寸；`filter` 可按文字或类型筛选 |
| `iphone_wait_for` | 等某段文字出现（页面加载完、弹窗出现），出现就返回；输入框里的字不算 |

## 操作

| 工具 | 作用 |
|---|---|
| `iphone_tap` | 按坐标点击 |
| `iphone_tap_element` | 按文字点击；中心被悬浮栏、键盘挡住时改点露出来的部分，整个被挡住就不点 |
| `iphone_long_press` | 长按 |
| `iphone_swipe` | 滑动；列表停稳后才返回 |
| `iphone_type_text` | 在当前输入框输入文字（支持中文），输完读回核对，丢字会补上；`submit` 时按回车 |
| `iphone_press_button` | `home`、`volume_up`、`volume_down`、`lock` |
| `iphone_launch_app` / `iphone_terminate_app` | 启动 / 结束 App，可以用名字（如「设置」）或 Bundle ID |
| `iphone_open_url` | 打开链接或 URL Scheme |
| `iphone_list_apps` | 已安装的 App |
| `iphone_batch` | 一次执行最多 20 步，出错就停 |

## 操作后顺带看界面：`then`

每个操作工具都可以带 `then`，等界面稳定后顺带返回操作后的样子，省一次往返：

| 参数 | 含义 |
|---|---|
| `then` | `elements`（元素列表）或 `screenshot`（截图） |
| `then_filter` | 只列包含这段文字（或这种类型）的元素 |
| `then_region` | 只返回截图里 `[x, y, width, height]` 这块的放大图，例如游戏棋盘 |
| `then_settle` | 最多等几秒让界面稳定，默认 1.5，最多 10 |

结果里会说明等了多久、页面是否可能还在加载、有没有弹窗挡着。不带 `then` 时，如果有系统弹窗挡着也会提醒。

完整参数见仓库里的 [docs/tools.md](https://github.com/Derrors/iphone-control/blob/main/docs/tools.md)。
