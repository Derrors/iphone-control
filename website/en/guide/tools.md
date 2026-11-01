# Tools

The MCP server provides 15 tools. Coordinates are in **points**, matching the pixels of a full-screen screenshot (430×932 on an iPhone 15 Pro Max), so the agent can tap what it reads off a screenshot.

## Looking at the screen

| Tool | What it does |
|---|---|
| `iphone_status` | WDA ready or not, locked or not, foreground app, screen size |
| `iphone_screenshot` | Screenshot; with `x`, `y`, `width`, `height` it returns an enlarged region for small text and icons |
| `iphone_list_elements` | Elements with text: type, text, center, size; `filter` by text or type |
| `iphone_wait_for` | Wait until some text appears (page loaded, dialog shown); text inside input fields doesn't count |

## Acting

| Tool | What it does |
|---|---|
| `iphone_tap` | Tap coordinates |
| `iphone_tap_element` | Tap by text; if the center is covered by a floating bar or the keyboard it taps a visible part, and if it's fully covered it doesn't tap |
| `iphone_long_press` | Long press |
| `iphone_swipe` | Swipe; returns once the list has stopped scrolling |
| `iphone_type_text` | Type into the focused field (Chinese supported), read it back and fix dropped characters; `submit` presses Return |
| `iphone_press_button` | `home`, `volume_up`, `volume_down`, `lock` |
| `iphone_launch_app` / `iphone_terminate_app` | Launch / quit an app by name (e.g. `设置`) or bundle id |
| `iphone_open_url` | Open a link or URL scheme |
| `iphone_list_apps` | Installed apps |
| `iphone_batch` | Run up to 20 steps in one call, stopping at the first error |

## See the screen after an action: `then`

Every action tool accepts `then`: it waits for the screen to settle and returns it, saving a round trip.

| Parameter | Meaning |
|---|---|
| `then` | `elements` (element list) or `screenshot` |
| `then_filter` | Only elements containing this text (or of this type) |
| `then_region` | Only an enlarged `[x, y, width, height]` region of the screenshot, e.g. a game board |
| `then_settle` | How long to wait for the screen to settle, in seconds (default 1.5, max 10) |

The result says how long it waited, whether the page is probably still loading, and whether a dialog is in the way. Even without `then`, it warns when a system alert covers the screen.

All parameters: [docs/tools.md](https://github.com/Derrors/iphone-control/blob/main/docs/tools.md) in the repository.
