# 配置

全部通过环境变量配置，都可以不填。

## 设置文件 {#settings-file}

设置写在 `~/.config/iphone-control/env`，一行一个 `KEY=value`，权限 0600（里面有通知密钥，不要提交、不要分享）。首次运行 `iphone-control` 的引导和 `iphone-control setup --reconfigure` 会替你写好；也可以照模板手改。模板是仓库的 `packaging/env.example`，安装时也放了一份：

- 一行命令安装：`~/.config/iphone-control/env.example`
- Homebrew：`$(brew --prefix)/opt/iphone-control/share/iphone-control/env.example`
- deb 包：`/usr/share/doc/iphone-control/env.example`

守护进程启动时读这个文件，其他命令（`login`、`notify-test`、`mcp-config`，以及 AI 客户端拉起的 `mcp`）启动时也读，不用先在 shell 里加载。环境里已经有的变量优先，所以命令行上临时设的值会盖过文件里的。

手改后重启守护进程才生效：

```bash
iphone-control service restart
```

## 常用

| 变量 | 默认 | 含义 |
|---|---|---|
| `HTTP_PROXY` / `HTTPS_PROXY` | 直连 | 访问苹果、anisette、开发者镜像、推送通知用的代理 |
| `IPHONE_CONTROL_LANG` | `zh` | 界面语言：`zh` 或 `en`。工具说明和结果、命令行输出、日志、通知都按它。MCP 和命令行各自按自己的设置请求，守护进程按请求的语言回答；日志和通知按守护进程自己的设置。见表下说明 |
| `IPHONE_CONTROL_DEV_TOOLS` | 关 | 设为 `1` 时 MCP 服务多出[开发者工具](./tools#开发者工具-显式开启)：日志、崩溃报告、安装和卸载 App、Web Inspector（在网页和 WebView 里执行 JavaScript）。风险见[安全与隐私](./security)。见表下说明 |
| `IPHONE_UDID` | 自动选 | 固定控制哪台设备。不设时自动选：正在控制的那台一直用到它断开；运行中可以用 `select` 命令或 `iphone_select_device` 工具切换。设了就不能切换 |
| `RESIGN_MIN_DAYS` | `2` | 描述文件剩余不到几天时重签 |
| `RESIGN_CHECK_HOURS` | `6` | 每隔几小时检查一次；手机重新连上或切换设备后 1 分钟内也查一次 |
| `OFFLINE_ALERT_MIN` | `10` | 手机离线超过几分钟推送提醒，`0` 不提醒 |

MCP 服务由 AI 客户端拉起，注册时写进去的环境变量优先于设置文件。引导和 `iphone-control mcp-config` 会把当时的 `IPHONE_CONTROL_LANG=en`、`IPHONE_CONTROL_DEV_TOOLS=1` 写进注册命令，所以改了这两项以后要重新注册：运行 `iphone-control setup --reconfigure` 选「注册 MCP」，再开一个新的会话。

用 JSON 配置的客户端见[接入 AI 客户端](./mcp-clients#claude-desktop、cursor-等)。

通知相关的变量见[通知](./notifications)。

## 高级

| 变量 | 含义 |
|---|---|
| `ICTL_ANISETTE_URL` | 自建 anisette 服务地址（默认用公共的第三方服务，见[安全与隐私](./security)） |
| `ICTL_DDI_URL` | 开发者镜像的下载地址，默认社区维护的镜像仓库 |
| `ICTL_DDI_DIR` | 自己从 Xcode 提取的开发者镜像目录。设了就不下载，也不会删里面的文件；缺文件时报错说明缺哪个。不设时用数据目录下的 `ddi/` 缓存，连续 3 次挂载失败后清掉重新下载 |
| `ICTL_WDA_UNSIGNED` | 未签名 WDA 的路径。默认是数据目录的 `wda/WDA-unsigned.ipa`，第一次签名前下载。自己指定的路径不会下载 |
| `SCREEN_W` / `SCREEN_H` | HID 后备路线换算坐标用的屏幕尺寸（点）：WDA 不可用、又没有这台设备的尺寸记录时才用，比如 WDA 从没连上过，或切到记录里没有尺寸的设备。不分设备，接多台时要小心 |
| `ICTL_LOG` | 日志级别，例如 `info,ictl_wda::http=debug` 记录每个 WDA 请求的耗时 |

## 目录和控制接口

一般不用改。要改时写进[设置文件](#settings-file)，不能写成空值：守护进程、命令行和 MCP 都要用同样的值。MCP 的注册命令里带着 `--socket`，改了 `ICTL_SOCKET` 要重新注册（`setup --reconfigure` 选「注册 MCP」）。

| 变量 | 默认 | 含义 |
|---|---|---|
| `ICTL_DATA_DIR` | root：`/var/lib/iphone-control`<br>普通用户：`$XDG_DATA_HOME/iphone-control`，没设时 `~/.local/share/iphone-control` | 数据目录，同命令行的 `--data-dir` |
| `ICTL_SOCKET` | root：`/run/iphone-control/ctl.sock`<br>普通用户：`$XDG_RUNTIME_DIR/iphone-control/ctl.sock`，没有这个变量时（macOS）放在数据目录里 | 守护进程的控制接口，同命令行的 `--socket` |
| `XDG_DATA_HOME` / `XDG_RUNTIME_DIR` | 系统设置 | 不是本项目的变量，只影响普通用户时上面两项的默认位置 |
