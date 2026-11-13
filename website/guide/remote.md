# 控制台与远程使用

`iphone-control viewer` 是浏览器里的控制台：看实时画面、直接点按滑动输入、看设备面板，把任务交给本机的 Claude Code 或 Codex 执行，暂停、接管、确认它的每一步。这一页讲怎么打开它、怎么从局域网和别的机器用它，以及别的 agent 平台怎么接入。命令和参数的完整列表见[命令行](./cli)。

## 打开控制台

```bash
iphone-control viewer          # 在这个终端里运行，Ctrl-C 停止
iphone-control viewer token    # 另开一个终端：显示登录令牌和点开就能登录的链接
```

控制台默认只监听本机的 `127.0.0.1:8200`（端口用 `--port` 或 `IPHONE_CONTROL_VIEWER_PORT` 改）。**所有经 HTTP 的访问都要登录，本机也一样**：控制台分不清是你在本机的浏览器，还是经 frp、nginx、cloudflared 这类反向代理转发进来的访问，它们看起来都来自 `127.0.0.1`。

- **浏览器**：点开 `viewer token` 打印的链接（`http://127.0.0.1:8200/?token=…`），或者打开控制台后在登录页粘贴令牌。登录一次后记住 30 天，地址栏里的令牌会被去掉。
- **程序**：请求头带 `Authorization: Bearer <令牌>`。
- 命令行、stdio 方式的 `iphone-control mcp`、`task run` 走本机的 Unix socket，不用登录。

令牌存在数据目录的 `viewer-tokens.json`（0600）。令牌就是钥匙，别发到聊天里或写进会公开的配置。

## 令牌：有名字的、只读的

第一个令牌叫 `owner`，什么都能做。可以再加有名字的令牌，比如每人一个、每台设备一个：

```bash
iphone-control viewer token add 客厅电视 --read-only   # 只读：显示令牌和登录链接
iphone-control viewer token add 小王                   # 和 owner 一样能操作
iphone-control viewer token list                       # 名字和权限（不显示令牌本身）
iphone-control viewer token show 小王 --new            # 换一个新的
iphone-control viewer token remove 客厅电视            # 用它登录的浏览器马上进不来
```

- **只读令牌**能看画面、设备状态、App 列表、元素、任务和它的每一步；点按、输入、开任务、暂停、确认、切换设备、`/mcp` 都会被拒绝。页面上顶栏会显示「只读」，输入框和按键都不显示。
- 任务记录里写下是哪个令牌做的控制，比如 `viewer:小王`。
- 增删、换令牌**马上生效**，不用重启控制台；被删或被换掉的令牌，用它登录的浏览器下一次请求就回到登录页。
- 最后一个能操作的令牌不能删；要换掉它用 `show <名字> --new`。

## 局域网访问和 HTTPS

```bash
iphone-control viewer --lan    # 或设置文件里 IPHONE_CONTROL_VIEWER_LAN=1
```

- 还是同一个端口，同时监听局域网。只接受回环、私有网段（10/8、172.16/12、192.168/16）、链路本地和 100.64/10（Tailscale）来的连接；`Host` 只认本机的名字和地址，别的名字用 `--lan-host` 加。
- **局域网里用 HTTPS**：浏览器只在 HTTPS 或 localhost 上提供 WebCodecs，用 HTTPS 打开时画面才能是 H.264 或手机自己的 HEVC（HTTP 下只能退回每秒几 MB 的 MJPEG），令牌和画面也加了密。局域网来的 `http://` 会跳到 `https://`；本机的 `http://127.0.0.1` 照旧。
- **证书**：默认用本机生成的自签证书（数据目录的 `viewer-tls/`，一直沿用；出现新的 IP 时重新生成）。浏览器第一次打开会提示不受信任：在警告页的证书详情里核对 SHA-256 指纹，和启动时或 `iphone-control service status` 显示的一致就继续访问。
- **不想看到警告**：用自己的证书，`--tls-cert`/`--tls-key`（或 `IPHONE_CONTROL_VIEWER_TLS_CERT`/`_KEY`），比如 mkcert 生成、装到你电脑上的本地 CA，或者 `tailscale cert` 拿到的正式证书。
- `--tls off` 关掉 HTTPS，见[配置](./configuration)。

不开局域网也可以用 SSH 隧道：`ssh -L 8200:127.0.0.1:8200 <用户>@<主机>`，浏览器开 `http://127.0.0.1:8200/`。

## 装成后台服务

```bash
iphone-control service viewer install   # 登录后自动启动；读设置文件里的 IPHONE_CONTROL_VIEWER_*
iphone-control service status           # 是否在运行，以及 HTTPS 证书的指纹
```

安装时会记下当前终端的 `PATH`，好让控制台找到 `claude` 和 `codex`；`PATH` 变了再装一次。改了 `IPHONE_CONTROL_VIEWER_*` 设置后 `service viewer restart`。升级时会先停下控制台、升级完再启动；有 AI 任务在跑时，自动升级会推迟。

## 从别的机器用

手机接在一台机器上（比如 NAS），在别的机器上用它：

| 你要做的 | 怎么做 |
|---|---|
| 在浏览器里看、操作 | 打开 `https://<那台机器>:8200/?token=…`（控制台开着 `--lan`） |
| 只能填 URL 的 MCP 客户端 | `https://<那台机器>:8200/mcp`，请求头 `Authorization: Bearer <令牌>`，见[接入 AI 客户端](./mcp-clients#通过-url-接入-http) |
| 只能填命令的 MCP 客户端 | 在这台机器上装好 iphone-control，填 `iphone-control mcp --url https://<那台机器>:8200`（加 `--ca cert.pem`），登录令牌放在 `IPHONE_CONTROL_VIEWER_TOKEN`。它把 stdio 转到那台机器的 `/mcp` |
| 在那台手机上开 AI 任务 | `iphone-control task run --url https://<那台机器>:8200 -- <命令>`，见下一节 |
| 看、控制那台机器上的任务 | `iphone-control task list`、`events <id>`、`pause`、`approve <id>`… 都加 `--url`（或设 `IPHONE_CONTROL_URL`） |

自签证书时，别的机器要信任它：把那台机器上的 `~/.local/share/iphone-control/viewer-tls/cert.pem` 拷过来，iphone-control 的命令加 `--ca cert.pem`（或设 `IPHONE_CONTROL_CA`），Node 写的客户端用 `NODE_EXTRA_CA_CERTS=cert.pem` 启动。那台电脑配了代理时，把这台机器的 IP 加进 `NO_PROXY`。

## 给 agent 平台用：AI 任务

AI 任务让控制台看到 agent 的每一步，可以暂停、接管、停止，付款、发消息之前会等你确认；任务进行中，别的客户端只能看，不能操作手机。

**同一台机器上**：

```bash
iphone-control task run --title "整理相册" [--app 照片] -- <启动 agent 的命令>
```

`task run` 开任务、持有租约，再带着任务口令（`IPHONE_CONTROL_TASK_TOKEN`）运行命令；命令里启动的 `iphone-control mcp` 自动加入任务。命令结束，任务就结束；任务在别处被停止，命令也会被停止。`--app` 把任务限定在这些 App 里，到了别的 App 每一步都先等你确认。

**在另一台机器上**：加 `--url`，登录令牌放在 `IPHONE_CONTROL_VIEWER_TOKEN`：

```bash
export IPHONE_CONTROL_VIEWER_TOKEN=<令牌>
iphone-control task run --url https://<那台机器>:8200 --ca cert.pem -- <启动 agent 的命令>
```

命令拿到任务口令、`IPHONE_CONTROL_URL` 和 `IPHONE_CONTROL_MCP_URL`，**拿不到登录令牌**。里面的 agent 照常配置 `iphone-control mcp` 就行：它看到 `IPHONE_CONTROL_URL` 会自动转接到那台机器、只带任务口令；也可以直接用 `$IPHONE_CONTROL_MCP_URL` 加请求头 `Iphone-Control-Task-Token` 走 HTTP。

**执行和控制分开**：agent 只拿任务口令，只能为这个任务操作手机，不能暂停、继续、接管或批准自己的确认；这些由人在控制台上、或由平台用登录令牌来做。同一台机器上，agent 的进程（以及它启动的进程）发来的控制请求也会被拒绝。

**不装 iphone-control 的平台**直接调控制台的接口（都要登录令牌，除了标明的两个）：

| 接口 | 作用 |
|---|---|
| `POST /api/agent/tasks/open`，`{title, owner?, apps?}` | 开任务。返回事件流：第一条 `opened` 带 `{id, token}`，之后是任务的事件。**连接保持着就是租约**，断开后任务记为 abandoned；每 15 秒有一条注释防代理掐断 |
| `GET /api/agent/tasks` | 最近的任务 |
| `GET /api/agent/tasks/<id>/events` | 任务的事件（先重放，再跟着推送） |
| `POST /api/agent/tasks/<id>/pause`（`resume`、`takeover`、`handback`、`stop`） | 控制任务 |
| `POST /api/agent/tasks/<id>/confirm`，`{"approve": true}` | 回答确认 |
| `/mcp`，请求头 `Iphone-Control-Task-Token` | agent 用：只凭任务口令，不用登录 |
| `POST /api/agent/tasks/finish`，`{ok, summary}`，同样的请求头 | agent 用：结束任务、写总结 |

## 经反向代理或隧道访问

用 frp、nginx、cloudflared 把控制台转发到公网时：

- 登录照样起作用，令牌是 256 位随机数；一分钟里错 20 次后，这一分钟内不再校验令牌。
- 用 HTTPS（反向代理那一侧配正式证书），否则令牌和 cookie 在公网上明文传输。
- 更推荐 Tailscale 这类只有你自己的设备连得进来的方式，比直接暴露在公网安全得多。

## 安全须知

- 有能操作的令牌，就能看屏幕、操作手机、用本机登录的 Claude Code / Codex 执行任务：只发给你信任的人，泄露了马上 `viewer token show <名字> --new` 或 `remove`。给只需要看的人发只读令牌。
- 同一个系统用户下，进程能读到彼此的文件：本机上的 agent 能读到令牌文件，但它的控制请求会按进程关系被拒绝。这不是严格的安全边界，要真正隔离，让 agent 换一个系统用户运行或放进沙箱。
- 更多见[安全与隐私](./security)。
