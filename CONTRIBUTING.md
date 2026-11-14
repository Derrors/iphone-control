# Contributing

Thanks for helping! Issues and pull requests are welcome in **English or Chinese** (欢迎用中文提 issue 和 PR).

Most of the code comments, design documents and commit messages in this repository are written in Chinese. Follow the language of the code you are editing; English is fine for new files and for commit messages.

## Before you start

- **Bugs:** include the iPhone or iPad model, iOS version, how you installed it (install.sh, Homebrew, .deb or source; Linux or macOS), and the output of `iphone-control doctor`, with private data removed. The [issue template](.github/ISSUE_TEMPLATE/bug_report.yml) asks for these.
- **Larger changes:** open an issue first. The key design decisions and their reasons are in [docs/decisions.md](docs/decisions.md); the regression cases every change must keep passing are in [docs/dev/regression-cases.md](docs/dev/regression-cases.md).
- **Tests:** every pull request must pass the tests in [Required tests](#required-tests); read it before you start, since the device part needs a phone.
- **Security problems:** don't open an issue — see [SECURITY.md](SECURITY.md).

## Building and testing without a phone

You need Rust 1.88 or later (CI pins 1.99.1) and a C compiler. Everything except the device tests runs offline:

```bash
# --all-features also builds the debug-cmds feature, as CI does
cargo test --workspace --all-features --locked
# Linux/macOS PTY regression; temporary settings only, no daemon or device actions
python3 tests/terminal.py --binary target/debug/iphone-control --test-dir target/debug/deps
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
# vendor/ is upstream code and isn't formatted; check only this repository's packages:
for p in ictl-l10n ictl-webinspector ictl-device ictl-wda ictl-sign ictl-unlock ictl-daemon ictl-mcp ictl-protocol iphone-control; do cargo fmt -p $p -- --check; done
```

Or, without Rust installed:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1-trixie sh -c \
  'rustup component add clippy && cargo test --workspace --all-features --locked && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings'
```

Behind a firewall add `--network host -e HTTP_PROXY -e HTTPS_PROXY` (`idevice` is a git dependency). `cargo test -p <one crate>` can fail to compile `plume_store` because of feature unification; test the whole workspace instead.

The pure logic — element-tree parsing, occlusion, the auto-unlock rules (against a fake WebDriverAgent), the daemon state machine, MCP argument handling — is covered by unit tests. Real iOS 27 element trees used as fixtures are in [crates/ictl-wda/tests/fixtures](crates/ictl-wda/tests/fixtures).

## Testing on a real phone

Anything that touches the device, timing or WebDriverAgent behaviour needs a real phone. With the daemon running:

```bash
python3 -m venv .venv && .venv/bin/pip install -r tests/device/requirements.txt
export MCP_CMD="$PWD/target/release/iphone-control mcp"   # your build; the daemon must be the same build
.venv/bin/python tests/device/run.py            # default steps: harmless (Home, Clock tabs, Spotlight search)
.venv/bin/python tests/device/run.py '[["iphone_launch_app", {"app": "设置"}], ["iphone_screenshot", {}]]'
```

`run.py` prints every result with its wall time, ends with a line counting calls and failures, saves screenshots to `./shots` and exits with 1 if any call failed. `MCP_CMD` defaults to `iphone-control mcp` on your `PATH`. To test your branch, run its daemon too (stop the installed service with `iphone-control service stop`, then start `target/release/iphone-control daemon` in another terminal; `service start` afterwards). What a pull request has to run on the device is in [Required tests](#required-tests).

AI tasks (anything in `crates/ictl-daemon/src/tasks.rs`, `task`, the viewer's task API, the MCP server's task handling) have their own scenarios in `tests/device/tasks.py`, each check a PASS / FAIL line:

```bash
export ICTL_BIN="$PWD/target/release/iphone-control"         # your build (the daemon too)
.venv/bin/python tests/device/tasks.py                       # every scenario but restart
.venv/bin/python tests/device/tasks.py pause confirm         # only these
```

With `MCP_URL` (the viewer's `/mcp`) the agents connect over HTTP instead of stdio, and `ICTL_VIEWER_TOKEN` is the viewer's login token (`iphone-control viewer token`); `ICTL_VIEWER` (the viewer's address) adds the checks through its API, and the `remote` scenario, which plays a platform on another machine, needs all three. `MCP_CA` trusts the viewer's self-signed certificate for an `https://` address. The scenarios are harmless (Home, Settings, Clock, screenshots) and stop any task they leave behind.

On macOS, the full checklist (setup, `debug` checks, coexistence with Finder, reconnect) is in [docs/dev/macos-verification.md](docs/dev/macos-verification.md).

Notes:

- Wait until `iphone-control status` says 就绪 (ready) before calling tools after a restart. Restarting the daemon several times in quick succession can make the phone ask for the passcode to enable UI automation again.
- Never run two daemons, or the hidden `iphone-control debug …` diagnostics, against the same phone at once: only one usbmuxd and one XCTest session can own it. `debug` is not compiled by default; build it with `cargo build -p iphone-control --features debug-cmds` or `cargo install --path apps/iphone-control --locked --features debug-cmds`.
- Don't automate anything that sends messages, pays, deletes data or changes account or privacy settings in tests.

### Adding an element-tree fixture

1. `iphone-control wda-bridge --port 18100` (stop it afterwards — WebDriverAgent has no authentication).
2. ```bash
   curl -s "127.0.0.1:18100/source?format=xml&excluded_attributes=visible,accessible,focused,enabled,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame" \
     | python3 -c 'import sys, json; print(json.load(sys.stdin)["value"])' > fixture.xml
   ```
3. **Scrub it:** names, phone numbers, email addresses, locations, notification contents, the device name. Check with `grep -o 'label="[^"]*"' fixture.xml | sort -u`.

## Rules of thumb

- **Pin exact versions** (`=x.y.z`) and keep `Cargo.lock` committed; upgrade dependencies in separate commits ([D9](docs/decisions.md)).
- **Don't break the MCP interface:** tool names, parameters and default outputs stay compatible; add optional parameters instead ([D10](docs/decisions.md)).
- **Vendored code** (`vendor/`) keeps its license. Record any change in [THIRD_PARTY.md](THIRD_PARTY.md); changed MPL-2.0 files stay MPL-2.0.
- **Never commit** `data/`, `~/.config/iphone-control/env` or other settings files with secrets, certificates, provisioning profiles, pairing records or anything Apple ships (Developer Disk Images, frameworks).
- Comments explain *why*, especially device quirks; put measured numbers next to timeouts and thresholds.

## Required tests

Every pull request must pass the tests below, and its description must show it: the [pull request template](.github/PULL_REQUEST_TEMPLATE.md) has a section for each. A pull request without them is asked for them before review. The rules apply to maintainers too. 每个 PR 都必须通过下面的测试，并在描述里写明结果。

**Every bug fix and every new feature must be verified on a real device, with the results attached to the pull request ([part 4](#4-real-device-proof--every-bug-fix-and-every-feature)). Without them it is not merged. 所有 bug 修复和新功能都必须经过真机测试验证，并在 PR 里附上测试结果，否则不予合并。**

### 1. Unit tests — every pull request

- `cargo test`, clippy with `-D warnings` and `cargo fmt --check` ([commands above](#building-and-testing-without-a-phone)) pass on your machine. CI runs them on Linux and macOS, plus the MSRV check, gitleaks and the package checks; a pull request with a failing check isn't merged.
- New behaviour and every bug fix come with a unit test when the logic can run without a phone (parsing, decisions, state, arguments, protocol). For a fix, the test fails without it. Name the tests in the pull request, or say why there can't be one.
- Don't delete, `#[ignore]` or loosen an existing test to make a change pass. If a test is wrong, fix it and explain why in the pull request.
- Tests marked `#[ignore]` are run by hand when their area changes, and named in the pull request: `slow_page_stays_close_to_live` (`apps/iphone-control/src/video/`, needs ffmpeg) and `measure_recording` (`video/vt.rs`, macOS only). Run one with `cargo test --workspace --all-features --locked -- --ignored --nocapture <name>`.

### 2. Real-device regression — every pull request that touches the phone

Needed when the change touches code that talks to a phone or changes what it does or how long it takes: anything under `crates/` and `apps/` except pure text changes (comments, translations that don't change meaning). Not needed for changes limited to documentation, the website, CI, packaging or other tests; say so in the template. A bug fix or a feature always needs [part 4](#4-real-device-proof--every-bug-fix-and-every-feature), wherever the change is.

- **Default steps:** build your branch, run its daemon and `tests/device/run.py` with the default steps on a real iPhone or iPad ([Testing on a real phone](#testing-on-a-real-phone)). Every call succeeds and the exit code is 0.
- **No slower, no less stable:** on the same device, run the default steps once more on `main` (or the latest release), and paste both outputs. A step that became clearly slower (more than about 20 %, or by more than 0.5 s) needs a reason. A change that could affect long-running behaviour (WDA start-up and recovery, reconnecting, unlocking, the live picture, timers) also needs a longer run: repeat the default steps for at least an hour, e.g. `for i in $(seq 12); do .venv/bin/python tests/device/run.py || break; sleep 300; done`, and report failures and how the times developed.
- **What you changed:** also run steps that exercise the change itself (pass them as a JSON list) and paste the output.
- **The right hardware:** when the change concerns iPad, landscape, an iOS version or a platform, test on that. Changes to macOS-only code go through [docs/dev/macos-verification.md](docs/dev/macos-verification.md).
- **In the pull request:** device model, iOS version, Linux or macOS, and the outputs (the `===` lines and the last line are enough; remove private data).

### 3. Regression cases — every pull request

[docs/dev/regression-cases.md](docs/dev/regression-cases.md) lists the problems this project has run into, with the right behaviour and how each is tested. Every pull request must keep all of them passing.

- Cases tested by unit tests are covered by part 1.
- Cases tested on a device (真机, headless Chrome, measured by hand) in the area you changed: run them again and list their numbers in the pull request. To find them, search the file for the module, tool or behaviour you changed; if none apply, say so.
- A pull request that fixes a problem not in the list adds a row: the problem, the right behaviour, and how it is tested (the unit test's name, or the device steps).

### 4. Real-device proof — every bug fix and every feature

Part 2 checks that a change doesn't break what already works; this part shows that the fix or the feature itself works, on real hardware. It applies to every bug fix and every new feature, whatever part of the code it touches (daemon, MCP tools, CLI, Viewer, installer, updater, packaging). **A bug fix or feature without it is not merged.**

- **Real device, real installation:** an iPhone or iPad connected over USB to the host, running your build. Unit tests, a fake or mocked daemon, simulated device states, fixtures and headless browsers are welcome in addition, but they are not the proof. Host-side changes (CLI, installer, updater, packages) are run on a real installation on a real host, the way a user runs them, with a phone connected.
- **Bug fix: before and after.** Reproduce the bug on `main` (or the release that has it) and show it gone with your branch, on the same device and with the same steps. If the bug can only be reproduced under conditions you can't create, say what you did instead and why.
- **Feature: end to end.** Exercise the new behaviour the way a user or an agent would (the MCP tool call, the CLI command, the action on the Viewer page), including the error and edge cases it is meant to handle (refused, not trusted, locked, disconnected, landscape, …).
- **The right hardware and conditions:** the iOS version, device kind (iPhone, iPad), orientation and host OS (Linux, macOS) the change is about. If you don't have them, say which part is untested; the maintainers test it before merging, or the pull request waits.
- **Attach the results** in the pull request: device model, iOS version, host OS, the exact steps, and their output (tool or command output, `run.py` lines, relevant daemon log lines, a screenshot or recording for anything visible). Remove private data.

## Pull requests

- Keep them focused; describe what changed and why.
- The tests above pass and are reported in the template ([Required tests](#required-tests)). Bug fixes and features without real-device results are not merged.
- CI must pass: format, clippy with `-D warnings`, tests (on Linux and macOS), the MSRV check, gitleaks and the package checks (`.deb`, `install.sh`, the Homebrew formula).
- User-visible changes get a line in [CHANGELOG.md](CHANGELOG.md) and [CHANGELOG.zh-CN.md](CHANGELOG.zh-CN.md) under *Unreleased* (a Chinese line is welcome but optional for outside contributors; maintainers add it before release).

## Documentation site

The user guide at [derrors.github.io/iphone-control](https://derrors.github.io/iphone-control/) is a VitePress site in [`website/`](website/) (Chinese at the root, English under `en/`; keep both in step). Preview it with `cd website && pnpm install && pnpm docs:dev`. [`docs.yml`](.github/workflows/docs.yml) builds it on pull requests and pushes to `main` that change `website/` (dead links fail the build) and deploys it from `main`. Changes elsewhere, including `docs/`, don't trigger it.

## Releasing (maintainers)

1. Move the *Unreleased* entries in [CHANGELOG.md](CHANGELOG.md) and [CHANGELOG.zh-CN.md](CHANGELOG.zh-CN.md) under `## [x.y.z] - YYYY-MM-DD` and update the compare links at the bottom. Both files need the section — the release notes are built from the two (English, then Chinese) and the workflow fails if either is missing. Keep entries short: one line per user-visible change, no implementation details.
2. Set `version` in the root `Cargo.toml` (`[workspace.package]`) and run `cargo check --workspace` to update `Cargo.lock`.
3. Configure the publisher and signing keys before tagging (any newer signed stable release can be installed from any version; there is no per-release upgrade path to set). Commit, tag `vx.y.z` and push the tag. The release workflow builds the Linux archives and Debian packages, creates the GitHub Release from both changelogs, then adds the Apple Silicon archive. Stable releases also publish the installer and signed platform manifests. If a tap is configured, the workflow updates its formula after signing all three archives. See the [release configuration](docs/dev/release-updates.md) and [release workflow](.github/workflows/release.yml).
4. Update Glama: on glama.ai, the build steps under this server's *Admin → Dockerfile* pin the version. Change it and rebuild.

By contributing you agree that your contributions are licensed under the MIT license, except for changes to vendored files, which stay under their original licenses.
