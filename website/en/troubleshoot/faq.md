# FAQ

Start with these two commands; they explain most problems:

```bash
iphone-control status    # with Docker, prefix: docker exec -it iphone-control
iphone-control doctor    # checks each layer and prints a fix for every failure
```

## States

Messages are in Simplified Chinese by default, and in English with `IPHONE_CONTROL_LANG=en`. The table lists both wordings. The daemon re-checks every 10 seconds and moves on by itself once the cause is fixed.

| `status` shows | Meaning | What to do |
|---|---|---|
| 手机未连接<br>Phone not connected | usbmuxd sees no phone | Check the cable and unlock the phone; with a usbmuxd on a Linux host, `sudo systemctl mask usbmuxd`. With several devices, a selected device that isn't connected also shows this: check the `选设备` (`Device`) line of `status`, or run `select auto`; see [Several devices](../guide/multi-device) |
| 未配对<br>Not paired | No pairing record | Run `setup` and tap *Trust* on the phone |
| 开发者模式未打开<br>Developer Mode is off | Developer Mode is off | *Settings → Privacy & Security → Developer Mode*. The toggle only appears after the phone has been seen by a developer tool; running `setup` takes care of that |
| 手机锁屏，隧道建不起来<br>The phone is locked, the tunnel can't be opened | Not unlocked since the last reboot | Unlock it once by hand; the state stays until then and the daemon keeps retrying |
| WDA 还没签名安装<br>WDA not signed and installed yet | WDA not installed | Run `sign` or `setup` |
| 正在启动 WDA<br>Starting WDA | Starting WDA (up to about 2 minutes) | Wait. On iOS 27 and later, taps, swipes and screenshots already work through the HID fallback, but a landscape touch needs an orientation from WDA in the last 30 seconds or it is refused, and an unknown screen size returns an error; see [Tools](../guide/tools#when-wda-is-down-the-hid-fallback). On older iOS, wait for WDA |
| WDA 卡在手机上的「Enable UI Automation」密码框<br>WDA is stuck at the “Enable UI Automation” passcode prompt | The phone asks for the passcode for "XCTest" | Enter the passcode on the phone. This happens after a reboot, and sometimes after the daemon is restarted several times in a row. The daemon retries every 60 seconds and notifies you once |
| WDA 启动失败，稍后重试：…<br>WDA failed to start, retrying later: … | Other failure | Read the error after the colon and run `doctor`. Retries back off up to 5 minutes |
| 就绪<br>Ready | Everything works | — |

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
- **Developer Disk Image fails:** the first line of `status` says `WDA 启动失败，稍后重试：开发者镜像：…`, with details in `logs/daemon.log`; `doctor` has no separate check for it. Downloading needs GitHub; set `HTTP_PROXY` / `HTTPS_PROXY`, or point `ICTL_DDI_DIR` at a directory extracted from Xcode; see [Configuration](../guide/configuration#advanced).

## Login

- **Can't type the code:** `login` and `setup` are interactive; use `-it` with Docker.
- **Anisette errors:** the default public anisette server may be down; retry later or run your own and set `ICTL_ANISETTE_URL`.

## AI clients

- **MCP disconnected:** restarting the container ends the MCP process; reconnect in the client (`/mcp` in Claude Code).
- **"守护进程正忙…已放弃执行":** an action queued for over 30 seconds and was dropped rather than tapping a screen that changed. Take a screenshot and retry.
- **An action reports that the WDA connection dropped:** the request was already sent, so it may have happened. It isn't redone, to avoid tapping or typing twice. Take a screenshot first.
- **Taps do nothing:** a system alert may be covering the screen; the result says so (「屏幕上有系统弹窗…」). Dismiss it first.
- **Typed text is incomplete:** `iphone_type_text` reads the field back and fixes it; if it still reports a mismatch, the app rewrites the field while typing — type shorter pieces.

## "Paired computer" icon stays and the phone won't auto-lock

The HID fallback opens a screen-mirroring session while in use and closes it after 30 seconds idle. If the icon doesn't go away, restart the daemon.

## Reporting a problem

[Open an issue](https://github.com/Derrors/iphone-control/issues/new/choose) with the phone model, iOS version, how you run it, and the `doctor` output — with UDID, Apple ID and notification keys removed.
