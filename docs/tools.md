# MCP tools

The MCP server (`iphone-control mcp`) exposes 17 tools (23 with the [developer tools](#developer-tools-opt-in)). The tool descriptions the agent sees, and every result text, are in Simplified Chinese by default, or in English with `IPHONE_CONTROL_LANG=en` (see [configuration](https://derrors.github.io/iphone-control/en/guide/configuration)); this page is the English reference. The definitions live in [`crates/ictl-mcp/src/lib.rs`](../crates/ictl-mcp/src/lib.rs).

Coordinates are in **points**, the same unit as the pixels of a full-screen `iphone_screenshot` (for example 430×932 on an iPhone 15 Pro Max), so an agent can read a position off a screenshot and tap it directly. Coordinates follow the current screen orientation: after a rotation the same spot has different coordinates, so take a new screenshot. Screen sizes differ per device (an iPad mini 5 is 768×1024 in portrait); don't assume 430×932.

## Looking at the screen

| Tool | Parameters | Notes |
|---|---|---|
| `iphone_status` | — | WDA ready, locked or not, foreground app, screen size, daemon state |
| `iphone_screenshot` | `x`, `y`, `width`, `height` (all optional, points) | Full screen scaled to points. With a region: cropped from the full-resolution capture and enlarged; the result says the zoom factor and how to convert back to screen points |
| `iphone_list_elements` | `filter` (default `""`), `limit` (default 150) | Elements that carry text: type, label, id, value, center point and size. `filter` matches type, label, name or value, case-insensitive (for example `SearchField`). Visibility is judged by position, so something hidden behind a dialog can still be listed |
| `iphone_wait_for` | `text`, `timeout` (seconds, default 5, max 30) | Returns as soon as an element containing `text` appears; text typed into input fields does not count. Use this instead of polling with screenshots |
| `iphone_list_apps` | `include_system` (default false) | Name and bundle id of installed apps |

## Acting

| Tool | Parameters | Notes |
|---|---|---|
| `iphone_tap` | `x`, `y` | For targets without text (icons, games, maps); falls back to HID when WDA is down |
| `iphone_tap_element` | `text`, `index` (default 0) | Exact label or name first, then substring. If the element's center is covered (floating toolbar, keyboard, overlay), taps an uncovered part of it; if it is fully covered, does not tap and says so |
| `iphone_long_press` | `x`, `y`, `seconds` (default 1, max 10) | Holds one finger still, then lifts: context menus, home-screen quick actions, text selection |
| `iphone_swipe` | `from_x`, `from_y`, `to_x`, `to_y`, `seconds` (default 0.3, max 10) | Returns after the list has stopped scrolling (up to 2.5 s). Swipe up (`from_y` > `to_y`) to see content further down |
| `iphone_type_text` | `text`, `submit` (default false) | Types into the focused field (Chinese supported); fails if no keyboard is shown. Reads the field back afterwards: dropped trailing characters are typed again; if the field still doesn't match, it reports an error and does not press Return |
| `iphone_press_button` | `button`: `home`, `volume_up`, `volume_down`, `lock` | `home` refuses to act while a system alert is on screen (it would have no effect). `home` is sent as an IOHID AC Home event and waits until the home screen is in front; `lock` presses the power key over IOHID. Each falls back to the XCTest/WebDriverAgent way if it had no effect |
| `iphone_launch_app` | `app` (bundle id or display name, e.g. `设置`) | Cold-launches the app, or brings it back to where it was if it is already running. Waits until it is in the foreground; returns early and says so if a system alert is blocking it |
| `iphone_terminate_app` | `app` | Force-quits, like swiping it away in the app switcher; unsaved input is lost. No error if it wasn't running |
| `iphone_open_url` | `url` | Web pages open in the default browser; URL schemes open their app |

## Several devices

Only one device is controlled at a time; every other tool acts on it. With one device connected nothing changes.

| Tool | Parameters | Notes |
|---|---|---|
| `iphone_list_devices` | — | Every iPhone/iPad on USB: name, model, iOS version, UDID; marks the one being controlled and whether it has an auto-unlock passcode |
| `iphone_select_device` | `device`: UDID, device name, or `auto` | Switches to that device and keeps controlling it, also after a restart. A unique substring of the name works too. `auto` goes back to automatic choice: the controlled device is kept until it disconnects. WDA has to start on the new device (10–60 s). Refused when `IPHONE_UDID` is set |

## Developer tools (opt-in)

With `IPHONE_CONTROL_DEV_TOOLS=1` in the MCP server's environment, six more tools are listed. They are off by default, so agents that don't need them don't pay for their descriptions.

| Tool | Parameters | Notes |
|---|---|---|
| `iphone_app_logs` | `process`, `grep`, `since` (seconds, default 60, at most 3600), `limit` (default 100, 1 to 1000), `errors_only` | The phone's unified log (os_log) at the default, Error and Fault levels, one line per entry: seconds ago, `process[pid]`, level, subsystem, message. `process` is an app (name or bundle ID) or a process name. Read-only. See the recording notes below the table |
| `iphone_crash_reports` | `filter`, `name` | Lists crash reports, newest first (at most 50); with `name`, returns that report (`.ips`, cut at 40,000 characters). Read-only |
| `iphone_install_app` | `path` | Installs a signed `.ipa` or `.app` from a path on the machine that runs the daemon (inside the container for a container deployment). Destructive: it replaces an installed copy of the same app |
| `iphone_uninstall_app` | `app` | Removes an app and its data. `app` must be a bundle ID or the full display name (case-insensitive); an empty, partial or ambiguous name is refused and the candidates are listed. Refuses to remove the WebDriverAgent runner. Destructive |
| `iphone_web_targets` | — | Inspectable web pages and WebViews over the Web Inspector service: Safari tabs and inspectable WebViews in apps (debug builds of Capacitor, Cordova, React Native…). One line each: id, type, app, title, URL. Only apps in the foreground answer; needs Settings → Safari → Advanced → Web Inspector. Read-only |
| `iphone_web_eval` | `expression`, `target`, `timeout` (10 s, at most 60) | Runs JavaScript in a page and returns the result as JSON; a promise is awaited, a DOM node comes back as its description (`"<body>"`). `target` is an id from `iphone_web_targets`, or text in the URL or title; it can be left out when there is one page. Destructive: a script can change the page, submit forms and send requests |

How `iphone_app_logs` records:

- The first call starts recording, so earlier lines aren't available: call it once, reproduce the problem, call it again.
- Debug and Info messages are left out, because streaming them makes every process on the phone produce them and slows it down.
- The buffer keeps the last 20,000 lines and at most 8 MB of text, and each message is cut to 600 characters. When it doesn't reach back as far as `since`, the result says how old its oldest line is.
- Recording stops after 15 minutes without a call, when the daemon switches to another device, and when the device goes offline.

See [SECURITY.md](../SECURITY.md#developer-tools) before turning these tools on.

## Auto-unlock and "read-only" tools

When auto-unlock is configured for the controlled device, a locked phone is unlocked first by the action tools and by `iphone_screenshot`, `iphone_list_elements` and `iphone_wait_for`. `iphone_status`, `iphone_list_apps`, `iphone_list_devices`, `iphone_select_device`, the developer tools and `iphone_press_button` with `lock` never unlock it. While WDA is unavailable nothing is unlocked.

Every tool carries MCP annotations: the six read-only tools (`iphone_status`, `iphone_screenshot`, `iphone_list_elements`, `iphone_list_apps`, `iphone_wait_for`, `iphone_list_devices`) are `readOnlyHint: true`, and with the developer tools on so are `iphone_app_logs`, `iphone_crash_reports` and `iphone_web_targets`, nine in all. "Read-only" means they don't change what's on screen; with auto-unlock on, `iphone_screenshot`, `iphone_list_elements` and `iphone_wait_for` still unlock the phone first. The action tools are not read-only, and taps, long-presses, `iphone_terminate_app` and `iphone_batch` are also marked `destructiveHint: true`, since what they hit on screen can delete or send things. So are `iphone_web_eval`, `iphone_install_app` and `iphone_uninstall_app`.

## Returning the screen after an action: `then`

The tools under *Acting* and `iphone_batch` accept the following options. `iphone_select_device` does not: use `iphone_status` after switching to check that WDA is ready.

| Parameter | Meaning |
|---|---|
| `then` | `none` (default), `elements` (up to 80 elements) or `screenshot` |
| `then_filter` | With `elements`: only elements matching this text or type |
| `then_region` | With `screenshot`: `[x, y, width, height]` to return only an enlarged region (for example a game board) |
| `then_settle` | Seconds to wait for the screen to settle first (default 1.5, max 10) |

"Settled" means two consecutive element trees match and no loading spinner is shown. On screens with few elements (games, maps) it compares screenshots instead. The result says how long it waited, whether something is probably still loading, and whether a dialog is in the way.

Without `then`, the result still warns when a system alert covers the screen, because taps and swipes would land on the alert.

## Batches: `iphone_batch`

`steps` is a list of up to 20 objects, each `{"tool": <name>, ...parameters}`. Allowed tools: `iphone_tap`, `iphone_tap_element`, `iphone_long_press`, `iphone_swipe`, `iphone_type_text`, `iphone_press_button`, `iphone_launch_app`, `iphone_terminate_app`, `iphone_open_url`, `iphone_wait_for`, and `{"tool": "wait", "seconds": n}` (n ≤ 5).

Steps run in order. The batch stops at the first error or the first `iphone_tap_element` that finds nothing, and reports which step it stopped at. `then` applies once, after the last step. `reuse_elements: true` lets `iphone_tap_element` reuse the element tree from the last 10 seconds; use it only when the steps don't change the layout (for example pressing keys on the same keypad).

If the MCP client cancels the call, the remaining steps don't run; a request still queued for the device is dropped too. If no device was selected and the controlled device changes during a batch (for example it was unplugged and another one took over), the batch stops with an error.

```json
{
  "steps": [
    {"tool": "iphone_launch_app", "app": "设置"},
    {"tool": "iphone_tap_element", "text": "通用"},
    {"tool": "iphone_tap_element", "text": "关于本机"}
  ],
  "then": "screenshot"
}
```

## HID fallback

When WDA can't be reached, screenshots, taps, swipes and buttons fall back to CoreDevice HID (iOS 27 or later). The limits on landscape, screen size and redoing an action are in the [Tools guide](https://derrors.github.io/iphone-control/en/guide/tools#when-wda-is-down-the-hid-fallback).

## Tips for agents

- Prefer `iphone_tap_element` over coordinates when the element has text.
- Games, maps and video have no useful element tree: work from screenshots and zoom into small icons with a region.
- Ask the user before anything that pays, deletes, sends a message, or changes account or privacy settings (the server's instructions say the same).
