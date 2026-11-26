# iOS 27 上不用 WDA（原生控制）

状态：0.2.7 起，iOS 27 及以上的手机默认走原生控制（#123）；`ICTL_IOS_BACKEND=wda` 可以改回 WDA。iOS 26 及以下照旧用 WDA。代码在 `crates/ictl-ax`、`crates/ictl-device/src/ax.rs` 和 `crates/ictl-daemon/src/tools/native.rs`，读界面的流程见[架构](../architecture.md)。相关：[wda-dependency-research.md](wda-dependency-research.md)（WDA 依赖调研）。

## 1. 为什么

- 主机按 Xcode 的方式回答 remoteAXService 的握手后，没有 WDA 也能读到元素树（iPhone 15 Pro Max，iOS 27.2.1）。早先「没有 WDA 读不到」，是探针没回答握手造成的。
- 点击、按键、Home 本来就能走 HID（iOS 27 的 `dtuhidd`），截图能走 screencaptureservice，输入中文能走剪贴板粘贴（#118、#122）。读界面是最后一块。

不用 WDA 省掉的：Apple ID 登录、免费描述文件和 7 天重签、证书被吊销、「UI 自动化」开关和重启后输密码（`NeedsAutomationPin`）、WDA 启动慢（10～60 秒）、卡死和重启。还要的：配对、开发者模式、开发者镜像。

## 2. 范围

- iOS 27 及以上的手机，**全部工具**在不装 WDA 的情况下可用，结果和措辞对 Agent 尽量不变；可以改回 WDA。
- 不做：iOS 26 及以下（没有 HID，只能 WDA）；双指缩放（本来就没有这个工具）；删掉 WDA 代码（低版本还要用，改回 WDA 时也照常重签）。正在用原生控制的设备不自动重签、也不查 Apple ID 会话，即使以前装过 WDA。

## 3. 每项能力怎么做

| 能力 | WDA | 原生 |
|---|---|---|
| 截图 | `/screenshot`，约 0.4 秒 | CoreDevice screencaptureservice，约 0.5 秒 |
| 读元素（`list_elements`、`then=elements`、按名字点击） | `/source` XML | remoteAXService 逐个元素读（走树）：请求类型 4 拿前台 App，再按层请求类型 5 的属性，最多 400 个元素、4 秒。字段：类型（属性 61，和 WDA 类型名一致）、文字 33/3、值 53、id 25、位置 21、点击点 1、traits 77、可用 27、子元素 8。转成 WDA 风格的 XML，交给 `ictl_wda::elements::parse_tree`，按名字找、遮挡、弹窗、密码栏的逻辑都复用 |
| 跨进程元素（小组件等） | `parse_tree` 换算相对坐标 | traits 标出宿主视图，加上宿主的位置；网页（WebKit WebContent 进程）的元素就是屏幕坐标，原样放置 |
| 系统弹窗、通知横幅、权限请求 | `/alert/text`、SpringBoard 的树 | 另读 SpringBoard 的应用元素（请求类型 1，带它的进程号）：除了状态栏，就是盖在 App 上的东西，最多 80 个、2 秒 |
| 点击、长按、滑动 | W3C actions | HID；按名字点用无障碍给的点击点 |
| 等界面稳定、界面变没变 | 轮询 `/source` | 同样轮询：连续两次读到的树一样、没有加载圈就算稳定；操作后先等 0.3 秒再读第一次 |
| 输入文字 | `/wda/keys` | 一律经剪贴板粘贴（HID 按键走硬件键盘的输入法，拼音键盘下会变成拼音）；自动点「允许粘贴」，读回输入框核对，马上还原剪贴板 |
| 键盘在不在 | 找 `Keyboard` 元素 | 同样找 `Keyboard`；键有时算 App 自己的元素，有 8 个以上 `Key` 也算 |
| Home、电源、音量 | XCTest 按键 / IOHID | HID；Home 没回到主屏幕时改从屏幕底部上滑 |
| 启动 App、前台 App | `/wda/apps/launch`、`/wda/activeAppInfo` | CoreDevice AppService 启动；前台 App 用请求类型 4 |
| 结束 App | `/wda/apps/terminate` | AppService 发 SIGKILL，等进程真的退出（最多 2 秒） |
| 打开链接 | `/url` | AppService 启动处理这个链接的 App 时带 `payloadURL`（同 `devicectl --payload-url`） |
| 锁屏状态 | `/wda/locked` + 树核对 | lockdown 的 `PasswordProtected`（要先建会话） |
| 自动解锁 | `ictl-unlock` 经 WDA 读密码栏、逐位点击核对 | 同一套 `ictl-unlock` 流程（`ictl_unlock::Screen`），读界面经无障碍、点击经 HID；停用标记和「每位之前确认还锁着」的规则不变 |
| 屏幕尺寸、方向 | `/wda/screen`、`/rotation` | 每次读界面记下尺寸；方向用 springboardservices |
| 控制台画面 | WDA 截图（低版本） | 手机自己的 HEVC（iOS 27 本来就用它） |
| 开发者工具（日志、崩溃、安装、网页） | 不经 WDA | 不变 |

## 4. 架构

- **`ictl-ax`**：remoteAXService 的 DTX 会话。握手 `_notifyOfPublishedCapabilities:` → 等 `deviceAPIVersion` → `clientNeedsAccessibility: true`；回答 `hostAPIVersion = 7`；手机每条要回执的消息都回空回执。自己读帧（idevice 的 `RemoteServerClient` 遇到解不开的消息会退出）。两层 NSKeyedArchive 解码成 `AXPTranslatorResponse`，只解用到的字段。`tree::assemble` 按阅读顺序拼树，`xml` 转成 WDA 风格的 XML。
- **会话归属**：和 HID 会话一样归 `Tunnel` 所有、常驻复用，空闲 60 秒关掉（`AX_IDLE`）。连接断了就重建，不重放操作。
- **守护进程**：`ICTL_IOS_BACKEND`（`auto`、`native`、`wda`）决定 iOS 27 及以上走哪条路，低于 27 一律 WDA。各工具在设备走原生时改走 `tools/native.rs`；自动解锁复用 `ictl-unlock`，原生一侧实现它的 `Screen`。
- **状态机和引导**：原生设备的「就绪」= 已配对 + 开发者模式 + 镜像已挂载 + 隧道 + 无障碍会话握手成功，没有 `NeedsSigning`、`WdaStarting`、`NeedsAutomationPin`。准备失败显示「准备手机失败，稍后重试」。自动重签和 Apple ID 会话检查只对用 WDA 的设备做（`supervise::wda_upkeep`）：正在控制的设备走原生控制时两样都不做，这一轮也不算数，换到用 WDA 的设备后下一轮就做；没连手机时会话检查照做（只要装过 WDA），重签不做；从没装过 WDA 的设备两样都不做。`setup` 对原生设备跳过 Apple ID 和签名。`status`、`doctor`、控制台都标明「原生」。

## 5. 对 Agent 可见的差别

- 元素粒度更粗：无障碍服务常把一行合成一个元素（「Wi-Fi、Home-WiFi」），WDA 还列出里面的文字和箭头。按包含的文字照样找得到，`list_elements` 的输出变短。
- 屏幕外的行不列：长列表要先滑到才找得到（WDA 会列出但点不到）。
- 输入一律粘贴，「允许粘贴」由守护进程点掉；输入框里原来就是同一段文字（Spotlight 留着上次的搜索词）时，结果会提示截图确认（#133）。

## 6. 风险和对策

| 风险 | 对策 |
|---|---|
| 协议和属性编号没公开，iOS 更新可能变 | 每个 iOS 大版本发布后跑一遍真机回归；准备不好时 `status` 报「准备手机失败」，可以设 `ICTL_IOS_BACKEND=wda` 改回 WDA（要 Apple ID）。不会自动退回 WDA |
| 推送漏了、来晚了 | 不靠推送：每次读界面都走树，有元素数和时间上限 |
| 无障碍运行时一直开着影响手机 | 只在有人用设备时保持会话，空闲 60 秒关掉 |
| 解锁把密码输错 | 和 WDA 同一套规则：停用标记、每一位之前确认还锁着、只试一次 |
| 和 WDA 并存，代码变复杂 | 低版本路径不动；原生的差别集中在 `tools/native.rs` |

## 附：真机上发现的（iPhone 15 Pro Max，iOS 27.2.1）

- **推送只在前台 App 变化时来**（以及刚连上时）：App 内换页（设置里点进「关于本机」再返回）不推送，所以每次读界面都自己走树。主动请求整棵树（类型 11，不带参数）会让手机断开连接。
- **走树要流水线**：一个元素一次请求、一来一回约 40 毫秒，57 个元素要 2.5 秒。答复里带着元素对象（`associatedTranslationObject`），所以一层的属性请求一次发出、按对象收答复，再用 `tree::assemble` 拼起来：同一屏 0.45～1.2 秒。
- **HID 的屏幕媒体流开着时，锁屏对无障碍是空的**：任何会话读到的都只有应用元素自己。读锁屏前先关 HID 会话，元素 0.1～0.7 秒后回来；下一次点击再重开（约 0.5 秒）。解锁后等 0.7 秒再操作，否则锁屏滑开的动画会吞掉点击。
- **锁屏的密码栏是 `TextField`**（值「共6位数，已输入0位」），转 XML 时改标成 `SecureTextField`，解锁逻辑照旧认得。
- **SpringBoard 问的问题没有 Alert 元素**：「“设置”想从“dtpasteboardd”粘贴」出来时前台变成 SpringBoard，只有两段文字和两个按钮。前台是 SpringBoard、元素不多、至少两个按钮、没有图标/键盘/密码栏/单元格时，转 XML 时包一层 `Alert`，弹窗提醒和自动点「允许粘贴」都靠它。
- **键盘的键有时算 App 自己的元素**，合成不出 `Keyboard`；输入前判断键盘时，有 8 个以上 `Key` 也算。
- **锁屏状态**读 lockdown 的 `PasswordProtected` 要先建会话，否则报 `this request was prohibited`。
- **自动解锁的耗时**：关 HID 后等锁屏可读约 1.5 秒，每位数字点击加核对约 0.7 秒，6 位密码一共约 8～9 秒（#135 起结果里会写明）。
