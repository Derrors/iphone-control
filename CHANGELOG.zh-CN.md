# 更新日志

[English](CHANGELOG.md) | **简体中文**

格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)（0.x 期间次版本号也可能有不兼容的变化）。和 [CHANGELOG.md](CHANGELOG.md) 一一对应，发布说明从这两个文件里取。

## [Unreleased]

### 新增

- `iphone-control mcp --url https://<那台机器>:8200`：把 stdio 转到另一台机器控制台的 `/mcp`，只能填命令的 MCP 客户端也能操作接在那台机器上的手机。有任务口令就只带它，否则用 `IPHONE_CONTROL_VIEWER_TOKEN` 登录；`task run --url` 启动的命令已经设好 `IPHONE_CONTROL_URL`，里面照常配置的 `iphone-control mcp` 自动转接。
- `task` 的所有子命令都能加 `--url`（或设 `IPHONE_CONTROL_URL`）：`list`、`events`、`pause`、`resume`、`takeover`、`handback`、`stop`、`approve`、`reject` 经另一台机器的控制台操作，登录令牌放在 `IPHONE_CONTROL_VIEWER_TOKEN`。
- 有名字的登录令牌，可以是只读的：`iphone-control viewer token add <名字> [--read-only]`、`list`、`show <名字> [--new]`、`remove <名字>`。只读令牌能看画面、设备状态和任务，所有操作（点按、输入、开任务、暂停、确认、切换设备、`/mcp`）都被拒绝，页面上也不显示操作入口。任务记录里写下是哪个令牌做的控制（`viewer:<名字>`）。令牌改存在 `viewer-tokens.json`，原来的令牌成了 `owner`，升级后不用重新登录；增删、换令牌马上生效，不用重启控制台。
- 从另一台机器开任务：`iphone-control task run --url https://<那台机器>:8200 -- <命令>`（登录令牌放在 `IPHONE_CONTROL_VIEWER_TOKEN`），或者直接调 `POST /api/agent/tasks/open`，返回的事件流就是租约。另一台机器上的 agent 只拿任务口令：`/mcp` 认进行中任务的口令（请求头 `Iphone-Control-Task-Token`），它能为这个任务操作手机，但碰不到暂停、确认这些控制接口，不能批准自己。
- 控制台和它的 `/mcp` 要登录了，本机也一样（控制台分不清本机的浏览器和经 frp、cloudflared 等反向代理转发进来的访问）。令牌第一次启动时生成，存在数据目录的 `viewer-token`（0600）；`iphone-control viewer token` 显示令牌、本机和局域网的登录链接以及接入 HTTP MCP 的命令，`--new` 换一个新令牌。浏览器登录一次后记住 30 天；程序和 MCP 客户端用 `Authorization: Bearer <令牌>`。输错太多次（一分钟 20 次）会暂停校验一分钟。命令行、`iphone-control mcp`（stdio）和 `task run` 走本机的 Unix socket，不受影响。
- MCP over HTTP：控制台同一个端口上的 `/mcp`（Streamable HTTP）提供和 `iphone-control mcp` 一样的工具，只能填 URL 的客户端、局域网里另一台电脑上的 agent（`--lan`，走 HTTPS）都能接入，例如 `claude mcp add --transport http iphone http://127.0.0.1:8200/mcp`。任务里的 agent 在请求头 `Iphone-Control-Task-Token` 里带上任务口令。和控制台一样没有登录，`--mcp off`（`IPHONE_CONTROL_VIEWER_MCP=off`）关掉。
- 控制台对局域网开放（`--lan`）时改用 HTTPS，还是同一个端口：局域网里打开时画面也能用 H.264 或手机自己的 HEVC，不再退回 MJPEG，传输也加了密。默认用本机生成的自签证书（存在数据目录的 `viewer-tls/`，一直沿用），浏览器第一次打开会提示不受信任，核对启动时或 `service status` 显示的 SHA-256 指纹后继续；也可以用 `--tls-cert`/`--tls-key`（`IPHONE_CONTROL_VIEWER_TLS_CERT`/`_KEY`）换成自己的证书。局域网来的 `http://` 跳转到 `https://`，本机的 `http://127.0.0.1` 照旧。`--tls off`（`IPHONE_CONTROL_VIEWER_TLS=off`）恢复成 HTTP。这不是登录：局域网里能连上的人照样能操作手机。
- `iphone-control service viewer install` 把控制台装成登录后自动启动的后台服务（还有 `uninstall`、`start`、`stop`、`restart`）。端口和是否对局域网开放读设置文件（新增 `IPHONE_CONTROL_VIEWER_PORT`；`IPHONE_CONTROL_VIEWER_LAN`；新增 `IPHONE_CONTROL_VIEWER_LAN_HOSTS`），并记下安装时终端的 `PATH` 以找到 `claude` 和 `codex`。升级时先停下它、升级完再启动；有 AI 任务在跑时自动升级会推迟。
- AI 任务的 Agent 不能再自己暂停、继续、接管、交还或确认，越界确认和 `iphone_confirm` 只能由人回答。带任务口令（`IPHONE_CONTROL_TASK_TOKEN`）的控制请求会被拒绝；控制台和 `task run` 启动的 Agent，连同它再启动的进程，也会被拒绝，即使去掉了口令，或者调用控制台的接口。在 macOS 上，调用控制台接口的这条路还查不出是哪个进程。停止任务不受限制。任务记录里会写下谁做了控制（控制台、命令行、任务的发起方）。
- 设置菜单支持方向键选择、Enter 确认和 Esc 取消，保留数字选择；基础终端继续使用逐行提示。
- `iphone-control completions <shell>` 可生成 Bash、Zsh 和 Fish 补全脚本，不启动守护进程、不访问设备。
- `iphone-control viewer --lan`（或 `IPHONE_CONTROL_VIEWER_LAN=1`）让控制台也对局域网开放，还是同一个端口。还没有登录：局域网里任何能连上这台机器的人都能操作手机，只在可信的网络里开。只接受局域网地址来的连接，只认本机自己的名字和地址；`--lan-host` 可以再加名字。从局域网地址打开时画面是 MJPEG，因为浏览器只在 localhost 或 HTTPS 上提供 WebCodecs。
- 控制台上开任务前可以限定 App 范围（Agent 旁边的 App 按钮）：到了别的 App，每一步都先等你确认，和 `task run --app` 一样。任务卡片上显示范围，重新执行时沿用。
- 任何 agent 平台都能用 AI 任务：`iphone-control task run -- <命令>` 在任务里运行命令，它启动的 `iphone-control mcp` 自动加入任务（控制台能看到每一步，可以暂停、接管、停止、确认）。`task list`、`task events`、`task pause`/`resume`/`takeover`/`handback`/`stop`、`task approve`/`reject` 在命令行里查看和控制任务。
- 控制台会接上正在进行的任务：页面刷新后，以及在别处开的任务。
- 任务可以限定 App（`task run --app 设置`）：在别的 App 里操作、启动别的 App、打开网页以外的链接，每一步都先等你确认；同意后这个 App 就加入任务范围。

### 变更

- 控制台的画面只能用上方的加减号缩放：触控板或触屏上双指捏合、按住 Ctrl/⌘ 滚动滚轮都不再缩放画面（拖动时很容易误触成缩放）。
- AI 任务改由守护进程管理，不再由控制台管理。任务进行中，其他客户端（另一个 MCP 服务、命令行）可以看，但操作和切换设备都会被拒绝，用户接管后才放行；任务只操作开始时的那台设备。任务记录在数据目录（`tasks/`，最近 50 个）；守护进程停止时还在进行的任务显示为已中断。

### 移除

- MCP 服务的 `ICTL_AGENT_GATE` / `ICTL_AGENT_TOKEN` 环境变量：任务里的 MCP 服务改用 `IPHONE_CONTROL_TASK_TOKEN`。

### 修复

- 升级时检查新程序（运行它的 `--version`）偶尔会因为「Text file busy」失败：同一进程的其他线程恰好在新程序还以写方式打开时启动了子进程。现在遇到这种情况会稍等重试。
- `task run`：命令自己用 `task finish` 结束任务后，不再在它正常退出时被立刻停掉、退出码变成 1。现在它有 5 秒自己退出，退出码就是它自己的；超时才停掉，这时任务完成则退出码为 0。
- 在设置菜单中长按方向键时，选项会连续移动，按键序列不会回显到菜单里。
- 控制台上 AI 任务的总结按格式显示：粗体、列表、行内代码和换行不再显示成 Markdown 原文。
- iOS 27 及以上，控制台实时画面里看不到的系统弹窗（例如「允许“日历”使用你的位置？」这类权限请求）现在画在画面里它所在的位置，带着弹窗的按钮，点按钮就是点手机上的那个按钮。
- 部分 iPhone（如 iPhone 15 Pro Max，iOS 27.0.1）上锁屏不再每次要 10 秒左右：经 IOHID 短按电源键在这些设备上不起作用，守护进程会按设备记住，之后先用 WDA 的锁屏（约 2 秒）。
- iOS 27 以下的 iPad 现在能锁屏了（iPad mini 5，iPadOS 26.7.3：一直锁不上），iPhone 15 Pro Max 第一次锁屏也从约 10 秒变成约 2 秒：经 IOHID 按电源键改成按 0.15 秒，原来的 0.1 秒这些设备不认。
- WDA 还应答 `/status`、经 XCTest 的请求却全都不应答（前台 App、截图、读元素、锁屏状态全部超时）时，现在约 30 秒内就会发现并重启它，工具调用接连不断时也一样（经 XCTest 的请求连续 3 次没有应答后，不再一次次等满时限）。以前守护进程一直显示「就绪」，后台模式（`ICTL_WDA_DETACHED`）下连重启守护进程都会把卡死的 WDA 接回来。
- iOS 27 以下的设备上，WDA 没能锁屏时（经 IOHID 按电源键、WDA 自己的锁屏都没锁上）报的是「WDA 不可用（刚掉线）」，其实 WDA 好好的。现在如实说两种锁屏办法各自的结果。

## [0.2.3] - 2026-11-13

### 升级注意事项

- 从 0.2.2 升级：0.2.2 的命令行还没有不带子命令的 `update`。签名安装脚本装的受管服务运行 `iphone-control update apply`；Homebrew 运行 `brew upgrade iphone-control`；Debian / Ubuntu 从发布页下载对应架构的 `.deb`，用 `sudo apt install ./<文件名>` 安装。升级到 0.2.3 之后，直接运行 `iphone-control update` 即可。

### 新增

- 运行 `iphone-control update` 可检查并通过受管服务、Homebrew 或 Debian 软件包升级到新稳定版。已是最新时会直接提示。只想检查可继续用 `update check`。
- `iphone-control update` 安装前先说明要做什么并询问，`--yes` 跳过询问。Homebrew 和 Debian 升级后，正在运行的后台服务会自动重启到新版本。
- Debian 软件包也有发布者签名，`iphone-control update` 验证签名后才用 sudo 交给 APT 安装。APT 里锁定（hold）的包会提示先解除锁定。

### 移除

- `iphone-control update rollback` 和升级后保留的上一版程序。新版本起不来时仍会自动换回旧版本。
- 发布清单里的回滚兼容标记：任何更新的签名稳定版都能直接升级，不再因“需要手动迁移”而停下。

### 修复

- 控制台开着、浏览器解不了 HEVC（Firefox、多数 Linux 上的 Chrome）或设备低于 iOS 27 时，WDA 不再每 1～2 分钟被 iOS 杀掉一次：画面改成原尺寸、横屏时由页面转正，WDA 直接转发手机硬件编好的截图，不再逐帧缩放、旋转、重新编码，CPU 不再超出 iOS 的上限。
- `iphone-control status | head -1` 这类把输出接给提前退出的程序的用法，不再报 “Broken pipe” 崩溃，而是像其他命令行工具一样直接结束。
- 控制台的触控恢复灵敏：按下时不再每次都向 WDA 量屏幕尺寸（每次约 0.1～0.2 秒，0.2.2 引入），只在转屏时和最多每分钟量一次。鼠标在画面上移动时也保持触控会话打开，停顿后的第一下不用再等约 0.5 秒重开。

## [0.2.2] - 2026-11-12

### 升级注意事项

- 使用脚本新装时，请从发布者的已签名 GitHub Release 下载 `install.sh`，需要 Python 3 和 OpenSSL 3。旧版脚本用户请先停止服务，用原安装脚本卸载，再运行新脚本。以 0.2.0 为例，从原发布仓库的 `v0.2.0` 标签取出旧脚本，传入 `--uninstall` 和原安装前缀。设置和设备数据都会保留。

### 新增

- 运行 `iphone-control update check` 或打开控制台的「关于与更新」，可查看当前构建的发布者提供的最新稳定版。发现新版后，Viewer 会在按钮上显示红点。打开面板即可查看版本号，并在有适用命令时一键复制。
- 通过签名发行包的安装脚本部署的用户服务可用 `iphone-control update apply` 升级，用 `iphone-control update rollback` 回滚。默认仅检查更新。运行 `iphone-control update policy auto-patch` 后，服务会在空闲时自动安装签名补丁，新守护进程健康检查失败时恢复旧版本。
- 控制台改版：工作区适配宽窄窗口，侧栏和浮层用磨砂表面（高对比度或减少透明度时为实色），任务输入、执行器和设置集中在侧栏底部。设备显示商品名（如 iPhone 15 Pro，而不是 `iPhone16,1`）并套用 iPhone 或 iPad 外框；状态栏区分等待设备、正在连接、仅可观看、实时画面和画面断开。

### 修复

- 控制台的点击、滑动和实时触控绑定到你看到的设备和画面：换机、转屏或断流后会被拒绝，不再点到旧位置。WDA 偶尔一次没响应不再重置画面、也不会在拖动中途松开手指；没有 `/rotation` 的 WDA 改从 SpringBoard 读方向。
- 关闭或冻结的标签页留下的画面连接会被释放，反复重连不再占满控制台的连接数。

## [0.2.1] - 2026-11-12

### 新增

- iOS 27 及以上，控制台的画面直接用手机自己硬件编码的视频（CoreDevice 屏幕媒体流，HEVC）：约每秒 60 帧、原生分辨率，本机不编码，WDA 也不用截图；转屏时画面跟着转正。浏览器解不了 HEVC（Firefox、多数 Linux 上的 Chrome）或设备低于 iOS 27 时自动改用 H.264 或 MJPEG。`IPHONE_CONTROL_VIEWER_CODEC=device` 只用这条流。
- 控制台的画面改用 H.264，流量约为原来的 1/15：macOS 上本程序直接调 VideoToolbox（JPEG 也由硬件解码），不需要 ffmpeg；Linux 上装了 ffmpeg 时显卡编码优先（NVENC、VAAPI、QSV、V4L2，都不行用 x264）；链路慢时降帧率，画面不会越来越滞后。没有 ffmpeg 时仍是 MJPEG，静止时不再重复发同一张。设置：`IPHONE_CONTROL_VIEWER_CODEC`、`IPHONE_CONTROL_VIEWER_QP`。
- 实验开关 `ICTL_WDA_DETACHED=1`：在后台启动 WebDriverAgent、不持有 XCTest 会话，锁屏时也能启动，守护进程重启时它不跟着断。见 [WDA 依赖调研](docs/dev/wda-dependency-research.md)。

### 修复

- 控制台开着时 WDA 不再每隔几分钟被 iOS 杀掉（iOS 27 及以上）：WDA 一直截图给画面用，CPU 超过一个核，iOS 按一分钟平均超过 80% 杀掉它，守护进程随之重启 WDA，期间操作失败。改用手机自己的视频后 WDA 的 CPU 约 3%。iOS 27 以下仍靠 WDA 截图，不看时请关掉控制台页面。

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

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.2.3...HEAD
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
