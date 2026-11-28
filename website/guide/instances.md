# 同时控制多台设备（多实例）

一个守护进程同一时间只控制一台设备（见[多台设备](./multi-device)）。要让两个 agent 各控制一台、互不影响，就给每台设备一个**实例**：一个自己的守护进程、控制接口、控制台端口和数据目录。原来的那一套叫**默认实例**，照旧运行，不用改。

## 添加一个实例

先让默认实例控制它要留着的那台，再把另一台给新实例：

```bash
iphone-control devices                                   # 看两台设备的 UDID
iphone-control select <默认实例要的那台>
iphone-control instance add ipad --device <另一台的 UDID>
```

`instance add` 会：

- 显示默认实例正在控制哪台、新实例拿哪台，确认后继续（`--yes` 不问）；
- 把默认实例**固定**在它正在控制的那台：有命名实例时，默认实例不再自动选设备，`select auto` 会被拒绝；
- 写登记文件 `~/.config/iphone-control/instances/ipad.env`，从 8201 起找一个空的控制台端口（`--port` 指定）；
- 装好并启动 `iphone-control@ipad`、`iphone-control-viewer@ipad` 两个服务（macOS 是 `io.github.derrors.iphone-control.instance.ipad`），等它们起来；起不来就停掉并撤回登记。

设备现在没接也要登记时加 `--force`。中途被打断时，`instance list` 会报出来，`instance add --resume ipad` 继续，`instance remove ipad` 清掉。

添加之前要满足：

- 默认实例的守护进程在运行，并且是这个版本；
- 设置目录用默认的 `~/.config/iphone-control`，数据目录用默认的 `~/.local/share/iphone-control`（不支持改过的 `XDG_CONFIG_HOME`、`XDG_DATA_HOME` 或 `ICTL_DATA_DIR`）；
- 登录过 Apple ID 时，Anisette 状态要先确认来源：旧版本生成的状态没有记录，`instance add` 会提示先运行 `iphone-control login --reprovision-anisette`（确知就是现在配置的地址生成的，也可以 `--confirm-anisette-origin`）。

## 使用

```bash
iphone-control instance list                     # 每个实例一行：设备、状态、控制台端口
iphone-control status --all                      # 同上
iphone-control --instance ipad status            # 对 ipad 实例运行任何命令
iphone-control --instance ipad mcp-config        # 打印注册 MCP 的命令，服务名 iphone-ipad
```

- 每个命令都认 `--instance <名字>`，也可以设环境变量 `IPHONE_CONTROL_INSTANCE`。命令行上写了 `--socket` 时，环境和设置文件里的实例名不起作用。
- `mcp-config` 生成的命令是 `iphone-control --socket <绝对路径> mcp --local`：`--local` 让它只连这个 socket，设置文件里的 `IPHONE_CONTROL_URL`、别的实例名都带不走它。两个 agent 各注册一个，就各控制各的设备。
- 控制台 `http://127.0.0.1:<端口>/`：所有实例共用一套登录令牌和证书，但登录会话按实例分开。
- 实例自己的设置（比如这台设备的通知）写在 `instances/<名字>.env` 空行以下；写成空值表示清掉共用 `env` 里的那一项。空行以上是 `instance add` 写的，不要改。

## 共用的和分开的

| 共用 | 分开 |
|---|---|
| Apple ID 账号、签名证书、Anisette 状态 | 守护进程、控制接口、控制台端口 |
| 锁屏密码、自动解锁停用记录（按设备） | `state.json`、任务、日志 |
| 控制台登录令牌和证书 | WDA 安装包、开发者镜像缓存 |

**一台设备同一时间只归一个实例。** 每个实例控制设备前先拿这台设备的租约（`devices/<UDID>.lock`），另一个实例拿着时状态显示「这台设备由实例 X 控制」，`devices` 在那一行后面标出来，不会去读它。

**每日 Apple ID 会话检查**只由一个实例做，那个实例停了别的会接手；通知不重复发。

**后台启动的 WDA** 记录在共用的数据目录里，另一个实例接手这台设备时直接接上，不用重启 WDA。新实例接手一台已经装好 WDA 的设备时，只要 WDA 是本机账号签的，不用重新签名。

## 移除

```bash
iphone-control instance remove ipad            # 停服务，确认停下后删登记和服务
iphone-control instance remove ipad --purge    # 连它的数据目录一起删
```

服务停不下时登记会留着并报错。默认实例仍然固定在它那台；没有命名实例以后可以 `select auto` 改回自动选。

## 升级

`iphone-control update` 会一起处理所有实例：任何一个实例有任务在进行时拒绝，升级期间不开新任务，先停所有实例的控制台和守护进程，换好程序后按原来的状态启动。升级中途被打断时运行 `iphone-control update recover`。

有命名实例时**不要降级到 0.3.2**：先 `instance remove` 掉所有命名实例。

## 限制

- 不能让一个守护进程同时控制多台；一个页面也看不到所有设备。
- 不支持 root 和普通用户同时运行守护进程控制同一批设备。
- 命名实例不能用 `service` 命令和首次运行引导：它们的服务由 `instance add`、`instance remove` 管。
