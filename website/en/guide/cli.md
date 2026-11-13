# CLI

Every command is `iphone-control <command>`. `--help` shows help, `--version` the version. Output is in Simplified Chinese by default; set `IPHONE_CONTROL_LANG=en` for English (the first-run guide asks).

`iphone-control` with no command runs the [first-run guide](./getting-started#first-run) the first time; once set up, it shows the status (and starts the daemon first if it isn't running).

## Global options

These go before the command, for example `iphone-control --socket /path/ctl.sock mcp`.

| Option | Environment variable | Meaning |
|---|---|---|
| `--data-dir <dir>` | `ICTL_DATA_DIR` | Data directory; defaults in [Configuration](./configuration#directories-and-control-socket) |
| `--socket <path>` | `ICTL_SOCKET` | The daemon's control socket; defaults in the same place |

## Running

| Command | What it does |
|---|---|
| `daemon` | The long-running daemon. Usually started by the background service; you can also run it in a terminal |
| `mcp` | MCP stdio server, started by the AI client |
| `mcp-config` | Prints the command that registers the MCP server with Claude Code, with a program path that stays the same across upgrades (Homebrew's `opt` link) |

## Host software updates

Run `iphone-control update` to check the publisher's latest stable release. It reports when you are up to date. When an upgrade is available, it first says what it will do (from which version to which, and whether the background service restarts), asks whether to continue, then upgrades through the installation method of the running executable. In a script or without a terminal, add `--yes` to upgrade without asking.

| Installation | What `iphone-control update` does |
| --- | --- |
| Managed per-user service installed by the signed release script | Verifies the publisher's signature, installs the update, then restarts the service and checks its health; if the new version doesn't start within 60 seconds, the old one is put back. Close Viewer, MCP and other device clients first. If you pinned a version with `--version`, run `iphone-control update unpin` when you are ready to upgrade. |
| Homebrew | Runs the Homebrew upgrade through the tap bound to this installation. The tap determines which version is available. A running `brew services` service is restarted afterwards. |
| Debian / Ubuntu package | Downloads the matching `.deb` for the target version and host architecture from the same publisher, checks the publisher's signature for it, then installs it with apt through `sudo` (asks for your password). The package's systemd user service is restarted afterwards if it was running. A package held with `apt-mark hold` isn't upgraded; release it with `sudo apt-mark unhold iphone-control` first. |
| Source builds, script installs without a managed service, or unrecognized installations | Shows instructions for a manual upgrade. A development build without a publisher displays guidance locally. |

Restarting the background service interrupts running AI tasks and the control page's picture. After a Homebrew or apt upgrade the command checks that the service now runs the new version; if it can't confirm that, it prints the restart command to run by hand. Reopen Viewer and reconnect your MCP client to use the new executable there too.

Use `iphone-control update check` for a version check only, or `iphone-control update check --json` for machine-readable output. This check and **About & updates** in Viewer share the local release cache. If the network check fails, the previous successful record is kept. A red dot on the ⓘ button beside **Settings** indicates a newer release. Open the panel to read the release notes and copy the update command for the current executable into a local terminal.

Viewer listens on loopback. Other users on the same host can read installation information and request a version check. Upgrades and policy changes are performed from a local terminal.

Managed services check in the background by default. `update apply` installs without asking (managed services), and `update recover` finishes an interrupted replacement. There is no command to go back to the previous version: to run an older one, uninstall with the release's install script and `--uninstall` (settings and device data are kept), then install that version with `--version vX.Y.Z`. If you currently run the 0.2.2 CLI, use `iphone-control update check` and follow the command it shows to upgrade to a version with the new `iphone-control update` command.

Run `iphone-control update policy auto-patch` to opt in to installing compatible patches when idle. Viewer, MCP and direct device clients (including paused tasks) must exit before the service can restart. If the new daemon fails its health checks the updater restores the old executable and holds the failed version for an explicit retry. `update policy notify` checks without installing; `update policy manual` removes the scheduled job.

If the new executable cannot start, invoke `<prefix>/bin/.iphone-control-update/recovery update recover` directly. For a custom socket, add `--socket <path>` before `update`.

## Background service

The daemon runs as a per-user service: launchd on macOS, a systemd user service on Linux. It starts when you log in and restarts after a crash. No root needed.

| Command | What it does |
|---|---|
| `service install` | Set up the service and start it; if one exists, just start it |
| `service uninstall` | Stop and remove the service this program set up; data is kept. A service that came with Homebrew or the .deb isn't removed: it is disabled, or you're told how to stop it |
| `service start` / `stop` | Start or stop it |
| `service restart` | Restart it, after editing `~/.config/iphone-control/env` |
| `service status` | Which service is set up, and whether the daemon is running |

These commands find the service set up by the one-command install, `iphone-control service install`, `brew services` or the .deb, and act on whichever is there.

## Setup and signing {#setup}

| Command | What it does |
|---|---|
| `setup` | Runs the whole guide again: language, proxy, background service, pairing, Developer Mode, Apple ID, signing and installing WDA, auto-unlock, notifications, MCP registration. Done steps are skipped. The phone part only acts on the controlled device |
| `setup --reconfigure` | Set up one part again, picked from a menu: all of it, language, proxy, background service, Apple ID (sign in again and re-sign WDA), phone (pairing, Developer Mode, WDA), auto-unlock passcode, notifications, MCP registration (Claude Code, Codex) |
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
| `viewer [--port <port>]` | Control page in the browser at `http://127.0.0.1:8200/` (default port), in English or Chinese (follows `IPHONE_CONTROL_LANG`, switchable on the page). **Screen:** the live picture works like a touchscreen: click to tap, drag to swipe, hold to long-press, scroll with the mouse wheel or trackpad, plus a text field and the hardware buttons. On iOS 27 and later the finger follows the pointer as it moves (over CoreDevice HID, which keeps the phone awake until 30 seconds after the last touch); older versions get the whole gesture on release. Everything goes through the daemon like an MCP call, so it queues with the agent and unlocks the phone first. On iOS 27 and later the picture is the phone's own video; otherwise it comes from WDA (none until WDA is ready), as H.264 or MJPEG; see [Picture](#viewer-picture) below. Touch is available when the frame matches the measured size and orientation. After rotating or switching devices, start a new gesture on the updated picture. Settings at the bottom of the sidebar controls the theme and text size. The picture has its own zoom controls, the − and + above it (pinching doesn't zoom, so a drag can't turn into one). **Device panel:** state and readiness checks, installed apps, on-screen elements. The page follows the device the daemon controls: switch on the page or anywhere else, and the picture and panels move over; see [Several Devices](./multi-device#control-page). **AI tasks:** type a task and a local agent carries it out (Claude Code or Codex, whichever is on `PATH`; picked on the page). You can pause, take over and hand back, or stop it, and it asks on the page before paying, deleting or sending messages. Every local user can reach the port, so stop it when done |
| `task run [--title <title>] [--app <app>…] -- <command>` | AI tasks for other agent platforms: opens a task on the controlled device and runs the command in it. An `iphone-control mcp` the command starts takes part (through `IPHONE_CONTROL_TASK_TOKEN`): the control page shows every step and can pause, take over and stop it, and it waits for you before paying or sending a message. The task ends when the command does; when the task is stopped elsewhere, so is the command; Ctrl-C stops the task. While a task runs, other clients can look but not act on the phone. `--app` (repeatable, name or bundle ID) limits the task to those apps: in any other app, or to open a link that isn't a web page, each step waits for you first |
| `task list` / `task events <id>` | Recent tasks (`--json` for programs); follow a task's events, one JSON per line |
| `task pause\|resume\|takeover\|handback\|stop <id>`, `task approve\|reject <id>` | Control a task from the command line, like the buttons on the control page |

The `debug` subcommand for diagnostics isn't compiled by default; see the [contributing guide](https://github.com/Derrors/iphone-control/blob/main/CONTRIBUTING.md#testing-on-a-real-phone).

## Notes

- **Need the daemon running:** the phone part of `setup`, `status`, `doctor`, `sign`, `resign`, `devices`, `select`, `unlock-reset`, `wda-bridge`, `viewer`, `task`, and MCP tool calls.
- **Need an interactive terminal:** `iphone-control` with no command (the first time), `setup`, `login`, `passcode set`.
- **Settings:** every command reads `~/.config/iphone-control/env` when it starts; a variable already in the environment wins. See [Configuration](./configuration#settings-file).

## The control page's picture {#viewer-picture}

**iOS 27 and later**: the picture is the phone's own hardware-encoded video (the CoreDevice screen media stream, HEVC), about 60 frames a second at native resolution; nothing is encoded on this machine and WDA doesn't capture anything. It takes 150–600 KB/s while scrolling, and a little even on a still screen (about 15 KB/s on a white page, 250 KB/s on a busy one). When the screen turns, the page turns the picture with it within about 1–2 seconds. The browser has to decode HEVC: Chrome and Safari on a Mac can; Firefox and Chrome on most Linux machines can't and fall back to H.264 or MJPEG as below. While you watch, the phone doesn't auto-lock and the Dynamic Island shows it is being viewed; that ends 30 seconds after the page closes.

**Below iOS 27, or a browser without HEVC**:
On macOS, and on Linux with ffmpeg installed, the control page's picture is H.264; otherwise MJPEG. The phone's picture is encoded on this machine (on macOS by VideoToolbox, called by this program, no ffmpeg needed; on Linux by ffmpeg) and decoded in the browser with WebCodecs (the browser picks hardware decoding itself). For the same picture it is about 1/15 of MJPEG's traffic: next to nothing on a still screen and about 300–450 KB/s while scrolling, against 4–10 MB/s for MJPEG. The delay is within a frame of MJPEG's.

- **Encoder:** `viewer` tries them in order when it starts, uses the first that works and says which in the terminal: NVENC on NVIDIA; VAAPI on AMD and Intel (the JPEG is decoded by the GPU too); QSV on Intel as well; V4L2 on ARM boards such as the Raspberry Pi; VideoToolbox on macOS (called directly, the JPEG decoded by the hardware too; with ffmpeg installed, its VideoToolbox is the fallback); otherwise x264 in software (uses the CPU). One that fails on the real picture is replaced by the next. On Linux the user must be in the `render` group to use the GPU; `doctor` checks it.
- **Slow links:** the page tells viewer every 100 ms how much it has received. When the link can't keep up, fewer frames are encoded: the frame rate drops, but the picture doesn't fall further and further behind. On a 200 KB/s link, for example, the picture catches up about 0.6 s after scrolling stops.
- **Falling back to MJPEG:** on Linux without ffmpeg, without a working encoder, or in a browser without WebCodecs (older ones), the page uses MJPEG. It no longer repeats the same frame while the screen is still, and on a slow link it only sends the newest frame.
- **Which one is in use:** shown under the picture, e.g. "Live · WDA · HEVC iPhone 312 KB/s" (the phone's own video) or "Live · WDA · H.264 VAAPI 254 KB/s".
- **Settings:** `IPHONE_CONTROL_VIEWER_CODEC` (`device`: only the phone's own video, `mjpeg`: only MJPEG), `IPHONE_CONTROL_VIEWER_QP`, `ICTL_FFMPEG`, see [Configuration](./configuration). On Linux install ffmpeg with `sudo apt install ffmpeg`; the .deb recommends it. Nothing to install on macOS.
