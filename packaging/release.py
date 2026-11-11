"""Build the pinned bootstrap installer and sign per-platform GitHub Release metadata."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

from installer import digest, verify_manifest


ROOT = Path(__file__).resolve().parents[1]
SLUG = r"[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9][A-Za-z0-9_.-]*"
NUMBER = r"(?:0|[1-9][0-9]*)"
STABLE = rf"v{NUMBER}\.{NUMBER}\.{NUMBER}"
TARGETS = ("linux-x86_64", "linux-aarch64", "macos-arm64")


def config():
    repo = os.environ.get("ICTL_UPDATE_PUBLISHER", "")
    keys = os.environ.get("ICTL_UPDATE_PUBLIC_KEYS", "")
    tap = os.environ.get("ICTL_UPDATE_TAP", "")
    if not re.fullmatch(SLUG, repo) or repo != os.environ["GITHUB_REPOSITORY"]:
        raise ValueError("configure ICTL_UPDATE_PUBLISHER to match this repository")
    if not all(re.fullmatch(r"[a-f0-9]{64}", key) for key in keys.split(",")):
        raise ValueError("configure ICTL_UPDATE_PUBLIC_KEYS with pinned Ed25519 public keys")
    if tap and not re.fullmatch(SLUG, tap):
        raise ValueError("ICTL_UPDATE_TAP must name a Homebrew tap (owner/tap)")
    return repo, keys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("validate")
    sub.add_parser("installer").add_argument("output", type=Path)
    sub.add_parser("upload").add_argument("files", type=Path, nargs="+")
    for action in ("sign", "sha"):
        cmd = sub.add_parser(action)
        cmd.add_argument("directory", type=Path)
        cmd.add_argument("target", choices=TARGETS)
    args = parser.parse_args()
    repo, keys = config()
    tag = os.environ["GITHUB_REF_NAME"]
    if args.command == "validate":
        cargo = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version=1"], cwd=ROOT))
        version = next(p["version"] for p in cargo["packages"] if p["name"] == "iphone-control")
        if tag != f"v{version}":
            raise ValueError("release tag must match the iphone-control Cargo version")
        return
    if args.command == "upload":
        # Re-running a partial job must never replace already published bytes.
        assets = json.loads(subprocess.check_output(["gh", "release", "view", tag, "--json", "assets"]))["assets"]
        existing = {asset["name"] for asset in assets}
        for file in args.files:
            if file.name in existing:
                with tempfile.TemporaryDirectory() as directory:
                    downloaded = Path(directory) / file.name
                    subprocess.run(["gh", "release", "download", tag, "--pattern", file.name,
                                    "--dir", directory], check=True)
                    if digest(downloaded) != digest(file):
                        raise ValueError(f"existing release asset differs: {file.name}")
            else:
                subprocess.run(["gh", "release", "upload", tag, file], check=True)
        return
    if args.command == "installer":
        code = (ROOT / "packaging/installer.py").read_text()
        code = code.replace('PUBLISHER = ""', f"PUBLISHER = {repo!r}", 1)
        code = code.replace('PUBLIC_KEYS = ""', f"PUBLIC_KEYS = {keys!r}", 1)
        args.output.write_text("#!/bin/sh\nexec python3 - \"$@\" <<'ICTL_INSTALLER'\n" + code + "\nICTL_INSTALLER\n")
        args.output.chmod(0o755)
        return
    if not re.fullmatch(STABLE, tag):
        raise ValueError("only stable release manifests may be signed")
    name = f"iphone-control-{tag}-{args.target}"
    manifest = args.directory / f"{name}.manifest.json"
    if args.command == "sha":
        data = verify_manifest(args.directory, manifest, Path(str(manifest) + ".sig"), repo, keys, tag[1:], args.target)
        print(data["sha256"])
        return
    archive = args.directory / f"{name}.tar.gz"
    size = archive.stat().st_size
    if not 0 < size <= 128 * 1024 * 1024:
        raise ValueError("release archive exceeds client limits")
    compatible = os.environ.get("ICTL_ROLLBACK_COMPATIBLE_TAG") == tag
    rollback_from = None
    if compatible:
        rollback_from = os.environ.get("ICTL_ROLLBACK_FROM_VERSION", "")
        if not re.fullmatch(STABLE, "v" + rollback_from):
            raise ValueError("set ICTL_ROLLBACK_FROM_VERSION to the exact tested source version (X.Y.Z)")
        if tuple(map(int, rollback_from.split("."))) >= tuple(map(int, tag[1:].split("."))):
            raise ValueError("the tested source version must precede the release")
    manifest.write_text(json.dumps(dict(
        schema=1, publisher=repo, version=tag[1:], target=args.target, asset=archive.name,
        sha256=hashlib.sha256(archive.read_bytes()).hexdigest(), size=size,
        unattended=os.environ.get("ICTL_UNATTENDED_TAG") == tag,
        rollback_compatible=compatible, rollback_from=rollback_from,
    ), separators=(",", ":")) + "\n")
    # The signing key is a PKCS#8 PEM from the protected signing environment.
    with tempfile.TemporaryDirectory() as private:
        key = Path(private) / "key.pem"
        key.write_text(os.environ.pop("ICTL_UPDATE_SIGNING_KEY"))
        public = subprocess.check_output(["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER"])
        spki = bytes.fromhex("302a300506032b6570032100")
        if not public.startswith(spki) or public[len(spki):].hex() not in keys.split(","):
            raise ValueError("signing key does not match pinned Ed25519 keys")
        subprocess.run(["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key,
                        "-in", manifest, "-out", str(manifest) + ".sig"], check=True)


if __name__ == "__main__":
    main()
