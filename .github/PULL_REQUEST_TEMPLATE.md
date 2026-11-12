## What and why

<!-- What this changes and why. Link the issue if there is one. 英文或中文都可以。 -->

## Tests

<!-- Required for every pull request, see CONTRIBUTING.md, "Required tests". 每一项都要填写，不适用的写明原因。 -->

### 1. Unit tests

- [ ] `cargo test --workspace --all-features --locked`, `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` and `cargo fmt --check` (per package) pass
- [ ] New or changed behaviour / the fixed bug has a unit test: <!-- test names, or why there can't be one -->
- [ ] No existing test removed, ignored or loosened
- [ ] `#[ignore]` tests of the changed area run by hand: <!-- names, or "none apply" -->

### 2. Real-device regression

- Device, iOS version, host (Linux / macOS): <!-- e.g. iPhone 15 Pro Max, iOS 27.0.1, Debian 12 -->
- [ ] `tests/device/run.py` default steps pass on this branch (exit code 0)
- [ ] The same steps on `main` or the latest release, same device, for comparison; no step clearly slower, or why:
- [ ] Steps exercising this change: <!-- what you ran -->
- [ ] Longer run (WDA start-up/recovery, reconnecting, unlocking, live picture, timers): <!-- duration and result, or "not affected" -->
- [ ] Not needed: the change doesn't touch code that talks to a phone — why:

<details><summary>run.py output</summary>

```text
this branch:

main / latest release:
```

</details>

### 3. Regression cases (docs/dev/regression-cases.md)

- [ ] All cases still hold; device cases in the changed area re-run: <!-- #numbers, or "none apply" -->
- [ ] A newly fixed problem is added as a row: <!-- #number, or "not applicable" -->

## Checklist

- [ ] MCP tool names, parameters and default outputs stay compatible (docs/decisions.md D10)
- [ ] New or changed dependencies are pinned to exact versions; vendor changes recorded in THIRD_PARTY.md
- [ ] User-visible changes noted in CHANGELOG.md (and CHANGELOG.zh-CN.md if you can) under *Unreleased*
- [ ] No private data in fixtures, screenshots, logs or the outputs above
