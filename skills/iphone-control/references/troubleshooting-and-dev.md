# Troubleshooting, Native vs WDA, and Developer Tools

## iOS 27 Native Control vs WDA (iOS 17–26)

| Aspect | Native (iOS 27+ default) | WDA (iOS 17–26 or `ICTL_IOS_BACKEND=wda`) |
| --- | --- | --- |
| Signing / WDA | Not needed (requires USB pair + Developer Mode + DDI) | Requires signed WDA + UI Automation enabled |
| Element tree | Coarser rows (often one element per cell); off-screen rows omitted | Finer sub-nodes; off-screen rows may appear but cannot be tapped without scrolling |
| Incomplete read | `incomplete: true` + `incomplete_reason` (`element_limit`, `timeout`, `depth_limit`, `unreadable`, `overlays_incomplete`) | Snapshot limit note in output text |
| Text input | Pasteboard + auto *"Allow Paste"* | `/wda/keys` (falls back to HID if WDA is down) |

## Alerts, Lock Screen, and Stuck Screens

- **System alerts:** If a tool reports a system alert covering the screen, handle the alert first with `iphone_tap_element` (e.g. `kind: "Button"`) before tapping behind it.
- **Lock screen:** When an auto-unlock passcode is configured, reading elements, taking screenshots, and acting unlock automatically (`iphone_status`, `iphone_list_apps`, `iphone_list_devices`, `iphone_select_device`, developer tools, and pressing `lock` do not). If auto-unlock is disabled after a failure or no passcode is configured, stop and ask the user to unlock manually.
- **Incomplete element tree:** When a read reports `incomplete: true` (for instance `element_limit` or `timeout` on a complex screen), avoid assuming a target does not exist; verify with `iphone_screenshot` or scroll/filter.

## Developer Tools (`IPHONE_CONTROL_DEV_TOOLS=1`)

- **`iphone_app_logs` (two-call pattern):** `os_trace_relay` streams live logs only from the first call onward (Default/Error/Fault levels). Call once to start capture, reproduce the issue, then call again with `process`, `grep`, `since`, or `errors_only: true`. Capture stops automatically after 15 minutes idle.
- **`iphone_crash_reports`:** Call with no `name` to list recent crash reports (optionally filtered by app), then pass the exact `name` to read the report contents.
- **`iphone_install_app` / `iphone_uninstall_app`:** Confirm with the user before uninstalling apps or overwriting production data.
- **`iphone_web_targets` / `iphone_web_eval`:** Inspect Safari and `WKWebView` pages (requires *Settings → Safari → Advanced → Web Inspector* on the device).
