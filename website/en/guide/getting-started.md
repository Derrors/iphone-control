# Getting Started

Deploying with Docker takes about 15 minutes. Without Docker, see [Source / Binary](../deploy/source).

## You need

- A Linux host with Docker (including Compose v2)
- An iPhone (iOS 17+) and a cable that carries data
- A free Apple ID (a dedicated one is recommended)
- Internet access to Apple and GitHub from the host (set a proxy if needed)

::: warning One phone, one owner
Don't run two copies of iphone-control, or other usbmuxd-based tools (go-ios, pymobiledevice3, …), against the same phone. If the host has usbmuxd installed, run `sudo systemctl mask usbmuxd` first.
:::

## 1. Start

```bash
git clone https://github.com/Derrors/iphone-control && cd iphone-control
cp .env.example .env        # set HTTP_PROXY / HTTPS_PROXY here if you need a proxy
docker compose up -d --build
```

The first build compiles Rust and takes a few minutes. After a release you can use the prebuilt image instead; see [Docker](../deploy/docker).

## 2. Run the setup wizard

Plug in the phone, unlock it, then:

```bash
docker exec -it iphone-control iphone-control setup
```

The wizard has six steps. What you do on the phone:

| Step | On the phone |
|---|---|
| 1. Connect | Unlock it |
| 2. Pair | Tap *Trust* when asked "Trust This Computer?", enter the passcode |
| 3. Developer Mode | *Settings → Privacy & Security → Developer Mode → On*, restart, then tap *Turn On* after unlocking |
| 4. Apple ID | (in the terminal) enter the Apple ID, password and two-factor code |
| 5. Sign and install WDA | Keep the phone unlocked |
| 6. Start WDA | *Settings → General → VPN & Device Management* → trust your Apple ID; if asked to *enter the iPhone passcode for "XCTest"*, enter it |

You can run the wizard again at any time: pairing, Developer Mode and login are skipped when already done; signing is redone. You're finished when it says WDA is ready (WDA 就绪).

::: tip Your password stays in the terminal
The Apple ID password and code are only typed here and never written to disk; only the resulting session is stored.
:::

## 3. Connect your AI client

```bash
docker exec -it iphone-control iphone-control mcp-config
```

It prints the registration command. For Claude Code:

```bash
claude mcp add iphone -s user -- docker exec -i iphone-control iphone-control mcp
```

Other clients: [Connect an AI Client](./mcp-clients).

## 4. Try it

In Claude Code, say:

> Open Settings on the iPhone and tell me which iOS version it runs.

The agent will use `iphone_launch_app`, `iphone_tap_element`, `iphone_screenshot` and friends.

## Checking on it

```bash
docker exec iphone-control iphone-control status   # state, signing time left, recent log
docker exec iphone-control iphone-control doctor   # layer-by-layer checks with a fix for each failure
```

Something wrong? See the [FAQ](../troubleshoot/faq).
