# 配置

全部通过环境变量配置，都可以不填：

- **Docker**：写在仓库目录的 `.env`（从 `.env.example` 复制），改完 `docker compose up -d` 重建容器。
- **只写 compose 文件**：写在 `compose.yaml` 的 `environment:` 里（或同目录的 `.env` 加 `env_file`），改完同样 `docker compose up -d`。
- **从源码运行**：写在 `~/.config/iphone-control/env`（systemd 用户服务会读），改完 `systemctl --user restart iphone-control`。
- **macOS**：同样写在 `~/.config/iphone-control/env`，启动守护进程前加载，改完重启守护进程，见 [macOS](../deploy/macos#配置)。

## 常用

| 变量 | 默认 | 含义 |
|---|---|---|
| `HTTP_PROXY` / `HTTPS_PROXY` | 直连 | 访问苹果、anisette、开发者镜像、推送通知用的代理 |
| `IPHONE_UDID` | 第一台 | 接了多台手机时指定用哪台 |
| `RESIGN_MIN_DAYS` | `2` | 描述文件剩余不到几天时重签 |
| `RESIGN_CHECK_HOURS` | `6` | 每隔几小时检查一次 |
| `OFFLINE_ALERT_MIN` | `10` | 手机离线超过几分钟推送提醒，`0` 不提醒 |

通知相关的变量见[通知](./notifications)。

## 高级

| 变量 | 含义 |
|---|---|
| `ICTL_ANISETTE_URL` | 自建 anisette 服务地址（默认用公共的第三方服务，见[安全与隐私](./security)） |
| `ICTL_DDI_URL` / `ICTL_DDI_DIR` | 开发者镜像的下载地址 / 本地目录（从 Xcode 提取） |
| `ICTL_MANAGE_USBMUXD` | 宿主机已经运行 usbmuxd 服务时设为 `0`，守护进程就不自己拉起 |
| `SCREEN_W` / `SCREEN_H` | WDA 从没连上过时，HID 后备路线换算坐标用的屏幕尺寸（点） |
| `ICTL_LOG` | 日志级别，例如 `info,ictl_wda::http=debug` 记录每个 WDA 请求的耗时 |

## 只给 Docker Compose 用的

用仓库自带的 `compose.yaml` 时才有这几项；自己写 compose 文件时直接改对应的 `container_name`、`image`、`volumes`。

| 变量 | 默认 | 含义 |
|---|---|---|
| `CONTAINER_NAME` | `iphone-control` | 容器名 |
| `IMAGE` | `iphone-control:local` | 镜像名；用预构建镜像时改成 `ghcr.io/derrors/iphone-control:<版本>` |
| `DATA_DIR` | `./data` | 数据目录，见[数据与备份](../deploy/backup) |
