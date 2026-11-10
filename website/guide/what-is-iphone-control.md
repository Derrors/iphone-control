# 什么是 iphone-control？

iphone-control 让 AI agent（例如 Claude）操作一台用数据线接在 Linux 主机或 Mac 上的真实 iPhone 或 iPad：看屏幕、点按钮、输入文字、打开 App，就像人拿着手机一样。

它以 [MCP](https://modelcontextprotocol.io)（Model Context Protocol）工具的形式提供能力，支持 MCP 的客户端（Claude Code、Claude Desktop、Cursor 等）都能直接用。

## 能用来做什么

- 让 agent 在手机上完成任务：查信息、填表、改设置、玩游戏。
- 在真机上测试自己的 App 或网页；用 Linux 主机就行，不需要 Mac。
- 把一台闲置 iPhone 变成可以远程脚本化的设备。

## 怎么工作

![架构：MCP 客户端经 stdio 调用 iphone-control mcp，再经 Unix socket 交给守护进程；守护进程经 USB 用 WebDriverAgent 操作 iPhone 或 iPad](/architecture-light.svg){.light-only}
![架构：MCP 客户端经 stdio 调用 iphone-control mcp，再经 Unix socket 交给守护进程；守护进程经 USB 用 WebDriverAgent 操作 iPhone 或 iPad](/architecture-dark.svg){.dark-only}

- **守护进程**常驻，独占手机：配对、挂载开发者镜像、建立隧道，在手机上运行 [WebDriverAgent](https://github.com/appium/WebDriverAgent)（WDA，苹果 XCTest 自动化框架上的代理），用它读界面、点击和输入。
- **MCP 服务**由 AI 客户端按需拉起，只是把工具调用转给守护进程。
- WDA 用你的**免费 Apple ID** 签名。免费签名 7 天过期，守护进程会在到期前自动重签。
- WDA 不可用时（例如正在重启），点击、滑动、按键和截图改走 CoreDevice HID，照样能用（要 iOS 27 及以上）。

## 兼容性

| | |
|---|---|
| 主机 | Linux，x86_64 或 arm64：一行命令安装、Homebrew 或 deb 包（Debian 12+、Ubuntu 22.04+），要系统的 usbmuxd；也可以用 Rust 1.88+ 从源码编译<br>macOS，Apple Silicon：一行命令安装或 Homebrew，也可以从源码编译，见 [macOS](../deploy/macos)<br>不支持 Windows |
| 手机 | iPhone 或 iPad，iOS 17 及以上；HID 后备路线要 iOS 27 及以上 |
| 已在真机验证 | iPhone 15 Pro Max，iOS 27.0.1（Linux、macOS）<br>iPhone 12 mini，iOS 26.4.1（Linux）<br>iPad mini 5，iOS 26.6.1（Linux） |
| 账号 | 免费 Apple ID 即可，建议专门注册一个 |

其他机型和 iOS 版本应该也能用，但还没测过，欢迎[反馈](https://github.com/Derrors/iphone-control/issues)。

::: tip 输出语言
命令行、`doctor` 的输出和 MCP 工具返回的文字默认是简体中文，设 `IPHONE_CONTROL_LANG=en` 切换英文，见[配置](./configuration)。
:::

## 限制

**需要你在手机上操作**

- 不绕过任何 iOS 安全机制：信任电脑、打开开发者模式、信任开发者证书、启用 UI 自动化，都需要你在手机上亲自操作一次。
- 手机重启后，第一次解锁（BFU）必须人工完成，还要再输一次密码允许 UI 自动化，这是 iOS 的要求。

**使用**

- 一台主机可以接多台 iPhone/iPad，但同一时间只控制一台，见[多台设备](./multi-device)。不要让两份 iphone-control 或其他基于 usbmuxd 的工具连同一台。
- 游戏、地图、视频这类界面没有元素树，agent 只能看截图按坐标操作。
- HID 后备路线要 iOS 27 及以上；更早的 iOS 全部经 WebDriverAgent 操作，WDA 重启期间操作会失败。
- 免费 Apple ID 的描述文件 7 天有效。重签是自动的，但 Apple ID 会话失效后要运行 `login` 重新登录。
- 登录 Apple ID 默认用公共的第三方 anisette 服务，可以设 `ICTL_ANISETTE_URL` 换成自建服务，见[配置](./configuration)。

**macOS**

- 预编译版本只支持 Apple Silicon；Intel Mac 要从源码编译，没测过。
- 预编译二进制没有经过苹果公证，用浏览器下载的要先去掉隔离标记。
- 手机的充电上限要改成 100%，开着「80% 上限」时 USB 大约每分钟断开一次。

和 Linux 的其他区别见 [macOS 已知差异](../deploy/macos#已知差异)。

下一步：[快速开始](./getting-started)。
