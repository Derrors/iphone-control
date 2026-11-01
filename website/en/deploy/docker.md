# Docker

The recommended way to run it. The image holds one Rust binary plus Debian's usbmuxd, and the container is not `privileged`.

## Install

### Build it yourself

```bash
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cp .env.example .env
docker compose up -d --build
```

### Use the prebuilt image

After a release, images are published at `ghcr.io/derrors/iphone-control` (amd64, arm64). In `.env`:

```ini
IMAGE=ghcr.io/derrors/iphone-control:latest   # or pin a version, e.g. :0.1.0
```

Then:

```bash
docker compose pull && docker compose up -d --no-build
```

Afterwards, run the [setup wizard](../guide/getting-started) once.

## What the compose file does

- Allows only USB character devices (`device_cgroup_rules: "c 189:* rmw"`) and bind-mounts `/dev/bus/usb`, so device nodes created after re-plugging show up in the container.
- `network_mode: host` — only so the host's proxy is reachable; nothing listens on a port.
- Mounts `./data` at `/var/lib/iphone-control`, so pairing and login survive re-creating the container.
- `init: true` and `stop_grace_period: 30s` — on stop it closes the sessions on the phone first, so no "paired computer" icon is left behind.

## Upgrade

```bash
git pull
docker compose up -d --build                            # self-built
# or
docker compose pull && docker compose up -d --no-build  # prebuilt image
```

Data lives in `./data` and is untouched. Restarting the container drops the MCP connection in your AI client; reconnect it (`/mcp` in Claude Code).

::: tip Wait until it's ready
WDA is ready about 10 seconds after a restart. Several restarts in a row can make the phone ask for the passcode for "XCTest" again — just enter it.
:::

## Everyday commands

```bash
docker exec iphone-control iphone-control status    # state
docker exec iphone-control iphone-control doctor    # diagnosis
docker logs iphone-control                          # logs
docker compose down                                 # stop (data is kept)
```
