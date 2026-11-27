# macOS 真机验证

> **当前验证基线：v0.3.0。** 上一次完整 macOS 真机验证完成于 2026-11-03（早于 0.1.3，见文末[历史验证记录](#历史验证记录)）。v0.3.0 相比当时新增了 iOS 27+ 原生控制（免 Apple ID / WDA）、多设备管理、浏览器控制台（VideoToolbox 硬件 H.264 / 手机原生 HEVC、局域网 HTTPS、`/mcp`）、macOS `libproc` 回环进程识别、launchd 后台服务与自更新、异常重启自感知及 `usbmuxd Listen` 热插拔即时唤醒等能力，待在真实 Mac 上按本清单完整重跑。

CI 已经确认项目在 macOS（Apple Silicon）上能编译、链接，单元测试全部通过（含 `x86_64-apple-darwin` 交叉编译检查与 `libproc` 回环进程识别单测），但单元测试不连手机。这份文档是在一台插着 iPhone/iPad 的 Mac 上要跑的完整验证清单，换了 macOS、iOS 版本、机型或发大版本时可照着重跑一遍。

## 要回答的问题

| # | 问题 | 通过的标准 |
|---|---|---|
| 1 | 系统自带的 `usbmuxd`、首次引导与 launchd 服务是否正常 | 装好 launchd 服务后 `iphone-control` / `setup` 顺利走完（iOS 27+ 自动跳过 Apple ID/WDA，iOS 26 及以下完成签名安装），`status` 就绪、`doctor` 全绿 |
| 2 | iOS 27+ 原生控制（免 WDA）与 iOS 26 及以下 WDA 路径是否均正常 | 隧道、开发者镜像、截图、无障碍读树、HID 点击/输入/粘贴均正常；WDA 路径（如有 iOS 26 及以下设备或设 `ICTL_IOS_BACKEND=wda`）正常启动并响应 |
| 3 | MCP 回归脚本与多设备切换是否正常 | `tests/device/run.py` 全部通过；多台设备下 `devices` / `select`、`iphone_list_devices` / `iphone_select_device` 及按设备存锁屏密码均正常 |
| 4 | 控制台（`viewer`）、VideoToolbox / HEVC 画面与 `libproc` 进程识别是否正常 | 画面流畅、点击/操作正常；WDA 或 `IPHONE_CONTROL_VIEWER_CODEC=videotoolbox` 下不依赖 ffmpeg 直出 H.264；任务子进程通过本机回环 HTTP API 试图操控自身任务时被拒绝 |
| 5 | launchd 后台服务、自更新与异常重启自感知是否正常 | `service install` / `service viewer install` / `update check` / 受管更新（或本地签名包演练）通过；`kill -9` 守护进程后 launchd 拉起并写异常重启记录，正常 `service restart` 不误报 |
| 6 | 和 macOS 自带的 `remoted` / `remotepairingd` 会不会抢连接，拔插能否即时唤醒 | 开着 Finder（及可选 Xcode）连续操作与 30 分钟 `soak` 不掉线；USB 拔插经 `usbmuxd Listen` 立即触发状态更新，锁屏/重启后自动恢复 |
| 7 | （可选）Intel Mac 预编译包是否可用 | `macos-x86_64` 包在真实 Intel Mac 上直接启动、连机与 VideoToolbox 编码正常（目前仅 Rosetta 跑过单测） |

第 6 项在跨 macOS 大版本升级时风险最高：插上 iPhone 后，macOS 的 `remoted`、`remotepairingd` 会自己和手机建立 CoreDevice 连接。本项目经 USB 走 CoreDeviceProxy 建自己的隧道，理论上来源地址不同，但每次 macOS / iOS 大版本变更仍应实测确认。

## 准备

- **Mac**：macOS 11 及以上，Apple Silicon（Intel Mac 也可测，结果单独记录）。
- **iOS 设备**：iOS 17 及以上，用**能传数据的数据线**连接：
  - 测 **iOS 27+ 原生控制**：准备一台 iOS 27 及以上的 iPhone/iPad（默认免 Apple ID、免 WDA）。
  - 测 **多设备 / WDA 路径**：再准备一台 iOS 26 及以下设备（或对 iOS 27+ 临时在 `~/.config/iphone-control/env` 中设置 `ICTL_IOS_BACKEND=wda`）。
- **Apple ID**：仅测试 WDA 路径（iOS 26 及以下或 `ICTL_IOS_BACKEND=wda`）时需要，免费账号即可，建议专用账号。
- **不需要装 Xcode**。如果已安装 Xcode，第 6 步多测场景 C。
- **可选 `ffmpeg`**：macOS 上 WDA 画面直接调用系统 VideoToolbox 编 H.264，无需 `ffmpeg`；但 iOS 27+ 默认走手机原生 HEVC，如果用不支持 HEVC 的浏览器打开控制台，需要 `brew install ffmpeg` 转成 H.264。
- **充电设置**：把手机「设置 → 电池 → 充电」上限改为 100%，Mac 接上电源。开着「80% 上限」时，手机在 80% 附近反复开始/停止充电，USB-C 重新协商供电会让 USB 断开 3～5 秒（约每分钟一次），导致第 6 步大面积失败；测完改回。

### 安装

按你要验证的安装路径选择其一（验证第 5 步 `iphone-control update` 自更新需使用**签名脚本安装**或 **Homebrew**；验证第 2 步 `debug` 和第 6 步 D `soak` 需额外从源码带 `--features debug-cmds` 编译一份）：

```bash
# 方式 A：签名脚本安装（推荐，装到 ~/.local/bin/iphone-control 并自动配置 launchd 服务）
curl -fsSLo iphone-control-install.sh https://github.com/Derrors/iphone-control/releases/latest/download/install.sh
sh iphone-control-install.sh

# 方式 B：Homebrew
brew install derrors/tap/iphone-control
brew services start iphone-control

# 方式 C：从源码编译（跑 debug / soak 检查必须带 --features debug-cmds）
xcode-select --install            # 命令行工具：C 编译器、git（已装会提示）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust 1.89 以上
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked --features debug-cmds
iphone-control --version
```

### 注意事项

- **全程用普通用户运行，不要 sudo。** 以 root 运行时，守护进程改用 Linux 的系统路径（`/var/lib/iphone-control`、`/run/iphone-control`），安装脚本也会拒绝 root。
- 数据目录：`~/.local/share/iphone-control`；控制接口：`~/.local/share/iphone-control/ctl.sock`（macOS 没有 `XDG_RUNTIME_DIR`）；设置文件：`~/.config/iphone-control/env`；launchd 服务日志：`~/Library/Logs/iphone-control/daemon.log`。
- 同一时间只能有一个程序控制手机：守护进程、`iphone-control debug …`、Xcode 的 UI 测试不要同时跑。

### 记录环境

```bash
sw_vers; uname -m; iphone-control --version
# 源码编译时也记一下：
rustc -V
```

记下测试设备的型号、iOS 版本与连接数量，填到文末的结果表里。

## 第 0 步：系统 usbmuxd 能看到手机

```bash
ls -l /var/run/usbmuxd     # 应该是一个 socket（s 开头）
```

插上手机。macOS 弹出「允许配件连接」时点「允许」。手机上弹出「要信任此电脑吗？」时可先不点，留到第 1 步由引导处理（若已在 Finder 信任过也无妨，在结果里注明）。

## 第 1 步：引导安装、launchd 服务与 `doctor`

如果使用源码编译且尚未安装 launchd 服务，先安装并启动后台服务（或先在另一终端前台运行 `iphone-control daemon` 观察实时输出）：

```bash
iphone-control service install
iphone-control service status
ls -l ~/Library/LaunchAgents/io.github.derrors.iphone-control.plist
```

运行首次引导（已运行过引导时改用 `iphone-control setup`）：

```bash
iphone-control
```

逐步观察并记录设备配置阶段（`setup`）：

| 步骤 | iOS 27+ 原生控制（默认） | iOS 26 及以下 / `ICTL_IOS_BACKEND=wda` | 要特别留意 |
|---|---|---|---|
| `[1/6]` 等待手机连接 | 打印手机名称/型号/UDID | 同左 | 一直等不到：检查系统 `usbmuxd` |
| `[2/6]` 配对 | 「已配对」或「配对成功」 | 同左 | 配对记录保存在 `/var/db/lockdown`（和 Finder 共用）；若报错记下原文，并在 Finder 信任后重试 |
| `[3/6]` 开发者模式 | 「已打开」或按提示打开并重启 | 同左 | 手机重启后能否自动恢复连接 |
| `[4/6]` 登录 Apple ID | **自动跳过**（提示原生控制不用 WDA/签名） | 登录免费 Apple ID | 确认 iOS 27+ 不应要求登录 Apple ID |
| `[5/6]` 签名并安装 WDA | **自动跳过** | 下载未签名 WDA（校验 sha256）、签名并安装 | WDA 路径首次签名时手机保持解锁 |
| `[6/6]` 等待就绪 | 挂载开发者镜像、连无障碍服务 →「就绪（原生控制）」 | 信任开发者证书、输 XCTest 密码 →「WDA 就绪」 | 后台 WDA 应启动且只监听手机回环 `127.0.0.1` |

完成后检查状态与逐层诊断：

```bash
iphone-control status      # iOS 27+ 应显示「控制方式 原生」；WDA 路径记下 Bundle ID 供第 2 步使用
iphone-control doctor      # 每一项都应是 ✅（确认 macOS 画面行提示 VideoToolbox + HEVC/ffmpeg，无 Linux 误报）
```

## 第 2 步：底层诊断（`debug`，需 `--features debug-cmds`）

这些检查直接操作手机，**先停掉守护进程**：

```bash
iphone-control service stop
```

停掉后，手机灵动岛（或状态栏）上的「配对的电脑」图标应在几秒内消失；如果一直不消失，记录下来。

`--width`、`--height` 是屏幕点尺寸，默认 `430×932`（Pro Max），其他机型按实际尺寸传入（例如 6.1 英寸 Pro 为 `393×852`）：

```bash
# 开发者镜像：已挂载就报告版本，否则下载并挂载
iphone-control debug ddi ~/.local/share/iphone-control/ddi

# CoreDevice 截图
iphone-control debug screenshot /tmp/ictl-shot.png && open /tmp/ictl-shot.png

# iOS 27+ 无障碍读树（原生控制核心）
iphone-control debug ax-screen

# HID 点击 + Home：先手动打开「设置」；点标题区域后应回到主屏幕（以手机实际效果为准）
iphone-control debug --width 430 --height 932 hid 215 120

# 可选：HID 键盘输入与剪贴板粘贴（先在手机上点中一个输入框）
iphone-control debug type "hello123"
iphone-control debug paste "测试粘贴"

# WDA 路径（iOS 26 及以下，或测试 WDA 时）
iphone-control debug --width 430 --height 932 e2e <WDA Bundle ID> --out /tmp/ictl-e2e.png
```

测完重新启动守护进程：

```bash
iphone-control service start
iphone-control status
```

## 第 3 步：MCP 回归脚本与多设备

### 3.1 回归脚本

等待 `iphone-control status` 显示就绪后运行：

```bash
python3 --version   # 需要 3.10 以上；系统自带 python3 若偏旧可用 brew install python 或 uv
python3 -m venv .venv && .venv/bin/pip install -r tests/device/requirements.txt
MCP_CMD="iphone-control mcp" .venv/bin/python tests/device/run.py
```

默认步骤无害（Home、时钟标签页、Spotlight 搜索），跨语言跨分辨率校验每步结果；所有步骤均需通过，截图保存在 `./shots`。可选进一步运行针对性场景：

```bash
MCP_CMD="iphone-control mcp" .venv/bin/python tests/device/scenarios.py --list
```

可选验证英文界面与开发者工具：在临时环境跑 `IPHONE_CONTROL_LANG=en iphone-control status` / `doctor` 与 `IPHONE_CONTROL_DEV_TOOLS=1`，核对双语输出无异常。

### 3.2 多设备切换（接入 2 台设备时）

同时插上两台 iPhone/iPad（理想组合：1 台 iOS 27+ 原生 + 1 台 iOS 26 及以下 WDA）：

```bash
iphone-control devices                  # 应列出两台设备并标出当前受控的一台
iphone-control select <另一台名称或UDID> # 切换受控设备
iphone-control status                   # 确认已切换（含原生/WDA 控制方式自动切换）
iphone-control passcode set             # 可选：为单台设置自动解锁密码，核对 secrets/passcodes/<UDID> 按设备隔离
iphone-control select auto              # 切回自动选
```

同时接入 AI 客户端（`iphone-control mcp-config`）或通过 `run.py` 自定义步骤调用 `iphone_list_devices` 和 `iphone_select_device`，确认切换前后均正常工作；拔掉被 `select` 选定的设备时，确认状态变为离线而非静默切到另一台。

## 第 4 步：控制台（`viewer`）、VideoToolbox 与 `libproc` 进程识别

### 4.1 控制台画面与 VideoToolbox

启动控制台并打印登录链接与令牌：

```bash
iphone-control viewer token
iphone-control viewer                   # 或装成后台服务：iphone-control service viewer install
```

用浏览器打开回环地址并测试：
1. **认证与表单登录**：未登录访问返回登录页；手动在登录页粘贴令牌提交表单可正常登录（无 403 / `Origin: null` 报错）；使用带 token 的一次性登录链接会自动去掉 URL query 并写入 Cookie。
2. **画面与编码**：
   - **iOS 27+**：默认走手机原生 HEVC；在 Safari（支持 HEVC）下流畅出图，点击/滑动/输入正常；在不支持 HEVC 的浏览器下确认若未装 `ffmpeg` 给出提示，装了 `ffmpeg` 后自动转 H.264。
   - **WDA 设备（或临时设 `IPHONE_CONTROL_VIEWER_CODEC=videotoolbox`）**：观察控制台启动日志，确认直接调用 macOS **VideoToolbox** 硬件编 H.264（**不依赖 `ffmpeg`**），拖动滚动流畅、无明显延迟积压。
3. **可选局域网 HTTPS 与 `/mcp`**：使用 `iphone-control viewer --lan` 启动，在同局域网另一台设备用 HTTPS 打开、核对自签证书指纹、测试只读令牌（`iphone-control viewer token add guest --read-only`）仅能查看不能操作。

### 4.2 `libproc` 回环 HTTP 客户端 PID 识别

macOS 上 `viewer/peer.rs` 通过 Darwin `libproc` 反查回环 HTTP 请求的客户端 PID，防止任务子进程拿到控制台登录令牌后通过 `http://127.0.0.1:8200/api/tasks/...` 自批或停止自己的任务。

验证步骤（控制台运行在 `127.0.0.1:8200`，先记下 owner 令牌 `$TOKEN`）：

```bash
# 1. 在 task run 子进程内，用合法登录令牌经回环 HTTP 接口尝试暂停/停止当前任务（应被 403 / 拒绝，日志记入自控拦截）
iphone-control task run --title "libproc-check" -- sh -c '
  curl --noproxy "127.0.0.1,localhost" -i -X POST \
    -H "Authorization: Bearer '"$TOKEN"'" \
    -H "Origin: http://127.0.0.1:8200" \
    -H "Content-Type: application/json" \
    -d "{}" \
    "http://127.0.0.1:8200/api/tasks/${IPHONE_CONTROL_TASK_ID}/pause"
  sleep 2
'
```

期望结果：任务子进程发出的暂停请求被守护进程拒绝（不可自控自己的任务）；而同时在**另一个独立终端**执行 `iphone-control task pause <id>` 或在浏览器控制台点暂停/停止，可以正常控制该任务。

## 第 5 步：launchd 自更新与异常重启自感知

### 5.1 检查与自更新

```bash
iphone-control update check
iphone-control update check --json
```

在签名脚本安装或 Homebrew 安装环境上：
- 若当前低于最新稳定版，直接跑 `iphone-control update` 验证实际升级（核对后台 `viewer` 服务自动暂停并在升级后恢复、空闲 MCP 不断、新守护进程 PID 切换且通过健康检查）。
- 若已在最新版，可按 [docs/dev/release-updates.md](release-updates.md) 中的临时密钥 + 本地 HTTPS 演练流程，用独立 `HOME` 和独立 launchd 标签演练一次正常升级与健康检查失败自动回滚。

### 5.2 异常重启自感知与防风暴

确保守护进程由 launchd 托管（`iphone-control service status` 正常）：

```bash
# 1. 正常重启不应触发异常重启记录
iphone-control service restart
tail -n 20 ~/.local/share/iphone-control/logs/daemon.log

# 2. 强制 kill -9 守护进程，等待 launchd 自动拉起
pkill -9 -f 'iphone-control daemon'
sleep 5
iphone-control status
tail -n 20 ~/.local/share/iphone-control/logs/daemon.log
```

期望结果：`kill -9` 后 launchd 自动拉起守护进程，`daemon.log` 中出现异常退出恢复记录（若已配置通知渠道则收到一条重启告警，30 分钟冷却期内再次 `kill -9` 不刷屏）；正常 `service restart` 绝不误报。

## 第 6 步：与 `remoted` 共存、长测与热插拔恢复

开三个窗口同时观察：

```bash
# 窗口 1：守护进程事件日志
tail -f ~/.local/share/iphone-control/logs/daemon.log

# 窗口 2：macOS 系统服务的日志（必须写全路径：zsh 有同名内置命令 log）
/usr/bin/log stream --style compact \
  --predicate 'process == "remoted" OR process == "remotepairingd"' | tee ~/remoted.log
```

在窗口 3 用循环对各场景连续操作约 10 分钟（每轮约半分钟）：

```bash
export MCP_CMD="iphone-control mcp"
for i in $(seq 1 20); do
  .venv/bin/python tests/device/run.py > /tmp/ictl-round-$i.log 2>&1 && echo "第 $i 轮 ok" || echo "第 $i 轮失败"
  sleep 5
done
```

| 场景 | 做法 |
|---|---|
| A. 基线 | 关掉 Finder 窗口，只跑循环 |
| B. Finder | 打开 Finder，在侧边栏选中这部 iPhone，停在它的页面上，跑循环；中途切换几次「通用」「文件」等标签 |
| C. Xcode（装了才测） | 打开 Xcode 的 Window → Devices and Simulators 并选中这部手机，跑循环 |
| D. 隧道长测 | 停掉守护进程（`iphone-control service stop`），Finder 保持选中手机，运行 `iphone-control debug soak --minutes 30`（需 `--features debug-cmds`），测完重启服务 |
| E. 热插拔与恢复 | 守护进程运行中：拔线再插回（观察 `daemon.log` 是否在 1～2 秒内因 `usbmuxd Listen` 立即唤醒，而非等满 10 秒）；锁屏再解锁；重启手机（重启后手动解锁，WDA 路径按提示输密码）。记录每次恢复耗时 |

每个场景核对三处：
- 循环里有没有失败轮次（失败看 `/tmp/ictl-round-N.log`）。
- `daemon.log` 里有没有反复出现隧道重建、无障碍断连、WDA 重启、离线记录。
- `remoted.log` 里有没有 `interrupted`、`disconnect` 记录，时间点是否与我们的操作/失败对应。

判定：
- **通过**：A、B（及 C）全部轮次成功，D 跑满 30 分钟不断开，E 均自动恢复且插拔即时感知。
- **部分通过**：偶尔断开但能自动恢复；记下频率与触发条件。
- **不通过**：开着 Finder 或 Xcode 就持续断开。

## 已知差异（不算失败）

- 数据目录在 `~/.local/share/iphone-control`，不是 Mac 习惯的 `~/Library/Application Support`。
- 控制接口在 `~/.local/share/iphone-control/ctl.sock`（macOS 无 `XDG_RUNTIME_DIR`）。
- Mac 睡眠时守护进程暂停；长测或无人值守时在终端运行 `caffeinate -i`。

## 清理

```bash
# 按安装方式卸载：
sh iphone-control-install.sh --uninstall                 # 签名脚本安装
# brew services stop iphone-control && brew uninstall iphone-control   # Homebrew
# iphone-control service uninstall && cargo uninstall iphone-control   # 源码安装

# 若装了控制台后台服务：
iphone-control service viewer uninstall

# 可选：彻底删除数据目录与设置（含 Apple ID 会话、令牌和签好的 WDA）
rm -rf ~/.local/share/iphone-control ~/.config/iphone-control
```

仅用过 WDA 路径时：在手机上删除 WebDriverAgent 这个 App（「设置 → 通用 → VPN 与设备管理」里的描述文件随 App 一起消失）。

## 结果（v0.3.0 模板）

把下面这张表填好，连同失败时的日志原文，贴到 issue 或 PR 里。

| 项目 | 结果 |
|---|---|
| Mac 型号 / 架构（arm64 或 x86_64）/ macOS 版本 | |
| 测试设备与 iOS 版本（注明原生 / WDA、是否多台） | |
| 安装方式（install.sh / Homebrew / 源码）与版本 | |
| 是否装了 Xcode / ffmpeg | |
| 第 0 步 `usbmuxd` 看到设备 | |
| 第 1 步 首次引导 / `setup` / launchd 服务 / `doctor` | |
| 第 2 步 `debug`（ddi / screenshot / ax-screen / hid / e2e）与图标消失 | |
| 第 3 步 `run.py` 回归脚本与多设备切换（含英文界面） | |
| 第 4 步 控制台 + VideoToolbox / HEVC + `libproc` 自控拦截 | |
| 第 5 步 launchd `update` 自更新与 `kill -9` 异常重启自感知 | |
| 第 6 步 A 基线 / B Finder / C Xcode | |
| 第 6 步 D `soak` 30 分钟 / E 热插拔即时唤醒与恢复 | |
| 其他现象 | |

---

## 历史验证记录

### 2026-11-03（早于 0.1.3，PR #14）— 通过

> 历史记录保留供对照。当时尚未支持 iOS 27+ 原生控制、多设备、控制台、`libproc`、launchd 自更新；当时使用的 `ICTL_MANAGE_USBMUXD=0` 已随去除 Docker 架构移除（D5、D14）。

**环境**：MacBook Pro（Mac15,6，M3 Pro）/ macOS 26.6.2，未装 Xcode；iPhone 15 Pro Max / iOS 27.0.1；普通用户、历史参数 `ICTL_MANAGE_USBMUXD=0`、前台运行、旧 6 步纯 WDA 向导。

| 项目 | 结果 |
|---|---|
| 第 0～1 步 usbmuxd、setup（旧 6 步 WDA）、doctor | ✅（配对沿用了手机和这台 Mac 已有的配对） |
| 第 2 步 ddi / screenshot / hid / e2e | ✅ e2e 11 项共 8.3 秒 |
| 第 3 步 回归脚本 | ✅ 14/14 |
| 第 4 步 A 基线 / B Finder | ✅ 20/20 / 20/20 |
| 第 4 步 C Xcode | 跳过（没装 Xcode） |
| 第 4 步 D soak 30 分钟 | ✅ 530/530，没断开 |
| 第 4 步 E 拔插 / 锁屏 / 重启 | ✅ 都自动恢复 |

结论：和 `remoted` 不会抢连接；所有断开都是 USB 物理层先断，`remoted` 只是跟着断。

当时验证中发现并修复的问题（PR #14）：
- USB 断开几秒又接上、整段落在两次巡检之间时，守护进程继续用 usbmuxd 的旧设备编号，一直报 `bad device`，不会恢复（Linux 上也会发生）。
- `doctor` 在 macOS 上误报「USB 上没有 Apple 设备」，修复提示是 Linux 的说法。
- 手机开着「80% 上限」、Mac 用电池时，USB 大约每分钟断开一次（见上文「准备」）。

当时未覆盖：Xcode 打开时的共存、`setup` 自己发起配对、开发者镜像的下载和挂载（每次都已由 macOS 挂好）、停掉守护进程后「配对的电脑」图标是否消失、多设备、控制台与 VideoToolbox / `libproc`、launchd 服务与自更新、iOS 27+ 原生控制、Intel Mac。
