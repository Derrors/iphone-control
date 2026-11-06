# Configuration

Everything is configured with environment variables, and all of them are optional:

- **Docker:** in `.env` in the repository directory (copied from `.env.example`); apply with `docker compose up -d`.
- **Just a compose file:** under `environment:` in your `compose.yaml` (or a `.env` next to it plus `env_file`); apply with `docker compose up -d` as well.
- **Source install:** in `~/.config/iphone-control/env` (read by the systemd user service); apply with `systemctl --user restart iphone-control`.
- **macOS:** also in `~/.config/iphone-control/env`, loaded before starting the daemon; restart the daemon to apply. See [macOS](../deploy/macos#configuration).

## Common

| Variable | Default | Meaning |
|---|---|---|
| `HTTP_PROXY` / `HTTPS_PROXY` | direct | Proxy for Apple, anisette, Developer Disk Image and notification traffic |
| `IPHONE_UDID` | automatic | Pin the device to control. Unset: the controlled device is kept until it disconnects, and you can switch with the `select` command or the `iphone_select_device` tool. When set, switching is refused |
| `RESIGN_MIN_DAYS` | `2` | Re-sign when fewer days than this remain |
| `RESIGN_CHECK_HOURS` | `6` | How often to check |
| `OFFLINE_ALERT_MIN` | `10` | Notify after the phone has been offline this many minutes; `0` disables |

Notification variables: see [Notifications](./notifications).

## Advanced

| Variable | Meaning |
|---|---|
| `ICTL_ANISETTE_URL` | Your own anisette server (the default is a public third-party one; see [Security](./security)) |
| `ICTL_DDI_URL` / `ICTL_DDI_DIR` | Developer Disk Image download URL / local directory (extracted from Xcode) |
| `ICTL_MANAGE_USBMUXD` | Set to `0` when the host already runs usbmuxd, so the daemon doesn't start its own |
| `SCREEN_W` / `SCREEN_H` | Screen size in points for the HID fallback before WDA has ever connected |
| `ICTL_LOG` | Log filter, e.g. `info,ictl_wda::http=debug` logs every WDA request with its duration |

## Docker Compose only

These apply to the repository's own `compose.yaml`; in a compose file you wrote yourself, edit `container_name`, `image` and `volumes` directly.

| Variable | Default | Meaning |
|---|---|---|
| `CONTAINER_NAME` | `iphone-control` | Container name |
| `IMAGE` | `iphone-control:local` | Image; set `ghcr.io/derrors/iphone-control:<version>` for the prebuilt one |
| `DATA_DIR` | `./data` | Data directory; see [Data and Backups](../deploy/backup) |
