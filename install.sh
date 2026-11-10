#!/bin/sh
# iphone-control installer for macOS (Apple Silicon) and Linux (x86_64, aarch64).
#
#   curl -fsSL https://raw.githubusercontent.com/Derrors/iphone-control/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/Derrors/iphone-control/main/install.sh | sh -s -- --version v0.1.5
#
# Downloads the release archive for this machine, checks its sha256, installs the binary into ~/.local/bin and
# sets up the daemon as a per-user service (launchd on macOS, systemd --user on Linux) that starts at login and
# restarts after a crash. Running it again upgrades. Nothing runs as root; your data stays in
# ~/.local/share/iphone-control.
#
# Options (or the environment variable in brackets):
#   --version vX.Y.Z   install this release instead of the latest   [IPHONE_CONTROL_VERSION]
#   --prefix DIR       install into DIR/bin (default ~/.local)       [IPHONE_CONTROL_PREFIX]
#   --no-service       only install the binary; start the daemon yourself   [IPHONE_CONTROL_NO_SERVICE=1]
#   --uninstall        stop and remove the service and the binary (keeps your data)
set -eu

REPO="Derrors/iphone-control"
LABEL="io.github.derrors.iphone-control"
VERSION="${IPHONE_CONTROL_VERSION:-}"
PREFIX="${IPHONE_CONTROL_PREFIX:-$HOME/.local}"
SERVICE=1
[ "${IPHONE_CONTROL_NO_SERVICE:-}" = 1 ] && SERVICE=0
UNINSTALL=0

# Messages in Chinese when IPHONE_CONTROL_LANG or the locale says so, English otherwise
case "${IPHONE_CONTROL_LANG:-${LC_ALL:-${LC_MESSAGES:-${LANG:-}}}}" in
  zh*) ZH=1 ;;
  *) ZH=0 ;;
esac
say() { if [ "$ZH" = 1 ]; then printf '%s\n' "$1"; else printf '%s\n' "$2"; fi; }
die() { say "错误：$1" "Error: $2" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:-}"; shift 2 ;;
    --prefix) PREFIX="${2:-}"; shift 2 ;;
    --no-service) SERVICE=0; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,19p' "$0" 2>/dev/null || true; exit 0 ;;
    *) die "不认识的参数 $1" "unknown option $1" ;;
  esac
done

# As root, the daemon would use system paths and, on macOS, fight the system usbmuxd
[ "$(id -u)" -ne 0 ] || die "不要用 root 或 sudo 运行，以普通用户安装即可" "don't run this as root or with sudo; install as your regular user"

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Darwin)
    # a shell running under Rosetta reports x86_64 on Apple Silicon
    if [ "$ARCH" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then ARCH=arm64; fi
    [ "$ARCH" = arm64 ] || die "只有 Apple 芯片的 Mac 有预编译版本；Intel Mac 请从源码编译（见 macOS 指南）" \
      "prebuilt binaries are for Apple Silicon Macs only; on an Intel Mac build from source (see the macOS guide)"
    TARGET="macos-arm64"
    ;;
  Linux)
    case "$ARCH" in
      x86_64|amd64) TARGET="linux-x86_64" ;;
      aarch64|arm64) TARGET="linux-aarch64" ;;
      *) die "不支持的架构 $ARCH" "unsupported architecture $ARCH" ;;
    esac
    ;;
  *) die "只支持 macOS 和 Linux" "only macOS and Linux are supported" ;;
esac

BIN_DIR="$PREFIX/bin"
BIN="$BIN_DIR/iphone-control"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT="$UNIT_DIR/iphone-control.service"
CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/iphone-control"

has_systemd_user() { command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; }

stop_service() {
  if [ "$OS" = Darwin ]; then
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  elif has_systemd_user && [ -f "$UNIT" ]; then
    systemctl --user stop iphone-control 2>/dev/null || true
  fi
  # the daemon can take up to half a minute to stop (it ends the phone's HID session and WDA first); both service
  # managers end it by force after 30 s, so wait a little longer than that
  n=0
  while pgrep -f "$BIN daemon" >/dev/null 2>&1 && [ "$n" -lt 45 ]; do sleep 1; n=$((n + 1)); done
}

if [ "$UNINSTALL" = 1 ]; then
  stop_service
  if [ "$OS" = Darwin ]; then
    rm -f "$PLIST"
  elif [ -f "$UNIT" ]; then
    if has_systemd_user; then systemctl --user disable iphone-control 2>/dev/null || true; fi
    rm -f "$UNIT"
    if has_systemd_user; then systemctl --user daemon-reload || true; fi
  fi
  rm -f "$BIN"
  say "已卸载 iphone-control。数据还在 ~/.local/share/iphone-control（Apple ID 会话、签名、自动解锁密码），不需要的话自己删除；手机上的 WebDriverAgent 在手机上删除。" \
      "iphone-control is uninstalled. Your data is still in ~/.local/share/iphone-control (Apple ID session, signing, auto-unlock passcodes); delete it if you no longer need it, and remove WebDriverAgent on the phone."
  exit 0
fi

command -v curl >/dev/null 2>&1 || die "需要 curl" "curl is required"
command -v tar >/dev/null 2>&1 || die "需要 tar" "tar is required"

if [ -z "$VERSION" ]; then
  # the latest release's tag, from where /releases/latest redirects (no API rate limit)
  url="$(curl -fsSLo /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" ||
    die "查不到最新版本，检查网络，或用 --version 指定" "couldn't find the latest release; check the network or pass --version"
  VERSION="${url##*/}"
fi
case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac

NAME="iphone-control-$VERSION-$TARGET"
BASE="https://github.com/$REPO/releases/download/$VERSION"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

say "下载 iphone-control ${VERSION}（${TARGET}）…" "Downloading iphone-control $VERSION ($TARGET)…"
# a progress bar only in a terminal
if [ -t 2 ]; then progress=--progress-bar; else progress=-s; fi
curl -fSL "$progress" -o "$TMP/$NAME.tar.gz" "$BASE/$NAME.tar.gz" ||
  die "下载失败：$BASE/$NAME.tar.gz" "download failed: $BASE/$NAME.tar.gz"
curl -fsSL -o "$TMP/$NAME.tar.gz.sha256" "$BASE/$NAME.tar.gz.sha256" || die "下载校验和失败" "couldn't download the checksum"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$TMP" && sha256sum -c "$NAME.tar.gz.sha256" >/dev/null) || die "sha256 校验不通过" "sha256 check failed"
else
  (cd "$TMP" && shasum -a 256 -c "$NAME.tar.gz.sha256" >/dev/null) || die "sha256 校验不通过" "sha256 check failed"
fi
tar -xzf "$TMP/$NAME.tar.gz" -C "$TMP"

# replacing the binary under a running daemon: stop it first, start it again below
stop_service
mkdir -p "$BIN_DIR" "$CONF_DIR"
install -m 755 "$TMP/$NAME/iphone-control" "$BIN"
[ -f "$TMP/$NAME/env.example" ] && cp "$TMP/$NAME/env.example" "$CONF_DIR/env.example"
say "已安装 ${BIN}（$("$BIN" --version)）" "Installed $BIN ($("$BIN" --version))"

if [ "$OS" = Linux ] && ! command -v usbmuxd >/dev/null 2>&1 && [ ! -x /usr/sbin/usbmuxd ]; then
  say "还缺 usbmuxd（和 iPhone 通过 USB 通信要用）：sudo apt install usbmuxd（其他发行版用对应的包管理器）" \
      "usbmuxd is missing (needed to talk to the iPhone over USB): sudo apt install usbmuxd (or your distribution's package manager)"
fi

started=0
if [ "$SERVICE" = 1 ]; then
  if pgrep -f "iphone-control daemon" >/dev/null 2>&1; then
    # a daemon started by hand: a second one would fight it for the phone
    say "已有一个手动启动的守护进程在运行，没有启动服务。先停掉它（pkill -f 'iphone-control daemon'），再重新运行这个脚本。" \
        "A daemon you started by hand is running, so the service wasn't started. Stop it (pkill -f 'iphone-control daemon') and run this script again."
  elif [ "$OS" = Darwin ]; then
    mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/iphone-control"
    # settings from ~/.config/iphone-control/env, like the systemd service on Linux
    cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>-c</string>
    <string>set -a; f="\$HOME/.config/iphone-control/env"; [ -f "\$f" ] &amp;&amp; . "\$f"; set +a; exec "$BIN" daemon</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ExitTimeOut</key><integer>30</integer>
  <key>StandardOutPath</key><string>$HOME/Library/Logs/iphone-control/daemon.log</string>
  <key>StandardErrorPath</key><string>$HOME/Library/Logs/iphone-control/daemon.log</string>
</dict>
</plist>
EOF
    launchctl bootstrap "gui/$(id -u)" "$PLIST" && started=1
  elif has_systemd_user; then
    mkdir -p "$UNIT_DIR"
    cat > "$UNIT" <<EOF
# Installed by install.sh. Settings: ~/.config/iphone-control/env (see env.example next to it)
[Unit]
Description=iphone-control daemon (iPhone over USB, MCP)
After=network-online.target

[Service]
ExecStart=$BIN daemon
EnvironmentFile=-%h/.config/iphone-control/env
Environment=MALLOC_ARENA_MAX=2
Restart=on-failure
RestartSec=10
TimeoutStopSec=30

[Install]
WantedBy=default.target
EOF
    systemctl --user daemon-reload
    systemctl --user enable --now iphone-control && started=1
    if command -v loginctl >/dev/null 2>&1 && [ "$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null)" != yes ]; then
      say "要不登录也随开机启动，运行：sudo loginctl enable-linger $(id -un)" \
          "To start it at boot without logging in, run: sudo loginctl enable-linger $(id -un)"
    fi
  else
    say "没有可用的 systemd 用户会话，没有安装服务。手动启动：$BIN daemon" \
        "No systemd user session, so no service was installed. Start it yourself: $BIN daemon"
  fi
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) say "$BIN_DIR 不在 PATH 里。把这行加到 shell 的配置文件（~/.zshrc 或 ~/.bashrc）：export PATH=\"$BIN_DIR:\$PATH\"" \
         "$BIN_DIR isn't on your PATH. Add this line to your shell profile (~/.zshrc or ~/.bashrc): export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

echo
if [ "$started" = 1 ]; then
  say "守护进程已作为服务运行，登录后自动启动。" "The daemon runs as a service and starts when you log in."
elif [ "$SERVICE" = 0 ]; then
  say "没有安装服务。启动守护进程：$BIN daemon" "No service installed. Start the daemon: $BIN daemon"
fi
say "接下来：用数据线连上 iPhone，然后运行

  iphone-control

它会一步步带你完成设置（配对、开发者模式、Apple ID、安装 WDA、注册到 Claude Code / Codex），设置好以后再运行只显示状态。" \
"Next: connect the iPhone with a cable, then run

  iphone-control

It walks you through setup step by step (pairing, Developer Mode, Apple ID, WDA, registering with Claude Code / Codex); once set up, running it again just shows the status."
