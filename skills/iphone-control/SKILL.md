---
name: iphone-control
description: Control a real USB-connected iPhone or iPad through the iphone-control MCP server. Use whenever automating iOS/iPadOS UI, inspecting elements or screenshots, scrolling lists, typing text, handling alerts, debugging app logs/crashes, or inspecting WKWebView/Safari pages on a real device.
---

# Driving a Real iPhone / iPad with `iphone-control`

`iphone-control` controls a physical iPhone or iPad via a single daemon (`iphone-control daemon`) over USB. Coordinates are always in **screen points (pt)**, matching the pixel coordinates of a full-screen `iphone_screenshot` image and rotating automatically with the device.

## Core Loop

1. **Inspect before acting.** Start with `iphone_status` (if device state is unknown) or `iphone_list_elements` / `iphone_screenshot`.
2. **Choose the right observation tool:**
   - Standard iOS UI (Settings, lists, forms, dialogs): `iphone_list_elements` — faster and uses far fewer tokens than screenshots.
   - Games, maps, video, custom canvases, icons without text, or whenever `iphone_list_elements` reports an **incomplete** tree: `iphone_screenshot` (and zoom into small targets with `x`, `y`, `width`, `height`).
3. **Prefer semantic targeting over coordinates.** Use `iphone_tap_element` with `text`, and disambiguate duplicate labels with `kind`, `id`, or `match` (`exact` / `contains`). Fall back to `iphone_tap` only for textless icons or canvas views.
4. **Combine readback with `then`.** Pass `then: "elements"` or `then: "screenshot"` on action tools so the daemon waits for the screen to settle and returns the post-action state in one round trip.
5. **Wait instead of polling.** Use `iphone_wait_for` (`state: "appeared"` or `"disappeared"`) instead of looping on `iphone_screenshot`.
6. **Batch predictable sequences.** Use `iphone_batch` for multi-step menu drills, keypad sequences (`one_gesture: true`), or form fills.

## Read References On Demand

Load only the reference file relevant to the current task:

| Situation | Reference |
| --- | --- |
| Ambiguous labels, `kind`/`id`/`match` selectors, long lists, iPad split views, `iphone_batch` | [references/navigation-and-selectors.md](references/navigation-and-selectors.md) |
| Typing CJK/long text, iOS 27 native paste prompt, games/maps zoom & coordinate math | [references/text-and-visual.md](references/text-and-visual.md) |
| Native iOS 27 vs WDA differences, alerts, lock screen, `iphone_app_logs`, crashes, WebView | [references/troubleshooting-and-dev.md](references/troubleshooting-and-dev.md) |

## Safety & Reliability Rules

- **Confirm destructive or external actions first.** Before paying, ordering, transferring money, deleting data, sending messages/emails/posts, or modifying account/password/privacy settings, ask the user (or call `iphone_confirm` when running inside a viewer task).
- **Never blindly retry an unchanged action.** If `then` reports the screen did not change (or repeats 2+ times), take a screenshot to check for an alert, keyboard, overlay, or wrong coordinates before acting again.
- **Respect `delivery` on failures.** When a tool error reports `delivery: "unknown"` or `delivery: "sent"`, inspect the screen before retrying so you do not submit twice.
- **Watch for incomplete reads.** If `iphone_list_elements` or `iphone_tap_element` warns that the accessibility walk hit the element limit or timed out (`incomplete: true`), do not assume a missing label is absent—verify with `iphone_screenshot` or narrow down after scrolling.
- **Multi-device awareness.** Only one device is active at a time; check `iphone_list_devices` and switch with `iphone_select_device` if needed.
