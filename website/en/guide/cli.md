# CLI

Every command is `iphone-control <command>`. With Docker, prefix it with `docker exec -it iphone-control`; for a source install and on macOS, run it directly. `--help` shows help, `--version` the version. Output is in Simplified Chinese by default; set `IPHONE_CONTROL_LANG=en` for English.

## Global options

These go before the command, for example `iphone-control --socket /path/ctl.sock mcp`.

| Option | Environment variable | Meaning |
|---|---|---|
| `--data-dir <dir>` | `ICTL_DATA_DIR` | Data directory; defaults in [Configuration](./configuration#directories-and-control-socket) |
| `--socket <path>` | `ICTL_SOCKET` | The daemon's control socket; defaults in the same place |

## Running

| Command | What it does |
|---|---|
| `daemon` | The long-running daemon. The container's entry point; for a source install it's started by systemd or by hand, and on macOS you run it in a terminal |
| `mcp` | MCP stdio server, started by the AI client |
| `mcp-config [--container <name>]` | Prints the command that registers the MCP server with Claude Code. Inside a container or with `--container` it uses `docker exec`; otherwise it calls this binary directly |

## Setup and signing

| Command | What it does |
|---|---|
| `setup` | Setup wizard: pairing, Developer Mode, Apple ID sign-in, signing and installing WDA. Safe to re-run; only acts on the controlled device |
| `login [email]` | Sign in to the Apple ID. The password and code are only typed here; without an email it asks for one |
| `sign` | Sign and install WDA now, same as `resign --force`. Registers the device with the Apple ID |
| `resign [--force]` | Re-sign only when fewer than `RESIGN_MIN_DAYS` days remain; `--force` re-signs now. Without `--force` it doesn't register a device that has never had WDA |

## Checking state

| Command | What it does |
|---|---|
| `status` | State, phone, how the device was chosen, time left on the profile, auto-unlock, notifications, recent log |
| `doctor` | Layer-by-layer checks with a fix for each failure |

## Several devices

See [Several Devices](./multi-device).

| Command | What it does |
|---|---|
| `devices` | Lists devices on USB; marks the controlled one and whether it has an auto-unlock passcode |
| `select <UDID\|name\|auto>` | Control another device, remembered across restarts; `auto` goes back to automatic choice |

## Auto-unlock

See [Auto-unlock](./auto-unlock). `--device` takes a UDID or name, and the device must be connected; without it, the controlled device is used.

| Command | What it does |
|---|---|
| `passcode set [--device <device>]` | Save this device's passcode and turn auto-unlock on |
| `passcode clear [--device <device>]` | Delete this device's passcode |
| `passcode clear --all` | Delete every device's passcode |
| `unlock-reset` | Turn auto-unlock back on after it was disabled. It doesn't check the passcode, so make sure it's right first |

## Other

| Command | What it does |
|---|---|
| `notify-test [event]` | Send test notifications: one per event type, or one type (`resign`, `session`, `pin`, `pin-recovered`, `unlock`, `offline`, `online`) |
| `wda-bridge [--port <port>]` | Expose WDA on `127.0.0.1` (default port 8100) for other tools. WDA has no authentication and every local user can reach it, so stop it when done |
| `viewer [--port <port>]` | Control page in the browser at `http://127.0.0.1:8200/` (default port), in English or Chinese (follows `IPHONE_CONTROL_LANG`, switchable on the page). **Screen:** the live picture works like a touchscreen: click to tap, drag to swipe, hold to long-press, scroll with the mouse wheel or trackpad, plus a text field and the hardware buttons. On iOS 27 and later the finger follows the pointer as it moves (over CoreDevice HID, which keeps the phone awake until 30 seconds after the last touch); older versions get the whole gesture on release. Everything goes through the daemon like an MCP call, so it queues with the agent and unlocks the phone first. The picture is WDA's MJPEG stream, so there is none until WDA is ready. **Device panel:** state and readiness checks, installed apps, on-screen elements. Switching devices on the page moves the picture to the new one; after a `select` elsewhere its actions are refused until you pick the device again on the page. **AI tasks:** type a task and a local agent carries it out (Claude Code or Codex, whichever is on `PATH`; picked on the page). You can pause, take over and hand back, or stop it, and it asks on the page before paying, deleting or sending messages. Every local user can reach the port, so stop it when done |

The `debug` subcommand for diagnostics isn't compiled by default; see the [contributing guide](https://github.com/Derrors/iphone-control/blob/main/CONTRIBUTING.md#testing-on-a-real-phone).

## Notes

- **Need the daemon running:** `setup`, `status`, `doctor`, `sign`, `resign`, `devices`, `select`, `unlock-reset`, `wda-bridge`, `viewer`, and MCP tool calls.
- **Need an interactive terminal:** `setup`, `login`, `passcode set`; use `-it` with Docker.
- **Environment variables:** the CLI reads the environment of the current shell. For a source install and on macOS, only the daemon loads `~/.config/iphone-control/env`; before `login`, `setup` (if you set `ICTL_ANISETTE_URL`), `notify-test` or `mcp-config`, run `set -a; . ~/.config/iphone-control/env; set +a`.
