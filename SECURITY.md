# Security

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: **[Report a vulnerability](https://github.com/Derrors/iphone-control/security/advisories/new)** (Security tab → *Report a vulnerability*). Do not open a public issue for security problems. Reports in English or Chinese are both fine.

You can expect an acknowledgement within a week. Fixes go into the latest release; older versions are not patched while the project is at 0.x.

## What this software has access to

iphone-control is, by design, a remote control for a phone. Anyone who can talk to it can see the screen and do anything the phone's owner can do while it is unlocked. Treat the machine that runs it like the phone itself.

| Asset | Where | Who can reach it |
|---|---|---|
| Phone control (taps, typing, screenshots, apps) | Control socket: `/run/iphone-control/ctl.sock` inside the container, `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` for a source install; mode 0600 | Root on the host; anyone who can `docker exec` into the container; the user running a source install |
| WebDriverAgent (no authentication) | On the phone, reached through usbmuxd; nothing listens on the network unless you run `wda-bridge` | With `wda-bridge`: **every local user** on the host, because the container uses host networking. Browsers are blocked by a Host-header check (DNS rebinding), other local processes are not |
| Apple ID session tokens, development signing key and certificate | `account/` in the data directory (0700, key 0600) | Whoever can read the data directory. The Apple ID password itself is never stored |
| Pairing records | `lockdown/` in the data directory (container) or the system's `/var/lib/lockdown` | Root on the host. A pairing record lets its holder talk to the phone over USB without another *Trust* prompt |
| Lock-screen passcode | `secrets/passcode` in the data directory (0600), **plain text**, only if you ran `passcode set` | Whoever can read the data directory |
| Notification credentials (Bark key, Telegram token) | `.env` or the systemd environment file | Whoever can read that file |

The data directory is `./data` next to `compose.yaml` for the container (root-owned, mode 0700) and `~/.local/share/iphone-control` for a source install.

## Design choices that limit exposure

- No network listener by default. WebDriverAgent is reached through usbmuxd; `wda-bridge` only binds 127.0.0.1, only forwards requests whose Host header is a loopback name, and is meant to be stopped after use.
- The control socket is a Unix socket with mode 0600, not TCP.
- The container is not `privileged`: it is only allowed USB character devices (major 189), and the tunnel to the phone uses a user-space TCP stack, so no network interface or routing changes on the host.
- Auto-unlock is off by default. When enabled, it stops for good after a single wrong attempt (it writes a stop marker before entering the second digit) and sends a notification; it never retries a passcode that might be wrong. It does not work before the first unlock after a reboot (BFU) — that always needs a person.
- Dependencies are pinned to exact versions, `Cargo.lock` is committed, base images are pinned by digest, and the WebDriverAgent download is checked against a sha256. CI runs gitleaks on every push.

## Known risks and recommendations

- **The container runs as root.** It needs read-write access to the USB device nodes and must restart `usbmuxd` after the phone is re-plugged (the container has no udev). Running it unprivileged would need host udev rules and is not supported yet. Keep the host itself locked down.
- **Third-party anisette server.** Apple ID logins need "anisette" data that normally only Apple hardware produces. By default it comes from a public server (`https://mac-ani.jkcoxson.com`, the default of the Impactor/omnisette libraries). The server does not receive your Apple ID password, but your logins depend on an identity it provisions, and it sees when you log in. Set `ICTL_ANISETTE_URL` to an [anisette-v3 server](https://github.com/Dadoum/anisette-v3-server) you run yourself to avoid this.
- **Use a dedicated Apple ID.** The session token and signing key on disk are enough to manage that account's development certificates and devices. A separate free account limits the damage if the host is compromised and keeps your main account clear of developer-account restrictions.
- **Auto-unlock stores your passcode in plain text.** Anyone with root on the host, or access to backups of the data directory, can read it — and the passcode is also what protects the phone's data and Apple ID. Only enable it on a host you fully control, and prefer leaving Auto-Lock off on a phone dedicated to automation.
- **Agents act with your hands.** An agent with these tools can send messages, make purchases and change settings. The MCP server tells agents to ask before paying, deleting, sending messages or changing account or privacy settings, but that is an instruction, not an enforcement mechanism. Use a phone and accounts you are comfortable automating.
- **Back up the data directory securely**, and never commit it. `.gitignore` and `.dockerignore` exclude `data/`, `.env` and key material, so they don't end up in git or in image layers.

## Developer Disk Image and WebDriverAgent sources

- The Developer Disk Image is downloaded at runtime from a community-maintained mirror (the same source pymobiledevice3 uses) unless you set `ICTL_DDI_URL` or `ICTL_DDI_DIR`. The phone verifies Apple's signature through personalization (TSS), so a tampered image will not mount.
- The unsigned WebDriverAgent comes from Appium's official release (pinned version, sha256-checked) and is signed locally with your Apple ID.
