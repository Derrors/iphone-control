# Changelog

**English** | [简体中文](CHANGELOG.zh-CN.md)

All notable changes to this project are documented here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/) (while at 0.x, minor versions may contain breaking changes).

## [Unreleased]

### Added

- Named login tokens, read-only ones among them: `iphone-control viewer token add <name> [--read-only]`, `list`, `show <name> [--new]`, `remove <name>`. A read-only token can watch the picture, the device's state and tasks; every action (taps, typing, starting, pausing or confirming tasks, switching devices, `/mcp`) is refused, and the page hides them. The task log records which token controlled it (`viewer:<name>`). Tokens moved to `viewer-tokens.json` and the earlier one became `owner`, so nobody logs in again after upgrading; adding, replacing and removing tokens takes effect at once, without restarting the viewer.
- Tasks opened from another machine: `iphone-control task run --url https://<that machine>:8200 -- <command>` (the login token in `IPHONE_CONTROL_VIEWER_TOKEN`), or `POST /api/agent/tasks/open` directly, whose event stream is the lease. The agent on the other machine gets only the task's token: `/mcp` takes the token of an active task (header `Iphone-Control-Task-Token`), so it can act on the phone for that task but can't reach pausing or confirming, and can't approve itself.
- The viewer and its `/mcp` need a login, from this machine too (the viewer can't tell a local browser from a visit forwarded by frp, cloudflared or another reverse proxy). The token is made on the first start and kept in `viewer-token` in the data directory (0600); `iphone-control viewer token` shows it with links that log in here and on the local network and the command to connect to the HTTP MCP, and `--new` replaces it. A browser stays logged in for 30 days; programs and MCP clients send `Authorization: Bearer <token>`. After too many wrong tokens (20 in a minute) tokens aren't checked for a minute. The CLI, `iphone-control mcp` (stdio) and `task run` use the local Unix socket and are unaffected.
- MCP over HTTP: `/mcp` (Streamable HTTP) on the viewer's port serves the same tools as `iphone-control mcp`, for clients that only take a URL and for agents on another computer on the local network (`--lan`, over HTTPS), e.g. `claude mcp add --transport http iphone http://127.0.0.1:8200/mcp`. A task's agent sends the task's token in the `Iphone-Control-Task-Token` header. No login, as for the viewer; `--mcp off` (`IPHONE_CONTROL_VIEWER_MCP=off`) turns it off.
- The viewer opened to the local network (`--lan`) now speaks HTTPS, on the same port: opened from another machine the picture can be H.264 or the phone's own HEVC instead of MJPEG, and traffic is encrypted. It uses a self-signed certificate made on this machine (kept in `viewer-tls/` in the data directory and reused); the browser warns the first time, so check the SHA-256 fingerprint shown at startup or by `service status` and go on. `--tls-cert`/`--tls-key` (`IPHONE_CONTROL_VIEWER_TLS_CERT`/`_KEY`) use a certificate of your own. `http://` from the network is redirected to `https://`; `http://127.0.0.1` on this machine works as before. `--tls off` (`IPHONE_CONTROL_VIEWER_TLS=off`) goes back to HTTP. It isn't a login: anyone on the network who can reach the page can still control the phone.
- `iphone-control service viewer install` runs the viewer as a background service of its own that starts at login (`uninstall`, `start`, `stop`, `restart` too). It reads its port and whether it is open to the local network from the settings file (`IPHONE_CONTROL_VIEWER_PORT`, new; `IPHONE_CONTROL_VIEWER_LAN`; `IPHONE_CONTROL_VIEWER_LAN_HOSTS`, new) and keeps the installing shell's `PATH` to find `claude` and `codex`. Updates stop it and start it again; an automatic update waits while an AI task runs.
- An AI task's agent can no longer pause, resume, take over, hand back or confirm its own task, so only a person answers out-of-scope steps and `iphone_confirm`. Control requests carrying the task's token (`IPHONE_CONTROL_TASK_TOKEN`) are refused, and so are those from an agent the control page or `task run` started, or any process it starts, even without the token or through the page's API. On macOS, calls through the page's API can't be traced to a process yet. Stopping a task is always allowed. The task's log records who controlled it (the page, the command line, the task's opener).
- Navigate setup menus with arrow keys, confirm with Enter, or cancel with Esc. Number selection remains available, and basic terminals keep the line-by-line prompts.
- Generate Bash, Zsh and Fish completion scripts with `iphone-control completions <shell>`, without starting the daemon or accessing a device.
- `iphone-control viewer --lan` (or `IPHONE_CONTROL_VIEWER_LAN=1`) also opens the control page to the local network, on the same port. There is no login yet: anyone on the network who can reach the machine can control the phone, so only use it on a trusted network. It only accepts connections from local-network addresses and only answers to this machine's own names; `--lan-host` adds another name. Opened at a network address the picture is MJPEG, since browsers only offer WebCodecs on localhost or HTTPS.
- On the control page, a task can be limited to some apps before it starts (the App button next to the agent): in any other app each step waits for you, as with `task run --app`. The task card shows the scope, and running a task again keeps it.
- AI tasks for any agent platform: `iphone-control task run -- <command>` runs a command in a task, and an `iphone-control mcp` it starts takes part (the control page shows its steps and can pause, take over, stop and confirm). `task list`, `task events`, `task pause`/`resume`/`takeover`/`handback`/`stop` and `task approve`/`reject` follow and steer tasks from the command line.
- The control page picks up a task in progress: after a reload, and for tasks started elsewhere.
- A task can be limited to some apps (`task run --app Settings`): in any other app, launching it, or opening a link that isn't a web page, each step waits for you to confirm; approving lets the task into that app.

### Changed

- On the control page the picture zooms only with the − and + buttons above it: pinching on a trackpad or touch screen, or Ctrl/⌘ + scroll, no longer zooms it (a drag easily turned into a zoom).
- AI tasks are kept by the daemon instead of the control page. While a task runs, other clients (another MCP server, the command line) can look, but their actions and device switches are refused until the user takes over; the task stays on the device it started on. Tasks are logged in the data directory (`tasks/`, the last 50); one that was running when the daemon stopped shows as interrupted.

### Removed

- The `ICTL_AGENT_GATE` / `ICTL_AGENT_TOKEN` variables of the MCP server: a task's MCP server gets `IPHONE_CONTROL_TASK_TOKEN` instead.

### Fixed

- Checking a new version during an update (running its `--version`) could fail now and then with "Text file busy": another thread started a process while the new program was still open for writing. It now waits a moment and tries again.
- `task run`: a command that ended its task with `task finish` is no longer stopped on its way out with exit code 1. It now has 5 seconds to exit with its own code; if it is stopped after that, the exit code is 0 when the task completed.
- Holding an arrow key in a setup menu keeps moving the selection without echoing key sequences into the menu.
- AI task summaries on the control page are formatted: bold, lists, inline code and line breaks show as such instead of raw Markdown.
- On iOS 27 and later, a system alert (a permission prompt such as "Allow Calendar to use your location?") that the live picture on the control page doesn't show is now drawn on the picture where it is, with its buttons; a button there taps it on the phone.
- Locking no longer takes about 10 s every time on some iPhones (e.g. iPhone 15 Pro Max, iOS 27.0.1): pressing the power key via IOHID does nothing there, so the daemon remembers it per device and uses WDA's lock first (about 2 s).
- Locking now works on iPads below iOS 27 (iPad mini 5, iPadOS 26.7.3: it never locked) and takes about 2 s instead of 10 s the first time on an iPhone 15 Pro Max: the power key via IOHID is now held 0.15 s instead of 0.1 s, which those devices ignored.
- A WDA that still answers `/status` but no longer answers anything that goes through XCTest (foreground app, screenshots, elements, lock state all time out) is now noticed and restarted within about 30 s, also while tool calls keep the device busy (requests through XCTest stop waiting out their timeouts once three in a row went unanswered). Before, the daemon kept showing it as ready, and in background mode (`ICTL_WDA_DETACHED`) even a daemon restart attached to the stuck WDA again.
- On devices below iOS 27, a lock that WDA could not carry out (neither the power key via IOHID nor WDA's own lock locked the screen) was reported as "WDA unavailable (just dropped)", although WDA was fine. It now says what each way of locking returned.

## [0.2.3] - 2026-11-13

### Upgrade notes

- Upgrading from 0.2.2: its CLI has no bare `update` yet. For a managed service installed by the signed script, run `iphone-control update apply`; on Homebrew, `brew upgrade iphone-control`; on Debian / Ubuntu, download the `.deb` for your architecture from the release and install it with `sudo apt install ./<file>`. From 0.2.3 on, `iphone-control update` does it.

### Added

- Run `iphone-control update` to check for a new stable release and install it through your managed service, Homebrew or Debian package. It reports when you already have the latest version. `update check` remains available for a check only.
- `iphone-control update` says what it will do and asks before installing; `--yes` skips the question. After a Homebrew or Debian upgrade a running background service is restarted on the new version.
- The Debian packages are signed by the publisher too; `iphone-control update` checks the signature before handing the package to APT through sudo. A package on hold in APT is reported with how to release it.

### Removed

- `iphone-control update rollback` and the previous executable kept after an upgrade. A new version that doesn't start is still replaced by the old one automatically.
- The rollback compatibility mark in release manifests: any newer signed stable release can be installed directly, without stopping at "manual migration required".

### Fixed

- With the control page open in a browser that can't decode HEVC (Firefox, Chrome on most Linux) or on a device below iOS 27, WDA is no longer killed by iOS every 1–2 minutes: the picture is taken at full size and turned upright on the page in landscape, so WDA passes on the screenshot the phone encodes in hardware instead of scaling, rotating and re-encoding every frame, which took more CPU than iOS allows.
- Piping output into a program that exits early, such as `iphone-control status | head -1`, no longer crashes with "Broken pipe"; the command ends quietly like other command-line tools.
- Touches on the control page respond faster again: a press no longer measures the screen size on WDA each time (about 0.1–0.2 s, new in 0.2.2); the size is measured on rotation and at most once a minute. Moving the mouse over the picture also keeps the touch session open, so the first press after a pause doesn't wait about 0.5 s for it to reopen.

## [0.2.2] - 2026-11-12

### Upgrade notes

- For a new script installation, use the `install.sh` asset from your publisher's signed GitHub Release. It requires Python 3 and OpenSSL 3. To migrate an older script installation, stop the service and uninstall it with the original installer, then run the new installer. For 0.2.0, use the original publisher's script at the `v0.2.0` tag and pass `--uninstall` with your original prefix. Your settings and device data are kept.

### Added

- Check for host software updates with `iphone-control update check` or **About & updates** in the viewer. A red dot marks a newer stable release from the build's publisher. Open the panel to see the version and copy the upgrade command for your installation when available.
- User services installed from a signed release can upgrade with `iphone-control update apply` and roll back with `iphone-control update rollback`. They check for updates by default. Opt in with `iphone-control update policy auto-patch` to install signed patches while idle and restore the previous version if the daemon fails its health check.
- A redesigned control page: a workspace that fits wide and narrow windows, frosted panels (solid with high contrast or reduced transparency), and the task input, executor and settings together at the foot of the sidebar. Devices show their product name (iPhone 15 Pro rather than `iPhone16,1`) in an iPhone or iPad frame, and the status bar tells apart waiting for a device, connecting, view only, live and disconnected.

### Fixed

- Taps, swipes and live touch from the control page are bound to the device and picture you see: after a device switch, a rotation or a lost picture they are refused instead of landing at the old position. A brief WDA hiccup no longer resets the picture or lifts a finger mid-drag, and WDA builds without `/rotation` read the orientation from SpringBoard.
- Picture connections from closed or frozen tabs are released, so many reconnects no longer use up the control page's connections.

## [0.2.1] - 2026-11-12

### Added

- On iOS 27 and later the control page shows the phone's own hardware-encoded video (the CoreDevice screen media stream, HEVC): about 60 frames a second at native resolution, nothing encoded on this machine and no capturing by WDA; the picture turns with the screen. Browsers that can't decode HEVC (Firefox, Chrome on most Linux machines) and older iOS fall back to H.264 or MJPEG. `IPHONE_CONTROL_VIEWER_CODEC=device` uses only this stream.
- The control page shows the picture as H.264 at about 1/15 of the traffic: on macOS with VideoToolbox called directly (the JPEG decoded by the hardware too), no ffmpeg needed; on Linux with ffmpeg installed, GPU encoding first (NVENC, VAAPI, QSV, V4L2, else x264); on a slow link the frame rate drops instead of the picture falling further behind. Without ffmpeg it stays MJPEG, now without repeated frames. Settings: `IPHONE_CONTROL_VIEWER_CODEC`, `IPHONE_CONTROL_VIEWER_QP`.
- Experimental `ICTL_WDA_DETACHED=1`: WebDriverAgent is launched in the background without an XCTest session, so it also starts while the phone is locked and keeps running when the daemon restarts. See [WDA dependency research](docs/dev/wda-dependency-research.md).

### Fixed

- With the control page open, WDA is no longer killed by iOS every few minutes (iOS 27 and later): capturing the picture took WDA over a CPU core, iOS kills a process averaging more than 80 % over a minute, and the daemon restarted WDA, failing actions meanwhile. With the phone's own video WDA stays around 3 %. Below iOS 27 the picture still comes from WDA; close the control page when you aren't watching.

## [0.2.0] - 2026-11-11

### Upgrade notes

- Docker is no longer released: no image on ghcr, no `compose.yaml`. Move to the one-line `install.sh`, Homebrew or the `.deb` (see Added). Stop the container first (`docker compose down`) so it doesn't hold the phone.
- Your data was in the container's volume (`./data` next to `compose.yaml`, owned by root). Before the first run, copy it to `~/.local/share/iphone-control` and make it yours (`sudo cp -a ./data ~/.local/share/iphone-control && sudo chown -R "$(id -un)": ~/.local/share/iphone-control`; see [Data and backups](https://derrors.github.io/iphone-control/en/deploy/backup)) to keep the Apple ID session, the signed WebDriverAgent and auto-unlock passcodes. Pairing records now live with the system usbmuxd, so the phone may ask you to trust the computer again.
- Settings move from `.env` to `~/.config/iphone-control/env` (same `KEY=value` lines; mode 0600). The first-run guide writes it, or copy your values over and run `iphone-control service restart`.
- `ICTL_MANAGE_USBMUXD` and `ICTL_USBMUXD_BIN` are gone: the daemon no longer starts usbmuxd. On Linux install the system one (`sudo apt install usbmuxd`; the `.deb` depends on it), and unmask it if you masked it for the container (`sudo systemctl unmask usbmuxd`).
- Register the MCP server again: the old `docker exec` registration no longer works. The first-run guide does it, or run the command `iphone-control mcp-config` prints.

### Added

- First-run guide: after installing, connect the phone and run `iphone-control`. The first time it walks you through setup in the terminal: language, proxy, the background service, pairing, Developer Mode, Apple ID, signing and installing WebDriverAgent, the optional auto-unlock passcode and notifications (Bark or Telegram, with a test message), and registering the MCP server with Claude Code and Codex. Steps already done are skipped; afterwards `iphone-control` shows the status and starts the service if it isn't running.
- `iphone-control setup --reconfigure` redoes all of setup or one part: language, proxy, background service, Apple ID (sign in again and re-sign WebDriverAgent), phone, auto-unlock passcode, notifications or MCP registration.
- `iphone-control service install|uninstall|start|stop|restart|status` manages the background service (launchd on macOS, a systemd user unit on Linux), whichever installed it: `install.sh`, `brew services` or the `.deb`.
- Settings file `~/.config/iphone-control/env`: the service loads it, and so does every command (a variable already set in the environment wins). After editing it by hand, run `iphone-control service restart`.
- `THIRD_PARTY_LICENSES.html` (licenses of all Rust dependencies) is in every release archive and in the `.deb`.
- The control page follows the device the daemon controls: when it is switched elsewhere (another tab, `iphone-control select`, the MCP tool, or automatically after an unplug), the page notices within about 10 seconds, says so, and moves its picture and panels over. Actions carry the device the page shows and are refused, never sent to another phone, once the daemon controls a different one.
- The control page says why a device has no picture (not connected, needs pairing or Developer Mode, WebDriverAgent starting or why it failed to start, needs unlocking), and offers a "Sign and install WDA" button for a device without WebDriverAgent when an Apple ID is signed in.
- Recovery from a revoked signing certificate: a free Apple ID holds at most two certificates, and signing with it somewhere else (another machine, moving from Docker to a native install, another sideloading tool) creates one and revokes an old one, after which WDA signed with that certificate won't start on any device although its profile still looks valid. When a device now refuses to start WDA, the daemon checks whether that WDA was signed with this machine's certificate and whether this machine's certificate is still in the Apple ID's team, and re-signs the device automatically if not (at most once per device every 6 hours).
- `doctor` shows both certificate checks; signing that creates or revokes a certificate is logged and notified (a new notification type `cert`, try it with `notify-test cert`).
- Simpler installs: `curl -fsSL https://raw.githubusercontent.com/Derrors/iphone-control/main/install.sh | sh` on macOS (Apple Silicon) and Linux installs the release binary and runs the daemon as a per-user service (launchd on macOS, which had none before; systemd on Linux), and upgrades or uninstalls it again; `brew install derrors/tap/iphone-control` with `brew services start iphone-control`; and a `.deb` for Debian 12+ and Ubuntu 22.04+ (amd64, arm64) with a systemd user service, depending on usbmuxd. The release workflow builds the packages and updates the Homebrew tap.
- AI tasks in the live viewer: type a task on the page and a local agent (Claude Code or Codex, picked on the page) carries it out on the phone through the MCP tools, while the page shows each step with what the agent says it is doing, the result and screenshots, and marks where it taps. Pause, resume, take over (act on the phone yourself) and hand back, or stop. Before paying, deleting, sending a message or changing account settings the agent asks on the page (`iphone_confirm`) and waits; a rejected step isn't done. The steps, holds and confirmations go through `iphone-control mcp` itself, so they work the same with either agent; the agent runs with only the phone tools (no shell or file tools for Claude Code, a read-only sandbox for Codex).

### Changed

- New viewer page: AI assistant, live screen with zoom, and a device panel with state and readiness checks, installed apps (launch, quit, open a URL) and on-screen elements (highlight on the picture, tap by name, wait for text). The device can be switched from the page, and the picture follows. While an AI task runs, manual actions are refused until you take over.
- On iOS 27 and later the viewer's picture works like a touchscreen: pressing, moving and releasing the mouse moves a finger on the phone as you go (over CoreDevice HID), so a list scrolls while you drag, and the mouse wheel or trackpad scrolls the page under the pointer without holding the button. A tap round trip is about 20 ms instead of about 530 ms, and a drag no longer waits for the list to stop (1.8 to 3 s before). Older iOS versions keep whole gestures sent on release. While the viewer is used, the HID session's screen stream keeps the phone awake, until 30 seconds after the last touch.
- The live picture runs at about 50 frames a second at 75% size (WDA's default was 10 at full size).
- The viewer page is in English or Chinese: it follows `IPHONE_CONTROL_LANG`, and a button switches it. The daemon's messages and the AI task (instructions, tool results) use the page's language.
- The viewer's task history shows one plain sentence per step (what the agent said it is doing, or a description of the action), without screenshots, tool names, parameters or raw results; a step that didn't work says so. Screenshots are no longer passed from the agent's tools to the viewer.
- The viewer no longer draws marks for your own taps and swipes, and no longer shows a message for each successful manual action; errors are still shown.
- The viewer serves more read-only JSON (`/api/daemon`, `/status`, `/devices`, `/apps`, `/elements`) and refuses reads that a browser marks as sent by another site (`Sec-Fetch-Site`), since reading elements can unlock the phone.

### Removed

- The Docker image (`ghcr.io/derrors/iphone-control`), `compose.yaml`, `docker/Dockerfile` and `.dockerignore`. A container can't reach the local Claude Code or Codex that the control page hands tasks to, and the native installs are simpler.
- The daemon no longer starts or watches usbmuxd (`ICTL_MANAGE_USBMUXD`, `ICTL_USBMUXD_BIN`); the system usbmuxd is used on Linux and macOS.
- `deploy/iphone-control.service`: `install.sh` and `iphone-control service install` write the unit now.
- `iphone-control mcp-config --container`: `mcp-config` takes no arguments and prints a path that survives Homebrew upgrades.

### Fixed

- Control page: the mouse wheel works again on devices below iOS 27 (whole-gesture mode). It waited for a leftover scroll amount that never drains in that mode, so no swipe went out and later wheel events piled up.
- Pressing Home right after an app comes to the front, while its transition is still running, was ignored by iPadOS for both ways of pressing (about one time in three on an iPad mini 5 / iPadOS 26.6.1). When neither brings back the home screen, Home is now pressed once more.
- When WebDriverAgent fails to start with only `unexpected response`, the hint adds the second likely cause: if the developer is already trusted, the signing certificate may have changed; run `iphone-control sign` to re-sign the device. In that case the control page offers a "Re-sign and install WDA" button when an Apple ID is signed in.
- In automatic mode the daemon, after a restart, goes on with the last device that was ready instead of whichever usbmuxd lists first (with an iPhone and an iPad connected, a restart could switch to a device whose WebDriverAgent can't start).
- `iphone-control viewer` no longer exits when the daemon isn't up yet and several devices are connected: it starts anyway and the page follows once the daemon has picked a device.
- Control page: on a device below iOS 27 without WebDriverAgent, the route shows "Unavailable" instead of the HID fallback (which needs iOS 27); an unknown lock state is no longer shown as "Unlocked"; when WebDriverAgent fails to start, the picture area shows why instead of "usually 10–60 seconds".
- "WDA failed to start: WDA failed to start:" is no longer doubled in the log and the state.
- `devices`, `select` and `doctor` on the command line also name `iphone-control select` / `iphone-control status`, not only the MCP tools; when the daemon can't be reached, the hint is `iphone-control service start`.
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

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.2.3...HEAD
[0.2.3]: https://github.com/Derrors/iphone-control/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Derrors/iphone-control/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Derrors/iphone-control/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Derrors/iphone-control/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/Derrors/iphone-control/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Derrors/iphone-control/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Derrors/iphone-control/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Derrors/iphone-control/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Derrors/iphone-control/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Derrors/iphone-control/releases/tag/v0.1.0
