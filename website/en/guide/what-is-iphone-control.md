# What is iphone-control?

iphone-control lets an AI agent such as Claude use a real iPhone or iPad that is plugged into a Linux machine or a Mac: look at the screen, tap buttons, type, open apps — like a person holding the phone.

It works as a set of [MCP](https://modelcontextprotocol.io) (Model Context Protocol) tools, so any MCP client — Claude Code, Claude Desktop, Cursor and others — can use it directly.

The core is device control: a long-running daemon owns the phone and keeps serving with the browser's control page closed. The control page and remote access are optional; see [Viewer and Remote Use](./remote).

## What you can do with it

- Let an agent get things done on the phone: look things up, fill in forms, change settings, play games.
- Test your own app or website on a real device; a Linux box will do, no Mac needed.
- Turn a spare iPhone into a device you can script remotely.
- Watch and act on the phone from a control page in the browser, or hand tasks to a local Claude Code or Codex and pause, take over or confirm each step; from the local network and other machines too, see [Viewer and Remote Use](./remote).

## How it works

![Architecture: AI clients use iphone-control mcp over stdio, and the optional control page serves browsers and remote agents over HTTPS; both talk to the daemon over a Unix socket. The daemon is the core: it drives the iPhone or iPad over USB, natively on iOS 27 and later and through WebDriverAgent on iOS 17–26, and keeps running with the control page closed](/architecture-light.svg){.light-only}
![Architecture: AI clients use iphone-control mcp over stdio, and the optional control page serves browsers and remote agents over HTTPS; both talk to the daemon over a Unix socket. The daemon is the core: it drives the iPhone or iPad over USB, natively on iOS 27 and later and through WebDriverAgent on iOS 17–26, and keeps running with the control page closed](/architecture-dark.svg){.dark-only}

- A **daemon** runs all the time and owns the phone: it pairs, mounts the Developer Disk Image, opens a tunnel, and then drives the phone one of two ways, by iOS version:
  - **iOS 27 and later: native control (default).** The screen is read through the system's accessibility service, taps, swipes and buttons go through CoreDevice HID, and apps launch through CoreDevice. Nothing is installed on the phone, no Apple ID is needed, and it's ready in seconds.
  - **iOS 17–26: WebDriverAgent.** [WebDriverAgent](https://github.com/appium/WebDriverAgent) (WDA, an agent on top of Apple's XCTest automation) runs on the phone to read the screen, tap and type. It is signed with your **free Apple ID**; free signatures expire after 7 days and the daemon re-signs before that. This route stays maintained.
- The **MCP server** is started by the AI client on demand and simply forwards tool calls to the daemon.
- On iOS 27 and later set back to WDA (`ICTL_IOS_BACKEND=wda`), taps, swipes, buttons and screenshots go through CoreDevice HID while WDA is unavailable (for example while it restarts).

## Compatibility

| | |
|---|---|
| Host | Linux, x86_64 or arm64: one-command install, Homebrew or a .deb (Debian 12+, Ubuntu 22.04+), with the system usbmuxd; or build from source with Rust 1.89+<br>macOS, Apple Silicon: one-command install or Homebrew, or build from source, see [macOS](../deploy/macos)<br>No Windows |
| Phone | iPhone or iPad, iOS 17 or later. **iOS 27 or later recommended** (native control); iOS 17–26 through WebDriverAgent |
| Verified on | iPhone 15 Pro Max, iOS 27.2.1, native control (Linux)<br>iPhone 15 Pro Max, iOS 27.0.1, WDA (Linux, macOS)<br>iPhone 12 mini, iOS 26.4.1 (Linux)<br>iPad mini 5, iOS 26.6.1 (Linux) |
| Account | None on iOS 27 and later (native control); on iOS 26 and earlier a free Apple ID, a dedicated one recommended |

Other models and iOS versions should work but are untested — [reports](https://github.com/Derrors/iphone-control/issues) are welcome.

::: tip Output language
The CLI, `doctor` output and MCP tool results are in Simplified Chinese by default; set `IPHONE_CONTROL_LANG=en` for English, see [Configuration](./configuration).
:::

## Limitations

**Things you do on the phone**

- It does not bypass any iOS security: trusting the computer and enabling Developer Mode each need you on the phone once; with WDA, so do trusting the developer certificate and enabling UI automation.
- After the phone reboots, the first unlock (BFU) always needs a person; iOS requires this. With WDA, the passcode is also entered once more to allow UI automation.

**Usage**

- A host can have several iPhones and iPads connected, but controls one at a time; see [Several devices](./multi-device). Don't run two copies of iphone-control, or other usbmuxd-based tools, against the same device.
- Games, maps and video have no element tree; agents work from screenshots and coordinates there.
- Native control (iOS 27 and later): rows off the screen aren't listed, so a long list has to be scrolled to them first; text is always pasted, and the daemon taps *Allow Paste* itself and puts the pasteboard back afterwards.
- On iOS 26 and earlier everything goes through WebDriverAgent, with no HID fallback, and actions fail while it restarts.
- With WDA: free Apple ID profiles last 7 days. Re-signing is automatic, but an expired Apple ID session needs `login`.
- With WDA, Apple ID logins use a public third-party anisette server by default; set `ICTL_ANISETTE_URL` to use your own, see [Configuration](./configuration).

**macOS**

- Prebuilt binaries are for Apple Silicon only; an Intel Mac needs a build from source and is untested.
- The prebuilt binary isn't notarized; a copy downloaded with a browser needs its quarantine flag removed first.
- Set the phone's charging limit to 100%: with the 80% limit, USB drops about once a minute.

Other differences from Linux: [macOS known differences](../deploy/macos#known-differences).

Next: [Getting Started](./getting-started).
