# 快速开始

装好程序、接上手机、运行 `iphone-control`，跟着引导走完，大约 15 分钟。Linux 和 Mac 的步骤相同，Mac 的细节见 [macOS](../deploy/macos)。

## 准备

- 一台主机：Mac（Apple Silicon 或 Intel），或 Linux（x86_64、aarch64）
- iPhone 或 iPad 和能传数据的线。**推荐 iOS 27 及以上**：原生控制，手机上什么都不装，不用 Apple ID；iOS 17–26 也能用，经 WebDriverAgent
- iOS 26 及以下还要一个免费 Apple ID（建议专门注册一个），用来给 WebDriverAgent 签名
- 主机能访问苹果服务器和 GitHub；国内网络一般要配代理（引导里会问）

::: warning 一部手机只接一份
不要同时运行两份 iphone-control，或其他基于 usbmuxd 的工具（go-ios、pymobiledevice3 等）连同一部手机。
:::

## 1. 安装 {#install}

三种方式任选一种，均以普通用户安装（不要用 root 或 `sudo` 运行 `iphone-control`）。不知道选哪种时可先看下表：

| 安装方式 | 适用系统 | 后台服务 | `usbmuxd` | 升级方式 |
|---|---|---|---|---|
| **一行命令（推荐）** | macOS / Linux（需 Python 3 + OpenSSL 3） | 自动安装 launchd / systemd 用户服务 | Linux 需另装 | `iphone-control update`（支持签名自动补丁 `auto-patch`） |
| **Homebrew** | macOS / Linux | `brew services` | Linux 需另装 | `iphone-control update`（走 Homebrew tap） |
| **`.deb` 软件包** | Debian 12+ / Ubuntu 22.04+ | 自带 systemd 用户 unit | **自动安装** | `iphone-control update`（验证签名后走 `apt`） |
| **从源码编译** | 需 Rust 1.89+（[源码指南](../deploy/source)） | `service install` | Linux 需另装 | `git pull && cargo install` |

**一行命令**（macOS 或 Linux，需在 PATH 中提供 Python 3 和 OpenSSL 3）

```bash
curl -fsSLo iphone-control-install.sh https://github.com/Derrors/iphone-control/releases/latest/download/install.sh && sh iphone-control-install.sh
```

程序装到 `~/.local/bin`，守护进程装成用户服务（macOS 用 launchd，Linux 用 systemd），登录后自动启动。运行 `iphone-control update` 检查并升级；只想查看版本可用 `iphone-control update check`。给脚本传入 `--version vX.Y.Z` 可装指定的签名版本，`--prefix DIR` 可装到 `DIR/bin`，`--no-service` 可只装程序。

**Homebrew**（macOS、Linux）：

```bash
brew install derrors/tap/iphone-control
brew services start iphone-control
```

**Debian / Ubuntu 的 deb 包**：从 [GitHub Release](https://github.com/Derrors/iphone-control/releases) 下载 `iphone-control_<版本>_amd64.deb`（ARM 机器用 `_arm64.deb`），然后：

```bash
sudo apt install ./iphone-control_<版本>_amd64.deb
systemctl --user enable --now iphone-control     # 以普通用户运行，每个用户各自开启
```

不开启也行，下面的引导会替你启动。

::: tip Linux 要装系统的 usbmuxd
和手机通过 USB 通信要用系统的 usbmuxd：`sudo apt install usbmuxd`（其他发行版用对应的包管理器）。deb 包会自动装上它。
:::

::: tip 可选：ffmpeg
Linux 上装了 ffmpeg，控制台的画面改用 H.264，流量约为原来的 1/15，经隧道、走外网时更顺：`sudo apt install ffmpeg`。macOS 上不用装，直接就是 H.264。见[控制台的画面](./cli#viewer-picture)。
:::

一行命令和 Homebrew 在 Apple Silicon 和 Intel 的 Mac 上都能用（Intel 的包从 0.2.14 起提供）。参与开发从源码运行，见[从源码编译](../deploy/source)。

<a id="legacy-script"></a>

::: details 上游 v0.2.0 旧版脚本安装与迁移
v0.2.0 使用安装包校验和，发行附件中没有签名安装器。要安装该版本，请下载 v0.2.0 标签里的脚本并指定同一版本：

```sh
curl -fsSLo iphone-control-v0.2.0-install.sh https://raw.githubusercontent.com/Derrors/iphone-control/v0.2.0/install.sh && sh iphone-control-v0.2.0-install.sh --version v0.2.0
```

要迁移已有的 v0.2.0 脚本安装，先退出 Viewer 和 MCP 客户端、停止守护进程，再用 v0.2.0 脚本卸载旧程序和服务（设置和设备数据会保留）：

```sh
curl -fsSLo iphone-control-v0.2.0-install.sh https://raw.githubusercontent.com/Derrors/iphone-control/v0.2.0/install.sh && sh iphone-control-v0.2.0-install.sh --uninstall
```

然后运行上面的[签名安装器](#install)。如旧版本装在自定义前缀，卸载与重装时均传入相同的 `--prefix DIR`。
:::

## 2. 接上手机，运行 iphone-control {#first-run}

![首次设置流程：安装程序、接手机配对与开发者模式，按 iOS 版本分流至 iOS 27+ 原生控制或 iOS 17–26 WebDriverAgent，最后注册 MCP 到 AI 客户端](/diagrams/setup-flow-zh-light.svg){.light-only}
![首次设置流程：安装程序、接手机配对与开发者模式，按 iOS 版本分流至 iOS 27+ 原生控制或 iOS 17–26 WebDriverAgent，最后注册 MCP 到 AI 客户端](/diagrams/setup-flow-zh-dark.svg){.dark-only}

用数据线接上手机并解锁，然后在终端里运行：

```bash
iphone-control
```

第一次运行时，它会在终端里一步步带你完成设置。已经做好的步骤会跳过；中途退出了，再运行 `iphone-control` 会接着做。它要在终端里交互运行。

iOS 27 及以上的手机走原生控制，引导会跳过登录 Apple ID 和签名安装 WDA，下表标了「iOS 26 及以下」的步骤都不用做。

| 步骤 | 你要做的 |
|---|---|
| 语言 | 选中文或 English |
| 代理 | 需要的话填代理地址，例如 `http://127.0.0.1:7890` |
| 后台服务 | 还没装服务时，回车装成后台服务并启动 |
| 等待手机连接 | 解锁手机 |
| 配对 | 手机上弹出「要信任此电脑吗？」时点「信任」，输入锁屏密码 |
| 开发者模式 | 手机上：设置 → 隐私与安全性 → 开发者模式 → 打开 → 重新启动；重启解锁后在弹窗里点「打开」 |
| 登录 Apple ID（iOS 26 及以下） | 在终端里输入 Apple ID、密码和双重认证验证码 |
| 签名安装 WDA（iOS 26 及以下） | 保持手机解锁 |
| 等待手机准备好（iOS 27 及以上） | 保持手机解锁，通常几秒到半分钟 |
| 等待 WDA 启动（iOS 26 及以下） | 手机上：设置 → 通用 → VPN 与设备管理 → 信任你的 Apple ID（要联网）；如果弹出「为 XCTest 输入 iPhone 密码」，输入锁屏密码 |
| 自动解锁（可选） | 要的话输入锁屏密码，见[自动解锁](./auto-unlock) |
| 通知（可选） | 选 Bark 或 Telegram，填 key 或 token，会发一条测试通知，见[通知](./notifications) |
| 注册 MCP | 找到 Claude Code 或 Codex 时，回车注册到它 |

看到「完成」就好了。设置保存在 `~/.config/iphone-control/env`，见[配置](./configuration#settings-file)。

::: tip 密码只在这里输入
Apple ID 密码和验证码只在这个终端里输入，不会保存到磁盘；保存的只是登录后的会话。
:::

以后再运行 `iphone-control` 只显示状态；守护进程没在运行时会先启动它。要改设置，运行 `iphone-control setup --reconfigure`，选要重新设置的那一项，见[命令行](./cli#setup)。

## 3. 试一下

开一个新的 Claude Code 或 Codex 会话，说：

> 用 iPhone 打开设置，看看 iOS 版本是多少。

agent 会调用 `iphone_launch_app`、`iphone_tap_element`、`iphone_screenshot` 等工具完成。其他客户端见[接入 AI 客户端](./mcp-clients)。

也可以在浏览器里看画面、直接操作手机，或把任务交给本机的 Claude Code、Codex 执行：

```bash
iphone-control viewer          # 控制台，开在 http://127.0.0.1:8200
iphone-control viewer token    # 另开一个终端：点开它打印的链接就登录了
```

控制台要先登录，本机也一样；登录一次记住 30 天。局域网访问、只读令牌、从别的机器用，见[控制台与远程使用](./remote)。

## 检查状态

```bash
iphone-control status   # 当前状态、控制方式（原生或 WDA）、最近日志
iphone-control doctor   # 逐层诊断，每项失败都给出修复办法
```

遇到问题看[常见问题](../troubleshoot/faq)。

## 卸载

| 安装方式 | 卸载 |
|---|---|
| 签名安装器 | 用上面下载的脚本运行 `sh iphone-control-install.sh --uninstall`，若安装时指定了前缀，传入相同的前缀 |
| v0.2.0 脚本 | 运行 [v0.2.0 卸载命令](#legacy-script) |
| Homebrew | `brew services stop iphone-control`，再 `brew uninstall iphone-control` |
| deb 包 | `systemctl --user disable --now iphone-control`，再 `sudo apt remove iphone-control` |

数据目录 `~/.local/share/iphone-control`（Apple ID 会话、签名、自动解锁密码）会留着，不需要的话自己删除；用过 WDA 的话，手机上的 WebDriverAgent 在手机上删除。
