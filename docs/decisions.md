# 关键决策记录

格式：背景 → 决定 → 理由 → 代价/何时重新评估。

## D1：基于 idevice，而不是从协议层自己写

- **决定**：设备通讯全部基于 `idevice`。版本用 `[patch.crates-io]` 锁在上游 master 的 git 提交 `89702099`（0.1.68 发版代码的 cryptexd install 会挂住，修复已在 master、未发版）；发版后换回 crates.io。
- **理由**：它已覆盖我们需要的全部能力：usbmuxd、lockdown、CoreDeviceProxy 用户态隧道、RSD、cryptexd 个性化镜像安装、universalhidservice/indigo HID、screencaptureservice、XCTest 启动 WDA、WDA 客户端；MIT；StikDebug、CrossCode 等在生产中使用；Impactor 也依赖它，和签名部分同源。
- **代价**：0.2.0 之前每个小版本都可能有破坏性变更；主要一人维护；`tss` 自述部分支持、TCP 栈自述 "naive"。
- **重新评估**：spike 第 4、5 项不过时（已全部通过，见 [spike-results](dev/archive/spike-results.md)）。

## D2：守护进程独占设备，MCP 是薄客户端

- **决定**：`daemon` 常驻持有隧道、HID 会话、XCTest 会话；`mcp` 通过 Unix socket 调用。
- **理由**：MCP 进程每个会话一个、短命、可能并发；HID 事件要求显示媒体流同时存在；WDA 只能有一个 runner。toolkit 每条 HID 命令重建隧道要 3～5 秒。
- **代价**：多一层协议。

## D3：WDA 不在本机监听端口

- **决定**：用 idevice 的 provider 直连设备端口访问 WDA；外部工具需要时显式开 bridge，只绑 127.0.0.1。
- **理由**：WDA 没有认证；toolkit 早期用 `ios forward` 监听了所有网卡，局域网内任何人都能控制手机。
- **补充**：`iphone-control viewer` 用同样的保护模型（显式开启、只绑 127.0.0.1、检查 `Host`），另加操作的 `Origin` 和 JSON 内容类型检查、读接口的 `Sec-Fetch-Site` 检查，并只作用于页面跟着的那台设备。它的 AI 任务不开新端口：agent 的 MCP 服务经控制接口旁边的 Unix socket（0600，每个任务一个口令）回报 viewer。

## D4：WDA 仍然是 Objective-C，在 macOS CI 上编译

- **决定**：不重写 WDA；fork appium/WebDriverAgent，在 GitHub Actions macOS runner 上不签名编译，发布未签名 IPA + sha256。
- **理由**：WDA 是 XCTest bundle，只能用 Xcode 编译，依赖苹果私有/测试框架；Rust 无法替代。
- **代价**：发布流程依赖 macOS runner。
- **现状**：自己编译还没做；暂时在第一次签名前于本机下载 Appium 官方 Release 的未签名真机包（v16.15.3，校验 sha256）。

## D5：使用系统的 usbmuxd

- **决定**：不内置 USB 协议栈。Linux 上用发行版的 `usbmuxd` 包（.deb 依赖它），macOS 用系统自带的；守护进程不启动也不重启它。
- **理由**：idevice 只有 usbmuxd 客户端；自己实现 USB 多路复用收益很小；usbmuxd 是 GPL-3.0，作为独立进程运行不影响本项目许可证。
- **代价**：Linux 上要用户自己装 usbmuxd（install.sh 会提示，.deb 自动装上）。
- **变更（2026-11-10）**：以前在容器里把 usbmuxd 作为子进程看护（`ICTL_MANAGE_USBMUXD`、`ICTL_USBMUXD_BIN`），还要处理容器没有 udev 的热插拔（数 sysfs，台数对不上就重启它）。去掉 Docker 后这部分删了，见 D14。

## D6：签名组件 vendor 而不是 git 依赖

- **决定**：把 Impactor 的 `plume_core`、`plume_store`、`omnisette` 和打过补丁的 apple-codesign vendor 进本仓库。`plume_utils` 不 vendor：需要的 bundle 逻辑改写进 `crates/ictl-sign/src/bundle.rs`（含 `.xctest` 修复）。
- **理由**：Impactor 用 `[patch.crates-io]` 把 apple-codesign 指向本地打过补丁的副本；作为 git 依赖引用时，**依赖方 workspace 的 patch 不会生效**，会拿到未打补丁的 apple-codesign。另外需要修 `.xctest` 漏签，还要去掉许可证有问题的可选依赖（见 THIRD_PARTY.md）。
- **代价**：要定期同步上游；MPL-2.0 文件改动需单独标注。修复同时提 PR 回上游，争取将来改回普通依赖。

## D7：开发者镜像下载由我们负责

- **决定**：`ictl-device` 提供 DDI 下载与缓存（来源可配置，默认与 pymobiledevice3 相同），`install_ddi` 只接收本地目录。
- **理由**：idevice 的 `Cryptex1Assets::load` 只读本地解包目录，不负责下载；苹果文件不能随项目分发。
- **重新评估**：idevice 若加入下载功能。

## D8：自动解锁默认关闭

- **决定**：需要显式开启并配置密码文件。
- **理由**：存储锁屏密码意味着能访问主机的人就能解锁手机；输错累计还可能触发设备抹除。保护规则见 `crates/ictl-unlock/src/lib.rs` 顶部注释。

## D9：所有依赖锁定精确版本

- **决定**：`Cargo.toml` 用 `=x.y.z`，`Cargo.lock` 入库，CI 和发布用的 Rust 工具链、cargo-about 也锁版本。
- **理由**：toolkit 两次被版本漂移坑过（mcp 2.x 改名；go-ios main 分支命令变化）。idevice 官方也要求锁定。

## D10：与 toolkit 工具接口一致

- **决定**：现在共 17 个 MCP 工具，其中来自 toolkit 的 13 个，工具名、参数、返回文本格式保持与 toolkit 一致。
- **理由**：切换时 agent 侧零改动；toolkit 的回归脚本可直接用于验收。
- **补充（2026-11-01）**：只做向后兼容的扩展，不改已有工具的名字、参数和默认返回。为减少 agent 往返（每次往返是一轮模型推理，
  真机观察约 8～13 秒，远大于工具本身的 0.3～1.5 秒），操作类工具加可选参数 `then`（`elements` / `screenshot`：等界面稳定后
  顺带返回界面），并新增 `iphone_wait_for`（等某段文字出现）和 `iphone_batch`（一次执行最多 20 步，出错即停）。多设备的 `iphone_list_devices`、`iphone_select_device` 见 D11。
- **补充**：另有 6 个可选的开发者工具（`IPHONE_CONTROL_DEV_TOOLS=1`，见 D13），不在这里的兼容承诺内。

## D11：接多台设备时，同一时间只控制一台

- **决定**：一个守护进程可以接多台 iPhone/iPad，但只控制一台（一个 WDA 会话、一把设备锁）。优先级：`IPHONE_UDID` 固定 > 运行中选定的（`select`、`iphone_select_device`，持久化到 `state.json`）> 自动选（正在控制的那台一直用到它断开）。选定的那台没接时就是离线，不静默换到另一台。锁屏密码按设备存；自动解锁的停用标记是全局的。
- **理由**：操作或密码落到另一台手机上后果严重，所以宁可离线也不换台，密码也绝不跨设备使用。同时控制多台要多套会话，每个工具还要加设备参数，和 D10 的接口兼容冲突。停用标记全局是偏保守的选择：一台失败后先让人看一眼。
- **代价**：选定的设备没接时可能触发离线通知；一台自动解锁失败，所有设备都停用；切换后 WDA 要 10～60 秒才起来；自动重签只管正在控制的那台，闲置设备的描述文件会过期，切回后才重签；通知文字不带设备名。
- **重新评估**：出现同时驱动多台的需求时。

## D12：界面文字可选英文，默认仍是中文

- **决定**：`IPHONE_CONTROL_LANG=en|zh` 选择人和 agent 看到的文字的语言（工具说明、工具结果、命令行输出、日志、通知）；不设或认不出时是简体中文。每条文字在代码里原地写成中英一对（`ictl-l10n` 的 `tr!` / `trf!`），不另建翻译目录。从手机界面读到的文字（例如「滚动条」）不翻译。
- **结果的语言由客户端决定**：MCP 和命令行在每个请求里带上自己的语言（`lang` 键），守护进程按它回答；不带 `lang` 的请求（旧版本客户端）照旧得到中文，因为它们按中文的固定开头认结果。守护进程自己的设置管它主动写的文字（事件日志、日志行、通知），处理请求时写的也一样（`ictl-l10n` 的 `log_tr!` / `log_trf!`），所以日志和通知始终是同一种语言。上次重签的结果存成时间和成败，显示时再按看的人的语言拼句子。MCP 和守护进程启动时都读设置文件 `~/.config/iphone-control/env`，自然一致（注册 MCP 时用 `-e` 设的值优先）。
- **客户端要据以行动的结果用字段表示**：文字结果带 `outcome`（`tapped_nothing`：按名字没点到；`alert_reported`：结果里已经报过系统弹窗），新版本 MCP 只看字段，不再从文字里认；没有 `outcome` 的响应来自旧版本守护进程，那时文字一定是中文，才按 `TAP_MISSED`、`ALERT_BLOCKING` 认。旧版本客户端不认识这个字段，会忽略它。
- **理由**：D10 承诺已有工具的默认返回不变，所以英文只能是可选项，不设就和以前完全一样。两种语言并排写在原处，改文字时另一种就在旁边，审查时也能逐条对照。
- **代价**：每条新文字都要写两种语言；取语言是一次原子读，开销可以忽略。
- **重新评估**：多数用户改用英文时，再讨论默认值。

## D13：开发者工具显式开启，开关不是权限边界

- **决定**：日志、崩溃报告、安装、卸载和 Web Inspector 的 6 个工具由 `IPHONE_CONTROL_DEV_TOOLS` 控制，默认关闭。开关只在 MCP 层决定提不提供，守护进程始终接受这些请求。日志只要默认、Error、Fault 级别，不开 Debug、Info。Web Inspector 保留一条连接复用，只有连接层的错误才重开。卸载只接受 Bundle ID 或完整显示名（不分大小写），不像启动、结束 App 那样模糊匹配，并拒绝卸载 WDA。
- **理由**：
  - 默认关闭：不改变默认的工具列表（D10），用不到的 agent 不必多读工具说明。能连上控制接口（权限 0600）的人本来就能操控手机，在守护进程再加一道开关隔离不了什么。
  - 日志：idevice 默认的 `StreamFlags` 带 Debug，手机上每个进程都开始产生 Debug 消息。真机上约 650 行/秒，不开时约 80 行/秒；开着时手势变慢、失败（`crates/ictl-device/src/oslog.rs`）。
  - Web Inspector：刚关闭后新开一条连接约 8 秒，几轮连接、断开后手机不再向新连接报告 Safari，要重启 Safari；保留的连接上列表和执行只要几毫秒（iPhone 15 Pro Max，iOS 27.0.1，`crates/ictl-daemon/src/web.rs`）。
  - 卸载连同数据一起删掉，猜错名字的代价比启动错 App 大得多。
- **代价**：开关挡不住能连上控制接口的本机进程，真正的边界是 socket 权限；开关要对 MCP 进程生效：写在设置文件里，或注册 MCP 时用 `-e` 设（首次运行引导和 `mcp-config` 会带上）；看不到 Debug、Info 级别的日志；保留的连接要 5 分钟没有请求才关。
- **重新评估**：出现需要在守护进程上按调用方限权的场景时；iOS 能在手机端按进程过滤日志时。

## D14：去掉 Docker，只做原生安装（2026-11-10）

- **背景**：以前 Linux 上推荐用 Docker（`compose.yaml`、ghcr 上的镜像），macOS 和从源码运行是另一套步骤。控制台（`iphone-control viewer`）加了 AI 任务以后，任务要交给本机的 Claude Code 或 Codex 执行。
- **决定**：从下一个版本起不再发布 Docker 镜像，删掉 `compose.yaml`、`docker/Dockerfile`、`.dockerignore` 和 `deploy/iphone-control.service`。安装只有原生方式：一行命令的 `install.sh`（macOS Apple Silicon、Linux x86_64/aarch64）、Homebrew（`derrors/tap`）、Debian/Ubuntu 的 `.deb`，以及从源码编译（Intel Mac、开发）。守护进程以普通用户运行，装成用户服务（macOS 是 launchd，Linux 是 systemd 用户服务），由 `install.sh`、`iphone-control service install`、`brew services` 或 .deb 的 unit 管理。usbmuxd 用系统的（D5）。装好后运行 `iphone-control`，首次运行引导一步步完成设置，写 `~/.config/iphone-control/env`。
- **理由**：
  - 容器里的守护进程够不到宿主机上的 Claude Code / Codex（它们的登录、配置都在用户的家目录里），控制台的 AI 任务用不了。
  - 原生安装更简单：不用 root、不用 Docker、不用 USB 直通，macOS 和 Linux 是同一套步骤；Mac 上本来就不能用 Docker（Docker Desktop 不支持 USB 直通）。
  - 少一套要维护的东西：自管 usbmuxd、容器没有 udev 的热插拔兜底、镜像构建和 SBOM、`docker exec` 形式的注册命令都不要了。
- **代价**：Docker 用户要迁移（数据目录拷到 `~/.local/share/iphone-control`，`.env` 改成 `~/.config/iphone-control/env`，手机可能要重新信任，见更新日志的升级注意事项）；Linux 上要自己装 usbmuxd；不能再用容器把它和宿主机隔离开，守护进程的权限就是登录用户的权限；Intel Mac 没有预编译版本，要从源码编译。
- **重新评估**：有人需要在无人值守的服务器上隔离运行、又不用控制台的 AI 任务时。

## D15：控制台画面用 H.264，调用外部 ffmpeg，显卡编码优先（2026-11-11）

- **背景**：控制台（`iphone-control viewer`）的画面是 WDA 的 MJPEG 流，每秒 4～10 MB（画面静止时 WDA 也每秒重发约 25 张）。经 SSH 隧道在外网看时卡，而且系统和隧道的缓冲会把积压藏起来，画面越来越滞后。
- **决定**：装了 ffmpeg 时画面改用 H.264，页面用 WebCodecs 解码；没有 ffmpeg、没有能用的编码器、浏览器不支持时退回 MJPEG。编码交给外部的 ffmpeg 进程，按机器挑编码器，显卡优先：NVENC、VAAPI、QSV、V4L2 M2M、VideoToolbox，最后是 x264。默认画质 qp 20，可以用 `IPHONE_CONTROL_VIEWER_QP` 调。页面回报收到多少，链路跟不上时少编几帧。
- **理由**：
  - 实测（AMD 780M，VAAPI，在设置里来回滚动）：MJPEG 约 4.5 MB/s，H.264 约 250～430 KB/s，静止时为零，逐帧 SSIM 中位 0.999；收到新画面的时间和 MJPEG 差 −2～+37 毫秒；CPU 约 2%（JPEG 也交给显卡解码），软件 x264 约 36% 单核。
  - 调用 ffmpeg 程序而不链接它：各发行版的 ffmpeg 已经带了各家显卡的编码器，不用自己适配；x264 是 GPL，不链接就不影响本项目的 MIT 许可证；不装也照常能用。
  - 帧格式选 FLV：每帧带长度，帧一编好就能发；裸 H.264 要等下一帧开头才知道这一帧结束，而下一帧正被 ffmpeg 扣着。
- **代价**：
  - 多一个可选依赖，Linux 上用显卡还要在 `render` 组里。
  - 4:2:0 色度采样，彩色小字的边缘比 JPEG 略软。
  - 依赖 ffmpeg 命令行的行为：它会扣住一帧，靠紧跟的副本挤出来，扣几帧在探测时测，换了 ffmpeg 版本可能变。
  - Intel、NVIDIA、树莓派的编码器参数只按文档定，还没在真机上验证。
- **补充（2026-11-11）**：macOS 上改为本程序直接调 VideoToolbox（JPEG 也交给 VideoToolbox 的硬件解码器），不再需要 ffmpeg；ffmpeg 的 VideoToolbox 留作备用。VideoToolbox 不扣帧，不用补帧；qp 能直接定住，画质设置和其他硬件编码器一致。只多了几个系统框架的链接，没有新依赖。实测（Apple 芯片，macOS 27）：从送进 JPEG 到出帧 3.5～6 毫秒；模拟列表滚动的测试画面（480×1040）每帧 5～9 KB，关键帧约 31 KB；每秒 20 KB 的链路上，画面停下后约 0.8 秒最后一帧到达。手机真实画面（iPhone 18 Pro Max，990×2151，76 秒：主屏翻页、设置里滚动、网页滚动，931 个不同的帧）：qp 20 时 H.264 是去重后 MJPEG 的 11.4%，平均 337 KB/s，最忙的一秒 1076 KB/s；逐帧 SSIM 中位 0.9976、最低 0.9927；从送进到出帧中位 17 毫秒。qp 16/18/24 的 SSIM 中位是 0.9986/0.9982/0.9964，流量是 qp 20 的 1.64/1.29/0.58 倍。SSIM 是本程序的量法（`measure_recording`：页面解出的画面对比编码器拿到的 NV12，ffmpeg `ssim` 的 8×8 窗口），和 Linux 上用 ffmpeg 量的 0.999 不是同一个工具、同一台手机，不能直接比。VideoToolbox 的 SPS 不带 VUI，Chrome 的硬件解码器会因此扣住 4 帧，所以发给页面前改写 SPS，写明不重排帧（见架构文档）。
- **重新评估**：浏览器普遍支持更省的编码（AV1）且本机有硬件编码时；或者 iOS 27 的 CoreDevice 屏幕媒体流（手机自己编的视频）能直接转给页面时，可以省掉本机编码。

## D16：iOS 27 及以上，控制台画面直接用手机自己编的视频（2026-11-12）

- **背景**：控制台的画面原来都来自 WDA 截图（MJPEG）。复杂页面上 WDA 每秒只出 7～16 帧，而且截图本身让 WDA 的 CPU 超过一个核：控制台开着时，iOS 按「一分钟平均超过 80%」每隔几分钟就把 WDA 杀掉（`cpu_resource_fatal`），WDA 重启期间操作失败。降帧率、改缩放和画质都没有明显改善。
- **决定**：iOS 27 及以上，页面优先用 CoreDevice 屏幕媒体流：手机硬件编码的 HEVC，守护进程解 RTP、组帧，经 viewer 原样转给页面，用 WebCodecs 解码。浏览器解不了 HEVC、设备低于 iOS 27、设了 `IPHONE_CONTROL_VIEWER_CODEC=mjpeg` 或指定某个编码器时，照旧用 D15 的 H.264 或 MJPEG。
- **理由**（iPhone 18 Pro Max，iOS 27.2.1，Mac 上的 Chrome）：
  - 滚动时页面每秒画出约 52 帧，原生分辨率 1320×2868；WDA 画面同样的操作是 9～16 帧、990×2151。
  - 主机不解 JPEG、不编码；WDA 的 CPU 从 110～120% 降到约 3%，不再被杀。
  - 延迟和 WDA 画面相当或更早；码流里写明不重排帧，Chrome 的硬件解码器不扣帧。
  - HID 会话本来就持有这条流，多的只是不再把数据丢掉。
- **代价**：
  - 只有 iOS 27 及以上；Chrome 只能用硬件解 HEVC，Firefox 和多数 Linux 上的 Chrome 解不了，要退回 D15。
  - 流量不比 H.264 省：滚动时每秒 150～600 KB，静止时也不为零（白底页约 15 KB/s，图多的页约 250 KB/s）；链路慢时只能丢帧等关键帧，不能少编几帧。
  - 看画面期间手机不自动锁屏、灵动岛显示正被查看（同 HID 后备路线）。
  - 视频总是竖的：界面方向要另外问 WDA（每秒一次），转屏后约 1～2 秒画面才转正；WDA 不可用时按最后知道的方向。
  - 依赖苹果没公开的协议：RTCP 反馈照上游对 Device Hub 的抓包；上游提醒不回 RCTL 时剧烈运动会花屏，回了之后的长时间表现还没测过。
- **重新评估**：长时间运行出现花屏或断流时；浏览器普遍能软解 HEVC 时，可以去掉对硬件解码的依赖。
