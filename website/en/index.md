---
layout: home

hero:
  name: iphone-control
  text: Let AI agents use a real iPhone
  tagline: Control an iPhone or iPad over USB from Linux or a Mac and hand it to Claude and other AI as MCP tools. No Xcode, no paid developer account.
  image:
    src: /logo.svg
    alt: iphone-control
  actions:
    - theme: brand
      text: What is iphone-control?
      link: /en/guide/what-is-iphone-control
    - theme: alt
      text: Getting Started
      link: /en/guide/getting-started
    - theme: alt
      text: GitHub
      link: https://github.com/Derrors/iphone-control

features:
  - icon: 🤖
    title: Tools for agents
    details: Screenshots, on-screen elements, tap by text, swipe, typing (Chinese too), apps, waiting for pages, multi-step batches — 17 MCP tools
  - icon: 💻
    title: Linux or Mac
    details: No Mac needed on Linux, no Xcode needed on a Mac. WebDriverAgent is signed with a free Apple ID and re-signed before the 7-day profile expires
  - icon: 🔁
    title: Keeps itself running
    details: Pairs, mounts the Developer Disk Image, starts WDA, and recovers after unplugging, reboots and crashes
  - icon: 🛟
    title: Fallback path
    details: When WDA is down, taps, swipes, buttons and screenshots still work through CoreDevice HID (iOS 27+)
  - icon: 🔔
    title: Tells you when it needs you
    details: Bark or Telegram notifications for failed re-signing, expired sessions, reboots that need the passcode, or going offline
  - icon: 🔒
    title: No open ports
    details: WDA is reached through usbmuxd; nothing listens on the network. Auto-unlock is off by default
  - icon: 📦
    title: One binary
    details: Written in Rust. Install with one command, Homebrew or a .deb, then run iphone-control and it walks you through setup; or build from source
  - icon: 🧭
    title: Fewer round trips
    details: Actions can return the resulting screen after waiting for it to settle, and flag loading pages and blocking alerts
---
