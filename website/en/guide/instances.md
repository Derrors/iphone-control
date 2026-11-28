# Several Devices at Once (Instances)

One daemon controls one device at a time (see [Several Devices](./multi-device)). For two agents to control a device each without getting in each other's way, give every device an **instance**: a daemon, control socket, control page port and data directory of its own. What you had before is the **default instance**; it keeps running as it is.

## Adding an instance

Have the default instance control the device it keeps first, then give the other one to the new instance:

```bash
iphone-control devices                                   # the two devices' UDIDs
iphone-control select <the default instance's device>
iphone-control instance add ipad --device <the other one's UDID>
```

`instance add`:

- shows which device the default instance controls and which the new one gets, and asks (`--yes` doesn't);
- **pins** the default instance to the device it controls: beside named instances the default one no longer chooses a device by itself, and `select auto` is refused;
- writes the registration `~/.config/iphone-control/instances/ipad.env`, with the first free control page port from 8201 (`--port` gives one);
- sets up and starts the services `iphone-control@ipad` and `iphone-control-viewer@ipad` (`io.github.derrors.iphone-control.instance.ipad` on macOS) and waits for them; if they don't come up, it stops them and takes the registration back.

To register a device that isn't connected now, add `--force`. If it is interrupted, `instance list` says so; `instance add --resume ipad` continues, `instance remove ipad` clears it.

Before adding:

- the default instance's daemon runs, of this version;
- the settings directory is the default `~/.config/iphone-control` and the data directory the default `~/.local/share/iphone-control` (a changed `XDG_CONFIG_HOME`, `XDG_DATA_HOME` or `ICTL_DATA_DIR` isn't supported);
- if you signed in to an Apple ID, the anisette state's origin must be known: an older version didn't record it, and `instance add` asks you to run `iphone-control login --reprovision-anisette` first (or `--confirm-anisette-origin` if you know it came from the address configured now).

## Use

```bash
iphone-control instance list                     # one line per instance: device, state, control page port
iphone-control status --all                      # the same
iphone-control --instance ipad status            # any command for the ipad instance
iphone-control --instance ipad mcp-config        # the MCP registration, server name iphone-ipad
```

- Every command takes `--instance <name>`, or the environment variable `IPHONE_CONTROL_INSTANCE`. A `--socket` on the command line wins over an instance named in the environment or the settings file.
- `mcp-config` prints `iphone-control --socket <absolute path> mcp --local`: `--local` keeps it on that socket, whatever `IPHONE_CONTROL_URL` or instance name the settings file has. Register one per agent and each controls its own device.
- Control page `http://127.0.0.1:<port>/`: every instance takes the same login tokens and certificate, but logins are per instance.
- An instance's own settings (this device's notifications, say) go below the blank line of `instances/<name>.env`; an empty value clears the shared one. Above the blank line is what `instance add` wrote: don't change it.

## Shared and separate

| Shared | Separate |
|---|---|
| The Apple ID account, signing certificate, anisette state | The daemon, control socket, control page port |
| Passcodes, auto-unlock disabled records (per device) | `state.json`, tasks, logs |
| The control page's login tokens and certificate | The WDA package, developer disk image cache |

**A device belongs to one instance at a time.** Each instance takes the device's lease (`devices/<UDID>.lock`) before controlling it; while another holds it, the state says "This device is controlled by the instance X", `devices` marks it on its line, and it isn't read.

**The daily Apple ID session check** is run by one instance; another takes over when it stops, and a notification isn't sent twice.

**The WDA launched in the background** is recorded in the shared data directory: another instance taking the device over attaches to it without restarting WDA. A new instance taking over a device with WDA installed needs no signing, as long as this host's account signed it.

## Removing

```bash
iphone-control instance remove ipad            # stop its services, then remove them and the registration
iphone-control instance remove ipad --purge    # its data directory too
```

If its services don't stop, the registration stays and it says so. The default instance stays pinned to its device; with no named instance left, `select auto` goes back to automatic.

## Updating

`iphone-control update` takes every instance through it: it refuses while any instance has a task under way, opens no new task meanwhile, stops every instance's viewer and daemon, and starts what ran before once the program is replaced. If it is interrupted, run `iphone-control update recover`.

With named instances **don't downgrade to 0.3.2**: `instance remove` every named instance first.

## Limits

- One daemon doesn't control several devices, and no page shows them all.
- A daemon of root's and a user's controlling the same devices isn't supported.
- A named instance has no `service` commands or first-run guide: its services are `instance add`'s and `instance remove`'s.
