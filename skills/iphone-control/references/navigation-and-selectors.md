# Navigation, Selectors, Scrolling, and Batching

## Disambiguating Duplicate Labels (`kind`, `id`, `match`)

Many iOS screens repeat the same visible word across a title, a text field, and an action button (for example `"Search"` as `StaticText`, `SearchField`, and `Button`). Instead of guessing `index`:

- Filter by element type with `kind` (case-insensitive, e.g. `Button`, `TextField`, `SearchField`, `Cell`, `Switch`, `StaticText`):
  ```json
  {"text": "搜索", "kind": "Button"}
  ```
- Filter by exact accessibility identifier (`id="..."` in `iphone_list_elements`) with `id`; when `id` or `kind` uniquely identifies the target, `text` may be `""`:
  ```json
  {"text": "", "id": "com.apple.settings.general"}
  ```
- Control text matching with `match`:
  - `"auto"` (default): exact label/name first, then case-insensitive substring.
  - `"exact"`: exact label or name only (useful when `"App"` must not match `"Apple Account"`).
  - `"contains"`: substring match without preferring exact.

The same `kind`, `id`, and `match` options work on `iphone_wait_for` (`state: "appeared"` and `"disappeared"`).

## Covered Targets and TOCTOU Protection

- `iphone_tap_element` automatically shifts the tap to an uncovered part of an element when a floating bar or keyboard covers its center, and refuses to tap if the element is completely covered.
- Before sending the tap, the daemon checks that the target point is within bounds and (under iOS 27 native control) that the foreground app and screen orientation have not flipped since the tree was read. If context drifted, nothing is tapped (`tapped_nothing: true`)—re-read the screen before acting.

## Long Lists & iPad Split Views

- **Tap an off-screen list item:** pass `scroll: "down"` (or `"up"`) and `max_scrolls` (default 8, max 15) to `iphone_tap_element`. On iOS 27 native control, off-screen rows are not listed at all until scrolled into view.
- **Read/collect a list without tapping:** use `iphone_scroll_collect` with optional `filter`, `until`, `max_scrolls`, and `limit`.
- **iPad split views (Settings, Mail):** swiping at screen center scrolls the detail pane, not the sidebar. Pass an explicit `x` inside the sidebar column to `iphone_scroll_collect` (or swipe inside that column).

## Batching (`iphone_batch`)

- Use `iphone_batch` (up to 20 steps) when every step is predictable (opening a menu tree, entering a keypad code, filling a known form).
- Set `reuse_elements: true` only when consecutive `iphone_tap_element` steps do not change the layout (e.g. pressing keys on one numeric keypad).
- Set `one_gesture: true` when all steps are `iphone_tap` on one keyboard or keypad (much faster, especially for WebViews that ignore `iphone_type_text`).
- Put `then: "elements"` or `then: "screenshot"` on the batch call itself rather than individual steps.
