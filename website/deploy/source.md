# 从源码编译

一般用不着：一行命令、Homebrew 和 deb 包装的都是 Release 里编好的程序，见[快速开始](../guide/getting-started#install)。没有预编译包的机器（Intel Mac）和参与开发时才从源码编译。在 Mac 上见 [macOS](./macos)。

## 准备

- Rust 1.88 以上（[rustup](https://rustup.rs)）和 C 编译器
- Linux 上要系统的 usbmuxd（它由 udev 在插上手机时拉起）。守护进程不会自己启动 usbmuxd

## 编译安装

```bash
sudo apt install build-essential git usbmuxd     # 其他发行版用对应的包管理器
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked   # 装到 ~/.cargo/bin/iphone-control
```

## 运行

以普通用户运行，不要加 sudo。用数据线接上手机，然后：

```bash
iphone-control
```

第一次运行是设置引导，和安装包装的一样，见[快速开始](../guide/getting-started#first-run)。其中「后台服务」一步会装好 systemd 用户服务（`~/.config/systemd/user/iphone-control.service`，读取 `~/.config/iphone-control/env`），之后登录就自动启动守护进程。以后也可以单独装：

```bash
iphone-control service install
sudo loginctl enable-linger "$USER"   # 不登录也随开机启动
```

开发时也可以不装服务，在一个终端里前台运行守护进程，日志直接打印在这里：

```bash
iphone-control daemon
```

每个命令启动时都会读 `~/.config/iphone-control/env`，环境里已经有的变量优先，见[配置](../guide/configuration#settings-file)。

::: warning
- 守护进程、命令行和 MCP 要用同一个用户运行。
- 一部手机只接一份：不要同时运行两份 iphone-control（比如装了服务又前台运行一个），或其他基于 usbmuxd 的工具（go-ios、pymobiledevice3 等）。
:::

## 升级

升级前先看[更新日志](https://github.com/Derrors/iphone-control/blob/main/CHANGELOG.zh-CN.md)里的升级注意事项。

```bash
git pull && cargo install --path apps/iphone-control --locked
iphone-control service restart
```

## 卸载

```bash
iphone-control service uninstall    # 停止并移除后台服务
cargo uninstall iphone-control
```

数据目录 `~/.local/share/iphone-control` 和设置 `~/.config/iphone-control` 会保留，见[数据与备份](./backup)。
