# 方案：iOS 27 上不用 WDA（草案）

2026-11-15。状态：已在分支 `feat/ios27-native` 实现（默认原生），真机验证中。相关：[wda-dependency-research.md](wda-dependency-research.md)（WDA 依赖调研）、分支 `spike/ax-service-2` 的 `docs/dev/ax-spike-results.md`（remoteAXService 三轮实验）。

## 1. 为什么现在提

- 实验第三轮（iPhone 15 Pro Max，iOS 27.2.1）：主机按 Xcode 的方式回答 remoteAXService 的握手后，没有 WDA 也能读到完整的元素树。前两轮「没有 WDA 读不到」是探针没回答握手造成的。
  - **更正（实现后真机复测）**：手机只在**前台 App 变化时**（以及刚连上时）主动推送整棵树；App 自己换页（设置里点进「关于本机」再返回）不推。所以读界面一律自己「走一遍」：请求类型 4 拿前台 App，再逐个元素请求类型 5 的属性（和 Pinky 一样，最多 400 个、4 秒）。主动请求整棵树（类型 11，不带参数）会让手机直接断开连接。
- 点击、按键、Home 早已经能走 HID（iOS 27 的 `dtuhidd`），截图能走 screencaptureservice，输入中文能走剪贴板粘贴（#118、#122）。读界面是最后一块。
- [Pinky](https://github.com/DIYgod/pinky)（Apache-2.0）在 iOS 27 上已经这样做：手机上什么都不装，README 写「除了双指缩放都能用」（iPhone 13 mini，iOS 27.0.1）。下文标「Pinky」的做法都出自它的代码和 `docs/notes/ios.md`。

不用 WDA 能省掉的：Apple ID 登录、免费描述文件和 7 天重签、证书被吊销、「UI 自动化」开关和重启后输密码（`NeedsAutomationPin`）、WDA 启动慢（10～60 秒）、卡死和重启、`ictl-sign`/`plume_core`/omnisette 这一串依赖（对只有 iOS 27 手机的用户）。还要的：配对、开发者模式、开发者镜像。

## 2. 目标和不做的

- 目标：iOS 27 及以上的手机，**全部工具**在不装 WDA 的情况下可用，结果和措辞对 Agent 尽量不变；可以按设备切回 WDA。
- 不做：iOS 26 及以下（没有 HID，仍然只能 WDA）；双指缩放（我们本来就没有这个工具）；把 WDA 代码删掉（低版本还要用）。

## 3. 每项能力怎么实现

「已验证」= 我们在真机上测过；「Pinky」= Pinky 已在真机上做到、我们没测；「待测」= 都没验证。

| 能力 | 现在（WDA） | 不用 WDA 时 | 状态 |
|---|---|---|---|
| 截图 | `/screenshot` 约 0.4 秒 | CoreDevice screencaptureservice（已是后备），约 0.5 秒 | 已验证 |
| 读元素（`list_elements`、`then=elements`） | `/source` XML | remoteAXService 逐个元素读（走树），字段和推送的树一样：类型（属性 61，和 WDA 类型名一致）、文字 33/3、值 53、id 25、位置 21、点击点 1、traits 77、可用 27、子元素 8 | 已实现；字段对照已验证（设置、主屏） |
| 跨进程元素（小组件等） | WDA 原始坐标也是相对的，`parse_tree` 已换算 | traits 第 34 位标出宿主视图，加上宿主的位置（Pinky `observe_rax.go`） | Pinky |
| 系统弹窗、通知横幅、权限请求 | `/alert/text`、SpringBoard 的树 | 另读 SpringBoard 应用元素（请求类型 1，带它的 pid）：子元素里除了状态栏（有专门的 trait 位）就是盖在 App 上的东西（Pinky `springboard.go`） | Pinky |
| 点击、长按、滑动 | W3C actions | HID（已有后备）。按名字点用 AX 的点击点 | 已验证 |
| 等界面稳定、界面变没变 | 轮询 `/source` | 和 WDA 一样轮询：连续两次读到的树一样、没有加载圈就算稳定；操作后先等 0.4 秒再读第一次（推送靠不住，见第 1 节的更正） | 已实现 |
| 输入文字 | `/wda/keys` | 全部走剪贴板粘贴（拼音键盘下按 ASCII 键会变成拼音输入，已见过）；读回输入框（AX 的值）核对 | 粘贴已验证 |
| 「允许粘贴」弹窗 | 不会出现 | 用 AX 在 SpringBoard 的元素里找到弹窗，点「允许粘贴」，等输入框的值变了再还原剪贴板（不再需要 Agent 去点、等 45 秒） | 弹窗出现已验证（备忘录）；自动点待做。Pinky 在设置的搜索框里没遇到弹窗 |
| 键盘在不在、哪个输入框有焦点 | 找 `XCUIElementTypeKeyboard` | 树里的 Keyboard 类型元素；应用的焦点元素（属性 19）、`isFocused`（28） | Pinky |
| Home、电源、音量 | XCTest 按键 / IOHID | HID（已有） | 已验证 |
| 启动 App、等到前台 | `/wda/apps/launch` | CoreDevice AppService `launch_application`；前台 App 用请求类型 4（frontmost app） | 启动已有；前台判断待做 |
| 结束 App | `/wda/apps/terminate` | AppService 发信号（已在实验里用过 SIGKILL） | 已验证 |
| 打开链接（`open_url`） | `/url` | AppService 启动目标 App 时带 `payloadURL`（同 `devicectl --payload-url`），先查清这个链接由哪个 App 打开 | Pinky |
| 锁屏状态 | `/wda/locked` + 树核对 | lockdown 的 `PasswordProtected`（Pinky：iOS 27 的镜像没实现 CoreDevice getlockstate）；屏幕亮不亮看 CoreDevice 显示信息里的背光状态 | Pinky |
| 自动解锁 | WDA 读密码栏、逐位点击核对（`ictl-unlock`） | 上滑叫出密码键盘，AX 确认键盘在（实验里读到过「密码栏」和数字键），用虚拟 HID 键盘逐位敲，每位之前确认还锁着，最后看 lockdown（Pinky `power.go`）。保留现在的停用标记和「从第 2 位起先写停用标记」 | Pinky；我们要重写 |
| 屏幕尺寸、方向 | `/wda/screen`、`/rotation` | 截图像素 ÷ 机型倍数；方向用 springboardservices（已有 `interface_rotation`）或 AX 属性 129 | 已有部分 |
| 控制台画面 | WDA MJPEG（低版本） | 手机自己的 HEVC（iOS 27 已默认用它） | 已在用 |
| 开发者工具（日志、崩溃、安装、网页） | 本来就不经 WDA | 不变 | — |

## 4. 架构

### 4.1 新 crate：`ictl-ax`

- **会话**：DTX 连接 `com.apple.accessibility.axAuditDaemon.remoteAXService`；握手 `_notifyOfPublishedCapabilities:` → `deviceAPIVersion`（等答复）→ `clientNeedsAccessibility: true`；回答 `hostAPIVersion = 7`；手机每条要回执的消息都回空回执。idevice 的 `RemoteServerClient` 遇到解不开的消息会退出，探针是自己读帧，正式代码沿用这个做法或给 idevice 提补丁（idevice 的 AI.md 要求 PR 描述由作者亲手写）。
- **解码**：两层 NSKeyedArchive → `AXPTranslatorResponse`。引入 `ns-keyed-archive` crate（idevice 已经依赖它）。只解我们用的字段，未知字段忽略；解码是纯函数，用脱敏后的真机数据做测试夹具。
- **请求**：类型 1（某进程的应用元素，读 SpringBoard 用）、2/5（属性）、4（前台 App）。一次只发一个请求。类型 11（整棵树）不带参数会让手机断开连接，不用。
- **转换**：输出现在的 `ictl_wda::elements::Tree`/`Element`（类型名、文字、值、中心点、大小），这样 `find`、遮挡判断、`describe`、`close_to` 都能复用。为此把 `Tree`/`Element` 和与 WDA 无关的逻辑从 `ictl-wda` 挪到一个两边都依赖的地方（`ictl-protocol` 或新的 `ictl-ui`）。
- **会话归属**：和 HID 会话一样归 `Tunnel` 所有、常驻复用（空闲 60 秒关掉）。每次读界面都走一遍树，不用推送的树（推送只在前台 App 变化时才来）。连接断了就重建，不重放操作。

### 4.2 守护进程：按设备选「后端」

现在工具代码里大约 56 处直接用 `WdaClient`（`tools/`、`core.rs`、`supervise.rs`），`ictl-unlock` 有 52 处。做法：

- 定义一组守护进程内部的操作（读树、截图、点、滑、按键、输入、启动、结束、开链接、锁屏状态、解锁、前台 App），WDA 和「原生」（AX + HID + CoreDevice）各实现一份；工具代码只调这组操作。现有的 HID 后备逻辑并进原生那一份。
- 选择：iOS 27 及以上默认原生，低于 27 用 WDA；`ICTL_IOS_BACKEND=wda|native|auto`（按设备也可以设）随时切回。
- `ictl-unlock` 拆成「判断/输入密码的流程」和「读界面、按键」两层，流程不变，下层各有 WDA、原生两种。

### 4.3 状态机和引导

- 原生设备的「就绪」= 已配对 + 开发者模式 + 镜像已挂载 + 隧道 + AX 会话握手成功。不再有 `NeedsSigning`、`WdaStarting`、`NeedsAutomationPin`。
- 重签、Apple ID 会话检查只对还在用 WDA 的设备做；全部设备都是原生时，`setup` 跳过 Apple ID 和签名两步。
- `status`、`doctor` 说明这台用的是哪种后端。

## 5. 对 Agent 可见的差别

- 元素粒度不同：AX 把一行合成一个元素（「Wi-Fi、Derrors-5G」），WDA 还列出里面的文字和箭头。按包含文字查找都找得到；`list_elements` 的输出会变短。
- 屏幕外的行 AX 不列：长列表要先滑到才找得到（WDA 会列出但点不到，影响不大）。
- 时间：推送的树在变化后约 0.3 秒先来通知、1～2 秒内来完整的树（WDA 在时测的；没有 WDA 时要用常驻 HID 会话重测）。 **更正**：没有 WDA 时 App 内换页不推送，见第 1 节。
- 粘贴弹窗由守护进程自己点掉，`type_text` 的结果不再要求 Agent 去点。

## 6. 风险

| 风险 | 对策 |
|---|---|
| 协议和属性编号没公开，iOS 更新可能变 | 握手失败或解码不出必需字段时，这台设备自动退回 WDA（装了的话）或报清楚原因；每个 iOS 大版本发布后跑一遍回归 |
| 推送漏了、来晚了 | 不靠推送：每次读界面都走树（类型 4 + 类型 5），有 400 个元素、4 秒的上限 |
| 无障碍运行时打开后影响手机（耗电、App 行为） | 只在有人用设备时保持会话，空闲关掉（和 HID 会话一样 30 秒） |
| 系统弹窗读不全 | SpringBoard 那一读失败时回到截图；先在常见弹窗（权限、粘贴、提醒事项到期）上验证 |
| 解锁失败把密码输错 | 沿用现有的停用标记；每一位之前确认还锁着；只试一次 |
| 和 WDA 并存，代码变复杂 | 后端接口一处定义；低版本路径不动，只把工具代码改成调接口 |
| 借用 Pinky 的做法 | Apache-2.0，可以参考和移植，移植的代码保留出处并记进 THIRD_PARTY.md |

## 7. 分阶段

每一步都能单独合并，原来的 WDA 路线一直可用。

1. **`ictl-ax` 和读元素后备**（约 1～1.5 周）：会话、握手、解码、转成 `Tree`、跨进程坐标、SpringBoard 弹窗。先只在 **WDA 不可用时**用：`list_elements`、`tap_element`、`wait_for`、`then` 等稳定、HID 粘贴的弹窗自动点。验收：设置、主屏（小组件）、备忘录、微信等 5 个界面和 WDA 对照，按名字点一致；粘贴不再需要 Agent 点弹窗。
2. **后端接口和原生实现**（约 1.5～2 周）：把工具代码改成调接口；原生实现启动/前台、开链接、锁屏状态、解锁；状态机加原生设备。开关默认关（`ICTL_IOS_BACKEND=auto` 仍按 WDA）。验收：现有回归用例（`docs/dev/regression-cases.md`）在原生后端上全部过；`ictl-soak` 长测 8 小时（含锁屏、重启守护进程、拔插）。
3. **默认切换**（约 0.5 周）：iOS 27 设备默认原生；`setup` 在只有原生设备时跳过 Apple ID 和签名；文档、网站、更新日志。验收：一台没装过 WDA 的 iOS 27 手机从 `setup` 到 Agent 完成任务。

阶段 1 本身就有用（WDA 掉线时也能读界面），做完再决定要不要进阶段 2。

## 8. 需要你决定的

1. 方向：按第 7 节分阶段做，还是只做阶段 1 当后备。
2. iOS 27 设备默认切到原生的时机：阶段 3 一完成就切，还是先作为可选项跑一个版本。
3. 已经签好 WDA 的 iOS 27 手机：切到原生后 WDA 留着当备用（重签继续），还是不再管它。
4. 解码和协议部分是参考 Pinky 自己写，还是直接移植它的 Go 代码改写成 Rust（后者快一些，要保留 Apache-2.0 的出处说明）。
