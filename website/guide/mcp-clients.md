# 接入 AI 客户端

MCP 服务由客户端按需拉起，命令是 `iphone-control --socket <路径> mcp`。首次运行引导的最后一步会替你注册到 Claude Code 和 Codex；其他客户端照下面手动配置。不确定时运行 `iphone-control mcp-config`，它会打印这台机器上正确的命令。

## Claude Code 和 Codex

引导找到 PATH 里的 `claude` 或 `codex` 时，会问要不要注册，相当于运行：

```bash
claude mcp add iphone -s user -- /path/to/iphone-control --socket <路径> mcp
codex mcp add iphone -- /path/to/iphone-control --socket <路径> mcp
```

注册完开一个新的会话就能用。`-s user` 让 Claude Code 的所有项目都能用。程序路径用的是升级后不变的那个：Homebrew 装的用 `opt` 下的链接，`brew upgrade` 后不用重新注册。

后来才装 Claude Code 或 Codex、或者要重新注册时，运行 `iphone-control setup --reconfigure`，选「注册 MCP」。也可以手动执行 `iphone-control mcp-config` 打印的命令。

守护进程重启后 MCP 连接会断开，在 Claude Code 里运行 `/mcp` 重新连接。

## Claude Desktop、Cursor 等

在客户端的 MCP 配置文件里加上，`command`、`args` 换成 `mcp-config` 打印的程序路径和参数，例如：

```json
{
  "mcpServers": {
    "iphone": {
      "command": "/home/you/.local/bin/iphone-control",
      "args": ["--socket", "/run/user/1000/iphone-control/ctl.sock", "mcp"]
    }
  }
}
```

要开启[开发者工具](./tools#开发者工具-显式开启)或英文界面，在 `iphone` 这一项里加 `"env": {"IPHONE_CONTROL_DEV_TOOLS": "1", "IPHONE_CONTROL_LANG": "en"}`。

macOS 上没有 `XDG_RUNTIME_DIR`，socket 在 `~/.local/share/iphone-control/ctl.sock`，同样以 `mcp-config` 打印的为准。Claude Desktop 的配置文件在 `~/Library/Application Support/Claude/claude_desktop_config.json`。

::: warning 客户端和守护进程要在同一台机器上
MCP 服务经本机的 Unix socket 找守护进程。客户端在另一台电脑上时，用下面的 [URL 方式](#通过-url-接入-http)，或者用 SSH 拉起，后面跟那台主机上 `mcp-config` 打印的程序路径和参数，例如 `"command": "ssh", "args": ["主机", "/home/you/.local/bin/iphone-control", "--socket", "/run/user/1000/iphone-control/ctl.sock", "mcp"]`。
:::

## 通过 URL 接入（HTTP）

控制台（`iphone-control viewer`，或装成服务的 `service viewer install`）在同一个端口上还提供 MCP over HTTP（Streamable HTTP），路径是 `/mcp`，工具和上面的 stdio 方式完全一样。适合只能填 URL 的客户端，以及另一台电脑上的 agent。

- **本机**：`http://127.0.0.1:8200/mcp`（端口跟着控制台）。Claude Code：`claude mcp add --transport http iphone http://127.0.0.1:8200/mcp`；配置文件里写 `{"type": "http", "url": "http://127.0.0.1:8200/mcp"}`。
- **局域网里另一台电脑**：控制台要开 `--lan`（`IPHONE_CONTROL_VIEWER_LAN=1`），地址是 `https://<这台机器的 IP>:8200/mcp`。证书是本机生成的自签证书时，客户端要信任它：把那台机器上的 `~/.local/share/iphone-control/viewer-tls/cert.pem` 拷过来，Node 写的客户端（Claude Code 等）用 `NODE_EXTRA_CA_CERTS=cert.pem` 启动；也可以用 `--tls-cert`/`--tls-key` 换成自己的证书。客户端配了代理时，把这台机器的 IP 加进 `NO_PROXY`。
- **在任务里**：agent 平台用 `task run` 开任务时，让 agent 的 MCP 请求带上请求头 `Iphone-Control-Task-Token: <IPHONE_CONTROL_TASK_TOKEN 的值>`，作用和 stdio 方式下的环境变量一样（每一步显示在控制台上，暂停时等待，多出 `iphone_confirm`）。
- **关掉**：`viewer --mcp off`（`IPHONE_CONTROL_VIEWER_MCP=off`）。

::: warning 还没有登录
能打开控制台的人就能用 `/mcp`：本机是本机所有用户，开了 `--lan` 就是局域网里所有能连上这台机器的人。和控制台一样只接受局域网地址来的连接、检查 `Host`；浏览器里别的网页借不了它（带 `Origin` 的请求必须来自控制台自己）。
:::

## 给 agent 的建议

服务端已经把用法告诉了 agent，一般不用额外说明。可以提醒它：

- 涉及付款、删除、发消息、改账号或隐私设置的操作，先问你。
- 能按文字点就别按坐标点；游戏、地图只能看截图操作。
