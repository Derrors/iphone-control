---
layout: home

hero:
  name: iphone-control
  text: 让 AI agent 操作真实的 iPhone
  tagline: 在 Linux 或 Mac 上通过 USB 稳定、高效地控制 iPhone 或 iPad，以 MCP 工具交给 Claude 等 AI。不需要 Xcode，不需要付费开发者账号；推荐 iOS 27 及以上，连 Apple ID 都不用。
  image:
    src: /logo.svg
    alt: iphone-control
  actions:
    - theme: brand
      text: 什么是 iphone-control？
      link: /guide/what-is-iphone-control
    - theme: alt
      text: 快速开始
      link: /guide/getting-started
    - theme: alt
      text: GitHub
      link: https://github.com/Derrors/iphone-control

features:
  - icon: 🤖
    title: 给 AI 用的工具
    details: 截图、读屏幕元素、按文字点击、滑动、输入中文、启动 App、等页面加载、一次执行多步，共 17 个 MCP 工具；操作后顺带返回稳定后的界面
  - icon: ⚡
    title: iOS 27 原生控制
    details: iOS 27 及以上默认原生控制：手机上什么都不装，不用 Apple ID、不用签名，几秒就绪。推荐用 iOS 27 及以上的设备
  - icon: 📱
    title: iOS 17–26 也能用
    details: 经 WebDriverAgent 控制，用免费 Apple ID 签名，7 天到期前自动重签。这条路线继续维护
  - icon: 🔁
    title: 自己维持运行
    details: 常驻守护进程独占手机，拔插、重启、断连后自动恢复；不开控制台也一直提供服务
  - icon: 🙋
    title: 人工把关
    details: AI 任务可以随时暂停、接管、停止；付款、删除、发消息之前要你确认，Agent 不能批准自己的任务
  - icon: 🖥️
    title: 可选的控制台
    details: 浏览器里看实时画面、直接操作、下达任务；可以对局域网开放，别的机器上的 AI 经 HTTP MCP 接入。默认不开端口
  - icon: 🔔
    title: 需要你时会通知
    details: 手机离线、重启后要输密码、重签失败时，经 Bark 或 Telegram 推送
  - icon: 📦
    title: 一个二进制
    details: Rust 编写。一行命令、Homebrew 或 deb 包安装，运行 iphone-control 一步步带你设置好；也可以从源码编译
---
