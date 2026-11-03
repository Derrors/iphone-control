# 什么是 iphone-control？

iphone-control 让 AI agent（例如 Claude）操作一台用数据线接在 Linux 主机或 Mac 上的真实 iPhone：看屏幕、点按钮、输入文字、打开 App，就像人拿着手机一样。

它以 [MCP](https://modelcontextprotocol.io)（Model Context Protocol）工具的形式提供能力，支持 MCP 的客户端（Claude Code、Claude Desktop、Cursor 等）都能直接用。

## 能用来做什么

- 让 agent 在手机上完成任务：查信息、填表、改设置、玩游戏。
- 在真机上测试自己的 App 或网页；用 Linux 主机就行，不需要 Mac。
- 把一台闲置 iPhone 变成可以远程脚本化的设备。

## 怎么工作

![架构：MCP 客户端经 stdio 调用 iphone-control mcp，再经 Unix socket 交给守护进程；守护进程经 USB 用 WebDriverAgent 操作 iPhone](/architecture-light.svg){.light-only}
![架构：MCP 客户端经 stdio 调用 iphone-control mcp，再经 Unix socket 交给守护进程；守护进程经 USB 用 WebDriverAgent 操作 iPhone](/architecture-dark.svg){.dark-only}

- **守护进程**常驻，独占手机：配对、挂载开发者镜像、建立隧道，在手机上运行 [WebDriverAgent](https://github.com/appium/WebDriverAgent)（WDA，苹果 XCTest 自动化框架上的代理），用它读界面、点击和输入。
- **MCP 服务**由 AI 客户端按需拉起，只是把工具调用转给守护进程。
- WDA 用你的**免费 Apple ID** 签名。免费签名 7 天过期，守护进程会在到期前自动重签。
- WDA 不可用时（例如正在重启），点击、滑动、按键和截图改走 CoreDevice HID，照样能用（要 iOS 27 及以上）。

## 兼容性

| | |
|---|---|
| 主机 | Linux，x86_64 或 arm64：Docker（Compose v2），或 Rust 1.88+ 从源码编译<br>macOS，Apple Silicon：Rust 1.88+ 从源码编译，见 [macOS](../deploy/macos) |
| 手机 | iOS 17 及以上；HID 后备路线要 iOS 27 及以上 |
| 已在真机验证 | iPhone 15 Pro Max，iOS 27.0.1（Linux、macOS）<br>iPhone 12 mini，iOS 26.4.1（Linux） |
| 账号 | 免费 Apple ID 即可，建议专门注册一个 |

其他机型和 iOS 版本应该也能用，但还没测过，欢迎[反馈](https://github.com/Derrors/iphone-control/issues)。

::: tip 输出语言
命令行、`doctor` 的输出和 MCP 工具返回的文字目前都是简体中文。
:::

## 不能做什么

- 不绕过任何 iOS 安全机制：信任电脑、打开开发者模式、信任开发者证书、启用 UI 自动化，都需要你在手机上亲自操作一次。
- 手机重启后第一次解锁（BFU）必须人工完成。
- 游戏、地图、视频这类界面没有元素树，agent 只能看截图按坐标操作。

下一步：[快速开始](./getting-started)。
