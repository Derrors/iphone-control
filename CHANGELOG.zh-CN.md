# 更新日志

[English](CHANGELOG.md) | **简体中文**

格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)（0.x 期间次版本号也可能有不兼容的变化）。和 [CHANGELOG.md](CHANGELOG.md) 一一对应，发布说明从这两个文件里取。

## [Unreleased]

### 新增

- 实验开关 `ICTL_WDA_DETACHED=1`：在后台启动 WebDriverAgent、不持有 XCTest 会话，锁屏时也能启动，守护进程重启时它不跟着断。见 [WDA 依赖调研](docs/dev/wda-dependency-research.md)。
- 运行 `iphone-control update check` 或打开控制台的「关于与更新」，可查看当前构建的发布者提供的最新稳定版，以及适合本机安装方式的升级步骤。
- 通过签名安装脚本部署的用户服务可用 `iphone-control update apply` 升级，用 `iphone-control update rollback` 回滚。默认仅检查更新。运行 `iphone-control update policy auto-patch` 后，服务会在空闲时自动安装签名补丁，新守护进程健康检查失败时恢复旧版本。

### 升级注意事项

- 使用脚本新装时，请从发布者的已签名 GitHub Release 下载 `install.sh`，需要 Python 3 和 OpenSSL 3。旧版脚本用户请先停止服务，用原安装脚本卸载，再运行新版签名安装脚本。设置和设备数据都会保留。

## [0.2.0] - 2026-11-11

### 升级注意事项

- 不再发布 Docker：ghcr 上不再有镜像，也没有 `compose.yaml`。请改用一行命令的 `install.sh`、Homebrew 或 `.deb`（见「新增」）。先停掉容器（`docker compose down`），免得它占着手机。
- 原来的数据在容器挂的卷里（`compose.yaml` 旁边的 `./data`，属于 root）。第一次运行前把它拷到 `~/.local/share/iphone-control` 并改成自己的（`sudo cp -a ./data ~/.local/share/iphone-control && sudo chown -R "$(id -un)": ~/.local/share/iphone-control`，见[数据与备份](https://derrors.github.io/iphone-control/deploy/backup)），Apple ID 会话、签好的 WebDriverAgent 和自动解锁密码就都还在。配对记录现在由系统的 usbmuxd 保存，手机可能会再问一次是否信任这台电脑。
- 设置从 `.env` 搬到 `~/.config/iphone-control/env`（同样是 `KEY=value`，权限 0600）。首次运行引导会写好它；也可以把原来的值抄过去，再运行 `iphone-control service restart`。
- `ICTL_MANAGE_USBMUXD`、`ICTL_USBMUXD_BIN` 已删除：守护进程不再自己启动 usbmuxd。Linux 上请装系统的 usbmuxd（`sudo apt install usbmuxd`，`.deb` 会自动装上）；以前为容器 mask 过的，要 `sudo systemctl unmask usbmuxd`。
- 重新注册 MCP：原来 `docker exec` 形式的注册不能用了。首次运行引导会注册，或者运行 `iphone-control mcp-config` 打印的命令。

### 新增

- 首次运行引导：装好后接上手机，运行 `iphone-control`。第一次会在终端里一步步带你完成设置：语言、代理、后台服务、配对、开发者模式、Apple ID、签名并安装 WebDriverAgent，可选的自动解锁密码和通知（Bark 或 Telegram，发一条测试消息），以及把 MCP 注册到 Claude Code 和 Codex。做过的步骤会跳过；设置好以后再运行 `iphone-control` 只显示状态，后台服务没在运行时先启动它。
- `iphone-control setup --reconfigure`：全部重新设置，或只改一项：语言、代理、后台服务、Apple ID（重新登录并重签 WebDriverAgent）、手机、自动解锁密码、通知、注册 MCP。
- `iphone-control service install|uninstall|start|stop|restart|status` 管理后台服务（macOS 是 launchd，Linux 是 systemd 用户服务），不管是 `install.sh`、`brew services` 还是 `.deb` 装的。
- 设置文件 `~/.config/iphone-control/env`：后台服务读它，每个命令启动时也读它（环境里已经设了的变量优先）。手改后运行 `iphone-control service restart`。
- 每个发布的压缩包和 `.deb` 里都带 `THIRD_PARTY_LICENSES.html`（所有 Rust 依赖的许可证）。
- 控制台跟着守护进程控制的设备走：在别处切换了设备（另一个标签页、`iphone-control select`、MCP 工具，或拔掉后自动换了一台），页面约 10 秒内发现、提示，并把画面和各面板换过去。操作带着页面显示的那台设备，守护进程已经在控制别的设备时会被拒绝，不会发到另一部手机上。
- 控制台没有画面时说明原因（没连上、要配对或开开发者模式、WebDriverAgent 正在启动或启动失败的原因、要先解锁）；没装 WebDriverAgent 的设备，登录了 Apple ID 时会显示「签名并安装 WDA」按钮。
- 签名证书被吊销时自动恢复：免费 Apple ID 同时最多两张证书，在别处（换机器、从 Docker 换成本机安装、别的侧载工具）用同一个 Apple ID 签名会新建证书并吊销一张旧的，用它签的 WDA 在每台设备上都起不来，描述文件却看着还有效。现在设备拒绝启动 WDA 时，守护进程会检查设备上的 WDA 是不是本机证书签的、本机证书还在不在 Apple ID 的团队里，有问题就自动重签这台（同一台 6 小时内最多一次）。
- `doctor` 显示这两项证书检查；签名时新建或吊销证书会写日志并发通知（新的通知类型 `cert`，`notify-test cert` 可以试发）。
- 安装更简单：macOS（Apple Silicon）和 Linux 上 `curl -fsSL https://raw.githubusercontent.com/Derrors/iphone-control/main/install.sh | sh` 一行装好 Release 的程序，并把守护进程装成用户服务（macOS 用 launchd，以前没有；Linux 用 systemd），再运行一次就是升级，也能卸载；`brew install derrors/tap/iphone-control` 加 `brew services start iphone-control`；Debian 12+ 和 Ubuntu 22.04+ 的 `.deb`（amd64、arm64），带 systemd 用户服务、依赖 usbmuxd。发布流程会打好这些包并更新 Homebrew 源。
- 实时画面页面里的 AI 任务：在页面上写下任务，本机的 agent（Claude Code 或 Codex，在页面上选）用 MCP 工具在手机上完成；页面逐步显示 agent 说它要做什么、结果和截图，并在画面上标出点击位置。可以暂停、继续、接管（自己动手）后交还，或者停止。付款、删除、发消息、改账号设置之前，agent 会在页面上请你确认（`iphone_confirm`）并等你回答，拒绝的那一步不会执行。步骤、暂停和确认都经过 `iphone-control mcp` 本身，所以两种 agent 表现一样；agent 只能用操作手机的工具（Claude Code 不带命令行和文件工具，Codex 用只读沙箱）。

### 变更

- 新的实时画面页面：AI 助手、可缩放的实时画面，以及设备面板（状态和就绪检查、已安装的 App（启动、结束、打开链接）、屏幕上的元素（在画面上框出、按名字点、等文字出现））。可以在页面上切换设备，画面跟着换。AI 任务执行中手动操作会被拒绝，接管后才能操作。
- iOS 27 起，viewer 的画面像触摸屏一样用：按下、移动、松开鼠标，手机上的手指跟着动（经 CoreDevice HID），拖动时列表跟着滚；在画面上滚动鼠标滚轮或触控板，不用按住鼠标就能滚动光标下的页面。点击往返从约 530 毫秒降到约 20 毫秒，拖动不再等列表停下（原来 1.8～3 秒）。iOS 27 以下仍在松开后发整段手势。用 viewer 操作时，HID 会话的屏幕媒体流会让手机保持亮屏，最后一次触摸 30 秒后关闭。
- 实时画面约每秒 50 帧、75% 尺寸（WDA 默认每秒 10 帧、原尺寸）。
- viewer 页面有中英文：默认跟着 `IPHONE_CONTROL_LANG`，页面上有按钮切换。守护进程的提示和 AI 任务（给 agent 的说明、工具结果）都跟着页面的语言。
- viewer 的执行记录每一步只显示一句看得懂的话（agent 说它要做什么，没说时按操作写一句），不再显示截图、工具名、参数和原始结果；没成功的一步注明「这一步没成功」。agent 工具的截图也不再传给 viewer。
- viewer 不再给自己的点击、滑动画轨迹，手动操作成功时也不再弹提示；出错照样提示。
- viewer 新增只读的 JSON 接口（`/api/daemon`、`/status`、`/devices`、`/apps`、`/elements`），并拒绝浏览器标明来自其他网站的读取（`Sec-Fetch-Site`），因为读元素可能会解锁手机。

### 移除

- Docker 镜像（`ghcr.io/derrors/iphone-control`）、`compose.yaml`、`docker/Dockerfile` 和 `.dockerignore`。容器够不到控制台要交任务的本机 Claude Code 或 Codex，原生安装也更简单。
- 守护进程不再启动、看护 usbmuxd（`ICTL_MANAGE_USBMUXD`、`ICTL_USBMUXD_BIN`），Linux 和 macOS 都用系统的 usbmuxd。
- `deploy/iphone-control.service`：改由 `install.sh` 和 `iphone-control service install` 写服务文件。
- `iphone-control mcp-config --container`：`mcp-config` 不再带参数，打印的路径在 Homebrew 升级后也有效。

### 修复

- 控制台：iOS 27 以下的设备（整段手势模式）上，滚轮滚动不再没反应。原来滚轮一直在等一个这种模式下永远不会归零的剩余量，滑动发不出去，之后的滚轮也一起积压。
- 刚启动 App、转场动画还没走完时按 Home，iPadOS 会把两种按法都忽略（iPad mini 5 / iPadOS 26.6.1 上约三次有一次）。现在两种都没回到主屏时，会再补按一次。
- WebDriverAgent 启动失败、只报 `unexpected response` 时，提示里加上第二种可能：已经信任过开发者的，可能是签名证书变了，运行 `iphone-control sign` 重签这台设备。控制台在这种情况下，登录了 Apple ID 时会给出「重新签名并安装 WDA」按钮。
- 自动选择设备时，守护进程重启后接着用上次就绪过的那台，不再按 usbmuxd 列表的顺序换到另一台（接着 iPhone 和 iPad 时，重启后可能换到一台起不来 WebDriverAgent 的设备）。
- `iphone-control viewer` 在守护进程还没起来、又接着多台设备时不再直接退出：先照常启动，守护进程选好设备后页面自动跟上。
- 控制台：iOS 27 以下的设备在 WebDriverAgent 没起来时，「操作通道」显示「不可用」，不再说改走 HID 后备（HID 后备要 iOS 27）；不知道锁没锁时不再显示「未锁定」；WebDriverAgent 启动失败时画面区域显示失败原因，不再一直说「通常 10–60 秒」。
- 日志和状态里的「WDA 启动失败：WDA 启动失败：」不再重复。
- 命令行里 `devices`、`select`、`doctor` 的提示同时给出 `iphone-control select` / `iphone-control status`，不再只写 MCP 工具名；连不上守护进程时提示 `iphone-control service start`。
- 新款 iPhone 上 `lock` 不再要 10 秒左右：电源键改为按 0.1 秒，不再按住 0.5 秒。iPhone 18 Pro Max（iOS 27.2.1）不认按住，每次锁屏都退到 WebDriverAgent 较慢的锁屏，还在日志里记一次超时。
- 不认 IOHID Home 事件的设备（iPhone 18 Pro Max，iOS 27.2.1）上 `home` 从约 3.7 秒变为约 1.3 秒：只有 XCTest 的按法有效时，守护进程按设备记住，之后先用它。只认 IOHID 按法的设备（iPhone 12 mini）顺序不变。
- 界面上一直有动画时，点击和长按不再要 26 秒左右（iPhone 18 Pro Max、iOS 27.2.1 上，键盘弹着时在设置里点搜索结果每次都这样）：和滑动一样，不再等 WebDriverAgent 的动画冷却。同一个点击现在约 1.4 秒。
- 手机断开 Web Inspector 连接时，`iphone_web_targets`、`iphone_web_eval` 提示去打开 设置 → Safari → 高级 → 网页检查器，不再只给一句 TLS 错误。
- `iphone_app_logs` 不再返回 Info 级别的行，和工具说明一致（不带调试消息的日志流里，有些进程仍会发 Info）。
- 英文的设备说明在括号前、逗号后加了空格：`name (model iOS version, UDID)`。

## [0.1.5] - 2026-11-10

### 新增

- 英文界面：设 `IPHONE_CONTROL_LANG=en` 后，工具说明和结果、命令行输出和帮助、`status`/`doctor`、日志、通知都改成英文。默认仍是简体中文，旧版本客户端照旧得到中文。
- `iphone-control viewer [--port]`：在浏览器里看实时画面并操作手机（默认 127.0.0.1:8200，显式开启）。本机其他用户能连上这个端口，用完就停。
- 开发者工具，设 `IPHONE_CONTROL_DEV_TOOLS=1` 开启：`iphone_app_logs`、`iphone_crash_reports`、`iphone_install_app`、`iphone_uninstall_app`、`iphone_web_targets`、`iphone_web_eval`。不设时工具列表不变。

### 修复

- 任何系统语言下元素列表都会去掉滚动条，不再只认中文和英文。
- `iphone_uninstall_app` 只接受 Bundle ID 或完整的显示名；空的、不完整的、对应多个 App 的名字都拒绝，并列出候选。
- 安装 App 和重签互相排队，不再同时往手机上传。
- 守护进程卡死时会自己退出，由 systemd 或 Docker 重启。
- HID 后备期间内存不再增长。
- `usbmuxd.log` 过大时会轮转。

## [0.1.4] - 2026-11-07

### 安全

- `zip` 升级到 9.4.1（原来是 8.6.0，vendor 的 apple-codesign 里是 2.6.1），修复 GHSA-m665-v7pm-7grj、GHSA-q7v9-x93c-xqv3、GHSA-hjqx-j4rw-g7xv、GHSA-9hj8-73fh-9963、GHSA-8hxf-qr8p-62x4。iphone-control 只解压核对过 sha256 的 Appium 官方 WDA 包，实际受影响的可能很小。
- 文档站的构建工具改用 vite 6.4.5、esbuild 0.25（只是开发依赖，发布的网站不受影响）。

### 变更

- `ICTL_DDI_DIR`（自己提取的开发者镜像目录）不再往里下载，也不会删除里面的文件；缺文件时报出缺哪个。只有默认缓存 `ddi/` 在连续 3 次**挂载**失败后才清掉重新下载（建不起隧道不再计入）。
- `debug` 子命令默认不再编译，需要时用 `--features debug-cmds` 编译。
- 工具描述写明：配置了自动解锁时，截图、`iphone_list_elements`、`iphone_wait_for` 也会先解锁；HID 后备需要 iOS 27。

### 修复

- WebDriverAgent 请求已经发出、之后连接断开或响应不完整时，不再改用 HID 重做一遍（点击、输入可能执行两次），改为提示手机可能已经执行。
- 对 usbmuxd、lockdown、AMFI、misagent、installation_proxy 的调用都加了时限：手机或 usbmuxd 接了连接却不应答时，守护进程不会再卡死。
- 重签有总时限（15 分钟），苹果服务器或安装卡住时不再一直“正在重签”；Apple ID 会话检查也有时限。手机重新接上或切换设备后，1 分钟内就检查是否需要重签，不用等满 `RESIGN_CHECK_HOURS`。给一台设备重签完成时，不再拆掉期间切换过去的另一台的会话。
- 手机重启后还没解锁、WDA 起不来时，`status` 一直显示需要解锁手机，每 20 秒重试一次，不再在“正在启动”和它之间来回切、每 10 秒重试。
- 开启 HID 会话超时时，不再把屏幕媒体流留在手机上（不自动锁屏、灵动岛图标常驻）。
- 取消 MCP 调用后真的会停下：`iphone_batch` 剩下的步骤不再执行，还在排队等设备的请求也会放弃。
- `iphone_batch` 执行中控制的设备变了（例如拔掉后自动改控另一台）就停下。
- 新启动的 WebDriverAgent 要等测试计划开始后才算就绪，手机上残留的旧 WDA 不会让它提前被当成就绪。
- `doctor` 只把 iPhone、iPad、iPod 算作 USB 上的手机，接着苹果键盘时不再误报。
- 点到的元素文字里正好含“系统弹窗”时，操作后的弹窗检查不再被跳过。
- 自动解锁也能认出滑动、轮询中途被打断后残留的 WDA 等待设置，并恢复默认值。
- iPad：WebDriverAgent 在 iPad 锁屏时启动会一直报“未锁”，而 Touch ID 机型熄屏、有通知时的锁屏上又没有锁的标记，以前会因此一直相信 WDA、永远不自动解锁。现在认不出时按可能锁着处理：配置了密码就自动解锁，`status` 显示“可能已锁屏”。
- iPad：设置等 iPadOS 26 App 的浮动侧栏里的项目又能按名字点了，以前把列表背后和侧栏一样大的空容器误当成遮挡。
- WDA 在两次巡检之间掉线时，提示改为“刚掉线”，不再显示成“就绪”。

## [0.1.3] - 2026-11-07

### 升级注意事项

- 自动解锁密码改为按设备保存。升级前只连接旧版保存密码所对应的那台设备：仅连接一台时，旧密码会自动归到它名下。若不确定旧密码属于哪台，请先用旧版 `passcode clear` 清除，再升级并分别运行 `passcode set --device <UDID>` 设置。操作前先备份数据目录。
- 所有客户端共享同一台被控设备。显式 `select` 选择会跨守护进程重启保留；自动模式只在进程运行期间、当前设备不断开的情况下保持选择。
- 横屏 HID 后备触摸需要 30 秒内且匹配的 WDA 旋转角。WDA 不可用期间不要从一个横屏方向翻到另一侧；请等 WDA 恢复后再翻转。

### 新增

- 接着多台 iPhone/iPad 时可以选择控制哪一台：新增 MCP 工具 `iphone_list_devices`、`iphone_select_device`，命令 `devices`、`select`。选定的设备写进 state.json，重启后还记得；`select auto` 改回自动选。设了 `IPHONE_UDID` 时以它为准、不能切换。
- 自动解锁密码按设备分开存（`secrets/passcodes/<UDID>`），`passcode set/clear` 可加 `--device`。旧版本的那份密码在 USB 上只接着一台时自动归到那台名下，接着多台时不会用。

### 修复

- 转屏后实时更新屏幕尺寸，避免截图拉伸和屏幕下部元素被过滤；将 iPad 横屏系统界面的竖屏坐标换算为屏幕坐标，按状态栏位置区分两个横屏方向。
- Touch ID 设备自动解锁会先点“使用密码”；补全锁屏识别及 iPad 删除键定位，并在输入前校验数字键布局。
- 经 misagent 读取当前设备已安装的 WDA 描述文件，供 `doctor`、`status` 和自动重签判断到期时间，避免混用另一台设备的签名记录。
- `setup` 在签名安装前等待守护进程识别目标设备；修正设备离线和 iOS 低于 27 时的提示，并为 WDA 启动的笼统错误补充首次安装时信任开发者的排查提示。
- `open_url` 等待前台 App 稳定后返回，减少冷启动时截到黑屏或把短暂系统服务当成目标 App 的情况。
- HID 后备路线的点击、滑动会把横屏坐标换算回设备原生的竖屏坐标；持久化尺寸和复用的 HID 会话都按竖屏归一化。每次后备触摸前重新截图核对方向；横屏还需 30 秒内且匹配的 WDA 旋转角，缺失时（包括守护进程刚重启）拒绝触摸。WDA 不可用期间不要从一个横屏方向翻到另一个：仅凭截图宽高无法识别 180° 翻转。
- 没设 `IPHONE_UDID`、接着多台时，usbmuxd 列表顺序变了会悄悄换成控制另一台（自动解锁可能把一台的密码输进另一台）。现在正在控制的那台一直用到它断开。
- 设了 `IPHONE_UDID` 而那台没接、另一台接着时，usbmuxd 每 30 秒被重启一次。
- 容器里已经接着一台时再插一台，usbmuxd 认不到新的那台（没有 udev，收不到热插拔），也不会被重启。现在比较 USB 上和 usbmuxd 认到的 iPhone/iPad 台数。
- 换设备后还用上一台的 App 列表缓存和屏幕尺寸。

## [0.1.2] - 2026-11-04

### 新增

- Releases 里提供 macOS 预编译二进制（Apple Silicon，macOS 11 以上）：`iphone-control-vX.Y.Z-macos-arm64.tar.gz`。没有经过苹果公证，被 Gatekeeper 拦下时见 macOS 指南。

## [0.1.1] - 2026-11-04

### 新增

- 支持 macOS（Apple Silicon）：从源码编译，以普通用户运行，用系统自带的 usbmuxd，不需要 Xcode。见新增的 macOS 使用指南。

### 变更

- MCP 工具描述说明了何时用、返回什么、具体行为，每个参数都有说明，每个工具都带 MCP 注解。工具名、参数和返回结果不变。
- Release 里的二进制压缩包改名为 `iphone-control-vX.Y.Z-linux-<架构>.tar.gz`。

### 修复

- 部分手机上 `home`、`lock` 没有效果（在 iPhone 12 mini、iOS 26.4.1 上遇到），自动解锁也点不亮熄灭的屏幕。
- 用系统 usbmuxd 的部署（macOS、Linux 从源码运行）断开几秒后会卡在「bad device」，现在能自己重连。
- macOS 上 `doctor` 不再误报「USB 上没有 Apple 设备」，修复提示改用 macOS 的命令。
- 自动解锁调不出密码键盘时，提示里说明待机显示会挡住键盘。

## [0.1.0] - 2026-11-02

首个公开版本。

### 新增

- 守护进程独占手机：usbmuxd 看护、配对和开发者模式检查、开发者镜像下载挂载、启动 WebDriverAgent；拔插、usbmuxd 重启、手机重启后自动恢复。
- MCP 服务，15 个工具：截图（可放大区域）、元素列表、点击、长按、滑动、输入（支持中文）、按键、App 启动/结束、打开链接、`iphone_wait_for`、`iphone_batch`；操作类工具可用 `then` 顺带返回操作后的界面。
- WebDriverAgent 不可用时，点击、滑动、按键、截图经 HID 后备完成。
- 用免费 Apple ID 签名 WebDriverAgent（支持双重认证），7 天描述文件到期前自动重签，每天检查 Apple ID 会话。
- Bark、Telegram 通知；可选的自动解锁（输错一次即停用）。
- `setup`、`doctor`、`status`、`mcp-config`、`wda-bridge`、`notify-test` 命令。
- Docker 镜像（非 privileged，linux/amd64、linux/arm64）；也可不用 Docker，从源码或预编译二进制以普通用户运行，附 systemd 用户服务。

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Derrors/iphone-control/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/Derrors/iphone-control/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Derrors/iphone-control/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Derrors/iphone-control/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Derrors/iphone-control/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Derrors/iphone-control/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Derrors/iphone-control/releases/tag/v0.1.0
