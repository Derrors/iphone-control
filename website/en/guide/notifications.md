# Notifications

The daemon notifies you when the phone needs attention, through whichever channels are configured.

## When

| Event | What to do |
|---|---|
| Re-signing failed | Check the error in `status`; run `login` again if the session expired |
| Apple ID session expired (checked daily) | Run `iphone-control login` |
| The phone needs the passcode after a reboot (Enable UI Automation) | Enter the passcode on the phone |
| WDA recovered | — |
| Auto-unlock failed and was disabled | See [Auto-unlock](./auto-unlock) |
| Phone offline / back online | Check the cable and the host |

How repeats are limited:

- Re-signing failed, Apple ID session expired: not repeated for 20 hours; sent again after that if the problem is still there.
- Passcode needed, phone offline: sent once until it recovers. Only these two have a recovery notification ("WDA recovered", "back online").
- Auto-unlock disabled: sent once; it can only be sent again after `unlock-reset`.
- This record is kept in memory only. Restarting the daemon clears it, so a problem that isn't solved yet is sent again.

Notifications don't name the device; with several connected, use `status` to see which one. See [Several devices](./multi-device).

## Bark

```ini
BARK_SERVER=https://api.day.app   # or your own Bark server
BARK_KEY=your-key
```

## Telegram

```ini
TG_BOT_TOKEN=123456:ABC...
TG_CHAT_ID=your-chat-id
```

## Test

```bash
docker exec iphone-control iphone-control notify-test           # one per event type, titled "[测试]"
docker exec iphone-control iphone-control notify-test offline   # one type: resign / session / pin / pin-recovered / unlock / offline / online
```

::: info No notification if the container itself dies
Notifications come from the daemon. Watching the container or host needs separate monitoring, which this project doesn't provide.
:::
