# What is iphone-control?

iphone-control lets an AI agent such as Claude use a real iPhone or iPad that is plugged into a Linux machine or a Mac: look at the screen, tap buttons, type, open apps — like a person holding the phone.

It works as a set of [MCP](https://modelcontextprotocol.io) (Model Context Protocol) tools, so any MCP client — Claude Code, Claude Desktop, Cursor and others — can use it directly.

## What you can do with it

- Let an agent get things done on the phone: look things up, fill in forms, change settings, play games.
- Test your own app or website on a real device; a Linux box will do, no Mac needed.
- Turn a spare iPhone into a device you can script remotely.

## How it works

![Architecture: an MCP client talks to iphone-control mcp over stdio, which talks to the daemon over a Unix socket; the daemon drives the iPhone or iPad over USB through WebDriverAgent](/architecture-light.svg){.light-only}
![Architecture: an MCP client talks to iphone-control mcp over stdio, which talks to the daemon over a Unix socket; the daemon drives the iPhone or iPad over USB through WebDriverAgent](/architecture-dark.svg){.dark-only}

- A **daemon** runs all the time and owns the phone: it pairs, mounts the Developer Disk Image, opens a tunnel, and runs [WebDriverAgent](https://github.com/appium/WebDriverAgent) (WDA, an agent on top of Apple's XCTest automation) on the phone to read the screen, tap and type.
- The **MCP server** is started by the AI client on demand and simply forwards tool calls to the daemon.
- WDA is signed with your **free Apple ID**. Free signatures expire after 7 days; the daemon re-signs before that.
- While WDA is unavailable (for example while it restarts), taps, swipes, buttons and screenshots go through CoreDevice HID instead (iOS 27 and later).

## Compatibility

| | |
|---|---|
| Host | Linux, x86_64 or arm64: one-command install, Homebrew or a .deb (Debian 12+, Ubuntu 22.04+), with the system usbmuxd; or build from source with Rust 1.88+<br>macOS, Apple Silicon: one-command install or Homebrew, or build from source, see [macOS](../deploy/macos)<br>No Windows |
| Phone | iPhone or iPad, iOS 17 or later; the HID fallback needs iOS 27 or later |
| Verified on | iPhone 15 Pro Max, iOS 27.0.1 (Linux, macOS)<br>iPhone 12 mini, iOS 26.4.1 (Linux)<br>iPad mini 5, iOS 26.6.1 (Linux) |
| Account | A free Apple ID; a dedicated one is recommended |

Other models and iOS versions should work but are untested — [reports](https://github.com/Derrors/iphone-control/issues) are welcome.

::: tip Output language
The CLI, `doctor` output and MCP tool results are in Simplified Chinese by default; set `IPHONE_CONTROL_LANG=en` for English, see [Configuration](./configuration).
:::

## Limitations

**Things you do on the phone**

- It does not bypass any iOS security: trusting the computer, enabling Developer Mode, trusting the developer certificate and enabling UI automation each need you on the phone once.
- After the phone reboots, the first unlock (BFU) always needs a person, who also enters the passcode once more to allow UI automation. iOS requires this.

**Usage**

- A host can have several iPhones and iPads connected, but controls one at a time; see [Several devices](./multi-device). Don't run two copies of iphone-control, or other usbmuxd-based tools, against the same device.
- Games, maps and video have no element tree; agents work from screenshots and coordinates there.
- The HID fallback needs iOS 27 or later. On older iOS everything goes through WebDriverAgent, and actions fail while it restarts.
- Free Apple ID profiles last 7 days. Re-signing is automatic, but an expired Apple ID session needs `login`.
- Apple ID logins use a public third-party anisette server by default; set `ICTL_ANISETTE_URL` to use your own, see [Configuration](./configuration).

**macOS**

- Prebuilt binaries are for Apple Silicon only; an Intel Mac needs a build from source and is untested.
- The prebuilt binary isn't notarized; a copy downloaded with a browser needs its quarantine flag removed first.
- Set the phone's charging limit to 100%: with the 80% limit, USB drops about once a minute.

Other differences from Linux: [macOS known differences](../deploy/macos#known-differences).

Next: [Getting Started](./getting-started).
