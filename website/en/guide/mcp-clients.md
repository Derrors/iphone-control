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
The MCP server reaches the daemon through a local Unix socket. If the client runs on another computer, connect [by URL](#connect-by-url-http) as below, or start it over SSH, followed by the program path and arguments `mcp-config` prints on that host, e.g. `"command": "ssh", "args": ["host", "/home/you/.local/bin/iphone-control", "--socket", "/run/user/1000/iphone-control/ctl.sock", "mcp"]`.
:::

## Connect by URL (HTTP)

The viewer (`iphone-control viewer`, or the service from `service viewer install`) also serves MCP over HTTP (Streamable HTTP) on its port, at `/mcp`, with the same tools as stdio above. For clients that only take a URL, and agents on another computer.

- **Login**: like the page, it needs the token, as `Authorization: Bearer <token>`. `iphone-control viewer token` shows it and the command below.
- **This machine**: `http://127.0.0.1:8200/mcp` (the viewer's port). Claude Code: `claude mcp add --transport http iphone http://127.0.0.1:8200/mcp --header "Authorization: Bearer <token>"`; in a configuration file `{"type": "http", "url": "http://127.0.0.1:8200/mcp", "headers": {"Authorization": "Bearer <token>"}}`.
- **Another computer on the local network**: the viewer needs `--lan` (`IPHONE_CONTROL_VIEWER_LAN=1`); the address is `https://<this machine's address>:8200/mcp`. With the self-signed certificate made there, the client has to trust it: copy `~/.local/share/iphone-control/viewer-tls/cert.pem` from that machine and start Node-based clients (Claude Code and others) with `NODE_EXTRA_CA_CERTS=cert.pem`; or give the viewer a certificate of your own (`--tls-cert`/`--tls-key`). If the client uses a proxy, add the machine's address to `NO_PROXY`.
- **In a task**: when an agent platform opens a task with `task run`, have the agent's MCP requests carry the header `Iphone-Control-Task-Token: <the value of IPHONE_CONTROL_TASK_TOKEN>`; it does what that variable does on stdio (each step shows on the page, calls wait while paused, `iphone_confirm` is listed). **That header alone, without a login, is enough**: while the task runs, `/mcp` takes its token, and the agent can act on the phone for that task only, with no way to pause or confirm. Across machines that is how it should be: the platform keeps the login, the agent gets the task's token.
- **A platform on another machine**: `iphone-control task run --url https://<address>:8200 -- <command>` opens the task and holds its lease for you; the command can set up its agent with `$IPHONE_CONTROL_MCP_URL` and `$IPHONE_CONTROL_TASK_TOKEN`. A platform without iphone-control calls the API: `POST /api/agent/tasks/open` (`Authorization: Bearer <login token>`, JSON `{title, owner?, apps?}`) answers with server-sent events, `opened` with `{id, token}` first; the open connection is the lease, and when it closes the task is abandoned. Pause, resume, take over, hand back, stop and confirm are `POST /api/agent/tasks/<id>/<action>` (confirm with `{"approve": true}`), events `GET /api/agent/tasks/<id>/events`, the list `GET /api/agent/tasks`; the agent ends its own task with `POST /api/agent/tasks/finish` (its task's token, `{ok, summary}`).
- **Off**: `viewer --mcp off` (`IPHONE_CONTROL_VIEWER_MCP=off`).

::: warning The token is the key
Whoever has it can control the phone: keep it out of configuration that gets shared, and replace it with `iphone-control viewer token --new` if it leaks. As for the page, only local-network addresses may connect and `Host` is checked; other web pages in a browser can't use it (a request with an `Origin` must come from the page itself).
:::

## Tips for the agent

The server already explains the tools to the agent, so usually nothing else is needed. You may want to remind it to:

- Ask you before paying, deleting, sending messages, or changing account or privacy settings.
- Tap by text when possible; games and maps only work from screenshots.
