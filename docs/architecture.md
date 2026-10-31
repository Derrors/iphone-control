# 架构

## 1. 总览

```
┌──────────────────────── Linux 主机 / 容器 ─────────────────────────────────────┐
│                                                                               │
│  Claude Code ──stdio──▶ iphone-control mcp ──unix socket──┐                   │
│  （每个会话一个，短命）    （ictl-mcp，无设备状态）            │                   │
│                                                           ▼                   │
│                         iphone-control daemon（ictl-daemon，常驻，唯一持有设备）  │
│                         ┌───────────────────────────────────────────────────┐ │
│   usbmuxd（系统包）◀─看护─┤ 设备状态机   usbmux 监听   重签调度   通知          │ │
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
   │ iPhone（iOS 17+，开发者模式）          │
   │ 开发者镜像（Cryptex，iOS 27 含 dtuhidd）│
   │ WebDriverAgent（免费 Apple ID 签名）   │
   └─────────────────────────────────────┘
```

和 toolkit 的根本区别：toolkit 每条 HID 命令都要重新建一次隧道（3～5 秒），WDA 端口转发要单独一个进程，各工具各建各的隧道。D 方案里**守护进程独占设备**，隧道、HID 会话（含显示媒体流）、WDA 会话都常驻复用；MCP 进程只是一个薄客户端。

## 2. 进程模型

| 进程 | 生命周期 | 职责 |
|---|---|---|
| `iphone-control daemon` | 常驻，容器入口 | 唯一和设备通信的进程。维护设备状态机，对外提供本地控制接口 |
| `iphone-control mcp` | 每个 agent 会话一个 | MCP stdio 服务，把工具调用翻译成对守护进程的请求 |
| `usbmuxd` | 守护进程的子进程 | 系统包，GPL-3.0，独立进程运行，不链接 |
| `iphone-control doctor/setup/sign/...` | 一次性 | 运维命令，也通过控制接口或直接调用库 |

**控制接口**：Unix socket（默认 `/run/iphone-control/ctl.sock`，权限 0600），JSON 行协议。理由：

- MCP 进程由 `docker exec -i` 拉起，短命且可能同时有多个；设备连接（尤其是 HID 需要的显示媒体流、WDA 的 XCTest 会话）只能有一个持有者。
- 不开 TCP 端口，避免重蹈 `ios forward` 监听所有网卡的覆辙。WDA 本身也通过 idevice 的 provider 直连设备端口，**本机不再监听 8100**。如需给外部工具用，`iphone-control wda-bridge` 显式开启，只绑 127.0.0.1。

## 3. 守护进程状态机

每台设备一个状态机（先支持单设备，结构上允许多设备）。

```
          ┌─────────┐ usbmux Attached  ┌──────────┐ 有配对记录    ┌──────────┐
  ───────▶│ Absent  │─────────────────▶│ Attached │─────────────▶│  Paired  │
          └─────────┘◀─────────────────└──────────┘              └────┬─────┘
               ▲        Detached / 掉线        │ 无配对记录              │ 开发者模式已开
               │                              ▼                        ▼
               │                        NeedsTrust（提示用户点信任） ┌──────────┐
               │                                                    │ Tunneling│◀─┐
               │                                                    └────┬─────┘  │
               │                    PasswordProtected：锁屏/BFU         │ RSD 握手成功
               │                    → NeedsUnlock（BFU 必须人工）        ▼          │
               │                                                    ┌──────────┐  │ 隧道断
               │                                                    │ DdiCheck │  │
               │                                                    └────┬─────┘  │
               │                                    未挂载 → install_ddi │ 已挂载    │
               │                                                         ▼         │
               │                                                    ┌──────────┐  │
               │        「Enable UI Automation」超时 → NeedsAutomationPin │WdaStarting│──┘
               │        （推送通知，60 秒重试，不指数退避）             └────┬─────┘
               │                                                         │ /status ready
               │                                                         ▼
               │                                                    ┌──────────┐
               └────────────────────────────────────────────────────│  Ready   │
                                                                    └──────────┘
                                                                    子状态：Locked / Unlocked
```

要点（每一条都来自 toolkit 实际踩过的坑）：

- **热插拔**：容器里没有 udev，usbmuxd 收不到热插拔事件。守护进程同时做两件事：订阅 usbmuxd 的 `Listen` 事件；并每 10 秒检查 sysfs `/sys/bus/usb/devices/*/idVendor` 是否有 `05ac`。连续 3 次"USB 上有 Apple 设备但 usbmuxd 看不到"就重启 usbmuxd 子进程。
- **隧道**：CoreDeviceProxy + jktcp 用户态 TCP 栈（idevice `tunnel_tcp_stack`），不需要 root，不创建 TUN 网卡。锁屏且从未解锁过（BFU）时报 `PasswordProtected`，进入 `NeedsUnlock`，只能人工解锁。
- **开发者镜像**：每次进入 `DdiCheck` 都先 `installed_ddi()` 查询（toolkit 实测已挂载时约 1 秒），未挂载才走 `install_ddi()`（个性化 + TSS）。手机重启后镜像会掉，所以这一步不能省。
- **WDA 就绪判定**：必须同时满足"本进程持有的 XCTest 会话存活"和 `/status` 返回 ready。toolkit 踩过：host 侧 runner 退出后，手机上残留的 WDA 还会响应几十秒再自杀，单看 `/status` 会误判。
- **自动化密码框**：手机重启后首次启用 UI 自动化会弹「为 XCTest 输入 iPhone 密码」，没人输就报 `Timed out while enabling automation mode`。识别到这个错误：进入 `NeedsAutomationPin`、只推送一次通知、保持 60 秒重试；恢复后再推送一次。
- **退避**：其它 WDA 启动失败按 60s → 120s → 300s 退避，上限 5 分钟；回到 `Absent` 时重置。

## 4. 模块划分

| crate | 职责 | 主要依赖 |
|---|---|---|
| `ictl-device` | `Device` 句柄：usbmux 发现与监听、lockdown、配对状态、开发者模式状态、隧道（`TunnelHandle`）、开发者镜像、HID 会话（持有显示媒体流）、截图、App 列表/启动/结束、安装 IPA | `idevice`（锁定版本） |
| `ictl-wda` | `WdaRunner`（XCUITestService 启动/停止）、`WdaClient`（会话、点击、W3C actions、keys、source、screenshot、appium settings）、元素树解析（纯函数，可单测） | `idevice`（`xctest`、`wda`）、`quick-xml` |
| `ictl-sign` | Apple ID 登录（含双重认证交互）、会话持久化、注册设备、App ID、描述文件、签名 `.app`/`.ipa`（**包括 `.xctest`**） | 内置的 `plume_core`/`plume_utils`/`omnisette`（vendored，MPL-2.0/MIT），`apple-codesign`（打补丁） |
| `ictl-unlock` | 锁屏密码键盘定位、逐位输入并核对密码栏、失败即停用 | `ictl-wda` |
| `ictl-daemon` | 状态机、usbmuxd 子进程看护、重签调度、通知（Bark/Telegram）、控制接口服务端 | 以上全部、`tokio` |
| `ictl-mcp` | MCP stdio 服务，13 个 `iphone_*` 工具；截图缩放/裁剪 | `rmcp`（`server`、`transport-io`）、`image` |
| `apps/iphone-control` | CLI 入口：`daemon`、`mcp`、`doctor`、`setup`、`login`、`sign`、`resign`、`status`、`wda-bridge` | `clap` |

依赖方向：`apps` → `ictl-daemon` / `ictl-mcp` → `ictl-unlock` → `ictl-wda` → `ictl-device`；`ictl-sign` 只被 `ictl-daemon` 和 `apps` 使用。`ictl-mcp` 不依赖 `ictl-device`，只依赖控制接口的协议类型（放在 `ictl-daemon::protocol`，后续可拆成 `ictl-proto`）。

## 5. 截图与坐标

- 坐标单位统一用**点（pt）**，与 WDA 一致；屏幕点尺寸从 WDA `/wda/screen` 读取并缓存，WDA 不可用时用设备型号表兜底（不写死 430×932）。
- HID 坐标是 0～65535 归一化，换算 `x / w * 65535`。
- 截图优先走 WDA（toolkit 实测约 0.4 秒）；WDA 不可用时走 CoreDevice `screencaptureservice`（不需要签名）。
- 区域放大：从原始像素截图裁剪，长边 < 600 放大到 600，> 1200 缩到 1200，返回倍数和坐标换算式（三位有效数字）。

## 6. 数据与秘密目录

全部在一个数据根目录下（默认 `/var/lib/iphone-control`，容器里挂卷）：

| 路径 | 内容 | 权限 | 进 git / 进镜像 |
|---|---|---|---|
| `lockdown/` | 配对记录（usbmuxd 读写，即 `/var/lib/lockdown`） | 0700 | 否 |
| `account/` | Apple ID 会话、开发私钥、证书 | 0700 | 否 |
| `profiles/` | 当前描述文件 | 0700 | 否 |
| `ddi/` | 开发者镜像缓存（运行时下载的苹果文件） | 0755 | **否，不得分发** |
| `wda/` | 未签名 WDA.ipa（来自本项目 Release，校验 sha256）、当前签好的包 | 0755 | 未签名包作为 Release 附件 |
| `secrets/passcode` | 锁屏密码（仅启用自动解锁时） | 0600 | 否 |
| `notify.toml` | 通知渠道配置 | 0600 | 否 |
| `logs/` | 日志，按大小轮转 | 0755 | 否 |
| `state.json` | 状态机快照、自动解锁停用标记、上次重签结果 | 0600 | 否 |

配置文件 `config.toml`（数据根目录下）只放非秘密项：UDID（可空，自动探测）、代理、通知开关、重签阈值、自动解锁开关。

## 7. 重签与通知

- 守护进程每天检查一次：描述文件剩余 < 2 天才重签；同时**每天验证一次 Apple ID 会话**（toolkit 的教训：只在要重签时才发现登录过期，只剩 2 天处理）。
- 重签流程：`plume_core` 取新描述文件 → `ictl-sign` 签名（含 `.xctest`）→ `installation_proxy` 安装 → 重启 WDA。安装成功后才替换旧文件。
- 通知事件：重签失败、Apple ID 会话失效、需要输入自动化密码、WDA 恢复、自动解锁失败已停用、手机离线超过 10 分钟。同一事件不重复推送，恢复时推送一次。
- 容器整个挂掉时守护进程发不了通知，这部分仍需宿主机侧的 systemd 单元（可选，放在 `contrib/`）。
