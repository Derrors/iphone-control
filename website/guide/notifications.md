# 通知

手机需要你处理时，守护进程会推送通知。配了哪个渠道就发哪个，都不配就不发。

下表里和重签、Apple ID、签名证书、WDA 有关的通知只出现在用 WDA 的设备上；iOS 27 及以上的原生控制不会有。

## 什么时候通知

| 事件 | 适用路线 | 去重 / 冷却 | 恢复通知 | 建议处理 |
|---|---|---|---|---|
| **重签失败** (`resign`) | 仅 WDA | 20 小时内不重复 | — | 看 `status` 错误；会话失效时重跑 `login` |
| **Apple ID 会话失效** (`session`，每天检查一次；网络/Anisette 瞬断会自动重试并在 1 小时后复查，不误报) | 仅 WDA | 20 小时内不重复 | — | 运行 `iphone-control login` 重新登录 |
| **重启后要输 UI 自动化密码** (`pin`) | 仅 WDA | 推一次直至恢复 | ✅ WDA 已恢复 (`pin-recovered`) | 在手机上输入锁屏密码 |
| **签名证书换了** (`cert`，新建或吊销了旧证书) | 仅 WDA | 每次都推 | — | 本机受影响设备会自动重签；其他机器或侧载工具需在那边重签（见[常见问题](../troubleshoot/faq#cert)） |
| **自动解锁失败已停用** (`unlock`) | 全部 | 推一次直至 `unlock-reset` | — | 先人工解锁并核对密码，见[自动解锁](./auto-unlock) |
| **手机离线超阈值** (`offline`，默认 10 分钟) | 全部 | 推一次直至恢复 | ✅ 恢复在线 (`online`) | 检查数据线、供电上限和主机 |
| **守护进程异常退出 / 看门狗重启** (`daemon`) | 全部 | **30 分钟冷却**（持久化在 `run-state.json`，冷却后汇总合并次数与近 1 小时重启数） | — | 看 `status` 或 `logs/daemon.log` 排查原因；正常停止、手动 `service restart` 与升级不会触发 |

除 `daemon` 的冷却状态持久化在 `run-state.json` 外，其余去重记录仅存内存（守护进程重启后清空，未解决的问题会再推一次）。设备相关通知的标题会带上手机名称（手机「设置 › 通用 › 关于本机」中的名称，如「我的 iPhone 已离线 10 分钟」），多台设备时一眼可辨，见[多台设备](./multi-device)。

## 设置

最简单的是在引导里设：首次运行 `iphone-control` 时会问，以后运行 `iphone-control setup --reconfigure` 选「通知」。选 Bark 或 Telegram，填好后会发一条测试通知，并重启守护进程让设置生效。

也可以手动写进[设置文件](./configuration#settings-file) `~/.config/iphone-control/env`，改完运行 `iphone-control service restart`。

## Bark

```ini
BARK_SERVER=https://api.day.app   # 自建 Bark 服务时改成自己的地址
BARK_KEY=你的key
```

## Telegram

```ini
TG_BOT_TOKEN=123456:ABC...
TG_CHAT_ID=你的chat id
```

## 测试

```bash
iphone-control notify-test            # 每类事件各发一条，标题带「[测试]」
iphone-control notify-test offline    # 只发一类：resign / session / pin / pin-recovered / unlock / offline / online / cert / daemon
```

::: info 没被拉起或主机宕机时发不了通知
通知由守护进程发出。守护进程异常退出或看门狗触发重启后，后台服务自动拉起的新进程会发重启告警；但如果服务没装/没拉起，或主机直接宕机，仍需要宿主机侧的外部监控，本项目没有提供。
:::
