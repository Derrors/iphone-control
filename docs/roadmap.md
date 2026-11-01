# 路线图

每个阶段都有明确的完成标准；未达标不进入下一阶段。toolkit 在阶段 7 之前一直是正式版本。

## 完成情况

2026-11-01。阶段 0～6 已在真机（iPhone 15 Pro Max，iOS 27.0.1）通过（第 0 步见 [spike-results](spike-results.md)）；中间一列是已实现的内容（有离线单测）和真机结果，右列是仍待真机确认或未完成的。

| 阶段 | 代码 | 仍待真机确认 / 未完成 |
|---|---|---|
| 0 spike | ✅ 5 项全过：TSS/挂载、HID Home 键、截图、30 分钟隧道、WDA 启动与读回 | 手机重启后补跑正式 `debug ddi` 挂载；HID 点击/drag/键盘；隧道锁屏 |
| 1 设备核心库 | usbmux 发现、sysfs 兜底、配对、开发者模式、CoreDeviceProxy 用户态隧道 + RSD、cryptexd 查询/挂载、DDI 下载缓存、截图、HID（持有媒体流）、列 App/安装；WDA 直连设备端口的 HTTP 客户端、会话自动重建、元素树解析、XCTest 启动 runner。完成标准的示例程序 `iphone-control debug e2e`（原 `ictl-spike e2e`）已在真机通过（9.8 秒；WDA 和 HID 两条路线的点击与 Home 都以前台 App 变化确认生效）。自己签的 WDA 已在真机安装，守护进程拉起后 10 秒就绪，e2e 同样通过 | 500KB 元素树经 jktcp 读回。（「Enable UI Automation」已确认：超时报 `Timed out while enabling automation mode.`，锁屏时报 `com.apple.LocalAuthentication Code=-4`；idevice 解码不了这条错误，从 runner 调试日志识别） |
| 2 守护进程 | 状态机、usbmuxd 子进程看护、WDA 就绪判定与退避、密码框 60 秒重试、控制 socket、事件日志轮转、state.json。正式容器真机已过：冷启动 10～12 秒；杀 usbmuxd 约 11 秒恢复；拔插 USB 约 40～50 秒恢复（sysfs 兜底重启 usbmuxd）；手机重启 → 61 秒内进入「需要输入密码」并通知 → 输密码后 10 秒就绪 | 24 小时内存（2026-11-01 13:13 开始采样）；拔插恢复可再省 10 秒（重启 usbmuxd 后当轮重新列设备）；iOS 27 上开发者镜像重启后仍在，从未挂载开始的挂载流程没测到 |
| 3 MCP | 13 个工具，参数与返回文字对齐 toolkit；区域放大；HID 后备。toolkit 的回归脚本 `tests/device/client.py` 原样跑通（13 步，截图确认生效）。Claude Code 已切到本项目 MCP：元素场景（设置 › 显示与亮度 › 自动锁定 改 5 分钟再改回）和纯坐标场景（开心消消乐 392 关，16 步通关 3 星）都完成 | 无（锁屏误判已修，见回归用例 #20、#21） |
| 4 签名与重签 | vendor plume_core/plume_store/omnisette/apple-codesign（plume_utils 未 vendor，bundle 逻辑改写进 `ictl-sign`）；登录（2FA、短信）、注册设备、App ID、描述文件、深度优先签名（XCTest 无 entitlements）、签后自检、定期重签（每 6 小时检查）、每日会话验证。真机已验证：登录（2FA）→ 签名 → 安装 → WDA 启动（XCTest 不带 entitlements 可以） | 强制重签后 WDA 自动恢复；会话失效通知；修复提交 Impactor 上游未做 |
| 5 通知 | Bark、Telegram，事件去重（重签/会话失效 20 小时冷却）；`iphone-control notify-test` 每类事件各发一条。Bark 实测 7 类全部收到；守护进程实际触发的「需要输入密码」「已恢复」也已在手机重启测试中走通 | Telegram 未实测（没有配置） |
| 6 自动解锁 | 规则全部实现，用假锁屏验证（键盘常驻树中、首位不生效重来、中间位被吞即停、输错一次即停用、残留输入用删除键清）。真机 6 种场景各 5 次全部成功、全程无输错：锁屏后 0 秒 15.3～16.0 秒（含有意等的 3 秒）、3/30/70 秒约 12.3 秒、键盘留 2 位残留 13.8～14.6 秒、自然自动锁屏（30 秒）11.8～12.4 秒；配置错误密码只输错一次即停用并推送，恢复密码 + unlock-reset 后正常 | 无 |
| 7 打包 | 多阶段 Dockerfile（单二进制 + Debian usbmuxd；构建缓存挂载，首次构建约 2 分钟；release 二进制 strip）、镜像带 cargo-about 生成的第三方许可证清单（`docker/about.toml` 白名单以外的许可证让构建失败）、compose（去掉 `privileged`：`/dev/bus/usb` 绑定挂载 + `device_cgroup_rules: "c 189:* rmw"`，`init: true`，`stop_grace_period: 30s`）、.env.example、setup/doctor、GitHub Actions（fmt/clippy/test 锁 1.99.1、MSRV 1.88 检查、gitleaks、镜像构建用 GHA 缓存） | 新 compose 设置下的 USB 拔插已真机复测（拔线后离线、插回 sysfs 兜底重启 usbmuxd，约 50 秒内就绪）；macOS CI 编译 WDA、多架构镜像、SBOM、镜像发布未做；数据目录归属（PUID/PGID）未做，容器内以 root 运行 |

已知简化：
- `iphone_status` 在 toolkit 的输出后多一行「守护进程: …」。
- 开发者模式状态只在未就绪时检查；就绪后关掉开发者模式要等 WDA 掉线才会发现。

## 阶段 0：真机验证（spike）

见 [spike-plan.md](spike-plan.md)。

**完成标准**：`spike/RESULTS.md`（现为 [spike-results.md](spike-results.md)）给出 5 项结论，并按判定表做出继续/折中/暂停的决定。

## 阶段 1：设备核心库（`ictl-device`、`ictl-wda`）

- `Device`：发现、监听、配对状态、开发者模式、隧道、镜像、HID 会话（持有媒体流）、截图、App
- `WdaRunner` + `WdaClient`：启动/停止 runner、会话自动重建、W3C actions、appium settings 读写
- 元素树解析做成纯函数，用保存的 XML 样本做单测（样本脱敏）

**完成标准**：一个示例程序在真机上依次完成"建隧道 → 确认镜像 → 启动 WDA → 截图 → 读元素 → 点击「设置」→ Home"；单测覆盖下方回归用例中与解析相关的条目。

## 阶段 2：守护进程（`ictl-daemon`）

- 设备状态机（architecture §3）、usbmuxd 子进程看护、sysfs 兜底
- 控制接口（Unix socket，JSON 行协议）
- 日志轮转

**完成标准**：
- 冷启动到 Ready ≤ 90 秒
- 拔插 USB、杀掉 usbmuxd、手机重启三种场景都能自动回到 Ready（手机重启场景以出现 `NeedsAutomationPin` 并在人工输密码后恢复为准）
- 连续运行 24 小时无内存增长

## 阶段 3：MCP 服务（`ictl-mcp`）

- 13 个 `iphone_*` 工具，名字、参数、返回文本与 toolkit 一致
- 截图区域放大
- 错误信息走 MCP 工具错误，文字可读

**完成标准**：toolkit 的端到端回归脚本（工具步骤 JSON）原样跑通；把 Claude Code 的 MCP 配置从 toolkit 切到本项目后，开心消消乐一局能打完（纯坐标场景）、设置里改自动锁定能完成（元素场景）。

## 阶段 4：签名与重签（`ictl-sign`）

- vendor `plume_core`、`plume_store`、`omnisette`（保持原许可证文件头）和打过权限补丁的 apple-codesign；`plume_utils` 不 vendor，bundle 逻辑改写进 `ictl-sign/src/bundle.rs` 并修 `.xctest`
- `iphone-control login`（交互式密码 + 双重认证）、`sign`、`resign`
- 守护进程内部每 6 小时检查（`RESIGN_CHECK_HOURS`）：描述文件剩余 < 2 天重签；每日验证 Apple ID 会话
- anisette 服务地址可配置

**完成标准**：用免费 Apple ID 从未签名 WDA 一路签到装好，`.xctest` 带签名，WDA 能启动；强制重签后 WDA 自动恢复；人为让会话失效能收到通知。修复提交到 Impactor 上游。

## 阶段 5：通知

Bark、Telegram；事件见 architecture §7；同一事件去重。

**完成标准**：每种事件都能用测试开关触发并收到一次推送。

## 阶段 6：自动解锁（`ictl-unlock`，默认关闭）

按 capability-matrix §5 的 7 条规则实现。

**完成标准**：锁屏 0/3/30/70 秒、键盘上留半截输入、自然自动锁屏 6 种场景各 5 次全部成功，全程不触发停用；人为配置错误密码时只输错一次即停用并通知。

## 阶段 7：打包发布

- 多阶段 Dockerfile：编译 → `debian:trixie-slim` + `usbmuxd`
- `iphone-control setup` 首次安装向导、`doctor` 逐层诊断
- GitHub Actions：fmt/clippy/test、macOS 上编译未签名 WDA（不含苹果 XCTest 框架）、amd64/arm64 镜像、SBOM
- 文档：setup、troubleshooting、security

**完成标准**：在一台全新的 Linux 机器上，只按 README 操作能装起来；toolkit 停用，本项目接管。

## 必须覆盖的回归用例

这些全部是 toolkit 实际踩过的坑。能单测的写单测，不能单测的进真机回归清单。

| # | 坑 | 正确行为 | 测试方式 |
|---|---|---|---|
| 1 | 锁屏密码键盘不显示时，`Key` 元素仍留在元素树里（`visible=false`） | 不以"树里有键盘"判断键盘已出现；固定先唤醒再上滑 | 单测（脱敏 XML 样本）+ 真机 |
| 2 | `displayed` 接口在锁屏上返回 false，即使键盘可见 | 以密码栏「已输入 N 位」为准 | 真机 |
| 3 | 锁屏界面 Face ID 动画使 WDA 每请求等 idle 至 10 秒，键盘随后自动收起 | 解锁期间 `waitForIdleTimeout=1`、`animationCoolOffTimeout=0.2`，结束恢复；0 会导致唤醒失败 | 真机 |
| 4 | 多位密码合成一个 W3C 手势会漏键 | 每位单独一个手势并逐位核对 | 真机 |
| 5 | 锁屏上 `/wda/tap` 每次约 2.9 秒 | 解锁用 W3C 单手势（约 0.7 秒） | 真机计时 |
| 6 | 同名「设置」有 5 个 `SBAppTags` 含 `hidden` 的后台服务 | 按名字解析 App 时跳过 hidden | 单测 |
| 7 | 容器无 udev，手机重启/重插后 usbmuxd 看不到设备 | sysfs 有 `05ac` 但 usbmuxd 无设备连续 3 次 → 重启 usbmuxd | 真机（杀 usbmuxd 模拟） |
| 8 | 手机重启后 XCTest 弹「Enable UI Automation」密码框 | 识别错误、只推送一次、60 秒重试、恢复后再推送 | 单测（错误识别）+ 真机 |
| 9 | plumesign 不签 `.xctest`，WDA 报 XCTest 103 | `.xctest` 签名、无 entitlements | 单测（签完检查 `_CodeSignature`）+ 真机 |
| 10 | `ios forward` 监听所有网卡，WDA 无认证，局域网可直接控制手机 | 默认不监听任何端口；bridge 只绑 127.0.0.1 | 集成测试（检查监听地址） |
| 11 | 依赖大版本漂移（mcp 2.x 把 FastMCP 改名、idevice 0.1.x 每版破坏性变更） | 所有依赖锁定精确版本 + `Cargo.lock` 入库；升级单独提交 | CI |
| 12 | host 侧 runner 退出后手机上残留 WDA 仍响应几十秒 | 就绪 = 本进程 XCTest 会话存活 且 `/status` ready | 真机 |
| 13 | App 冷启动后立即点击会被吞 | `launch_app` 等到前台再停 1 秒 | 真机 |
| 14 | 完整元素树 7～8 秒 | `excluded_attributes` 排除可见性等属性（约 0.3 秒），可见性按坐标判断 | 单测 + 计时 |
| 15 | 铺满全屏的 Application/Window/Toolbar 等元素没有定位价值 | 列表中过滤 | 单测 |
| 16 | 区域放大倍数只保留 2 位有效数字导致边缘约 1.4 点误差 | 保留 3 位 | 单测 |
| 17 | 只有 ToolError 的消息能透传给 MCP 客户端（Python SDK） | 所有预期错误都转成工具错误文本 | 单测 |
| 18 | 锁屏（未首次解锁）时隧道报 `PasswordProtected` | 进入 NeedsUnlock，提示人工 | 真机 |
| 19 | Apple ID 会话过期只在重签时才发现 | 每日独立验证会话 | 单测（模拟过期） |
| 20 | WDA 在锁屏时启动，`/wda/locked` 一直报 false（只靠变化通知维护，初值 false） | 本次 WDA 启动后确认过一次未锁屏之前，用元素树核对（锁图标 `proud-lock` / `passcode-view`） | 单测（iOS 27 真机样本）+ 真机 |
| 21 | 下拉通知中心时 `/wda/locked` 报 true | WDA 说锁了时核对元素树：只有 `lockscreen-date-view`、没有锁图标的是通知中心 | 单测（真机样本）+ 真机 |
| 22 | iOS 27 上锁屏密码键盘有残留输入时，按 Home 不清空（键隐藏但仍可按） | 用右下角删除键逐位删到 0（按位置认，不依赖语言），每删一位核对 | 单测（真机样本 + 假锁屏）+ 真机 |
| 23 | 熄屏锁屏时第一个无障碍查询等约 8 秒；锁屏 0.5 秒内唤醒，第一下上滑卡约 16 秒后报 `point.x != INFINITY` | 能自动解锁时先按 Home 再核对；本进程锁屏后 3 秒内不解锁；唤醒/上滑报错算这轮落空重来 | 单测 + 真机计时 |
| 24 | HID 会话的屏幕媒体流没正常停止时，手机端一直挂着：灵动岛「配对的电脑」图标常驻、不自动锁屏，进程退出和拔插都清不掉 | HID 空闲 30 秒关闭；所有丢弃路径（含退出）先停流；WDA 锁屏报错时确认已锁就不退到 HID | 真机 |
| 25 | 自动解锁第 2 位之后出错（点击报错、核对锁屏报错、进程被杀）会跳过写停用标记，下次可能把同一个错误密码再输一遍 | 输第 2 位之前先写停用标记，只有确认解开或确认没凑满整串才撤掉；出错返回 `UnlockError::Uncertain` 并推送通知 | 单测（`ictl-unlock`：`error_after_second_digit_keeps_auto_unlock_disabled`、`double_press_that_fills_the_entry_disables`、`dropped_last_digit_is_not_a_wrong_attempt`） |
| 26 | 最后一位输完后用 WDA 的锁屏状态确认结果：WDA 在锁屏时启动会报 false，把输错当成解开 | 确认时用严格判断 `WdaClient::check_lock(_, strict = true)`：核对出错一律算仍锁着 | 单测（`stale_wda_lock_state_cannot_turn_a_wrong_entry_into_unlocked`） |
| 27 | HID 会话在屏幕媒体流起来之后才失败，媒体流没停（症状同 #24） | `HidSession::open`、`media::start` 出错时先停流；HID 会话归 `Tunnel` 所有，关隧道必先停流 | 真机 |
| 28 | 一个长操作（启动 WDA 等自动化密码最长 120 秒）挡住所有工具、`status` 和 SIGTERM，排队的点击几分钟后才执行 | 启动 WDA、重签、会话检查不持设备锁；`status` 读快照；改变手机状态的请求排队超过 30 秒就放弃；停止信号优先处理（architecture §2 并发模型） | 真机 |
| 29 | WDA 请求超时后退到 HID，同一个动作做了两遍 | 传输层区分连不上（`WdaError::Unavailable`，可以退到 HID）和请求已发出后超时（`WdaError::Timeout`，不退） | 单测 + 代码审查 |
| 30 | `.env` 里留空的 `ICTL_LOG=` 生成一个什么都不放行的过滤器，日志全没了 | 空值当作没设 | 真机（`docker logs`） |
| 31 | `/wda/tap` 每次先等界面空闲：主屏点图标约 1.7 秒（调小等待设置也没用）；W3C actions 手势做完又要等动画冷却（默认 2 秒），App 里切标签约 1.9 秒 | 点击用单个 actions 手势，期间临时把 waitForIdleTimeout/animationCoolOffTimeout 调成 1/0.2，做完恢复（原值只读一次）。真机：主屏点图标 1.70→0.91 秒（到前台 1.80→1.01 秒），App 里 0.62→0.72 秒 | 单测（请求顺序、恢复原值）+ 真机计时 |
