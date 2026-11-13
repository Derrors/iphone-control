# Configuration

Everything is configured with environment variables, and all of them are optional.

## Settings file {#settings-file}

Settings go in `~/.config/iphone-control/env`, one `KEY=value` per line, mode 0600 (it holds notification secrets: don't commit or share it). The first-run guide of `iphone-control` and `iphone-control setup --reconfigure` write it for you; you can also edit it by hand, following the template. The template is `packaging/env.example` in the repository, and a copy is installed:

- One-command install: `~/.config/iphone-control/env.example`
- Homebrew: `$(brew --prefix)/opt/iphone-control/share/iphone-control/env.example`
- .deb: `/usr/share/doc/iphone-control/env.example`

The daemon reads the file when it starts, and so does every other command (`login`, `notify-test`, `mcp-config`, and the `mcp` server the AI client starts), so there is no need to load it into your shell first. A variable that is already in the environment wins, so a value you set on the command line overrides the file.

After editing it by hand, restart the daemon:

```bash
iphone-control service restart
```

## Common

| Variable | Default | Meaning |
|---|---|---|
| `HTTP_PROXY` / `HTTPS_PROXY` | direct | Proxy for Apple, anisette, Developer Disk Image and notification traffic |
| `IPHONE_CONTROL_LANG` | `zh` | Language: `zh` or `en`. Applies to tool descriptions and results, CLI output, logs and notifications. The MCP server and the CLI each send their own setting and the daemon answers in it; logs and notifications follow the daemon's own setting. See the note below the table |
| `IPHONE_CONTROL_DEV_TOOLS` | off | `1` adds the [developer tools](./tools#developer-tools-opt-in) to the MCP server: logs, crash reports, installing and removing apps, and Web Inspector (JavaScript in web pages and WebViews). See [Security](./security) for the risks. See the note below the table |
| `IPHONE_CONTROL_VIEWER_CODEC` | `auto` | The control page's picture: `auto` uses the phone's own video (HEVC) on iOS 27 and later, else H.264 (on macOS with VideoToolbox directly, on Linux when ffmpeg is installed), GPU encoding first; `device` only the phone's own video, MJPEG where it can't be used; `mjpeg` only MJPEG; or name one of `nvenc`, `vaapi`, `qsv`, `v4l2m2m`, `videotoolbox`, `x264`. `iphone-control viewer` says which one it uses when it starts, see [CLI](./cli#viewer-picture) |
| `IPHONE_CONTROL_VIEWER_QP` | `20` | H.264 quality, 1 to 51: lower is sharper and larger. `20` looks like the source in normal use; `24` saves about a third of the traffic, small text blurs a little while scrolling fast |
| `IPHONE_CONTROL_VIEWER_LAN` | off | Set to `1` to open the viewer to the local network too, like `viewer --lan`. There is no login yet: anyone on the network who can reach this machine can control the phone and start AI tasks, so only on a trusted network |
| `IPHONE_UDID` | automatic | Pin the device to control. Unset: the controlled device is kept until it disconnects, and you can switch with the `select` command or the `iphone_select_device` tool. When set, switching is refused |
| `RESIGN_MIN_DAYS` | `2` | Re-sign when fewer days than this remain |
| `RESIGN_CHECK_HOURS` | `6` | How often to check; it is also checked within a minute after the phone reconnects or you switch devices |
| `OFFLINE_ALERT_MIN` | `10` | Notify after the phone has been offline this many minutes; `0` disables |

The AI client starts the MCP server, and variables set in its registration win over the settings file. The guide and `iphone-control mcp-config` put `IPHONE_CONTROL_LANG=en` and `IPHONE_CONTROL_DEV_TOOLS=1` into the registration command when they are set, so after changing either one, register again: run `iphone-control setup --reconfigure`, pick MCP registration, and open a new session.

For clients configured with JSON, see [Connect an AI Client](./mcp-clients#claude-desktop-cursor-and-others).

Notification variables: see [Notifications](./notifications).

## Advanced

| Variable | Meaning |
|---|---|
| `ICTL_ANISETTE_URL` | Your own anisette server (the default is a public third-party one; see [Security](./security)) |
| `ICTL_DDI_URL` | Developer Disk Image download URL; the default is a community-maintained mirror |
| `ICTL_DDI_DIR` | A Developer Disk Image directory you extracted from Xcode. The daemon doesn't download anything or delete its files, and the error names any missing file. When unset, the `ddi/` cache in the data directory is used, and it is deleted and downloaded again after 3 failed mounts in a row |
| `ICTL_WDA_UNSIGNED` | Path of the unsigned WDA. Default: `wda/WDA-unsigned.ipa` in the data directory, downloaded before the first signing. A path you set yourself is not downloaded |
| `SCREEN_W` / `SCREEN_H` | Screen size in points for the HID fallback, used only while WDA is down and there is no record of this device's size: WDA has never connected, or you switched to a device without one. It applies to every device, so take care with several |
| `ICTL_LOG` | Log filter, e.g. `info,ictl_wda::http=debug` logs every WDA request with its duration |
| `ICTL_FFMPEG` | Path to ffmpeg (the control page's H.264 picture; on macOS only the fallback for VideoToolbox). By default it is looked up on `PATH`, and on macOS in Homebrew's places too |

## Directories and control socket

You normally leave these alone. If you do change them, put them in the [settings file](#settings-file) and don't leave them empty: the daemon, the CLI and the MCP server need the same values. The MCP registration carries `--socket`, so after changing `ICTL_SOCKET`, register again (`setup --reconfigure`, MCP registration).

| Variable | Default | Meaning |
|---|---|---|
| `ICTL_DATA_DIR` | root: `/var/lib/iphone-control`<br>regular user: `$XDG_DATA_HOME/iphone-control`, or `~/.local/share/iphone-control` if unset | Data directory; same as the CLI's `--data-dir` |
| `ICTL_SOCKET` | root: `/run/iphone-control/ctl.sock`<br>regular user: `$XDG_RUNTIME_DIR/iphone-control/ctl.sock`, or in the data directory without that variable (macOS) | The daemon's control socket; same as the CLI's `--socket` |
| `XDG_DATA_HOME` / `XDG_RUNTIME_DIR` | set by the system | Not this project's variables; they only change the two defaults above for a regular user |
