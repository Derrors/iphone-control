# Security

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: **[Report a vulnerability](https://github.com/Derrors/iphone-control/security/advisories/new)** (Security tab → *Report a vulnerability*). Do not open a public issue for security problems. Reports in English or Chinese are both fine.

You can expect an acknowledgement within a week. Fixes go into the latest release; older versions are not patched while the project is at 0.x.

## What this software has access to

iphone-control is, by design, a remote control for a phone. Anyone who can talk to it can see the screen and do anything the phone's owner can do while it is unlocked. Treat the machine that runs it like the phone itself.

| Asset | Where | Who can reach it |
|---|---|---|
| Phone control (taps, typing, screenshots, apps) | Control socket: `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` on Linux, `~/.local/share/iphone-control/ctl.sock` on macOS; mode 0600 | The user the daemon runs as, and root |
| WebDriverAgent (no authentication) | On the phone, reached through usbmuxd; nothing listens on the network unless you run `wda-bridge` | With `wda-bridge`: **every local user** on the host, since the port is on 127.0.0.1. Browsers are blocked by a Host-header check (DNS rebinding), other local processes are not |
| Live viewer (`iphone-control viewer`, opt-in) | A page on `127.0.0.1` (port 8200 by default); nothing listens unless you run it. Follows the device the daemon controls. Its AI tasks run Claude Code or Codex as the user who started the viewer, and their MCP servers report to it over a Unix socket next to the control socket (`viewer-<port>.sock`, mode 0600, a secret token per task) | **Every local user** on the host can watch the screen, control the phone and start AI tasks (which use that user's agent login) through the port: it has no login. Other web pages in a browser are blocked by the `Host`, `Origin`, JSON and `Sec-Fetch-Site` checks, other local processes are not |
| Apple ID session tokens, development signing key and certificate | `account/` in the data directory (0700, key 0600) | Whoever can read the data directory. The Apple ID password itself is never stored |
| Pairing records | The system's `/var/lib/lockdown` (Linux, kept by usbmuxd) or `/var/db/lockdown` (macOS, shared with Finder) | Root on the host. A pairing record lets its holder talk to the phone over USB without another *Trust* prompt |
| Lock-screen passcodes | `secrets/passcodes/<UDID>` in the data directory (0600), one file per device, **plain text**, only if you ran `passcode set`. A `secrets/passcode` file from an older version is moved to the device's file when exactly one device is connected, and stays where it is until then | Whoever can read the data directory |
| Notification credentials (Bark key, Telegram token) | `~/.config/iphone-control/env` (mode 0600) | Whoever can read that file |

The data directory is `~/.local/share/iphone-control`. Everything runs as your regular user; `install.sh` refuses to run as root.

## Design choices that limit exposure

- No network listener by default. WebDriverAgent is reached through usbmuxd. `wda-bridge` and `viewer` are opt-in and bind 127.0.0.1 only, and both are meant to be stopped after use. `wda-bridge` only forwards requests whose Host header is a loopback name. `viewer` checks the Host header the same way; its actions also need an `Origin` of the viewer page itself and a JSON body, so other web pages can't act through it, reads a browser marks as coming from another site (`Sec-Fetch-Site`) are refused, and each action carries the UDID of the device the page follows, so the daemon refuses it once another device is controlled.
- The control socket is a Unix socket with mode 0600, not TCP.
- The tunnel to the phone uses a user-space TCP stack, so no network interface or routing changes on the host, and the daemon needs no root.
- Auto-unlock is off by default. When enabled, it stops for good after a single wrong attempt (it writes a stop marker, `logs/UNLOCK-DISABLED`, before entering the second digit) and sends a notification; it never retries a passcode that might be wrong. The marker is global: a failure on one device stops auto-unlock on all of them until you run `unlock-reset`. It does not work before the first unlock after a reboot (BFU) — that always needs a person.
- Dependencies are pinned to exact versions, `Cargo.lock` is committed, release archives come with a sha256 that `install.sh` checks, and the WebDriverAgent download is checked against a sha256. CI runs gitleaks on every push.

## Known risks and recommendations

- **Anyone who can act as your user can control the phone.** The daemon runs as a per-user service and talks to the phone through the system usbmuxd. Keep the account and the host locked down.
- **Third-party anisette server.** Apple ID logins need "anisette" data that normally only Apple hardware produces. By default it comes from a public server (`https://mac-ani.jkcoxson.com`, the default of the Impactor/omnisette libraries). The server does not receive your Apple ID password, but your logins depend on an identity it provisions, and it sees when you log in. Set `ICTL_ANISETTE_URL` to an [anisette-v3 server](https://github.com/Dadoum/anisette-v3-server) you run yourself to avoid this.
- **Use a dedicated Apple ID.** The session token and signing key on disk are enough to manage that account's development certificates and devices. A separate free account limits the damage if the host is compromised and keeps your main account clear of developer-account restrictions.
- **Auto-unlock stores your passcode in plain text.** Anyone with root on the host, or access to backups of the data directory, can read it — and the passcode is also what protects the phone's data and Apple ID. Only enable it on a host you fully control, and prefer leaving Auto-Lock off on a phone dedicated to automation.
- **Agents act with your hands.** An agent with these tools can send messages, make purchases and change settings. The MCP server tells agents to ask before paying, deleting, sending messages or changing account or privacy settings, but that is an instruction, not an enforcement mechanism. The same goes for AI tasks started from the viewer: the agent is told to call `iphone_confirm` first and the page waits for your answer, and a step you reject is refused, but an agent that skips the call isn't stopped. There it gets only the phone tools (Claude Code without its shell and file tools, Codex in a read-only sandbox). Use a phone and accounts you are comfortable automating.
- **Back up the data directory securely**, and never commit it or the settings file. `.gitignore` excludes `data/`, `secrets/` and key material in case you keep copies next to the source.

## Developer tools

`IPHONE_CONTROL_DEV_TOOLS=1` adds six MCP tools that reach well beyond the screen. They are off by default; turn them on for the work that needs them and off again afterwards.

- **`iphone_web_eval` runs arbitrary JavaScript** in any inspectable page, including Safari tabs. A script can read cookies that aren't HttpOnly and `localStorage`, and submit forms or send requests as the logged-in user.
- **`iphone_install_app` accepts any path on the machine that runs the daemon** that the daemon's user can read.
- **`iphone_app_logs` reads the system log of every process**, and `iphone_crash_reports` returns full crash reports. Both end up in the agent's context and can contain private data.
- **`iphone_uninstall_app` deletes an app and its data.** It refuses the WebDriverAgent runner and only accepts a bundle ID or the app's exact display name.
- **The switch is not a permission boundary.** `IPHONE_CONTROL_DEV_TOOLS` only decides whether the MCP server offers these tools. The daemon always accepts the requests, so anyone who can reach the control socket (mode 0600, so the same user or root) can use them with the switch off.

## Developer Disk Image and WebDriverAgent sources

- The Developer Disk Image is downloaded at runtime from a community-maintained mirror (the same source pymobiledevice3 uses) unless you set `ICTL_DDI_URL` or `ICTL_DDI_DIR`. The phone verifies Apple's signature through personalization (TSS), so a tampered image will not mount.
- The unsigned WebDriverAgent comes from Appium's official release (pinned version, sha256-checked) and is signed locally with your Apple ID.
