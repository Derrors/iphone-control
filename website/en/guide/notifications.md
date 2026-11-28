# Notifications

The daemon notifies you when the phone needs attention, through whichever channels are configured.

The notifications below about re-signing, the Apple ID, the signing certificate and WDA only come for devices on WDA; native control on iOS 27 and later has none of them.

## When

| Event | Applies to | Dedup / cooldown | Recovery alert | What to do |
|---|---|---|---|---|
| **Re-signing failed** (`resign`) | WDA only | Once per 20 h | — | Check the error in `status`; run `login` if the session expired |
| **Apple ID session expired** (`session`, checked daily; transient network/Anisette glitches retry automatically and recheck in 1 h without alerting) | WDA only | Once per 20 h | — | Run `iphone-control login` |
| **Passcode needed after reboot** (`pin`, Enable UI Automation) | WDA only | Once until recovered | ✅ WDA recovered (`pin-recovered`) | Enter the passcode on the phone |
| **Signing certificate replaced** (`cert`, new cert created or old one revoked) | WDA only | Every time | — | Local devices auto re-sign when WDA refuses to start; re-sign on other machines or sideloading tools ([FAQ](../troubleshoot/faq#cert)) |
| **Auto-unlock disabled** (`unlock`) | All | Once per device (it isn't tried there again until `unlock-reset`) | — | Unlock manually and verify the passcode ([Auto-unlock](./auto-unlock)) |
| **Phone offline** (`offline`, default 10 min) | All | Once until back online | ✅ Back online (`online`) | Check cable, charging limit and host |
| **Daemon unexpected exit / watchdog restart** (`daemon`) | All | **30 min cooldown** (persisted in `run-state.json`; summarizes suppressed restarts and 1 h total after cooldown) | — | Inspect `status` or `logs/daemon.log`; clean stops, manual `service restart` and updates never trigger it |

Except for `daemon` (persisted in `run-state.json`), dedup records live in memory and reset when the daemon restarts. Device-scoped alert titles include the device's name from *Settings › General › About* (e.g. "My iPhone offline for 10 minutes"); see [Several devices](./multi-device).

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
