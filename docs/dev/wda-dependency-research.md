# WDA 依赖调研

2026-11-06 ～ 2026-11-12。问题：项目依赖的 WebDriverAgent（WDA）是不是必需的，能不能去掉？顺带看了能不能少依赖一点。

真机：iPhone 15 Pro Max（iPhone16,2）iOS 27.0.1；部分对照用了 iPad mini 5（iPad11,1）iPadOS 26.6.1。下面每条结论都只对这两台、这两个系统版本验证过。

## 1. 结论

1. **以现在的功能，WDA 去不掉。** 中文输入、读元素树、按名字点、自动解锁都要靠 XCTest 的能力，而这些能力只开放给手机上已签名、已授权的测试进程（第 6 节）。只要还需要这些能力，就得在手机上跑一个签了名的测试包，Apple ID 签名也就省不掉。
2. **苹果有一条电脑端直接做 UI 自动化的通道**，就是 `com.apple.dt.testmanagerd.remote.automation`，功能比 WDA 还全，但要求 `AppleInternal` 权限。普通电脑能连上 TCP，手机的 `remoted` 不会把连接转给 testmanagerd（6.2 节）。
3. **不装 WDA 也能做的**（iOS 27+）：HID 点击、滑动、按键，CoreDevice 截图，CoreDevice appservice 启动和结束 App，springboard 的屏幕方向，以及 remoteAXService 读元素树（又慢又不全）。拼起来可以做一个降级模式，但输不了中文，也做不了自动解锁（第 4、5 节）。
4. **WDA 不需要电脑一直拿着 XCTest 会话。** 在手机上直接启动 xctrunner 时，runner 会自己生成测试配置，testmanagerd 照常授权。用 DVT processcontrol 加 `ActivateSuspended` 在后台启动，锁屏时也能起来，4 秒就绪。这条路线已经做成守护进程的实验开关 `ICTL_WDA_DETACHED=1`，并在真机上验证过（第 8 节）。

## 2. WDA 的原理

WDA 是一个**永远不结束的 UI 测试**。签好名的 `WebDriverAgentRunner-Runner.app` 里有一个测试包 `PlugIns/WebDriverAgentRunner.xctest`，测试包里只有一个测试方法。这个方法在手机的 8100 端口起一个 HTTP 服务（WebDriver 协议加上 `/wda/*` 扩展），在 9100 端口起一个 MJPEG 画面流，然后一直不返回。

WDA 自己只是沙盒里的普通 App，没有任何特权。**真正有权限的是 testmanagerd**，它在开发者镜像里。测试进程通过 XCTest 的私有接口请 testmanagerd 代办这些事：

| WDA 接口 | 背后的 XCTest 机制 |
|---|---|
| 点击、滑动、长按（W3C actions） | `_XCT_synthesizeEvent:`：testmanagerd 在系统层注入合成的触摸事件 |
| 输入文字 `/wda/keys` | `_XCT_sendString:`：按**字符串**注入，所以中文也能输（HID 只能按美式键盘的键） |
| 读元素树 `/source` | `_XCT_fetchSnapshotForElement:`：无障碍（AX）快照，能跨进程读 SpringBoard 和任意 App |
| 等界面空闲 | XCTest 的 quiescence，这就是 `waitForIdleTimeout` 设置的来历 |
| 启动、结束 App、Home、锁屏、截图 | XCTest 里对应的 App 控制、设备按键、截屏接口 |

前置条件：开发者模式、挂载开发者镜像（testmanagerd 在镜像里）、开发证书签名（要带 `get-task-allow`，免费 Apple ID 就够），以及手机重启后第一次启动时输入密码，允许 UI 自动化。

## 3. 现在的启动方式

`ictl-wda` 用 idevice 的 `XCUITestService`（`crates/ictl-wda/src/runner.rs`）启动 WDA：

1. 经 RSD 连 `com.apple.dt.testmanagerd.remote`，开两条 DTX 连接（ctrl、main），再连 `com.apple.instruments.dtservicehub`。
2. 发 `XCTestConfiguration`，用 DVT processcontrol 启动 runner，环境变量里带上配置路径和会话 ID，启动选项是 `ActivateSuspended`。
3. 授权测试进程，开始执行测试计划，然后**一直拿着会话**。

电脑这端断开后，testmanagerd 会结束测试，WDA 过几十秒自己退出。所以就绪判定要同时看「本进程的 runner 任务活着」和 `/status`。守护进程重启、升级时，WDA 都会跟着重启。

## 4. 各功能对 WDA 的依赖

| 功能 | 现在 | 不用 WDA 的替代 | 情况 |
|---|---|---|---|
| 截图 | WDA 优先 | CoreDevice `screencaptureservice` | 已是后备路线 |
| 点击、滑动、按键 | WDA 优先 | HID（universalhidservice、indigo） | 已有，只支持 iOS 27+（开发者镜像里的 `dtuhidd`），见 `hid_supported` |
| 长按 | 只走 WDA | HID 按下，等一会，再松开 | 容易补 |
| 启动、结束 App | 只走 WDA | CoreDevice appservice：`launch_application`、`list_processes`、`send_signal` | 可行，但「等 App 到前台」现在靠 WDA 的 activeAppInfo |
| 屏幕方向 | WDA 的旋转角 | `springboardservices::get_interface_orientation`（走 lockdown，不需要镜像） | 可行，还能分出 HID 现在分不出的 180° 翻转 |
| 屏幕点尺寸 | `/wda/screen` | 截图像素除以机型缩放倍数 | 可行 |
| 读元素树、按名字点、`wait_for`、等界面稳定、看弹窗 | 只走 WDA | remoteAXService（第 5 节） | 能用，但慢、不全 |
| 输入中文 | `/wda/keys` | 没有。HID 只能按键 | 无替代（第 7 节有个可能的办法） |
| 自动解锁 | 逐位核对密码栏的「已输入 N 位」 | AX 能读锁屏界面，读不读得到密码栏位数没测过 | 要重做 |
| `open_url` | `/url` | 网页可以用 Web Inspector；跳到其他 App 的链接没有办法 | 部分替代 |
| 控制台画面 | WDA 9100 端口的 MJPEG | 轮询截图，或 HID 会话的显示媒体流；镜像里还有 `com.apple.coredevice.dtremotedisplayd`，没看过 | 没验证 |

去掉 WDA 能省掉的：Apple ID 登录、免费描述文件和 7 天重签、证书被吊销和信任开发者这类问题、「UI 自动化」开关、WDA 崩溃恢复，以及 `ictl-sign`、`plume_core`、omnisette 这些依赖。开发者模式和开发者镜像还是要有。

## 5. 不经 WDA 读元素树：remoteAXService（2026-11-06）

服务是 `com.apple.accessibility.axAuditDaemon.remoteAXService`，走 DTX：发 `clientNeedsAccessibility:`，手机用 `processDataFromRemoteDevice:` 推两层 NSKeyedArchive，里面是 `AXPTranslatorResponse` 的 `treeDump`。属性是没公开的数字编号，按真机数据反推出来：33 是标签，25 是 identifier，53、54、64 是值，21、43、48 是边框。

- 坐标准：同名元素中心差的中位数约 0.2pt。
- 慢：新连接拿到第一棵树要 0.9～1.2 秒，WDA 的 `iphone_list_elements` 只要 0.1～0.4 秒。
- 不全：只列可见、没被挡住的元素，没有容器和屏幕外的行，没有元素类型。
- **小组件这类跨进程视图的坐标是错的**，边框是相对小组件自身的，和 WDA 最多差 141pt。
- 锁屏时照样能用。画面变化后约 0.5～0.7 秒手机会主动推新树。
- WDA 真正停掉时还能不能用，没测过。

探针代码（`ax_probe.rs`、`debug ax`）和完整记录在 git stash「spike: remoteAXService 读元素树实验」里，没有提交。当时决定先不接入：收益只覆盖 WDA 不可用的短暂空档，维护没公开的协议不划算。

## 6. 能不能从电脑直接调用 testmanagerd

### 6.1 静态分析

本机没有 sudo，Debian 源里也没有 apfs-fuse。做法是在 `debian:trixie-slim` 容器里从源码编译 [apfs-fuse](https://github.com/sgan81/apfs-fuse)，把缓存的开发者镜像（`ddi/Image.dmg`，zlib 分块压缩，解开是 APFS，版本 27A5228h）只读挂载，把文件拷出来。testmanagerd 在 `usr/libexec/testmanagerd`，符号表没被剥掉，能直接看出「类 → 方法」。

launchd 配置（`Library/LaunchDaemons/com.apple.dt.testmanagerd.plist`）：

| 服务 | 类型 | 要求 |
|---|---|---|
| `com.apple.dt.testmanagerd.remote` | RSD（电脑能连） | `com.apple.private.dt.testmanagerd.client` |
| `com.apple.dt.testmanagerd.remote.automation` | RSD（电脑能连） | **`AppleInternal`** |
| `runner`、`remote.runner`、`uiprocess`、`control` | Mach 服务 | 只有手机上的进程能连 |

三类会话：

| 会话 | 类 | 方法 |
|---|---|---|
| 电脑经 `remote` 连上（Xcode、idevice 都走这个） | `XCTDHarnessSession` | 只有 `_IDE_*` 一类：启动测试计划、授权测试进程、崩溃报告、spindump、录制。**没有点击、输入、读元素树** |
| 手机上的测试进程 | `XCTestSession` | `_XCT_synthesizeEvent`、`_XCT_sendString`、`_XCT_fetchSnapshot`、截图、启动 App……要 `get-task-allow`，还要电脑端授权，否则报 `Not authorized for performing UI testing actions.` |
| 电脑经 `remote.automation` 连上 | `XCTDRemoteAutomationSession` | 约 90 个 `_XCTD_*` 方法，覆盖 WDA 的全部能力，还多了 `setAttribute`、录屏等。DTX 频道名是 `com.apple.testmanagerd.remote-automation` |

### 6.2 真机验证

在两台设备上用独立探针（idevice DTX 帧，自己读写）连这两个服务，同时抓手机系统日志：

| | `testmanagerd.remote`（对照） | `testmanagerd.remote.automation` |
|---|---|---|
| RSD 列表里有没有 | 有 | **有**（两台都有，标 `AppleInternal`） |
| TCP | 能连上 | 能连上 |
| DTX 握手 | 马上回 capabilities | **没有回应** |
| 开频道 | 成功 | 没有回应 |
| 调 `_XCTD_*` | `Unable to invoke -[XCTDHarnessSession _XCTD_beginSessionWithClientProtocolVersion:] - it does not respond to the selector` | 走不到 |
| 手机日志 | testmanagerd 接到会话 | 只有 `remoted … Got incoming connection`，之后 testmanagerd 一行都没有；30 多秒不回应也不断开 |

可以确定两点：`remote.automation` 被负责转交 RSD 连接的 `remoted` 按权限拦下了，在电脑端怎么改协议都绕不过去；普通通道上确实没有 UI 操作。

## 7. 参考：iCTRL

[OpenMinis/iCTRL](https://github.com/OpenMinis/iCTRL)（Apache-2.0）是一个用 Swift 写的 XCUITest runner，在手机上提供加密的 JSON-RPC（8200 端口），面向跑在手机上或局域网里的 agent。它同样要签名、挂开发者镜像、输入「UI 自动化」密码，NOTICE 里还记着做过 App 内签名又删掉了，和第 6 节的结论一致。

值得借鉴的地方：

- **runner 在手机上自己启动**，点图标或打开 `ictrl://start` 就行，不需要电脑拿着会话。第 8 节的实验就是从这一点来的。
- **点屏幕键盘输入**（`SoftKeyboardTyper.swift`），中文先打拼音、再点候选词。它的说明是：XCTest 打字时会挂一个虚拟硬件键盘，软键盘因此收起，有的输入栏把这当成编辑结束而吞字。这可以解释我们在高德遇到的吞字（现在靠 `verify_typed` 补救）。用 WDA 的点击也能做到。没有 WDA 时，用 HID 点键加 AX 定位键位，理论上也能输中文，但没验证过。
- 手势细节：长按时每 100ms 重报一次位置，否则微信会认成轻点；双击的两下放在同一个事件路径里，分成两次请求会超过系统约 250ms 的窗口。
- `page_source{visible_only, max_children}` 同时限制快照的深度和宽度，长信息流上中位数 416ms。
- 在手机上用 VideoToolbox 编 H.264，截图直接向 testmanagerd 要。要自己写 runner 才用得上。

## 8. 不挂 XCTest 会话启动 WDA

### 8.1 实验

停掉守护进程，用独立探针直接启动已安装的 xctrunner，不传任何环境变量和参数，再轮询 8100 端口的 `/status`：

| 启动方式 | 解锁时 | 锁屏时 |
|---|---|---|
| CoreDevice appservice `launch_application` | ✅ 3.6 秒就绪 | ❌ SpringBoard：`RequestDenied … for reason: Locked` |
| DVT processcontrol，不带 `ActivateSuspended` | — | ❌ 同样被 SpringBoard 拒绝，DTServiceHub 重试 4 次，47 秒后报错 |
| **DVT processcontrol + `ActivateSuspended: true`** | — | ✅ 0.9 秒返回进程号，**4.0 秒就绪** |

后两种方式拿到进程号后都马上断开了 DVT 连接，之后观察 3 分钟：进程号不变，`/status`、`/wda/activeAppInfo`、`/wda/screen`、`/source`、`/screenshot` 都正常。第一次 `/source` 约 9 秒，包括 testmanagerd 第一次加载 AX。

iPad 上那次失败和启动方式无关：手机端 amfid 报 `AppleMobileFileIntegrityError -400`，是签名的问题。

### 8.2 机制（手机日志）

- runner：`Found most recent test bundle …` → **`Synthesizing a test configuration for the test bundle`** → `Running tests...`。没收到配置时，XCTestCore 会自己生成一份。
- testmanagerd：`Received new test session connection from process with PID …` → `Enabling Automation Mode...` → `Finished loading AX.`，和有主机会话时一样。
- SpringBoard：带 `ActivateSuspended` 时是 `Bootstrapping … with intent background`，不走锁屏检查。
- runner 启动时会按一下 Home（`XCDeviceEvent page 12 usage 64`）。

现在的 `XCUITestService` 启动方式在锁屏时能用，就是因为 idevice 也带了 `ActivateSuspended`（`build_launch_env`）。

### 8.3 实验开关 `ICTL_WDA_DETACHED=1`

开关在 `~/.config/iphone-control/env` 里设，重启服务后生效。代码：

- `ictl-device`：`Tunnel::launch_in_background`（processcontrol，选项 `StartSuspendedKey: false`、`ActivateSuspended: true`、`KillExisting: true`，不带环境变量和参数）、`Tunnel::processes`（appservice `list_processes`）、`Tunnel::kill`（processcontrol `killPid:`）。
- `ictl-wda`：`WdaRunner::attach` 只等 `/status` ready。这样得到的 runner 没有任务，`alive()` 总是 true，`detached()` 是 true；丢掉它不会结束手机上的 WDA。
- `ictl-daemon`：`spawn_start` 挂好开发者镜像后，在同一条隧道上调 `prepare_detached`：
  - 开关开着：`state.json` 里的 `detached_wda`（UDID + 进程号）对应的进程还在、可执行文件是 WebDriverAgentRunner，就**直接接上**（`/status` 等 15 秒）；否则在后台启动一个（会先结束已在运行的 WDA，包括 XCTest 会话启动的），等最多 120 秒。就绪后记下进程号；接上的那个不应答就忘掉它，下一次重新启动。
  - 开关关着：记录里的那个还在就先结束它，再照原来的方式启动。不然它一直占着 8100，新 runner 的 HTTP 服务起不来。
- 健康检查不变：每 10 秒 `/status`，连续 3 次不应答就丢掉，重新启动。

### 8.4 守护进程里的验证（2026-11-12，iPhone16,2 / iOS 27.0.1）

| 场景 | 结果 |
|---|---|
| 锁屏时用开关启动 | 10 秒就绪（日志「已在后台启动 WDA（进程 N，不持有 XCTest 会话）」），记录写进 `state.json` |
| 截图（触发自动解锁）、`iphone_launch_app` 时钟 + `then=elements`、`iphone_tap_element`、`iphone_press_button lock` 后再截图 | 全部正常，锁屏后经 WDA 自动解锁 |
| 重启守护进程 | **接上同一个进程**（进程号不变），没有重新启动。就绪要等下一轮巡检（10 秒） |
| 同一个二进制关掉开关 | 先结束后台 WDA，再用 XCTest 会话启动，11 秒就绪，记录清掉 |
| 开着开关退出，直接换回 0.2.0 | 0.2.0 的 runner 替换掉了后台 WDA（进程号变了），11 秒就绪。旧版保存 `state.json` 时丢掉了这个字段。降级是安全的 |
| 手机重启后，签 WDA 的证书已被吊销 | 后台启动被拒：processcontrol 回 NSError code 2，手机日志里 amfid 报 `The identity used to sign the executable is no longer valid`。第一版没有触发自动重签，只会退避重试；改成和 XCTest 会话启动的 `unexpected response` 一样处理（`DeviceError::LaunchRefused` → `StartOutcome::Refused` → `maybe_heal_certificate`）后，3 秒查出证书不在团队里，12 秒重签完，接着启动 |

### 8.5 已知限制和没验证的

- **「UI 自动化」密码框靠手机系统日志识别**，等待和输入后恢复已在手机重启后验证；一直不输、等满 60 秒的那条路还没亲眼看到（见 8.6 节）。
- 证书被吊销时，手机没重启前签名校验有缓存，WDA 照样能起来；**重启后才起不来**。这对两种启动方式都一样。
- 守护进程停了 WDA 还在跑（这正是想要的效果），所以 `service stop` 不会停掉 WDA。
- 只测了几分钟。还要做长时间运行、反复锁屏和熄屏、手机重启、拔插 USB 的测试。
- 只在一台 iPhone、iOS 27.0.1 上验证过；iOS 17～26 和 iPad 都没测。
- 接上旧进程时只按「进程号 + 可执行文件名」判断。手机重启后进程号恰好撞上另一个 WDA 进程的可能性很小；就算撞上了，`/status` 等不到也会重新启动。
- 切换设备时，旧设备上的后台 WDA 继续跑，记录只留一台。
- 控制台画面（9100 MJPEG）和 `fast_stream`、`keep_stream_upright` 这些 appium settings 在接上旧进程时的表现没有专门测过。

### 8.6 识别「UI 自动化」密码框（2026-11-12）

现有方式靠 runner 经 XCTest 会话发回的调试日志（`entering wait loop … enabling automation mode`）认出密码框。这些日志不进系统日志，后台启动时收不到。改成看 testmanagerd 写进系统日志的几行，字符串来自开发者镜像 27A5228h 里的 testmanagerd：

| 行 | 含义 |
|---|---|
| `Enabling Automation Mode...` | 开始启用（等待） |
| `Finished enabling Automation Mode` | 启用完成 |
| `Failed to enable automation mode for test session with pid N: …`、`Failed to enable Automation Mode: …` | 失败 |

已授权时前两行几乎同时出现（真机上相隔 11 毫秒）。手机重启后要先在手机上输密码，输之前应该一直没有「完成」。

做法：后台启动 WDA 之前，先用 `Device::log_stream`（os_trace_relay，不含 debug 级别）打开系统日志流。`WdaRunner::attach` 一边等 `/status`，一边在后台读这个流，只看 testmanagerd 的行。

- 规则和 `WdaRunner::start` 一样：看到「开始」后把期限延到开始后 65 秒；3 秒还没「完成」就在日志里说 runner 在等密码。
- 到期时还在等，或者看到「失败」，就报 `WdaError::NeedsAutomationPin`。守护进程照旧进入 `NeedsAutomationPin`、推送通知、60 秒后重试。
- 日志流打不开不影响启动，只是认不出密码框，记一条警告。

已验证：

- 不重启时正常后台启动：两行都收到了，相隔 11 毫秒，没有误报，就绪时间不变。
- 手机重启后（2026-11-12，iPhone16,2 / iOS 27.0.1）：后台启动后 0.9 秒出现 `Enabling Automation Mode...`，3 秒后日志里报出「runner 在等手机上的「Enable UI Automation」密码」。57 秒时有人在手机上输入密码，随即出现 `Finished enabling Automation Mode`，2 秒后就绪。

还没验证：一直不输密码、等满 runner 的 60 秒之后 testmanagerd 会不会写「失败」。按代码，到期时还在等就报 `NeedsAutomationPin`，和现有方式一样推送通知、60 秒后重试。要验证得再重启一次手机，等 70 秒以上再输。

## 9. 方法和复现要点

- 跟手机打交道的实验都要**先停守护进程**（`systemctl --user stop iphone-control.service`，macOS 用 launchctl）：同一时间只能有一个 XCTest 会话和一个 WDA 的拥有者。做完马上恢复。
- 探针都是 scratchpad 里的独立 crate，用路径依赖锁定版本的 idevice 源码（rev `89702099`），在 `ictl-rust:bookworm` 容器里编译。主要用到：`RemoteServerClient::connect_rsd` + `make_channel` + `call_method` / `read_message`；DTX 帧要自己读写（参考 stash 里的 `ax_probe.rs`）。
- 手机系统日志用 `syslog_relay` 抓，比 os_trace 方便；按进程名或关键字过滤，去掉守护进程每 10 秒查 WDA 产生的 `Fetching AX attributes` 噪音。
- 锁屏时 SpringBoard 拒绝前台启动，报 `RequestDenied … Locked`，很容易被误判成别的问题。看到「启动失败」先看手机日志。
- WDA 的 HTTP 连接不会自己关，读应答要按 `Content-Length`，不能读到 EOF。
- 顺带发现的两件事：iPhone 上装着两个 WDA（另一个是别的团队签的）；testmanagerd 日志里有「551 pending harness sessions」，可能是反复启动 WDA 留下的连接，没确认。

## 10. 后续可做

按价值排：

1. 做长时间稳定性测试；顺带验证密码框等满 60 秒的那条路（8.6 节）。通过的话可以考虑改成默认，就绪判定和启动流程都会简单很多。
2. 输入：参考 iCTRL 改成点屏幕键盘，解决吞字。
3. 只有 iOS 27+ 的降级模式（HID + 截图 + appservice + springboard 方向 + remoteAXService），在 WDA 不可用时多撑一些功能。先解决 AX 跨进程坐标的问题，并实测 WDA 停掉时 AX 还能不能用。
4. 苹果放开 `remote.automation` 之前，「完全不要 WDA」这条路没有意义。新版 iOS 或 Xcode 出来时，可以照第 6 节的方法再查一次权限有没有变。
