<div align="center">
  <a href="https://github.com/Derrors/iphone-control">
    <img src="docs/images/banner.svg" alt="iphone-control: Let AI agents operate a real iPhone" width="100%">
  </a>

  <p align="center">
    Let an AI agent operate a real iPhone or iPad plugged into a Linux machine or a Mac — one Rust binary, no Xcode, no paid developer account, no jailbreak.
  </p>

  <p align="center">
    <a href="https://github.com/Derrors/iphone-control/releases/latest"><img src="https://img.shields.io/github/v/release/Derrors/iphone-control?style=flat" alt="Release"></a>
    <img src="https://img.shields.io/badge/iOS-17%2B-black?style=flat&logo=apple" alt="iOS">
    <img src="https://img.shields.io/badge/platform-Linux%20%7C%20macOS-0E7490?style=flat" alt="Platform: Linux | macOS">
    <img src="https://img.shields.io/badge/MCP-17%20tools-8A2BE2?style=flat" alt="MCP">
    <a href="Cargo.toml"><img src="https://img.shields.io/badge/Rust-1.88%2B-CE422B?style=flat&logo=rust&logoColor=white" alt="Rust"></a>
    <a href="LICENSE"><img src="https://img.shields.io/github/license/Derrors/iphone-control?style=flat" alt="License"></a>
    <a href="https://m8ven.ai/mcp/derrors-iphone-control-1pcmse?s=readme"><img src="https://m8ven.ai/badge/mcp/derrors-iphone-control-1pcmse?variant=verified" alt="M8ven verified"></a>
  </p>

  <p align="center">
    <a href="#quick-start"><strong>🚀 Quick Start</strong></a>
    &nbsp;·&nbsp;
    <a href="https://derrors.github.io/iphone-control/en/"><strong>📖 Docs</strong></a>
    &nbsp;·&nbsp;
    <a href="https://github.com/Derrors/iphone-control/releases">📦 Releases</a>
    &nbsp;·&nbsp;
    <a href="https://github.com/Derrors/iphone-control/issues">🐛 Issues</a>
    &nbsp;·&nbsp;
    <a href="README.zh-CN.md">🌐 简体中文</a>
  </p>

  <br>
  <img src="docs/images/demo-stopwatch.webp" alt="Claude Code on the left calls iphone_launch_app and iphone_tap_element; on the right, the iPhone shows the Clock stopwatch running" width="900">
  <p><sub>Asking Claude Code to “open Clock, switch to the stopwatch and start it”. Left: Claude Code’s tool calls (first part only). Right: the phone afterwards.</sub></p>
</div>

## Overview

AI agents can drive browsers and desktops, but a real iPhone is harder: Apple's automation tools need a Mac and Xcode, and the open-source alternatives come as separate pieces that each need their own setup, signing and babysitting.

iphone-control does it all in **one Rust binary** on Linux or macOS, built on [idevice](https://github.com/jkcoxson/idevice), [Impactor](https://github.com/claration/impactor) and [WebDriverAgent](https://github.com/appium/WebDriverAgent). A long-running daemon owns the phone — tunnel, HID session, WebDriverAgent session — and exposes it to Claude Code or any MCP client. It succeeds [iphone-control-toolkit](https://github.com/Derrors/iphone-control-toolkit) and keeps its MCP tools compatible.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="website/public/architecture-dark.svg">
    <img src="website/public/architecture-light.svg" alt="Architecture: an MCP client talks to iphone-control mcp over stdio, which talks to the daemon over a Unix socket; the daemon drives the iPhone or iPad over USB through WebDriverAgent, with CoreDevice HID as fallback" width="900">
  </picture>
</p>

## Features

<p align="center">
  <img src="docs/images/features.svg" alt="One binary, 17 MCP tools, fewer round trips, runs unattended, HID fallback, doctor and alerts" width="900">
</p>

- **See and touch the screen**: screenshots with region zoom, tap, swipe, long-press, text input (including Chinese), hardware buttons
- **Work with UI elements**: read the element tree, tap by text (on a part not covered by floating bars or the keyboard), launch and close apps, open URLs, wait for text to appear
- **Fewer round trips**: any action can return the resulting screen once it has settled, flagging pages still loading and blocking alerts; `iphone_batch` runs up to 20 steps in one call
- **Automatic fallback**: when WebDriverAgent is down, screenshots, taps, swipes and keys switch to Apple's CoreDevice HID service (iOS 27+)
- **Runs unattended**: recovers from replugging, phone reboots and WebDriverAgent crashes; re-signs WebDriverAgent with a free Apple ID before the 7-day profile expires
- **Alerts**: Telegram or Bark notifications when re-signing fails, the Apple ID session expires, the phone goes offline, or a passcode is needed after a reboot
- **Optional auto-unlock**: every passcode digit is verified, and a wrong passcode disables auto-unlock after one attempt
- **Diagnostics**: `doctor` checks every layer from USB to WebDriverAgent and tells you how to fix each failure

## Quick Start

You need an iPhone or iPad on iOS 17+ connected by a **data** cable, a free Apple ID (use a dedicated one), and an Apple Silicon Mac or a Linux machine.

**macOS or Linux, one command** (upstream example; for a fork, use the installer asset from its own signed release)

```bash
curl -fsSL https://github.com/Derrors/iphone-control/releases/latest/download/install.sh | sh
```

It downloads the release for your machine, verifies the Ed25519 release manifest and archive SHA-256 (Python 3 and OpenSSL 3 required), installs `iphone-control` into `~/.local/bin` and runs the daemon as a per-user service that starts at login (launchd on macOS, systemd on Linux). It never runs as root. Use `iphone-control update apply` to upgrade this managed service; `… | sh -s -- --uninstall` removes it and keeps your data.

**Or with a package manager**

```bash
# macOS or Linux (Homebrew)
brew install derrors/tap/iphone-control && brew services start iphone-control

# Debian 12+ / Ubuntu 22.04+: the .deb from Releases (amd64 or arm64), pulls in usbmuxd
sudo apt install ./iphone-control_<version>_amd64.deb && systemctl --user enable --now iphone-control
```

On Linux the system usbmuxd is needed (`sudo apt install usbmuxd`; the .deb brings it). On an Intel Mac, build from source.

**Then, on every system**

Connect the phone with the cable and run:

```bash
iphone-control
```

The first time, it walks you through setup in the terminal, one step at a time: language, proxy, the background service, pairing, Developer Mode, Apple ID, signing and installing WebDriverAgent, the optional auto-unlock and notifications, and registering with Claude Code and Codex. Do what it asks on the phone. Steps already done are skipped; afterwards, `iphone-control` just shows the status.

Then open a new Claude Code or Codex session and ask, for example, *“Open Clock, switch to the stopwatch and start it.”* Or run `iphone-control viewer` for the control page in the browser (http://127.0.0.1:8200), where you can act on the live screen and hand tasks to a local Claude Code or Codex.

More: [step-by-step guide](https://derrors.github.io/iphone-control/en/guide/getting-started) · [build from source](https://derrors.github.io/iphone-control/en/deploy/source) · [macOS guide](https://derrors.github.io/iphone-control/en/deploy/macos) · [other MCP clients](https://derrors.github.io/iphone-control/en/guide/mcp-clients).

## MCP Tools

| Tool | Description |
| --- | --- |
| `iphone_status` | Connection, lock state, foreground app, screen size |
| `iphone_screenshot` | Screenshot scaled to points; pass `x/y/width/height` to zoom into a region |
| `iphone_list_elements` | Elements with text and their center coordinates; filter by text or type |
| `iphone_tap` / `iphone_tap_element` | Tap by coordinates or by element text |
| `iphone_swipe` / `iphone_long_press` | Swipe (returns once the list stops), long-press |
| `iphone_type_text` | Type into the focused field; reads it back and fixes dropped characters |
| `iphone_press_button` | `home`, `volume_up`, `volume_down`, `lock` |
| `iphone_list_apps` / `iphone_launch_app` / `iphone_terminate_app` | Manage apps by name or bundle ID |
| `iphone_open_url` | Open a URL or URL scheme |
| `iphone_wait_for` | Wait until some text appears instead of polling with screenshots |
| `iphone_batch` | Run up to 20 steps in one call, stopping at the first error |
| `iphone_list_devices` | Every iPhone/iPad on USB; marks the one being controlled |
| `iphone_select_device` | Switch to another device, or back to automatic choice |

`IPHONE_CONTROL_DEV_TOOLS=1` adds six opt-in [developer tools](docs/tools.md#developer-tools-opt-in): logs, crash reports, installing and removing apps, and two for Web Inspector (list web pages, run JavaScript in them). They are off by default; read the [security notes](SECURITY.md#developer-tools) before turning them on.

Action tools accept `then: "elements" | "screenshot"` to return the screen after the action. Screenshot pixels are tap coordinates, so the agent can tap what it sees. Games, maps and video players draw their own UI and have no element tree — use screenshots and coordinates there. Full parameters: [docs/tools.md](docs/tools.md).

Tool texts, results, CLI output and logs are in Simplified Chinese by default; set `IPHONE_CONTROL_LANG=en` for English ([configuration](https://derrors.github.io/iphone-control/en/guide/configuration)).

## Everyday Commands

| Command | What it does |
| --- | --- |
| *(none)* | The first time, the setup guide; after that, the status (and starts the service if it isn't running) |
| `setup [--reconfigure]` | Run the setup guide again; `--reconfigure` redoes one part (language, proxy, service, Apple ID, phone, auto-unlock, notifications, MCP registration) |
| `service install\|uninstall\|start\|stop\|restart\|status` | The background service (launchd on macOS, systemd user unit on Linux) |
| `daemon` | The long-running daemon; the service runs it, or start it by hand |
| `mcp` | The MCP server; your AI client starts it |
| `status` | Daemon state, time left on the signing profile, recent log |
| `doctor` | Layer-by-layer health check with a fix for each failure |
| `login` | Sign in to the Apple ID again when the session expires |
| `sign` | Sign and install WebDriverAgent now |
| `resign [--force]` | Re-sign WebDriverAgent if it expires soon (or now) |
| `passcode set\|clear` | Turn auto-unlock on or off for a device (passcodes are kept per device) |
| `unlock-reset` | Turn auto-unlock back on after it was disabled |
| `devices` / `select <UDID\|name\|auto>` | List connected devices / switch which one is controlled |
| `notify-test [event]` | Send test notifications |
| `mcp-config` | Print the command that registers the MCP server |
| `wda-bridge [--port]` | Expose WebDriverAgent on 127.0.0.1 for other tools; stop it when done |
| `viewer [--port]` | Control page on 127.0.0.1:8200: live screen you can tap, swipe and type on, device panel, and AI tasks run by a local Claude Code or Codex; stop it when done |

All commands and options: [CLI reference](https://derrors.github.io/iphone-control/en/guide/cli).

Check in Viewer → **About & updates** or run `iphone-control update check --json`. For a managed user service, use `iphone-control update apply`. To opt in to signed patches when no device clients or tasks are active, use `iphone-control update policy auto-patch`; switch back with `iphone-control update policy manual`. Other channels upgrade with `brew upgrade iphone-control`, or by installing the new `.deb`; for a source build, see [build from source](https://derrors.github.io/iphone-control/en/deploy/source). Read the upgrade notes in the [changelog](CHANGELOG.md) first. Uninstall with `install.sh --uninstall`, `brew uninstall iphone-control` or `sudo apt remove iphone-control`; your data stays. Settings live in `~/.config/iphone-control/env`: the guide writes it, and after editing it by hand run `iphone-control service restart` — see [configuration](https://derrors.github.io/iphone-control/en/guide/configuration). All state lives in `~/.local/share/iphone-control`: the Apple ID session and signing key, and the passcode if auto-unlock is on. **Back it up; never commit or share it.**

## Documentation

| | |
| --- | --- |
| Guide | [What is it](https://derrors.github.io/iphone-control/en/guide/what-is-iphone-control) · [Limitations](https://derrors.github.io/iphone-control/en/guide/what-is-iphone-control#limitations) · [Getting started](https://derrors.github.io/iphone-control/en/guide/getting-started) · [AI clients](https://derrors.github.io/iphone-control/en/guide/mcp-clients) · [Tools](https://derrors.github.io/iphone-control/en/guide/tools) |
| Usage | [Configuration](https://derrors.github.io/iphone-control/en/guide/configuration) · [CLI](https://derrors.github.io/iphone-control/en/guide/cli) · [Several devices](https://derrors.github.io/iphone-control/en/guide/multi-device) · [Notifications](https://derrors.github.io/iphone-control/en/guide/notifications) · [Auto-unlock](https://derrors.github.io/iphone-control/en/guide/auto-unlock) |
| Deployment | [Install](https://derrors.github.io/iphone-control/en/guide/getting-started#install) · [Build from source](https://derrors.github.io/iphone-control/en/deploy/source) · [macOS](https://derrors.github.io/iphone-control/en/deploy/macos) · [Data and backups](https://derrors.github.io/iphone-control/en/deploy/backup) |
| Problems | [FAQ](https://derrors.github.io/iphone-control/en/troubleshoot/faq) · [Troubleshooting](docs/troubleshooting.md) · [Issues](https://github.com/Derrors/iphone-control/issues) |
| Security | [SECURITY.md](SECURITY.md): what is stored, who can reach it, reporting vulnerabilities |
| Development | [Contributing](CONTRIBUTING.md) · [Architecture](docs/architecture.md) · [Decisions](docs/decisions.md) · [Changelog](CHANGELOG.md) |
| Licensing | [THIRD_PARTY.md](THIRD_PARTY.md): vendored components, versions and licenses |

The architecture and design documents are in Chinese for now.

> [!WARNING]
> This tool can fully control your phone. An agent may tap payment, delete or send buttons — keep tool-call confirmation on in your MCP client. Not affiliated with Apple; use it only with devices and accounts you own. Read [SECURITY.md](SECURITY.md) before enabling auto-unlock.

## Contributing

Issues and pull requests are welcome — see the [contributing guide](CONTRIBUTING.md).

## License

[MIT](LICENSE). Third-party components: [THIRD_PARTY.md](THIRD_PARTY.md).
