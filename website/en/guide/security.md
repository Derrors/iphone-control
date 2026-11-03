# Security and Privacy

iphone-control is a remote control for a phone: whoever can reach it can do anything the owner can do while the phone is unlocked. Treat the host like the phone itself.

## What is stored and who can reach it

| What | Where | Who can reach it |
|---|---|---|
| Phone control | Control socket (Unix socket, mode 0600) | Root on the host, anyone who can `docker exec` into the container, the same user for a source install |
| Apple ID session, signing key | `account/` in the data directory | Whoever can read the data directory. The Apple ID password is never stored |
| Pairing records | `lockdown/` in the data directory (Docker) | Whoever can read it; it lets the holder talk to the phone over USB without another *Trust* prompt |
| Passcode (only with auto-unlock) | `secrets/passcode`, **plain text** | Whoever can read the data directory |
| Notification keys | `.env` or `~/.config/iphone-control/env` | Whoever can read that file |

## Built-in protections

- **No network ports.** WDA is reached through usbmuxd. `wda-bridge` is an explicit opt-in that binds 127.0.0.1 only — but WDA has no authentication, so every local user can use it; stop it when done.
- **Not privileged**: the container only gets USB character devices.
- **Auto-unlock is off by default** and disables itself after one wrong attempt.
- Exact dependency versions, base images pinned by digest, and the WebDriverAgent download is sha256-checked.

## Risks to know about

- **The container runs as root.** It needs the USB device nodes and must restart usbmuxd after re-plugging (there is no udev in the container).
- **The anisette server.** Apple ID logins need anisette data, which by default comes from a public third-party server (`mac-ani.jkcoxson.com`). It never gets your password, but your logins depend on the identity it provisions and it can see when you log in. To avoid it, run your own [anisette-v3-server](https://github.com/Dadoum/anisette-v3-server) and set `ICTL_ANISETTE_URL`.
- **Use a dedicated Apple ID.** The session and signing key on disk are enough to manage that account's development certificates and devices; a separate account contains the damage if the host is compromised.
- **The agent acts with your hands.** It can send messages, buy things and change settings. The server tells agents to ask first for these, but that is an instruction, not enforcement.

## Disclaimer

This project is not affiliated with, endorsed by, or sponsored by Apple Inc. iPhone, iOS and Xcode are trademarks of Apple Inc. Only use it with devices and accounts you own or are authorized to control; automating third-party apps may violate their terms; Apple may limit free developer accounts. The software is provided "as is", without warranty of any kind.

Found a security problem? [Report it privately](https://github.com/Derrors/iphone-control/security/advisories/new). Full details: [SECURITY.md](https://github.com/Derrors/iphone-control/blob/main/SECURITY.md).
