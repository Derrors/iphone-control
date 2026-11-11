# Getting Started

Install the program, plug in the phone, run `iphone-control` and follow the guide. It takes about 15 minutes. The steps are the same on Linux and a Mac; Mac details are in [macOS](../deploy/macos).

## You need

- A host: a Mac with Apple silicon, or Linux (x86_64, aarch64)
- An iPhone or iPad (iOS 17+) and a cable that carries data
- A free Apple ID (a dedicated one is recommended)
- Internet access to Apple and GitHub from the host (the guide asks for a proxy if you need one)

::: warning One phone, one owner
Don't run two copies of iphone-control, or other usbmuxd-based tools (go-ios, pymobiledevice3, …), against the same phone.
:::

## 1. Install {#install}

Pick one of the three. Install as your regular user; don't run `iphone-control` as root or with sudo.

**One command** (macOS or Linux, with Python 3 and OpenSSL 3 on PATH)

```bash
curl -fsSL https://github.com/Derrors/iphone-control/releases/latest/download/install.sh | sh
```

It installs the program into `~/.local/bin` and the daemon as a per-user service (launchd on macOS, a systemd user service on Linux) that starts when you log in. Run `iphone-control update check` for upgrade instructions. Options go after `sh -s --`: `--version vX.Y.Z` installs that release, `--prefix DIR` installs into `DIR/bin`, `--no-service` installs only the program.

**Homebrew** (macOS, Linux):

```bash
brew install derrors/tap/iphone-control
brew services start iphone-control
```

**Debian / Ubuntu package:** download `iphone-control_<version>_amd64.deb` (`_arm64.deb` on ARM) from the [GitHub Release](https://github.com/Derrors/iphone-control/releases), then:

```bash
sudo apt install ./iphone-control_<version>_amd64.deb
systemctl --user enable --now iphone-control     # as your regular user; each user turns it on for themselves
```

You can skip the second line: the guide below starts it for you.

::: tip Linux needs the system usbmuxd
Talking to the phone over USB goes through the system's usbmuxd: `sudo apt install usbmuxd` (or your distribution's package manager). The .deb pulls it in.
:::

Intel Macs have no prebuilt binary and need a build from source, as does development. See [Build from Source](../deploy/source).

## 2. Plug in the phone and run iphone-control {#first-run}

Plug in the phone with a cable, unlock it, then run in a terminal:

```bash
iphone-control
```

The first time, it walks you through setup step by step. Steps that are already done are skipped; if you stop halfway, running `iphone-control` again picks up where you left. It needs an interactive terminal.

| Step | What you do |
|---|---|
| Language | Pick 中文 or English |
| Proxy | Enter a proxy URL if you need one, e.g. `http://127.0.0.1:7890` |
| Background service | If no service is set up yet, press Enter to set it up and start it |
| Waiting for the phone | Unlock it |
| Pairing | On the phone, tap *Trust* when asked "Trust This Computer?", enter the passcode |
| Developer Mode | On the phone: *Settings → Privacy & Security → Developer Mode → On*, restart, then tap *Turn On* after unlocking |
| Apple ID | In the terminal, enter the Apple ID, password and two-factor code |
| Sign and install WDA | Keep the phone unlocked |
| Waiting for WDA | On the phone: *Settings → General → VPN & Device Management* → trust your Apple ID (needs internet); if asked to *enter the iPhone passcode for "XCTest"*, enter it |
| Auto-unlock (optional) | Enter the passcode if you want it; see [Auto-unlock](./auto-unlock) |
| Notifications (optional) | Pick Bark or Telegram and enter the key or token; a test message is sent. See [Notifications](./notifications) |
| MCP registration | If Claude Code or Codex is found, press Enter to register with it |

You're finished when it says *All set*. Settings are saved in `~/.config/iphone-control/env`; see [Configuration](./configuration#settings-file).

::: tip Your password stays in the terminal
The Apple ID password and code are only typed here and never written to disk; only the resulting session is stored.
:::

From then on, `iphone-control` just shows the status, and starts the daemon first if it isn't running. To change a setting, run `iphone-control setup --reconfigure` and pick the part to set up again; see [CLI](./cli#setup).

## 3. Try it

Open a new Claude Code or Codex session and say:

> Open Settings on the iPhone and tell me which iOS version it runs.

The agent will use `iphone_launch_app`, `iphone_tap_element`, `iphone_screenshot` and friends. Other clients: [Connect an AI Client](./mcp-clients).

You can also watch and control the phone in a browser, or hand tasks to a local Claude Code or Codex there:

```bash
iphone-control viewer      # then open http://127.0.0.1:8200
```

## Checking on it

```bash
iphone-control status   # state, signing time left, recent log
iphone-control doctor   # layer-by-layer checks with a fix for each failure
```

Something wrong? See the [FAQ](../troubleshoot/faq).

## Uninstall

| Installed with | Uninstall |
|---|---|
| One command | `curl -fsSL https://github.com/Derrors/iphone-control/releases/latest/download/install.sh \| sh -s -- --uninstall` |
| Homebrew | `brew services stop iphone-control`, then `brew uninstall iphone-control` |
| .deb | `systemctl --user disable --now iphone-control`, then `sudo apt remove iphone-control` |

The data directory `~/.local/share/iphone-control` (Apple ID session, signing, auto-unlock passcodes) stays; delete it if you no longer need it, and remove WebDriverAgent on the phone.
