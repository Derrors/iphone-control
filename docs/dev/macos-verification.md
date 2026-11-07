# macOS 真机验证

> **下面的验证记录早于 0.1.3**：多设备工具（`iphone_list_devices`、`iphone_select_device`，命令 `devices`、`select`）没有在 macOS 上验证过。

CI 已经确认项目在 macOS（Apple Silicon）上能编译、链接，单元测试全部通过（PR #12），但单元测试不连手机。这份文档是在一台插着 iPhone 的 Mac 上要跑的步骤，换了 macOS、iOS 版本或机型时可以照着重跑一遍。

## 已有的验证记录

**2026-11-03，通过。** MacBook Pro（Mac15,6，M3 Pro）/ macOS 26.6.2，没装 Xcode；iPhone 15 Pro Max / iOS 27.0.1；普通用户、`ICTL_MANAGE_USBMUXD=0`、前台运行。

| 项目 | 结果 |
|---|---|
| 第 0～1 步 usbmuxd、setup、doctor | ✅（配对沿用了手机和这台 Mac 已有的配对） |
| 第 2 步 ddi / screenshot / hid / e2e | ✅ e2e 11 项共 8.3 秒 |
| 第 3 步 回归脚本 | ✅ 14/14 |
| 第 4 步 A 基线 / B Finder | ✅ 20/20 / 20/20 |
| 第 4 步 C Xcode | 跳过（没装 Xcode） |
| 第 4 步 D soak 30 分钟 | ✅ 530/530，没断开 |
| 第 4 步 E 拔插 / 锁屏 / 重启 | ✅ 都自动恢复 |

和 `remoted` 不会抢连接：所有断开都是 USB 物理层先断，`remoted` 只是跟着断。

验证中发现并修复的问题（PR #14）：
- USB 断开几秒又接上、整段落在两次巡检之间时，守护进程继续用 usbmuxd 的旧设备编号，一直报 `bad device`，不会恢复。Linux 上也会发生。
- `doctor` 在 macOS 上误报「USB 上没有 Apple 设备」，修复提示是 Linux 的说法。
- 手机开着「80% 上限」、Mac 用电池时，USB 大约每分钟断开一次（见下面「准备」）。

还没覆盖：Xcode 打开时的共存、`setup` 自己发起配对、开发者镜像的下载和挂载（每次都已由 macOS 挂好）、停掉守护进程后「配对的电脑」图标是否消失、Intel Mac。

## 要回答的问题

| # | 问题 | 通过的标准 |
|---|---|---|
| 1 | 系统自带的 usbmuxd 能不能直接用，配对能不能完成 | `setup` 走完 6 步，`status` 显示就绪 |
| 2 | 隧道、开发者镜像、截图、HID 是否正常 | `debug` 的几项检查都通过 |
| 3 | WDA 和 MCP 工具是否正常 | 回归脚本默认步骤全部成功 |
| 4 | 和 macOS 自带的 `remoted` 会不会抢连接 | 开着 Finder 连续操作约 30 分钟不掉线 |

第 4 项风险最大：插上 iPhone 后，macOS 的 `remoted`、`remotepairingd` 会自己和手机建立 CoreDevice 连接。pymobiledevice3 记录过，同一台电脑连手机的 RSD 服务时，后来的连接会把先前的挤掉。本项目经 USB 走 CoreDeviceProxy 建自己的隧道，理论上来源地址不同，但只能实测确认。

## 准备

- **Mac**：Apple Silicon。Intel Mac 也可以测，结果单独记录。
- **iPhone**：iOS 17 以上，这份清单实测过的是 iPhone 15 Pro Max（iOS 27.0.1）。用**数据线**连接。
- **Apple ID**：免费的即可，建议用专门的账号，不用主账号。
- **系统语言**：回归脚本的默认步骤按中文 App 名（「时钟」「设置」）操作，手机最好设为简体中文；不是的话第 3 步要改步骤。
- **不需要装 Xcode**。如果已经装了，第 4 步多测一个场景。
- **充电**：把手机「设置 → 电池 → 充电」的上限改成 100%，Mac 最好接上电源。开着「80% 上限」时，手机在 80% 附近反复开始、停止充电，每次 USB-C 重新协商供电都会让 USB 断开 3～5 秒，第 4 步会大面积失败。测完再改回来。

### 安装

```bash
xcode-select --install            # 命令行工具：C 编译器、git（已装会提示）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust，1.88 以上即可
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked --features debug-cmds   # 装到 ~/.cargo/bin/iphone-control；第 2 步和第 4 步 D 的 debug 命令要这个 feature
iphone-control --version
```

### 注意事项

- **全程用普通用户运行，不要 sudo。** 以 root 运行时，守护进程会自己启动一个 usbmuxd，和系统的冲突。保险起见，每个终端先执行：
  ```bash
  export ICTL_MANAGE_USBMUXD=0
  ```
- 数据目录是 `~/.local/share/iphone-control`，控制接口在 `~/.local/share/iphone-control/ctl.sock`（macOS 没有 `XDG_RUNTIME_DIR`）。
- 同一时间只能有一个程序控制手机：守护进程、`iphone-control debug …`、Xcode 的 UI 测试不要同时跑。

### 记录环境

```bash
sw_vers; uname -m; rustc -V; iphone-control --version
```

再记下 iPhone 型号和 iOS 版本，填到文末的结果表里。

## 第 0 步：系统 usbmuxd 能看到手机

```bash
ls -l /var/run/usbmuxd     # 应该是一个 socket（s 开头）
```

插上手机。macOS 弹出「允许配件连接」时点「允许」。手机上弹出「要信任此电脑吗？」时**先不要点**，留到第 1 步由 `setup` 处理；如果已经点了，也没关系，在结果里注明。

## 第 1 步：配对、开发者模式、签名

终端 A，前台运行守护进程，日志直接打印在这里：

```bash
iphone-control daemon
```

终端 B，运行安装向导：

```bash
iphone-control setup
```

逐步观察并记录：

| 步骤 | 期望 | 要特别留意 |
|---|---|---|
| [1/6] 等待手机连接 | 打印手机 UDID | 一直等不到：说明经系统 usbmuxd 找不到设备 |
| [2/6] 配对 | 「已配对」或「配对成功」 | 配对记录由 macOS 的 usbmuxd 保存（`/var/db/lockdown`）。如果报错，记下原文，然后在 Finder 里信任这台电脑，再重跑 `setup`，看能否复用 Finder 的配对 |
| [3/6] 开发者模式 | 「已打开」，或按提示在手机上打开并重启 | 手机重启后能否自动恢复连接 |
| [4/6] 登录 Apple ID | 登录成功 | 和平台无关，只要能通过即可 |
| [5/6] 签名并安装 WDA | 安装成功 | 第一次会下载未签名的 WDA（核对 sha256） |
| [6/6] 等待 WDA 启动 | 「WDA 就绪」 | 首次要在手机「设置 → 通用 → VPN 与设备管理」里信任你的 Apple ID；弹出「为 XCTest 输入 iPhone 密码」时输入锁屏密码 |

然后：

```bash
iphone-control status      # 状态应为就绪；记下「WDA」那一行的 Bundle ID，第 2 步要用
iphone-control doctor      # 每一项都应是 ✅
```

没合入 PR #14 的版本，`doctor` 会误报「❌ USB 上没有 Apple 设备」，修复提示也是 Linux 的说法（`systemctl`、udev），可以忽略。

## 第 2 步：隧道、开发者镜像、截图、HID

`debug` 子命令默认不编译，预编译二进制里也没有，要用上面「安装」里带 `--features debug-cmds` 的命令装。这些检查直接操作手机，**先在终端 A 按 Ctrl-C 停掉守护进程**。停掉后，手机灵动岛（或状态栏）上「配对的电脑」图标应该在几秒内消失；如果一直不消失，记录下来。

`--width`、`--height` 是屏幕的点尺寸，默认 430×932（Pro Max 机型）。其他机型请换成对应尺寸（例如 6.1 英寸 Pro 是 393×852）。

```bash
# 开发者镜像：已挂载就报告版本，否则下载并挂载
iphone-control debug ddi ~/.local/share/iphone-control/ddi

# CoreDevice 截图
iphone-control debug screenshot /tmp/ictl-shot.png && open /tmp/ictl-shot.png

# HID：先手动打开「设置」；命令会点一下给定坐标（这里是标题区域，没有副作用），再按 Home
iphone-control debug --width 430 --height 932 hid 215 120

# 端到端：隧道 → 镜像 → WDA → 截图 → 读元素 → 点「设置」→ Home，再用 HID 重做一遍
iphone-control debug --width 430 --height 932 e2e <第 1 步记下的 WDA Bundle ID> --out /tmp/ictl-e2e.png
```

HID 检查以手机上的实际效果为准（是否真的回到了主屏幕），命令返回成功不代表生效。

## 第 3 步：通过 MCP 跑回归脚本

重新启动守护进程（终端 A：`iphone-control daemon`），等 `iphone-control status` 显示就绪后，在终端 B 运行：

```bash
python3 --version   # 需要 3.10 以上；命令行工具自带的 python3 可能是 3.9，可用 brew install python 或 uv
python3 -m venv .venv && .venv/bin/pip install -r tests/device/requirements.txt
MCP_CMD="iphone-control mcp" .venv/bin/python tests/device/run.py
```

默认步骤都是无害的操作（Home、时钟 App 的标签页、Spotlight 搜索）。每一步都要成功，截图保存在 `./shots`。

可选：用 `iphone-control mcp-config` 打印注册命令，接入 Claude Code，让它实际操作几分钟。

## 第 4 步：和 remoted 共存

这一步回答最关键的问题。开三个窗口同时观察：

```bash
# 窗口 1：守护进程（第 3 步已在运行）

# 窗口 2：守护进程的事件日志
tail -f ~/.local/share/iphone-control/logs/daemon.log

# 窗口 3：macOS 系统服务的日志（要写全路径：zsh 有个同名的内置命令 log）
/usr/bin/log stream --style compact \
  --predicate 'process == "remoted" OR process == "remotepairingd"' | tee ~/remoted.log
```

每个场景用下面的循环连续操作约 10 分钟（每轮约半分钟）：

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
| D. 隧道长测 | 停掉守护进程，Finder 保持选中手机，运行 `iphone-control debug soak --minutes 30`（同一条隧道持续截图；要 `--features debug-cmds`） |
| E. 恢复 | 守护进程运行中：拔线再插回；锁屏再解锁；重启手机（重启后要手动解锁并输一次密码）。每次都看多久回到就绪 |

每个场景看三处：

- 循环里有没有失败的轮次（失败时看 `/tmp/ictl-round-N.log`）。
- `daemon.log` 里有没有反复出现隧道重建、WDA 重启、离线这类记录。
- `remoted.log` 里有没有 `interrupted`、`disconnect` 之类的记录，时间点是否和我们的操作、失败对得上。

判定：

- **通过**：A、B（以及 C）全部轮次成功，D 跑满 30 分钟不断开，E 都能自动恢复。
- **部分通过**：偶尔断开但能自动恢复。记下频率和触发条件，再决定要不要处理。
- **不通过**：只要开着 Finder 或 Xcode 就持续断开。需要另想办法（例如复用 macOS 自己的隧道），先不宣布支持。

## 已知差异（不算失败）

- 数据目录在 `~/.local/share` 下，不是 Mac 习惯的 `~/Library/Application Support`。
- 还没有 launchd 配置，只能前台运行。
- 不能用 Docker：Docker Desktop 在 Mac 上不支持 USB 直通。

## 清理

```bash
# 在终端 A 按 Ctrl-C 停掉守护进程
rm -rf ~/.local/share/iphone-control     # 删数据目录（含 Apple ID 会话和签好的 WDA）
cargo uninstall iphone-control
```

手机上：删除 WebDriverAgent 这个 App；「设置 → 通用 → VPN 与设备管理」里的描述文件随 App 一起消失。

## 结果

把下面这张表填好，连同失败时的日志原文，贴到 issue 或 PR 里。

| 项目 | 结果 |
|---|---|
| Mac 型号 / macOS 版本 | |
| iPhone 型号 / iOS 版本 | |
| 是否装了 Xcode | |
| 第 0 步 usbmuxd 看到手机 | |
| 第 1 步 配对方式（setup / Finder）| |
| 第 1 步 setup 6 步 | |
| 第 1 步 doctor | |
| 第 2 步 ddi / screenshot / hid / e2e | |
| 第 2 步 停掉后「配对的电脑」图标消失 | |
| 第 3 步 回归脚本 | |
| 第 4 步 A 基线 | |
| 第 4 步 B Finder | |
| 第 4 步 C Xcode | |
| 第 4 步 D soak 30 分钟 | |
| 第 4 步 E 拔插 / 锁屏 / 重启恢复 | |
| 其他现象 | |
