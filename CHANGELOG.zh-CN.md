# 更新日志

[English](CHANGELOG.md) | **简体中文**

每个版本改了什么，一行一条。版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)；0.x 期间次版本号也可能有不兼容的变化，升级前先看「升级注意事项」。和 [CHANGELOG.md](CHANGELOG.md) 一一对应，发布说明从这两个文件里取。

## [Unreleased]

## [0.2.9] - 2026-11-18

### 修复

- 原生控制（iOS 27）下，手机上的无障碍服务不再应答时，读界面不会再一直失败到重新插线为止；会自动把它重启（#152）。
- 原生控制（iOS 27）下，自动解锁不会再在密码键盘已经出来时报「调不出锁屏密码键盘」（#154）。
- 原生控制（iOS 27）下，按 Home 后马上启动的 App 不会再被送回主屏幕，守护进程刚启动后的头几个操作以前常常这样（#155）。
- 原生控制（iOS 27）下，手机上的无障碍服务把每个 App 都读成空的时，读界面不会再一直失败；这时也会自动把它重启（#156）。

## [0.2.8] - 2026-11-18

### 新功能

- `iphone_tap` 新增 `count`：在同一点一次连点几下，双击和要短时间内连点几下的入口都能点到（#141）。
- `iphone_batch` 新增 `one_gesture`：一串坐标点击（比如键盘上的键）放进一个手势里点完，快一倍左右（#143）。
- `iphone_swipe` 新增 `hold`：先按住再移动，主屏幕图标等要先长按才拿得起来的东西也能拖动了（#144）。
- `iphone_tap_element` 新增 `scroll`（`down` / `up`）和 `max_scrolls`：屏幕上没有时边滑边找，找到就点（#147）。

### 改进

- 同一个操作原样重做、界面还是没变时，结果里说明已经连续几次，并提示换个做法（#145）。
- 操作前先自动解锁了的话，结果里会说明，并写明用了多久（#135）。

### 修复

- App 内浏览器里的元素位置不再整体偏下一个导航栏的高度，按文字点击能点中（#139）。
- 第一次启动系统 App 时，不再被隐私说明页卡住一分钟、还让 WDA 重启；马上说明被说明页挡着（#148）。
- 原生控制（iOS 27）下，设置页底部悬浮搜索栏下面的行不再隔着搜索栏去点、打开搜索（#150）。
- `iphone_tap_element` 什么都没点到时，带 `then` 不再说「操作可能没生效」，返回也更快（#149）。
- 几个慢的 WDA 请求不再累计成一次不必要的 WDA 重启（#140）。

## [0.2.7] - 2026-11-15

### 升级注意事项

- iOS 27 及以上的手机改用原生控制。想继续用 WebDriverAgent，在 `~/.config/iphone-control/env` 里设 `ICTL_IOS_BACKEND=wda`（#123）。
- 从源码编译需要 Rust 1.89（#119）。
- 去掉了 `ICTL_WDA_DETACHED`：WDA 现在总在后台启动，设置文件里留着这一项也不起作用（#125）。

### 新功能

- iOS 27 及以上不再需要 WebDriverAgent：不用 Apple ID、不用签名，几秒就绪。iOS 26 及以下不变（#123）。
- WebDriverAgent 不可用时也能输入中文等非 ASCII 文字，手机询问时点「允许粘贴」（#118）。
- 带 `then` 的操作会说明界面变没变，Agent 能看出点击没生效（#121）。

### 改进

- WDA 在锁屏时也能启动，守护进程重启、升级时它不跟着断（#125）。
- 依赖升级（#119）。

### 修复

- 机器很忙时，引导菜单里在半个按键之后按的 Ctrl+C 不再被忽略（#131）。
- 原生控制的手机上，`iphone-control status` 显示「原生」，不再提示去装 WDA（#127）。
- `service stop`、`service uninstall` 也会结束后台启动的 WDA，每台手机各用各的（#124）。
- 粘贴到手机上的文字不再经「通用剪贴板」传到 Mac（#122）。
- 弹着通知横幅时，读到的是 App 的元素，不再是横幅的（#121）。
- 机器很忙时，引导菜单里的方向键不再变成字母（#120）。
- WDA 没及时应答的点击、按键，不会过一阵又落到屏幕上（#118）。
- 元素特别多的界面读得更快，列表被截短时会说明（#118）。
- 停一会儿后的第一次点按不再等重新读屏幕尺寸（#110）。
- 控制台的 H.264 或手机视频启动慢时会再试，不再一直停在 MJPEG（#109）。
- 控制台上的滑动返回更快（#108）。
- 守护进程重启后，部分设备第一次按 Home 不再慢（#107）。

## [0.2.6] - 2026-11-14

### 新功能

- 控制台画面的弱网模式，在「设置」里打开，流量约减一半（#104、#105）。

### 改进

- 控制台复用连接，Wi-Fi 下点按和查询的响应快了好几倍（#103）。

### 修复

- 控制台上手机自己的视频在 Wi-Fi 下不再卡顿（#100）。
- 控制台的点按和 Home 不再排在页面的后台查询后面（#101）。
- 控制台上按 Home 的响应时间约减一半（#102）。
- 手机重开屏幕媒体流后，控制台会重新接上画面（#104）。

## [0.2.5] - 2026-11-14

### 升级注意事项

- `--url` 指向另一台机器时必须用 `https://`（自签证书加 `--ca`），本机的 `http://127.0.0.1` 照旧（#89）。
- 只读令牌不再让手机自动解锁（#89）。

### 改进

- 登录页和控制台风格一致，有自己的明暗和语言切换（#97）。
- 守护进程或 WDA 重启后约快 10 秒就绪（#90）。
- 等「Enable UI Automation」密码时，手机上大部分时间都有密码框可输（#90）。
- WDA 卡死后约 35 秒重启，原来要 90 秒（#90）。
- 守护进程重启时，命令、MCP 调用和控制台会等它回来，不再直接失败（#90）。
- 输错令牌太多次不再连正确的令牌也拒绝（#89）。

### 修复

- 没有 WDA 时，iOS 27 以下的设备点按、滑动会马上说明需要 WDA（#98）。
- 确认不再冲掉你的暂停或接管，同时出现两个确认时依次问（#88）。
- 你暂停、接管或交还前 Agent 刚发出的一步，不会在之后才执行（#88）。
- 交还控制后，Agent 要先看过屏幕才能操作（#88）。
- 删除或更换登录令牌后，用它打开的连接 5 秒内全部断开（#89）。
- 控制台启动的 Agent 改在私有目录里工作，不再用 `/tmp`（#89）。
- 控制台出错后照常接受连接，并断开卡住的连接（#88）。
- 断电时写了一半的任务记录会被清理（#88）。

## [0.2.4] - 2026-11-14

### 升级注意事项

- 控制台和它的 `/mcp` 要登录了。运行 `iphone-control viewer token`，打开它打印的链接；经 HTTP 接入的 MCP 客户端带 `Authorization: Bearer <令牌>`。命令行和 stdio 方式的 MCP 不受影响（#79）。
- 开了 `--lan` 时，局域网里走 HTTPS，第一次打开时核对 `service status` 显示的证书指纹（#76）。
- `ICTL_AGENT_GATE`、`ICTL_AGENT_TOKEN` 改成 `IPHONE_CONTROL_TASK_TOKEN`（#64）。

### 新功能

- 任何 Agent 都能用 AI 任务：`iphone-control task run -- <命令>`，用 `task list`、`events`、`pause`、`resume`、`takeover`、`handback`、`stop`、`approve`、`reject` 控制（#64）。
- 任务可以限定 App：`task run --app`，或控制台上的 App 按钮。到了别的 App，每一步都先等你确认（#69、#70）。
- Agent 不能暂停、批准或以其他方式控制自己的任务（#75）。
- 页面刷新后，控制台会接上正在进行的任务，包括在别处开的（#64）。
- `viewer --lan` 把控制台经 HTTPS 开放到局域网（#72、#76）。
- 控制台同一端口提供 MCP over HTTP（`/mcp`）（#77）。
- 有名字的登录令牌，可以是只读的：`viewer token add <名字> [--read-only]`（#80）。
- 操作另一台机器上的手机：`mcp --url` 转到它的 `/mcp`，`task … --url` 在那里开任务、控制任务（#80、#82）。
- `service viewer install` 把控制台装成后台服务（#74）。
- 设置菜单可以用方向键选择；`iphone-control completions <shell>` 生成补全脚本（#71）。

### 改进

- 任务由守护进程管理：任务进行中，其他客户端只能看、不能操作（#64）。
- 画面只能用上方的 − / + 缩放，不再响应捏合（#68）。

### 修复

- 部分 iPhone 锁屏从约 10 秒降到约 2 秒，iOS 27 以下的 iPad 也能锁屏了（#59、#63）。
- WDA 还应答 `/status`、其他请求都不应答时，约 30 秒内会被重启（#60、#62）。
- 锁屏失败时如实说明两种锁屏办法各自的结果，不再报「WDA 刚掉线」（#61）。
- iOS 27 上，手机视频里看不到的系统弹窗会画在画面上，可以直接点（#67）。
- 控制台上的任务总结按格式显示，不再是 Markdown 原文（#66）。
- 接着多台设备时，每台的屏幕尺寸都记住，不再只记最后一台（#86）。
- 通知标题写设备自己的名字，不再一律写「iPhone」（#85）。
- 升级不再偶尔因「Text file busy」失败（#81）。
- `task finish` 之后，`task run` 保留命令自己的退出码（#78）。
- 在设置菜单里长按方向键不再回显按键序列（#73）。

## [0.2.3] - 2026-11-13

### 升级注意事项

- 从 0.2.2 升级要手动一次：脚本安装运行 `iphone-control update apply`，Homebrew 运行 `brew upgrade iphone-control`，Debian / Ubuntu 运行 `sudo apt install ./<文件名>.deb`。之后直接用 `iphone-control update`（#51）。
- 去掉了 `update rollback`。新版本起不来时仍会自动换回旧版本（#52）。

### 新功能

- `iphone-control update` 检查并安装新稳定版，支持脚本安装的服务、Homebrew 和 `.deb`。安装前会先问，`--yes` 跳过（#51、#52）。
- Debian 软件包有签名，安装前会验证（#52）。

### 修复

- 控制台显示 H.264 或 MJPEG 时，WDA 不再每 1～2 分钟被 iOS 杀掉（#55）。
- 控制台的触控恢复灵敏（#54）。
- `iphone-control status | head -1` 这类用法不再报「Broken pipe」崩溃（#53）。

## [0.2.2] - 2026-11-12

### 升级注意事项

- 新装用签名 GitHub Release 里的 `install.sh`，需要 Python 3 和 OpenSSL 3。旧版脚本安装要先用原脚本卸载，设置和设备数据会保留（#44）。

### 新功能

- 检查更新：`iphone-control update check`，或控制台的「关于与更新」（#44）。
- 脚本安装用 `update apply` 升级，`update policy auto-patch` 可开启自动安装签名补丁（#44）。
- 控制台改版，适配宽窄窗口，显示设备名称和外框（#45）。

### 修复

- 换机、转屏或断流后，点按和触控会被拒绝，不再点到旧位置（#45）。
- 关闭或冻结的标签页会释放画面连接（#45）。

## [0.2.1] - 2026-11-12

### 新功能

- iOS 27 及以上，控制台直接显示手机自己的视频：HEVC，约 60 帧，原生分辨率（#42）。
- 其他情况用 H.264，流量约为原来的 1/15，能用硬件编码就用（#42）。
- 实验开关 `ICTL_WDA_DETACHED=1`：在后台启动 WDA，锁屏时也能启动（#41）。

### 修复

- iOS 27 及以上，开着控制台时 WDA 不再每隔几分钟被 iOS 杀掉（#42）。

## [0.2.0] - 2026-11-11

### 升级注意事项

- 不再发布 Docker 镜像。先停掉容器（`docker compose down`），改用 `install.sh`、Homebrew 或 `.deb`。
- 想保留 Apple ID 会话、签好的 WDA 和解锁密码，先把容器的 `./data` 复制到 `~/.local/share/iphone-control`（[做法](https://derrors.github.io/iphone-control/deploy/backup)）。手机可能会再问一次是否信任这台电脑。
- 设置从 `.env` 搬到 `~/.config/iphone-control/env`。
- 守护进程不再自己运行 usbmuxd。Linux 上装系统的（`sudo apt install usbmuxd`），之前为容器屏蔽过的要解除屏蔽。
- 重新注册 MCP 服务：首次引导会做，或运行 `iphone-control mcp-config` 打印的命令。

### 新功能

- 一行安装：macOS（Apple Silicon）和 Linux 用 `install.sh`，还有 Homebrew 和 Debian 12+ / Ubuntu 22.04+ 的 `.deb`，都带后台服务。
- 首次引导：装好后运行 `iphone-control`；`setup --reconfigure` 可以重做任意一部分。
- `iphone-control service install|uninstall|start|stop|restart|status`。
- 控制台上的 AI 任务：输入任务，Claude Code 或 Codex 在手机上完成。随时可以暂停、接管或停止，付款、删除、发送前会先问你。
- 签名证书被吊销时会自动发现并重新签名。
- 控制台跟着切换的设备走，说明没有画面的原因，还能签名并安装 WDA。

### 改进

- 新的控制台：AI 助手、可缩放的实时画面、带 App 和元素的设备面板，中英文都有。
- iOS 27 及以上，画面像触屏一样操作：一次点按约 20 毫秒，原来约 530 毫秒。
- 画面约每秒 50 帧。

### 修复

- iOS 27 以下，控制台的鼠标滚轮恢复可用。
- 新款 iPhone 上 `lock`、`home` 快了很多，界面有动画时点按不再要 26 秒。
- iPad 上 App 切换动画中按的 Home 会再补按一次。
- 重启后，守护进程沿用上次就绪的设备。
- 守护进程还没选好设备时，`iphone-control viewer` 也能启动。
- WDA 报 `unexpected response`、没开 Web Inspector、连不上守护进程时，提示更清楚。
- `iphone_app_logs` 不再返回 Info 级别的日志。

## [0.1.5] - 2026-11-10

### 新功能

- 英文：`IPHONE_CONTROL_LANG=en` 切换工具、命令行、日志和通知的语言。
- `iphone-control viewer`：在浏览器里看着手机操作。
- 开发者工具（`IPHONE_CONTROL_DEV_TOOLS=1`）：App 日志、崩溃报告、安装和卸载 App、Safari 的 Web Inspector。

### 修复

- 所有系统语言下，元素列表都去掉滚动条。
- `iphone_uninstall_app` 拒绝不完整或有歧义的名字。
- 安装 App 和重新签名不再同时上传。
- 守护进程卡死时自己退出，由服务重启。
- 用 HID 后备时内存不再增长，`usbmuxd.log` 会轮转。

## [0.1.4] - 2026-11-07

### 改进

- 不再往 `ICTL_DDI_DIR` 里写东西；只有默认缓存在挂载失败三次后重新下载。
- `debug` 子命令要用 `--features debug-cmds` 编译。

### 修复

- 安全：`zip` 升级到 9.4.1，修复 GHSA-m665-v7pm-7grj 等问题，实际影响很小。
- 连接断开的操作不再经 HID 重做一遍。
- 访问手机、usbmuxd 和苹果服务器都有时限，守护进程不会卡死在这些调用上。
- 重新连接或切换设备后，一分钟内检查是否要重新签名。
- 取消 MCP 调用会真的停下，包括 `iphone_batch` 剩下的步骤；设备变了，批量操作也会停。
- 超时的 HID 会话不再让手机的屏幕流一直开着。
- 旧的 WDA 不会被误判为已就绪，WDA 刚掉线时会如实报告。
- iPad：锁屏时启动 WDA 后自动解锁照常工作，侧栏的行又能按名字点了。
- `doctor` 不再把苹果键盘当成手机。

## [0.1.3] - 2026-11-07

### 升级注意事项

- 自动解锁密码改为按设备保存。升级前只接着存过密码的那台设备，或先清掉密码，再用 `passcode set --device <UDID>` 逐台设置。
- 所有客户端共用设备选择，`select` 的选择在重启后保留。
- 没有 WDA 时，横屏状态下不要把设备转到另一侧横屏。

### 新功能

- 选择要控制的设备：`iphone_list_devices`、`iphone_select_device`，或命令 `devices`、`select`。
- 自动解锁密码按设备保存。

### 修复

- 屏幕尺寸跟着转屏变：截图不再被拉伸、元素不再丢失，iPad 横屏坐标正确。
- HID 后备的点按和滑动在横屏下正确，方向不明时会拒绝。
- Touch ID 设备自动解锁时先点「使用密码」，并核对数字键盘。
- usbmuxd 列表顺序变化时，不再跳到另一台设备。
- `doctor`、`status` 和重新签名读当前设备自己的 WDA 描述文件。
- `setup` 等到设备就绪再签名，`open_url` 等 App 打开后再返回。
- 切换设备后不再沿用上一台的 App 列表和屏幕尺寸。
- `IPHONE_UDID` 指向的设备不在时，不再每 30 秒重启 usbmuxd；容器里能认出第二台设备。

## [0.1.2] - 2026-11-04

### 新功能

- 预编译的 macOS 二进制（Apple Silicon，macOS 11+）。没有公证，见 macOS 指南。

## [0.1.1] - 2026-11-04

### 新功能

- 支持 macOS（Apple Silicon），不需要 Xcode。

### 改进

- 工具说明写清楚每个工具什么时候用、返回什么。
- 发布文件改名为 `iphone-control-vX.Y.Z-linux-<arch>.tar.gz`。

### 修复

- iPhone 12 mini 等手机上 `home`、`lock` 和自动解锁时的唤醒恢复可用。
- 用系统 usbmuxd 时，短暂断开后能自己重连。
- `doctor` 在 macOS 上可用。

## [0.1.0] - 2026-11-02

首个公开版本。

### 新功能

- 守护进程管理手机：配对、开发者模式、开发者镜像、隧道和 WebDriverAgent，拔线、重启后自动恢复。
- MCP 服务，15 个工具：截图、元素、点按、滑动、输入（支持中文）、按键、App、链接、等待和批量操作。
- WDA 不可用时的 HID 后备。
- 用免费 Apple ID 签名 WDA，过期前自动重签。
- Bark、Telegram 通知，可选的自动解锁。
- `setup`、`doctor`、`status`、`mcp-config`。
- Docker 镜像和预编译的 Linux 二进制。

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.2.9...HEAD
[0.2.9]: https://github.com/Derrors/iphone-control/compare/v0.2.8...v0.2.9
[0.2.8]: https://github.com/Derrors/iphone-control/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/Derrors/iphone-control/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/Derrors/iphone-control/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/Derrors/iphone-control/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/Derrors/iphone-control/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/Derrors/iphone-control/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Derrors/iphone-control/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Derrors/iphone-control/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Derrors/iphone-control/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/Derrors/iphone-control/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Derrors/iphone-control/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Derrors/iphone-control/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Derrors/iphone-control/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Derrors/iphone-control/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Derrors/iphone-control/releases/tag/v0.1.0
