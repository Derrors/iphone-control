# Changelog

**English** | [简体中文](CHANGELOG.zh-CN.md)

All notable changes to this project are documented here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/) (while at 0.x, minor versions may contain breaking changes).

## [Unreleased]

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

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/Derrors/iphone-control/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Derrors/iphone-control/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Derrors/iphone-control/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Derrors/iphone-control/releases/tag/v0.1.0
