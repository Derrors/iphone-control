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
MCP 服务经本机的 Unix socket 找守护进程。客户端在另一台电脑上时，用 SSH 拉起，后面跟那台主机上 `mcp-config` 打印的程序路径和参数，例如 `"command": "ssh", "args": ["主机", "/home/you/.local/bin/iphone-control", "--socket", "/run/user/1000/iphone-control/ctl.sock", "mcp"]`。
:::

## 给 agent 的建议

服务端已经把用法告诉了 agent，一般不用额外说明。可以提醒它：

- 涉及付款、删除、发消息、改账号或隐私设置的操作，先问你。
- 能按文字点就别按坐标点；游戏、地图只能看截图操作。
