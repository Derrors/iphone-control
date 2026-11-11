# macOS

iphone-control 也能直接跑在 Mac 上：用 macOS 自带的 usbmuxd，不需要 Xcode，也不需要付费开发者账号。和 Linux 上基本一样，区别见文末的[已知差异](#已知差异)。

::: info 验证情况
已在 MacBook Pro（Apple M3 Pro）/ macOS 26.6.2 + iPhone 15 Pro Max / iOS 27.0.1 上验证：安装向导、当时的全部 MCP 工具、开着 Finder 连续操作、同一条隧道连续 30 分钟、拔插和手机重启后自动恢复。这次验证早于 0.1.3，多设备工具（`iphone_list_devices`、`iphone_select_device`）没有在 macOS 上验证过。Intel Mac 还没测过。验证步骤和结果见 [macOS 真机验证](https://github.com/Derrors/iphone-control/blob/main/docs/dev/macos-verification.md)。
:::

## 需要

- Apple Silicon 的 Mac：一行命令和 Homebrew 装的是编好的程序，什么都不用另外装。Intel Mac 没有预编译包，要从源码编译（Xcode 命令行工具和 Rust 1.88 以上）。都**不需要**完整的 Xcode
- 一部 iOS 17 以上的 iPhone 或 iPad，用能**传数据**的线连接
- 一个免费 Apple ID（建议专门注册一个）

## 安装

### 一行命令（推荐）

```bash
curl -fsSL https://github.com/Derrors/iphone-control/releases/latest/download/install.sh | sh
```

它会下载最新版本、用 Python 3 和 OpenSSL 3 验证发行清单签名和安装包 SHA-256，装到 `~/.local/bin/iphone-control`，再装一个 launchd 用户服务（`~/Library/LaunchAgents/io.github.derrors.iphone-control.plist`）：登录后自动启动守护进程，崩溃后自动重启，读取 `~/.config/iphone-control/env`，日志在 `~/Library/Logs/iphone-control/daemon.log`。可选参数写在 `sh -s --` 后面：`--version v0.2.0`（指定版本）、`--prefix 目录`（装到 `目录/bin`）、`--no-service`（只装程序，不装服务）。

### Homebrew

```bash
brew install derrors/tap/iphone-control
brew services start iphone-control      # 守护进程常驻，登录后自动启动
```

服务同样读取 `~/.config/iphone-control/env`，日志在 `$(brew --prefix)/var/log/iphone-control.log`。设置模板在 `$(brew --prefix)/opt/iphone-control/share/iphone-control/env.example`。

### 从源码编译（Intel Mac）

需要 Xcode 命令行工具和 Rust 1.88 以上，M3 Pro 上约 2 分半：

```bash
xcode-select --install                                         # 已经装过会提示
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh  # 装好后按提示重开终端
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked              # 装到 ~/.cargo/bin/iphone-control
iphone-control --version
```

后台服务由下面的设置引导装好。

## 运行

**全程用普通用户运行，不要加 sudo。** macOS 自带的 usbmuxd 一直在运行，守护进程直接用它。以 root 运行时数据目录和控制接口会换成按 Linux 设的系统路径（`/var/lib`、`/run`），一行命令的脚本也会拒绝以 root 运行。

用数据线接上手机，然后：

```bash
iphone-control
```

第一次运行会一步步带你完成设置：语言、代理、后台服务、配对、开发者模式、Apple ID、签名安装 WDA、自动解锁和通知（可选）、注册到 Claude Code 和 Codex。手机上要做的事见[快速开始](../guide/getting-started#first-run)。如果这部手机已经在 Finder 里信任过这台 Mac，配对这一步会直接显示「已配对」。设置好以后再运行 `iphone-control` 只显示状态。

### 手动做

引导做的事都能单独做：

```bash
iphone-control daemon        # 在一个终端里前台运行守护进程（不装后台服务时）
iphone-control setup         # 另开一个终端，重走引导，做过的步骤跳过
iphone-control mcp-config    # 打印注册 MCP 的命令，复制运行即可
```

`mcp-config` 打印的命令形如 `claude mcp add iphone -s user -- /Users/you/.local/bin/iphone-control --socket /Users/you/.local/share/iphone-control/ctl.sock mcp`。Homebrew 装的，程序路径是 `/opt/homebrew/opt/iphone-control/bin/iphone-control`，升级后不变。其他客户端见[接入 AI 客户端](../guide/mcp-clients)。

### 配置

设置在 `~/.config/iphone-control/env`，引导和 `iphone-control setup --reconfigure` 会写好它。手改之后运行 `iphone-control service restart`。变量说明见[配置](../guide/configuration#settings-file)。

### 保持运行

一行命令、Homebrew 或引导装的后台服务，登录后自动启动守护进程，不用再管：

```bash
iphone-control service status     # 服务和守护进程是否在运行
iphone-control service restart    # 重启，比如改了设置之后
```

Mac 睡眠时守护进程会暂停。长时间无人值守时，在一个终端里运行 `caffeinate -i` 并让它一直开着，防止睡眠。

## 让 USB 连接稳定

- **把手机的充电上限改成 100%**：设置 → 电池 → 充电。开着「80% 上限」时，手机在 80% 附近反复开始、停止充电，每次 USB-C 都会重新协商供电，USB 断开 3～5 秒，大约每分钟一次。守护进程能自己重连，但正在执行的操作会失败。
- 长时间运行时 Mac 最好接上电源。
- Finder 可以一直开着、停在手机页面，不会抢连接。

## 日常命令

```bash
iphone-control status    # 状态、描述文件还剩多久、最近日志
iphone-control doctor    # 逐层诊断
```

看 macOS 系统服务的日志时要写全路径 `/usr/bin/log`：zsh 有一个同名的内置命令 `log`，直接写 `log stream` 会报 `too many arguments`。

usbmuxd 本身出问题（`doctor` 报连不上 usbmuxd）时重启它：

```bash
sudo launchctl kickstart -k system/com.apple.usbmuxd
```

## 升级

升级前先看[更新日志](https://github.com/Derrors/iphone-control/blob/main/CHANGELOG.zh-CN.md)里的升级注意事项。

受管脚本安装：关闭 Viewer、MCP 等客户端后，运行 `iphone-control update apply`。Homebrew：`brew upgrade iphone-control && brew services restart iphone-control`。从源码编译：

```bash
cd iphone-control && git pull
cargo install --path apps/iphone-control --locked
iphone-control service restart
```

## 卸载

一行命令安装的：`curl -fsSL https://github.com/Derrors/iphone-control/releases/latest/download/install.sh | sh -s -- --uninstall`。Homebrew：`brew services stop iphone-control && brew uninstall iphone-control`。从源码编译的：`iphone-control service uninstall && cargo uninstall iphone-control`。

都会保留数据目录和设置。要彻底删除：

```bash
rm -rf ~/.local/share/iphone-control ~/.config/iphone-control   # 会删掉 Apple ID 会话和签好的 WDA
```

手机上删除 WebDriverAgent 这个 App，「VPN 与设备管理」里的描述文件会随它一起消失。

## 已知差异

| | Linux | macOS |
|---|---|---|
| 安装 | 一行命令、Homebrew、deb 或从源码编译 | 一行命令、Homebrew 或从源码编译（只有 Apple Silicon 的包） |
| 常驻 | systemd 用户服务 | launchd 用户服务 |
| usbmuxd | 系统包，插上手机时由 udev 拉起 | macOS 自带，一直在运行 |
| 数据目录 | `~/.local/share/iphone-control` | 同左（不是 Mac 习惯的 `~/Library/Application Support`） |
| 控制接口 | `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` | `~/.local/share/iphone-control/ctl.sock` |
| 配对记录 | `/var/lib/lockdown` | `/var/db/lockdown`（和 Finder 共用） |

还没在 Mac 上验证过的：Xcode 的「设备与模拟器」窗口开着时会不会抢连接、手机第一次在这台 Mac 上配对的流程、iPad、接多台设备、Intel Mac。遇到问题欢迎[反馈](https://github.com/Derrors/iphone-control/issues)。
