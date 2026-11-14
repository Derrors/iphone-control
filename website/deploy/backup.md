# 数据与备份

## 数据目录里有什么

| 路径 | 内容 | 丢了会怎样 |
|---|---|---|
| `account/` | Apple ID 会话、开发签名私钥和证书 | 要重新登录；会生成新证书 |
| `wda/` | 签好的 WebDriverAgent | 自动重签 |
| `ddi/` | 开发者镜像缓存 | 自动重新下载 |
| `secrets/passcodes/` | 各设备的锁屏密码（仅开启自动解锁时） | 要重新 `passcode set` |
| `viewer-tokens.json` | 控制台的登录令牌（像密码一样保管，别放进会同步到别处的备份） | 控制台第一次启动时生成新的 `owner`；之前的令牌、登录过的浏览器和 MCP 客户端都要换新的 |
| `viewer-tls/` | 控制台局域网 HTTPS 用的自签证书和私钥（同样要保密） | 重新生成，证书指纹变了：浏览器会再提示一次，`--ca` 要换新证书 |
| `tasks/` | 最近 50 个 AI 任务的记录 | 控制台上看不到以前的任务，不影响使用 |
| `state.json` | 签好的 WDA 包信息（Team ID、Bundle ID、描述文件到期时间）、选定的设备、每台设备的屏幕尺寸、上次重签结果 | 守护进程按「WDA 还没签名」启动：要重新 `sign` 或 `setup`，选过设备的要重新 `select` |
| `logs/` | 日志、自动解锁的停用标记 `UNLOCK-DISABLED` | 日志没了不要紧。但停用标记没了就撤销了这层保护：之前输错过的话，可能再用同一个错误密码输一遍 |

位置：`~/.local/share/iphone-control`（权限 0700），Linux 和 macOS 一样，不管是一行命令、Homebrew、deb 还是从源码装的。配对记录不在这里，由系统的 usbmuxd 保存：Linux 上在 `/var/lib/lockdown`，macOS 上在 `/var/db/lockdown`（和 Finder 共用）。设置在 `~/.config/iphone-control/env`，里面有通知密钥，同样别外传。

::: danger 这些是秘密
数据目录能用来登录你的 Apple ID 开发者账号、免确认地连接手机，开启自动解锁时还有明文锁屏密码。不要提交到 git，不要发给别人，备份也要加密保存。
:::

## 备份

```bash
iphone-control service stop
tar czf iphone-control-data.tar.gz -C ~/.local/share iphone-control
cp ~/.config/iphone-control/env iphone-control-env      # 设置
sudo tar czf lockdown.tar.gz /var/lib/lockdown          # 配对记录，只在 Linux 上
iphone-control service start
```

守护进程是前台运行的，就先在它的终端按 Ctrl-C 停掉。macOS 上配对记录归系统管理，不用备份，换了 Mac 在手机上重新点一次「信任」即可。

## 迁移到新主机

先在新主机上安装（见[快速开始](../guide/getting-started#install)），不要急着运行 `iphone-control`。把数据目录、设置文件（Linux 上还有 `/var/lib/lockdown`）原样恢复到同一位置，再运行 `iphone-control`，不需要重新登录；恢复了配对记录的也不用重新配对。

## 从 Docker 部署迁过来

从下一个版本起不再提供 Docker 镜像。原来的数据在 compose 文件旁边的 `./data`（或 `DATA_DIR` 指的目录），属于 root。装好新版本后、第一次运行 `iphone-control` 之前：

```bash
docker compose down                                  # 在原来的 compose.yaml 所在目录
mkdir -p ~/.local/share
sudo cp -a ./data ~/.local/share/iphone-control
sudo chown -R "$(id -un)": ~/.local/share/iphone-control
```

`~/.local/share/iphone-control` 已经存在时（比如已经运行过 `iphone-control`），先 `iphone-control service stop`，把它改名留着，再复制。

- 原来 `.env` 里的设置搬到 `~/.config/iphone-control/env`（格式一样，`KEY=value`），权限设成 0600，见[配置](../guide/configuration#settings-file)。也可以直接运行 `iphone-control`，让引导重新问一遍。
- 容器里的配对记录在 `./data/lockdown/`，系统的 usbmuxd 不读这里。第一次运行时手机会再问一次「要信任此电脑吗？」，引导会带你点「信任」。
- `ICTL_MANAGE_USBMUXD`、`ICTL_USBMUXD_BIN` 已经去掉。Linux 上装系统的 usbmuxd（`sudo apt install usbmuxd`；deb 包会自动装），以前为 Docker 做过 `sudo systemctl mask usbmuxd` 的，先 `sudo systemctl unmask usbmuxd`。
