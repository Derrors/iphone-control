## What and why

<!-- What this changes and why. Link the issue if there is one. 英文或中文都可以。 -->

## How it was tested

- [ ] `cargo test --workspace --all-features`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo fmt --check` (per package)
- [ ] On a real phone (model, iOS version, what you ran — e.g. `tests/device/run.py`):
- [ ] Not needed (docs only, or no device-facing change)

## Checklist

- [ ] MCP tool names, parameters and default outputs stay compatible (docs/decisions.md D10)
- [ ] New or changed dependencies are pinned to exact versions; vendor changes recorded in THIRD_PARTY.md
- [ ] User-visible changes noted in CHANGELOG.md (and CHANGELOG.zh-CN.md if you can) under *Unreleased*
- [ ] No private data in fixtures, screenshots or logs
