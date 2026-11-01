# iphone-control

**English** | [简体中文](README.zh-CN.md)

[![CI](https://github.com/Derrors/iphone-control/actions/workflows/ci.yml/badge.svg)](https://github.com/Derrors/iphone-control/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Control a real, USB-connected iPhone from Linux — no Mac, no paid Apple developer account — and hand it to AI agents as [MCP](https://modelcontextprotocol.io) tools. One Rust binary plus the system `usbmuxd`.

> **Status: early (0.x).** Everything below works end to end on one device: iPhone 15 Pro Max, iOS 27.0.1. Other models and iOS 17+ should work but are untested — reports are welcome.
>
> The CLI, `doctor` output and MCP tool results are currently in **Simplified Chinese**. Agents handle this fine; a human reading the logs may need a translator.

## What it does

- **Agent tools** — screenshots (with zoom-in regions), on-screen element lists, tap by coordinates or by text, swipe, long press, typing (including Chinese), hardware buttons, launch/terminate apps, open URLs, wait for text to appear, and batches of steps. Any action can also return the resulting screen, saving the agent a round trip. See [docs/tools.md](docs/tools.md).
- **Keeps itself running** — pairs with the phone, mounts the Developer Disk Image, signs WebDriverAgent with your free Apple ID and re-signs it before the 7-day profile expires, restarts WDA when it dies, and recovers after the phone is unplugged or rebooted.
- **Fallback path** — when WDA is down, taps, swipes, key presses and screenshots still work through CoreDevice HID.
- **Tells you when it needs you** — Bark or Telegram notifications for a failed re-sign, an expired Apple ID session, a reboot that needs the passcode, or the phone going offline.
- **Optional auto-unlock** — off by default; read [SECURITY.md](SECURITY.md) before turning it on.
- **No open ports** — WDA is reached through usbmuxd; nothing listens on the network.

```
AI agent ──stdio──▶ iphone-control mcp ──unix socket──▶ iphone-control daemon ──usbmuxd/USB──▶ iPhone (WebDriverAgent)
```

One long-running daemon owns the device (tunnel, HID session, WDA session); each agent session gets a thin MCP client. Details: [docs/architecture.md](docs/architecture.md) (Chinese).

## Requirements

- A Linux host (x86_64 or arm64) with a USB port.
- Docker with Compose v2 — or, without Docker, Rust 1.88+, a C compiler and `usbmuxd` ([install from source](#install-from-source-without-docker)).
- An iPhone on iOS 17 or later with a data cable. Setup walks you through Developer Mode.
- A free Apple ID. Using a dedicated one is recommended (see [Security](#security-and-privacy)).
- Internet access to Apple, to the anisette server, and to GitHub (the Developer Disk Image and WebDriverAgent are downloaded on first use).

## Quick start (Docker)

```bash
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cp .env.example .env              # optional: proxy, notifications, auto-unlock settings
docker compose up -d --build      # the first build compiles Rust and takes a few minutes
docker exec -it iphone-control iphone-control setup
docker exec -it iphone-control iphone-control mcp-config   # prints the command to register the MCP server
```

Once a release is published you can skip the build: set `IMAGE=ghcr.io/derrors/iphone-control:latest` in `.env`, then run `docker compose pull && docker compose up -d --no-build`.

`setup` can be re-run at any time. It walks through:

1. **Connect** the phone and unlock it.
2. **Pair** — tap *Trust* on the phone and enter the passcode.
3. **Developer Mode** — *Settings → Privacy & Security → Developer Mode → On*, restart, then confirm after unlocking.
4. **Apple ID login** — the password and two-factor code are typed into this terminal only.
5. **Sign and install WebDriverAgent** — keep the phone unlocked.
6. **Start WebDriverAgent** — the first time, trust your Apple ID under *Settings → General → VPN & Device Management*. If the phone asks you to *enter the iPhone passcode for "XCTest"* (Enable UI Automation), enter it.

The container is not `privileged`: it only gets USB character devices (`device_cgroup_rules: "c 189:* rmw"`) plus a bind mount of `/dev/bus/usb`. Don't run two copies (or another usbmuxd-based tool) against the same phone.

## Connect an MCP client

Claude Code (this is what `mcp-config` prints for the container):

```bash
claude mcp add iphone -s user -- docker exec -i iphone-control iphone-control mcp
```

Other clients (Claude Desktop, Cursor, …) take the same command in their JSON config:

```json
{
  "mcpServers": {
    "iphone": { "command": "docker", "args": ["exec", "-i", "iphone-control", "iphone-control", "mcp"] }
  }
}
```

For a source install, use `iphone-control --socket <path> mcp` instead; `iphone-control mcp-config` prints the exact command.

## Install from source (without Docker)

Debian/Ubuntu example:

```bash
sudo apt install build-essential git usbmuxd     # udev starts usbmuxd when the phone is plugged in
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked  # installs ~/.cargo/bin/iphone-control
iphone-control daemon      # foreground; see below for a systemd user service
iphone-control setup       # in a second terminal
iphone-control mcp-config
```

To keep it running, use the systemd user service in [deploy/iphone-control.service](deploy/iphone-control.service):

```bash
mkdir -p ~/.config/systemd/user ~/.config/iphone-control
cp deploy/iphone-control.service ~/.config/systemd/user/
cp .env.example ~/.config/iphone-control/env      # CONTAINER_NAME, IMAGE and DATA_DIR are compose-only
systemctl --user daemon-reload && systemctl --user enable --now iphone-control
loginctl enable-linger "$USER"                    # start at boot without logging in
```

| | Container | Source install (regular user) |
|---|---|---|
| Data directory | `./data` (`/var/lib/iphone-control` inside) | `~/.local/share/iphone-control` |
| Control socket | `/run/iphone-control/ctl.sock` inside the container | `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` |
| usbmuxd | started and supervised by the daemon (no udev in the container) | the system service; the daemon does not start it |
| Pairing records | `lockdown/` in the data directory | the system's `/var/lib/lockdown` |
| Unsigned WDA | shipped in the image | downloaded (sha256-checked) before the first signing |
| Configuration | `.env` | environment variables, e.g. the systemd `EnvironmentFile` |

Run the daemon, the CLI and the MCP server as the same user. With a host `usbmuxd` installed, `sudo systemctl mask usbmuxd` before going back to the container, or the two will fight over the phone.

## Commands

| Command | Purpose |
|---|---|
| `daemon` | The long-running daemon (container entry point) |
| `mcp` | MCP stdio server; talks to the daemon over the control socket |
| `mcp-config` | Print the command that registers the MCP server with Claude Code |
| `status` / `doctor` | State snapshot and recent log / layer-by-layer diagnosis with fixes |
| `setup` | First-run wizard (re-runnable) |
| `login` | Log in to an Apple ID (no echo; two-factor and SMS supported) |
| `sign` / `resign [--force]` | Sign and install WDA now / re-sign only if the profile is close to expiring |
| `passcode set\|clear`, `unlock-reset` | Auto-unlock (off by default) |
| `wda-bridge --port 8100` | Expose WDA on 127.0.0.1 for other tools — unauthenticated, stop it when done |
| `notify-test` | Send one test notification per event type |

## Configuration

Everything is configured through environment variables; all of them are optional. See [.env.example](.env.example) for the full list. The common ones:

| Variable | Meaning |
|---|---|
| `HTTP_PROXY`, `HTTPS_PROXY` | Proxy for Apple, anisette, Developer Disk Image and notification traffic |
| `IPHONE_UDID` | Which phone to use when several are connected |
| `BARK_KEY`, `BARK_SERVER`, `TG_BOT_TOKEN`, `TG_CHAT_ID` | Notifications |
| `RESIGN_MIN_DAYS`, `RESIGN_CHECK_HOURS` | When to re-sign WebDriverAgent |
| `ICTL_ANISETTE_URL` | Your own anisette server instead of the default third-party one |
| `ICTL_DDI_URL`, `ICTL_DDI_DIR` | Where the Developer Disk Image comes from |

## Security and privacy

Read [SECURITY.md](SECURITY.md) before deploying. In short:

- The data directory holds your Apple ID session, the development signing key, pairing records and — only if you enable auto-unlock — the phone passcode in plain text. Back it up, and never commit or share it.
- The container runs as root, because it needs the USB device nodes and must be able to restart `usbmuxd` after the phone is unplugged.
- Apple ID logins use an anisette server. The default is a public third-party server; set `ICTL_ANISETTE_URL` to use your own.
- Anyone who can reach the control socket, or a running `wda-bridge`, can control the phone.

## Disclaimer

This project is not affiliated with, endorsed by, or sponsored by Apple Inc. iPhone, iOS and Xcode are trademarks of Apple Inc.

Use it only with devices and accounts you own or are authorized to control. Automating third-party apps may violate their terms of service, and Apple may limit or suspend developer accounts — consider a dedicated Apple ID. The software is provided "as is", without warranty of any kind (see [LICENSE](LICENSE)).

Nothing here bypasses iOS security: trusting the computer, enabling Developer Mode, trusting the developer certificate and enabling UI automation all require the owner on the phone.

## Documentation

| Document | Contents |
|---|---|
| [docs/tools.md](docs/tools.md) | MCP tools and their parameters |
| [docs/troubleshooting.md](docs/troubleshooting.md) | What each state means and how to fix common problems |
| [SECURITY.md](SECURITY.md) | Threat model, hardening, reporting vulnerabilities |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Building, testing (with and without a phone), pull requests |
| [CHANGELOG.md](CHANGELOG.md) | Release notes |
| [docs/architecture.md](docs/architecture.md) | Architecture, daemon state machine, modules, data layout (Chinese) |
| [docs/decisions.md](docs/decisions.md) | Key design decisions (Chinese) |
| [docs/licensing.md](docs/licensing.md), [THIRD_PARTY.md](THIRD_PARTY.md) | License analysis and third-party components (Chinese) |
| [docs/dev/](docs/dev/) | Development history: spike results, roadmap, regression cases (Chinese) |

## Acknowledgements

Built on [idevice](https://github.com/jkcoxson/idevice) (device protocols), [Impactor](https://github.com/claration/impactor) (Apple ID and signing), [apple-codesign](https://github.com/indygreg/apple-platform-rs), [WebDriverAgent](https://github.com/appium/WebDriverAgent) (on-device automation), [usbmuxd](https://github.com/libimobiledevice/usbmuxd) and the [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk).

## License

The code in this repository is [MIT](LICENSE). Vendored components keep their own licenses (MPL-2.0 and MIT); see [THIRD_PARTY.md](THIRD_PARTY.md). Apple files such as the Developer Disk Image are downloaded at runtime and never distributed.
