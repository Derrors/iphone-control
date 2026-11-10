# Connect an AI Client

The client starts the MCP server on demand with `iphone-control --socket <path> mcp`. The last step of the first-run guide registers it with Claude Code and Codex for you; configure other clients by hand as below. When in doubt, run `iphone-control mcp-config` — it prints the right command for this machine.

## Claude Code and Codex

When the guide finds `claude` or `codex` on `PATH`, it asks whether to register with it. That amounts to:

```bash
claude mcp add iphone -s user -- /path/to/iphone-control --socket <path> mcp
codex mcp add iphone -- /path/to/iphone-control --socket <path> mcp
```

Open a new session afterwards and the tools are there. `-s user` makes it available in every Claude Code project. The program path is one that stays the same across upgrades: for Homebrew, the `opt` link, so there is no need to register again after `brew upgrade`.

If you install Claude Code or Codex later, or want to register again, run `iphone-control setup --reconfigure` and pick MCP registration. You can also run the command that `iphone-control mcp-config` prints.

Restarting the daemon drops the MCP connection; run `/mcp` in Claude Code to reconnect.

## Claude Desktop, Cursor and others

Add this to the client's MCP configuration, with the program path and arguments that `mcp-config` prints, for example:

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

To turn on the [developer tools](./tools#developer-tools-opt-in) or English, add `"env": {"IPHONE_CONTROL_DEV_TOOLS": "1", "IPHONE_CONTROL_LANG": "en"}` to the `iphone` entry.

macOS has no `XDG_RUNTIME_DIR`, so the socket is `~/.local/share/iphone-control/ctl.sock`; again, use what `mcp-config` prints. Claude Desktop's configuration file is `~/Library/Application Support/Claude/claude_desktop_config.json`.

::: warning Client and daemon on the same machine
The MCP server reaches the daemon through a local Unix socket. If the client runs on another computer, start it over SSH, followed by the program path and arguments `mcp-config` prints on that host, e.g. `"command": "ssh", "args": ["host", "/home/you/.local/bin/iphone-control", "--socket", "/run/user/1000/iphone-control/ctl.sock", "mcp"]`.
:::

## Tips for the agent

The server already explains the tools to the agent, so usually nothing else is needed. You may want to remind it to:

- Ask you before paying, deleting, sending messages, or changing account or privacy settings.
- Tap by text when possible; games and maps only work from screenshots.
