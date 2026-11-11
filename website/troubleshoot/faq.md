# 常见问题

先运行这两个命令，大多数问题都能看出来：

```bash
iphone-control status    # 守护进程和设备的状态、最近日志
iphone-control doctor    # 逐层检查，每项失败都给出修复办法
```

## 状态说明

| `status` 显示 | 含义 | 怎么办 |
|---|---|---|
| 手机未连接 | usbmuxd 看不到手机 | 检查数据线、解锁手机；Linux 上确认装了系统的 usbmuxd（`sudo apt install usbmuxd`）。接着多台时，选定的那台没接也显示这个：看 `status` 的「选设备」行，或运行 `select auto`，见[多台设备](../guide/multi-device) |
| 未配对 | 没有配对记录 | 运行 `setup --reconfigure` 选「手机」，手机上点「信任」 |
| 开发者模式未打开 | — | 在手机的 设置 → 隐私与安全性 → 开发者模式 打开。手机被开发者工具连接过才会出现这个开关，运行 `setup --reconfigure` 选「手机」会处理 |
| 手机锁屏，隧道建不起来 | 重启后还没解锁过 | 手动解锁一次；解锁前状态一直是这个，守护进程会定期重试 |
| WDA 还没签名安装 | — | 运行 `sign` 或 `setup` |
| 正在启动 WDA | 最长约 2 分钟 | 等一下。iOS 27 起期间点击、滑动、截图走 HID 后备，仍可用，但横屏时要 30 秒内从 WDA 拿到过方向，否则拒绝触摸；屏幕尺寸未知时报错，见[工具一览](../guide/tools#wda-不可用时-hid-后备)。更早的 iOS 要等 WDA 起来 |
| WDA 卡在手机上的「Enable UI Automation」密码框 | 手机在等「为 XCTest 输入 iPhone 密码」 | 在手机上输入锁屏密码。手机重启后会出现，连续重启守护进程几次后有时也会。守护进程每 60 秒重试，只通知一次 |
| WDA 启动失败，稍后重试：… | 其他原因 | 看冒号后的错误，运行 `doctor`。重试会退避，最长间隔 5 分钟 |
| 就绪 | 一切正常 | — |

## 手机连不上

- **Linux 上没有 usbmuxd：** 守护进程用系统的 usbmuxd，不会自己启动。`sudo apt install usbmuxd`（deb 包会自动装）。以前为 Docker 部署做过 `sudo systemctl mask usbmuxd` 的，先 `sudo systemctl unmask usbmuxd`。
- **守护进程没在运行：** `iphone-control service status` 看看，`iphone-control service start` 启动。直接运行 `iphone-control` 也会在服务没运行时启动它。
- **别的工具占着手机：** 同时运行 go-ios、pymobiledevice3 等会冲突。
- **数据线：** 有的线只能充电。`lsusb` 里看不到 Apple 设备（`05ac`）时换根线或换个口。
- **拔插后：** 大约 1 分钟内自动恢复。
- **充电时大约每分钟断开一次：** 手机开着「设置 → 电池 → 充电 → 80% 上限」时，在 80% 附近反复开始、停止充电，每次 USB-C 重新协商供电都会让 USB 断开 3～5 秒。用来做自动化的手机把上限改成 100%。
- **Mac 上：** usbmuxd 是系统自带的，`doctor` 报连不上时运行 `sudo launchctl kickstart -k system/com.apple.usbmuxd`。见 [macOS](../deploy/macos)。

## WDA 起不来

- **「不受信任的开发者」：** 首次安装后在 设置 → 通用 → VPN 与设备管理 里信任你的 Apple ID（手机要联网）。
- <a id="cert"></a>**签名证书被吊销了：**`status` 报 `unexpected response`，描述文件却还有效；在设备上点 WebDriverAgentRunner 图标提示「无法验证 App」。
  - 原因：每个安装（每台机器、每个数据目录）有自己的签名私钥和证书，免费 Apple ID 同时最多两张证书。在新地方第一次签名（换机器、从 Docker 换成本机安装、删了数据目录），或在 AltStore、SideStore 等侧载工具里用同一个 Apple ID 签名，都会新建证书；满了就吊销一张旧的，用它签的 App 在每台设备上都起不来。
  - 守护进程会自己处理：设备拒绝启动 WDA 时，如果设备上的 WDA 不是本机证书签的，或本机的证书已不在 Apple ID 的团队里，就自动重签这台（同一台 6 小时内最多一次），并发通知。`doctor` 会显示这两项检查。
  - 手动处理：`iphone-control sign` 重签正在控制的设备；控制台里 WDA 启动失败时也有「重新签名并安装 WDA」按钮。
  - 别让两处同时用同一个 Apple ID 签名：不用的旧部署停掉，或给它们用不同的 Apple ID。
- **签名过期：** 免费签名 7 天有效，会自动重签。`status` 能看到还剩多久，`resign --force` 立即重签。
- **Apple ID 会话失效：** 会收到通知。运行 `iphone-control setup --reconfigure` 选「Apple ID」，重新登录并重签 WDA。
- **开发者镜像失败：** `status` 第一行显示「WDA 启动失败，稍后重试：开发者镜像：…」，详情在 `logs/daemon.log`；`doctor` 没有单独的镜像检查。下载需要能访问 GitHub，配好代理（`iphone-control setup --reconfigure` 选「代理」），或用 `ICTL_DDI_DIR` 指向从 Xcode 提取的目录，见[配置](../guide/configuration#高级)。

## 登录

- **输不了验证码：** `login`、`setup` 和第一次运行的 `iphone-control` 都要在终端里交互运行，不能放进脚本或管道。
- **anisette 报错：** 默认的公共 anisette 服务可能暂时不可用，稍后重试，或自建后设 `ICTL_ANISETTE_URL`。

## AI 客户端

- **MCP 工具不见了：** 运行 `iphone-control setup --reconfigure` 选「注册 MCP」，或运行 `iphone-control mcp-config` 打印注册命令，然后开一个新会话。
- **「守护进程正忙…已放弃执行」：** 一个操作排队超过 30 秒，为免点到已经变了的屏幕而放弃。截图看看再重试。
- **操作报错说 WDA 连接中途断开：** 请求已经发出，动作可能已经做了，为免点两次、输两遍不会重做。先截图确认。
- **点了没反应：** 可能有系统弹窗挡着，工具结果里会提示「屏幕上有系统弹窗…」，先点掉它。
- **输入的字不全：** `iphone_type_text` 会读回核对并补上；仍提示对不上时，说明 App 在输入过程中改写了输入框，分几段输入。

## 控制台切换设备后没有画面

画面区域会写明原因：没连上、还要配对或打开开发者模式（运行 `iphone-control setup --reconfigure` 选「手机」）、WDA 正在启动（通常 10–60 秒）、要先解锁。新设备上还没装 WDA 时，已登录 Apple ID 的可以点「签名并安装 WDA」，大约 1–2 分钟，期间保持手机解锁；没登录的运行 `iphone-control setup --reconfigure` 选「Apple ID」。AI 任务执行中，或设了 `IPHONE_UDID` 时，不能切换设备。见[多台设备](../guide/multi-device)。

## 控制台画面卡、流量大

看画面下方写的编码方式。是 MJPEG 的话，装上 ffmpeg 再重启 `iphone-control viewer`，画面改用 H.264，流量约为原来的 1/15；启动时终端里会写用的是哪个编码器，`doctor` 会检查 ffmpeg 和显卡能不能用。已经是 H.264、链路又很慢时，画面帧率会下降，但不会越来越滞后；还嫌慢，可以把 `IPHONE_CONTROL_VIEWER_QP` 调大一些（比如 `24`）。见[控制台的画面](../guide/cli#viewer-picture)。

## 灵动岛一直显示「配对的电脑」、手机不自动锁屏

HID 后备路线使用时会开一个屏幕镜像会话，空闲 30 秒自动关闭。图标一直不消失时，重启一下守护进程（`iphone-control service restart`）。

## 报告问题

[提 issue](https://github.com/Derrors/iphone-control/issues/new/choose) 时请附上机型、iOS 版本、安装方式（一行命令、Homebrew、deb 或源码）和 `doctor` 的输出，先去掉 UDID、Apple ID、通知密钥等隐私信息。
