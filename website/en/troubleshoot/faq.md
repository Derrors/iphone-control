# FAQ

Start with these two commands; they explain most problems:

```bash
iphone-control status    # daemon and device state, recent log
iphone-control doctor    # checks each layer and prints a fix for every failure
```

## States

Messages are in Simplified Chinese by default, and in English with `IPHONE_CONTROL_LANG=en`. The table lists both wordings. The daemon re-checks every 10 seconds and moves on by itself once the cause is fixed.

| `status` shows | Meaning | What to do |
|---|---|---|
| 手机未连接<br>Phone not connected | usbmuxd sees no phone | Check the cable and unlock the phone; on Linux, make sure the system usbmuxd is installed (`sudo apt install usbmuxd`). With several devices, a selected device that isn't connected also shows this: check the `选设备` (`Device`) line of `status`, or run `select auto`; see [Several devices](../guide/multi-device) |
| 未配对<br>Not paired | No pairing record | Run `setup --reconfigure`, pick *Phone*, and tap *Trust* on the phone |
| 开发者模式未打开<br>Developer Mode is off | Developer Mode is off | *Settings → Privacy & Security → Developer Mode*. The toggle only appears after the phone has been seen by a developer tool; running `setup --reconfigure` and picking *Phone* takes care of that |
| 手机锁屏，隧道建不起来<br>The phone is locked, the tunnel can't be opened | Not unlocked since the last reboot | Unlock it once by hand; the state stays until then and the daemon keeps retrying |
| WDA 还没签名安装<br>WDA not signed and installed yet | WDA not installed | Run `sign` or `setup` |
| 正在启动 WDA<br>Starting WDA | Starting WDA (up to about 2 minutes) | Wait. On iOS 27 and later, taps, swipes and screenshots already work through the HID fallback, but a landscape touch needs an orientation from WDA in the last 30 seconds or it is refused, and an unknown screen size returns an error; see [Tools](../guide/tools#when-wda-is-down-the-hid-fallback). On older iOS, wait for WDA |
| WDA 卡在手机上的「Enable UI Automation」密码框<br>WDA is stuck at the “Enable UI Automation” passcode prompt | The phone asks for the passcode for "XCTest" | Enter the passcode on the phone. This happens after a reboot, and sometimes after the daemon is restarted several times in a row. The daemon retries every 60 seconds and notifies you once |
| WDA 启动失败，稍后重试：…<br>WDA failed to start, retrying later: … | Other failure | Read the error after the colon and run `doctor`. Retries back off up to 5 minutes |
| 就绪<br>Ready | Everything works | — |

## The phone isn't detected

- **No usbmuxd on Linux:** the daemon uses the system usbmuxd and doesn't start one itself. `sudo apt install usbmuxd` (the .deb pulls it in). If you ran `sudo systemctl mask usbmuxd` for the Docker deployment, run `sudo systemctl unmask usbmuxd` first.
- **The daemon isn't running:** check with `iphone-control service status` and start it with `iphone-control service start`. Running `iphone-control` also starts the service when it isn't running.
- **Another tool holds the phone:** go-ios, pymobiledevice3 and similar tools conflict.
- **Cable:** some cables only charge. If `lsusb` shows no Apple device (`05ac`), try another cable or port.
- **After re-plugging:** it recovers within about a minute.
- **Drops about once a minute while charging:** with *Settings → Battery → Charging → 80% Limit* on, the phone keeps starting and stopping charging near 80%, and each switch renegotiates USB-C power, which drops USB for 3–5 seconds. Set the limit to 100% on a phone used for automation.
- **On a Mac:** usbmuxd is part of macOS; if `doctor` can't reach it, run `sudo launchctl kickstart -k system/com.apple.usbmuxd`. See [macOS](../deploy/macos).

## WDA won't start

- **"Untrusted developer":** after the first install, trust your Apple ID under *Settings → General → VPN & Device Management* (the phone needs internet).
- <a id="cert"></a>**Signing certificate revoked:** `status` says `unexpected response` although the profile is still valid; tapping the WebDriverAgentRunner icon on the device says the app can't be verified.
  - Why: every install (each machine, each data directory) has its own signing key and certificate, and a free Apple ID holds at most two certificates at a time. Signing somewhere new for the first time (another machine, moving from Docker to a native install, a deleted data directory) or with the same Apple ID in a sideloading tool such as AltStore or SideStore creates a certificate; when the team is full an old one is revoked, and apps signed with it won't start on any device.
  - The daemon handles it: when a device refuses to start WDA and its WDA wasn't signed with this machine's certificate, or this machine's certificate is no longer in the Apple ID's team, it re-signs that device automatically (at most once per device every 6 hours) and sends a notification. `doctor` shows both checks.
  - By hand: `iphone-control sign` re-signs the controlled device; the control page also offers "Re-sign and install WDA" when WDA fails to start.
  - Don't sign with the same Apple ID in two places at once: stop old deployments you no longer use, or give them different Apple IDs.
- **Expired signature:** free signatures last 7 days and are renewed automatically. `status` shows the time left; `resign --force` re-signs now.
- **Apple ID session expired:** you get a notification. Run `iphone-control setup --reconfigure` and pick *Apple ID* to sign in again and re-sign WDA.
- **Developer Disk Image fails:** the first line of `status` says `WDA 启动失败，稍后重试：开发者镜像：…`, with details in `logs/daemon.log`; `doctor` has no separate check for it. Downloading needs GitHub; set a proxy (`iphone-control setup --reconfigure`, pick *Proxy*), or point `ICTL_DDI_DIR` at a directory extracted from Xcode; see [Configuration](../guide/configuration#advanced).

## Login

- **Can't type the code:** `login`, `setup` and the first run of `iphone-control` are interactive; run them in a terminal, not from a script or a pipe.
- **Anisette errors:** the default public anisette server may be down; retry later or run your own and set `ICTL_ANISETTE_URL`.

## AI clients

- **The MCP tools are missing:** run `iphone-control setup --reconfigure` and pick *MCP registration*, or run `iphone-control mcp-config` to print the registration command, then start a new session.
- **"守护进程正忙…已放弃执行":** an action queued for over 30 seconds and was dropped rather than tapping a screen that changed. Take a screenshot and retry.
- **An action reports that the WDA connection dropped:** the request was already sent, so it may have happened. It isn't redone, to avoid tapping or typing twice. Take a screenshot first.
- **Taps do nothing:** a system alert may be covering the screen; the result says so (「屏幕上有系统弹窗…」). Dismiss it first.
- **Typed text is incomplete:** `iphone_type_text` reads the field back and fixes it; if it still reports a mismatch, the app rewrites the field while typing — type shorter pieces.

## No picture on the control page after switching devices

The picture area says why: not connected; pairing or Developer Mode still needed (run `iphone-control setup --reconfigure` and pick *Phone*); WDA starting (usually 10–60 seconds); or it needs unlocking. When the new device has no WDA yet and an Apple ID is signed in, click *Sign and install WDA*; it takes about 1–2 minutes, keep the phone unlocked. Without an Apple ID, run `iphone-control setup --reconfigure` and pick *Apple ID*. Switching is refused while an AI task runs, and when `IPHONE_UDID` is set. See [Several devices](../guide/multi-device).

## "Paired computer" icon stays and the phone won't auto-lock

The HID fallback opens a screen-mirroring session while in use and closes it after 30 seconds idle. If the icon doesn't go away, restart the daemon (`iphone-control service restart`).

## Reporting a problem

[Open an issue](https://github.com/Derrors/iphone-control/issues/new/choose) with the phone model, iOS version, how you installed it (one command, Homebrew, .deb or source), and the `doctor` output — with UDID, Apple ID and notification keys removed.
