# 自动解锁

默认关闭。开启后，手机锁屏时，MCP 操作前会先用你保存的锁屏密码解锁手机。

::: danger 先想清楚风险
密码以**明文**存在数据目录里。能登录这台主机、能读数据目录备份、或能调用这个 MCP 的人，都能解锁你的手机。只在你完全掌控的主机上开启，最好用一部专门用来自动化的手机。
:::

## 开启

```bash
docker exec -it iphone-control iphone-control passcode set
```

只支持 4 位以上的纯数字密码。关闭：

```bash
docker exec -it iphone-control iphone-control passcode clear
```

## 保护规则

- 每按一位都核对密码栏的位数，对不上立即停止（没输完不算输错）；只有第一位没生效时会重来，最多 3 轮。
- 整串输完手机仍然锁着，就**立即停用**自动解锁并推送通知，不会第二次再试，因此不会累计输错触发抹掉数据。
- 手机重启后第一次解锁（BFU）必须人工完成，自动解锁不处理。

## 停用后恢复

如果因为输错被停用（例如你改了锁屏密码），先更新密码，再恢复：

```bash
docker exec -it iphone-control iphone-control passcode set
docker exec -it iphone-control iphone-control unlock-reset
```

::: tip 不想用自动解锁
可以在手机上把「设置 → 显示与亮度 → 自动锁定」设为「永不」，手机就不会自己锁屏。
:::
