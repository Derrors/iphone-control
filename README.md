<a name="readme-top"></a>

<div align="right">
  <b>English</b> | <a href="README.zh-CN.md">简体中文</a>
</div>

<div align="center">
  <a href="https://github.com/Derrors/iphone-control">
    <img src="website/public/logo.svg" alt="Logo" width="120">
  </a>

  <h3 align="center">iphone-control</h3>

  <p align="center">
    Let an AI agent operate a real iPhone plugged into a Linux machine or a Mac — one Rust binary, no Xcode, no paid developer account, no jailbreak.
  </p>

  <p align="center">
    <a href="https://github.com/Derrors/iphone-control/releases/latest"><img src="https://img.shields.io/github/v/release/Derrors/iphone-control?style=flat-square" alt="Release"></a>
    <a href="https://github.com/Derrors/iphone-control/pkgs/container/iphone-control"><img src="https://img.shields.io/badge/ghcr.io-derrors%2Fiphone--control-2496ED?style=flat-square&logo=docker&logoColor=white" alt="Docker"></a>
    <img src="https://img.shields.io/badge/iOS-17%2B-black?style=flat-square&logo=apple" alt="iOS">
    <img src="https://img.shields.io/badge/platform-Linux%20%7C%20macOS-555?style=flat-square" alt="Platform: Linux | macOS">
    <img src="https://img.shields.io/badge/MCP-15%20tools-8A2BE2?style=flat-square" alt="MCP">
    <a href="Cargo.toml"><img src="https://img.shields.io/badge/Rust-1.88%2B-CE422B?style=flat-square&logo=rust&logoColor=white" alt="Rust"></a>
    <a href="https://github.com/Derrors/iphone-control/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/Derrors/iphone-control/ci.yml?branch=main&style=flat-square&label=CI" alt="CI"></a>
    <a href="LICENSE"><img src="https://img.shields.io/github/license/Derrors/iphone-control?style=flat-square" alt="License"></a>
  </p>

  <p align="center">
    <a href="#quick-start"><strong>🚀 Quick Start</strong></a>
    &nbsp;·&nbsp;
    <a href="https://derrors.github.io/iphone-control/en/"><strong>📖 Docs</strong></a>
    &nbsp;·&nbsp;
    <a href="https://github.com/Derrors/iphone-control/releases">📦 Releases</a>
    &nbsp;·&nbsp;
    <a href="https://github.com/Derrors/iphone-control/issues">🐛 Issues</a>
  </p>
</div>

> [!NOTE]
> Early release (0.x). Verified with an **iPhone 15 Pro Max (iOS 27.0.1)** on Linux and macOS (Apple Silicon), and an **iPhone 12 mini (iOS 26.4.1)** on Linux. Other models and iOS 17+ should work but are untested — reports are welcome. The CLI, `doctor` output and MCP tool results are currently in Simplified Chinese; agents handle this fine.

## Overview

AI agents can drive browsers and desktops, but a real iPhone is harder: Apple's automation tools need a Mac and Xcode, and the open-source alternatives come as separate pieces that each need their own setup, signing and babysitting.

iphone-control does it all in **one Rust binary** on Linux or macOS, built on [idevice](https://github.com/jkcoxson/idevice), [Impactor](https://github.com/claration/impactor) and [WebDriverAgent](https://github.com/appium/WebDriverAgent). A long-running daemon owns the phone — tunnel, HID session, WebDriverAgent session — and exposes it to Claude Code or any MCP client. It succeeds [iphone-control-toolkit](https://github.com/Derrors/iphone-control-toolkit) and keeps its MCP tools compatible.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="website/public/architecture-dark.svg">
    <img src="website/public/architecture-light.svg" alt="Architecture: an MCP client talks to iphone-control mcp over stdio, which talks to the daemon over a Unix socket; the daemon drives the iPhone over USB through WebDriverAgent, with CoreDevice HID as fallback" width="900">
  </picture>
</p>

## Features

- **See and touch the screen**: screenshots with region zoom, tap, swipe, long-press, text input (including Chinese), hardware buttons
- **Work with UI elements**: read the element tree, tap by text (on a part not covered by floating bars or the keyboard), launch and close apps, open URLs, wait for text to appear
- **Fewer round trips**: any action can return the resulting screen once it has settled, flagging pages still loading and blocking alerts; `iphone_batch` runs up to 20 steps in one call
- **Automatic fallback**: when WebDriverAgent is down, screenshots, taps, swipes and keys switch to Apple's CoreDevice HID service (iOS 27+)
- **Runs unattended**: recovers from replugging, phone reboots and WebDriverAgent crashes; re-signs WebDriverAgent with a free Apple ID before the 7-day profile expires
- **Alerts**: Telegram or Bark notifications when re-signing fails, the Apple ID session expires, the phone goes offline, or a passcode is needed after a reboot
- **Optional auto-unlock**: every passcode digit is verified, and a wrong passcode disables auto-unlock after one attempt
- **Diagnostics**: `doctor` checks every layer from USB to WebDriverAgent and tells you how to fix each failure

## Quick Start

On Linux you need a host (x86_64 or arm64) with Docker (Compose v2), an iPhone on iOS 17+ connected by a **data** USB cable, and a free Apple ID (use a dedicated one, not your main account).

```bash
git clone https://github.com/Derrors/iphone-control.git
cd iphone-control
cp .env.example .env                                  # optional: proxy, notifications
docker compose up -d --build                          # the first build compiles Rust, a few minutes
docker exec -it iphone-control iphone-control setup   # guided setup; safe to re-run
claude mcp add iphone -s user -- docker exec -i iphone-control iphone-control mcp
```

`setup` walks you through pairing, Developer Mode, signing in to your Apple ID, and signing and installing WebDriverAgent. On the phone you tap *Trust*, turn on Developer Mode, and trust your Apple ID under *Settings → General → VPN & Device Management* — see the [step-by-step guide](https://derrors.github.io/iphone-control/en/guide/getting-started).

Open a new Claude Code session and ask, for example:

> Open the Clock app, switch to the stopwatch, and show me a screenshot.

> [!TIP]
> Other MCP clients: the server speaks stdio through `docker exec` and opens no network port.
>
> ```json
> { "mcpServers": { "iphone": { "command": "docker", "args": ["exec", "-i", "iphone-control", "iphone-control", "mcp"] } } }
> ```

You can also skip both the clone and the build: in a new directory, write a `compose.yaml` that uses the prebuilt image `ghcr.io/derrors/iphone-control:latest` and run `docker compose up -d` — see [Docker](https://derrors.github.io/iphone-control/en/deploy/docker#just-a-compose-file) for the full example. If you already cloned the repository, set `IMAGE=ghcr.io/derrors/iphone-control:latest` in `.env` and run `docker compose pull && docker compose up -d --no-build`. Without Docker, use a [prebuilt binary or build from source](https://derrors.github.io/iphone-control/en/deploy/source) and run it as a systemd user service.

### macOS

Docker on a Mac can't pass USB devices through to containers, so on a Mac (Apple Silicon; no Xcode needed) run the binary directly, as your regular user. Download it from [Releases](https://github.com/Derrors/iphone-control/releases) (`…-macos-arm64.tar.gz`):

```bash
V=v0.1.2   # latest version
curl -fsSL https://github.com/Derrors/iphone-control/releases/download/$V/iphone-control-$V-macos-arm64.tar.gz | tar xz
mkdir -p ~/.local/bin && install -m 755 iphone-control-$V-macos-arm64/iphone-control ~/.local/bin/
export PATH="$HOME/.local/bin:$PATH"                # add this line to ~/.zprofile too
iphone-control daemon                               # keep this terminal open
iphone-control setup                                # in a second terminal
iphone-control mcp-config                           # prints the `claude mcp add` command to run
```

Downloaded with a browser instead of `curl`? Gatekeeper blocks the unnotarized binary; run `xattr -d com.apple.quarantine ~/.local/bin/iphone-control`. To build from source instead: `cargo install --path apps/iphone-control --locked` in a clone (needs the Xcode Command Line Tools and Rust 1.88+).

Set the phone's charging limit to 100% (*Settings → Battery → Charging*): with the 80% limit, USB drops about once a minute. Configuration, keeping it running and the known differences are in the [macOS guide](https://derrors.github.io/iphone-control/en/deploy/macos).

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

Action tools accept `then: "elements" | "screenshot"` to return the screen after the action. Screenshot pixels are tap coordinates, so the agent can tap what it sees. Games, maps and video players draw their own UI and have no element tree — use screenshots and coordinates there. Full parameters: [docs/tools.md](docs/tools.md).

## Everyday Commands

Run them as `docker exec -it iphone-control iphone-control <command>`, or directly for a source install and on macOS.

| Command | What it does |
| --- | --- |
| `status` | Daemon state, time left on the signing profile, recent log |
| `doctor` | Layer-by-layer health check with a fix for each failure |
| `setup` | Guided setup; re-run any time |
| `login` | Sign in to the Apple ID again when the session expires |
| `resign [--force]` | Re-sign WebDriverAgent if it expires soon (or now) |
| `passcode set\|clear` | Turn auto-unlock on or off |
| `notify-test [event]` | Send test notifications |
| `mcp-config` | Print the command that registers the MCP server |

Upgrade with `git pull && docker compose up -d --build` (or `docker compose pull` for the prebuilt image). Settings live in `.env` — see [configuration](https://derrors.github.io/iphone-control/en/guide/configuration). All state lives in `data/`: pairing records, the Apple ID session and signing key, and the passcode if auto-unlock is on. **Back it up; never commit or share it.**

## Documentation

| | |
| --- | --- |
| Guide | [What is it](https://derrors.github.io/iphone-control/en/guide/what-is-iphone-control) · [Getting started](https://derrors.github.io/iphone-control/en/guide/getting-started) · [AI clients](https://derrors.github.io/iphone-control/en/guide/mcp-clients) · [Tools](https://derrors.github.io/iphone-control/en/guide/tools) |
| Usage | [Configuration](https://derrors.github.io/iphone-control/en/guide/configuration) · [Notifications](https://derrors.github.io/iphone-control/en/guide/notifications) · [Auto-unlock](https://derrors.github.io/iphone-control/en/guide/auto-unlock) |
| Deployment | [Docker](https://derrors.github.io/iphone-control/en/deploy/docker) · [Source / binary](https://derrors.github.io/iphone-control/en/deploy/source) · [macOS](https://derrors.github.io/iphone-control/en/deploy/macos) · [Data and backups](https://derrors.github.io/iphone-control/en/deploy/backup) |
| Problems | [FAQ](https://derrors.github.io/iphone-control/en/troubleshoot/faq) · [Troubleshooting](docs/troubleshooting.md) · [Issues](https://github.com/Derrors/iphone-control/issues) |
| Security | [SECURITY.md](SECURITY.md): what is stored, who can reach it, reporting vulnerabilities |
| Development | [Contributing](CONTRIBUTING.md) · [Architecture](docs/architecture.md) · [Decisions](docs/decisions.md) · [Changelog](CHANGELOG.md) |
| Licensing | [THIRD_PARTY.md](THIRD_PARTY.md): vendored components, versions and licenses |

The architecture and design documents are in Chinese for now.

## Limitations

- One phone per host; don't run two instances (or other usbmuxd-based tools) against the same phone
- Free Apple ID profiles last 7 days. Re-signing is automatic, but an expired Apple ID session needs `login`
- After the phone reboots, you must unlock it and enter the passcode once to allow UI automation — iOS requires this
- Apple ID logins use a public third-party anisette server by default; set `ICTL_ANISETTE_URL` to use your own
- The container runs as root: it needs the USB device nodes and must restart usbmuxd after replugging
- The HID fallback needs iOS 27 or later; on older iOS everything goes through WebDriverAgent
- On macOS: Apple Silicon only (Intel Macs untested, no prebuilt binary), the binary isn't notarized, run the daemon in a terminal (no launchd service yet), no Docker (no USB passthrough); data lives in `~/.local/share/iphone-control`

> [!WARNING]
> This tool can fully control your phone. An agent may tap payment, delete or send buttons — keep tool-call confirmation on in your MCP client. Not affiliated with Apple; use it only with devices and accounts you own. Read [SECURITY.md](SECURITY.md) before enabling auto-unlock.

## Contributing

Issues and pull requests are welcome — see the [contributing guide](CONTRIBUTING.md).

## License

[MIT](LICENSE). Third-party components: [THIRD_PARTY.md](THIRD_PARTY.md).

<p align="right">(<a href="#readme-top">back to top</a>)</p>
