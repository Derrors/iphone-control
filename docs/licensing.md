# 许可证

## 结论

- 本项目自己的代码使用 **MIT**。
- 可以做到：依赖中没有 GPL 代码被链接进本项目的二进制。
- 但依赖里有 **MPL-2.0**（文件级 copyleft），vendor 和修改这些文件时要遵守 MPL，见下文。这一点比早期分析（"全部 MIT"）更严格，是读源码后修正的。

## 依赖许可证（按源码和 crates.io 核实，2026-11）

| 依赖 | 用途 | 许可证 | 使用方式 | 义务 |
|---|---|---|---|---|
| idevice 0.1.68 | 设备通讯 | MIT | crates.io 依赖 | 保留版权声明 |
| jktcp 0.1.8 | 用户态 TCP（idevice 间接依赖） | MIT | 间接依赖 | 同上 |
| plume_core（Impactor） | Apple ID、开发者 API、描述文件 | **MPL-2.0** | vendor 源码 | 被修改的 MPL 文件必须继续以 MPL-2.0 提供源码；文件头保留；不影响我们其它文件的 MIT |
| plume_utils（Impactor） | Bundle、Signer | MIT（Impactor workspace 许可） | vendor 源码，打 `.xctest` 补丁 | 保留版权声明 |
| omnisette（Impactor 3rdparty） | anisette 数据 | MIT（Impactor workspace 许可） | vendor 源码 | 保留版权声明 |
| apple-codesign 0.29.0 | 代码签名 | **MPL-2.0** | crates.io 依赖 + Impactor 的权限补丁 | 补丁后的文件以 MPL-2.0 提供源码（放在 `vendor/` 或 `patches/` 里即满足） |
| rmcp 3.8.0 | MCP SDK | Apache-2.0（部分历史代码 MIT） | crates.io 依赖 | 保留 NOTICE/版权声明 |
| tokio、reqwest、quick-xml、image、clap、serde 等 | 通用 | MIT / Apache-2.0 | crates.io 依赖 | 保留声明，用 `cargo about` 或 `cargo deny` 生成第三方清单 |
| WebDriverAgent（Appium fork） | 手机端代理 | BSD-3-Clause（Facebook 原始） | 发布未签名 IPA（自己在 macOS CI 上编译） | 保留 BSD 声明 |
| usbmuxd | USB 守护进程 | **GPL-3.0**（以其 README 为准；GitHub 显示 GPL-2.0 有误） | 系统包，独立进程，不链接 | 仅"放在一起分发"。发布镜像时要提供对应源码（Debian 源码包）和许可证文本 |

## MPL-2.0 怎么处理

MPL-2.0 是按文件计算的弱 copyleft：

- 我们自己新写的文件可以是 MIT，与 MPL 文件编译进同一个二进制没问题。
- 我们修改过的 MPL 文件（例如 apple-codesign 的 `bundle_signing.rs`）必须继续是 MPL-2.0，并在分发二进制时让接收者能拿到这些文件的源码。仓库公开即可满足。
- 做法：`vendor/` 下每个上游目录保留原 `LICENSE` 和文件头；`THIRD_PARTY.md` 列出每个文件的来源、上游提交号和改动摘要。

## 不能分发的东西

| 东西 | 原因 | 处理 |
|---|---|---|
| 开发者镜像（DDI、Cryptex 组件、BuildManifest） | 苹果的文件 | 运行时下载到 `ddi/` 缓存；镜像和仓库都不包含。下载来源默认与 pymobiledevice3 一致（社区维护的镜像仓库），也允许用户指向从 Xcode 提取的目录 |
| XCTest.framework 等苹果框架 | 苹果的文件 | 编译 WDA 时不嵌入（Appium 官方包也不带），由手机上的开发者镜像提供 |
| 证书、描述文件、配对记录、Apple ID 会话 | 用户秘密 | 只在用户的数据目录里，`.gitignore` 排除 |

## 其它合规提示

- 免费开发者账号（Xcode Free Provisioning）签出的描述文件只适合个人在自己设备上使用。
- 登录时 anisette 数据默认来自第三方服务（Impactor 的默认值）。文档要明确说明，并允许改成自建服务。
- 本项目不包含、也不帮助绕过任何 iOS 安全机制：重启后首次解锁、XCTest 自动化授权都需要用户本人在手机上完成。
