# Troubleshooting

Start with these two commands:

```bash
iphone-control status    # current state, signing info, last log lines
iphone-control doctor    # checks each layer in order and prints a fix for every failure
```

Messages are in Simplified Chinese by default, and in English with `IPHONE_CONTROL_LANG=en`. The default Chinese wording is quoted below so you can match it.

## States

`status` shows the state on its first line. What each state means and what to do about it: [FAQ](https://derrors.github.io/iphone-control/en/troubleshoot/faq#states) (English) · [常见问题](https://derrors.github.io/iphone-control/troubleshoot/faq#状态说明) (中文).

## Phone not detected

- **usbmuxd missing (Linux):** the daemon talks to the phone through the system usbmuxd and doesn't start one itself. Install it with `sudo apt install usbmuxd` (the .deb depends on it); udev starts it when the phone is plugged in. Don't mask it.
- **Another tool holds the phone:** Xcode-less tools such as go-ios or pymobiledevice3 running at the same time can conflict. Stop them.
- **Cable:** some cables only charge. If `lsusb` doesn't show an Apple device (vendor `05ac`), try another cable or port.
- **Drops every minute or so while charging:** with *Settings → Battery → Charging → 80% Limit* on, the phone keeps starting and stopping charging near 80%, and each switch renegotiates USB-C power, which briefly disconnects USB (3–5 s). The daemon reconnects by itself, but in-flight actions fail. Set the limit to 100% while the phone is used for automation.

## Native control won't get ready

iOS 27 and later are controlled natively by default, which needs only pairing, Developer Mode, the developer disk image and the accessibility service. When `status` keeps saying 「准备手机失败，稍后重试」:

- **The disk image doesn't download:** it comes from GitHub; set a proxy if needed (`iphone-control setup --reconfigure`, pick *Proxy*).
- **The phone is locked:** keep it unlocked while it gets ready.
- **Still stuck:** run `doctor` to see which layer fails and [open an issue](https://github.com/Derrors/iphone-control/issues/new/choose). Meanwhile you can go back to WDA with `ICTL_IOS_BACKEND=wda` in `~/.config/iphone-control/env` and `iphone-control service restart` (needs an Apple ID; see the next section).

## WebDriverAgent won't start

- **"Untrusted developer":** after the first install, trust your Apple ID under *Settings → General → VPN & Device Management* (needs internet on the phone).
- **Revoked signing certificate:** the launch fails with only `unexpected response` while the profile still looks valid. Each account directory has its own private key (`account/keys/<team>/key.pem`) and certificate; a free team holds two, and plume revokes the first one it can when a new one is needed, whoever it belongs to. Every app signed with the revoked certificate stops launching on every device. The daemon checks two things when the launch is refused: whether the device's WDA profile lists a certificate with this machine's public key (offline, `foreign_wda`), and whether this machine's certificate is still in the team (online, cached as `own_cert` by the daily session check and before an automatic re-sign). Either failing triggers an automatic re-sign of that device, at most once per device every `CERT_HEAL_EVERY` (6 h). Signing that creates or revokes a certificate is logged and notified (`cert`). `doctor` shows both checks.
- **Expired signature:** free provisioning profiles last 7 days. The daemon re-signs when fewer than `RESIGN_MIN_DAYS` (default 2) remain. `status` shows the remaining time; `resign --force` re-signs now. Automatic re-signing only covers the controlled device, and is checked within a minute after it reconnects or you switch to it. A re-sign that takes too long counts as failed and you get a notification.
- **Apple ID session expired:** you get a notification, and `resign` fails with a login error. Run `login` again.
- **Developer Disk Image:** a failure shows on the first line of `status` as `WDA 启动失败，稍后重试：开发者镜像：…`; `doctor` has no separate check for it, and the details are in `logs/daemon.log`. Downloading needs access to GitHub; behind a firewall set `HTTP_PROXY`/`HTTPS_PROXY`, or point `ICTL_DDI_DIR` at an image extracted from Xcode. With `ICTL_DDI_DIR` the daemon neither downloads nor deletes anything, and the error names any missing file. The default cache (`ddi/` in the data directory) is deleted and downloaded again after 3 failed mounts in a row.

## Login and signing

- **Two-factor code:** `login`, `setup` and the first run of `iphone-control` ask for it in the terminal, so run them in an interactive terminal (not from a script or a pipe).
- **Anisette errors:** transient glitches or timeouts on the default public anisette server are retried automatically (up to 3 attempts with short backoff), and routine daily session checks log transient glitches and retry in an hour instead of firing a false "session expired" alert. If the public node stays down, retry later or run your own and set `ICTL_ANISETTE_URL` (see [SECURITY.md](../SECURITY.md)).
- **Account limits:** free accounts can only have a limited number of app IDs and devices. Use a dedicated Apple ID if your main one is already used with other sideloading tools.

## Abnormal daemon restart alerts and removed settings

- **Daemon restarted unexpectedly (`daemon` alert):** the daemon records a running marker in `run-state.json` and clears it on clean shutdown. If it crashes, gets `SIGKILL`ed, or exits via the 600-second watchdog and is restarted by launchd/systemd, the new process logs the event and sends a `daemon` alert (rate-limited to once every 30 minutes during a crash loop). Inspect `logs/daemon.log` and service logs (`journalctl --user -u iphone-control` or `~/Library/Logs/iphone-control/daemon.log`).
- **Removed settings warning in `doctor`:** if an older `~/.config/iphone-control/env` still sets removed variables to non-empty values, the daemon logs a gentle notice and `doctor` shows a non-fatal `⚠️` warning. Remove those lines and run `iphone-control service restart`.

## MCP

- **The client can't connect** ("connection closed", "failed to reconnect"): the MCP process itself ended or couldn't start (restarting the daemon doesn't end it: each call connects anew and waits up to 10 seconds for a daemon that is restarting). Reconnect from the client (in Claude Code, run `/mcp`) and check the registered command with `iphone-control mcp-config`. After an upgrade, reconnect too: a running MCP process keeps the old version's tools.
- **"守护进程正忙…已放弃执行":** an action waited more than 30 seconds for the device, so it was dropped instead of tapping a screen that has changed. Take a screenshot and try again.
- **An action reports that the WDA connection dropped:** the request was already sent, so it may have happened. It is not redone over HID, to avoid tapping or typing twice. Take a screenshot before trying again.
- **Taps do nothing:** a system alert may be covering the screen; the tool result says so (「屏幕上有系统弹窗…」). Dismiss it with `iphone_tap_element`. Taps right after launching an app are delayed until the launch animation is over.
- **Typed text is incomplete:** `iphone_type_text` reads the field back and types missing characters again. If it still reports a mismatch, the app is rewriting the field while you type; type shorter pieces.

## The phone shows a "paired computer" icon and won't auto-lock

The HID fallback keeps a screen-mirroring session open while it is in use (the Dynamic Island shows a computer icon), and so do taps, swipes and buttons under native control on iOS 27 and later, which go through HID; the control page showing the phone's own video uses the same session. It closes after 30 seconds of inactivity. If the icon stays, restart the daemon; stopping it closes the session.

## macOS

- **Run it as your regular user** ([macOS guide](https://derrors.github.io/iphone-control/en/deploy/macos)), never with `sudo`: as root it uses Linux system paths.
- **usbmuxd:** it is part of macOS and always running. If `doctor` can't reach it: `sudo launchctl kickstart -k system/com.apple.usbmuxd`.
- **Finder** can stay open on the phone; macOS's `remoted` doesn't take the connection away. Xcode's *Devices and Simulators* window is untested.

## Logs

- Event log, every system: `~/.local/share/iphone-control/logs/daemon.log`.
- Linux service: `journalctl --user -u iphone-control` (Homebrew's unit: `-u homebrew.iphone-control`).
- macOS service: `~/Library/Logs/iphone-control/daemon.log` (installed by `install.sh` or `iphone-control service install`) or `$(brew --prefix)/var/log/iphone-control.log` (`brew services`).
- A daemon started by hand logs to its terminal.
- macOS system logs: use `/usr/bin/log` (zsh has a built-in `log`).
- `iphone-control service status` shows which service is set up and whether the daemon runs.
- More detail: set `ICTL_LOG=info,ictl_wda::http=debug` in `~/.config/iphone-control/env` and run `iphone-control service restart` to log every WebDriverAgent request with its duration. Remove it afterwards; it is verbose.

## Reporting a bug

Please include the iPhone or iPad model, iOS version, how you installed it (install.sh, Homebrew, .deb or source; Linux or macOS), and the output of `iphone-control doctor`. Remove anything private (UDID, Apple ID, notification keys) first.
