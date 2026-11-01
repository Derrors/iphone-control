# Connect an AI Client

The client starts the MCP server on demand: `docker exec -i iphone-control iphone-control mcp` for Docker, `iphone-control --socket <path> mcp` for a source install. When in doubt, run `iphone-control mcp-config` — it prints the right command for your setup.

## Claude Code

```bash
claude mcp add iphone -s user -- docker exec -i iphone-control iphone-control mcp
```

`-s user` makes it available in every project. Restarting the container drops the MCP connection; run `/mcp` in Claude Code to reconnect.

## Claude Desktop, Cursor and others

Add this to the client's MCP configuration:

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

For a source install, use the command that `mcp-config` prints, for example:

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

::: warning Client and daemon on the same machine
The MCP server reaches the daemon through a local Unix socket. If the client runs on another computer, start it over SSH, e.g. `"command": "ssh", "args": ["host", "docker", "exec", "-i", "iphone-control", "iphone-control", "mcp"]`.
:::

## Tips for the agent

The server already explains the tools to the agent, so usually nothing else is needed. You may want to remind it to:

- Ask you before paying, deleting, sending messages, or changing account or privacy settings.
- Tap by text when possible; games and maps only work from screenshots.
