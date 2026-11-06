# 更新日志

[English](CHANGELOG.md) | **简体中文**

格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)（0.x 期间次版本号也可能有不兼容的变化）。和 [CHANGELOG.md](CHANGELOG.md) 一一对应，发布说明从这两个文件里取。

## [Unreleased]

### 新增

- 接着多台 iPhone/iPad 时可以选择控制哪一台：新增 MCP 工具 `iphone_list_devices`、`iphone_select_device`，命令 `devices`、`select`。选定的设备写进 state.json，重启后还记得；`select auto` 改回自动选。设了 `IPHONE_UDID` 时以它为准、不能切换。
- 自动解锁密码按设备分开存（`secrets/passcodes/<UDID>`），`passcode set/clear` 可加 `--device`。旧版本的那份密码在 USB 上只接着一台时自动归到那台名下，接着多台时不会用。

### 修复

- 转屏后实时更新屏幕尺寸，避免截图拉伸和屏幕下部元素被过滤；将 iPad 横屏系统界面的竖屏坐标换算为屏幕坐标，按状态栏位置区分两个横屏方向。
- Touch ID 设备自动解锁会先点“使用密码”；补全锁屏识别及 iPad 删除键定位，并在输入前校验数字键布局。
- 经 misagent 读取当前设备已安装的 WDA 描述文件，供 `doctor`、`status` 和自动重签判断到期时间，避免混用另一台设备的签名记录。
- `setup` 在签名安装前等待守护进程识别目标设备；修正设备离线和 iOS 低于 27 时的提示，并为 WDA 启动的笼统错误补充首次安装时信任开发者的排查提示。
- `open_url` 等待前台 App 稳定后返回，减少冷启动时截到黑屏或把短暂系统服务当成目标 App 的情况。
- HID 后备路线的点击、滑动会把横屏坐标换算回设备原生的竖屏坐标；持久化尺寸和复用的 HID 会话都按竖屏归一化。每次后备触摸前重新截图核对方向；横屏还需 30 秒内且匹配的 WDA 旋转角，缺失时（包括守护进程刚重启）拒绝触摸。WDA 不可用期间不要从一个横屏方向翻到另一个：仅凭截图宽高无法识别 180° 翻转。
- 没设 `IPHONE_UDID`、接着多台时，usbmuxd 列表顺序变了会悄悄换成控制另一台（自动解锁可能把一台的密码输进另一台）。现在正在控制的那台一直用到它断开。
- 设了 `IPHONE_UDID` 而那台没接、另一台接着时，usbmuxd 每 30 秒被重启一次。
- 容器里已经接着一台时再插一台，usbmuxd 认不到新的那台（没有 udev，收不到热插拔），也不会被重启。现在比较 USB 上和 usbmuxd 认到的 iPhone/iPad 台数。
- 换设备后还用上一台的 App 列表缓存和屏幕尺寸。

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
