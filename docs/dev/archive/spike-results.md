# spike 结果

> **历史记录，2026-11-01 第 0 步的结果**，不再更新。文中的 `iphone-control debug` 现在要用 `--features debug-cmds` 编译才有。

设备：iPhone 15 Pro Max（iPhone16,2），iOS 27.0.1（24A446）。日期：2026-11-01。
环境：单独的 spike 容器（`--privileged`，挂 `/dev/bus/usb`，容器内 Debian usbmuxd 1.1.1），程序为当时的 `spike/` crate（`cargo build --release -p ictl-spike`）。

> 本文原为 `spike/RESULTS.md`。`spike/` 已删除：`ddi`、`hid`、`screenshot`、`soak`、`wda`、`e2e` 并入隐藏命令 `iphone-control debug <子命令>`（`apps/iphone-control/src/debug.rs`）；`cryptexd-probe`、`ddi-steps` 已删除。下文的命令按现在的写法给出。
依赖：idevice 锁定上游 master `89702099`（0.1.68 发版代码的 cryptexd install 会挂住，见下方「上游问题」）。

## 结论：5 项全部通过 → D 方案进入阶段 1 的真机确认

| # | 项 | 结果 | 关键数据 |
|---|---|---|---|
| 1 | 开发者镜像（Cryptex + TSS） | ✅ 通过 | 未挂载 → 挂载成功，TSS 0.6s，install 0.6s，全程 1.2s |
| 2 | HID | ✅ 通过（部分） | Home 键确认生效；tap 未单独确认，drag、键盘未测 |
| 3 | 截图 | ✅ 通过 | 1290×2796 PNG，约 7.8MB |
| 4 | 隧道稳定性 | ✅ 通过（部分） | 30 分钟 529 次截图 0 失败，共 4.1GB，最慢 1.49s；锁屏、内存未记录 |
| 5 | 启动 WDA | ✅ 通过 | 2.1s 就绪；元素树 42KB/0.25s；截图 433KB/0.18s |

> 下文的「未覆盖」和「待补」是当时的状态。后来确认的结论见[路线图的完成情况](roadmap.md#完成情况)：HID 点击和 Home 已由 `debug e2e` 以前台 App 变化确认生效；「Enable UI Automation」的报错文字已确认（回归用例 [#8](../regression-cases.md)）；从未挂载开始的挂载流程在 iOS 27 上仍没测到（重启后镜像还在）。

## 1. 开发者镜像

- 挂载前 `copy-installed` 返回空数组（未挂载）。
- `ddi-steps <DDI 目录>`（分步、每步限时；这个子命令已删除）：
  ```
  读个性化标识：成功（0.0s）
  取 cryptex nonce：成功（0.0s）
  TSS 签票：成功（0.6s）
  票据字段：Cryptex1,Ticket
  install：成功（0.6s）
  ```
- 挂载后：`copy-installed` 列出 `com.apple.MobileAsset.DDI`；`iphone-control debug ddi` 报告「开发者镜像已挂载 27.1.5228.8」；RSD 服务列表含 `com.apple.coredevice.hid.universalhidservice`。
- 票据在 `Cryptex1,Ticket`（不是 `ApImg4Ticket`）。
- **未覆盖**：正式的 `Tunnel::mount_ddi`（`debug ddi` 的挂载分支）还没在未挂载状态下跑过，下次手机重启后补跑。
- 早先一次（03:12）卡在连接 cryptexd 的 XPC 握手之后（`exit=124`）。之后同一程序的只读探测和挂载都正常，未再复现；怀疑是 idevice 补丁前的构建，未确认。

## 2. HID

- `iphone-control debug hid <x> <y>`：在「时钟 › 秒表」界面点击后按 Home，前后截图对比，Home 回到主屏幕，按键生效。点击落点在空白处，点击本身是否生效看不出来。
- **未覆盖**：点击生效（「设置」返回按钮）、`drag` 翻页、`MainKeyboardService` 打字。HID 只是后备路线，不阻塞。

## 3. 截图

- `screencaptureservice` 返回 1290×2796、16 位 RGBA PNG，每张约 7.8MB。
- 单次耗时见第 4 项（隧道已建好时平均约 1.4s，最慢 1.49s）。

## 4. 隧道稳定性

- `iphone-control debug soak --minutes 30`：同一条隧道每 2 秒截一次图，**529 次成功、0 次失败，共 4131.7MB，最慢 1.49s**，期间没有重建隧道。
- 吞吐：每张约 7.8MB / 约 1.4s ≈ 5.5MB/s。
- **未覆盖**：中途锁屏 5 分钟、内存曲线、10 秒一次的 RSD 小请求。留给阶段 2 的 24 小时测试。

## 5. 启动 WDA

`iphone-control debug wda com.facebook.WebDriverAgentRunner.xctrunner.<团队 ID>`（复用 toolkit 已签名安装的 WDA）：

```
WDA 就绪，耗时 2.1s
屏幕 ScreenSize { width: 430.0, height: 932.0 }
元素树 42771 字节，耗时 0.25s，解析出 38 个元素
截图 433223 字节，耗时 0.18s
runner 已停止
```

- 第一次运行（10:30）`/status` 每次 5 秒超时：WDA 收到 `Connection: close` 也不关连接，客户端读到 EOF 就会挂住。已改为收齐一个完整响应即停（提交 `53c5c82`），修复后一次通过。
- runner 停止后，手机上的 WDA 仍响应 `/status`（停止后半分钟内仍为 `ready: true`，约 80 秒后不可达），与回归用例 #12 一致：就绪判定不能只看 `/status`。
- **未覆盖**：手机重启后首次启动时「Enable UI Automation」超时的报错文字（回归用例 #8）。

## 上游问题

| 组件 | 问题 | 状态 |
|---|---|---|
| idevice 0.1.68 | cryptexd install 挂住（persistence 字段错、隧道 MSS） | 已在上游 master 修复、未发版；本项目用 `[patch.crates-io]` 锁 `89702099`，发版后换回 |

## 待补（不阻塞阶段 1）

1. 手机重启后：正式 `debug ddi` 挂载一次 + `debug wda` 首次启动看「Enable UI Automation」报错。
2. HID：返回按钮、drag、键盘。
3. 隧道：锁屏 5 分钟不断线、内存曲线（并入阶段 2 的 24 小时测试）。
