# iphone-control

在 Linux 上（不需要 Mac、不需要付费开发者账号）控制一台通过 USB 连接的真实 iPhone，并把能力以 MCP 工具的形式提供给 AI agent。用 Rust 写成，最终产物是**一个静态二进制**，外加系统自带的 `usbmuxd`。

> **状态：设计阶段。** 本仓库目前只有设计文档和 Cargo workspace 骨架，所有函数都是 `todo!()`。能用的版本是保底方案 [iphone-control-toolkit](../iphone-control-toolkit)。

## 与 iphone-control-toolkit 的关系

| | iphone-control-toolkit（B 方案，保底） | iphone-control（本仓库，D 方案） |
|---|---|---|
| 做法 | 容器里装 go-ios、pymobiledevice3、plumesign 的命令行，由 Python MCP 和 shell 脚本调用 | 基于 Rust 库 [idevice](https://github.com/jkcoxson/idevice) 和 [Impactor](https://github.com/claration/impactor) 的组件组装，一个进程完成全部工作 |
| 语言 | Go + Python + Rust + Shell | Rust（手机端 WDA 仍是 Objective-C） |
| 许可证 | MIT（自己的代码） | MIT（自己的代码）；依赖里有 MPL-2.0 文件级 copyleft，见 [licensing](docs/licensing.md) |
| iOS 27 实测 | 全部通过 | 全部待验证，见 [spike-plan](docs/spike-plan.md) |

D 方案验证通过并达到功能对等之前，toolkit 一直是可用版本。两者的 MCP 工具名和参数保持一致，切换时 agent 侧不用改。

## 文档

| 文档 | 内容 |
|---|---|
| [architecture.md](docs/architecture.md) | 架构图、单进程守护状态机、模块划分、数据和秘密目录 |
| [capability-matrix.md](docs/capability-matrix.md) | 现有每项能力在 D 方案里由哪个 crate 实现、状态和风险 |
| [spike-plan.md](docs/spike-plan.md) | 第 0 步：真机验证 5 项，决定 D 方案能否走下去 |
| [roadmap.md](docs/roadmap.md) | 分阶段计划、完成标准、必测的回归用例 |
| [licensing.md](docs/licensing.md) | 许可证分析、不能分发的苹果文件 |
| [decisions.md](docs/decisions.md) | 关键取舍记录 |

## 目录

```
crates/
  ictl-device/    设备层：usbmuxd 发现、配对、隧道、开发者镜像、HID、截图、App
  ictl-wda/       WDA：启动 XCTest runner、HTTP 客户端、元素树解析
  ictl-sign/      Apple ID 登录、注册设备、描述文件、签名（含 .xctest）
  ictl-unlock/    锁屏自动解锁（默认关闭）
  ictl-daemon/    守护进程：设备状态机、usbmuxd 看护、重签调度、通知、本地控制接口
  ictl-mcp/       MCP 服务（stdio），工具定义与 toolkit 一致
apps/
  iphone-control/ 唯一的可执行程序：daemon / mcp / doctor / setup / sign 等子命令
spike/            第 0 步真机验证用的小程序
```

## 构建

骨架阶段只做编译检查：

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.91-slim-trixie cargo check --workspace
```

（需要 `--network host`，并在国内网络下设置 `HTTP_PROXY`/`HTTPS_PROXY`。）

## 骨架编译检查

2026-11-01，`rust:1.91-slim-trixie`（cargo 1.91.1）：

- `cargo check --workspace`：**通过**，只有 1 个 dead_code 警告（`WdaClient.session` 尚未使用）。idevice 用 `ring` 后端（`default-features = false`），不需要 cmake/clang。
- `cargo test --workspace`：3 个单测通过（密码栏解析、区域放大倍数）。
- 不需要 nightly：Impactor 的 `.cargo/config.toml` 只有 macOS 链接参数，仓库里没有 `rust-toolchain` 文件；最低版本由 rmcp 决定（1.88）。
- 骨架里 `ictl-sign` 暂时直接依赖 crates.io 的 apple-codesign 0.29.0（未打补丁），vendor Impactor 组件在阶段 4 进行。
