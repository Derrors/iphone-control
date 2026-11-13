# 命令行

所有命令都是 `iphone-control <命令>`。`--help` 看帮助，`--version` 看版本。

不带命令只运行 `iphone-control`：第一次是[首次运行引导](./getting-started#first-run)，设置好以后显示状态（守护进程没在运行时先启动它）。

## 全局选项

写在命令前面，例如 `iphone-control --socket /path/ctl.sock mcp`。

| 选项 | 环境变量 | 含义 |
|---|---|---|
| `--data-dir <目录>` | `ICTL_DATA_DIR` | 数据目录，默认值见[配置](./configuration#目录和控制接口) |
| `--socket <路径>` | `ICTL_SOCKET` | 守护进程的控制接口，默认值同上 |

## 运行

| 命令 | 作用 |
|---|---|
| `daemon` | 常驻守护进程。一般由后台服务启动，也可以在终端里手动运行 |
| `mcp` | MCP stdio 服务，由 AI 客户端拉起 |
| `mcp-config` | 打印把 MCP 注册到 Claude Code 的命令。程序路径用升级后不变的那个（Homebrew 用 `opt` 下的链接） |

## Shell 补全

`iphone-control completions <bash|zsh|fish>` 将补全脚本写到标准输出，包含当前构建的子命令、选项和枚举值。它不读取设置文件、不申请设备维护锁，也不连接守护进程或手机。生成的帮助文字默认中文；需要英文时，在命令前设置 `IPHONE_CONTROL_LANG=en`。

```bash
# Bash：当前会话启用
mkdir -p ~/.config/iphone-control
iphone-control completions bash > ~/.config/iphone-control/completion.bash
source ~/.config/iphone-control/completion.bash

# Zsh：保存到自定义补全目录
mkdir -p ~/.zsh/completions
iphone-control completions zsh > ~/.zsh/completions/_iphone-control

# Fish：自动加载目录
mkdir -p ~/.config/fish/completions
iphone-control completions fish > ~/.config/fish/completions/iphone-control.fish
```

Zsh 在 `~/.zshrc` 的 `compinit` 之前添加 `fpath=(~/.zsh/completions $fpath)`，并确保有 `autoload -Uz compinit; compinit`。Bash 若要每次启动生效，可将上面的 `source` 行放进 `~/.bashrc`。升级程序后，重新生成所用 shell 的补全文件。自定义 Fish 配置目录时使用对应的 `completions` 子目录。

## 主机软件更新

运行 `iphone-control update` 检查发布者的最新稳定版。已是最新时，命令会显示结果并退出。有新版时，它先说明要做什么（从哪个版本升到哪个版本、会不会重启后台服务），问你是否继续，再按当前程序的安装方式升级。脚本里或没有终端时加 `--yes`，不询问直接升级。

| 安装方式 | 运行 `iphone-control update` 后 |
| --- | --- |
| 签名安装脚本部署的受管用户服务 | 验证发布者签名后安装，重启服务并检查健康状态；新版本 60 秒内起不来就自动换回旧版本。先关闭 Viewer、MCP 和其他设备客户端。通过 `--version` 固定版本的安装，须先显式运行 `iphone-control update unpin` 才能升级。 |
| Homebrew | 通过该安装绑定的 tap 运行 Homebrew 升级，以 tap 提供的版本为准。`brew services` 在运行的话，升级后重启它。 |
| Debian / Ubuntu 软件包 | 下载同一发布者提供的、匹配目标版本和主机架构的 `.deb`，验证发布者对它的签名后，用 `sudo` 交给 apt 安装（会询问密码）。deb 带的 systemd 用户服务在运行的话，升级后重启它。用 `apt-mark hold` 锁定了的不会升级，先 `sudo apt-mark unhold iphone-control`。 |
| 源码安装、没有受管服务的脚本安装或无法识别的安装 | 显示适用的手动升级指引。未绑定发布者的开发构建在本地给出说明。 |

重启后台服务会中断正在执行的 AI 任务和控制台画面。Homebrew 和 apt 升级后，命令会确认后台服务已经在运行新版本；没能确认时，它会写出要手动运行的重启命令。重新打开 Viewer、重连 MCP 客户端后，这些进程也会使用新版。

只想查看版本可运行 `iphone-control update check`，脚本可用 `iphone-control update check --json` 读取结果。该检查与 Viewer「关于与更新」共用本地缓存，网络失败时保留上次成功的记录。Viewer 在「设置」旁的 ⓘ 上用红点提示新版本。打开面板可查看发行说明，并复制针对当前程序路径的更新命令，在本机终端执行。

Viewer 只监听本机回环地址。同机其他用户也能查看安装信息、触发版本检查。安装和更改策略都通过本机 CLI 执行。

受管服务默认只做后台检查。`update apply` 不询问直接安装（受管服务），替换意外中断后用 `update recover`。没有回退到上一版的命令：要换回旧版本，先用发行页的安装脚本加 `--uninstall` 卸载（设置和设备数据保留），再加 `--version vX.Y.Z` 安装那个版本。如果正在使用 0.2.2 的 CLI，先运行 `iphone-control update check` 并按该版本显示的命令升级。新版 CLI 才支持直接运行 `iphone-control update`。

主动运行 `iphone-control update policy auto-patch` 后，服务只在空闲时自动安装兼容补丁。请退出 Viewer、MCP 和直连设备的客户端；暂停的任务也视为占用。新 daemon 健康检查失败会恢复旧程序，失败版本须手动重试。`update policy notify` 改回仅检查，`update policy manual` 移除定时任务。

如果新程序无法启动，直接用安装目录中的备份：`<prefix>/bin/.iphone-control-update/recovery update recover`。自定义 socket 时，在 `update` 之前加 `--socket <路径>`。

## 后台服务

守护进程装成用户服务：macOS 是 launchd，Linux 是 systemd 用户服务。登录后自动启动，崩溃后自动重启。不用 root。

| 命令 | 作用 |
|---|---|
| `service install` | 装成后台服务并启动；已经有服务时直接启动 |
| `service uninstall` | 停止并移除本程序装的服务，数据保留。Homebrew 或 deb 包带的服务不删，只停用或提示怎么停 |
| `service start` / `stop` | 启动、停止 |
| `service restart` | 重启，改过 `~/.config/iphone-control/env` 后用 |
| `service status` | 装的是哪个服务、守护进程在不在运行，控制台服务装了没有、在不在运行 |
| `service viewer install` | 把控制台（`viewer`）也装成登录后自动启动的服务并启动。端口、是否对局域网开放读设置文件里的 `IPHONE_CONTROL_VIEWER_PORT`、`IPHONE_CONTROL_VIEWER_LAN`、`IPHONE_CONTROL_VIEWER_LAN_HOSTS`。会记下当前终端的 `PATH`，好让控制台找到 `claude` 和 `codex`；PATH 变了再装一次 |
| `service viewer uninstall` / `start` / `stop` / `restart` | 移除、启动、停止控制台服务；改过上面几项设置后 `restart`。升级时它会先停下，升级完再启动（有 AI 任务在跑时自动升级会推迟） |

这些命令认得一行命令安装、`iphone-control service install`、`brew services` 和 deb 包装的服务，哪个在就作用于哪个。

## 设置与签名 {#setup}

| 命令 | 作用 |
|---|---|
| `setup` | 把引导完整走一遍：语言、代理、后台服务、配对、开发者模式、Apple ID、签名安装 WDA、自动解锁、通知、注册 MCP。做过的步骤跳过。手机部分只作用于正在控制的那台 |
| `setup --reconfigure` | 重新设置其中一项，从菜单里选：全部重新走一遍、语言、代理、后台服务、Apple ID（重新登录并重签 WDA）、手机（配对、开发者模式、安装 WDA）、自动解锁密码、通知、注册 MCP（Claude Code、Codex） |
| `login [邮箱]` | 登录 Apple ID。密码和验证码只在这里输入；不给邮箱就交互输入 |
| `sign` | 立即签名并安装 WDA，同 `resign --force`。会把设备注册到 Apple ID |
| `resign [--force]` | 描述文件剩余不到 `RESIGN_MIN_DAYS` 天才重签；`--force` 立即重签。不带 `--force` 时不会替没装过 WDA 的设备注册 |

设置菜单中用 ↑/↓ 移动、Enter 确认，也可以输入编号再回车；确认提示支持 ←/→ 选择。Esc 取消并退出向导。Ctrl+C 中断向导，退出码为 130。两者都会保留已保存的步骤。菜单始终用 `>` 标记选中项，不依赖颜色。窗口缩放后按键会按新宽度重绘。

`TERM=dumb`、输出重定向或启动菜单时终端过小，菜单使用原来的编号/文字输入。交互菜单打开后再缩小窗口，会保留当前输入并暂停确认，放大窗口后继续，也可取消。设置向导仍要求标准输入连接终端，不能用管道无人值守执行。这里的快捷键只作用于菜单，密码继续隐藏。

取消不会回滚已保存的设置；尚未保存的修改会丢弃。完整向导中的语言和代理设置会一起保存，因此在代理步骤取消时，刚选的语言也尚未保存。

## 查看状态

| 命令 | 作用 |
|---|---|
| `status` | 状态、手机、选设备方式、描述文件还剩多久、自动解锁、通知、最近日志 |
| `doctor` | 逐层诊断，每项失败都给出修复办法 |

## 多台设备

见[多台设备](./multi-device)。

| 命令 | 作用 |
|---|---|
| `devices` | 列出 USB 上接着的设备，标出正在控制哪台、有没有设自动解锁密码 |
| `select <UDID\|设备名\|auto>` | 改为控制另一台，重启后还记得；`auto` 改回自动选 |

## 自动解锁

见[自动解锁](./auto-unlock)。`--device` 接 UDID 或设备名，设备要接着；不给就是正在控制的那台。

| 命令 | 作用 |
|---|---|
| `passcode set [--device <设备>]` | 保存这台设备的锁屏密码，开启自动解锁 |
| `passcode clear [--device <设备>]` | 删除这台设备的密码 |
| `passcode clear --all` | 删除所有设备的密码 |
| `unlock-reset` | 自动解锁被停用后恢复。它不核对密码，先自己确认密码没错 |

## 其他

| 命令 | 作用 |
|---|---|
| `notify-test [事件]` | 发测试通知：每类事件各一条，或只发一类（`resign`、`session`、`pin`、`pin-recovered`、`unlock`、`offline`、`online`） |
| `wda-bridge [--port <端口>]` | 把 WDA 桥接到 `127.0.0.1`，端口默认 8100，给别的工具用。WDA 没有认证，本机任何用户都能连上，用完就停 |
| `viewer [--port <端口>] [--lan] [--lan-host <名字>…] [--tls auto\|on\|off] [--tls-cert <PEM> --tls-key <PEM>]` | 浏览器里的控制台，地址 `http://127.0.0.1:8200/`（默认端口）；加 `--lan` 时局域网里也能用 `https://<这台机器的 IP>:8200/` 打开（同一个端口；局域网来的 `http://` 会跳到 `https://`）。还没有登录，局域网里任何人都能操作手机。证书默认是本机生成的自签证书（存在数据目录的 `viewer-tls/`，一直沿用），浏览器第一次打开会提示不受信任：核对启动时或 `service status` 显示的 SHA-256 指纹一致后继续。用 `--tls-cert`/`--tls-key` 换成自己的证书（mkcert、`tailscale cert` 等）就没有这个提示。`--tls off` 时局域网里是 HTTP，画面退回 MJPEG（浏览器只在 localhost 或 HTTPS 上提供 WebCodecs）。中英文（默认跟着 `IPHONE_CONTROL_LANG`，页面上可切换）。**画面：** 实时画面像触摸屏一样用：点一下是点击，拖动是滑动，按住是长按，滚动鼠标滚轮或触控板也能滚动页面，另有输入框和硬件键。iOS 27 起手指跟着鼠标实时移动（经 CoreDevice HID，最后一次触摸后 30 秒内手机保持亮屏）；更低的版本在松开后发整段手势。操作都和 MCP 调用一样经守护进程，和 agent 的操作排队，并先自动解锁。iOS 27 及以上画面直接用手机自己的视频；其余来自 WDA（WDA 没就绪时没有画面），是 H.264 或 MJPEG，见下面的[画面编码](#viewer-picture)。画面与设备实测的尺寸、方向一致时可以点按。转屏或换机后，请在新画面上重新操作。侧栏底部的「设置」可调整主题和字号，画面缩放用画面上方的加减号调整（双指捏合不缩放，免得拖动时误触）。**设备面板：** 状态和就绪检查、已安装的 App、屏幕上的元素。页面跟着守护进程控制的设备走，在页面上或别处切换设备，画面和面板都会跟过去，见[多台设备](./multi-device#control-page)。**AI 任务：** 写下任务，交给本机的 agent 执行（PATH 里有的 Claude Code 或 Codex，在页面上选）。开任务前可以用 Agent 旁边的 App 按钮限定范围，到了别的 App 每一步都先等你确认。可以暂停、接管后交还、停止；付款、删除、发消息之前它会在页面上请你确认。本机任何用户都能连上这个端口，用完就停 |
| `task run [--title <标题>] [--app <App>…] -- <命令>` | 给别的 agent 平台用的 AI 任务：在当前控制的设备上开一个任务，再运行这条命令。命令启动的 `iphone-control mcp` 自动加入任务（经环境变量 `IPHONE_CONTROL_TASK_TOKEN`），控制台上能看到每一步，可以暂停、接管、停止，付款、发消息之前会等你确认。命令结束，任务就结束；任务在别处被停止，命令也会被停止；按 Ctrl-C 等于停止任务。任务进行中，其他客户端只能看，不能操作手机。`--app`（可写多次，App 名或 Bundle ID）把任务限定在这些 App 里：到了别的 App、或者要打开网页以外的链接，每一步都先等你确认 |
| `task list` / `task events <id>` | 最近的任务（`--json` 给程序读）；跟着一个任务的事件，每行一个 JSON |
| `task pause\|resume\|takeover\|handback\|stop <id>`、`task approve\|reject <id>` | 在命令行里控制任务，和控制台上的按钮一样。只能由人来做：在任务里面（Agent 的环境或它启动的进程）执行会被拒绝，`stop` 除外 |

调试用的 `debug` 子命令默认不编译，见[贡献指南](https://github.com/Derrors/iphone-control/blob/main/CONTRIBUTING.md#testing-on-a-real-phone)。

## 使用说明

- **要守护进程在运行的：** `setup` 的手机部分、`status`、`doctor`、`sign`、`resign`、`devices`、`select`、`unlock-reset`、`wda-bridge`、`viewer`、`task`，以及 MCP 工具调用。
- **要在终端里交互运行的：** 不带命令的 `iphone-control`（首次运行时）、`setup`、`login`、`passcode set`。
- **设置：** 除 `completions` 外，每个命令启动时都会读 `~/.config/iphone-control/env`，环境里已经有的变量优先，见[配置](./configuration#settings-file)。

## 控制台的画面 {#viewer-picture}

**iOS 27 及以上**：画面直接用手机自己硬件编码的视频（CoreDevice 屏幕媒体流，HEVC），约每秒 60 帧、原生分辨率，本机不用编码，WDA 也不用截图。滚动时每秒 150～600 KB；画面静止时也一直有一点流量（白底页面约 15 KB/s，图多的页面约 250 KB/s）。转屏时页面按界面方向把画面转正，约 1～2 秒跟上。需要浏览器能解 HEVC：Mac 上的 Chrome、Safari 可以；Firefox 和多数 Linux 上的 Chrome 解不了，自动改用下面的 H.264 或 MJPEG。看画面期间手机不会自动锁屏，灵动岛显示屏幕正被查看，关掉页面 30 秒后恢复。

**iOS 27 以下，或浏览器解不了 HEVC**：
macOS 上，以及 Linux 上装了 ffmpeg 时，控制台的画面是 H.264，否则是 MJPEG。手机的画面先在本机编码（macOS 上本程序直接调 VideoToolbox，不需要 ffmpeg；Linux 上交给 ffmpeg），浏览器再用 WebCodecs 解码（浏览器自己选用硬件解码）。同样的画面，流量约为 MJPEG 的 1/15：画面静止时几乎为零，连续滚动时约 300～450 KB/s，MJPEG 要 4～10 MB/s。延迟和 MJPEG 相差不到一帧。

- **编码器：** `viewer` 启动时按顺序试，第一个能用的就用，并在终端里显示：NVIDIA 用 NVENC；AMD、Intel 用 VAAPI（JPEG 也交给显卡解码）；Intel 还可以用 QSV；树莓派一类的 ARM 板用 V4L2；macOS 用 VideoToolbox（本程序直接调用，JPEG 也由硬件解码；装了 ffmpeg 时，它的 VideoToolbox 作备用）；都不行就用 x264 软件编码（占 CPU）。处理真实画面时出错，会换下一个。Linux 上要用显卡，用户得在 `render` 组里，`doctor` 会检查。
- **链路慢时：** 页面每 100 毫秒告诉 viewer 收到了多少。链路跟不上时少编几帧，画面帧率下降，但不会越来越滞后。比如每秒 200 KB 的链路，滚动停下后约 0.6 秒画面追上。
- **自动改用 MJPEG：** Linux 上没有 ffmpeg、没有能用的编码器、浏览器不支持 WebCodecs（较旧的浏览器）时自动改用 MJPEG；画面静止时不再重复发同一张图，链路慢时只发最新的一张。
- **看现在用的是哪种：** 画面下方写着，例如「实时画面 · WDA · HEVC iPhone 312 KB/s」（手机自己的视频）或「实时画面 · WDA · H.264 VAAPI 254 KB/s」。
- **设置：** `IPHONE_CONTROL_VIEWER_CODEC`（`device` 只用手机自己的视频，`mjpeg` 只用 MJPEG）、`IPHONE_CONTROL_VIEWER_QP`、`ICTL_FFMPEG`，见[配置](./configuration)。Linux 上安装 ffmpeg：`sudo apt install ffmpeg`，deb 包会推荐安装它；macOS 上不用装。
