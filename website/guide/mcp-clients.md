# 接入 AI 客户端

MCP 服务由客户端按需拉起：Docker 部署时是 `docker exec -i iphone-control iphone-control mcp`，从源码运行时是 `iphone-control --socket <路径> mcp`。不确定时运行 `iphone-control mcp-config`，它会按你的部署方式打印正确的命令。

## Claude Code

```bash
claude mcp add iphone -s user -- docker exec -i iphone-control iphone-control mcp
```

`-s user` 让所有项目都能用。容器重启后 MCP 连接会断开，在 Claude Code 里运行 `/mcp` 重新连接。

## Claude Desktop、Cursor 等

在客户端的 MCP 配置文件里加上：

```json
{
  "mcpServers": {
    "iphone": {
      "command": "docker",
      "args": ["exec", "-i", "iphone-control", "iphone-control", "mcp"]
    }
  }
}
```

从源码运行时把 `command`、`args` 换成 `mcp-config` 打印的命令，例如：

```json
{
  "mcpServers": {
    "iphone": {
      "command": "/home/you/.cargo/bin/iphone-control",
      "args": ["--socket", "/run/user/1000/iphone-control/ctl.sock", "mcp"]
    }
  }
}
```

要开启[开发者工具](./tools#开发者工具-显式开启)或英文界面：直接运行时在 `iphone` 这一项里加 `"env": {"IPHONE_CONTROL_DEV_TOOLS": "1", "IPHONE_CONTROL_LANG": "en"}`；Docker 部署时在 `args` 的 `"-i"` 后面加 `"-e", "IPHONE_CONTROL_DEV_TOOLS=1"`（语言同样是 `"-e", "IPHONE_CONTROL_LANG=en"`）。

macOS 上没有 `XDG_RUNTIME_DIR`，socket 在 `~/.local/share/iphone-control/ctl.sock`，同样以 `mcp-config` 打印的为准。Claude Desktop 的配置文件在 `~/Library/Application Support/Claude/claude_desktop_config.json`。

::: warning 客户端和守护进程要在同一台机器上
MCP 服务经本机的 Unix socket 找守护进程。客户端在另一台电脑上时，用 SSH 拉起，例如 `"command": "ssh", "args": ["主机", "docker", "exec", "-i", "iphone-control", "iphone-control", "mcp"]`。
:::

## 给 agent 的建议

服务端已经把用法告诉了 agent，一般不用额外说明。可以提醒它：

- 涉及付款、删除、发消息、改账号或隐私设置的操作，先问你。
- 能按文字点就别按坐标点；游戏、地图只能看截图操作。
