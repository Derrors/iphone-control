# 安全与隐私

iphone-control 本质上是一个手机遥控器：能访问它的人，就能在手机解锁时做机主能做的任何事。像对待手机本身一样对待运行它的主机。

这一页是要点。存了什么、谁能访问、每项风险的细节见完整的 [SECURITY.md](https://github.com/Derrors/iphone-control/blob/main/SECURITY.md)。

## 存了什么

- **数据目录**里有 Apple ID 会话、开发签名私钥、配对记录，开启自动解锁时还有**明文**的锁屏密码，要像密码一样保管和备份。Apple ID 密码本身不保存。
- **控制接口**是权限 0600 的 Unix socket，能连上它就能操控手机：主机 root 和运行守护进程的那个用户。
- **通知密钥**在设置文件 `~/.config/iphone-control/env` 里，权限 0600。

## 默认的保护

- **不开网络端口。** WDA 经 usbmuxd 直连手机。`wda-bridge` 和实时画面 `viewer` 都要显式开启，只绑 127.0.0.1，浏览器里的其他网页借不了它们操作手机。但它们没有登录，本机任何用户都能用（包括在 viewer 上发起 AI 任务），用完就停。
- **不用 root。** 守护进程以你的普通用户身份作为用户服务运行，安装脚本拒绝以 root 运行。
- **自动解锁默认关闭**，输错一次即停用。
- **开发者工具默认关闭**。
- 依赖锁定精确版本，下载的 WebDriverAgent 核对 sha256，安装脚本核对下载包的 sha256。

## 需要知道的风险

- **anisette 服务。** 登录 Apple ID 默认用公共的第三方服务（`mac-ani.jkcoxson.com`）。它拿不到你的密码，但能看到你什么时候登录。介意的话自建 [anisette-v3-server](https://github.com/Dadoum/anisette-v3-server)，设 `ICTL_ANISETTE_URL`。
- **用专门的 Apple ID。** 主机被入侵时，专门的账号能把影响限制住。
- **agent 用你的手在操作。** 它能发消息、付款、改设置。服务端会要求 agent 在这类操作前先问你，但这只是指示，不是强制。
- **开发者工具开着时风险更大。** `iphone_web_eval` 能在任意可检查的页面（含 Safari 标签页）执行 JavaScript，读 cookie 和 localStorage，以已登录身份发请求；`iphone_install_app` 接受运行守护进程的机器上的任意路径；系统日志和崩溃报告会进入 agent 的上下文，可能含隐私。开关 `IPHONE_CONTROL_DEV_TOOLS` 只决定 MCP 提不提供这些工具，不是权限边界。用完就关。

## 免责声明

本项目与 Apple Inc. 没有任何关联，也未获其认可或赞助。iPhone、iOS、Xcode 是 Apple Inc. 的商标。只在你自己拥有或获得授权的设备和账号上使用；自动操作第三方 App 可能违反其服务条款；苹果可能限制免费开发者账号。本软件按「原样」提供，不提供任何担保。

发现安全问题请[私下报告](https://github.com/Derrors/iphone-control/security/advisories/new)。
