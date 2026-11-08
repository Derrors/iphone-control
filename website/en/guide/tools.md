# Tools

The MCP server provides 17 tools. Coordinates are in **points**, matching the pixels of a full-screen screenshot (430×932 on an iPhone 15 Pro Max), so the agent can tap what it reads off a screenshot. Coordinates follow the screen orientation: after a rotation the same spot has different coordinates, so take a new screenshot. Screen sizes differ per device; an iPad mini 5 is 768×1024 in portrait.

## Looking at the screen

| Tool | What it does |
|---|---|
| `iphone_status` | WDA ready or not, locked or not, foreground app, screen size |
| `iphone_screenshot` | Screenshot; with `x`, `y`, `width`, `height` it returns an enlarged region for small text and icons |
| `iphone_list_elements` | Elements with text: type, text, center, size; `filter` by text or type |
| `iphone_wait_for` | Wait until some text appears (page loaded, dialog shown); text inside input fields doesn't count. `timeout` defaults to 5 seconds, max 30 |
| `iphone_list_apps` | Installed apps; read-only, no `then` |

With [auto-unlock](./auto-unlock) on, `iphone_screenshot`, `iphone_list_elements` and `iphone_wait_for` also unlock a locked phone first. They are marked read-only because they don't change what's on screen. `iphone_status` and `iphone_list_apps` don't unlock it.

## Acting

| Tool | What it does |
|---|---|
| `iphone_tap` | Tap coordinates |
| `iphone_tap_element` | Tap by text; if the center is covered by a floating bar or the keyboard it taps a visible part, and if it's fully covered it doesn't tap |
| `iphone_long_press` | Long press; `seconds` defaults to 1, max 10 |
| `iphone_swipe` | Swipe; returns once the list has stopped scrolling. `seconds` defaults to 0.3, max 10 |
| `iphone_type_text` | Type into the focused field (Chinese supported), read it back and fix dropped characters; `submit` presses Return |
| `iphone_press_button` | `home`, `volume_up`, `volume_down`, `lock` |
| `iphone_launch_app` / `iphone_terminate_app` | Launch / quit an app by name (e.g. `设置`) or bundle id |
| `iphone_open_url` | Open a link or URL scheme |
| `iphone_batch` | Run up to 20 steps in one call, stopping at the first error. If the MCP client cancels the call, the remaining steps don't run; with no device selected, it also stops with an error if the controlled device changes |

## Several devices

Only one device is controlled at a time; every other tool acts on it. With one device connected you can ignore these.

| Tool | What it does |
|---|---|
| `iphone_list_devices` | Every iPhone/iPad on USB: name, model, iOS version, UDID; marks the one being controlled and whether it has an auto-unlock passcode |
| `iphone_select_device` | Switch to another device (UDID or name), remembered across restarts; `auto` goes back to automatic choice. WDA takes 10–60 s to start on the new device. Not possible when `IPHONE_UDID` is set |

Details: [Several devices](./multi-device).

## Developer tools (opt-in)

Set `IPHONE_CONTROL_DEV_TOOLS=1` for the MCP server to get four more tools. They are off by default, so agents that don't need them don't pay for their descriptions.

| Tool | What it does |
|---|---|
| `iphone_app_logs` | Read the phone's log, filtered by app or process, text and time. Recording starts with the first call: call it, reproduce the problem, call it again |
| `iphone_crash_reports` | List crash reports, or read one |
| `iphone_install_app` | Install a signed `.ipa` or `.app` (a path on the machine that runs the daemon) |
| `iphone_uninstall_app` | Remove an app and its data; never the WebDriverAgent runner |

## See the screen after an action: `then`

The tools under *Acting* accept `then`: it waits for the screen to settle and returns it, saving a round trip.

| Parameter | Meaning |
|---|---|
| `then` | `elements` (element list) or `screenshot` |
| `then_filter` | Only elements containing this text (or of this type) |
| `then_region` | Only an enlarged `[x, y, width, height]` region of the screenshot, e.g. a game board |
| `then_settle` | How long to wait for the screen to settle, in seconds (default 1.5, max 10) |

The result says how long it waited, whether the page is probably still loading, and whether a dialog is in the way. Even without `then`, it warns when a system alert covers the screen.

## When WDA is down: the HID fallback

When WDA can't be reached, screenshots, taps, swipes and buttons go through CoreDevice HID, with these limits:

- It needs iOS 27 or later. Older iOS has no such path, and actions fail while WDA restarts.
- **Landscape:** before each touch the screen is captured again to check its orientation, and the daemon also needs a matching orientation from WDA in the last 30 seconds. Without one, for example right after a daemon restart, the touch is refused rather than guessed. Screenshots and buttons still work. Don't flip the device to the other side during an outage; wait for WDA to recover first.
- **Screen size:** while WDA is down, taps and swipes return an error if there is no record of this device's size. Wait for WDA to come up once, or set [`SCREEN_W`/`SCREEN_H`](./configuration#advanced).
- **No redo:** if a request already reached WDA and the connection then dropped or the response was incomplete, it doesn't fall back to HID, because the action may have happened and redoing it would tap or type twice. The tool returns an error; the agent should take a screenshot first.

All parameters: [docs/tools.md](https://github.com/Derrors/iphone-control/blob/main/docs/tools.md) in the repository.
