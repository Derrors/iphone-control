# 快速开始

装好程序、接上手机、运行 `iphone-control`，跟着引导走完，大约 15 分钟。Linux 和 Mac 的步骤相同，Mac 的细节见 [macOS](../deploy/macos)。

## 准备

- 一台主机：Mac（Apple 芯片），或 Linux（x86_64、aarch64）
- iPhone 或 iPad（iOS 17+）和能传数据的线
- 一个免费 Apple ID（建议专门注册一个）
- 主机能访问苹果服务器和 GitHub；国内网络一般要配代理（引导里会问）

::: warning 一部手机只接一份
不要同时运行两份 iphone-control，或其他基于 usbmuxd 的工具（go-ios、pymobiledevice3 等）连同一部手机。
:::

## 1. 安装 {#install}

三种方式任选一种，都以普通用户安装，不要用 root 或 sudo 运行 `iphone-control`。

**一行命令**（macOS 或 Linux，需在 PATH 中提供 Python 3 和 OpenSSL 3）

```bash
curl -fsSL https://github.com/Derrors/iphone-control/releases/latest/download/install.sh | sh
```

程序装到 `~/.local/bin`，守护进程装成用户服务（macOS 是 launchd，Linux 是 systemd 用户服务），登录后自动启动。运行 `iphone-control update check` 查看升级指引。可以在 `sh` 后面加 `-s --` 再跟选项：`--version vX.Y.Z` 装指定版本，`--prefix DIR` 装到 `DIR/bin`，`--no-service` 只装程序、不装服务。

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

Intel Mac 没有预编译版本，要从源码编译；参与开发也从源码运行。见[从源码编译](../deploy/source)。

## 2. 接上手机，运行 iphone-control {#first-run}

用数据线接上手机并解锁，然后在终端里运行：

```bash
iphone-control
```

第一次运行时，它会在终端里一步步带你完成设置。已经做好的步骤会跳过；中途退出了，再运行 `iphone-control` 会接着做。它要在终端里交互运行。

| 步骤 | 你要做的 |
|---|---|
| 语言 | 选中文或 English |
| 代理 | 需要的话填代理地址，例如 `http://127.0.0.1:7890` |
| 后台服务 | 还没装服务时，回车装成后台服务并启动 |
| 等待手机连接 | 解锁手机 |
| 配对 | 手机上弹出「要信任此电脑吗？」时点「信任」，输入锁屏密码 |
| 开发者模式 | 手机上：设置 → 隐私与安全性 → 开发者模式 → 打开 → 重新启动；重启解锁后在弹窗里点「打开」 |
| 登录 Apple ID | 在终端里输入 Apple ID、密码和双重认证验证码 |
| 签名安装 WDA | 保持手机解锁 |
| 等待 WDA 启动 | 手机上：设置 → 通用 → VPN 与设备管理 → 信任你的 Apple ID（要联网）；如果弹出「为 XCTest 输入 iPhone 密码」，输入锁屏密码 |
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
iphone-control viewer      # 打开 http://127.0.0.1:8200
```

## 检查状态

```bash
iphone-control status   # 当前状态、描述文件还剩多久、最近日志
iphone-control doctor   # 逐层诊断，每项失败都给出修复办法
```

遇到问题看[常见问题](../troubleshoot/faq)。

## 卸载

| 安装方式 | 卸载 |
|---|---|
| 一行命令 | `curl -fsSL https://github.com/Derrors/iphone-control/releases/latest/download/install.sh \| sh -s -- --uninstall` |
| Homebrew | `brew services stop iphone-control`，再 `brew uninstall iphone-control` |
| deb 包 | `systemctl --user disable --now iphone-control`，再 `sudo apt remove iphone-control` |

数据目录 `~/.local/share/iphone-control`（Apple ID 会话、签名、自动解锁密码）会留着，不需要的话自己删除；手机上的 WebDriverAgent 在手机上删除。
