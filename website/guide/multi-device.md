# 多台设备

一台主机可以接多台 iPhone/iPad，但同一时间只控制一台：所有工具和命令都作用在它上面。只接一台时不用管这一页。

下面的命令在 Docker 部署时前面加 `docker exec -it iphone-control`。不要让两份 iphone-control 或其他基于 usbmuxd 的工具连同一台设备。

## 查看和切换

```bash
iphone-control devices                     # 列出接着的设备，标出正在控制哪台、有没有设自动解锁密码
iphone-control select <UDID 或设备名>       # 改为控制这一台
iphone-control select auto                 # 改回自动选
```

agent 用 `iphone_list_devices` 和 `iphone_select_device`，效果一样。

- 只能选 USB 上接着的设备。先比 UDID，再比设备名；设备名包含这段文字且只有一台时也行，对上多台就报错，要用 UDID 指定。
- 切换后 WDA 要 10~60 秒才起来，先用 `status`（或 `iphone_status`）确认就绪。没装过 WDA 的设备要先[安装](#给新设备安装)。
- `iphone-control viewer` 绑定启动时正在控制的设备：切换到别的设备后，它的操作会被拒绝，要重新运行才能看和操作新设备。

## 控制哪一台

| 优先级 | 来源 | 行为 |
|---|---|---|
| 1 | 环境变量 `IPHONE_UDID` | 固定这一台，`select` 切不走 |
| 2 | `select` 或 `iphone_select_device` 选定的 | 写进 `state.json`，重启后还记得 |
| 3 | 自动选（没选过，或 `select auto`） | 正在控制的那台一直用到它断开，断开后换接着的另一台 |

**选定的设备没接时**，状态是「手机未连接」，不会换到另一台，免得操作和自动解锁落到别的设备上。离线超过 `OFFLINE_ALERT_MIN` 分钟还会推送离线通知。`status` 的「选设备」行能看到选的是哪台，`select auto` 恢复自动选。

**自动选时**，如果 `iphone_batch` 执行到一半控制的设备变了（比如拔线后换到另一台），批处理会停下报错。

## 给新设备安装

```bash
iphone-control select <新设备>
iphone-control setup
```

`setup` 只作用于正在控制的那台，所以要先 `select`。它会把设备注册到你的 Apple ID，占用账号的设备名额。自动重签不会替没装过 WDA 的设备注册，这时 `doctor` 会提示运行 `setup`。

## 重签

自动重签只管正在控制的那台。闲置设备的描述文件 7 天后过期，切回去后 1 分钟内会自动检查，快过期或已过期就重签；也可以运行 `resign --force`。

## 自动解锁

密码按设备分开存：

```bash
iphone-control passcode set --device <UDID 或设备名>     # 设备要接着
```

停用标记是全局的：一台自动解锁失败，所有设备都停用，直到运行 `unlock-reset`。见[自动解锁](./auto-unlock)。

## 通知

通知文字不带设备名。收到通知时用 `status` 或 `devices` 看是哪台。

## iPad 的差异

- **Touch ID 机型：** 锁屏上滑后是「触控 ID」提示，要先点「使用密码」才有键盘，自动解锁时守护进程会自己点。
- **横屏：** 坐标按当前方向算，自动解锁和 HID 后备路线会换算回设备的原始方向。HID 后备路线在横屏时的限制见[工具一览](./tools#wda-不可用时-hid-后备)。
- **iOS 26 没有 HID 后备：** WDA 重启期间操作会失败，iPhone 上也一样。
- **屏幕尺寸不是 430×932：** iPad mini 5 竖放是 768×1024，以截图和 `iphone_status` 为准。

已验证：iPad mini 5，iOS 26.6.1（Linux，Docker）。
