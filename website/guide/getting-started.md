# 快速开始

在 Linux 上用 Docker 部署，大约 15 分钟。不用 Docker 见[从源码 / 二进制](../deploy/source)；用 Mac 的话见 [macOS](../deploy/macos)，安装向导和接入客户端的步骤与这里相同。

## 准备

- 一台 Linux 主机，装好 Docker（含 Compose v2）
- iPhone（iOS 17+）和能传数据的线
- 一个免费 Apple ID（建议专门注册一个）
- 主机能访问苹果服务器和 GitHub；国内网络一般要配代理

::: warning 一部手机只接一份
不要同时运行两份 iphone-control，或其他基于 usbmuxd 的工具（go-ios、pymobiledevice3 等）连同一部手机。Linux 宿主机装了 usbmuxd 的，先 `sudo systemctl mask usbmuxd`。
:::

## 1. 启动

```bash
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cp .env.example .env        # 需要代理的话在这里填 HTTP_PROXY / HTTPS_PROXY
docker compose up -d --build
```

首次构建要编译 Rust，需要几分钟。发布了正式版本后也可以直接用预构建镜像，见 [Docker 部署](../deploy/docker)。

## 2. 运行安装向导

用数据线接上手机并解锁，然后：

```bash
docker exec -it iphone-control iphone-control setup
```

向导分 6 步，每一步需要你在手机上做的事：

| 步骤 | 手机上要做的 |
|---|---|
| 1. 连接 | 解锁手机 |
| 2. 配对 | 弹出「要信任此电脑吗？」时点「信任」，输入锁屏密码 |
| 3. 开发者模式 | 设置 → 隐私与安全性 → 开发者模式 → 打开 → 重新启动；重启解锁后在弹窗里点「打开」 |
| 4. 登录 Apple ID | （在终端里）输入 Apple ID、密码和双重认证验证码 |
| 5. 签名安装 WDA | 保持手机解锁 |
| 6. 启动 WDA | 设置 → 通用 → VPN 与设备管理 → 信任你的 Apple ID；如果弹出「为 XCTest 输入 iPhone 密码」，输入锁屏密码 |

向导可以重复运行：已经完成的配对、开发者模式、登录会跳过，签名会重新做一遍。看到「WDA 就绪」就好了。

::: tip 密码只在这里输入
Apple ID 密码和验证码只在这个终端里输入，不会保存到磁盘；保存的只是登录后的会话。
:::

## 3. 接入 AI 客户端

```bash
docker exec -it iphone-control iphone-control mcp-config
```

它会打印注册命令，Claude Code 直接执行即可：

```bash
claude mcp add iphone -s user -- docker exec -i iphone-control iphone-control mcp
```

其他客户端见[接入 AI 客户端](./mcp-clients)。

## 4. 试一下

在 Claude Code 里说：

> 用 iPhone 打开设置，看看 iOS 版本是多少。

agent 会调用 `iphone_launch_app`、`iphone_tap_element`、`iphone_screenshot` 等工具完成。

## 检查状态

```bash
docker exec iphone-control iphone-control status   # 当前状态、描述文件还剩多久、最近日志
docker exec iphone-control iphone-control doctor   # 逐层诊断，每项失败都给出修复办法
```

遇到问题看[常见问题](../troubleshoot/faq)。
