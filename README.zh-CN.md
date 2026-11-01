# iphone-control

[English](README.md) | **简体中文**

[![CI](https://github.com/Derrors/iphone-control/actions/workflows/ci.yml/badge.svg)](https://github.com/Derrors/iphone-control/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

在 Linux 上控制一台通过 USB 连接的真实 iPhone（不需要 Mac，不需要付费开发者账号），并把能力以 [MCP](https://modelcontextprotocol.io) 工具的形式交给 AI agent。用 Rust 写成，产物是一个二进制，外加系统的 `usbmuxd`。

> **状态：早期版本（0.x）。** 下面的功能都在一台真机上完整跑通：iPhone 15 Pro Max，iOS 27.0.1。其他机型和 iOS 17 以上应该也能用，但没测过，欢迎反馈。

## 能做什么

- **给 agent 的工具**：截图（可放大某块区域）、列出屏幕上的元素、按坐标或按文字点击、滑动、长按、输入文字（支持中文）、硬件按键、启动/结束 App、打开链接、等某段文字出现、一次执行多步。任何操作都可以顺带返回操作后的界面，省一次往返。见 [docs/tools.md](docs/tools.md)。
- **自己维持运行**：配对、挂载开发者镜像、用免费 Apple ID 给 WebDriverAgent 签名，并在 7 天描述文件过期前自动重签；WDA 挂了自动重启；拔插、手机重启后自动恢复。
- **后备路线**：WDA 不可用时，点击、滑动、按键、截图仍可经 CoreDevice HID 完成。
- **需要你时会通知**：重签失败、Apple ID 会话失效、手机重启后要输密码、手机离线时，经 Bark 或 Telegram 推送。
- **可选的自动解锁**：默认关闭；开启前先读 [SECURITY.md](SECURITY.md)。
- **不开端口**：WDA 经 usbmuxd 直连，本机不监听任何网络端口。

```
AI agent ──stdio──▶ iphone-control mcp ──unix socket──▶ iphone-control daemon ──usbmuxd/USB──▶ iPhone（WebDriverAgent）
```

一个常驻守护进程独占设备（隧道、HID 会话、WDA 会话常驻复用）；每个 agent 会话是一个薄的 MCP 客户端。详见 [docs/architecture.md](docs/architecture.md)。

## 需要什么

- 一台有 USB 口的 Linux 主机（x86_64 或 arm64）。
- Docker（Compose v2）；不用 Docker 时需要 Rust 1.88 以上、C 编译器和 `usbmuxd`（见[从源码运行](#从源码运行不用-docker)）。
- iOS 17 以上的 iPhone 和数据线；开发者模式由 `setup` 引导打开。
- 一个免费 Apple ID，建议专门注册一个（见[安全与隐私](#安全与隐私)）。
- 能访问苹果服务器、anisette 服务和 GitHub（首次使用时下载开发者镜像和 WebDriverAgent）。国内网络一般要在 `.env` 里配代理。

## 快速开始（Docker）

```bash
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cp .env.example .env              # 可选：代理、通知、自动解锁
docker compose up -d --build      # 首次构建要编译 Rust，需要几分钟
docker exec -it iphone-control iphone-control setup
docker exec -it iphone-control iphone-control mcp-config   # 打印注册 MCP 的命令
```

发布版本后可以不自己构建：在 `.env` 里设 `IMAGE=ghcr.io/derrors/iphone-control:latest`，然后 `docker compose pull && docker compose up -d --no-build`。

`setup` 可以重复运行，依次是：

1. **连接**：用数据线接好手机并解锁。
2. **配对**：手机上点「信任」，输入锁屏密码。
3. **开发者模式**：设置 → 隐私与安全性 → 开发者模式 → 打开 → 重新启动；重启解锁后在弹窗里确认。
4. **登录 Apple ID**：密码和双重认证验证码只在这个终端里输入。
5. **签名并安装 WebDriverAgent**：期间手机保持解锁。
6. **启动 WebDriverAgent**：首次要在 设置 → 通用 → VPN 与设备管理 里信任你的 Apple ID；如果弹出「为 XCTest 输入 iPhone 密码」（Enable UI Automation），输入锁屏密码。

容器不用 `privileged`：只开放 USB 字符设备（`device_cgroup_rules: "c 189:* rmw"`），`/dev/bus/usb` 绑定挂载。不要同时运行两份（或其他基于 usbmuxd 的工具）连同一部手机。

## 接入 MCP 客户端

Claude Code（容器部署时 `mcp-config` 打印的就是这条）：

```bash
claude mcp add iphone -s user -- docker exec -i iphone-control iphone-control mcp
```

其他客户端（Claude Desktop、Cursor 等）在 JSON 配置里写同一条命令：

```json
{
  "mcpServers": {
    "iphone": { "command": "docker", "args": ["exec", "-i", "iphone-control", "iphone-control", "mcp"] }
  }
}
```

从源码运行时改为 `iphone-control --socket <路径> mcp`，具体命令用 `iphone-control mcp-config` 打印。

## 从源码运行（不用 Docker）

以 Debian/Ubuntu 为例：

```bash
sudo apt install build-essential git usbmuxd     # usbmuxd 由 udev 在插上手机时拉起
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked  # 装到 ~/.cargo/bin/iphone-control
iphone-control daemon      # 前台运行；常驻用下面的 systemd 用户服务
iphone-control setup       # 另开一个终端
iphone-control mcp-config
```

常驻运行用 [deploy/iphone-control.service](deploy/iphone-control.service)（systemd 用户服务）：

```bash
mkdir -p ~/.config/systemd/user ~/.config/iphone-control
cp deploy/iphone-control.service ~/.config/systemd/user/
cp .env.example ~/.config/iphone-control/env      # CONTAINER_NAME、IMAGE、DATA_DIR 只给 compose 用
systemctl --user daemon-reload && systemctl --user enable --now iphone-control
loginctl enable-linger "$USER"                    # 不登录也随开机启动
```

| | 容器 | 从源码（普通用户） |
|---|---|---|
| 数据目录 | `./data`（容器内 `/var/lib/iphone-control`） | `~/.local/share/iphone-control` |
| 控制接口 | 容器内 `/run/iphone-control/ctl.sock` | `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` |
| usbmuxd | 守护进程拉起并看护（容器里没有 udev） | 系统服务，守护进程不拉起 |
| 配对记录 | 数据目录的 `lockdown/` | 系统的 `/var/lib/lockdown` |
| 未签名的 WDA | 镜像自带 | 第一次签名前下载同一版本（核对 sha256） |
| 配置 | `.env` | 环境变量，例如 systemd 的 `EnvironmentFile` |

守护进程、命令行和 MCP 要用同一个用户运行；别的用户要用 `--socket`、`--data-dir`（或 `ICTL_SOCKET`、`ICTL_DATA_DIR`）指过去。以 root 运行时默认路径和容器里一样，守护进程会自己拉起 usbmuxd；宿主机已有 usbmuxd 服务时设 `ICTL_MANAGE_USBMUXD=0`。装了系统 usbmuxd 又要用回容器版时，先 `sudo systemctl mask usbmuxd`，否则两边会抢设备。

## 命令

| 命令 | 作用 |
|---|---|
| `daemon` | 常驻守护进程（容器入口） |
| `mcp` | MCP stdio 服务，经控制接口调用守护进程 |
| `mcp-config` | 打印把 MCP 注册到 Claude Code 的命令 |
| `status` / `doctor` | 状态快照和最近日志 / 逐层诊断，每项失败都给出修复办法 |
| `setup` | 首次安装向导（可重复运行） |
| `login` | 登录 Apple ID（密码不回显，支持双重认证和短信） |
| `sign` / `resign [--force]` | 立即签名安装 WDA / 描述文件快过期才重签 |
| `passcode set\|clear`、`unlock-reset` | 自动解锁（默认关闭） |
| `wda-bridge --port 8100` | 把 WDA 桥接到 127.0.0.1 给别的工具用：没有认证，用完就停 |
| `notify-test` | 每类事件发一条测试通知 |

## 配置

全部通过环境变量配置，都可以不填。完整列表见 [.env.example](.env.example)，常用的：

| 变量 | 含义 |
|---|---|
| `HTTP_PROXY`、`HTTPS_PROXY` | 访问苹果、anisette、开发者镜像和推送通知时用的代理 |
| `IPHONE_UDID` | 接了多台手机时指定用哪台 |
| `BARK_KEY`、`BARK_SERVER`、`TG_BOT_TOKEN`、`TG_CHAT_ID` | 通知 |
| `RESIGN_MIN_DAYS`、`RESIGN_CHECK_HOURS` | 什么时候重签 WebDriverAgent |
| `ICTL_ANISETTE_URL` | 用自建 anisette 服务代替默认的第三方服务 |
| `ICTL_DDI_URL`、`ICTL_DDI_DIR` | 开发者镜像从哪里来 |

## 安全与隐私

部署前请读 [SECURITY.md](SECURITY.md)。要点：

- 数据目录里有 Apple ID 会话、开发签名私钥、配对记录；开启自动解锁时还有明文的锁屏密码。要备份，不要提交或分享。
- 容器以 root 运行：要访问 USB 设备节点，拔插后还要能重启 `usbmuxd`。
- 登录 Apple ID 要用 anisette 服务，默认是公共的第三方服务；可用 `ICTL_ANISETTE_URL` 换成自建服务。
- 能访问控制接口（Unix socket）或正在运行的 `wda-bridge` 的人，都能操控手机。

## 免责声明

本项目与 Apple Inc. 没有任何关联，也未获其认可或赞助。iPhone、iOS、Xcode 是 Apple Inc. 的商标。

只在你自己拥有或获得授权的设备和账号上使用。自动操作第三方 App 可能违反其服务条款；苹果可能限制或停用开发者账号，建议使用专门的 Apple ID。本软件按「原样」提供，不提供任何担保（见 [LICENSE](LICENSE)）。

本项目不绕过任何 iOS 安全机制：信任电脑、开启开发者模式、信任开发者证书、启用 UI 自动化都需要机主本人在手机上操作。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/tools.md](docs/tools.md) | MCP 工具和参数（英文） |
| [docs/troubleshooting.md](docs/troubleshooting.md) | 各状态的含义和常见问题的处理（英文，附中文状态原文） |
| [SECURITY.md](SECURITY.md) | 威胁模型、加固建议、漏洞报告（英文） |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 构建、测试（有无手机两种）、提交 PR（英文，PR 和 issue 可以用中文） |
| [CHANGELOG.md](CHANGELOG.md) | 版本记录 |
| [docs/architecture.md](docs/architecture.md) | 架构、守护进程状态机、模块划分、数据目录 |
| [docs/decisions.md](docs/decisions.md) | 关键取舍记录 |
| [docs/licensing.md](docs/licensing.md)、[THIRD_PARTY.md](THIRD_PARTY.md) | 许可证分析、第三方组件 |
| [docs/dev/](docs/dev/) | 开发过程：spike 结果、路线图、回归用例 |

## 致谢

基于 [idevice](https://github.com/jkcoxson/idevice)（设备协议）、[Impactor](https://github.com/claration/impactor)（Apple ID 与签名）、[apple-codesign](https://github.com/indygreg/apple-platform-rs)、[WebDriverAgent](https://github.com/appium/WebDriverAgent)（手机端自动化）、[usbmuxd](https://github.com/libimobiledevice/usbmuxd) 和 [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk)。

## 许可证

本仓库的代码使用 [MIT](LICENSE)。vendor 进来的组件保留各自的许可证（MPL-2.0 和 MIT），见 [THIRD_PARTY.md](THIRD_PARTY.md)。开发者镜像等苹果文件在运行时下载，从不分发。
