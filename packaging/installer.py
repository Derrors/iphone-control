"""Install a verified iphone-control release for the current user (Python 3 and OpenSSL 3)."""
import argparse
import contextlib
import fcntl
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.parse
import urllib.request


# packaging/release.py binds these in the generated, standalone install.sh.
PUBLISHER = ""
PUBLIC_KEYS = ""
LABEL = "io.github.derrors.iphone-control"
MIB = 1024 * 1024
VERSION = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"


def allowed(url):
    parsed = urllib.parse.urlparse(url)
    if (parsed.scheme != "https" or parsed.username or parsed.password or parsed.port not in (None, 443)
            or parsed.hostname not in {"api.github.com", "github.com", "release-assets.githubusercontent.com",
                                       "objects.githubusercontent.com"}):
        raise ValueError("unexpected release download host")


class GitHubRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        allowed(newurl)
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def copy_limited(source, dest, limit):
    count = 0
    while True:
        chunk = source.read(min(MIB, limit - count + 1))
        if not chunk:
            return count
        count += len(chunk)
        if count > limit:
            raise ValueError("download or archive exceeds size limit")
        dest.write(chunk)


def download(url, dest, limit):
    allowed(url)
    with urllib.request.build_opener(GitHubRedirects()).open(url, timeout=30) as response, dest.open("xb") as out:
        return copy_limited(response, out, limit)


def digest(path):
    with path.open("rb") as f:
        result = hashlib.sha256()
        for chunk in iter(lambda: f.read(MIB), b""):
            result.update(chunk)
        return result.hexdigest()


def verify_manifest(directory, manifest, sig, repo, keys, version, target, asset=None):
    keyfile = directory / "public.der"
    for key in keys.split(","):
        keyfile.write_bytes(bytes.fromhex("302a300506032b6570032100" + key))
        verified = subprocess.run(["openssl", "pkeyutl", "-verify", "-rawin", "-pubin", "-keyform", "DER",
                                   "-inkey", keyfile, "-in", manifest, "-sigfile", sig],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if verified.returncode == 0:
            break
    else:
        raise ValueError("Ed25519 manifest verification failed (requires OpenSSL 3 and a trusted signing key)")
    data = json.loads(manifest.read_bytes())
    name = asset or f"iphone-control-v{version}-{target}.tar.gz"
    if (type(data["schema"]) is not int or data["schema"] != 1 or data["publisher"] != repo
            or data["version"] != version or data["target"] != target or data["asset"] != name
            or not re.fullmatch("[0-9a-f]{64}", data["sha256"])
            or type(data["size"]) is not int or not 0 < data["size"] <= 128 * MIB
            or type(data["unattended"]) is not bool):
        raise ValueError("release manifest does not match requested installation")
    return data


def extract(archive, directory, name):
    # Bound inflated bytes including tar headers before letting the tar parser read metadata.
    raw = directory / "archive.tar"
    with gzip.open(archive, "rb") as stream, raw.open("xb") as out:
        copy_limited(stream, out, 256 * MIB)
    seen = set()
    with tarfile.open(raw, "r:") as tar:
        for item in tar:
            if item.isdir() and item.name == name:
                continue
            parent, _, leaf = item.name.rpartition("/")
            if (not item.isreg() or item.sparse is not None or parent != name or leaf in ("", ".", "..")
                    or leaf in seen or len(seen) >= 64):
                raise ValueError("invalid entry in release archive")
            seen.add(leaf)
            if leaf in ("iphone-control", "env.example"):
                with tar.extractfile(item) as src, (directory / leaf).open("xb") as out:
                    if copy_limited(src, out, item.size) != item.size:
                        raise ValueError("truncated release archive")
    binary = directory / "iphone-control"
    binary.chmod(0o755)
    return binary


def candidate(directory, target, requested):
    if requested:
        version = requested[1:] if requested.startswith("v") else requested
    else:
        latest = directory / "latest.json"
        download(f"https://api.github.com/repos/{PUBLISHER}/releases/latest", latest, 4 * MIB)
        release = json.loads(latest.read_bytes())
        if release["draft"] is not False or release["prerelease"] is not False or not release["tag_name"].startswith("v"):
            raise ValueError("latest release is not a stable tag")
        version = release["tag_name"][1:]
    if not re.fullmatch(VERSION, version):
        raise ValueError("expected a stable version (vX.Y.Z)")
    name = f"iphone-control-v{version}-{target}"
    base = f"https://github.com/{PUBLISHER}/releases/download/v{version}/{name}"
    manifest, sig, archive = (directory / ext for ext in ("manifest.json", "manifest.sig", "archive.tgz"))
    download(base + ".manifest.json", manifest, 16 * 1024)
    if download(base + ".manifest.json.sig", sig, 64) != 64:
        raise ValueError("invalid Ed25519 signature length")
    data = verify_manifest(directory, manifest, sig, PUBLISHER, PUBLIC_KEYS, version, target)
    size = download(base + ".tar.gz", archive, data["size"])
    if size != data["size"] or digest(archive) != data["sha256"]:
        raise ValueError("release archive size or SHA-256 mismatch")
    binary = extract(archive, directory, name)
    found = subprocess.check_output([binary, "--version"], timeout=15).decode().strip()
    if found != f"iphone-control {version}":
        raise ValueError("executable version does not match the signed manifest")
    return binary, version


def run(*args):
    return subprocess.run(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0


def service_files(config):
    if sys.platform == "darwin":
        return "launchd", Path.home() / "Library/LaunchAgents" / (LABEL + ".plist")
    return "systemd", config.parent / "systemd/user/iphone-control.service"


def service_text(binary, kind, config):
    env = {"XDG_CONFIG_HOME": str(config.parent)}
    for name in ("XDG_DATA_HOME", "XDG_RUNTIME_DIR"):
        value = os.environ.get(name)
        if value:
            if not Path(value).is_absolute():
                raise ValueError(f"{name} must be an absolute path")
            env[name] = value
    if kind == "launchd":
        log = str(Path.home() / "Library/Logs/iphone-control/daemon.log")
        return plistlib.dumps(dict(
            Label=LABEL, ProgramArguments=[str(binary), "daemon"], EnvironmentVariables=env,
            RunAtLoad=True, KeepAlive=dict(SuccessfulExit=False), ThrottleInterval=10, ExitTimeOut=30,
            StandardOutPath=log, StandardErrorPath=log,
        )).decode()
    # systemd expands percent specifiers and dollar variables after unquoting ExecStart.
    # Keep the path representation identical to the managed-service validator.
    if any(c in str(binary) for c in '"\\$%') or any(ord(c) < 32 for c in str(binary)):
        raise ValueError("unsupported character in systemd executable path; choose another prefix or --no-service")
    def quote(value):
        if any(ord(c) < 32 for c in value):
            raise ValueError("unsupported control character in service environment")
        return '"' + value.replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%') + '"'
    environment = "\n".join("Environment=" + quote(name + "=" + val) for name, val in env.items())
    return f"""# Installed by install.sh
[Unit]
Description=iphone-control daemon (iPhone over USB, MCP)
After=network-online.target
[Service]
ExecStart="{binary}" daemon
{environment}
Environment=MALLOC_ARENA_MAX=2
Restart=on-failure
RestartSec=10
TimeoutStopSec=30
[Install]
WantedBy=default.target
"""


def owns_service(path, binary, kind):
    if path.is_symlink() or not path.is_file():
        return False
    info = path.stat()
    if info.st_uid != os.getuid() or info.st_mode & 0o022:
        return False
    if kind == "launchd":
        data = plistlib.loads(path.read_bytes())
        args = data.get("ProgramArguments")
        script = f'set -a; f="$HOME/.config/iphone-control/env"; [ -f "$f" ] && . "$f"; set +a; exec "{binary}" daemon'
        # 'service install' can recreate a removed definition using the older shell form.
        shell_safe = not any(c in str(binary) for c in '\\"$`\n\r')
        return (data.get("Label") == LABEL and "Program" not in data and
                (args == [str(binary), "daemon"] or (shell_safe and args == ["/bin/sh", "-c", script])))
    lines = path.read_text().splitlines()
    commands = [line.strip() for line in lines if line.strip().startswith("ExecStart=")]
    if "# Installed by install.sh" in lines:
        return commands == [f'ExecStart="{binary}" daemon']
    return (bool(lines) and lines[0].startswith("# Written by iphone-control. Settings: ")
            and not any(c.isspace() or c in '\\"$%' for c in str(binary))
            and commands == [f"ExecStart={binary} daemon"])


def launchd_loaded(path):
    result = subprocess.run(["launchctl", "print", f"gui/{os.getuid()}/{LABEL}"],
                            env={**os.environ, "LC_ALL": "C"}, text=True, capture_output=True)
    if result.returncode:
        if "Could not find service" in result.stderr:
            return False
        raise ValueError("could not query the launchd user service")
    if f"\tpath = {path}" not in result.stdout.splitlines():
        raise ValueError("another launch agent owns this service label")
    return True


def systemd_owner(path):
    result = subprocess.run(["systemctl", "--user", "show", "iphone-control", "--all", "-p", "FragmentPath", "-p", "DropInPaths"],
                            text=True, check=True, capture_output=True)
    if f"FragmentPath={path}" not in result.stdout.splitlines() or "DropInPaths=" not in result.stdout.splitlines():
        raise ValueError("systemd loaded another unit or an override for this service")


def service_removed(kind, path):
    if path.exists() or path.is_symlink():
        return False
    # A missing definition may still be loaded, or systemd may have selected a
    # package's unit of the same name. Leave either service alone.
    if kind == "launchd":
        if launchd_loaded(path):
            raise ValueError("daemon service is still loaded without its definition")
    else:
        result = subprocess.run(["systemctl", "--user", "show", "iphone-control", "-p", "LoadState",
                                 "-p", "ActiveState"], text=True, capture_output=True)
        if not {"LoadState=not-found", "ActiveState=inactive"}.issubset(result.stdout.splitlines()):
            raise ValueError("could not confirm removal of the recorded systemd service")
    return True


def start_service(binary, config):
    kind, path = service_files(config)
    # Only finish a prior interrupted bootstrap when its complete generated file matches.
    expected = service_text(binary, kind, config)
    ours = path.is_file() and not path.is_symlink() and path.stat().st_uid == os.getuid() and path.read_text() == expected
    if kind == "launchd":
        loaded = launchd_loaded(path)
        if loaded and not ours:
            raise ValueError("another launch agent owns this service label")
        conflicts = [p for p in path.parent.glob("*.iphone-control.plist") if p != path]
    else:
        if not shutil.which("systemctl") or not run("systemctl", "--user", "show-environment"):
            print("No systemd user session. Start the installed binary with 'daemon'.")
            return "none"
        conflicts = [unit for unit in ("iphone-control", "homebrew.iphone-control") if not (unit == "iphone-control" and ours) and run("systemctl", "--user", "cat", unit)]
    if ((path.is_symlink() or path.exists()) and not ours) or conflicts or (not ours and run("pgrep", "-f", r"iphone-control daemon( |$)")):
        raise ValueError("another daemon or service already exists; stop it and resolve its ownership first")
    path.parent.mkdir(parents=True, exist_ok=True)
    if not ours:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
        with os.fdopen(fd, "w") as f:
            f.write(expected)
    if kind == "launchd":
        (Path.home() / "Library/Logs/iphone-control").mkdir(parents=True, exist_ok=True)
        if not loaded:
            subprocess.run(["launchctl", "bootstrap", f"gui/{os.getuid()}", path], check=True)
    else:
        subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
        systemd_owner(path)
        subprocess.run(["systemctl", "--user", "enable", "--now", "iphone-control"], check=True)
    return kind


def write_receipt(path, data):
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
        f.write("\n")
        f.flush()
        os.fsync(f.fileno())
    os.replace(f.name, path)


def read_receipt(path, binary, target):
    if path.is_symlink() or binary.is_symlink():
        raise ValueError("installation or receipt is a symlink")
    if not path.exists() and not binary.exists():
        return None
    if not path.is_file() or not binary.is_file():
        raise ValueError("existing installation has no matching receipt; close its clients and remove the old binary before installing")
    data = json.loads(path.read_bytes())
    if (data["schema"] != 1 or data["publisher"] != PUBLISHER or data["target"] != target
            or data["binary_path"] != str(binary) or data["service"] not in ("launchd", "systemd", "none")
            or binary.stat().st_uid != os.getuid() or path.stat().st_uid != os.getuid()
            or binary.stat().st_mode & 0o022 or path.stat().st_mode & 0o022
            or digest(binary) != data["sha256"]):
        raise ValueError("installed binary does not match the installation receipt")
    return data


def remove_update_state(binary):
    state = binary.parent / ".iphone-control-update"
    if state.is_symlink() or not state.is_dir():
        return
    meta = state.stat()
    if meta.st_uid != os.getuid() or meta.st_mode & 0o077:
        return

    # The caller holds the prefix lock and has verified the installation receipt.
    # Only unlink regular updater files in its private directory. Leave user files
    # and links untouched, including anything unrecognized in an interrupted stage.
    def remove_files(directory, names):
        for name in names:
            path = directory / name
            if not path.is_symlink() and path.is_file() and path.stat().st_uid == os.getuid():
                path.unlink()

    remove_files(state, ("recovery", "previous", "previous.json", "swap-link", "outcome.json"))
    stage = state / "stage"
    if not stage.is_symlink() and stage.is_dir() and stage.stat().st_uid == os.getuid() and not stage.stat().st_mode & 0o077:
        remove_files(stage, ("archive", "candidate"))
        if not any(stage.iterdir()):
            stage.rmdir()
    if not any(state.iterdir()):
        state.rmdir()


def owns_update_job(binary, config):
    launcher = config / "update-job.sh"
    if (not launcher.is_symlink() and launcher.is_file() and launcher.stat().st_uid == os.getuid()
            and not launcher.stat().st_mode & 0o022):
        with launcher.open(errors="replace") as f:
            header = (f.readline(), f.readline())
            try:
                assignment = shlex.split(f.readline())
            except ValueError:
                assignment = []
        return header == ("#!/bin/sh\n", "# iphone-control update job\n") and assignment == ["exe=" + str(binary)]
    return False


@contextlib.contextmanager
def disabled_update_job(binary, config):
    if owns_update_job(binary, config):
        subprocess.run([binary, "update", "policy", "manual"], check=True)
    # The CLI acquires this lock itself. Recheck after it exits, and keep the
    # lock through receipt changes/deletion so a concurrent CLI cannot re-arm.
    fd = os.open(config / "update-schedule.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "rb+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if owns_update_job(binary, config):
            raise ValueError("update schedule was re-enabled during removal; retry")
        clear_update_policy(binary, config)
        yield


def clear_update_policy(binary, config):
    # The updater keys script policy by publisher and canonical executable path.
    # A fresh install at this prefix must get its default policy, even if a
    # previous installation explicitly disabled its schedule during removal.
    key = hashlib.sha256(os.fsencode(binary)).hexdigest()
    policy = config / "updates" / PUBLISHER / (key + ".json")
    if (policy.is_file() and policy.resolve() == config.resolve() / policy.relative_to(config)
            and policy.stat().st_uid == os.getuid()):
        policy.unlink()


def uninstall(binary, config, receipt, data):
    if data is None:
        raise ValueError("no receipted installation at this prefix")
    kind, path = service_files(config)
    # A first install may have been interrupted between writing the unit and recording its activation.
    owned = owns_service(path, binary, kind)
    if data["service"] not in (kind, "none"):
        raise ValueError("unexpected service in the installation receipt")
    if data["service"] != "none" and not owned and not service_removed(kind, path):
        raise ValueError("daemon service no longer matches this installation")
    loaded = owned and kind == "launchd" and launchd_loaded(path)
    if owned and kind == "systemd":
        systemd_owner(path)
    with disabled_update_job(binary, config):
        if owned:
            if kind == "launchd":
                domain = f"gui/{os.getuid()}/{LABEL}"
                if loaded:
                    subprocess.run(["launchctl", "bootout", domain], check=True)
            else:
                subprocess.run(["systemctl", "--user", "disable", "--now", "iphone-control"], check=True)
            path.unlink()
            if kind == "systemd":
                subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
        remove_update_state(binary)
        binary.unlink()
        receipt.unlink()
    print("Removed iphone-control. Your configuration and device data are preserved.")


@contextlib.contextmanager
def transaction_lock(binary_dir):
    path = binary_dir / ".iphone-control-update.lock"
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "rb+") as lock:
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077:
            raise ValueError("invalid update lock ownership or permissions")
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError("an update is in progress") from None
        yield


def bootstrap(binary, config, receipt, data, target, requested, no_service):
    with tempfile.TemporaryDirectory(prefix=".iphone-control-install-", dir=binary.parent) as temp:
        directory = Path(temp)
        source, version = candidate(directory, target, requested)
        sha = digest(source)
        if data and (version != data["version"] or sha != data["sha256"]):
            raise ValueError("manual installation: close clients and uninstall the old version before reinstalling")
        if not data:
            os.link(source, binary)  # same filesystem; never clobber an existing executable
        new = dict(schema=1, publisher=PUBLISHER, target=target, binary_path=str(binary),
                   service="none", version=version, sha256=sha, pinned=bool(requested or (data and data["pinned"])))
        write_receipt(receipt, new)
        if (directory / "env.example").is_file():
            with tempfile.NamedTemporaryFile(dir=config, delete=False) as f:
                f.write((directory / "env.example").read_bytes())
            os.replace(f.name, config / "env.example")
        if not no_service:
            new["service"] = start_service(binary, config)
            write_receipt(receipt, new)
            if new["service"] != "none":
                subprocess.run([binary, "update", "schedule"], check=True)
    print(f"Installed iphone-control {version} at {binary} ({new['service']}).")
    if str(binary.parent) not in map(os.path.realpath, os.get_exec_path()):
        print("To run iphone-control by name, run this command and add it to your shell profile.")
        print(f'  export PATH={shlex.quote(str(binary.parent))}:"$PATH"')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", default=os.getenv("IPHONE_CONTROL_VERSION"))
    parser.add_argument("--prefix", type=Path, default=os.environ.get("IPHONE_CONTROL_PREFIX", str(Path.home() / ".local")))
    parser.add_argument("--no-service", action="store_true", default=os.environ.get("IPHONE_CONTROL_NO_SERVICE") == "1")
    parser.add_argument("--uninstall", action="store_true")
    args = parser.parse_args()
    if not PUBLISHER or not PUBLIC_KEYS:
        raise ValueError("unbound source installer; download install.sh from your publisher's GitHub Release assets")
    if os.getuid() == 0:
        raise ValueError("run as your regular user")
    arch = platform.machine().lower()
    if sys.platform == "darwin" and (arch == "arm64" or run("arch", "-arm64", "/usr/bin/true")):
        target = "macos-arm64"
    elif sys.platform == "linux" and arch in ("x86_64", "aarch64"):
        target = "linux-" + arch
    else:
        raise ValueError("no supported release for this platform")
    binary_dir = (args.prefix / "bin").resolve()
    binary_dir.mkdir(mode=0o755, parents=True, exist_ok=True)
    if binary_dir.stat().st_uid != os.getuid() or binary_dir.stat().st_mode & 0o022:
        raise ValueError("choose a user-owned prefix writable only by this user")
    binary = binary_dir / "iphone-control"
    config = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config") / "iphone-control"
    if not config.is_absolute():
        raise ValueError("XDG_CONFIG_HOME must be absolute")
    config.mkdir(mode=0o700, parents=True, exist_ok=True)
    if config.stat().st_uid != os.getuid() or config.stat().st_mode & 0o022:
        raise ValueError("configuration directory must be writable only by this user")
    receipt = config / "install-receipt.json"
    with (config / "install.lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        with transaction_lock(binary_dir):
            if (binary_dir / ".iphone-control-update/transaction.json").exists():
                raise ValueError(f"finish recovery first: {binary_dir}/.iphone-control-update/recovery update recover")
            data = read_receipt(receipt, binary, target)
            if args.uninstall:
                uninstall(binary, config, receipt, data)
                return
            if not data or data["service"] == "none":
                requested = args.version or (data["version"] if data and data["pinned"] else None)
                bootstrap(binary, config, receipt, data, target, requested, args.no_service)
                return
            if args.version and (args.version not in (data["version"], "v" + data["version"]) or not data["pinned"]):
                raise ValueError("managed installation: use the local update CLI to change versions or pinning")
            kind, path = service_files(config)
            if data["service"] != kind:
                raise ValueError("unexpected service in the installation receipt")
            if service_removed(kind, path):
                if args.no_service:
                    with disabled_update_job(binary, config):
                        data["service"] = "none"
                        write_receipt(receipt, data)
                    return
                if start_service(binary, config) != kind:
                    raise ValueError("could not restore the recorded user service")
            elif not owns_service(path, binary, kind):
                raise ValueError("daemon service no longer matches this installation")
            elif kind == "launchd":
                launchd_loaded(path)
            else:
                systemd_owner(path)
        # The updater acquires the same per-prefix lock.
        subprocess.run([binary, "update", "schedule"], check=True)
        if data["pinned"]:
            print(f"Keeping pinned iphone-control {data['version']}.")
            return
        # `update check` exits non-zero when the release check failed (rate limit, offline: a cached error is kept
        # until its retry time) but still prints the report. The reinstall itself succeeded: keep this version
        report = json.loads(subprocess.run([binary, "update", "check", "--json"], stdout=subprocess.PIPE).stdout)
        if report.get("error_code"):
            print(f"Could not check for a newer release ({report['error_code']}); keeping iphone-control "
                  f"{data['version']}. Run `iphone-control update check` later.")
            return
        if report["availability"] in ("current", "ahead"):
            print(report["guidance"])
            return
        if report["availability"] != "available" or report["install_method"] != "managed":
            raise ValueError(report["guidance"])
        subprocess.run([binary, "update", "apply"], check=True)



if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, tarfile.TarError, EOFError, subprocess.SubprocessError) as err:
        sys.exit(f"Install failed: {err}")
