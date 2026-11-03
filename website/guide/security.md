# 安全与隐私

iphone-control 本质上是一个手机遥控器：能访问它的人，就能在手机解锁时做机主能做的任何事。像对待手机本身一样对待运行它的主机。

## 存了什么、谁能访问

| 内容 | 位置 | 谁能访问 |
|---|---|---|
| 操控手机 | 控制接口（Unix socket，权限 0600） | 主机 root、能 `docker exec` 进容器的人、从源码运行时的同一用户 |
| Apple ID 会话、开发签名私钥 | 数据目录 `account/` | 能读数据目录的人。Apple ID 密码本身不保存 |
| 配对记录 | 数据目录 `lockdown/`（容器） | 能读数据目录的人；拿到它不用再点「信任」就能经 USB 访问手机 |
| 锁屏密码（仅开启自动解锁时） | 数据目录 `secrets/passcode`，**明文** | 能读数据目录的人 |
| 通知密钥 | `.env` 或 `~/.config/iphone-control/env` | 能读这个文件的人 |

## 默认的保护

- **不开网络端口。** WDA 经 usbmuxd 直连手机。`wda-bridge` 是显式开启的桥接，只绑 127.0.0.1，但 WDA 没有认证，本机任何用户都能连上，用完就停。
- **容器不用 privileged**，只开放 USB 字符设备。
- **自动解锁默认关闭**，输错一次即停用。
- 依赖锁定精确版本，基础镜像锁 digest，下载的 WebDriverAgent 核对 sha256。

## 需要知道的风险

- **容器以 root 运行。** 要读写 USB 设备节点，拔插后还要能重启 usbmuxd（容器里没有 udev）。
- **anisette 服务。** 登录 Apple ID 需要 anisette 数据，默认来自公共的第三方服务（`mac-ani.jkcoxson.com`）。它拿不到你的密码，但你的登录依赖它生成的设备身份，它也能看到你什么时候登录。介意的话自建 [anisette-v3-server](https://github.com/Dadoum/anisette-v3-server)，设 `ICTL_ANISETTE_URL`。
- **用专门的 Apple ID。** 磁盘上的会话和签名私钥足以管理这个账号的开发证书和设备；主机被入侵时，专门的账号能把影响限制住。
- **agent 用你的手在操作。** 它能发消息、付款、改设置。服务端会要求 agent 在这类操作前先问你，但这只是指示，不是强制。

## 免责声明

本项目与 Apple Inc. 没有任何关联，也未获其认可或赞助。iPhone、iOS、Xcode 是 Apple Inc. 的商标。只在你自己拥有或获得授权的设备和账号上使用；自动操作第三方 App 可能违反其服务条款；苹果可能限制免费开发者账号。本软件按「原样」提供，不提供任何担保。

发现安全问题请[私下报告](https://github.com/Derrors/iphone-control/security/advisories/new)，完整说明见 [SECURITY.md](https://github.com/Derrors/iphone-control/blob/main/SECURITY.md)。
