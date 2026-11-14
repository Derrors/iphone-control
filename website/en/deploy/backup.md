# Data and Backups

## What's in the data directory

| Path | Contents | If lost |
|---|---|---|
| `account/` | Apple ID session, signing key and certificate | Log in again; a new certificate is created |
| `wda/` | Signed WebDriverAgent | Re-signed automatically |
| `ddi/` | Developer Disk Image cache | Downloaded again |
| `secrets/passcodes/` | Per-device passcodes (only with auto-unlock) | Run `passcode set` again |
| `viewer-tokens.json` | The viewer's login tokens (keep them like passwords, out of backups that sync elsewhere) | The viewer makes a new `owner` on its next start; browsers and MCP clients that logged in need the new one |
| `viewer-tls/` | The self-signed certificate and key for the viewer's HTTPS on the local network (secret too) | Made again with a new fingerprint: browsers warn once more, and `--ca` needs the new certificate |
| `tasks/` | The last 50 AI tasks | The page no longer shows earlier tasks; nothing else |
| `state.json` | Signed WDA details (Team ID, bundle id, profile expiry), selected device, each device's screen size, last signing result | The daemon starts as if WDA isn't signed: run `sign` or `setup` again, and `select` again if you had selected a device |
| `logs/` | Logs, auto-unlock's stop marker `UNLOCK-DISABLED` | The logs don't matter. Losing the marker removes a safeguard: after a wrong passcode, the same one could be entered again |

Location: `~/.local/share/iphone-control` (mode 0700), on Linux and macOS alike, whether installed with the one command, Homebrew, the .deb or from source. Pairing records aren't in it; the system usbmuxd keeps them, in `/var/lib/lockdown` on Linux and `/var/db/lockdown` on macOS (shared with Finder). Settings are in `~/.config/iphone-control/env`, which holds notification secrets: don't share it either.

::: danger These are secrets
The data directory can log in to your Apple ID developer account, connect to the phone without confirmation, and — with auto-unlock — contains the passcode in plain text. Never commit or share it, and keep backups encrypted.
:::

## Back up

```bash
iphone-control service stop
tar czf iphone-control-data.tar.gz -C ~/.local/share iphone-control
cp ~/.config/iphone-control/env iphone-control-env      # settings
sudo tar czf lockdown.tar.gz /var/lib/lockdown          # pairing records, Linux only
iphone-control service start
```

If the daemon runs in the foreground, stop it with Ctrl-C in its terminal first. On macOS, pairing records belong to the system and don't need a backup; on a new Mac, tap *Trust* on the phone once more.

## Move to a new host

Install on the new host first (see [Getting Started](../guide/getting-started#install)), but don't run `iphone-control` yet. Restore the data directory and the settings file (and `/var/lib/lockdown` on Linux) to the same locations, then run `iphone-control`. No new login is needed, and no new pairing if you restored the pairing records.

## Moving from the Docker deployment

From the next release on there is no Docker image. Your data is in `./data` next to the compose file (or wherever `DATA_DIR` pointed), owned by root. After installing the new version, and before running `iphone-control` for the first time:

```bash
docker compose down                                  # in the directory of your old compose.yaml
mkdir -p ~/.local/share
sudo cp -a ./data ~/.local/share/iphone-control
sudo chown -R "$(id -un)": ~/.local/share/iphone-control
```

If `~/.local/share/iphone-control` already exists (for example because you already ran `iphone-control`), run `iphone-control service stop`, rename it to keep it, then copy.

- Move the settings from `.env` to `~/.config/iphone-control/env` (same `KEY=value` format) with mode 0600; see [Configuration](../guide/configuration#settings-file). Or just run `iphone-control` and let the guide ask again.
- The container's pairing records are in `./data/lockdown/`, which the system usbmuxd doesn't read. On the first run the phone asks "Trust This Computer?" once more, and the guide walks you through it.
- `ICTL_MANAGE_USBMUXD` and `ICTL_USBMUXD_BIN` are gone. On Linux, install the system usbmuxd (`sudo apt install usbmuxd`; the .deb pulls it in). If you ran `sudo systemctl mask usbmuxd` for Docker, run `sudo systemctl unmask usbmuxd` first.
