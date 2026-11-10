import { defineConfig } from 'vitepress'

const repo = 'https://github.com/Derrors/iphone-control'

// https://vitepress.dev/reference/site-config
export default defineConfig({
  // 发布到 GitHub Pages：https://derrors.github.io/iphone-control/
  base: '/iphone-control/',
  title: 'iphone-control',
  description: '在 Linux 或 macOS 上通过 USB 控制真实 iPhone，以 MCP 工具交给 AI agent',
  head: [['link', { rel: 'icon', type: 'image/svg+xml', href: '/iphone-control/logo.svg' }]],
  lastUpdated: true,
  cleanUrls: true,

  locales: {
    root: {
      label: '简体中文',
      lang: 'zh-CN',
      themeConfig: {
        nav: [
          { text: '首页', link: '/' },
          { text: '指南', link: '/guide/what-is-iphone-control' },
          { text: '更新日志', link: `${repo}/blob/main/CHANGELOG.zh-CN.md` },
        ],
        sidebar: [
          {
            text: '指南',
            items: [
              { text: '什么是 iphone-control？', link: '/guide/what-is-iphone-control' },
              { text: '快速开始', link: '/guide/getting-started' },
              { text: '接入 AI 客户端', link: '/guide/mcp-clients' },
              { text: '工具一览', link: '/guide/tools' },
            ],
          },
          {
            text: '使用',
            items: [
              { text: '配置', link: '/guide/configuration' },
              { text: '命令行', link: '/guide/cli' },
              { text: '多台设备', link: '/guide/multi-device' },
              { text: '通知', link: '/guide/notifications' },
              { text: '自动解锁', link: '/guide/auto-unlock' },
              { text: '安全与隐私', link: '/guide/security' },
            ],
          },
          {
            text: '安装部署',
            items: [
              { text: '从源码编译', link: '/deploy/source' },
              { text: 'macOS', link: '/deploy/macos' },
              { text: '数据与备份', link: '/deploy/backup' },
            ],
          },
          {
            text: '故障排除',
            items: [{ text: '常见问题', link: '/troubleshoot/faq' }],
          },
          {
            text: '参与开发',
            items: [
              { text: '贡献指南', link: `${repo}/blob/main/CONTRIBUTING.md` },
              { text: '架构与设计', link: `${repo}/tree/main/docs` },
            ],
          },
        ],
        editLink: { pattern: `${repo}/edit/main/website/:path`, text: '在 GitHub 上编辑此页' },
        lastUpdated: { text: '最后更新' },
        docFooter: { prev: '上一页', next: '下一页' },
        outline: { label: '本页目录' },
        returnToTopLabel: '回到顶部',
        sidebarMenuLabel: '菜单',
        darkModeSwitchLabel: '外观',
        footer: {
          message: '基于 MIT 协议开源 · 与 Apple Inc. 无关',
          copyright: 'Copyright © 2026-present iphone-control contributors',
        },
      },
    },
    en: {
      label: 'English',
      lang: 'en-US',
      description: 'Control a real iPhone over USB from Linux or macOS and hand it to AI agents as MCP tools',
      themeConfig: {
        nav: [
          { text: 'Home', link: '/en/' },
          { text: 'Guide', link: '/en/guide/what-is-iphone-control' },
          { text: 'Changelog', link: `${repo}/blob/main/CHANGELOG.md` },
        ],
        sidebar: [
          {
            text: 'Guide',
            items: [
              { text: 'What is iphone-control?', link: '/en/guide/what-is-iphone-control' },
              { text: 'Getting Started', link: '/en/guide/getting-started' },
              { text: 'Connect an AI Client', link: '/en/guide/mcp-clients' },
              { text: 'Tools', link: '/en/guide/tools' },
            ],
          },
          {
            text: 'Usage',
            items: [
              { text: 'Configuration', link: '/en/guide/configuration' },
              { text: 'CLI', link: '/en/guide/cli' },
              { text: 'Several Devices', link: '/en/guide/multi-device' },
              { text: 'Notifications', link: '/en/guide/notifications' },
              { text: 'Auto-unlock', link: '/en/guide/auto-unlock' },
              { text: 'Security and Privacy', link: '/en/guide/security' },
            ],
          },
          {
            text: 'Deployment',
            items: [
              { text: 'Build from Source', link: '/en/deploy/source' },
              { text: 'macOS', link: '/en/deploy/macos' },
              { text: 'Data and Backups', link: '/en/deploy/backup' },
            ],
          },
          {
            text: 'Troubleshooting',
            items: [{ text: 'FAQ', link: '/en/troubleshoot/faq' }],
          },
          {
            text: 'Development',
            items: [
              { text: 'Contributing', link: `${repo}/blob/main/CONTRIBUTING.md` },
              { text: 'Architecture and Design', link: `${repo}/tree/main/docs` },
            ],
          },
        ],
        editLink: { pattern: `${repo}/edit/main/website/:path`, text: 'Edit this page on GitHub' },
        footer: {
          message: 'Released under the MIT License · Not affiliated with Apple Inc.',
          copyright: 'Copyright © 2026-present iphone-control contributors',
        },
      },
    },
  },

  themeConfig: {
    logo: { src: '/logo.svg', width: 24, height: 24 },
    socialLinks: [{ icon: 'github', link: repo }],
    search: {
      provider: 'local',
      options: {
        locales: {
          root: {
            translations: {
              button: { buttonText: '搜索', buttonAriaLabel: '搜索' },
              modal: {
                displayDetails: '显示详情',
                resetButtonTitle: '清除',
                backButtonTitle: '关闭搜索',
                noResultsText: '没有找到结果',
                footer: { selectText: '选择', navigateText: '切换', closeText: '关闭' },
              },
            },
          },
        },
      },
    },
  },
})
