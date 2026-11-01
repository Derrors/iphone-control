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

The same event isn't sent twice; recovery is sent once.

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
