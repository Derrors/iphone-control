# Several Devices

A host can have several iPhones and iPads connected, but it controls one at a time: every tool and command acts on that one. With a single device you can skip this page.

Don't run two copies of iphone-control, or other usbmuxd-based tools, against the same device.

## List and switch

```bash
iphone-control devices                  # connected devices; marks the controlled one and whether it has an auto-unlock passcode
iphone-control select <UDID or name>    # control this one
iphone-control select auto              # back to automatic choice
```

Agents use `iphone_list_devices` and `iphone_select_device`, which do the same.

- Only devices connected over USB can be selected. It tries the UDID, then the device name, then a part of the name that matches exactly one device. If it matches several, you get an error and need the UDID.
- WDA takes 10–60 seconds to start after switching; check with `status` (or `iphone_status`) that it's ready. A device that has never had WDA needs [installing](#install-on-a-new-device) first.
- The control page, `iphone-control viewer`, can switch too; see [Switching on the control page](#control-page).

## Switching on the control page {#control-page}

The device picker at the top of the `iphone-control viewer` page switches the controlled device.

- **The page follows the daemon.** When the device is switched elsewhere (another tab, `iphone-control select`, the `iphone_select_device` tool, or automatically after an unplug), the page notices within about 10 seconds, says so, and moves its live picture and panels (apps, elements) to the new device.
- **Actions only go to the device the page shows.** Every action the page sends carries that device; if the daemon no longer controls it, the action is refused, never sent to another phone.
- **When the new device isn't ready**, the picture area says why:
  - Not connected: plug it in with a cable that carries data.
  - Not paired or Developer Mode off: run `iphone-control setup --reconfigure` in a terminal and pick Phone.
  - WDA starting: usually 10–60 seconds.
  - Not unlocked since a restart: unlock it by hand once; the first time UI automation is enabled after a restart, enter the passcode on the device.
- **A device without WDA:** with an Apple ID signed in, the picture area has a *Sign and install WDA* button. It takes about 1–2 minutes; keep the phone unlocked. Without an Apple ID, it tells you to run `iphone-control setup --reconfigure` and pick Apple ID.
- Only one device is controlled at a time; you can't watch two phones side by side.
- Switching is refused while an AI task runs, and when `IPHONE_UDID` pins a device.

## Which device is controlled

| Priority | Source | Behaviour |
|---|---|---|
| 1 | The `IPHONE_UDID` environment variable | Pins this device; `select` can't switch away |
| 2 | Selected with `select` or `iphone_select_device` | Saved in `state.json`, so it survives a restart |
| 3 | Automatic (nothing selected, or `select auto`) | The controlled device is kept until it disconnects, then another connected one takes over |

**When the selected device isn't connected**, the state is 手机未连接 (phone not connected). It does not switch to another device, so actions and auto-unlock can't land on the wrong one. After `OFFLINE_ALERT_MIN` minutes you also get an offline notification. The `选设备` line of `status` shows which device is selected, and `select auto` goes back to automatic choice.

**With automatic choice**, if the controlled device changes during an `iphone_batch` (for example it was unplugged and another took over), the batch stops with an error.

## Install on a new device

```bash
iphone-control select <new device>
iphone-control setup --reconfigure     # pick Phone: pairing, Developer Mode, signing and installing WDA
```

Setup only acts on the controlled device, so `select` first. Signing registers the device with your Apple ID, which uses up one of the account's device slots. Automatic re-signing doesn't register a device that has never had WDA; `doctor` tells you to run `setup` in that case. You can also switch to the new device on the control page and click *Sign and install WDA* (the device must already be paired, with Developer Mode on).

## Re-signing

Automatic re-signing only covers the controlled device. An idle device's profile expires after 7 days. Within a minute of switching back, it is checked and re-signed if it has expired or is about to; you can also run `resign --force`.

Every device is re-signed with the same certificate, so re-signing one doesn't break another. But if an idle device's WDA was signed on another machine (or from an old data directory), that certificate may have been revoked: when WDA won't start after you switch to it, the daemon re-signs it automatically, see the [FAQ](../troubleshoot/faq#cert).

## Auto-unlock

Passcodes are kept per device:

```bash
iphone-control passcode set --device <UDID or name>     # the device must be connected
```

The stop marker is global: if auto-unlock fails on one device, it stops on all of them until you run `unlock-reset`. See [Auto-unlock](./auto-unlock).

## Notifications

Notifications don't name the device. Use `status` or `devices` to see which one it is.

## iPad differences

- **Touch ID models:** swiping up on the lock screen shows a Touch ID prompt, and the keypad only appears after tapping *Use Passcode*. The daemon does that itself during auto-unlock.
- **Landscape:** coordinates follow the current orientation, and auto-unlock and the HID fallback convert them back to the device's native orientation. For the HID fallback's landscape limits, see [Tools](./tools#when-wda-is-down-the-hid-fallback).
- **No HID fallback on iOS 26:** actions fail while WDA restarts, the same as on an iPhone.
- **The screen isn't 430×932:** an iPad mini 5 is 768×1024 in portrait. Go by the screenshot and `iphone_status`.

Verified on: iPad mini 5, iOS 26.6.1 (Linux).
