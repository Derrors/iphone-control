# 第三方组件

本项目自己的代码以 MIT 发布。下列组件保留各自的许可证。详细分析见 [docs/licensing.md](docs/licensing.md)。

| 组件 | 版本 / 提交 | 许可证 | 引入方式 | 改动 |
|---|---|---|---|---|
| [idevice](https://github.com/jkcoxson/idevice) | 0.1.68 | MIT | crates.io | 无 |
| [jktcp](https://crates.io/crates/jktcp) | 0.1.8（间接） | MIT | crates.io | 无 |
| [rmcp](https://github.com/modelcontextprotocol/rust-sdk) | 3.8.0 | Apache-2.0（部分历史代码 MIT） | crates.io | 无 |
| [apple-codesign](https://github.com/indygreg/apple-platform-rs) | 0.29.0（取自 Impactor `3rdparty/apple-codesign-0.29.0`） | MPL-2.0 | `vendor/apple-codesign`，`[patch.crates-io]` | Impactor 的 0o755 权限补丁（上游提交 428b42f），本仓库未再改 |
| [Impactor](https://github.com/claration/impactor) `plume_core` | v2.7.2（提交 b13d99559febd63e3710dfdd0ef8cdd78d10ae13） | MPL-2.0 | `vendor/plume_core` | 源码未改；Cargo.toml 把 workspace 继承展开成显式版本、修正 omnisette 路径 |
| Impactor `plume_store` | 同上 | MIT | `vendor/plume_store` | 同上 |
| Impactor `omnisette` | 同上 | MPL-2.0 | `vendor/omnisette` | 同上 |
| Impactor `plume_utils` | 同上 | MIT | 未 vendor；`crates/ictl-sign/src/bundle.rs` 改写自其 `bundle.rs` | 增加 `BundleType::XCTest`，修复 `.xctest` 漏签 |
| idevice tools | v0.1.68 | MIT | `crates/ictl-device/src/media.rs`、`hid.rs` 的按键/键码表改写自 `tools/src/coredevice_stream.rs`、`hid.rs` | 去掉日志、改错误类型 |
| [WebDriverAgent](https://github.com/appium/WebDriverAgent) | v16.15.3（Appium 官方构建，sha256 2176793538d9…） | BSD-3-Clause | 镜像构建时下载校验，打成未签名 IPA；计划改为本项目 macOS CI 编译 | 不嵌入苹果框架 |
| [usbmuxd](https://github.com/libimobiledevice/usbmuxd) | Debian 包 | GPL-3.0 | 独立进程，不链接 | 无 |

**不随项目分发**：开发者镜像（DDI/Cryptex）、XCTest.framework 等苹果文件；证书、描述文件、配对记录、Apple ID 会话等用户数据。

完整的传递依赖许可证清单在阶段 7 用 `cargo about` 生成。
