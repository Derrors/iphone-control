# 通知

手机需要你处理时，守护进程会推送通知。配了哪个渠道就发哪个，都不配就不发。

下表里和重签、Apple ID、签名证书、WDA 有关的通知只出现在用 WDA 的设备上；iOS 27 及以上的原生控制不会有。

## 什么时候通知

| 事件 | 你要做的 |
|---|---|
| 重签失败 | 看 `status` 里的错误；Apple ID 会话失效时重新 `login` |
| Apple ID 会话失效（每天检查一次） | 运行 `iphone-control login` |
| 手机重启后要输密码（Enable UI Automation） | 在手机上输入锁屏密码 |
| WDA 已恢复 | — |
| 自动解锁失败已停用 | 见[自动解锁](./auto-unlock) |
| 手机离线 / 恢复在线 | 检查数据线和主机 |
| 签名证书换了（签名时新建了证书，可能还吊销了一张旧的） | 一般不用管：用旧证书签的设备，WDA 起不来时会自动重签；用同一个 Apple ID 的其他机器、其他侧载工具要在那边重签，见[常见问题](../troubleshoot/faq#cert) |
| 守护进程异常退出或卡死由看门狗重启 | 看 `iphone-control status` 或 `logs/daemon.log` 排查原因；若提示冷却期内合并了多次重启，说明在循环崩溃 |

不重复推送的规则：

- 重签失败、Apple ID 会话失效：20 小时内不重复，问题还在的话 20 小时后再推。
- 要输密码、手机离线：推一次，直到恢复。只有这两类有恢复推送（「WDA 已恢复」「已重新连上」）。
- 自动解锁停用：推一次，`unlock-reset` 后才可能再推。
- 签名证书换了：每次都推。
- 守护进程异常退出 / 看门狗重启：30 分钟内不重复推（记录持久化在 `run-state.json`，崩溃循环不会刷屏；冷却结束后再发生异常会汇总期间合并的次数与近 1 小时重启次数）。正常停止、手动 `service restart` 和升级不会触发。
- 其余记录只在内存里，守护进程重启后清空，还没解决的问题会再推一次。

通知标题带着设备名（手机「设置 › 通用 › 关于本机」里的名称，比如「我的 iPhone 已离线 10 分钟」），接着多台时一看就知道是哪台，见[多台设备](./multi-device)。

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
