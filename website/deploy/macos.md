# macOS

iphone-control 也能直接跑在 Mac 上：用 macOS 自带的 usbmuxd，不需要 Xcode，也不需要付费开发者账号。和 Linux 上从源码运行基本一样，区别见文末的[已知差异](#已知差异)。

::: info 验证情况
已在 MacBook Pro（Apple M3 Pro）/ macOS 26.6.2 + iPhone 15 Pro Max / iOS 27.0.1 上验证：安装向导、当时的全部 MCP 工具、开着 Finder 连续操作、同一条隧道连续 30 分钟、拔插和手机重启后自动恢复。这次验证早于 0.1.3，多设备工具（`iphone_list_devices`、`iphone_select_device`）没有在 macOS 上验证过。Intel Mac 还没测过。验证步骤和结果见 [macOS 真机验证](https://github.com/Derrors/iphone-control/blob/main/docs/dev/macos-verification.md)。
:::

## 需要

- Apple Silicon 的 Mac
- 用预编译二进制什么都不用装；从源码编译要 Xcode 命令行工具和 Rust 1.88 以上。都**不需要**完整的 Xcode
- 一部 iOS 17 以上的 iPhone 或 iPad，用能**传数据**的线连接
- 一个免费 Apple ID（建议专门注册一个）

::: warning 不能用 Docker
Docker Desktop 在 Mac 上把容器跑在 Linux 虚拟机里，不支持 USB 直通，容器看不到手机。在 Mac 上只能直接运行程序。
:::

## 安装

### 一行命令（推荐）

```bash
curl -fsSL https://raw.githubusercontent.com/Derrors/iphone-control/main/install.sh | sh
```

它会下载最新版本、校验 sha256，装到 `~/.local/bin/iphone-control`，再装一个 launchd 用户服务（`~/Library/LaunchAgents/io.github.derrors.iphone-control.plist`）：登录后自动启动守护进程，崩溃后自动重启，读取 `~/.config/iphone-control/env`，日志在 `~/Library/Logs/iphone-control/daemon.log`。可选参数写在 `sh -s --` 后面：`--version v0.1.5`（指定版本）、`--prefix 目录`（装到 `目录/bin`）、`--no-service`（只装程序，守护进程自己启动）。

### Homebrew

```bash
brew install derrors/tap/iphone-control
brew services start iphone-control      # 守护进程常驻，登录后自动启动
```

用上面两种方式装好后，守护进程已经在运行，直接跳到下面「运行」一节的 `iphone-control setup`。

### 预编译二进制

从 0.1.2 起，[Releases](https://github.com/Derrors/iphone-control/releases) 里有 macOS（Apple Silicon，macOS 11 以上）的压缩包 `iphone-control-<版本>-macos-arm64.tar.gz`。在终端里下载安装（把 `V` 换成最新版本号）：

```bash
V=v0.1.5
curl -fsSLO https://github.com/Derrors/iphone-control/releases/download/$V/iphone-control-$V-macos-arm64.tar.gz
curl -fsSLO https://github.com/Derrors/iphone-control/releases/download/$V/iphone-control-$V-macos-arm64.tar.gz.sha256
shasum -a 256 -c iphone-control-$V-macos-arm64.tar.gz.sha256   # 应显示 OK
tar xzf iphone-control-$V-macos-arm64.tar.gz
mkdir -p ~/.local/bin && install -m 755 iphone-control-$V-macos-arm64/iphone-control ~/.local/bin/
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zprofile && source ~/.zprofile   # 只需做一次
iphone-control --version
```

::: tip 被系统拦下「无法验证开发者」
二进制没有经过苹果公证（那要付费开发者账号）。像上面这样用 `curl` 下载的不会被拦；用浏览器下载的会带上隔离标记，第一次运行时被 Gatekeeper 拦下。去掉标记即可：

```bash
xattr -d com.apple.quarantine ~/.local/bin/iphone-control
```

也可以在「系统设置 → 隐私与安全性」里点「仍要打开」。
:::

### 从源码编译

需要 Xcode 命令行工具和 Rust 1.88 以上，M3 Pro 上约 2 分半：

```bash
xcode-select --install                                         # 已经装过会提示
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh  # 装好后按提示重开终端
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked              # 装到 ~/.cargo/bin/iphone-control
iphone-control --version
```

## 运行

**全程用普通用户运行，不要加 sudo。** macOS 自带的 usbmuxd 一直在运行，守护进程直接用它；以 root 运行时守护进程会自己再启动一个 usbmuxd 和系统的抢设备，数据目录和控制接口也会换成按 Linux 设的系统路径（`/var/lib`、`/run`）。

在一个终端里运行守护进程，日志直接打印在这里：

```bash
iphone-control daemon
```

另开一个终端运行安装向导：

```bash
iphone-control setup
```

向导的六步和 Linux 上一样，手机上要做的事见[快速开始](../guide/getting-started#_2-运行安装向导)。如果这部手机已经在 Finder 里信任过这台 Mac，配对这一步会直接显示「已配对」。

向导显示「WDA 就绪」后，接入 AI 客户端：

```bash
iphone-control mcp-config    # 打印注册命令，复制运行即可
```

它打印的命令形如 `claude mcp add iphone -s user -- /Users/you/.cargo/bin/iphone-control --socket /Users/you/.local/share/iphone-control/ctl.sock mcp`。其他客户端见[接入 AI 客户端](../guide/mcp-clients)。

### 配置

配置写在 `~/.config/iphone-control/env`（格式同仓库里的 `.env.example`），启动守护进程前先加载：

```bash
mkdir -p ~/.config/iphone-control
cp .env.example ~/.config/iphone-control/env    # 按需填代理、通知（预编译包里叫 env.example）
set -a; . ~/.config/iphone-control/env; set +a
iphone-control daemon
```

改了配置要重启守护进程（Ctrl-C 后重新运行）。变量说明见[配置](../guide/configuration)。

### 保持运行

用一行命令或 Homebrew 安装的，守护进程已经是 launchd 服务，不用再管。手动安装的，守护进程要留在一个终端（或 tmux）里。Mac 睡眠时守护进程会暂停，长时间无人值守时用 `caffeinate` 防止睡眠：

```bash
caffeinate -i iphone-control daemon
```

## 让 USB 连接稳定

- **把手机的充电上限改成 100%**：设置 → 电池 → 充电。开着「80% 上限」时，手机在 80% 附近反复开始、停止充电，每次 USB-C 都会重新协商供电，USB 断开 3～5 秒，大约每分钟一次。守护进程能自己重连，但正在执行的操作会失败。
- 长时间运行时 Mac 最好接上电源。
- Finder 可以一直开着、停在手机页面，不会抢连接。

## 日常命令

和 Linux 一样，直接运行，不用加 `docker exec`：

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

一行命令安装的：重新运行同一条命令，它会先停服务、换程序、再启动。Homebrew：`brew upgrade iphone-control && brew services restart iphone-control`。

预编译二进制：按「安装」一节下载新版本，覆盖 `~/.local/bin/iphone-control`。从源码编译：

```bash
cd iphone-control && git pull
cargo install --path apps/iphone-control --locked
```

然后重启守护进程。

## 卸载

一行命令安装的：`curl -fsSL https://raw.githubusercontent.com/Derrors/iphone-control/main/install.sh | sh -s -- --uninstall`。Homebrew：`brew services stop iphone-control && brew uninstall iphone-control`。两种都保留数据目录，要彻底删除再执行下面第二行。

手动安装的，先在守护进程的终端按 Ctrl-C 停掉，然后：

```bash
rm -f ~/.local/bin/iphone-control      # 预编译二进制；从源码编译的用 cargo uninstall iphone-control
rm -rf ~/.local/share/iphone-control ~/.config/iphone-control   # 会删掉 Apple ID 会话和签好的 WDA
```

手机上删除 WebDriverAgent 这个 App，「VPN 与设备管理」里的描述文件会随它一起消失。

## 已知差异

| | Linux（从源码运行） | macOS |
|---|---|---|
| 安装 | 一行命令、deb、预编译二进制或从源码编译 | 一行命令、Homebrew、预编译二进制或从源码编译（只有 Apple Silicon 的包） |
| 常驻 | systemd 用户服务 | launchd 用户服务（一行命令或 Homebrew 安装时自动配置） |
| usbmuxd | 系统包，插上手机时由 udev 拉起 | macOS 自带，一直在运行 |
| 数据目录 | `~/.local/share/iphone-control` | 同左（不是 Mac 习惯的 `~/Library/Application Support`） |
| 控制接口 | `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` | `~/.local/share/iphone-control/ctl.sock` |
| 配对记录 | `/var/lib/lockdown` | `/var/db/lockdown`（和 Finder 共用） |
| Docker | 支持 | 不支持 |

还没在 Mac 上验证过的：Xcode 的「设备与模拟器」窗口开着时会不会抢连接、手机第一次在这台 Mac 上配对的流程、iPad、接多台设备、Intel Mac。遇到问题欢迎[反馈](https://github.com/Derrors/iphone-control/issues)。
