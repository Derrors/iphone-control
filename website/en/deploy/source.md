# Build from Source

Usually you don't need this: the one-line installer, Homebrew and the .deb all install the program built for the release; see [Getting Started](../guide/getting-started#install). Build from source on machines without a prebuilt package (Intel Macs) and for development. On a Mac, see [macOS](./macos).

## Requirements

- Rust 1.88+ ([rustup](https://rustup.rs)) and a C compiler
- On Linux, the system usbmuxd (udev starts it when the phone is plugged in). The daemon doesn't start usbmuxd itself

## Build and install

```bash
sudo apt install build-essential git usbmuxd     # or your distribution's package manager
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked   # installs ~/.cargo/bin/iphone-control
```

## Run

Run it as your regular user, not with sudo. Connect the phone with a cable, then:

```bash
iphone-control
```

The first run is the setup guide, the same as for a packaged install; see [Getting Started](../guide/getting-started#first-run). Its "Background service" step sets up a systemd user service (`~/.config/systemd/user/iphone-control.service`, reading `~/.config/iphone-control/env`), so the daemon starts when you log in. You can also set it up on its own:

```bash
iphone-control service install
sudo loginctl enable-linger "$USER"   # start at boot without logging in
```

For development you can skip the service and run the daemon in the foreground in a terminal, with its log printed there:

```bash
iphone-control daemon
```

Every command reads `~/.config/iphone-control/env` when it starts; a variable already in the environment wins. See [Configuration](../guide/configuration#settings-file).

::: warning
- Run the daemon, the CLI and the MCP server as the same user.
- One copy per phone: don't run two copies of iphone-control (for example the service plus one in the foreground), or other usbmuxd-based tools (go-ios, pymobiledevice3, …).
:::

## Upgrade

Read the upgrade notes in the [changelog](https://github.com/Derrors/iphone-control/blob/main/CHANGELOG.md) first.

```bash
git pull && cargo install --path apps/iphone-control --locked
iphone-control service restart
```

## Uninstall

```bash
iphone-control service uninstall    # stop and remove the background service
cargo uninstall iphone-control
```

The data directory `~/.local/share/iphone-control` and the settings in `~/.config/iphone-control` are kept; see [Data and Backups](./backup).
