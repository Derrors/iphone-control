# FAQ

Start with these two commands; they explain most problems:

```bash
iphone-control status    # with Docker, prefix: docker exec -it iphone-control
iphone-control doctor    # checks each layer and prints a fix for every failure
```

## States

The program's messages are in Chinese; here is what they mean.

| `status` shows | Meaning | What to do |
|---|---|---|
| 手机未连接 | usbmuxd sees no phone | Check the cable and unlock the phone; with a usbmuxd on a Linux host, `sudo systemctl mask usbmuxd` |
| 未配对 | Not paired | Run `setup` and tap *Trust* on the phone |
| 开发者模式未打开 | Developer Mode is off | Run `setup` and follow the prompts |
| 手机锁屏，隧道建不起来 | Not unlocked since the last reboot | Unlock it once by hand |
| WDA 还没签名安装 | WDA not installed | Run `sign` or `setup` |
| 正在启动 WDA | Starting WDA (up to about 2 minutes) | Wait; taps, swipes and screenshots already work |
| WDA 卡在手机上的「Enable UI Automation」密码框 | The phone asks for the passcode for "XCTest" | Enter the passcode on the phone |
| WDA 启动失败，稍后重试：… | Other failure | Read the error after the colon and run `doctor` |
| 就绪 | Ready | — |

## The phone isn't detected

- **Two usbmuxd fighting:** many distributions install usbmuxd and udev starts it on plug-in, taking the phone away from Docker. `sudo systemctl mask usbmuxd`.
- **Another tool holds the phone:** go-ios, pymobiledevice3 and similar tools conflict.
- **Cable:** some cables only charge. If `lsusb` shows no Apple device (`05ac`), try another cable or port.
- **After re-plugging:** Docker recovers within about a minute.
- **Drops about once a minute while charging:** with *Settings → Battery → Charging → 80% Limit* on, the phone keeps starting and stopping charging near 80%, and each switch renegotiates USB-C power, which drops USB for 3–5 seconds. Set the limit to 100% on a phone used for automation.
- **On a Mac:** Docker can't be used (no USB passthrough); see [macOS](../deploy/macos). usbmuxd is part of macOS; if `doctor` can't reach it, run `sudo launchctl kickstart -k system/com.apple.usbmuxd`.

## WDA won't start

- **"Untrusted developer":** after the first install, trust your Apple ID under *Settings → General → VPN & Device Management* (the phone needs internet).
- **Expired signature:** free signatures last 7 days and are renewed automatically. `status` shows the time left; `resign --force` re-signs now.
- **Apple ID session expired:** you get a notification; run `login` again.
- **Developer Disk Image download fails:** it needs GitHub; set `HTTP_PROXY` / `HTTPS_PROXY`.

## Login

- **Can't type the code:** `login` and `setup` are interactive; use `-it` with Docker.
- **Anisette errors:** the default public anisette server may be down; retry later or run your own and set `ICTL_ANISETTE_URL`.

## AI clients

- **MCP disconnected:** restarting the container ends the MCP process; reconnect in the client (`/mcp` in Claude Code).
- **"守护进程正忙…已放弃执行":** an action queued for over 30 seconds and was dropped rather than tapping a screen that changed. Take a screenshot and retry.
- **Taps do nothing:** a system alert may be covering the screen; the result says so (「屏幕上有系统弹窗…」). Dismiss it first.
- **Typed text is incomplete:** `iphone_type_text` reads the field back and fixes it; if it still reports a mismatch, the app rewrites the field while typing — type shorter pieces.

## "Paired computer" icon stays and the phone won't auto-lock

The HID fallback opens a screen-mirroring session while in use and closes it after 30 seconds idle. If the icon doesn't go away, restart the daemon.

## Reporting a problem

[Open an issue](https://github.com/Derrors/iphone-control/issues/new/choose) with the phone model, iOS version, how you run it, and the `doctor` output — with UDID, Apple ID and notification keys removed.
