# 第 0 步：真机验证（spike）

目的：用最少的代码确认 idevice 在目标设备（iPhone 15 Pro Max，iOS 27.0.1）上能完成 D 方案依赖的 5 项关键能力。**这一步的结果决定 D 方案是否继续。**

代码放在 `spike/`，一个子命令验证一项，每项独立可重跑。

## 执行前提和注意事项

- **必须先停掉 toolkit 的容器**（或在其中停止 usbmuxd 以外的进程）。两边同时持有隧道、HID 会话、XCTest 会话会互相干扰。
- 第 5 项启动 WDA 会让 toolkit 的 WDA **中断几分钟**。**执行前要先和用户确认时间**，结束后恢复 toolkit 容器并跑一次它的回归测试。
- 手机保持解锁；如果 toolkit 开着自动锁定 1 分钟，spike 期间改成「永不」或先手动解锁。
- spike 不签名、不安装任何东西，只复用手机上已装好的 WDA 和已有配对记录（只读挂载 toolkit 的 `lockdown/`）。
- 运行环境：`rust:1.91-slim-trixie` 容器，`--privileged`，挂 `/dev/bus/usb`，容器内跑 Debian 的 `usbmuxd`。

## 验证顺序（按风险从高到低）

### 1. 开发者镜像挂载（Cryptex + TSS）— 风险最高

| | |
|---|---|
| 做法 | 先在手机上卸载已有镜像（`cryptexd uninstall com.apple.MobileAsset.DDI`，或手机重启后天然就是未挂载状态）；用 `Cryptex1Assets::load(<DDI Restore 目录>)` 读取 toolkit 已缓存的同一份 DDI（`state/pymobiledevice3/Xcode_iOS_DDI_Cryptex/`，只读）；调用 `cryptexd::install_ddi()`；再 `installed_ddi()` 确认 |
| 通过标准 | `install_ddi` 返回成功；`installed_ddi()` 能查到 `com.apple.MobileAsset.DDI`；随后 RSD 服务列表里出现 `com.apple.coredevice.hid.universalhidservice`（证明 `dtuhidd` 来自新镜像） |
| 额外记录 | TSS 请求耗时、响应里票据在 `Cryptex1,Ticket` 还是 `ApImg4Ticket` |
| 失败退路 | ① 抓 TSS 请求与 pmd3 的请求对比，缺字段就补 `add_cryptex1_tags`，修好提 PR；② 短期折中：镜像挂载继续用 pymobiledevice3（只在手机重启后需要一次，挂上后一直有效），其余全走 Rust。代价是镜像里多一个 Python 依赖 |

> 卸载镜像会让 toolkit 的 HID 路线和 WDA 暂时不可用，属于需要用户确认的操作。如果不想卸载，可以先在手机重启后的天然未挂载窗口里做这一项。

### 2. HID 触摸

| | |
|---|---|
| 做法 | 建立隧道 → 启动 displayservice 媒体流（RTP 数据丢弃）→ `UniversalHidServiceClient::list_connected_services()` → `tap(x, y)`（0～65535 归一化）→ 截图确认；再测 `drag`、`IndigoHidClient::send_button(home)`、`MainKeyboardService` 打英文 |
| 通过标准 | 点击「设置」里的返回按钮能退回上一页；Home 键回主屏幕；`drag` 能翻页 |
| 注意 | idevice 文档明确：**没有媒体流时事件会被静默丢弃**，调用不报错。测试必须以截图对比为准，不能只看返回值 |
| 失败退路 | 确认服务名（idevice 两个都实现了：`com.apple.coredevice.hid.indigo`、`com.apple.coredevice.hid.universalhidservice`）；对照 pmd3 `hid_service.py` 的报文逐字节比较。HID 只是后备路线，失败不阻塞 D 方案，但 WDA 不可用时就没有兜底 |

### 3. 截图

| | |
|---|---|
| 做法 | `core_device::screencaptureservices::take_screenshot(ImageFormat::Png)` |
| 通过标准 | 拿到 1290×2796 PNG；记录耗时（toolkit 的 pmd3 路线每次 2～3 秒，含建隧道） |
| 失败退路 | 改用 `dvt::screenshot`（Instruments 通道）；再不行只依赖 WDA 截图 |

### 4. 隧道长时间稳定性

| | |
|---|---|
| 做法 | 建一条隧道，持续 30 分钟：每 2 秒截一次图（大响应），每 10 秒一次 RSD 小请求；中途锁屏 5 分钟再解锁 |
| 通过标准 | 30 分钟内无断线，或断线后 10 秒内能重建；内存不持续增长；锁屏期间已建隧道不断（toolkit 实测 go-ios 隧道锁屏不断） |
| 额外记录 | 截图吞吐（MB/s）、单次截图延迟分布 |
| 失败退路 | jktcp 的问题优先修 jktcp 并提 PR；实在不行改用内核 TUN 隧道（需要 root/CAP_NET_ADMIN，和"免 root"目标冲突，只作为可选项） |

### 5. 启动 WDA（会中断 toolkit 的 WDA，需用户确认）

| | |
|---|---|
| 做法 | `TestConfig::from_installation_proxy(<WDA runner bundle id>, None)` → `XCUITestService::run_until_wda_ready(cfg, 60s)`；用 `WdaClient` 直连设备 8100 端口读 `/status`、`/wda/screen`、`/source?format=xml&excluded_attributes=...`、`/screenshot`；最后 `abort()` |
| 通过标准 | 60 秒内 `ready: true`；元素树（排除可见性属性后约 60KB）和截图（约 4.8MB 的 JSON）都能完整读回；`abort()` 后 runner 进程退出 |
| 附加测试 | 在手机重启后的首次启动上跑一次，确认能识别「Enable UI Automation」超时的错误，以便实现 `NeedsAutomationPin` |
| 失败退路 | ① 用 `idevice-tools xctest --bridge <bundle id>` 命令行复现，区分是我们用法问题还是库问题；② 对照 go-ios `runwda` 的 testmanagerd 报文；③ 只有这一项不过时，可以临时让守护进程调用 go-ios 启动 WDA，其余用 Rust |

## 判定

| 结果 | 决定 |
|---|---|
| 5 项全过 | D 方案进入阶段 1（见 roadmap） |
| 只有第 1 项不过 | D 方案继续；镜像挂载暂用 pymobiledevice3 折中，同时给 idevice 补 TSS |
| 第 2 或第 3 项不过 | D 方案继续；HID/截图后备路线延后，WDA 为唯一路线 |
| 第 4 或第 5 项不过 | 暂停 D 方案，先修上游或评估 C 方案；toolkit 继续作为正式版本 |

## 产出

- `spike/RESULTS.md`：每项的通过与否、耗时、日志摘要（去掉 UDID 等个人信息）
- 发现的上游问题列表（准备提 issue/PR）
