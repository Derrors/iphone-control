# 常见问题

先运行这两个命令，大多数问题都能看出来：

```bash
iphone-control status    # Docker 部署前面加 docker exec -it iphone-control
iphone-control doctor    # 逐层检查，每项失败都给出修复办法
```

## 状态说明

| `status` 显示 | 含义 | 怎么办 |
|---|---|---|
| 手机未连接 | usbmuxd 看不到手机 | 检查数据线、解锁手机；Linux 宿主机装了 usbmuxd 时 `sudo systemctl mask usbmuxd`。接着多台时，选定的那台没接也显示这个：看 `status` 的「选设备」行，或运行 `select auto`，见[多台设备](../guide/multi-device) |
| 未配对 | 没有配对记录 | 运行 `setup`，手机上点「信任」 |
| 开发者模式未打开 | — | 运行 `setup`，按提示在手机上打开 |
| 手机锁屏，隧道建不起来 | 重启后还没解锁过 | 手动解锁一次；解锁前状态一直是这个，守护进程会定期重试 |
| WDA 还没签名安装 | — | 运行 `sign` 或 `setup` |
| 正在启动 WDA | 最长约 2 分钟 | 等一下。iOS 27 起期间点击、滑动、截图走 HID 后备，仍可用，但横屏时要 30 秒内从 WDA 拿到过方向，否则拒绝触摸；屏幕尺寸未知时报错，见[工具一览](../guide/tools#wda-不可用时-hid-后备)。更早的 iOS 要等 WDA 起来 |
| WDA 卡在手机上的「Enable UI Automation」密码框 | 手机在等「为 XCTest 输入 iPhone 密码」 | 在手机上输入锁屏密码 |
| WDA 启动失败，稍后重试：… | 其他原因 | 看冒号后的错误，运行 `doctor` |
| 就绪 | 一切正常 | — |

## 手机连不上

- **两个 usbmuxd 在抢：** 很多发行版装了 usbmuxd，插上手机就被 udev 拉起，抢走 Docker 里的连接。`sudo systemctl mask usbmuxd`。
- **别的工具占着手机：** 同时运行 go-ios、pymobiledevice3 等会冲突。
- **数据线：** 有的线只能充电。`lsusb` 里看不到 Apple 设备（`05ac`）时换根线或换个口。
- **拔插后：** Docker 部署大约 1 分钟内自动恢复。
- **充电时大约每分钟断开一次：** 手机开着「设置 → 电池 → 充电 → 80% 上限」时，在 80% 附近反复开始、停止充电，每次 USB-C 重新协商供电都会让 USB 断开 3～5 秒。用来做自动化的手机把上限改成 100%。
- **Mac 上：** 不能用 Docker（没有 USB 直通），见 [macOS](../deploy/macos)。usbmuxd 是系统自带的，`doctor` 报连不上时运行 `sudo launchctl kickstart -k system/com.apple.usbmuxd`。

## WDA 起不来

- **「不受信任的开发者」：** 首次安装后在 设置 → 通用 → VPN 与设备管理 里信任你的 Apple ID（手机要联网）。
- **签名过期：** 免费签名 7 天有效，会自动重签。`status` 能看到还剩多久，`resign --force` 立即重签。
- **Apple ID 会话失效：** 会收到通知，重新运行 `login`。
- **开发者镜像失败：** `status` 第一行显示「WDA 启动失败，稍后重试：开发者镜像：…」，详情在 `logs/daemon.log`；`doctor` 没有单独的镜像检查。下载需要能访问 GitHub，配好 `HTTP_PROXY` / `HTTPS_PROXY`，或用 `ICTL_DDI_DIR` 指向从 Xcode 提取的目录，见[配置](../guide/configuration#高级)。

## 登录

- **输不了验证码：** `login`、`setup` 要在终端里交互运行，Docker 部署记得带 `-it`。
- **anisette 报错：** 默认的公共 anisette 服务可能暂时不可用，稍后重试，或自建后设 `ICTL_ANISETTE_URL`。

## AI 客户端

- **MCP 连接断开：** 容器重启会断开 MCP 进程，在客户端里重新连接（Claude Code 运行 `/mcp`）。
- **「守护进程正忙…已放弃执行」：** 一个操作排队超过 30 秒，为免点到已经变了的屏幕而放弃。截图看看再重试。
- **操作报错说 WDA 连接中途断开：** 请求已经发出，动作可能已经做了，为免点两次、输两遍不会重做。先截图确认。
- **点了没反应：** 可能有系统弹窗挡着，工具结果里会提示「屏幕上有系统弹窗…」，先点掉它。
- **输入的字不全：** `iphone_type_text` 会读回核对并补上；仍提示对不上时，说明 App 在输入过程中改写了输入框，分几段输入。

## 灵动岛一直显示「配对的电脑」、手机不自动锁屏

HID 后备路线使用时会开一个屏幕镜像会话，空闲 30 秒自动关闭。图标一直不消失时，重启一下守护进程。

## 报告问题

[提 issue](https://github.com/Derrors/iphone-control/issues/new/choose) 时请附上机型、iOS 版本、部署方式和 `doctor` 的输出，先去掉 UDID、Apple ID、通知密钥等隐私信息。
