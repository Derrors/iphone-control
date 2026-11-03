# macOS

iphone-control 也能直接跑在 Mac 上：用 macOS 自带的 usbmuxd，不需要 Xcode，也不需要付费开发者账号。和 Linux 上从源码运行基本一样，区别见文末的[已知差异](#已知差异)。

::: info 验证情况
已在 MacBook Pro（Apple M3 Pro）/ macOS 26.6.2 + iPhone 15 Pro Max / iOS 27.0.1 上完整验证：安装向导、全部 MCP 工具、开着 Finder 连续操作、同一条隧道连续 30 分钟、拔插和手机重启后自动恢复。Intel Mac 还没测过。验证步骤和结果见 [macOS 真机验证](https://github.com/Derrors/iphone-control/blob/main/docs/dev/macos-verification.md)。
:::

## 需要

- Apple Silicon 的 Mac
- Xcode 命令行工具（提供 C 编译器和 git），**不需要**完整的 Xcode
- Rust 1.88 以上
- 一部 iOS 17 以上的 iPhone，用能**传数据**的线连接
- 一个免费 Apple ID（建议专门注册一个）

::: warning 不能用 Docker
Docker Desktop 在 Mac 上把容器跑在 Linux 虚拟机里，不支持 USB 直通，容器看不到手机。在 Mac 上只能直接运行程序。
:::

## 安装

目前 Releases 里没有 macOS 的预编译二进制，要从源码编译（M3 Pro 上约 2 分半）：

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
cp .env.example ~/.config/iphone-control/env    # 按需填代理、通知
set -a; . ~/.config/iphone-control/env; set +a
iphone-control daemon
```

改了配置要重启守护进程（Ctrl-C 后重新运行）。变量说明见[配置](../guide/configuration)。

### 保持运行

还没有 launchd 配置，守护进程要留在一个终端（或 tmux）里。Mac 睡眠时它也会暂停，长时间无人值守时用 `caffeinate` 防止睡眠：

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

```bash
cd iphone-control && git pull
cargo install --path apps/iphone-control --locked
```

然后重启守护进程。

## 卸载

先在守护进程的终端按 Ctrl-C 停掉，然后：

```bash
cargo uninstall iphone-control
rm -rf ~/.local/share/iphone-control ~/.config/iphone-control   # 会删掉 Apple ID 会话和签好的 WDA
```

手机上删除 WebDriverAgent 这个 App，「VPN 与设备管理」里的描述文件会随它一起消失。

## 已知差异

| | Linux（从源码运行） | macOS |
|---|---|---|
| 安装 | 预编译二进制或从源码编译 | 只能从源码编译 |
| 常驻 | systemd 用户服务 | 还没有 launchd 配置，在终端里前台运行 |
| usbmuxd | 系统包，插上手机时由 udev 拉起 | macOS 自带，一直在运行 |
| 数据目录 | `~/.local/share/iphone-control` | 同左（不是 Mac 习惯的 `~/Library/Application Support`） |
| 控制接口 | `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` | `~/.local/share/iphone-control/ctl.sock` |
| 配对记录 | `/var/lib/lockdown` | `/var/db/lockdown`（和 Finder 共用） |
| Docker | 支持 | 不支持 |

还没在 Mac 上验证过的：Xcode 的「设备与模拟器」窗口开着时会不会抢连接、手机第一次在这台 Mac 上配对的流程、Intel Mac。遇到问题欢迎[反馈](https://github.com/Derrors/iphone-control/issues)。
