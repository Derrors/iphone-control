---
layout: home

hero:
  name: iphone-control
  text: Let AI agents use a real iPhone
  tagline: Control an iPhone or iPad over USB from Linux or a Mac, reliably and fast, and hand it to Claude and other AI as MCP tools. No Xcode, no paid developer account; on iOS 27 and later, recommended, no Apple ID either.
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
    details: Screenshots, on-screen elements, tap by text, swipe, typing (Chinese too), apps, waiting for pages, multi-step batches — 17 MCP tools; actions can return the settled screen
  - icon: ⚡
    title: Native on iOS 27
    details: iOS 27 and later are controlled natively by default — nothing installed on the phone, no Apple ID, no signing, ready in seconds. Recommended
  - icon: 📱
    title: iOS 17–26 too
    details: Controlled through WebDriverAgent, signed with a free Apple ID and re-signed before the 7-day profile expires. This route stays maintained
  - icon: 🔁
    title: Keeps itself running
    details: A long-running daemon owns the phone and recovers after unplugging, reboots and lost connections; it serves with the control page closed
  - icon: 🙋
    title: A person stays in charge
    details: AI tasks can be paused, taken over or stopped at any time; paying, deleting or sending waits for your OK, and an agent can't approve its own task
  - icon: 🖥️
    title: Optional control page
    details: Watch the live screen in a browser, act on it and hand out tasks; open it to the local network and agents elsewhere connect over HTTP MCP. No ports open by default
  - icon: 🔔
    title: Tells you when it needs you
    details: Bark or Telegram notifications when the phone goes offline, needs its passcode after a reboot, or re-signing fails
  - icon: 📦
    title: One binary
    details: Written in Rust. Install with one command, Homebrew or a .deb, then run iphone-control and it walks you through setup; or build from source
---
