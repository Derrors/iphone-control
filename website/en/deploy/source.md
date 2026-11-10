# Source / Binary

Run it directly as a regular user on Linux, without Docker. You need the system usbmuxd (udev starts it when the phone is plugged in). On a Mac, see [macOS](./macos).

## Install

### One command (recommended)

```bash
sudo apt install usbmuxd     # or your distribution's package manager
curl -fsSL https://raw.githubusercontent.com/Derrors/iphone-control/main/install.sh | sh
```

It downloads the latest release, checks its sha256, installs `~/.local/bin/iphone-control`, and installs and starts a systemd user service (`~/.config/systemd/user/iphone-control.service`, reading `~/.config/iphone-control/env`). To also start it at boot without logging in, run `sudo loginctl enable-linger "$USER"`. Options go after `sh -s --`: `--version`, `--prefix`, `--no-service`, `--uninstall`.

### Debian / Ubuntu package

Download `iphone-control_<version>_amd64.deb` (`_arm64.deb` on ARM) from [Releases](https://github.com/Derrors/iphone-control/releases); it needs Debian 12 / Ubuntu 22.04 or newer:

```bash
sudo apt install ./iphone-control_<version>_amd64.deb   # pulls in usbmuxd
systemctl --user enable --now iphone-control           # as your regular user, no sudo
```

The binary is `/usr/bin/iphone-control`, the service file is in `/usr/lib/systemd/user/`.

### Prebuilt binary

[Releases](https://github.com/Derrors/iphone-control/releases) has Linux x86_64 and aarch64 archives (glibc 2.34 or later: Debian 12, Ubuntu 22.04 and newer):

```bash
sudo apt install usbmuxd
tar xzf iphone-control-v*-x86_64*.tar.gz   # on ARM, aarch64 instead of x86_64 (see uname -m)
install -Dm755 iphone-control-*/iphone-control ~/.cargo/bin/iphone-control
```

`~/.cargo/bin` matches the systemd unit below; if you put it elsewhere, change `ExecStart` in the unit too.

### Build from source

You need Rust 1.88+ ([rustup](https://rustup.rs)) and a C compiler:

```bash
sudo apt install build-essential git usbmuxd
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cargo install --path apps/iphone-control --locked   # installs ~/.cargo/bin/iphone-control
```

## Run

```bash
iphone-control daemon       # in the foreground
iphone-control setup        # setup wizard, in a second terminal
iphone-control mcp-config   # prints the MCP registration command
```

Only the systemd service below reads `~/.config/iphone-control/env`. In the foreground, load it first: `set -a; . ~/.config/iphone-control/env; set +a`.

### Keep it running: systemd user service

The unit is `deploy/iphone-control.service` in the repository (also included in the binary archives):

```bash
mkdir -p ~/.config/systemd/user ~/.config/iphone-control
cp deploy/iphone-control.service ~/.config/systemd/user/
cp .env.example ~/.config/iphone-control/env    # proxy, notifications, … (env.example in the binary archive)
systemctl --user daemon-reload && systemctl --user enable --now iphone-control
loginctl enable-linger "$USER"                  # start at boot without logging in
```

## Differences from Docker

| | Docker | Source (regular user) |
|---|---|---|
| Data directory | `./data` | `~/.local/share/iphone-control` |
| Control socket | `/run/iphone-control/ctl.sock` in the container | `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` |
| usbmuxd | started by the daemon | the system service |
| Pairing records | `lockdown/` in the data directory | the system's `/var/lib/lockdown` |
| Unsigned WDA | shipped in the image | downloaded before the first signing (sha256-checked) |
| Configuration | `.env` | `~/.config/iphone-control/env` |

::: warning
- Run the daemon, the CLI and the MCP server as the same user.
- Don't run it alongside the Docker version against the same phone. With a system usbmuxd installed, `sudo systemctl mask usbmuxd` before going back to Docker.
:::

## Upgrade

Read the upgrade notes in the [changelog](https://github.com/Derrors/iphone-control/blob/main/CHANGELOG.md) first.

Installed with the one command: run it again. .deb: install the new package with `sudo apt install ./…deb`, then `systemctl --user restart iphone-control`. From source or a binary:

```bash
git pull && cargo install --path apps/iphone-control --locked   # or drop in the new binary
systemctl --user restart iphone-control
```
