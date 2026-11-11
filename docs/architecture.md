# 架构

## 1. 总览

```
┌──────────────────── Linux / macOS 主机（以普通用户运行）────────────────────────┐
│                                                                               │
│  Claude Code ──stdio──▶ iphone-control mcp ──unix socket──┐                   │
│  （每个会话一个，短命）    （ictl-mcp，经 ictl-protocol）      │                   │
│                                                           ▼                   │
│                         iphone-control daemon（ictl-daemon，常驻，唯一持有设备）  │
│                         ┌───────────────────────────────────────────────────┐ │
│   usbmuxd（系统的）◀──────┤ 设备状态机   usbmux 监听   重签调度   通知          │ │
│        │                │     │                                             │ │
│        │                │  ictl-device: lockdown · CoreDeviceProxy 隧道       │ │
│        │                │               (jktcp 用户态 TCP) · RSD · cryptexd   │ │
│        │                │               · HID · 截图 · App 列表/启动          │ │
│        │                │  ictl-wda:    XCUITestService 启动 WDA ·            │ │
│        │                │               WDA HTTP（直连设备端口，不监听本机）    │ │
│        │                │  ictl-unlock: 锁屏解锁（可选）                        │ │
│        │                │  ictl-sign:   plume_core 登录 · 描述文件 · 签名       │ │
│        │                └───────────────────────────────────────────────────┘ │
└────────┼──────────────────────────────────────────────────────────────────────┘
         │ USB
   ┌─────▼───────────────────────────────┐
   │ iPhone / iPad（iOS 17+，开发者模式）   │
   │ 开发者镜像（Cryptex，iOS 27 含 dtuhidd）│
   │ WebDriverAgent（免费 Apple ID 签名）   │
   └─────────────────────────────────────┘
```

和 toolkit 的根本区别：toolkit 每条 HID 命令都要重新建一次隧道（3～5 秒），WDA 端口转发要单独一个进程，各工具各建各的隧道。D 方案里**守护进程独占设备**，隧道、HID 会话（含显示媒体流）、WDA 会话都常驻复用；MCP 进程只是一个薄客户端。

## 2. 进程模型

| 进程 | 生命周期 | 职责 |
|---|---|---|
| `iphone-control daemon` | 常驻，作为用户服务运行（macOS 是 launchd，Linux 是 systemd 用户服务；由 `install.sh`、`iphone-control service install`、`brew services` 或 .deb 的 unit 装好），也可以在终端里前台运行 | 唯一和设备通信的进程。维护设备状态机，对外提供本地控制接口 |
| `iphone-control mcp` | 每个 agent 会话一个 | MCP stdio 服务，把工具调用翻译成对守护进程的请求 |
| `usbmuxd` | 系统的服务：Linux 上是发行版的包（.deb 依赖它，udev 在插上手机时启动），macOS 自带；守护进程不启动也不重启它 | Linux 上是系统包，GPL-3.0，独立进程运行，不链接 |
| `iphone-control`（不带命令）、`setup`、`service`、`doctor`、`sign` 等 | 一次性 | 首次运行引导（`guide.rs`）、后台服务管理（`service.rs`）和运维命令，通过控制接口或直接调用库。启动时先把 `~/.config/iphone-control/env` 读进环境（`envfile.rs`，已有的环境变量优先） |

**控制接口**：Unix socket（root 默认 `/run/iphone-control/ctl.sock`，普通用户默认 `$XDG_RUNTIME_DIR/iphone-control/ctl.sock`，没有这个变量时（macOS）在数据目录下，权限 0600），JSON 行协议。请求/响应类型和客户端 `call(socket, req, timeout)` 在 `ictl-protocol`，MCP 和 CLI 共用。理由：

- MCP 进程由 AI 客户端直接调用二进制拉起，短命且可能同时有多个；设备连接（尤其是 HID 需要的显示媒体流、WDA 的 XCTest 会话）只能有一个持有者。
- 不开 TCP 端口，避免重蹈 `ios forward` 监听所有网卡的覆辙。WDA 本身也通过 idevice 的 provider 直连设备端口，**本机不再监听 8100**。如需给外部工具用，`iphone-control wda-bridge` 显式开启，只绑 127.0.0.1，且只转发 Host 头是 `localhost`/`127.0.0.1`/`[::1]` 的请求（挡住 DNS rebinding）。注意**本机的任何用户都能连上 bridge**，而 WDA 没有认证，用完就停。
- `iphone-control viewer` 同样显式开启、只绑 127.0.0.1、检查 Host 头：页面看 WDA 的 MJPEG 流（手机上的 9100 端口，经 usbmuxd，只读）；点击、滑动、输入、按键是 POST，还要检查 `Origin` 是页面本身、内容类型是 JSON，然后作为普通请求发给守护进程，和 agent 的调用排队、自动解锁。它跟着守护进程控制的设备：在本页、别的页面、`iphone-control select`、MCP 工具里切换，或拔掉后自动换了一台，`/api/info` 的 `udid` 都会变，页面最迟在下一次 10 秒轮询时发现，把画面（`/stream?udid=`）和各面板换过去。每个操作带上页面显示的那台的 UDID（`X-Ictl-Device`），守护进程控制的不是这台就拒绝，不会发到用户没在看的手机上。还没装 WDA 的设备，登录了 Apple ID 时页面上可以直接签名安装（`/api/sign`，即 `Request::Resign { force: true }`，时限 16 分钟）。AI 任务执行中不能切换设备，设了 `IPHONE_UDID` 时也不能。
- viewer 的画面像触摸屏：iOS 27 起按下、移动、松开各发一个 `Request::Touch`（`prepare`/`down`/`move`/`up`），守护进程在已开的 HID 会话上发一个 touchscreen 报告（约 2 毫秒）；解锁、核对方向只在 `down` 时做一次。页面一次只发一个请求，没发出去的移动只留最新的一个，按住不动时每 100 毫秒再报一次位置。滚轮、触控板换成同样的手指拖动。iOS 27 以下松开后发整段手势（WDA）。画面的 MJPEG 流在 `WdaClient::fast_stream` 下是每秒 60 帧、75% 尺寸。页面的文字中英两套，请求带 `X-Ictl-Lang`，viewer 在这种语言下处理，AI 任务记下开始时的语言。
- viewer 的画面编码（`video/`）：装了 ffmpeg 时页面拉 `/video`，是 H.264。每个连接读手机的 MJPEG，丢掉和上一帧相同的（静止时 WDA 每秒重复约 25 次），交给一个 ffmpeg 子进程（`mpjpeg` 进，FLV 出；FLV 每帧带长度，收完最后一个字节就能发，裸 H.264 要等下一帧开头才知道这一帧结束），再按「1 字节类型 + 4 字节长度 + 内容」发给页面，页面用 WebCodecs 解码画到 canvas。编码器在 `viewer` 启动时按顺序试（NVENC、各渲染节点上的 VAAPI（先显卡解 JPEG，再 CPU 解）、QSV、V4L2 M2M、VideoToolbox、x264），处理真实画面失败就换下一个。ffmpeg 会扣住最新的一帧等下一帧来（AMD 780M 的 VAAPI 是 1 帧），画面静止时最后一次变化就出不来，所以每帧后面紧跟着把它再写 K 遍，K 在探测时测出（写入的帧数减去出来的帧数），这几份副本各编成约 100 字节。流量控制：系统和 SSH 隧道的缓冲能吞下几 MB，光看队列满不满判断不了页面跟不跟得上（每秒读 30 KB 的客户端曾落后 40 秒），所以页面每 100 毫秒 `POST /api/video/ack` 报告收到多少字节；最早一个没确认的消息比平常的往返时间晚 300 毫秒以上，或者在路上的字节超过「链路速度 ×（往返时间 + 300 毫秒）」时，新画面先不交给编码器，追上后只交最新一张。编好的帧一个不丢，帧链完整，不需要关键帧。没有 ffmpeg 或浏览器不支持 WebCodecs 时用 `/stream`：同样丢掉重复帧、只发最新一张，每帧后面紧跟下一部分的分隔线和头部（Chrome 要看到这些才显示上一帧）。
- viewer 的 AI 任务（`agent.rs`）：页面上写的任务交给本机的 Claude Code（`claude -p --output-format stream-json`，`--tools ""`、`--strict-mcp-config`，只有 iphone 的 MCP 工具）或 Codex（`codex exec --json`，只读沙箱，`-c` 挂上 iphone 的 MCP 服务），作为子进程（单独的进程组，停止时整组结束）。它的 MCP 服务带上环境变量 `ICTL_AGENT_GATE`（viewer 在控制接口旁边监听的 Unix socket `viewer-<端口>.sock`，0600）和 `ICTL_AGENT_TOKEN`（这个任务的口令），于是每次工具调用前先问 viewer（`ictl_mcp::gate`）：记成任务的一步、暂停或用户接管时挂住、任务停了就拒绝；调用后把结果和截图报回去；另外多列一个 `iphone_confirm`，等用户在页面上同意或拒绝。所以步骤、暂停、接管、确认都在 iphone-control 这一层，和用哪种 agent 无关；agent 自己的输出只取它说的话（挂到下一步上）和最后的总结。Codex 对标了 destructive 的 MCP 工具默认要审批，非交互运行时会全部拒绝，所以给这个服务设 `default_tools_approval_mode="approve"`，由 `iphone_confirm` 把关。页面按 SSE 收任务事件（`step`/`confirm`/`state`/`done`），重连时先重放；同一时间只有一个任务，任务执行中（没有接管时）viewer 拒绝页面的手动操作。

### 并发模型

见 `crates/ictl-daemon/src/core.rs` 开头的模块文档。

- 全部跑在一个 `LocalSet` 里（单线程协作）；设备相关状态（隧道、HID 会话、runner、WDA 客户端）在一把锁后面。
- 每个连接一个任务，请求在任务里等锁；主循环本身从不阻塞。
- 10 秒巡检用 `try_lock`：有请求在用设备就跳过这一轮，不排队。
- 耗时的事不持锁：
  - 启动 WDA（单独开一条隧道查/挂开发者镜像，再启动 runner，最长约 2 分钟）是后台任务，巡检收结果；期间工具调用照常走 HID 后备路线。
  - 重签分三段：持锁取参数 → 不持锁签名、安装 → 持锁写结果。
  - Apple ID 会话检查在后台跑。
- `status`（`Daemon` 请求）读 watch 通道发布的快照，不碰锁。
- 安装（`iphone_install_app`）同样不持设备锁；安装之间互斥（`iphone_install_app`、重签、`sign`，锁在 `Device::install`），因为 idevice 把每个 .ipa 都上传到手机上同一个文件。
- Web 请求（`iphone_web_targets`、`iphone_web_eval`）用独立的锁，不占设备锁，慢页面不挡点击和截图。
- 改变手机状态的请求（含 `iphone_web_eval`、`iphone_install_app`）排队超过 30 秒就放弃，免得点击落到已经变了的屏幕上；调用方取消后还在排队的请求也不执行。
- 停止信号优先处理（`biased` select）：取消排队和执行中的请求、巡检，再停 HID 媒体流、runner。
- **卡死兜底**：约 10 分钟没有完成任何巡检或请求（设备锁被占死、运行时卡住）时，守护进程自己退出，交给服务管理器（launchd 或 systemd）重启，免得一直占着设备又不干活。

## 3. 守护进程状态机

状态定义在 `crates/ictl-daemon/src/machine.rs`（`DeviceState`），推进在 `supervise.rs`。同一时间只控制一台设备：`IPHONE_UDID` 固定 > 运行中选定的（`select`，存在 state.json）> 自动选（正在控制的那台一直用到它断开，没有才取第一台）。选定的那台没接时就是 Absent，不换成别的（`machine::pick_device`、`devices.rs`）。

每 10 秒巡检一次，从上往下判断，第一个不满足的就是当前状态：

```
  usbmuxd 看得到手机？ ──────── 否 ─▶ Absent（离线超过 10 分钟推送通知）
        │ 是
  有配对记录？ ──────────────── 否 ─▶ NeedsTrust（iphone-control setup，手机上点「信任」）
        │ 是
  开发者模式已开？ ──────────── 否 ─▶ DeveloperModeOff（只在未就绪时检查）
        │ 是
  WDA 签名安装过？ ──────────── 否 ─▶ NeedsSigning（iphone-control sign）
        │ 是
  本进程 runner 存活且 /status ready？ ── 是 ─▶ Ready
        │ 否（runner 还活着时连续 3 次才算）→ 丢掉 runner、WDA 客户端、隧道
        ▼
  WdaStarting：后台任务 开隧道 → 查/挂开发者镜像 → 关隧道 → 启动 runner（最长 120 秒）
        │ 结果由下一轮巡检收
        ├─ 成功 ──────────────────────────────▶ 健康检查通过后 Ready
        ├─ 隧道报 PasswordProtected（锁屏/BFU）─▶ NeedsUnlock（BFU 必须人工，定期重试，状态不来回变）
        ├─「Enable UI Automation」超时 ─────────▶ NeedsAutomationPin（推送通知，60 秒重试，不指数退避）
        └─ 镜像或 WDA 其它失败 ─────────────────▶ WdaStarting { last_error }，按退避重试
```

要点（每一条都来自 toolkit 实际踩过的坑）：

- **热插拔**：交给系统的 usbmuxd（Linux 上由 udev 通知）。守护进程每 10 秒向 usbmuxd 列一次设备；列设备连续 3 轮出错才当作离线，usbmuxd 抖一下不拆会话。`doctor` 另数 sysfs `/sys/bus/usb/devices/`（Linux）或 IORegistry（macOS）里的 iPhone/iPad/iPod（厂商号 `05ac` 且产品名匹配，键盘等配件不算），区分「USB 上没有手机」和「usbmuxd 看不到」。（以前的 Docker 镜像里没有 udev，守护进程自己看护 usbmuxd，台数对不上就重启它；去掉 Docker 后这部分删了，见 D5、D14。）
- **隧道**：CoreDeviceProxy + jktcp 用户态 TCP 栈（idevice `tunnel_tcp_stack`），不需要 root，不创建 TUN 网卡。锁屏且从未解锁过（BFU）时报 `PasswordProtected`，进入 `NeedsUnlock`，只能人工解锁。HID 和截图用的隧道按需建立、常驻复用；WDA 掉线或 usbmuxd 重启时连隧道一起丢。
- **HID 会话**：归 `Tunnel` 所有（`Tunnel::hid` / `close_hid` / `close`），持有屏幕媒体流。关闭时必须先停媒体流再断隧道，否则手机一直挂着「配对的电脑」会话（灵动岛图标常驻、不自动锁屏）。空闲 30 秒关闭，重开约 0.5 秒。
- **开发者镜像**：每次启动 WDA 前都先 `installed_ddi()` 查询（toolkit 实测已挂载时约 1 秒），未挂载才走 `install_ddi()`（个性化 + TSS）。手机重启后镜像可能会掉，所以这一步不能省。默认缓存（`ddi/`）连续 3 次**挂载**失败才清掉重新下载，隧道建不起来不计入。设了 `ICTL_DDI_DIR` 时不下载、也不删里面的文件，缺文件时报错说明缺哪个。
- **WDA 就绪判定**：必须同时满足"本进程持有的 XCTest 会话存活"和 `/status` 返回 ready。toolkit 踩过：host 侧 runner 退出后，手机上残留的 WDA 还会响应几十秒再自杀，单看 `/status` 会误判。
- **自动化密码框**：手机重启后首次启用 UI 自动化会弹「为 XCTest 输入 iPhone 密码」，没人输就报 `Timed out while enabling automation mode`。识别到这个错误：进入 `NeedsAutomationPin`、只推送一次通知、保持 60 秒重试；恢复后再推送一次。
- **退避**：其它 WDA 启动失败按 60s → 120s → 240s → 300s 退避，上限 5 分钟；手机重新连上、WDA 就绪、重签完成时重置。

## 4. 模块划分

| crate | 职责 | 主要依赖 |
|---|---|---|
| `ictl-device` | `Device` 句柄：usbmux 发现与监听、lockdown、配对状态、开发者模式状态、隧道（`Tunnel`，持有 HID 会话和显示媒体流）、开发者镜像（含下载缓存）、截图、App 列表/启动/结束、安装 IPA、卸载、崩溃报告、系统日志流（`oslog`，开发者工具用）、打开 Web Inspector；所有设备调用都有时限 | `idevice`（锁上游 git 提交） |
| `ictl-wda` | `WdaRunner`（XCUITestService 启动/停止）、`WdaClient`（会话、点击、W3C actions、keys、source、screenshot、appium settings、锁屏核对）、经 usbmuxd 直连设备 8100 端口的 HTTP 传输（区分连不上和请求超时）、元素树解析（纯函数，可单测） | `idevice`（`xctest`）、`quick-xml` |
| `ictl-webinspector` | Web Inspector 服务（`com.apple.webinspector`，经 lockdown）的客户端：列出可检查的网页和 WebView、在页面里执行 JavaScript（`Target` 域和旧版直接收命令两种都支持，Promise 用 `Runtime.awaitPromise` 等待）。纯协议，消息解析可单测 | `idevice`、`plist` |
| `ictl-l10n` | 界面文字的语言：取当前语言，`tr!` / `trf!` / `log_tr!` / `log_trf!` 宏（每条文字在代码里原地写成中英一对，见 D12） | 无 |
| `ictl-protocol` | 控制接口的 `Request`/`Response` 类型和客户端 `call(socket, req, timeout)` | `serde`、`tokio` |
| `ictl-sign` | Apple ID 登录（含双重认证交互）、会话持久化、注册设备、App ID、描述文件、签名 `.app`/`.ipa`（**包括 `.xctest`**；bundle 逻辑改写自 plume_utils）、签后自检 | vendored 的 `plume_core`/`plume_store`（MPL-2.0/MIT；经 plume_core 用 vendored 的 `omnisette` 和打过补丁的 `apple-codesign`） |
| `ictl-unlock` | 锁屏密码键盘定位、逐位输入并核对密码栏、从第 2 位起先写停用标记 | `ictl-wda` |
| `ictl-daemon` | `core`（共享状态、请求分发、快照）、`supervise`（巡检、后台启动 WDA、重签、会话检查）、`tools`（工具的实现、HID 后备）、`devtools`（日志采集、崩溃报告、安装、卸载）、`web`（Web Inspector 工具，保留一条连接复用）、`devices`（列出、选择设备）、`doctor`；通知（Bark/Telegram）、截图缩放/裁剪、控制接口服务端 | 以上全部、`tokio`、`reqwest`、`image` |
| `ictl-mcp` | MCP stdio 服务，`iphone_*` 工具 17 个，开启开发者工具后 23 个：toolkit 的 13 个 + `iphone_wait_for`、`iphone_batch`、`iphone_list_devices`、`iphone_select_device`，以及 6 个可选的开发者工具（D13）；操作类工具可带 `then` 顺带返回操作后的界面；经控制接口调用守护进程 | `ictl-protocol`、`ictl-l10n`、`rmcp`（`server`、`transport-io`） |
| `apps/iphone-control` | CLI 入口：不带命令时是首次运行引导或状态（`guide.rs`）、`daemon`、`mcp`、`mcp-config`、`status`、`doctor`、`setup [--reconfigure]`、`service`（`service.rs`：launchd / systemd 用户服务）、`login`、`sign`、`resign`、`passcode`、`unlock-reset`、`devices`、`select`、`wda-bridge`、`viewer`（`viewer.rs`：本机控制台网页，实时画面、设备面板；`video/`：画面的 H.264 编码（外部 ffmpeg）和 MJPEG 去重；`agent.rs`：页面上的 AI 任务，`viewer.html` 是页面）、`notify-test`；隐藏的 `debug <ddi\|hid\|screenshot\|soak\|wda\|e2e>` 真机诊断（原第 0 步 spike，不经守护进程；feature `debug-cmds`，默认不编译） | `clap` |

依赖方向：`apps` → `ictl-daemon` / `ictl-mcp`，还直接依赖 `ictl-device`、`ictl-sign`；`ictl-daemon` → `ictl-unlock` → `ictl-wda` → `ictl-device`；`ictl-daemon` 还直接依赖 `ictl-webinspector`、`ictl-sign` 和 `ictl-protocol`。`ictl-device` 也依赖 `ictl-webinspector`（打开连接）。`ictl-mcp` 只依赖 `ictl-protocol` 和 `ictl-l10n`，不依赖守护进程和设备层。`ictl-l10n` 不依赖其他 crate，除 `ictl-webinspector` 外都用它。`apps` 的 `debug`（feature `debug-cmds`）直接调用 `ictl-device`、`ictl-wda`。

## 5. 截图与坐标

- 坐标单位统一用**点（pt）**，与 WDA 一致，随界面方向变化；屏幕点尺寸从 WDA `/wda/screen` 读取。WDA 就绪时把竖放尺寸和设备 UDID 记到 `state.json`（只记最近一台）。WDA 不可用时：记录是这台的就用记录，否则用设置里的 `SCREEN_W`/`SCREEN_H`（不分设备），都没有就报错（不写死 430×932）。
- HID 坐标是 0～65535 归一化：先按界面方向转回竖放（`Rotation::to_portrait`），再换算 `x / w * 65535`。每次 HID 触摸前重新截图核对横竖；横屏要有 30 秒内且匹配的 WDA 旋转角，否则拒绝触摸。
- 截图优先走 WDA（toolkit 实测约 0.4 秒）；WDA 不可用时走 CoreDevice `screencaptureservice`（不需要签名）。
- 区域放大：从原始像素截图裁剪，长边 < 600 放大到 600，> 1200 缩到 1200，返回倍数和坐标换算式（三位有效数字）。

## 6. 数据与秘密目录

全部在一个数据根目录下（普通用户默认 `$XDG_DATA_HOME/iphone-control`，没设时 `~/.local/share/iphone-control`，macOS 相同；以 root 运行时是 `/var/lib/iphone-control`，安装方式都不这样用）。数据根目录是 0700；下表权限写「默认」的没有单独设，靠它保护：

| 路径 | 内容 | 权限 | 进 git / 进发布包 |
|---|---|---|---|
| `lockdown/` | 不再使用（以前容器里的 usbmuxd 把配对记录放这里）。配对记录由系统的 usbmuxd 保存：Linux 在 `/var/lib/lockdown`，macOS 在 `/var/db/lockdown`，和 Finder 共用 | 0700 | 否 |
| `account/` | Apple ID 会话、开发私钥、证书 | 0700 | 否 |
| `ddi/` | 开发者镜像缓存（运行时下载的苹果文件）；设了 `ICTL_DDI_DIR` 时用那个目录，不用这里 | 默认 | **否，不得分发** |
| `wda/` | 当前签好的包 `WDA-signed.ipa`，和未签名包 `WDA-unsigned.ipa`：第一次签名前下载 Appium 官方的未签名真机包（`WebDriverAgentRunner-Runner.zip` v16.15.3，校验 sha256）重新打包（`crates/ictl-daemon/src/wda_pkg.rs`）；计划改为本项目 macOS CI 编译。描述文件只嵌在签好的包里，到期时间记在 `state.json` | 默认 | 否（在本机下载） |
| `secrets/passcodes/<UDID>` | 各设备的锁屏密码（仅启用自动解锁时）；旧版本的 `secrets/passcode` 在只接着一台时归到那台名下 | 0600 | 否 |
| `logs/` | 事件日志 `daemon.log`（超过 5 MB 轮转成 `.1`）；自动解锁停用标记 `UNLOCK-DISABLED`（与 toolkit 一致，便于人工发现；全局，不分设备） | 默认 | 否 |
| `state.json` | 最近一次签名的 Team ID、WDA Bundle ID、描述文件到期时间；选定的设备；最近一台就绪设备的竖放屏幕尺寸和 UDID；上次重签结果 | 默认 | 否 |

守护进程的配置全部来自环境变量（`crates/ictl-daemon/src/config.rs`），都有默认值。设置文件 `~/.config/iphone-control/env`（`KEY=value`，0600，模板 `packaging/env.example`）由首次运行引导和 `setup --reconfigure` 写（`envfile.rs`：改一项就地替换，注释和其他项保留）；后台服务（launchd、systemd、`brew services`）把它读进守护进程的环境，其他命令启动时也读一遍，已有的环境变量优先。通知渠道的密钥也在这个文件里；其余秘密只放数据目录里的文件。

## 7. 重签与通知

- 守护进程每 `RESIGN_CHECK_HOURS`（默认 6 小时）检查一次：描述文件剩余 < `RESIGN_MIN_DAYS`（默认 2 天）才重签；另外**每天验证一次 Apple ID 会话**（toolkit 的教训：只在要重签时才发现登录过期，只剩 2 天处理）。两者都在后台跑，不持设备锁。只检查正在控制的那台；手机重新连上或切换设备后 1 分钟内也查一次。
- 重签流程：`plume_core` 取新描述文件 → `ictl-sign` 签名（含 `.xctest`）→ `installation_proxy` 安装 → 下一轮巡检重启 WDA。安装成功后才替换旧文件。整个重签有总时限，超时按失败处理并推送通知。没装过当前 Apple ID 签的 WDA 的设备不自动重签（注册设备占 Apple ID 的名额，交给用户决定）。
- **签名证书**：每个账号目录有自己的私钥（`account/keys/<团队>/key.pem`）和对应的开发证书；免费团队同时最多两张，新建时满了 plume 会吊销一张，不挑是谁的，用它签的 App 在所有设备上都起不来，描述文件却仍有效。所以：读设备上的 WDA 描述文件时顺带离线判断里面有没有本机公钥的证书（`foreign_wda`）；每天的会话检查顺带联网查本机证书还在不在团队里（`own_cert`）。设备拒绝启动 WDA（只报 `unexpected response`）时，两者任一有问题就自动重签这台，同一台 6 小时内最多一次；都没问题就多半是还没信任开发者，留给用户。签名新建或吊销证书时写日志、推送。`doctor` 显示这两项。
- 通知事件：重签失败、Apple ID 会话失效、需要输入自动化密码、WDA 恢复、自动解锁失败已停用、手机离线超过 10 分钟、恢复在线、签名证书换了。通知文字不带设备名。去重：重签失败、会话失效 20 小时内不重复；等密码、离线、自动解锁停用推一次，直到恢复或 `unlock-reset`；只有等密码和离线有恢复推送；签名证书换了每次都推。去重记录只在内存里，守护进程重启后清空。
- 守护进程整个挂掉时发不了通知，这部分仍需宿主机侧的监控，本项目还没有提供。后台服务（`iphone-control service install` 装的 launchd 或 systemd 服务）只负责崩溃后重启，同样不报警。
