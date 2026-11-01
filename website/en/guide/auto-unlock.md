# Auto-unlock

Off by default. When enabled, MCP actions unlock the phone with your saved passcode if it is locked.

::: danger Understand the risk first
The passcode is stored **in plain text** in the data directory. Anyone who can log in to the host, read backups of the data directory, or call this MCP server can unlock your phone. Only enable it on a host you fully control, preferably with a phone dedicated to automation.
:::

## Enable

```bash
docker exec -it iphone-control iphone-control passcode set
```

Numeric passcodes of 4 or more digits only. To disable:

```bash
docker exec -it iphone-control iphone-control passcode clear
```

## Safeguards

- After every digit it checks how many digits the passcode field shows and stops at once if they don't match (an incomplete entry is not a wrong attempt); only a first digit that didn't register is retried, up to 3 rounds.
- If the phone is still locked after the full passcode, auto-unlock is **disabled immediately** and you are notified. It never tries a second time, so it can't pile up wrong attempts and trigger data erasure.
- The first unlock after a reboot (BFU) always needs a person.

## Re-enable after it was disabled

If it was disabled because the passcode was wrong (for example, you changed it), save the new one and reset:

```bash
docker exec -it iphone-control iphone-control passcode set
docker exec -it iphone-control iphone-control unlock-reset
```

::: tip Don't need auto-unlock?
Set *Settings → Display & Brightness → Auto-Lock* to *Never* on the phone, and it won't lock by itself.
:::
