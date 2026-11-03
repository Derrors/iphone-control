# 更新日志

[English](CHANGELOG.md) | **简体中文**

格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)（0.x 期间次版本号也可能有不兼容的变化）。和 [CHANGELOG.md](CHANGELOG.md) 一一对应，发布说明从这两个文件里取。

## [Unreleased]

### 新增

- 支持 macOS（Apple Silicon）：从源码编译，以普通用户运行，用系统自带的 usbmuxd，不需要 Xcode。已在 macOS 26.6.2 + iPhone 15 Pro Max（iOS 27.0.1）上验证，开着 Finder 也正常。CI 增加 macOS 编译和测试。新增 macOS 使用指南。

### 变更

- MCP 工具描述补充了何时用这个工具而不是相近的工具、返回什么、具体行为（冷启动还是切回前台、强制结束、滑动惯性），每个参数都有说明，每个工具都带 MCP 注解（标题、只读、破坏性、幂等）。工具名、参数和返回结果不变。

### 修复

- 手机断开几秒又接上（拔插、USB-C 供电重新协商）后，守护进程不再卡在「bad device」：发现 usbmuxd 换了设备编号就重新连接。
- macOS 上 `doctor` 不再在手机已连接时报「USB 上没有 Apple 设备」，修复提示改用 `launchctl` 而不是 `systemctl`。
- iPhone 12 mini（iOS 26.4.1）上 `home`、`lock` 报成功但没有效果，自动解锁也点不亮熄灭的屏幕。Home 改为发 IOHID 的 AC Home 事件，没回到主屏再用 XCTest 的按键补一次；锁屏改为经 IOHID 按电源键，没锁上再用 WebDriverAgent 的锁屏。自动解锁轮流用这两种方式唤醒屏幕。
- 自动解锁调不出密码键盘时，提示里说明待机显示（手机横放充电时出现）会挡住键盘。

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

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Derrors/iphone-control/releases/tag/v0.1.0
