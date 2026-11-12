# 终端菜单与 Shell 补全验证

2026-11-13，Linux x86_64，Rust 1.99.1。对应个人仓库 Issue #1、#4；未连接手机或启动设备守护进程。

## 实现范围

- 设置向导提供方向键/数字菜单、确认与取消、明确选中态、长列表与宽度适配。Esc 取消当前向导；已保存步骤保留，不提供任意步骤回退或全流程事务回滚。
- 文本与密码输入沿用原行为；非交互设置仍拒绝执行，基础终端/重定向菜单回退逐行输入。
- `completions bash|zsh|fish` 从实际 Clap 定义生成，不读设置、不获取维护锁、不连接 daemon。
- Issue #1 的更广泛体验建议（所有长任务进度、完整多面板 TUI、Kaku 外观对照）没有在本次实现中宣称完成。

## 已通过

- `cargo test --workspace --all-features --locked`。
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`。
- `cargo fmt` 对 CONTRIBUTING 指定的所有自有包执行 `--check`。
- `cargo build --release --locked -p iphone-control`。
- `python3 tests/terminal.py --binary <debug或release程序> --test-dir <target/debug/deps>`：方向键、编号与多位编号、确认左右键、Esc/Ctrl+C、termios 恢复、中文长列表、缩放、dumb/小窗口/重定向/管道回退及取消时保留已保存设置、丢弃未保存语言。已接入 Linux/macOS CI。
- 扩展 PTY 回归：默认“是”时输入“否/不”、非法编号 `1x`、无效输入后缩窗、跨提示粘贴、输入长度上限、EOF 截断；CSI/SS3 方向键及中文 UTF-8 以每字节 40ms 分批发送。键盘序列总等待上限 250ms，不保证超过该时限的分片仍被识别为同一按键。
- 补全集成测试：FIFO 设置文件不被读取、未连接模拟 daemon socket、未创建状态目录、帮助中英文、错误参数，以及 Bash/Zsh/Fish 语法。Fish 3.6 通过临时解包的软件包验证实际 `complete -C` 候选、前置全局参数和相似参数值，不做全局安装。
- `pnpm@12.16.1 install --frozen-lockfile` 与 `pnpm run docs:build`，双语站点构建通过。
- `python3 packaging/test_installer.py`：16 项通过。
- `git diff --check`。

## 验证边界

- macOS、真实 SSH/tmux/Kaku 环境仍需对应环境验证；PTY 回归已加入 Linux/macOS CI。
- 本机 Rust 1.88.0 多次下载未能完成，最终停止了本次下载；尝试 MSRV 检查返回 `missing manifest in toolchain`，未得到有效的编译结果。保留现有 CI 的 1.88.0 检查。
- 未改变打包安装逻辑，补全安装方式为文档中的本地生成；未在本机执行 .deb 安装或 Homebrew 安装。
- 新依赖 clap_complete 4.6.11 声明 MSRV 1.85，MIT OR Apache-2.0 符合现有许可证白名单；console 0.15.11 复用现有锁定版本；termion 4.0.6 使用 MIT 许可证。
