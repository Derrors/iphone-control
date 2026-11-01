# iphone-control

在 Linux 上（不需要 Mac、不需要付费开发者账号）控制一台通过 USB 连接的真实 iPhone，并把能力以 MCP 工具的形式提供给 AI agent。用 Rust 写成，产物是**一个二进制**，外加系统自带的 `usbmuxd`。

> **状态：功能已实现，待真机验证。** 代码覆盖了路线图阶段 1～7 的功能（设备层、守护进程、MCP、签名重签、通知、自动解锁、打包），
> 78 个离线单测和 `clippy -D warnings` 都通过；**还没有在真机上跑过任何一项**。
> 真机验证（[spike-plan](docs/spike-plan.md)）通过之前，正式可用的仍是保底方案 [iphone-control-toolkit](../iphone-control-toolkit)。
> 哪些地方必须靠真机确认，见 [roadmap 的完成情况](docs/roadmap.md#完成情况)。

## 与 iphone-control-toolkit 的关系

| | iphone-control-toolkit（B 方案，保底） | iphone-control（本仓库，D 方案） |
|---|---|---|
| 做法 | 容器里装 go-ios、pymobiledevice3、plumesign 的命令行，由 Python MCP 和 shell 脚本调用 | 基于 Rust 库 [idevice](https://github.com/jkcoxson/idevice) 和 [Impactor](https://github.com/claration/impactor) 的组件，一个进程完成全部工作 |
| 语言 | Go + Python + Rust + Shell | Rust（手机端 WDA 仍是 Objective-C） |
| 进程 | supervisor.sh + 多个命令行进程，每条 HID 命令重建隧道 | 一个守护进程独占设备，隧道、HID 会话、WDA 会话常驻复用 |
| WDA 端口 | 转发到 127.0.0.1:8100 | 经 usbmuxd 直连手机，**本机不监听任何端口** |
| 许可证 | MIT（自己的代码） | MIT（自己的代码）；vendor 的签名组件是 MPL-2.0，见 [licensing](docs/licensing.md) |
| iOS 27 实测 | 全部通过 | 待验证 |

两者的 MCP 工具名、参数、返回文字保持一致，切换时 agent 侧不用改。

## 快速开始（真机验证通过之后）

```bash
cp .env.example .env            # 按需改代理、通知
docker compose up -d --build    # 首次构建要编译 Rust，约 10 分钟
docker exec -it iphone-control iphone-control setup   # 配对 → 开发者模式 → 登录 Apple ID → 签名安装 WDA
docker exec -it iphone-control iphone-control mcp-config   # 打印注册到 Claude Code 的命令
```

> 不要和 toolkit 同时连着同一部手机：两边都会拉起 usbmuxd、各自持有 XCTest 会话。

## 命令

| 命令 | 作用 |
|---|---|
| `daemon` | 常驻守护进程（容器入口）：状态机、usbmuxd 看护、重签调度、通知、控制接口 |
| `mcp` | MCP stdio 服务，经控制接口调用守护进程 |
| `status` / `doctor` | 状态快照、最近日志 / 逐层诊断 |
| `setup` | 首次安装向导，可重复运行 |
| `login` | 登录 Apple ID（密码不回显，支持双重认证和短信） |
| `sign` / `resign [--force]` | 签名安装 WDA / 快过期才重签 |
| `passcode set\|clear` / `unlock-reset` | 自动解锁（默认关闭） |
| `wda-bridge --port 8100` | 显式把 WDA 桥接到 127.0.0.1，供外部工具使用 |

## 文档

| 文档 | 内容 |
|---|---|
| [architecture.md](docs/architecture.md) | 架构图、单进程守护状态机、模块划分、数据和秘密目录 |
| [capability-matrix.md](docs/capability-matrix.md) | toolkit 的每项能力在本项目里由哪个 crate 实现 |
| [spike-plan.md](docs/spike-plan.md) | 真机验证 5 项（`spike/` 程序） |
| [roadmap.md](docs/roadmap.md) | 分阶段计划、**完成情况**、必测的回归用例 |
| [licensing.md](docs/licensing.md) | 许可证分析、不能分发的苹果文件 |
| [decisions.md](docs/decisions.md) | 关键取舍记录 |

## 目录

```
crates/
  ictl-device/    设备层：usbmuxd 发现、配对、隧道、开发者镜像（含下载）、HID（含媒体流）、截图、App
  ictl-wda/       WDA：启动 XCTest runner、直连设备端口的 HTTP 客户端、元素树解析
  ictl-sign/      Apple ID 登录、注册设备、描述文件、签名（修复 .xctest 漏签）、签后自检
  ictl-unlock/    锁屏自动解锁（默认关闭）
  ictl-daemon/    守护进程：状态机、usbmuxd 看护、重签调度、通知、控制接口
  ictl-mcp/       MCP 服务（stdio），13 个工具与 toolkit 一致
apps/
  iphone-control/ 唯一的可执行程序
spike/            真机验证程序（调用与守护进程相同的库）
vendor/           Impactor 的 plume_core / plume_store / omnisette 与打过补丁的 apple-codesign（见 THIRD_PARTY.md）
docker/           Dockerfile（编译 → debian:trixie-slim + usbmuxd）
```

## 构建与测试

```bash
docker run --rm -v "$PWD":/src -w /src rust:1-trixie sh -c \
  'rustup component add clippy && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings'
```

国内网络需要 `--network host` 和 `HTTP_PROXY`/`HTTPS_PROXY`。不需要 nightly；最低 Rust 1.88（由 rmcp 决定），开发时用 1.99。
`vendor/` 不是 workspace 成员，不对它跑 clippy；它自带的编译警告属于上游。
