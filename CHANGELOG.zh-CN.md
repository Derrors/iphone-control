# 更新日志

[English](CHANGELOG.md) | **简体中文**

格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)（0.x 期间次版本号也可能有不兼容的变化）。和 [CHANGELOG.md](CHANGELOG.md) 一一对应，发布说明从这两个文件里取。

## [Unreleased]

## [0.1.2] - 2026-11-04

### 新增

- Releases 里提供 macOS 预编译二进制（Apple Silicon，macOS 11 以上）：`iphone-control-vX.Y.Z-macos-arm64.tar.gz`。没有经过苹果公证，被 Gatekeeper 拦下时见 macOS 指南。

## [0.1.1] - 2026-11-04

### 新增

- 支持 macOS（Apple Silicon）：从源码编译，以普通用户运行，用系统自带的 usbmuxd，不需要 Xcode。见新增的 macOS 使用指南。

### 变更

- MCP 工具描述说明了何时用、返回什么、具体行为，每个参数都有说明，每个工具都带 MCP 注解。工具名、参数和返回结果不变。
- Release 里的二进制压缩包改名为 `iphone-control-vX.Y.Z-linux-<架构>.tar.gz`。

### 修复

- 部分手机上 `home`、`lock` 没有效果（在 iPhone 12 mini、iOS 26.4.1 上遇到），自动解锁也点不亮熄灭的屏幕。
- 用系统 usbmuxd 的部署（macOS、Linux 从源码运行）断开几秒后会卡在「bad device」，现在能自己重连。
- macOS 上 `doctor` 不再误报「USB 上没有 Apple 设备」，修复提示改用 macOS 的命令。
- 自动解锁调不出密码键盘时，提示里说明待机显示会挡住键盘。

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

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/Derrors/iphone-control/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Derrors/iphone-control/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Derrors/iphone-control/releases/tag/v0.1.0
