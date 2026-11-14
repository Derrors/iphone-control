# Viewer and Remote Use

`iphone-control viewer` is the control page in a browser: watch the live picture, tap, swipe and type on it, see the device panel, hand tasks to a local Claude Code or Codex, and pause, take over or confirm each step. This page covers opening it, using it from the local network and other machines, and how other agent platforms connect. Every command and option is listed in [CLI](./cli).

## Opening the viewer

```bash
iphone-control viewer          # in this terminal; Ctrl-C stops it
iphone-control viewer token    # in another one: the login token and a link that logs in
```

By default the viewer listens on `127.0.0.1:8200` only (change the port with `--port` or `IPHONE_CONTROL_VIEWER_PORT`). **Every HTTP visit needs a login, from this machine too**: the viewer can't tell your own browser from a visit forwarded by a reverse proxy such as frp, nginx or cloudflared, since both come from `127.0.0.1`.

- **Browsers**: open the link `viewer token` prints (`http://127.0.0.1:8200/?token=…`), or paste the token on the login page. A browser stays logged in for 30 days, and the token leaves the address bar.
- **Programs**: send `Authorization: Bearer <token>`.
- The CLI, `iphone-control mcp` over stdio and `task run` use the local Unix socket and need no login.

Tokens are kept in `viewer-tokens.json` in the data directory (0600). A token is a key: don't paste it into chats or configuration that gets shared.

## Tokens: named and read-only

The first token, `owner`, may do everything. Add named tokens, one per person or device, say:

```bash
iphone-control viewer token add living-room-tv --read-only   # read-only; shows it and links that log in
iphone-control viewer token add alex                         # may operate, like owner
iphone-control viewer token list                             # names and roles (not the tokens)
iphone-control viewer token show alex --new                  # replace it
iphone-control viewer token remove living-room-tv            # browsers that used it are out at once
```

- A **read-only token** can watch the picture, the device's state, apps, elements, and tasks with their steps; taps, typing, starting, pausing or confirming tasks, switching devices and `/mcp` are refused, and a locked phone isn't unlocked for it (reading the elements says it is locked). The page shows "Read-only" in its header and hides the text field and buttons.
- The task log records which token controlled a task, e.g. `viewer:alex`.
- Adding, replacing and removing tokens **takes effect at once**, without restarting the viewer; a browser that used a removed or replaced token is back at the login page on its next request, and the picture, task events, `/mcp` streams and task leases opened with it end within 5 s.
- The last token that may operate can't be removed; replace it with `show <name> --new`.

## The local network and HTTPS

```bash
iphone-control viewer --lan    # or IPHONE_CONTROL_VIEWER_LAN=1 in the settings file
```

- Same port, listening on the local network too. Only connections from loopback, private ranges (10/8, 172.16/12, 192.168/16), link-local and 100.64/10 (Tailscale) are accepted; `Host` must be one of this machine's names or addresses, and `--lan-host` adds another name.
- **HTTPS on the network**: browsers only offer WebCodecs on HTTPS or localhost, so only over HTTPS can the picture be H.264 or the phone's own HEVC (over HTTP it falls back to MJPEG at several MB/s), and the token and picture are encrypted. `http://` from the network is redirected to `https://`; `http://127.0.0.1` on this machine works as before.
- **The certificate** is a self-signed one made on this machine (in `viewer-tls/` in the data directory, kept; made again when a new address shows up). The first time, the browser warns that it isn't trusted: in the warning's certificate details, check that the SHA-256 fingerprint matches the one printed at startup or by `iphone-control service status`, then go on.
- **No warning**: use a certificate of your own with `--tls-cert`/`--tls-key` (or `IPHONE_CONTROL_VIEWER_TLS_CERT`/`_KEY`), such as one from a local CA made with mkcert and installed on your computer, or a real one from `tailscale cert`.
- `--tls off` turns HTTPS off; see [Configuration](./configuration).

Without `--lan`, an SSH tunnel works too: `ssh -L 8200:127.0.0.1:8200 <user>@<host>`, then open `http://127.0.0.1:8200/`.

## As a background service

```bash
iphone-control service viewer install   # starts at login; reads IPHONE_CONTROL_VIEWER_* from the settings file
iphone-control service status           # whether it runs, and the HTTPS certificate's fingerprint
```

Installing keeps this shell's `PATH` so the viewer finds `claude` and `codex`; install again if `PATH` changes. After changing `IPHONE_CONTROL_VIEWER_*` settings, `service viewer restart`. An update stops the viewer first and starts it again afterwards; an automatic update waits while an AI task runs.

## From another machine

The phone is connected to one machine (a NAS, say) and you use it from another:

| To | Do |
|---|---|
| Watch and act in a browser | Open `https://<that machine>:8200/?token=…` (the viewer runs with `--lan`) |
| Use an MCP client that takes a URL | `https://<that machine>:8200/mcp` with `Authorization: Bearer <token>`; see [Connect an AI Client](./mcp-clients#connect-by-url-http) |
| Use an MCP client that only starts a command | Install iphone-control on this machine and use `iphone-control mcp --url https://<that machine>:8200` (plus `--ca cert.pem`), with the login token in `IPHONE_CONTROL_VIEWER_TOKEN`. It bridges stdio to that machine's `/mcp` |
| Start AI tasks on that phone | `iphone-control task run --url https://<that machine>:8200 -- <command>`; see the next section |
| Follow and control tasks there | `iphone-control task list`, `events <id>`, `pause`, `approve <id>`… all with `--url` (or `IPHONE_CONTROL_URL`) |

With a self-signed certificate, the other machine has to trust it: copy `~/.local/share/iphone-control/viewer-tls/cert.pem` from the machine with the phone, give iphone-control commands `--ca cert.pem` (or `IPHONE_CONTROL_CA`), and start Node-based clients with `NODE_EXTRA_CA_CERTS=cert.pem`. If that computer uses a proxy, add this machine's address to `NO_PROXY`.

## For agent platforms: AI tasks

An AI task lets the control page show each step of an agent and pause, take over or stop it, and the agent waits for you before paying or sending a message; while a task runs, other clients can look but not act on the phone.

**On the same machine**:

```bash
iphone-control task run --title "Tidy the photos" [--app Photos] -- <command that starts the agent>
```

`task run` opens the task, holds its lease and runs the command with the task's token (`IPHONE_CONTROL_TASK_TOKEN`); an `iphone-control mcp` the command starts takes part in the task. The task ends when the command does; when the task is stopped elsewhere, so is the command. `--app` limits the task to those apps: in any other app each step waits for you first.

**On another machine**: add `--url`, with the login token in `IPHONE_CONTROL_VIEWER_TOKEN`:

```bash
export IPHONE_CONTROL_VIEWER_TOKEN=<token>
iphone-control task run --url https://<that machine>:8200 --ca cert.pem -- <command that starts the agent>
```

The command gets the task's token, `IPHONE_CONTROL_URL` and `IPHONE_CONTROL_MCP_URL`, and **not the login**. Its agent can keep a plain `iphone-control mcp`: seeing `IPHONE_CONTROL_URL`, it bridges to that machine with the task's token only. It can also use `$IPHONE_CONTROL_MCP_URL` with the header `Iphone-Control-Task-Token` over HTTP directly.

**Acting and controlling apart**: the agent has only the task's token, so it acts on the phone for that task but can't pause, resume, take over or approve its own confirmations; a person does that on the control page, or the platform with its login. On the same machine, control requests from the agent's processes (and those they start) are refused as well.

**A platform without iphone-control** calls the viewer's API (all with the login token, except the last two):

| Endpoint | What |
|---|---|
| `POST /api/agent/tasks/open`, `{title, owner?, apps?}` | Opens a task. Answers with an event stream: `opened` with `{id, token}` first, then the task's events. **The open connection is the lease**: when it closes, the task is abandoned; a comment every 15 s keeps proxies from closing it |
| `GET /api/agent/tasks` | Recent tasks |
| `GET /api/agent/tasks/<id>/events` | A task's events (replayed, then followed) |
| `POST /api/agent/tasks/<id>/pause` (`resume`, `takeover`, `handback`, `stop`) | Controls the task |
| `POST /api/agent/tasks/<id>/confirm`, `{"approve": true}` | Answers a confirmation |
| `/mcp` with the header `Iphone-Control-Task-Token` | For the agent: its task's token alone, no login |
| `POST /api/agent/tasks/finish`, `{ok, summary}`, the same header | For the agent: ends the task with a summary |

## Through a reverse proxy or tunnel

When frp, nginx or cloudflared forwards the viewer to the internet:

- The login still applies; a token is 256 random bits, and after 20 wrong tokens in a minute wrong ones get 429 for the rest of it (a right token still gets in, so someone trying tokens can't lock you out).
- Use HTTPS (a real certificate on the proxy), or the token and cookie cross the internet in the clear.
- Something like Tailscale, which only lets your own devices in, is much safer than exposing the port to the internet.

## Security

- A token that may operate can watch the screen, control the phone and run tasks with this machine's Claude Code / Codex login: give it only to people you trust, and if it leaks, `viewer token show <name> --new` or `remove` it at once. Give a read-only token to people who only need to watch.
- Processes of the same system user can read each other's files: an agent on this machine can read the token file, but its control requests are refused by their process. This isn't a strict boundary; for real isolation, run the agent as another system user or in a sandbox.
- See also [Security and Privacy](./security).
