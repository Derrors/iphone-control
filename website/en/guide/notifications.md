# Notifications

The daemon notifies you when the phone needs attention, through whichever channels are configured.

The notifications below about re-signing, the Apple ID, the signing certificate and WDA only come for devices on WDA; native control on iOS 27 and later has none of them.

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
| Daemon restarted after unexpected exit or watchdog timeout | Check `iphone-control status` or `logs/daemon.log`; if suppressed restarts are reported, check for a crash loop |

How repeats are limited:

- Re-signing failed, Apple ID session expired: not repeated for 20 hours; sent again after that if the problem is still there.
- Passcode needed, phone offline: sent once until it recovers. Only these two have a recovery notification ("WDA recovered", "back online").
- Auto-unlock disabled: sent once; it can only be sent again after `unlock-reset`.
- Signing certificate replaced: sent every time.
- Daemon unexpected exit / watchdog restart: not repeated for 30 minutes (persisted in `run-state.json`, so a crash loop cannot flood notifications; once cooldown expires, the next alert summarizes suppressed restarts and the 1-hour total). Clean stops, manual `service restart`, and updates never trigger it.
- The other records are kept in memory only. Restarting the daemon clears them, so a problem that isn't solved yet is sent again.

A notification's title names the device (its name from Settings › General › About on the phone, as in "My iPhone offline for 10 minutes"), so with several connected you can tell which one. See [Several devices](./multi-device).

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
iphone-control notify-test offline   # one type: resign / session / pin / pin-recovered / unlock / offline / online / cert / daemon
```

::: info No notification if the daemon isn't restarted or the host is down
Notifications come from the daemon. When the background service restarts it after a crash or watchdog exit, the new process sends a restart alert; if no service restarts it, or the host itself goes down, watching the host still needs separate monitoring, which this project doesn't provide.
:::
