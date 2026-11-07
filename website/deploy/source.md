# 从源码 / 二进制运行

在 Linux 上不用 Docker，以普通用户直接运行。需要系统的 usbmuxd（它由 udev 在插上手机时拉起）。在 Mac 上见 [macOS](./macos)。

## 安装

### 预编译二进制

[Releases](https://github.com/Derrors/iphone-control/releases) 里有 Linux x86_64 和 aarch64 的压缩包（需要 glibc 2.34 以上：Debian 12、Ubuntu 22.04 及更新的发行版）：

```bash
sudo apt install usbmuxd
tar xzf iphone-control-v*-x86_64*.tar.gz   # ARM 主机把 x86_64 换成 aarch64（看 uname -m）
install -Dm755 iphone-control-*/iphone-control ~/.cargo/bin/iphone-control
```

放在 `~/.cargo/bin` 是为了和下面的 systemd 服务文件一致；放别处的话同时改服务文件里的 `ExecStart`。

### 从源码编译

需要 Rust 1.88 以上（[rustup](https://rustup.rs)）和 C 编译器：

```bash
sudo apt install build-essential git usbmuxd
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked   # 装到 ~/.cargo/bin/iphone-control
```

## 运行

```bash
iphone-control daemon       # 前台运行
iphone-control setup        # 另开一个终端运行安装向导
iphone-control mcp-config   # 打印注册 MCP 的命令
```

只有下面的 systemd 服务会读 `~/.config/iphone-control/env`。前台运行时先加载它：`set -a; . ~/.config/iphone-control/env; set +a`。

### 常驻：systemd 用户服务

服务文件在仓库的 `deploy/iphone-control.service`（二进制压缩包里也有）：

```bash
mkdir -p ~/.config/systemd/user ~/.config/iphone-control
cp deploy/iphone-control.service ~/.config/systemd/user/
cp .env.example ~/.config/iphone-control/env    # 按需改代理、通知（二进制压缩包里叫 env.example）
systemctl --user daemon-reload && systemctl --user enable --now iphone-control
loginctl enable-linger "$USER"                  # 不登录也随开机启动
```

## 和 Docker 部署的区别

| | Docker | 从源码（普通用户） |
|---|---|---|
| 数据目录 | `./data` | `~/.local/share/iphone-control` |
| 控制接口 | 容器内 `/run/iphone-control/ctl.sock` | `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` |
| usbmuxd | 守护进程自己拉起 | 系统服务 |
| 配对记录 | 数据目录的 `lockdown/` | 系统的 `/var/lib/lockdown` |
| 未签名的 WDA | 镜像自带 | 第一次签名前下载（核对 sha256） |
| 配置 | `.env` | `~/.config/iphone-control/env` |

::: warning
- 守护进程、命令行和 MCP 要用同一个用户运行。
- 不要和 Docker 版同时连同一部手机。装了系统 usbmuxd 又要用回 Docker 版时，先 `sudo systemctl mask usbmuxd`。
:::

## 升级

升级前先看[更新日志](https://github.com/Derrors/iphone-control/blob/main/CHANGELOG.zh-CN.md)里的升级注意事项。

```bash
git pull && cargo install --path apps/iphone-control --locked   # 或换上新版本的二进制
systemctl --user restart iphone-control
```
