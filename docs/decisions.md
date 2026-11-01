# 关键决策记录

格式：背景 → 决定 → 理由 → 代价/何时重新评估。

## D1：基于 idevice，而不是从协议层自己写

- **决定**：设备通讯全部基于 `idevice`。版本用 `[patch.crates-io]` 锁在上游 master 的 git 提交 `89702099`（0.1.68 发版代码的 cryptexd install 会挂住，修复已在 master、未发版）；发版后换回 crates.io。
- **理由**：它已覆盖我们需要的全部能力：usbmuxd、lockdown、CoreDeviceProxy 用户态隧道、RSD、cryptexd 个性化镜像安装、universalhidservice/indigo HID、screencaptureservice、XCTest 启动 WDA、WDA 客户端；MIT；StikDebug、CrossCode 等在生产中使用；Impactor 也依赖它，和签名部分同源。
- **代价**：0.2.0 之前每个小版本都可能有破坏性变更；主要一人维护；`tss` 自述部分支持、TCP 栈自述 "naive"。
- **重新评估**：spike 第 4、5 项不过时（已全部通过，见 [spike-results](spike-results.md)）。

## D2：守护进程独占设备，MCP 是薄客户端

- **决定**：`daemon` 常驻持有隧道、HID 会话、XCTest 会话；`mcp` 通过 Unix socket 调用。
- **理由**：MCP 进程每个会话一个、短命、可能并发；HID 事件要求显示媒体流同时存在；WDA 只能有一个 runner。toolkit 每条 HID 命令重建隧道要 3～5 秒。
- **代价**：多一层协议。

## D3：WDA 不在本机监听端口

- **决定**：用 idevice 的 provider 直连设备端口访问 WDA；外部工具需要时显式开 bridge，只绑 127.0.0.1。
- **理由**：WDA 没有认证；toolkit 早期用 `ios forward` 监听了所有网卡，局域网内任何人都能控制手机。

## D4：WDA 仍然是 Objective-C，在 macOS CI 上编译

- **决定**：不重写 WDA；fork appium/WebDriverAgent，在 GitHub Actions macOS runner 上不签名编译，发布未签名 IPA + sha256。
- **理由**：WDA 是 XCTest bundle，只能用 Xcode 编译，依赖苹果私有/测试框架；Rust 无法替代。
- **代价**：发布流程依赖 macOS runner。
- **现状**：自己编译还没做；镜像构建时暂时下载 Appium 官方 Release 的未签名真机包（v16.15.3，校验 sha256）。

## D5：保留系统 usbmuxd，作为子进程看护

- **决定**：不内置 USB 协议栈，使用 Debian 的 `usbmuxd`。
- **理由**：idevice 只有 usbmuxd 客户端；自己实现 USB 多路复用收益很小；usbmuxd 是 GPL-3.0，作为独立进程运行不影响本项目许可证。
- **代价**：仍需处理容器无 udev 的热插拔问题（sysfs 兜底 + 重启）。

## D6：签名组件 vendor 而不是 git 依赖

- **决定**：把 Impactor 的 `plume_core`、`plume_store`、`omnisette` 和打过补丁的 apple-codesign vendor 进本仓库。`plume_utils` 不 vendor：需要的 bundle 逻辑改写进 `crates/ictl-sign/src/bundle.rs`（含 `.xctest` 修复）。
- **理由**：Impactor 用 `[patch.crates-io]` 把 apple-codesign 指向本地打过补丁的副本；作为 git 依赖引用时，**依赖方 workspace 的 patch 不会生效**，会拿到未打补丁的 apple-codesign。另外需要修 `.xctest` 漏签，还要去掉许可证有问题的可选依赖（见 THIRD_PARTY.md）。
- **代价**：要定期同步上游；MPL-2.0 文件改动需单独标注。修复同时提 PR 回上游，争取将来改回普通依赖。

## D7：开发者镜像下载由我们负责

- **决定**：`ictl-device` 提供 DDI 下载与缓存（来源可配置，默认与 pymobiledevice3 相同），`install_ddi` 只接收本地目录。
- **理由**：idevice 的 `Cryptex1Assets::load` 只读本地解包目录，不负责下载；苹果文件不能随项目分发。
- **重新评估**：idevice 若加入下载功能。

## D8：自动解锁默认关闭

- **决定**：需要显式开启并配置密码文件。
- **理由**：存储锁屏密码意味着能访问主机的人就能解锁手机；输错累计还可能触发设备抹除。保护规则见 capability-matrix §5。

## D9：所有依赖锁定精确版本

- **决定**：`Cargo.toml` 用 `=x.y.z`，`Cargo.lock` 入库，Docker 基础镜像锁 digest。
- **理由**：toolkit 两次被版本漂移坑过（mcp 2.x 改名；go-ios main 分支命令变化）。idevice 官方也要求锁定。

## D10：与 toolkit 工具接口一致

- **决定**：13 个 MCP 工具名、参数、返回文本格式保持与 toolkit 一致。
- **理由**：切换时 agent 侧零改动；toolkit 的回归脚本可直接用于验收。
