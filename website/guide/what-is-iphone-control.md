# 什么是 iphone-control？

iphone-control 让 AI agent（例如 Claude）操作一台用数据线接在 Linux 主机或 Mac 上的真实 iPhone 或 iPad：看屏幕、点按钮、输入文字、打开 App，就像人拿着手机一样。

它以 [MCP](https://modelcontextprotocol.io)（Model Context Protocol）工具的形式提供能力，支持 MCP 的客户端（Claude Code、Claude Desktop、Cursor 等）都能直接用。

核心是设备控制：一个常驻守护进程独占手机，不开浏览器的控制台也一直提供服务。控制台和远程接入都是可选的，见[控制台与远程使用](./remote)。

## 能用来做什么

- 让 agent 在手机上完成任务：查信息、填表、改设置、玩游戏。
- 在真机上测试自己的 App 或网页；用 Linux 主机就行，不需要 Mac。
- 把一台闲置 iPhone 变成可以远程脚本化的设备。
- 在浏览器的控制台里看画面、直接操作，或把任务交给本机的 Claude Code、Codex，每一步都能暂停、接管、确认；局域网和别的机器也能用，见[控制台与远程使用](./remote)。

## 怎么工作

![架构：AI 客户端经 stdio 调用 iphone-control mcp，可选的控制台经 HTTPS 服务浏览器和远程 agent，两者都经 Unix socket 交给守护进程。守护进程是核心：经 USB 操作 iPhone 或 iPad，iOS 27 起用原生控制，iOS 17–26 用 WebDriverAgent，控制台关着也照常运行](/architecture-zh-light.svg){.light-only}
![架构：AI 客户端经 stdio 调用 iphone-control mcp，可选的控制台经 HTTPS 服务浏览器和远程 agent，两者都经 Unix socket 交给守护进程。守护进程是核心：经 USB 操作 iPhone 或 iPad，iOS 27 起用原生控制，iOS 17–26 用 WebDriverAgent，控制台关着也照常运行](/architecture-zh-dark.svg){.dark-only}

- **守护进程**常驻，独占手机：配对、挂载开发者镜像、建立隧道，再按 iOS 版本自动选择控制路线；USB 热插拔通过系统 `usbmuxd` 立即感知。
- **MCP 服务**由 AI 客户端按需拉起，只是无状态薄客户端，将工具调用转给守护进程。

### 两条控制路线对比

| 维度 | iOS 27 及以上：原生控制（默认推荐） | iOS 17–26：WebDriverAgent |
|---|---|---|
| 手机端安装 | **免安装**，直接调用系统无障碍、HID 与 CoreDevice | 需在手机上签名安装 [WebDriverAgent](https://github.com/appium/WebDriverAgent) |
| Apple ID 与签名 | **不需要** | 需免费 Apple ID，7 天到期前自动重签 |
| 重启后操作 | 仅需人工解锁一次（BFU） | 除 BFU 解锁外，首次启用 UI 自动化需再输一次密码 |
| 就绪速度 | 几秒至半分钟 | 首次冷启动 10～60 秒；后台常驻切回几秒接管 |
| 控制台画面 | 手机硬件 HEVC（~60 fps，本机零编码） | WDA MJPEG → 本机 H.264（macOS 免 ffmpeg） |
| 切换方式 | 设 `ICTL_IOS_BACKEND=wda` 可切回 WDA（期间 WDA 不可用时有 HID 后备） | 固定使用 WDA，持续维护 |

## 兼容性

| | |
|---|---|
| 主机 | Linux，x86_64 或 arm64：一行命令安装、Homebrew 或 deb 包（Debian 12+、Ubuntu 22.04+），要系统的 usbmuxd；也可以用 Rust 1.89+ 从源码编译<br>macOS 11 及以上，Apple Silicon 或 Intel（Intel 的包从 0.2.14 起提供）：一行命令安装或 Homebrew，也可以从源码编译，见 [macOS](../deploy/macos)<br>不支持 Windows |
| 手机 | iPhone 或 iPad，iOS 17 及以上。**推荐 iOS 27 及以上**（原生控制）；iOS 17–26 经 WebDriverAgent |
| 已在真机验证 | iPhone 15 Pro Max，iOS 27.2.1，原生控制（Linux）<br>iPhone 15 Pro Max，iOS 27.0.1，WDA（Linux、macOS）<br>iPhone 12 mini，iOS 26.4.1（Linux）<br>iPad mini 5，iOS 26.6.1 / 26.7.3（Linux） |
| 账号 | iOS 27 及以上（原生控制）不需要；iOS 26 及以下要一个免费 Apple ID，建议专门注册一个 |

其他机型和 iOS 版本应该也能用，但还没测过，欢迎[反馈](https://github.com/Derrors/iphone-control/issues)。

::: tip 输出语言
命令行、`doctor` 的输出和 MCP 工具返回的文字默认是简体中文，设 `IPHONE_CONTROL_LANG=en` 切换英文，见[配置](./configuration)。
:::

## 限制

**需要你在手机上操作**

- 不绕过任何 iOS 安全机制：信任电脑、打开开发者模式，都需要你在手机上亲自操作一次；用 WDA 时还要信任开发者证书、启用 UI 自动化。
- 手机重启后，第一次解锁（BFU）必须人工完成，这是 iOS 的要求；用 WDA 时还要再输一次密码允许 UI 自动化。

**使用**

- 一台主机可以接多台 iPhone/iPad，但同一时间只控制一台，见[多台设备](./multi-device)。不要让两份 iphone-control 或其他基于 usbmuxd 的工具连同一台。
- 游戏、地图、视频这类界面没有元素树，agent 只能看截图按坐标操作。
- 原生控制（iOS 27 及以上）：屏幕外的列表行读不到，要先滑到才找得到；输入文字一律经剪贴板粘贴，守护进程会自动点「允许粘贴」，输完还原剪贴板。
- iOS 26 及以下全部经 WebDriverAgent 操作，没有 HID 后备，WDA 重启期间操作会失败。
- 用 WDA 时：免费 Apple ID 的描述文件 7 天有效。重签是自动的，但 Apple ID 会话失效后要运行 `login` 重新登录。
- 用 WDA 时，登录 Apple ID 默认用公共的第三方 anisette 服务，可以设 `ICTL_ANISETTE_URL` 换成自建服务，见[配置](./configuration)。

**macOS**

- Intel Mac 的包从 0.2.14 起提供，是在 Apple Silicon 上交叉编译的，只在 Rosetta 下跑过测试，还没在真正的 Intel Mac 上用过。
- 预编译二进制没有经过苹果公证，用浏览器下载的要先去掉隔离标记。
- 手机的充电上限要改成 100%，开着「80% 上限」时 USB 大约每分钟断开一次。

和 Linux 的其他区别见 [macOS 已知差异](../deploy/macos#已知差异)。

下一步：[快速开始](./getting-started)。
