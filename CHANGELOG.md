# Changelog

**English** | [简体中文](CHANGELOG.zh-CN.md)

All notable changes to this project are documented here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/) (while at 0.x, minor versions may contain breaking changes).

## [Unreleased]

### Changed

- MCP tool descriptions now say when to use each tool instead of its siblings, what it returns and how it behaves (cold launch vs. resume, force-quit, swipe inertia), every parameter has a description, and every tool carries MCP annotations (title, read-only, destructive, idempotent). Tool names, parameters and results are unchanged.

### Fixed

- `home` and `lock` did nothing on an iPhone 12 mini (iOS 26.4.1) while reporting success, and auto-unlock could not wake a dark screen. Home is now pressed as an IOHID AC Home event, falling back to the XCTest button if the home screen doesn't come up; lock presses the power key over IOHID, falling back to WebDriverAgent's lock. Auto-unlock alternates the two ways of waking the screen.
- When auto-unlock can't bring up the passcode keypad, the message now mentions StandBy (a phone charging on its side), which hides the keypad.

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

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Derrors/iphone-control/releases/tag/v0.1.0
