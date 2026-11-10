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
| Signing certificate replaced (signing created a new certificate, maybe revoking an old one) | Usually nothing: devices signed with the old certificate are re-signed automatically when their WDA won't start; other machines or sideloading tools using the same Apple ID have to re-sign there, see the [FAQ](../troubleshoot/faq#cert) |

How repeats are limited:

- Re-signing failed, Apple ID session expired: not repeated for 20 hours; sent again after that if the problem is still there.
- Passcode needed, phone offline: sent once until it recovers. Only these two have a recovery notification ("WDA recovered", "back online").
- Auto-unlock disabled: sent once; it can only be sent again after `unlock-reset`.
- Signing certificate replaced: sent every time.
- This record is kept in memory only. Restarting the daemon clears it, so a problem that isn't solved yet is sent again.

Notifications don't name the device; with several connected, use `status` to see which one. See [Several devices](./multi-device).

## Setting up

The easiest way is the guide: the first run of `iphone-control` asks, and later `iphone-control setup --reconfigure` with Notifications. Pick Bark or Telegram and enter the key or token; it sends a test message and restarts the daemon so the settings apply.

You can also put them in the [settings file](./configuration#settings-file), `~/.config/iphone-control/env`, and run `iphone-control service restart`.

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
iphone-control notify-test           # one per event type, titled "[test]" (or "[测试]" in Chinese)
iphone-control notify-test offline   # one type: resign / session / pin / pin-recovered / unlock / offline / online / cert
```

::: info No notification if the daemon itself dies
Notifications come from the daemon. The background service restarts it after a crash; watching the host itself needs separate monitoring, which this project doesn't provide.
:::
