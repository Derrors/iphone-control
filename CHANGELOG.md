# Changelog

**English** | [简体中文](CHANGELOG.zh-CN.md)

All notable changes to this project are documented here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/) (while at 0.x, minor versions may contain breaking changes).

## [Unreleased]

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

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/Derrors/iphone-control/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Derrors/iphone-control/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Derrors/iphone-control/releases/tag/v0.1.0
