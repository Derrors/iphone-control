# 第三方组件

本项目自己的代码以 MIT 发布。下列组件保留各自的许可证。详细分析见 [docs/licensing.md](docs/licensing.md)。

| 组件 | 版本 / 提交 | 许可证 | 引入方式 | 改动 |
|---|---|---|---|---|
| [idevice](https://github.com/jkcoxson/idevice) | 0.1.68 | MIT | crates.io | 无 |
| [jktcp](https://crates.io/crates/jktcp) | 0.1.8（间接） | MIT | crates.io | 无 |
| [rmcp](https://github.com/modelcontextprotocol/rust-sdk) | 3.8.0 | Apache-2.0（部分历史代码 MIT） | crates.io | 无 |
| [apple-codesign](https://github.com/indygreg/apple-platform-rs) | 0.29.0 | MPL-2.0 | 阶段 4 起 vendor | 计划：Impactor 的 0o755 权限补丁 |
| [Impactor](https://github.com/claration/impactor) `plume_core` | v2.7.2（阶段 4 时锁定提交） | MPL-2.0 | 阶段 4 起 vendor | 计划：无，或按需最小改动并标注 |
| Impactor `plume_utils` | 同上 | MIT | 阶段 4 起 vendor | 计划：`BundleType::XCTest`，修复 `.xctest` 漏签 |
| Impactor `omnisette` | 同上 | MIT | 阶段 4 起 vendor | 计划：anisette 地址可配置 |
| [WebDriverAgent](https://github.com/appium/WebDriverAgent) | 阶段 7 时锁定 | BSD-3-Clause | macOS CI 编译未签名 IPA，作为 Release 附件 | 不嵌入苹果框架 |
| [usbmuxd](https://github.com/libimobiledevice/usbmuxd) | Debian 包 | GPL-3.0 | 独立进程，不链接 | 无 |

**不随项目分发**：开发者镜像（DDI/Cryptex）、XCTest.framework 等苹果文件；证书、描述文件、配对记录、Apple ID 会话等用户数据。

完整的传递依赖许可证清单在阶段 7 用 `cargo about` 生成。
