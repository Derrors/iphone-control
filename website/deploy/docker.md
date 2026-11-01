# Docker 部署

推荐的部署方式。镜像里是一个 Rust 二进制加 Debian 的 usbmuxd，容器不用 `privileged`。

## 安装

### 自己构建

```bash
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cp .env.example .env
docker compose up -d --build
```

### 用预构建镜像

发布正式版本后，镜像在 `ghcr.io/derrors/iphone-control`（amd64、arm64）。在 `.env` 里设：

```ini
IMAGE=ghcr.io/derrors/iphone-control:latest   # 或固定版本，例如 :0.1.0
```

然后：

```bash
docker compose pull && docker compose up -d --no-build
```

装好后运行一次[安装向导](../guide/getting-started)。

## compose 做了什么

- 只开放 USB 字符设备（`device_cgroup_rules: "c 189:* rmw"`），`/dev/bus/usb` 绑定挂载，拔插后新的设备节点在容器里也看得到。
- `network_mode: host`：只为了能用宿主机上的代理；本身不监听任何端口。
- `./data` 挂到容器的 `/var/lib/iphone-control`，删掉重建容器也不丢配对和登录。
- `init: true`、`stop_grace_period: 30s`：停止时先关掉手机上的会话，避免留下「配对的电脑」图标。

## 升级

```bash
git pull
docker compose up -d --build                          # 自己构建
# 或
docker compose pull && docker compose up -d --no-build  # 预构建镜像
```

数据在 `./data`，升级不受影响。容器重启后，AI 客户端里的 MCP 连接会断开，重新连接即可（Claude Code 里运行 `/mcp`）。

::: tip 重启后先等就绪
重启后约 10 秒 WDA 就绪。短时间内连续重启多次，手机可能再次弹出「为 XCTest 输入 iPhone 密码」，输入锁屏密码即可。
:::

## 常用命令

```bash
docker exec iphone-control iphone-control status    # 状态
docker exec iphone-control iphone-control doctor    # 诊断
docker logs iphone-control                          # 日志
docker compose down                                 # 停止（数据保留）
```
