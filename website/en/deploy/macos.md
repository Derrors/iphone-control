# macOS

iphone-control also runs directly on a Mac, using the usbmuxd that ships with macOS. No Xcode and no paid developer account. It works like a source install on Linux; the differences are listed under [Known differences](#known-differences).

::: info Verified
Fully verified on a MacBook Pro (Apple M3 Pro) / macOS 26.6.2 with an iPhone 15 Pro Max / iOS 27.0.1: the setup wizard, every MCP tool, continuous use with Finder open, one tunnel kept busy for 30 minutes, and recovery after unplugging and phone reboots. Intel Macs are untested. Steps and results: [macOS verification](https://github.com/Derrors/iphone-control/blob/main/docs/dev/macos-verification.md) (in Chinese).
:::

## You need

- An Apple Silicon Mac
- The Xcode Command Line Tools (C compiler and git). Full Xcode is **not** needed
- Rust 1.88 or later
- An iPhone on iOS 17+, connected with a cable that carries **data**
- A free Apple ID (a dedicated one is recommended)

::: warning No Docker
Docker Desktop on a Mac runs containers in a Linux VM without USB passthrough, so a container can't see the phone. Run the program directly.
:::

## Install

There is no prebuilt macOS binary in Releases yet; build from source (about 2½ minutes on an M3 Pro):

```bash
xcode-select --install                                         # says so if already installed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh  # then open a new terminal
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked              # installs ~/.cargo/bin/iphone-control
iphone-control --version
```

## Run

**Run everything as your regular user, never with sudo.** macOS's own usbmuxd is always running and the daemon uses it. As root, the daemon would start a second usbmuxd that fights the system one, and would switch to Linux system paths (`/var/lib`, `/run`) for its data and control socket.

In one terminal, start the daemon; its log prints there:

```bash
iphone-control daemon
```

In a second terminal, run the setup wizard:

```bash
iphone-control setup
```

The six steps are the same as on Linux; what to do on the phone is in [Getting Started](../guide/getting-started#_2-run-the-setup-wizard). If the phone already trusts this Mac (from Finder), the pairing step just says it is paired.

When the wizard says WDA is ready (WDA 就绪), connect your AI client:

```bash
iphone-control mcp-config    # prints the registration command; run it
```

It prints something like `claude mcp add iphone -s user -- /Users/you/.cargo/bin/iphone-control --socket /Users/you/.local/share/iphone-control/ctl.sock mcp`. Other clients: [Connect an AI Client](../guide/mcp-clients).

### Configuration

Settings go in `~/.config/iphone-control/env` (same format as `.env.example` in the repository). Load it before starting the daemon:

```bash
mkdir -p ~/.config/iphone-control
cp .env.example ~/.config/iphone-control/env    # proxy, notifications, …
set -a; . ~/.config/iphone-control/env; set +a
iphone-control daemon
```

Restart the daemon (Ctrl-C, then run it again) after changing settings. The variables are listed in [Configuration](../guide/configuration).

### Keep it running

There is no launchd configuration yet, so keep the daemon in a terminal (or tmux). It pauses while the Mac sleeps; for long unattended runs, keep the Mac awake with `caffeinate`:

```bash
caffeinate -i iphone-control daemon
```

## Keep the USB connection stable

- **Set the phone's charging limit to 100%** under *Settings → Battery → Charging*. With the 80% limit on, the phone keeps starting and stopping charging near 80%, and each switch renegotiates USB-C power, which drops USB for 3–5 seconds about once a minute. The daemon reconnects by itself, but actions in progress fail.
- For long runs, keep the Mac on power.
- Finder can stay open on the phone's page; it doesn't take the connection away.

## Everyday commands

Same as on Linux, run directly without `docker exec`:

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

```bash
cd iphone-control && git pull
cargo install --path apps/iphone-control --locked
```

Then restart the daemon.

## Uninstall

Stop the daemon with Ctrl-C in its terminal, then:

```bash
cargo uninstall iphone-control
rm -rf ~/.local/share/iphone-control ~/.config/iphone-control   # deletes the Apple ID session and signed WDA
```

On the phone, delete the WebDriverAgent app; its profile under *VPN & Device Management* goes with it.

## Known differences

| | Linux (source install) | macOS |
|---|---|---|
| Install | Prebuilt binary or build from source | Build from source only |
| Keep running | systemd user service | No launchd configuration yet; run in a terminal |
| usbmuxd | System package, started by udev on plug-in | Built into macOS, always running |
| Data directory | `~/.local/share/iphone-control` | Same (not the usual `~/Library/Application Support`) |
| Control socket | `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` | `~/.local/share/iphone-control/ctl.sock` |
| Pairing records | `/var/lib/lockdown` | `/var/db/lockdown` (shared with Finder) |
| Docker | Supported | Not supported |

Not yet verified on a Mac: whether Xcode's *Devices and Simulators* window competes for the connection, pairing a phone with the Mac for the first time, and Intel Macs. [Reports](https://github.com/Derrors/iphone-control/issues) are welcome.
