# 更新日志

[English](CHANGELOG.md) | **简体中文**

格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)（0.x 期间次版本号也可能有不兼容的变化）。和 [CHANGELOG.md](CHANGELOG.md) 一一对应，发布说明从这两个文件里取。

## [Unreleased]

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
