# Auto-unlock

Off by default. When enabled, MCP actions unlock the phone with your saved passcode if it is locked, and so do the read-only screenshot, element-list and `iphone_wait_for` tools. Native control (iOS 27 and later) follows the same rules as WDA; a device on WDA isn't unlocked while WDA is unavailable.

::: danger Understand the risk first
The passcode is stored **in plain text** in the data directory. Anyone who can log in to the host, read backups of the data directory, or call this MCP server can unlock your phone. Only enable it on a host you fully control, preferably with a phone dedicated to automation.
:::

## Enable

The first-run guide of `iphone-control` offers it as an optional step; later, run `iphone-control setup --reconfigure` and pick Auto-unlock passcode. Or use the command directly:

```bash
iphone-control passcode set
```

The passcode is saved for the device the daemon is controlling; for another one add `--device <UDID or name>`. Numeric passcodes of 4 or more digits only. To disable:

```bash
iphone-control passcode clear           # the controlled device; --device for another
iphone-control passcode clear --all     # all devices
```

## Several devices

Passcodes are **kept per device** (`secrets/passcodes/<UDID>` in the data directory), so one device's passcode is never typed into another; a device without one needs a person to unlock it. The `devices` command (or the `iphone_list_devices` tool) shows which devices have one.

Older versions kept a single passcode (`secrets/passcode`) without knowing which device it belonged to. After upgrading, it is assigned to the device if exactly one is connected over USB; while several are connected it isn't used — run `passcode set` again with the right device connected.

## Safeguards

![Auto-unlock safety guardrail flow: check this device's disabled record and BFU first, load per-UDID passcode, write the device's record before digit 2, verify every digit, and disable this device on failure or crash](/diagrams/auto-unlock-en-light.svg){.light-only}
![Auto-unlock safety guardrail flow: check this device's disabled record and BFU first, load per-UDID passcode, write the device's record before digit 2, verify every digit, and disable this device on failure or crash](/diagrams/auto-unlock-en-dark.svg){.dark-only}

- After every digit it checks how many digits the passcode field shows and stops at once if they don't match (an incomplete entry is not a wrong attempt); only a first digit that didn't register is retried, up to 3 rounds.
- If the phone is still locked after the full passcode, auto-unlock is **disabled immediately** and you are notified. It never tries a second time, so it can't pile up wrong attempts and trigger data erasure.
- From the second digit on, a stop record for this device is written first (`secrets/unlock-disabled/<UDID>` in the data directory) and removed only once the phone is confirmed unlocked or the entry is confirmed incomplete. So an error during entry, the process being killed or the host losing power also leaves auto-unlock disabled.
- The stop records are **per device**: a failure on one device disables auto-unlock on that one only; the others keep unlocking.
- The first unlock after a reboot (BFU) always needs a person.
- A phone charging on its side goes into **StandBy**, which doesn't bring up the passcode keypad, so auto-unlock fails. Turn it off under *Settings → StandBy* when you use auto-unlock.

## Re-enable after it was disabled

Unlock the phone by hand first. If the passcode was wrong (for example, you changed it), save the new one and reset:

```bash
iphone-control passcode set
iphone-control unlock-reset
```

If it stopped because of an error during entry or a killed process, the passcode may be fine; once you've checked, `unlock-reset` alone is enough. `unlock-reset` doesn't check the passcode, and `passcode set` and `passcode clear` don't lift the stop. Don't delete files in the data directory to get going again: that removes this safeguard.

`unlock-reset` re-enables the device the daemon controls; for another one add `--device <UDID or name>` (it must be connected).

When you upgrade from 0.3.2 or earlier and the old version had disabled auto-unlock, it didn't record which device failed, so after the upgrade **every device stays disabled**: check each one's passcode and run `unlock-reset` for each (`doctor` says so).

::: tip Don't need auto-unlock?
Set *Settings → Display & Brightness → Auto-Lock* to *Never* on the phone, and it won't lock by itself.
:::
