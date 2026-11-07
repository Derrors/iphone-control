# 通知

手机需要你处理时，守护进程会推送通知。配了哪个渠道就发哪个，都不配就不发。

## 什么时候通知

| 事件 | 你要做的 |
|---|---|
| 重签失败 | 看 `status` 里的错误；Apple ID 会话失效时重新 `login` |
| Apple ID 会话失效（每天检查一次） | 运行 `iphone-control login` |
| 手机重启后要输密码（Enable UI Automation） | 在手机上输入锁屏密码 |
| WDA 已恢复 | — |
| 自动解锁失败已停用 | 见[自动解锁](./auto-unlock) |
| 手机离线 / 恢复在线 | 检查数据线和主机 |

不重复推送的规则：

- 重签失败、Apple ID 会话失效：20 小时内不重复，问题还在的话 20 小时后再推。
- 要输密码、手机离线：推一次，直到恢复。只有这两类有恢复推送（「WDA 已恢复」「已重新连上」）。
- 自动解锁停用：推一次，`unlock-reset` 后才可能再推。
- 这些记录只在内存里，守护进程重启后清空，还没解决的问题会再推一次。

通知文字不带设备名，接着多台时用 `status` 看是哪台，见[多台设备](./multi-device)。

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
docker exec iphone-control iphone-control notify-test            # 每类事件各发一条，标题带「[测试]」
docker exec iphone-control iphone-control notify-test offline    # 只发一类：resign / session / pin / pin-recovered / unlock / offline / online
```

::: info 容器整个挂掉时发不了通知
通知由守护进程发出。容器或主机宕机时需要宿主机侧的监控，本项目没有提供。
:::
