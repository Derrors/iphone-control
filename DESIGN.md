# Web 控制台设计规范

桌面从左到右排列 AI 助手、设备画面、设备面板。窄窗口用抽屉或标签切换。接入新控件时沿用这套结构，优先把可用空间留给设备画面。[页面实现](apps/iphone-control/src/viewer.html)

## 输入与排版

按「写任务 → 选执行器 → 发送」安排输入区的信息层级。标准字号采用下表。字号用 `rem` 表示，表中为浏览器默认根字号 16 px 时的 CSS 像素。

| 用途 | 标准字号 | 排版 |
| --- | --- | --- |
| 区块标题 | 18–20 px | 半粗体。AI 助手欢迎标题为 19 px。 |
| 任务输入、任务正文 | 14 px | 常规字重，正文行高约 1.5 倍。触屏手机输入框至少 16 px。 |
| 执行器名称 | 13 px | 与下拉箭头垂直居中。长名称收窄时可省略。 |
| 键盘提示、次要元数据 | 12 px | 常规字重。发送快捷键放在按钮提示和输入框的读屏说明中。 |

界面使用系统无衬线字体，中文由本机中文字体绘制。ProductType、UDID 和代码使用等宽字体。外观设置中的大字和特大字调整控制台文字，设备图像按屏幕工具栏的倍率显示。Primer 将标题、正文、注释及代码定义为不同的文本样式，表中的数值是本项目对中文控制台的选择。[Primer Typography](https://primer.style/product/primitives/typography/)

## 左栏底部

任务输入占第一行。第二行只放执行器和圆形发送键，整组靠右。执行器按钮随当前名称调整宽度，名称与 12 px 箭头之间留 4 px，按钮与发送键之间留 6 px。输入区使用细边和轻阴影，在侧栏的玻璃背景上保留清晰边界。该排列参考 Codex 公开界面中的双行输入区和右端的选择器、发送键。[OpenAI 官方输入区示意](https://learn.chatgpt.com/images/codex/video-posters/proactive-teammate-v2.webp)

执行器菜单与按钮右缘对齐，间隔 6 px，默认向上展开。浅色和深色主题都用磨砂底色，选项左侧对齐名称，右侧的勾标记当前执行器。方向键移动高亮，Enter 确认，Esc 取消并返回按钮。

全局设置独立放在输入区下方，以齿轮和文字显示，整行可点击。任务记录滚动时，用户仍能在同一位置找到语言、主题和字号。这个入口沿用官方桌面侧栏将全局操作放在底部的分组方式，具体内容取自本项目已有设置。[OpenAI 官方桌面侧栏示意](https://learn.chatgpt.com/docs/use-chatgpt)

## 图标与命中区域

发送、刷新、缩放等常规动作使用 16 px 图形。下拉箭头使用 12 px，和文字合用同一个选择器。20 px 留给需要强调的独立图形。Carbon 的常规按钮使用 16 px 图标，强调型按钮使用 20 px 图标。Primer 的 12 px 图标中有用于展开和收起的箭头。[Carbon Button](https://www.carbondesignsystem.com/building-blocks/core/components/button/specifications) · [Primer Octicons](https://primer.style/octicons/usage-guidelines/)

发送按钮的箭头固定为 16 px，居中放在桌面的 32 × 32 px 按钮内。对粗指针设备扩大为 44 × 44 CSS px，执行器选择器也至少 44 px 高。图标按钮提供可访问名称及可见的键盘焦点。44 px 是此控制台的触屏目标。WCAG 2.2 AA 对指针目标的常规下限是 24 × 24 CSS px，并列有间距等例外。[WCAG 2.5.8](https://www.w3.org/WAI/WCAG22/Understanding/target-size-minimum.html) · [Primer IconButton](https://primer.style/product/components/icon-button/)

## 玻璃材质

两侧面板外壳、屏幕的浮动工具条和弹窗共用中性色磨砂材质。每块表面只在外层模糊背景，内部控件用底色、细边和留白区分。文字、图标和设备视频保持清晰。正文较多的面板提高底色不透明度，深色主题单独指定表面颜色。Fluent Acrylic 建议将透明效果用于浮层，指出相邻模糊层容易产生接缝。常驻侧栏采用高不透明度的外壳是本项目的选择。[Fluent Acrylic](https://learn.microsoft.com/en-us/windows/apps/design/style/acrylic)

默认样式使用实色表面，在支持 `backdrop-filter` 的浏览器启用玻璃。系统开启减少透明度、更高对比度或强制色彩时回到实色。对比度按前景与最终合成背景计算。普通文字以 4.5∶1 为下限，用于识别可操作控件的图形以 3∶1 为下限。[WCAG 文字对比度](https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum.html) · [WCAG 非文本对比度](https://www.w3.org/WAI/WCAG22/Understanding/non-text-contrast.html)

## 评审时怎么检查

1. 用鼠标和键盘切换执行器，输入两行中文后发送。分别确认空草稿、可发送和正在执行三种状态。发送箭头与选择器文字对齐，输入区的高度随内容增长。
2. 切换中英文，在 320 px 宽度使用特大字号。检查长执行器名称、换行提示与发送按钮的占位，触摸目标互不遮挡。
3. 对照浅色、深色、减少透明度及强制色彩。用真实画面或录制帧检查视频边缘与叠层标记。改变字号后，设备画面仍完整，点击与可见图像位置对应。
