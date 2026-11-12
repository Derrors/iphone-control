# macOS

iphone-control also runs directly on a Mac, using the usbmuxd that ships with macOS. No Xcode and no paid developer account. It works much like on Linux; the differences are listed under [Known differences](#known-differences).

::: info Verified
Verified on a MacBook Pro (Apple M3 Pro) / macOS 26.6.2 with an iPhone 15 Pro Max / iOS 27.0.1: the setup wizard, every MCP tool at that time, continuous use with Finder open, one tunnel kept busy for 30 minutes, and recovery after unplugging and phone reboots. This predates 0.1.3: the multi-device tools (`iphone_list_devices`, `iphone_select_device`) haven't been verified on macOS. Intel Macs are untested. Steps and results: [macOS verification](https://github.com/Derrors/iphone-control/blob/main/docs/dev/macos-verification.md) (in Chinese).
:::

## You need

- An Apple Silicon Mac. The script installer requires Python 3 and OpenSSL 3 on PATH. Intel Macs have no prebuilt package and build from source (Xcode Command Line Tools and Rust 1.88+). Full Xcode is **not** needed
- An iPhone or iPad on iOS 17+, connected with a cable that carries **data**
- A free Apple ID (a dedicated one is recommended)

## Install

### One command (recommended)

```bash
curl -fsSLo iphone-control-install.sh https://github.com/Derrors/iphone-control/releases/latest/download/install.sh && sh iphone-control-install.sh
```

It downloads the latest release, verifies the Ed25519 release manifest and archive SHA-256 (Python 3 and OpenSSL 3 required), installs `~/.local/bin/iphone-control` and a launchd user service (`~/Library/LaunchAgents/io.github.derrors.iphone-control.plist`) that starts the daemon at login, restarts it after a crash, reads `~/.config/iphone-control/env` and logs to `~/Library/Logs/iphone-control/daemon.log`. Pass `--version vX.Y.Z` for a specific release with a signed macOS manifest, `--prefix DIR` to install into `DIR/bin`, or `--no-service` to install only the binary.

For upstream v0.2.0, follow the [legacy installation and migration steps](../guide/getting-started#legacy-script).

### Homebrew

```bash
brew install derrors/tap/iphone-control
brew services start iphone-control      # the daemon runs now and after each login
```

This service also reads `~/.config/iphone-control/env` and logs to `$(brew --prefix)/var/log/iphone-control.log`. The settings template is `$(brew --prefix)/opt/iphone-control/share/iphone-control/env.example`.

### Build from source (Intel Macs)

You need the Xcode Command Line Tools and Rust 1.88+; about 2½ minutes on an M3 Pro:

```bash
xcode-select --install                                         # says so if already installed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh  # then open a new terminal
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked              # installs ~/.cargo/bin/iphone-control
iphone-control --version
```

The setup guide below sets up the background service.

## Run

**Run everything as your regular user, never with sudo.** macOS's own usbmuxd is always running and the daemon uses it. As root, the daemon would switch to Linux system paths (`/var/lib`, `/run`) for its data and control socket, and the install script refuses to run as root.

Connect the phone with a cable, then:

```bash
iphone-control
```

The first run walks you through setup step by step: language, proxy, background service, pairing, Developer Mode, Apple ID, signing and installing WDA, auto-unlock and notifications (optional), and registering with Claude Code and Codex. What to do on the phone is in [Getting Started](../guide/getting-started#first-run). If the phone already trusts this Mac (from Finder), the pairing step just says it is paired. Once set up, running `iphone-control` again only shows the status.

### Doing it by hand

Each thing the guide does can be done on its own:

```bash
iphone-control daemon        # the daemon in the foreground, in one terminal (without the service)
iphone-control setup         # in a second terminal: the guide again, done steps skipped
iphone-control mcp-config    # prints the MCP registration command; run it
```

`mcp-config` prints something like `claude mcp add iphone -s user -- /Users/you/.local/bin/iphone-control --socket /Users/you/.local/share/iphone-control/ctl.sock mcp`. With Homebrew the program path is `/opt/homebrew/opt/iphone-control/bin/iphone-control`, which stays the same across upgrades. Other clients: [Connect an AI Client](../guide/mcp-clients).

### Configuration

Settings are in `~/.config/iphone-control/env`; the guide and `iphone-control setup --reconfigure` write it. After editing it by hand, run `iphone-control service restart`. The variables are listed in [Configuration](../guide/configuration#settings-file).

### Keep it running

The background service set up by the one command, Homebrew or the guide starts the daemon when you log in; nothing else to do:

```bash
iphone-control service status     # whether the service and the daemon run
iphone-control service restart    # restart, e.g. after changing settings
```

The daemon pauses while the Mac sleeps. For long unattended runs, run `caffeinate -i` in a terminal and leave it open to keep the Mac awake.

## Keep the USB connection stable

- **Set the phone's charging limit to 100%** under *Settings → Battery → Charging*. With the 80% limit on, the phone keeps starting and stopping charging near 80%, and each switch renegotiates USB-C power, which drops USB for 3–5 seconds about once a minute. The daemon reconnects by itself, but actions in progress fail.
- For long runs, keep the Mac on power.
- Finder can stay open on the phone's page; it doesn't take the connection away.

## Everyday commands

```bash
iphone-control status    # state, signing time left, recent log
iphone-control doctor    # layer-by-layer checks
```

To read macOS system logs, use the full path `/usr/bin/log`: zsh has a built-in `log` command, so a bare `log stream` fails with `too many arguments`.

If usbmuxd itself misbehaves (`doctor` can't reach it), restart it:

```bash
sudo launchctl kickstart -k system/com.apple.usbmuxd
```

## Upgrade

Read the upgrade notes in the [changelog](https://github.com/Derrors/iphone-control/blob/main/CHANGELOG.md) first.

For a managed script installation, close Viewer, MCP and other device clients, then run `iphone-control update`. On Homebrew the same command upgrades the executable on disk through the tap. A running `brew services` service is restarted afterwards. Reopen Viewer and reconnect your MCP client to use the new version.

For a source build, run the following commands.

```bash
cd iphone-control && git pull
cargo install --path apps/iphone-control --locked
iphone-control service restart
```

## Uninstall

For a signed script installation, run `sh iphone-control-install.sh --uninstall` with your original prefix. For v0.2.0, use the [tagged uninstall script](../guide/getting-started#legacy-script). Homebrew: `brew services stop iphone-control && brew uninstall iphone-control`. From source: `iphone-control service uninstall && cargo uninstall iphone-control`.

All of them keep your data and settings. To remove them too:

```bash
rm -rf ~/.local/share/iphone-control ~/.config/iphone-control   # deletes the Apple ID session and signed WDA
```

On the phone, delete the WebDriverAgent app; its profile under *VPN & Device Management* goes with it.

## Known differences

| | Linux | macOS |
|---|---|---|
| Install | One command, Homebrew, .deb or build from source | One command, Homebrew or build from source (packages for Apple Silicon only) |
| Keep running | systemd user service | launchd user service |
| usbmuxd | System package, started by udev on plug-in | Built into macOS, always running |
| Data directory | `~/.local/share/iphone-control` | Same (not the usual `~/Library/Application Support`) |
| Control socket | `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` | `~/.local/share/iphone-control/ctl.sock` |
| Pairing records | `/var/lib/lockdown` | `/var/db/lockdown` (shared with Finder) |

Not yet verified on a Mac: whether Xcode's *Devices and Simulators* window competes for the connection, pairing a phone with the Mac for the first time, iPads, several devices, and Intel Macs. [Reports](https://github.com/Derrors/iphone-control/issues) are welcome.
