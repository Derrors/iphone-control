# Changelog

**English** | [简体中文](CHANGELOG.zh-CN.md)

All notable changes to this project are documented here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/) (while at 0.x, minor versions may contain breaking changes).

## [Unreleased]

### Added

- AI tasks in the live viewer: type a task on the page and a local agent (Claude Code or Codex, picked on the page) carries it out on the phone through the MCP tools, while the page shows each step with what the agent says it is doing, the result and screenshots, and marks where it taps. Pause, resume, take over (act on the phone yourself) and hand back, or stop. Before paying, deleting, sending a message or changing account settings the agent asks on the page (`iphone_confirm`) and waits; a rejected step isn't done. The steps, holds and confirmations go through `iphone-control mcp` itself, so they work the same with either agent; the agent runs with only the phone tools (no shell or file tools for Claude Code, a read-only sandbox for Codex).

### Changed

- New viewer page: AI assistant, live screen with zoom, and a device panel with state and readiness checks, installed apps (launch, quit, open a URL) and on-screen elements (highlight on the picture, tap by name, wait for text). The device can be switched from the page, and the picture follows. While an AI task runs, manual actions are refused until you take over.
- On iOS 27 and later the viewer's picture works like a touchscreen: pressing, moving and releasing the mouse moves a finger on the phone as you go (over CoreDevice HID), so a list scrolls while you drag, and the mouse wheel or trackpad scrolls the page under the pointer without holding the button. A tap round trip is about 20 ms instead of about 530 ms, and a drag no longer waits for the list to stop (1.8 to 3 s before). Older iOS versions keep whole gestures sent on release. While the viewer is used, the HID session's screen stream keeps the phone awake, until 30 seconds after the last touch.
- The live picture runs at about 50 frames a second at 75% size (WDA's default was 10 at full size).
- The viewer page is in English or Chinese: it follows `IPHONE_CONTROL_LANG`, and a button switches it. The daemon's messages and the AI task (instructions, tool results) use the page's language.
- The viewer's task history shows one plain sentence per step (what the agent said it is doing, or a description of the action), without screenshots, tool names, parameters or raw results; a step that didn't work says so. Screenshots are no longer passed from the agent's tools to the viewer.
- The viewer no longer draws marks for your own taps and swipes, and no longer shows a message for each successful manual action; errors are still shown.
- The viewer serves more read-only JSON (`/api/daemon`, `/status`, `/devices`, `/apps`, `/elements`) and refuses reads that a browser marks as sent by another site (`Sec-Fetch-Site`), since reading elements can unlock the phone.

### Fixed

- `lock` no longer takes about 10 seconds on newer iPhones: the power key is pressed for 0.1 s instead of held for 0.5 s, which an iPhone 18 Pro Max (iOS 27.2.1) ignored, so every lock fell through to WebDriverAgent's slower lock and logged a timeout.
- `home` takes about 1.3 seconds instead of 3.7 on devices that ignore the IOHID Home event (iPhone 18 Pro Max, iOS 27.2.1): when only the XCTest press works, the daemon remembers that for the device and uses it first. Devices where only the IOHID press works (iPhone 12 mini) keep their order.
- Taps and long presses no longer take about 26 seconds while something on screen keeps animating (seen tapping a search result in Settings with the keyboard up, iPhone 18 Pro Max, iOS 27.2.1): like swipes, they no longer wait for WebDriverAgent's animation cool-off. The result came back in about 1.4 seconds.
- `iphone_web_targets` and `iphone_web_eval` say to turn on Settings → Safari → Advanced → Web Inspector when the phone closes the connection, instead of only showing a TLS error.
- `iphone_app_logs` no longer returns Info lines, matching its description (some processes still send them on a stream without debug messages).
- English device descriptions have a space before the parenthesis and after the comma: `name (model iOS version, UDID)`.

## [0.1.5] - 2026-11-10

### Added

- English: `IPHONE_CONTROL_LANG=en` switches tool descriptions and results, CLI output and help, `status`/`doctor`, logs and notifications to English. The default stays Simplified Chinese, and older clients still get Chinese.
- `iphone-control viewer [--port]`: watch and control the phone live in a browser (opt-in, 127.0.0.1:8200 by default). Other local users can reach the port, so stop it when done.
- Developer tools, opt-in with `IPHONE_CONTROL_DEV_TOOLS=1`: `iphone_app_logs`, `iphone_crash_reports`, `iphone_install_app`, `iphone_uninstall_app`, `iphone_web_targets` and `iphone_web_eval`. Without the switch the tool list is unchanged.

### Fixed

- Scroll bars are dropped from element lists in every system language, not only Chinese and English.
- `iphone_uninstall_app` only accepts a bundle ID or the full display name; empty, partial or ambiguous names are refused and the candidates are listed.
- App installs and re-signing wait for each other instead of uploading to the phone at the same time.
- A daemon that gets stuck exits by itself, so systemd or Docker can restart it.
- Memory no longer grows while the HID fallback is in use.
- `usbmuxd.log` is rotated when it gets too large.

## [0.1.4] - 2026-11-07

### Security

- Updated the `zip` crate to 9.4.1 (from 8.6.0, and 2.6.1 in the vendored apple-codesign) for GHSA-m665-v7pm-7grj, GHSA-q7v9-x93c-xqv3, GHSA-hjqx-j4rw-g7xv, GHSA-9hj8-73fh-9963 and GHSA-8hxf-qr8p-62x4. iphone-control only unpacks the checksum-verified WebDriverAgent package from Appium, so the practical exposure was small.
- The documentation site's build tooling uses vite 6.4.5 and esbuild 0.25 (development dependencies only; the published site was not affected).

### Changed

- `ICTL_DDI_DIR` (a Developer Disk Image directory you extracted yourself) is never downloaded into or deleted from; missing files are reported by name. Only the default `ddi/` cache is cleared and downloaded again, and only after three failed *mounts* (tunnel errors no longer count).
- The `debug` subcommands are no longer built by default; build with `--features debug-cmds` to get them.
- Tool descriptions say that screenshots, `iphone_list_elements` and `iphone_wait_for` also unlock the phone when auto-unlock is configured, and that the HID fallback needs iOS 27.

### Fixed

- An action whose WebDriverAgent request was sent but whose connection then dropped, or whose response was incomplete, is no longer repeated over HID (a tap or text could happen twice). It now reports that the phone may already have done it.
- Calls to usbmuxd, lockdown, AMFI, misagent and installation_proxy have time limits, so a phone or usbmuxd that accepts a connection but never answers can no longer hang the daemon.
- Re-signing has an overall time limit (15 minutes) instead of staying "in progress" forever when Apple's servers or the install stall, and the Apple ID session check has one too. After the phone reconnects or another device is selected, re-signing is checked within a minute instead of waiting up to `RESIGN_CHECK_HOURS`. Finishing a re-sign for one device no longer tears down the session of a device selected meanwhile.
- When the phone is locked before its first unlock and WDA can't start, `status` keeps showing that the phone needs unlocking and retries every 20 seconds, instead of flipping back to "starting" and retrying every 10 seconds.
- A HID session that timed out while opening no longer leaves the screen media stream running on the phone (no auto-lock, icon stuck in the Dynamic Island).
- Cancelling an MCP call stops it: the remaining `iphone_batch` steps don't run, and a request still waiting for the device is dropped.
- `iphone_batch` stops if the controlled device changes while it runs (for example, the device is unplugged and another one is picked automatically).
- A newly started WebDriverAgent is only considered ready after its test plan has started, so a stale WDA still answering on the phone can't make it look ready early.
- `doctor` counts only iPhones, iPads and iPods as phones on USB; an Apple keyboard no longer passes the check.
- The system-alert check after an action is no longer skipped when the tapped element's text happens to contain the words "系统弹窗".
- Auto-unlock now also recognizes leftover WDA wait settings from an interrupted swipe or poll and restores the defaults.
- iPad: when WebDriverAgent was started while the iPad was locked, it keeps reporting "not locked", and a dark Touch ID lock screen with notifications has no lock marker in the element tree. Auto-unlock used to trust WDA from then on and never unlock; such an ambiguous screen is now treated as possibly locked (unlocked automatically when a passcode is set, shown as "possibly locked" in `status`).
- iPad: rows in the floating sidebar of iPadOS 26 apps such as Settings can be tapped by name again; the empty sidebar-sized containers behind the list were mistaken for something covering it.
- When WDA drops between two health checks, errors say it just dropped instead of showing the state as "ready".

## [0.1.3] - 2026-11-07

### Upgrade notes

- Auto-unlock passcodes now belong to individual devices. Before upgrading, ensure the only connected device is the one whose passcode was saved by the old version; that legacy passcode is automatically assigned when exactly one device is connected. If its owner is uncertain, clear it with the old version's `passcode clear` before upgrading, then use `passcode set --device <UDID>` for each device. Back up the data directory first.
- Device selection is shared by all clients. Explicit `select` choices survive daemon restarts; automatic selection keeps the current device only while the daemon runs and it remains connected.
- Landscape HID fallback needs a matching WDA rotation sample no older than 30 seconds. Do not flip the device between landscape sides while WDA is unavailable; wait for WDA to recover before doing so.

### Added

- Choose which device to control when several iPhones/iPads are connected: new MCP tools `iphone_list_devices` and `iphone_select_device`, commands `devices` and `select`. The choice is saved in state.json and survives restarts; `select auto` goes back to automatic choice. `IPHONE_UDID`, when set, wins and switching is refused.
- Auto-unlock passcodes are kept per device (`secrets/passcodes/<UDID>`); `passcode set/clear` accept `--device`. A passcode saved by an older version is assigned to the device when exactly one is connected over USB, and isn't used while several are.

### Fixed

- Screen dimensions now follow rotation, preventing stretched screenshots and filtered-out elements near the bottom. iPad landscape system UI coordinates are converted to screen coordinates, using the status bar position to distinguish the two landscape directions.
- Auto-unlock on Touch ID devices now taps “Use Passcode” first; lock-screen detection and iPad delete-key positioning are handled, with numeric keypad layout validation before typing.
- Read the current device's installed WDA provisioning profiles via misagent for `doctor`, `status` and automatic re-signing, rather than relying on another device's last signing record.
- `setup` waits for the daemon to recognise the target device before signing and installing. Offline and pre-iOS-27 diagnostics are corrected; generic WDA launch errors include a first-install developer-trust troubleshooting hint.
- `open_url` waits for a stable foreground app, reducing cold-start black screenshots and transient system services being reported as the target app.
- HID fallback taps and swipes now map landscape coordinates back to the device's native portrait coordinate system. Saved dimensions and reused HID sessions are normalized to portrait. Every fallback touch checks a fresh screenshot; landscape touches need a matching WDA rotation sample no older than 30 seconds and are refused when it is unavailable, including after a daemon restart. Do not flip the device between landscape sides while WDA is unavailable: screenshot dimensions alone cannot detect a 180° change.
- Without `IPHONE_UDID` and with several devices connected, a change in usbmuxd's list order silently switched control to another device (auto-unlock could type one device's passcode into another). The controlled device is now kept until it disconnects.
- With `IPHONE_UDID` set to a device that isn't connected while another one is, usbmuxd was restarted every 30 seconds.
- In the container, a second device plugged in while one was already connected was never picked up by usbmuxd (no udev, so no hotplug events), and usbmuxd wasn't restarted. The number of iPhones/iPads on USB is now compared with the number usbmuxd sees.
- After switching devices, the previous device's app list cache and screen size were still used.

## [0.1.2] - 2026-11-04

### Added

- Prebuilt macOS binary (Apple Silicon, macOS 11+) in Releases: `iphone-control-vX.Y.Z-macos-arm64.tar.gz`. It isn't notarized; see the macOS guide if Gatekeeper blocks it.

## [0.1.1] - 2026-11-04

### Added

- macOS (Apple Silicon) support: build from source and run as a regular user with the system usbmuxd; no Xcode needed. See the new macOS guide.

### Changed

- MCP tool descriptions now explain when to use each tool, what it returns and how it behaves; every parameter is documented and every tool carries MCP annotations. Tool names, parameters and results are unchanged.
- Release binaries are named `iphone-control-vX.Y.Z-linux-<arch>.tar.gz`.

### Fixed

- `home` and `lock` had no effect on some phones (seen on an iPhone 12 mini, iOS 26.4.1), and auto-unlock couldn't wake a dark screen there.
- After a disconnect of a few seconds, setups using the system usbmuxd (macOS, Linux source installs) got stuck on "bad device"; they now reconnect by themselves.
- `doctor` on macOS no longer reports "no Apple device on USB", and its hints use macOS commands.
- When auto-unlock can't bring up the passcode keypad, the message mentions StandBy, which hides the keypad.

## [0.1.0] - 2026-11-02

First public release.

### Added

- `iphone-control daemon`: one long-running process owns the phone — usbmuxd supervision (with a sysfs fallback for containers without udev), pairing and Developer Mode checks, Developer Disk Image download and mounting, a CoreDevice tunnel with a user-space TCP stack, WebDriverAgent launch and health checks, and a state machine that recovers from unplugging, usbmuxd restarts and phone reboots.
- MCP server with 15 tools: status, screenshots with zoom-in regions, element lists, tap / tap by text, long press, swipe, typing (including Chinese), hardware buttons, app launch / terminate / list, URLs, `iphone_wait_for` and `iphone_batch`. Action tools accept `then` to return the screen after the action.
- HID fallback for taps, swipes, buttons, ASCII typing and screenshots while WebDriverAgent is unavailable.
- Signing of WebDriverAgent with a free Apple ID (two-factor and SMS login), including the `.xctest` bundle, a post-signing check, automatic re-signing before the 7-day profile expires, and a daily Apple ID session check.
- Notifications through Bark and Telegram, with de-duplication.
- Optional auto-unlock with strict safeguards (stops after one wrong attempt).
- `setup` wizard, `doctor` diagnostics, `status`, `mcp-config`, `wda-bridge`, `notify-test`.
- Docker image (single binary plus Debian's usbmuxd, not privileged) with a generated third-party license list, published to `ghcr.io/derrors/iphone-control` for linux/amd64 and linux/arm64.
- Running without Docker, from source or from the prebuilt Linux binaries (x86_64, aarch64): as a regular user with the system usbmuxd, XDG data and socket paths, the unsigned WebDriverAgent downloaded and checksum-verified before the first signing, `mcp-config` printing a direct command, and a systemd user service.

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/Derrors/iphone-control/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Derrors/iphone-control/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Derrors/iphone-control/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Derrors/iphone-control/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Derrors/iphone-control/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Derrors/iphone-control/releases/tag/v0.1.0
