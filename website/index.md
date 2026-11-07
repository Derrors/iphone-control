---
layout: home

hero:
  name: iphone-control
  text: 让 AI agent 操作真实的 iPhone
  tagline: 在 Linux 或 Mac 上通过 USB 控制 iPhone 或 iPad，以 MCP 工具交给 Claude 等 AI。不需要 Xcode，不需要付费开发者账号。
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
    details: 截图、读屏幕元素、按文字点击、滑动、输入中文、启动 App、等页面加载、一次执行多步，共 17 个 MCP 工具
  - icon: 💻
    title: Linux 或 Mac 都行
    details: Linux 上不需要 Mac，Mac 上不需要 Xcode。用免费 Apple ID 给 WebDriverAgent 签名，7 天到期前自动重签
  - icon: 🔁
    title: 自己维持运行
    details: 自动配对、挂载开发者镜像、拉起 WDA；拔插、重启、WDA 崩溃后自动恢复
  - icon: 🛟
    title: 后备路线
    details: WDA 不可用时，点击、滑动、按键、截图仍可经 CoreDevice HID 完成（iOS 27 起）
  - icon: 🔔
    title: 需要你时会通知
    details: 重签失败、Apple ID 会话失效、重启后要输密码、手机离线时，经 Bark 或 Telegram 推送
  - icon: 🔒
    title: 默认不开端口
    details: WDA 经 usbmuxd 直连手机，本机不监听任何网络端口；自动解锁默认关闭
  - icon: 📦
    title: 一个二进制
    details: Rust 编写，Linux 上 Docker 一条命令部署，也可以从源码或预编译二进制运行；Mac 上直接运行预编译二进制
  - icon: 🧭
    title: 省往返
    details: 操作可以顺带返回操作后的界面，自动等界面稳定、识别加载中和挡路的弹窗
---
