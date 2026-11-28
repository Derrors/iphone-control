# Text Input and Visual / Coordinate Operations

## Typing and Replacing Text (`iphone_type_text`)

- Make sure a text field is focused first (tap it and confirm the keyboard is up).
- Pass `replace: true` to clear existing text and verify the new text is present before optionally pressing Return (`submit: true`). Pass `text: ""` with `replace: true` to clear a field.
- **iOS 27 native control:** text is entered through the pasteboard + `Cmd-V`, and the daemon automatically taps the system *"Allow Paste"* prompt, reads back the field, and restores the original pasteboard. If the field already held the exact same text (such as Spotlight retaining the last query), verify with a screenshot.
- **WebViews / custom keyboards that ignore `iphone_type_text`:** tap individual soft-keyboard keys using `iphone_batch` with `one_gesture: true`.

## Screenshots, Zoom Regions, and Structured Geometry

- Full-screen `iphone_screenshot` is scaled to 1:1 screen points: pixel `(u, v)` in the returned JPEG is directly screen point `(u, v)`.
- For tiny icons, dense text, or game boards, pass `x`, `y`, `width`, `height` (in screen points) to crop from the full-resolution Retina capture and enlarge cleanly.
- Every `iphone_screenshot` returns both a human-readable caption and structured geometry (`width`, `height`, `point_width`, `point_height`, `region`, `coordinate_scale`). Convert any pixel `(u, v)` in a zoomed image back to screen points via:
  `(region.x + u / coordinate_scale, region.y + v / coordinate_scale)`.
- In MCP `structuredContent`, `iphone_screenshot` places those fields at the top level alongside `type: "screenshot"`, whereas action tools invoked with `then: "screenshot"` nest the geometry object under `structuredContent.screenshot` alongside `delivery` and `readback`.
- Coordinates follow the current screen orientation. After rotating the phone or iPad, always take a fresh screenshot or element read before tapping coordinates.

## Dragging & Long-Press Gestures

- `iphone_long_press` (default 1 s): context menus, Home screen quick actions, text selection.
- `iphone_swipe` with `hold > 0` (typically `0.6` s): drag items that must be picked up by a long press first (Home screen icons, reorderable list rows). `seconds + hold` must be ≤ 10.
- `iphone_tap` with `count > 1` (up to 10): double-tap (including Shift for Caps Lock) or rapid multi-tap triggers that separate calls are too slow for.
