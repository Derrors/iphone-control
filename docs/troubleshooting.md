# Troubleshooting

Start with these two commands (prefix them with `docker exec -it iphone-control` for the container):

```bash
iphone-control status    # current state, signing info, last log lines
iphone-control doctor    # checks each layer in order and prints a fix for every failure
```

Messages are in Simplified Chinese by default, and in English with `IPHONE_CONTROL_LANG=en`. The default Chinese wording is quoted below so you can match it.

## States

`status` shows the state on its first line. What each state means and what to do about it: [FAQ](https://derrors.github.io/iphone-control/en/troubleshoot/faq#states) (English) · [常见问题](https://derrors.github.io/iphone-control/troubleshoot/faq#状态说明) (中文).

## Phone not detected

- **Container, after unplugging:** the container has no udev, so the daemon notices the phone through sysfs and restarts its own usbmuxd; recovery takes up to about a minute.
- **Two usbmuxd instances:** a host `usbmuxd` (installed by many distributions, started by udev on plug-in) takes the phone away from the container. Mask it with `sudo systemctl mask usbmuxd`, or, for a source install, use the system one and don't run the container.
- **Another tool holds the phone:** Xcode-less tools such as go-ios or pymobiledevice3 running at the same time can conflict. Stop them.
- **Cable:** some cables only charge. If `lsusb` doesn't show an Apple device (vendor `05ac`), try another cable or port.
- **Drops every minute or so while charging:** with *Settings → Battery → Charging → 80% Limit* on, the phone keeps starting and stopping charging near 80%, and each switch renegotiates USB-C power, which briefly disconnects USB (3–5 s). The daemon reconnects by itself, but in-flight actions fail. Set the limit to 100% while the phone is used for automation.

## WebDriverAgent won't start

- **"Untrusted developer":** after the first install, trust your Apple ID under *Settings → General → VPN & Device Management* (needs internet on the phone).
- **Expired signature:** free provisioning profiles last 7 days. The daemon re-signs when fewer than `RESIGN_MIN_DAYS` (default 2) remain. `status` shows the remaining time; `resign --force` re-signs now. Automatic re-signing only covers the controlled device, and is checked within a minute after it reconnects or you switch to it. A re-sign that takes too long counts as failed and you get a notification.
- **Apple ID session expired:** you get a notification, and `resign` fails with a login error. Run `login` again.
- **Developer Disk Image:** a failure shows on the first line of `status` as `WDA 启动失败，稍后重试：开发者镜像：…`; `doctor` has no separate check for it, and the details are in `logs/daemon.log`. Downloading needs access to GitHub; behind a firewall set `HTTP_PROXY`/`HTTPS_PROXY`, or point `ICTL_DDI_DIR` at an image extracted from Xcode. With `ICTL_DDI_DIR` the daemon neither downloads nor deletes anything, and the error names any missing file. The default cache (`ddi/` in the data directory) is deleted and downloaded again after 3 failed mounts in a row.

## Login and signing

- **Two-factor code:** `login` and `setup` ask for it in the terminal. Run them with `-it` (`docker exec -it …`), otherwise there is no terminal to type into.
- **Anisette errors:** the default public anisette server may be slow or down. Retry later, or run your own and set `ICTL_ANISETTE_URL` (see [SECURITY.md](../SECURITY.md)).
- **Account limits:** free accounts can only have a limited number of app IDs and devices. Use a dedicated Apple ID if your main one is already used with other sideloading tools.

## MCP

- **The client can't connect** ("connection closed", "failed to reconnect"): restarting the container ends the MCP process; reconnect from the client (in Claude Code, run `/mcp`). Check the registered command with `iphone-control mcp-config`.
- **"守护进程正忙…已放弃执行":** an action waited more than 30 seconds for the device, so it was dropped instead of tapping a screen that has changed. Take a screenshot and try again.
- **An action reports that the WDA connection dropped:** the request was already sent, so it may have happened. It is not redone over HID, to avoid tapping or typing twice. Take a screenshot before trying again.
- **Taps do nothing:** a system alert may be covering the screen; the tool result says so (「屏幕上有系统弹窗…」). Dismiss it with `iphone_tap_element`. Taps right after launching an app are delayed until the launch animation is over.
- **Typed text is incomplete:** `iphone_type_text` reads the field back and types missing characters again. If it still reports a mismatch, the app is rewriting the field while you type; type shorter pieces.

## The phone shows a "paired computer" icon and won't auto-lock

The HID fallback keeps a screen-mirroring session open while it is in use (the Dynamic Island shows a computer icon) and closes it after 30 seconds of inactivity. If the icon stays, restart the daemon; stopping it closes the session.

## macOS

- **No Docker:** Docker Desktop has no USB passthrough; run the binary directly as your regular user ([macOS guide](https://derrors.github.io/iphone-control/en/deploy/macos)). Don't run it with `sudo`: as root it starts a second usbmuxd and uses Linux system paths.
- **usbmuxd:** it is part of macOS and always running. If `doctor` can't reach it: `sudo launchctl kickstart -k system/com.apple.usbmuxd`.
- **Finder** can stay open on the phone; macOS's `remoted` doesn't take the connection away. Xcode's *Devices and Simulators* window is untested.

## Logs

- Container: `docker logs iphone-control`, and the event log at `/var/lib/iphone-control/logs/daemon.log` inside the container (`./data/logs/` on the host).
- Source install: `journalctl --user -u iphone-control` and `~/.local/share/iphone-control/logs/`.
- macOS: the terminal running `iphone-control daemon`, and `~/.local/share/iphone-control/logs/`. For system logs use `/usr/bin/log` (zsh has a built-in `log`).
- More detail: set `ICTL_LOG=info,ictl_wda::http=debug` to log every WebDriverAgent request with its duration. Remove it afterwards; it is verbose.

## Reporting a bug

Please include the iPhone or iPad model, iOS version, how you run it (Docker, prebuilt binary or source; Linux or macOS), and the output of `iphone-control doctor`. Remove anything private (UDID, Apple ID, notification keys) first.
