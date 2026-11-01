# Troubleshooting

Start with these two commands (prefix them with `docker exec -it iphone-control` for the container):

```bash
iphone-control status    # current state, signing info, last log lines
iphone-control doctor    # checks each layer in order and prints a fix for every failure
```

The program's own messages are in Simplified Chinese; the Chinese text is quoted below so you can match it.

## States

`status` shows one of these on its first line. The daemon re-checks every 10 seconds and moves on by itself once the cause is fixed.

| State (Chinese text) | Meaning | What to do |
|---|---|---|
| 手机未连接 | usbmuxd sees no phone | Check the cable and unlock the phone. Container: make sure no host `usbmuxd` is running (`sudo systemctl mask usbmuxd`). See [Phone not detected](#phone-not-detected) |
| 未配对 | No pairing record | Run `setup` and tap *Trust* on the phone |
| 开发者模式未打开 | Developer Mode is off | *Settings → Privacy & Security → Developer Mode*. The toggle only appears after the phone has been seen by a developer tool; running `setup` takes care of that |
| 手机锁屏，隧道建不起来 | The phone has not been unlocked since it restarted (BFU) | Unlock it once by hand. Auto-unlock cannot do this step |
| WDA 还没签名安装 | WebDriverAgent is not installed | Run `sign` (or `setup`) |
| 正在启动 WDA | Starting WebDriverAgent (up to about 2 minutes) | Wait. Tools that only need coordinates (tap, swipe, screenshot, buttons) already work through the HID fallback |
| WDA 卡在手机上的「Enable UI Automation」密码框 | The phone is showing *Enter iPhone passcode for "XCTest"* | Enter the passcode on the phone. This happens after a reboot, and sometimes after the daemon is restarted several times in a row. The daemon retries every 60 seconds and notifies you once |
| WDA 启动失败，稍后重试：… | WebDriverAgent failed for another reason | Read the error after the colon and run `doctor`. Retries back off up to 5 minutes |
| 就绪 | Everything works | — |

## Phone not detected

- **Container, after unplugging:** the container has no udev, so the daemon notices the phone through sysfs and restarts its own usbmuxd; recovery takes up to about a minute.
- **Two usbmuxd instances:** a host `usbmuxd` (installed by many distributions, started by udev on plug-in) takes the phone away from the container. Mask it with `sudo systemctl mask usbmuxd`, or, for a source install, use the system one and don't run the container.
- **Another tool holds the phone:** Xcode-less tools such as go-ios or pymobiledevice3 running at the same time can conflict. Stop them.
- **Cable:** some cables only charge. If `lsusb` doesn't show an Apple device (vendor `05ac`), try another cable or port.

## WebDriverAgent won't start

- **"Untrusted developer":** after the first install, trust your Apple ID under *Settings → General → VPN & Device Management* (needs internet on the phone).
- **Expired signature:** free provisioning profiles last 7 days. The daemon re-signs when fewer than `RESIGN_MIN_DAYS` (default 2) remain. `status` shows the remaining time; `resign --force` re-signs now.
- **Apple ID session expired:** you get a notification, and `resign` fails with a login error. Run `login` again.
- **Developer Disk Image:** `doctor` reports whether it is mounted. Downloading needs access to GitHub; behind a firewall set `HTTP_PROXY`/`HTTPS_PROXY`, or point `ICTL_DDI_DIR` at an image extracted from Xcode.

## Login and signing

- **Two-factor code:** `login` and `setup` ask for it in the terminal. Run them with `-it` (`docker exec -it …`), otherwise there is no terminal to type into.
- **Anisette errors:** the default public anisette server may be slow or down. Retry later, or run your own and set `ICTL_ANISETTE_URL` (see [SECURITY.md](../SECURITY.md)).
- **Account limits:** free accounts can only have a limited number of app IDs and devices. Use a dedicated Apple ID if your main one is already used with other sideloading tools.

## MCP

- **The client can't connect** ("connection closed", "failed to reconnect"): restarting the container ends the MCP process; reconnect from the client (in Claude Code, run `/mcp`). Check the registered command with `iphone-control mcp-config`.
- **"守护进程正忙…已放弃执行":** an action waited more than 30 seconds for the device, so it was dropped instead of tapping a screen that has changed. Take a screenshot and try again.
- **Taps do nothing:** a system alert may be covering the screen; the tool result says so (「屏幕上有系统弹窗…」). Dismiss it with `iphone_tap_element`. Taps right after launching an app are delayed until the launch animation is over.
- **Typed text is incomplete:** `iphone_type_text` reads the field back and types missing characters again. If it still reports a mismatch, the app is rewriting the field while you type; type shorter pieces.

## The phone shows a "paired computer" icon and won't auto-lock

The HID fallback keeps a screen-mirroring session open while it is in use (the Dynamic Island shows a computer icon) and closes it after 30 seconds of inactivity. If the icon stays, restart the daemon; stopping it closes the session.

## Logs

- Container: `docker logs iphone-control`, and the event log at `/var/lib/iphone-control/logs/daemon.log` inside the container (`./data/logs/` on the host).
- Source install: `journalctl --user -u iphone-control` and `~/.local/share/iphone-control/logs/`.
- More detail: set `ICTL_LOG=info,ictl_wda::http=debug` to log every WebDriverAgent request with its duration. Remove it afterwards; it is verbose.

## Reporting a bug

Please include the phone model, iOS version, how you run it (container or source), and the output of `iphone-control doctor`. Remove anything private (UDID, Apple ID, notification keys) first.
