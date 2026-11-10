# Security and Privacy

iphone-control is a remote control for a phone: whoever can reach it can do anything the owner can do while the phone is unlocked. Treat the host like the phone itself.

This page has the essentials. What is stored, who can reach it and the details of each risk are in the full [SECURITY.md](https://github.com/Derrors/iphone-control/blob/main/SECURITY.md).

## What is stored

- **The data directory** holds the Apple ID session, the signing key, pairing records and, with auto-unlock on, the passcode in **plain text**. Keep it and its backups as safe as a password. The Apple ID password is never stored.
- **The control socket** is a Unix socket with mode 0600, and whoever can connect to it controls the phone: root on the host and the user the daemon runs as.
- **Notification keys** are in the settings file `~/.config/iphone-control/env`, mode 0600.

## Built-in protections

- **No network ports.** WDA is reached through usbmuxd. `wda-bridge` and the live `viewer` are explicit opt-ins that bind 127.0.0.1 only, and other web pages in your browser can't act through them. They have no login, though: every local user can use them, including starting AI tasks from the viewer, so stop them when done.
- **No root.** The daemon runs as your regular user, as a per-user service; the install script refuses to run as root.
- **Auto-unlock is off by default** and disables itself after one wrong attempt.
- **Developer tools are off by default.**
- Exact dependency versions; the WebDriverAgent download and the install script's download are sha256-checked.

## Risks to know about

- **The anisette server.** Apple ID logins use a public third-party server by default (`mac-ani.jkcoxson.com`). It never gets your password, but it can see when you log in. To avoid it, run your own [anisette-v3-server](https://github.com/Dadoum/anisette-v3-server) and set `ICTL_ANISETTE_URL`.
- **Use a dedicated Apple ID.** A separate account contains the damage if the host is compromised.
- **The agent acts with your hands.** It can send messages, buy things and change settings. The server tells agents to ask first for these, but that is an instruction, not enforcement.
- **The developer tools add risk while they are on.** `iphone_web_eval` runs JavaScript in any inspectable page, Safari tabs included: it can read cookies and `localStorage` and send requests as the logged-in user. `iphone_install_app` accepts any path on the machine that runs the daemon. System logs and crash reports go into the agent's context and can contain private data. The `IPHONE_CONTROL_DEV_TOOLS` switch only decides whether the MCP server offers these tools; it is not a permission boundary. Turn it off when done.

## Disclaimer

This project is not affiliated with, endorsed by, or sponsored by Apple Inc. iPhone, iOS and Xcode are trademarks of Apple Inc. Only use it with devices and accounts you own or are authorized to control; automating third-party apps may violate their terms; Apple may limit free developer accounts. The software is provided "as is", without warranty of any kind.

Found a security problem? [Report it privately](https://github.com/Derrors/iphone-control/security/advisories/new).
