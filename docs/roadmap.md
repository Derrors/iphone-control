# 路线图

每个阶段都有明确的完成标准；未达标不进入下一阶段。toolkit 在阶段 7 之前一直是正式版本。

## 阶段 0：真机验证（spike）

见 [spike-plan.md](spike-plan.md)。

**完成标准**：`spike/RESULTS.md` 给出 5 项结论，并按判定表做出继续/折中/暂停的决定。

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

- vendor `plume_core`、`plume_utils`、`omnisette`（保持原许可证文件头），打上 `.xctest` 修复和 apple-codesign 的权限补丁
- `iphone-control login`（交互式密码 + 双重认证）、`sign`、`resign`
- 守护进程内部每日检查：描述文件剩余 < 2 天重签；每日验证 Apple ID 会话
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
