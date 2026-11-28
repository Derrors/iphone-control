# Changelog

**English** | [简体中文](CHANGELOG.zh-CN.md)

What changed in each release, one line per change. Versions follow [Semantic Versioning](https://semver.org/); while at 0.x a minor version may break things, so read *Upgrade notes* first.

## [Unreleased]

### Improvements

- When auto-unlock fails on one device, only that device stops unlocking automatically; the others keep going. `iphone-control unlock-reset` re-enables the controlled device, `--device <UDID>` another one. If an older version had disabled auto-unlock, every device stays disabled after the upgrade until you reset each one (#231).

## [0.3.2] - 2026-11-28

### New

- `iphone_tap_element` and `iphone_wait_for` (as well as `iphone_batch` steps and the control page) now accept optional `kind`, `id`, and `match` (`auto`, `exact`, `contains`) selectors to distinguish controls with the same label, and allow omitting `text` when `kind` or `id` is provided (#229).
- `iphone_screenshot` and actions with `then: "screenshot"` now include structured image geometry (`width`, `height`, `point_width`, `point_height`, `region`, `coordinate_scale`) alongside the image so MCP clients can map zoomed pixels back to screen points without parsing captions (#229).
- Added an official layered Agent Skill (`skills/iphone-control/`) with core workflows, selector guides, text/visual tips and troubleshooting references (#229).

### Improvements

- iOS 27+: when an accessibility tree read stops early due to an element limit, timeout, depth limit or incomplete overlays, element tools now report that the read is incomplete and why instead of silently omitting unseen controls (#229).
- `iphone_tap_element` now checks right before tapping that the target is still within screen bounds and that orientation and (on iOS 27+) the foreground app have not changed, aborting instead of tapping a different screen (#229).
- Multiple open control page tabs now share an on-demand state push stream that pauses redundant polling and sleeps when no tab is active, reducing background load on the daemon (#229).
- macOS: the built-in VideoToolbox H.264 encoder now tags frames with explicit sRGB/BT.709 colorimetry so browsers render live screen colors accurately instead of decoding them as Rec.601 (#229).

## [0.3.1] - 2026-11-28

### Improvements

- Removed settings left set in `~/.config/iphone-control/env` or the daemon's environment (such as `ICTL_WDA_DETACHED` or `ICTL_AGENT_GATE`) now log a startup notice and appear as a non-fatal warning in `iphone-control doctor` instead of being silently ignored (#223).

### Fixes

- Transient failures or timeouts from the public Anisette server or Apple's developer service no longer crash the daemon or trigger a false "Apple ID session expired" notification; requests now retry automatically and routine session checks re-check later (#224).

## [0.3.0] - 2026-11-26

A major architecture, security, stability and performance release: hyper-based control page, direct iOS 27 accessibility tree, instant USB hotplug discovery, abnormal-restart alerting and dozens of reliability fixes.

### New

- Abnormal daemon restarts (crash, `SIGKILL`, power loss or watchdog restart) are detected on the next start, logged and reported through a new `daemon` notification (`iphone-control notify-test daemon`) with crash-loop throttling (#219).

### Improvements

- USB plug and unplug events wake the daemon right away via usbmuxd's `Listen` stream instead of waiting up to 10 seconds for the next poll, while keeping the periodic poll as a fallback (#221).
- On macOS, control requests sent to the viewer's local HTTP API now also identify the calling process (via `libproc`), so an agent on the same machine can no longer pause, resume or approve its own task through the HTTP API either (#215).
- On iOS 27 the screen is read without the detour through WebDriverAgent-style XML: the accessibility tree becomes the element tree directly, the same one WDA's source gives. When auto-unlock fails on such a phone, its message no longer says "WDA unavailable" (#211).
- Slow work that doesn't need the phone no longer holds up taps and reads: `doctor` checks this machine (usbmuxd, the Apple ID, ffmpeg) before it takes the phone, crash reports and uninstalling an app don't take it at all, and the details of a second connected device are read in the background (#207).
- The control page's server now runs on hyper (already in the program through its HTTP client and MCP), in place of the hand-written HTTP/1.1. The login, the checks on each request, HTTPS for the network and the picture and task streams work as before; streams now go out chunked instead of ending at the close (#209).
- Faster in a few places: a scroll search (`iphone_scroll_collect`, `iphone_tap_element` with `scroll`) no longer reads the screen again after each swipe (about 0.3 s a swipe); pasting into a password or web page field returns once the paste prompt was answered instead of after 5 s; the control page is sent gzipped (about 290 KB down to 90 KB); one phone's picture opening slowly no longer holds up another phone's pages; the developer disk image is written to disk as it downloads (#206).
- Several control pages on one phone running WebDriverAgent share one picture stream from the phone instead of opening one each, which could push WebDriverAgent over the CPU limit at which iOS stops it (#188).
- On iOS 17–26 the control page's picture comes through the daemon; apart from `wda-bridge`, only the daemon talks to the phone now (#192).
- The phone's own video on the control page pauses while the phone unlocks itself or the bitrate changes, instead of dropping and reconnecting (#190).
- While nobody watches the control page, the phone's screen video costs less CPU (#187).
- The control page polls less, and not at all from a background tab (#193).

### Fixes

- Long-running AI tasks can no longer grow their event logs without bound (capped at 16,384 events / 16 MiB while keeping start and completion records), and replaying task history no longer holds up other daemon operations (#218).
- iOS 27+: typing into a web page's field (Safari) works: the paste prompt is found and answered without reading the page, which can't be read until it is answered. Before, typing there failed, or the text went in but was reported missing (#213).
- iOS 27+: "Allow Paste" is tapped whatever the phone's language: when its text isn't known, the first of the prompt's two buttons (#213).
- `iphone_tap_element` with `scroll` no longer taps a list that is still creeping to a stop: such a tap only stopped the list, yet was reported as done. It now waits until the element it found stays in place (at most 2 s) before tapping it (#213).
- iOS 27+ on a Mac without ffmpeg: the control page's H.264 route for a browser that can't decode the phone's HEVC is refused instead of answering an empty stream the page kept retrying, and `doctor` says that this picture needs ffmpeg (`brew install ffmpeg`) instead of "no ffmpeg needed" (#213).
- The daemon's log no longer mixes languages when a client uses another language than the daemon: what timed out, and the reasons WDA was unavailable or didn't start, are kept in both languages and shown in the reader's (#208).
- `iphone-control update` says what actually has to stop before an upgrade (AI tasks, a viewer or `wda-bridge` started by hand; the viewer service and idle MCP clients are handled), and `notify-test --help` lists `cert`. The README, the site and the architecture notes match the code again: the tool table includes `iphone_scroll_collect`, notifications do name the device, restarting the daemon doesn't drop MCP clients, and the picture paths are described as they now work (#205).
- iOS 27+: a phone ready for native control is checked every 5 minutes while nobody uses it. When the developer disk image is gone or the tunnel no longer answers, it is prepared again instead of staying "ready" until the next action fails; after mounting the image again, the accessibility service left from before is restarted, as it would no longer read the screen (#204).
- iOS 27+: what a process of the phone is (the app in front, a web page's process) is looked up again after 10 minutes, and SpringBoard's process after a minute or when reading it fails. A daemon running for weeks could otherwise go by a process number another app had taken, or miss alerts after SpringBoard restarted (#204).
- The daemon no longer keeps the events of its last 50 tasks in memory: they are read from the task's log file when a page or `task events` replays them (#204).
- The control page's H.264 picture lost before its first frame (a restart, a dropped connection) is tried again later instead of being given up for good; only a refused one (no encoder, a codec the browser can't decode) is. A retry left over from before switching devices no longer reconnects the picture out of turn (#204).
- AI tasks: while a step waits for your confirmation you can take over first (to look or change something); the question stays, and can be answered while you're in control or after handing back (#201).
- The control page no longer stays on "running" when a task's event stream is closed for good: it follows the task again if it is still there, or marks it stopped. Reconnecting or reloading the page no longer redraws old gesture marks (#201).
- A page or `task events` that fell behind a busy task no longer misses its end and stays open; a task's end stops every step still under way (a step asking to leave the task's scope, and its question, are two), so none keeps spinning; nothing is recorded after the end (#201).
- `task run` waits for what the command started to stop (killing what ignores SIGTERM) before it exits; stopping a group no longer sends a late SIGKILL to a group number that may have been reused (#201).
- The picture of a phone driven by WebDriverAgent ends when you switch to another device, instead of showing the old one (#201).
- The H.264 copy of the phone's own video counts a crashed ffmpeg as a failure and moves on to the next encoder, instead of restarting it for ever (#201).
- `task run --url` stops within 10 seconds of Ctrl-C even if the viewer doesn't answer, and gives up an event stream that goes silent or sends an endless event (#201).
- Only one daemon runs per socket, even when two start at once, and the socket is private from the moment it is created; a lock that can't be taken at all is reported as such instead of as another daemon (#201).
- The control page and the login page only run their own scripts (a Content-Security-Policy with their hashes) and never send a referrer to other sites (the login form still works behind a reverse proxy that strips `Origin`); a client that stops reading no longer holds a connection for good (#202, #220).
- With HTTPS on, the viewer takes actions only from a page served the way it serves it (HTTPS, or plain HTTP from this machine) (#202).
- A viewer stopped while it made its certificate makes a new one on the next start, instead of failing to load a key that doesn't match (#202).
- A token removed with `viewer token remove` is refused at once also within the same second, and `viewer token` commands run at the same time no longer lose each other's changes (#202).
- A setting written twice in `~/.config/iphone-control/env` takes its last value, as the services read it (#202).
- Services install from a program path with spaces, and refuse one with characters systemd or the shell would read as their own; an update that paused the viewer's service starts it again without turning on its start at login; setting up the automatic update job and failing leaves none of its files behind, even when the service manager can't be reached (#202).
- An AI task from the control page works when `IPHONE_CONTROL_URL` is set in the settings file (#202).
- A read-only login no longer sends touches from the picture (each answered with an error) and shows no pointer on it (#202).
- iOS 27+: an ordinary text field showing a time or a fraction ("6:00", "2/4") is no longer taken for the lock screen's passcode field, which made the phone read as locked (#203).
- iOS 27+: an element the phone lists under another flag than its own is no longer left out of the screen with everything under it (#203).
- Malformed answers from the phone (endless or oversized message fragments from the accessibility service, a Web Inspector message split past its limit, a WebDriverAgent answer with an endless head or odd chunks) are refused instead of read for ever or crashing; a WebDriverAgent answer with trailers no longer waits for the timeout (#203).
- `iphone_web_eval`: a page that went away is reported at once instead of after the timeout, and a connection left out of step is opened again for the next call (#203).
- macOS: the built-in video encoder no longer piles up frames for a page that is slow to take them (#203).
- When pressing Home finds a system alert in the way, the daemon's answer now also marks it in its structured result (`alert_reported`), as launching an app or opening a link does; programs reading the result rather than its text missed it (#200).
- A long press falls back to HID like a tap when WebDriverAgent can't be reached (#200).
- Requests that can't run (a tap count of 0, an empty tap sequence, an unknown button, a point that isn't a number) are refused at once, before waiting for the phone and without dropping the cached screen (#200).
- The control page getting ready for a touch no longer counts as acting: it isn't refused while an AI task runs and doesn't make the agent look again after you take over (#200).
- Requests for a list scan sent by a newer client with extra fields are no longer refused by an older daemon (#200).
- Auto-unlock refuses a saved passcode shorter than 4 digits, as `passcode set` does; `state.json` is no longer replaced by an empty file if it can't be written out; notification keys are never shown in debug output (#200).
- iPads with Touch ID in a language other than Chinese or English: the "Touch ID" prompt after swiping up is known as the lock screen, and auto-unlock taps "Use Passcode" on it; the iPad was taken for unlocked and never checked again (#198).
- A malformed accessibility answer from the phone (iOS 27+) can no longer keep the daemon busy for ever; a malformed touch (a coordinate or duration that isn't a number) is refused instead of crashing or tapping the top-left corner (#198).
- The phone's log is read from now on only: the daemon took old lines for new, such as a past "Enable UI Automation" prompt, and `iphone_app_logs` showed them as just logged (#198).
- Re-signing locks down the Apple ID session and a new signing key as soon as they are written, leaves no scratch directory behind when it times out, and never leaves a half-written package (#198).
- macOS: a memory-safety bug in the built-in video encoder's JPEG decoding is fixed (#198).
- iOS 27+: an action that comes while the control page checks the phone in the background no longer leaves the accessibility connection half-read, and those checks no longer restart the phone's accessibility daemon; only actions decide that (#197).
- A task paused for a long time no longer keeps the control page's background checks from running (#197).
- Switching back to a phone made ready in the background takes effect at once even when the daemon was busy at that moment; it could wait 10 seconds. The daemon serves at most 256 clients at once; more wait their turn (#197).
- AI tasks limited to some apps: an action is checked again once it has the phone, in case another app came to the front meanwhile, and `iphone_web_eval` is checked against the app that shows the page (#197).
- `iphone_tap_element`'s scroll search and `iphone_wait_for` stop as soon as the task is paused or taken over or the caller goes away, instead of holding the phone until they time out; the daemon also limits a scroll search to 15 swipes (#197).
- The control page's server refuses malformed requests, so a crafted link can no longer send a browser from it to another site, and it no longer waits for request bodies from someone who isn't logged in (#196).
- AI tasks from the control page: a task whose text starts with `--` is no longer taken as an option by the agent, and the agent no longer gets the task's token on its command line or notification keys in its environment (#196).
- A malformed video report from a page no longer freezes its picture (#196).
- iOS 17–18: the developer disk image is mounted the way these versions need, so WebDriverAgent starts on them again (#186).
- Times in `status`, `doctor` and the event log are shown in local time with the time zone; they were UTC without saying so (#184).
- Errors from `iphone-control update`, `service` and the update panel on the control page follow the chosen language; they were always in English (#184).
- An idle `iphone-control mcp` (kept running by Claude Desktop, Cursor, Claude Code…) no longer keeps updates from installing; its next call reaches the new daemon (#190).
- Phones driven natively (iOS 27+) no longer trigger WebDriverAgent re-signing or the daily Apple ID session check (and its "session expired" warning) when another device once used WebDriverAgent (#187).
- After re-signing, the new WebDriverAgent starts right away (#187).
- A dragged finger held down (`iphone_swipe` with `hold`) is lifted if the drag fails or times out halfway (#187).
- A broken stream from the phone can no longer make the program use unbounded memory (#187).
- Screenshots of a phone driven natively say "via CoreDevice" instead of "via HID" (#187).
- A daemon that ended with the screen media stream open (stuck and restarted, crashed or killed) no longer leaves the phone showing "viewed remotely" and never auto-locking (#193).
- MCP over HTTP (`/mcp`) accepts request bodies sent chunked, as some clients and reverse proxies send them, and ends streamed answers so a proxy can tell their end from a dropped connection (#189).
- The control page's server takes more connections and answers 503 when they are all taken instead of dropping new ones silently (#189).
- At most 4 control pages at once get the phone's own video transcoded to H.264 (browsers without HEVC); more retry until one closes (#193).
- Stopping the viewer (`systemctl stop`, an update, Ctrl-C) also stops the agents of AI tasks started from the page; they kept running on their own (#189).

## [0.2.14] - 2026-11-24

### New

- Intel Macs (macOS 11+) can install with the one-line script or Homebrew and upgrade with `iphone-control update`; these builds haven't been tested on a real Intel Mac yet, so reports are welcome (#182).

### Fixes

- Other devices on the same Wi-Fi can no longer reach WebDriverAgent on the phone; a WebDriverAgent started by an older version restarts once after upgrading (#180).

## [0.2.13] - 2026-11-24

### Fixes

- Automatic unlock works on iPhones set to English, French, German, Spanish and other non-Chinese languages; it used to give up without entering a digit (#174).

## [0.2.12] - 2026-11-24

### New

- Agents can read across several screens of a list and stop at a matching row without tapping it (#170).
- Tap, swipe and text input results now distinguish a sent input from one that never went out or whose delivery is uncertain (#170).

## [0.2.11] - 2026-11-23

### Fixes

- `iphone-control login` and the setup wizard no longer fail with HTTP 403 from Apple before the password is checked. The `X-MMe-Client-Info` header sent to Apple was lowercased (`mac os x`), which Apple now rejects (#171).

## [0.2.10] - 2026-11-22

### Upgrade notes

- MCP tools now refuse an argument they don't take, and do nothing, instead of ignoring it. A misspelt argument, or one from a newer version, used to run the call with the default and report success: `iphone_wait_for` with `state: "disappeared"` answered that the text had appeared (#158). The error names the arguments the tool takes. This includes `then` on tools that don't list it, and on the steps of `iphone_batch` (the batch's own `then` applies).

### New

- `iphone_type_text` supports `replace: true` (empty text empties the field) and verifies the full field value before pressing Return. With native control, when it fails, the old text is left unselected; through WDA (iOS 26 and earlier, or `ICTL_IOS_BACKEND=wda`) the field is cleared first, so a failure there says the old text is gone (#162).
- `iphone_list_elements` reports the match count and whether `limit` truncated the list in its structured result (#159).
- `iphone_wait_for` can wait for text to disappear without mistaking an incomplete screen read for success (#159).

## [0.2.9] - 2026-11-18

### Fixes

- With native control (iOS 27), reading the screen no longer keeps failing until the phone is replugged when the phone's accessibility service stops answering; it is restarted on its own (#152).
- With native control (iOS 27), automatic unlocking no longer fails with "couldn't bring up the passcode keypad" while the keypad is on screen (#154).
- With native control (iOS 27), an app launched right after Home no longer goes back to the home screen, as it often did for the first actions after the daemon started (#155).
- With native control (iOS 27), reading the screen no longer keeps failing when the phone's accessibility service reads every app as empty; it is restarted on its own then too (#156).

## [0.2.8] - 2026-11-18

### New

- `iphone_tap` takes `count` to tap the same point several times in one go, so double taps and entries that need quick repeated taps work (#141).
- `iphone_batch` takes `one_gesture`: a run of coordinate taps, such as keys on a keyboard, goes in one gesture, about twice as fast (#143).
- `iphone_swipe` takes `hold` to hold still before moving, so home-screen icons and other things that need a long press first can be dragged (#144).
- `iphone_tap_element` takes `scroll` (`down` / `up`) and `max_scrolls`: when the element isn't on screen, it scrolls and looks again, and taps it once found (#147).

### Improvements

- When the same action leaves the screen unchanged again and again, the result says how many times in a row and to try something else (#145).
- An action that had to unlock the phone first says so, and how long it took (#135).

### Fixes

- Elements in an app's in-app browser are no longer placed one navigation bar too low, so tapping them by text hits them (#139).
- Launching a system app for the first time no longer hangs for a minute behind its privacy notice and restarts WDA; it says at once that the notice is in the way (#148).
- With native control (iOS 27), a row under the floating search bar of Settings is no longer tapped through the bar, which opened the search (#150).
- `iphone_tap_element` that tapped nothing no longer claims, with `then`, that the action may not have taken effect, and answers sooner (#149).
- A few slow WDA requests no longer add up to a needless WDA restart (#140).

## [0.2.7] - 2026-11-15

### Upgrade notes

- Phones on iOS 27 and later switch to native control. To keep WebDriverAgent, set `ICTL_IOS_BACKEND=wda` in `~/.config/iphone-control/env` (#123).
- Building from source needs Rust 1.89 (#119).
- `ICTL_WDA_DETACHED` is gone: WDA is always launched in the background now. Left in the settings file, it does nothing (#125).

### New

- iOS 27 and later work without WebDriverAgent: no Apple ID, no signing, ready in seconds. iOS 26 and earlier are unchanged (#123).
- Chinese and other non-ASCII text can be typed while WebDriverAgent is down. Tap "Allow Paste" when the phone asks (#118).
- Actions with `then` say whether the screen changed, so an agent can tell when a tap did nothing (#121).

### Improvements

- WDA starts on a locked phone too, and keeps running across daemon restarts and updates (#125).
- Dependencies updated (#119).

### Fixes

- Ctrl+C in setup menus is no longer ignored after a half-typed key on a busy machine (#131).
- `iphone-control status` on a phone under native control shows "native" instead of asking you to install WDA (#127).
- `service stop` and `service uninstall` also end a WDA launched in the background, and each phone keeps its own (#124).
- Text pasted on the phone no longer reaches your Mac through Universal Clipboard (#122).
- With a notification banner up, elements are read from the app instead of the banner (#121).
- Arrow keys in setup menus no longer turn into letters on a busy machine (#120).
- A tap or key that WDA didn't answer in time no longer lands on the screen later (#118).
- Screens with very many elements read faster, and the result says when the list was cut short (#118).
- The first tap after a pause no longer waits for the screen size to be measured (#110).
- The control page retries H.264 or the phone's video after a slow start instead of staying on MJPEG (#109).
- Swipes on the control page return sooner (#108).
- The first Home after a daemon restart is no longer slow on some devices (#107).

## [0.2.6] - 2026-11-14

### New

- Low-bandwidth mode for the control page's picture, in Settings: about half the traffic (#104, #105).

### Improvements

- The control page reuses its connections, so taps and checks over Wi-Fi answer several times faster (#103).

### Fixes

- The phone's video on the control page no longer stutters over Wi-Fi (#100).
- Taps and Home on the control page no longer wait behind the page's background checks (#101).
- Home on the control page responds in about half the time (#102).
- The control page picks up the phone's video again after the phone restarts its stream (#104).

## [0.2.5] - 2026-11-14

### Upgrade notes

- `--url` to another machine needs `https://` (with `--ca` for a self-signed certificate). `http://127.0.0.1` still works (#89).
- A read-only token no longer unlocks the phone (#89).

### Improvements

- The login page matches the control page, with its own light/dark and language switches (#97).
- Ready about 10 s sooner after the daemon or WDA restarts (#90).
- While waiting for the "Enable UI Automation" passcode, the prompt is up on the phone most of the time (#90).
- A stuck WDA is restarted after about 35 s instead of 90 s (#90).
- Commands, MCP calls and the control page wait for a restarting daemon instead of failing (#90).
- Too many wrong login tokens no longer lock out the right one (#89).

### Fixes

- Without WDA, devices below iOS 27 say at once that taps and swipes need it (#98).
- A confirmation no longer undoes your pause or takeover, and two at once are asked in turn (#88).
- A step the agent sent just before you paused, took over or handed back no longer runs after it (#88).
- After you hand back, the agent has to look at the screen before acting (#88).
- Removing or replacing a login token closes everything opened with it within 5 s (#89).
- The control page's agent works in a private directory instead of `/tmp` (#89).
- The control page keeps accepting connections after errors and drops stalled ones (#88).
- Task logs cut short by a power failure are cleaned up (#88).

## [0.2.4] - 2026-11-14

### Upgrade notes

- The control page and its `/mcp` need a login. Run `iphone-control viewer token` and open the link it prints. HTTP MCP clients send `Authorization: Bearer <token>`. The CLI and stdio MCP are unaffected (#79).
- With `--lan`, the page is HTTPS on the local network. Check the certificate fingerprint from `service status` the first time (#76).
- `ICTL_AGENT_GATE` and `ICTL_AGENT_TOKEN` are replaced by `IPHONE_CONTROL_TASK_TOKEN` (#64).

### New

- AI tasks for any agent: `iphone-control task run -- <command>`, steered with `task list`, `events`, `pause`, `resume`, `takeover`, `handback`, `stop`, `approve` and `reject` (#64).
- Limit a task to some apps with `task run --app` or the App button on the control page. Anywhere else, each step waits for you (#69, #70).
- An agent can't pause, approve or otherwise control its own task (#75).
- The control page picks up running tasks after a reload, including ones started elsewhere (#64).
- `viewer --lan` opens the control page to the local network over HTTPS (#72, #76).
- MCP over HTTP at `/mcp` on the control page's port (#77).
- Named and read-only login tokens: `viewer token add <name> [--read-only]` (#80).
- Use another machine's phone: `mcp --url` bridges to its `/mcp`, and `task … --url` runs and controls tasks there (#80, #82).
- `service viewer install` runs the control page as a background service (#74).
- Arrow-key setup menus, and shell completions with `iphone-control completions <shell>` (#71).

### Improvements

- Tasks are kept by the daemon. While one runs, other clients can look but not act (#64).
- The picture zooms only with its − / + buttons, not by pinching (#68).

### Fixes

- Locking takes about 2 s instead of 10 s on some iPhones, and now works on iPads below iOS 27 (#59, #63).
- A WDA that still answers `/status` but nothing else is restarted within about 30 s (#60, #62).
- A failed lock says what each way of locking did, instead of "WDA just dropped" (#61).
- On iOS 27, system alerts missing from the phone's video are drawn on the picture and can be tapped (#67).
- Task summaries on the control page are formatted instead of raw Markdown (#66).
- Every connected device's screen size is remembered, not only the last one's (#86).
- Notifications name the device instead of always saying "iPhone" (#85).
- Updates no longer fail now and then with "Text file busy" (#81).
- `task run` keeps the command's own exit code after `task finish` (#78).
- Holding an arrow key in setup menus no longer prints escape codes (#73).

## [0.2.3] - 2026-11-13

### Upgrade notes

- From 0.2.2, upgrade once by hand: `iphone-control update apply` (script install), `brew upgrade iphone-control`, or `sudo apt install ./<file>.deb`. After that, `iphone-control update` does it (#51).
- `update rollback` is gone. A new version that doesn't start is still replaced by the old one automatically (#52).

### New

- `iphone-control update` checks for and installs the new stable release, through the script's service, Homebrew or the `.deb`. It asks first, and `--yes` skips the question (#51, #52).
- Debian packages are signed, and the signature is checked before installing (#52).

### Fixes

- WDA is no longer killed every 1–2 minutes while the control page shows H.264 or MJPEG (#55).
- Touches on the control page respond quickly again (#54).
- `iphone-control status | head -1` and similar no longer crash with "Broken pipe" (#53).

## [0.2.2] - 2026-11-12

### Upgrade notes

- New script installs use `install.sh` from the signed GitHub Release and need Python 3 and OpenSSL 3. To move an older script install, uninstall it with its own script first. Settings and device data are kept (#44).

### New

- Update checks: `iphone-control update check`, or *About & updates* on the control page (#44).
- Script installs upgrade with `update apply`, and can install signed patches automatically with `update policy auto-patch` (#44).
- A redesigned control page that fits wide and narrow windows, with device names and frames (#45).

### Fixes

- Taps and touches are refused after a device switch, rotation or lost picture, instead of landing in the wrong place (#45).
- Closed or frozen tabs release their picture connections (#45).

## [0.2.1] - 2026-11-12

### New

- On iOS 27 and later, the control page shows the phone's own video: HEVC, about 60 fps, native resolution (#42).
- Elsewhere the picture is H.264 at about 1/15 of the traffic, hardware-encoded where possible (#42).
- Experimental `ICTL_WDA_DETACHED=1` launches WDA in the background, even while the phone is locked (#41).

### Fixes

- On iOS 27 and later, WDA is no longer killed every few minutes while the control page is open (#42).

## [0.2.0] - 2026-11-11

### Upgrade notes

- Docker is no longer released. Stop the container (`docker compose down`) and move to `install.sh`, Homebrew or the `.deb`.
- To keep the Apple ID session, signed WDA and passcodes, copy the container's `./data` to `~/.local/share/iphone-control` first ([how](https://derrors.github.io/iphone-control/en/deploy/backup)). The phone may ask to trust the computer again.
- Settings move from `.env` to `~/.config/iphone-control/env`.
- The daemon no longer runs usbmuxd. On Linux, install the system one (`sudo apt install usbmuxd`), and unmask it if you masked it for the container.
- Register the MCP server again: the first-run guide does it, or run the command `iphone-control mcp-config` prints.

### New

- One-line installs: `install.sh` for macOS (Apple Silicon) and Linux, Homebrew, and a `.deb` for Debian 12+ / Ubuntu 22.04+, each with a background service.
- First-run guide: run `iphone-control` after installing. `setup --reconfigure` redoes any part of it.
- `iphone-control service install|uninstall|start|stop|restart|status`.
- AI tasks on the control page: type a task and Claude Code or Codex does it on the phone. Pause, take over or stop it at any time, and it asks before paying, deleting or sending.
- A revoked signing certificate is detected and the device re-signed automatically.
- The control page follows device switches, explains why there is no picture, and can sign and install WDA.

### Improvements

- New control page: AI assistant, live screen with zoom, and a device panel with apps and elements. In English or Chinese.
- On iOS 27 and later the picture works like a touchscreen: about 20 ms per tap instead of 530 ms.
- The picture runs at about 50 fps.

### Fixes

- The mouse wheel works again on the control page below iOS 27.
- `lock` and `home` are much faster on newer iPhones, and taps no longer take 26 s while something animates.
- Home pressed during an app transition on iPad is retried.
- After a restart, the daemon goes on with the last device that was ready.
- `iphone-control viewer` starts even before the daemon has picked a device.
- Clearer hints when WDA fails with `unexpected response`, Web Inspector is off, or the daemon can't be reached.
- `iphone_app_logs` no longer returns Info lines.

## [0.1.5] - 2026-11-10

### New

- English: `IPHONE_CONTROL_LANG=en` for tools, CLI, logs and notifications.
- `iphone-control viewer`: watch and control the phone in a browser.
- Developer tools, with `IPHONE_CONTROL_DEV_TOOLS=1`: app logs, crash reports, installing and uninstalling apps, and Safari's Web Inspector.

### Fixes

- Scroll bars are left out of element lists in every language.
- `iphone_uninstall_app` refuses partial or ambiguous names.
- App installs and re-signing no longer upload at the same time.
- A stuck daemon exits so its service restarts it.
- Memory no longer grows with the HID fallback, and `usbmuxd.log` is rotated.

## [0.1.4] - 2026-11-07

### Improvements

- `ICTL_DDI_DIR` is never written to; only the default cache is downloaded again after three failed mounts.
- `debug` subcommands need `--features debug-cmds`.

### Fixes

- Security: `zip` updated to 9.4.1 for GHSA-m665-v7pm-7grj and related advisories. Exposure was small.
- An action whose connection dropped is no longer repeated over HID.
- Calls to the phone, usbmuxd and Apple have time limits, so the daemon can't hang on them.
- Re-signing is checked within a minute after reconnecting or switching devices.
- Cancelling an MCP call stops it, including the rest of an `iphone_batch`. A batch also stops if the device changes.
- A timed-out HID session no longer leaves the screen stream running.
- A stale WDA can't look ready early, and errors say when WDA just dropped.
- iPad: auto-unlock works when WDA was started while locked, and sidebar rows can be tapped by name again.
- `doctor` no longer counts an Apple keyboard as a phone.

## [0.1.3] - 2026-11-07

### Upgrade notes

- Auto-unlock passcodes are now per device. Before upgrading, connect only the device whose passcode was saved, or clear it and set each one with `passcode set --device <UDID>`.
- Device selection is shared by all clients, and `select` choices survive restarts.
- In landscape without WDA, don't turn the device to the other landscape side.

### New

- Choose which device to control: `iphone_list_devices` and `iphone_select_device`, or `devices` and `select`.
- Auto-unlock passcodes per device.

### Fixes

- Screen size follows rotation: no stretched screenshots or missing elements, and iPad landscape coordinates are right.
- HID fallback taps and swipes work in landscape, and are refused when the orientation is unknown.
- Auto-unlock on Touch ID devices taps "Use Passcode" first and checks the keypad.
- Control no longer jumps to another device when usbmuxd reorders its list.
- `doctor`, `status` and re-signing read the current device's own WDA profiles.
- `setup` waits for the device before signing, and `open_url` waits for the app before returning.
- Switching devices no longer reuses the previous device's app list and screen size.
- usbmuxd is no longer restarted every 30 s for a missing `IPHONE_UDID`, and a second device is picked up in the container.

## [0.1.2] - 2026-11-04

### New

- Prebuilt macOS binary (Apple Silicon, macOS 11+). It isn't notarized; see the macOS guide.

## [0.1.1] - 2026-11-04

### New

- macOS (Apple Silicon) support, no Xcode needed.

### Improvements

- Tool descriptions explain when to use each tool and what it returns.
- Release files are named `iphone-control-vX.Y.Z-linux-<arch>.tar.gz`.

### Fixes

- `home`, `lock` and waking for auto-unlock work on phones such as the iPhone 12 mini.
- Setups using the system usbmuxd reconnect after a short disconnect.
- `doctor` works on macOS.

## [0.1.0] - 2026-11-02

First public release.

### New

- A daemon that owns the phone: pairing, Developer Mode, disk image, tunnel and WebDriverAgent, recovering from unplugging and reboots.
- MCP server with 15 tools: screenshots, elements, taps, swipes, typing (Chinese too), buttons, apps, URLs, waiting and batches.
- HID fallback while WDA is down.
- WDA signed with a free Apple ID and re-signed before it expires.
- Bark and Telegram notifications, and optional auto-unlock.
- `setup`, `doctor`, `status` and `mcp-config`.
- Docker image and prebuilt Linux binaries.

[Unreleased]: https://github.com/Derrors/iphone-control/compare/v0.3.2...HEAD
[0.3.2]: https://github.com/Derrors/iphone-control/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/Derrors/iphone-control/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/Derrors/iphone-control/compare/v0.2.14...v0.3.0
[0.2.14]: https://github.com/Derrors/iphone-control/compare/v0.2.13...v0.2.14
[0.2.13]: https://github.com/Derrors/iphone-control/compare/v0.2.12...v0.2.13
[0.2.12]: https://github.com/Derrors/iphone-control/compare/v0.2.11...v0.2.12
[0.2.11]: https://github.com/Derrors/iphone-control/compare/v0.2.10...v0.2.11
[0.2.10]: https://github.com/Derrors/iphone-control/compare/v0.2.9...v0.2.10
[0.2.9]: https://github.com/Derrors/iphone-control/compare/v0.2.8...v0.2.9
[0.2.8]: https://github.com/Derrors/iphone-control/compare/v0.2.7...v0.2.8
[0.2.7]: https://github.com/Derrors/iphone-control/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/Derrors/iphone-control/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/Derrors/iphone-control/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/Derrors/iphone-control/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/Derrors/iphone-control/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Derrors/iphone-control/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Derrors/iphone-control/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Derrors/iphone-control/compare/v0.1.5...v0.2.0
[0.1.5]: https://github.com/Derrors/iphone-control/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/Derrors/iphone-control/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Derrors/iphone-control/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Derrors/iphone-control/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Derrors/iphone-control/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Derrors/iphone-control/releases/tag/v0.1.0
