"""Signed offline install fixture. The service manager, HOME, and network are isolated."""
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import plistlib
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import installer


ROOT = Path(__file__).resolve().parents[1]
REPO = "example/iphone-control"
VERSION = "0.2.1"


class InstallerTest(unittest.TestCase):
    def setUp(self):
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.root = Path(self.stack.enter_context(tempfile.TemporaryDirectory())).resolve()
        self.dist = self.root / "dist"
        self.dist.mkdir()
        self.prefix = self.root / "custom prefix"
        self.config = self.root / "config/iphone-control"
        self.receipt = self.config / "install-receipt.json"
        self.binary = self.prefix / "bin/iphone-control"
        commands = self.root / "commands"
        commands.mkdir()
        self.log = self.root / "calls"
        for cmd in ("systemctl", "pgrep", "launchctl"):
            stub = commands / cmd
            stub.write_text('#!/bin/sh\necho "$0 $*" >> "$INSTALL_LOG"\n'
                            'case "$0 $*" in *" --user show iphone-control "*) echo "FragmentPath=$XDG_CONFIG_HOME/systemd/user/iphone-control.service"; echo DropInPaths=;; *pgrep*|*"launchctl print "*|*" --user cat "*) exit 1;; esac\n')
            stub.chmod(0o755)
        key = self.root / "private.pem"
        subprocess.run(["openssl", "genpkey", "-algorithm", "Ed25519", "-out", key], check=True)
        pub = subprocess.check_output(["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER"])[-32:].hex()
        self.stack.enter_context(patch.dict(os.environ, {
            "HOME": str(self.root), "XDG_CONFIG_HOME": str(self.config.parent), "INSTALL_LOG": str(self.log),
            "PATH": str(commands) + os.pathsep + os.environ["PATH"],
            "ICTL_UPDATE_PUBLISHER": REPO, "GITHUB_REPOSITORY": REPO, "ICTL_UPDATE_PUBLIC_KEYS": pub,
            "ICTL_UPDATE_TAP": "", "GITHUB_REF_NAME": "v" + VERSION,
            "ICTL_UPDATE_SIGNING_KEY": key.read_text(), "ICTL_UNATTENDED_TAG": "", "ICTL_ROLLBACK_COMPATIBLE_TAG": "",
        }))
        self.stack.enter_context(patch.multiple(installer, PUBLISHER=REPO, PUBLIC_KEYS=pub))
        self.stack.enter_context(patch.object(installer, "download", self.download))
        self.target = "macos-arm64" if sys.platform == "darwin" else "linux-" + platform.machine()
        self.name = f"iphone-control-v{VERSION}-{self.target}"
        self.archive = self.dist / (self.name + ".tar.gz")
        self.manifest = self.dist / (self.name + ".manifest.json")
        self.sig = self.dist / (self.name + ".manifest.json.sig")
        self.make_archive()
        self.sign()

    def make_archive(self, bad=""):
        with tarfile.open(self.archive, "w:gz") as tar:
            exe = b'#!/bin/sh\nif [ "$1" = --version ]; then echo iphone-control 0.2.1; else echo "$*" >> "$INSTALL_LOG"; fi\n'
            for leaf, contents in [("iphone-control", exe), ("env.example", b"# Example\n")]:
                entry = tarfile.TarInfo(f"{self.name}/{leaf}")
                entry.size = len(contents)
                tar.addfile(entry, io.BytesIO(contents))
            if bad:
                entry = tarfile.TarInfo(f"{self.name}/../outside" if bad == "escape" else f"{self.name}/link")
                if bad == "link":
                    entry.type = tarfile.SYMTYPE
                    entry.linkname = "elsewhere"
                tar.addfile(entry)

    def sign(self):
        subprocess.run([sys.executable, ROOT / "packaging/release.py", "sign", self.dist, self.target], check=True)

    def download(self, url, dest, limit):
        if url == f"https://api.github.com/repos/{REPO}/releases/latest":
            dest.write_text(json.dumps(dict(tag_name="v" + VERSION, draft=False, prerelease=False)))
            return dest.stat().st_size
        self.assertTrue(url.startswith(f"https://github.com/{REPO}/releases/download/v{VERSION}/"))
        with (self.dist / url.rsplit("/", 1)[-1]).open("rb") as src, dest.open("xb") as out:
            return installer.copy_limited(src, out, limit)

    def install(self, *args):
        with patch.object(sys, "argv", ["install.sh", "--prefix", str(self.prefix), *args]):
            installer.main()

    def test_install_and_uninstall(self):
        self.install()
        self.assertEqual(json.loads(self.receipt.read_text()), dict(
            schema=1, publisher=REPO, binary_path=str(self.binary), target=self.target, version=VERSION,
            service=installer.service_files(self.config)[0], pinned=False,
            sha256=hashlib.sha256(self.binary.read_bytes()).hexdigest()))
        self.assertIn("update schedule", self.log.read_text())
        self.install()  # upgrades must go through the installed updater
        self.assertIn("update apply", self.log.read_text())
        self.install("--uninstall")
        self.assertFalse(self.binary.exists())
        self.assertFalse(self.receipt.exists())
        self.assertTrue((self.config / "env.example").exists())
        self.assertIn("update policy manual", self.log.read_text())

    def test_service_arguments_are_literal(self):
        binary = self.root / 'custom $(name) " & % path' / "iphone-control"
        launch = plistlib.loads(installer.service_text(binary, "launchd", self.config).encode())
        self.assertEqual(launch["ProgramArguments"], [str(binary), "daemon"])
        self.assertEqual(launch["EnvironmentVariables"]["XDG_CONFIG_HOME"], str(self.config.parent))
        with self.assertRaisesRegex(ValueError, "unsupported character"):
            installer.service_text(binary, "systemd", self.config)
        unit = installer.service_text(self.binary, "systemd", self.config)
        self.assertIn(f'ExecStart="{self.binary}" daemon', unit)
        self.assertIn(f'Environment="XDG_CONFIG_HOME={self.config.parent}"', unit)

    def test_fixed_manual_install(self):
        self.install("--version", VERSION, "--no-service")
        data = json.loads(self.receipt.read_text())
        self.assertTrue(data["pinned"])
        self.assertEqual(data["service"], "none")
        self.assertFalse(self.log.exists())
        self.install("--uninstall")

    def test_bad_key_digest_identity_and_tar(self):
        with patch.object(installer, "PUBLIC_KEYS", "11" * 32), self.assertRaisesRegex(ValueError, "verification failed"):
            self.install("--no-service")
        raw = self.archive.read_bytes()
        self.archive.write_bytes(raw[:-2] + b"ab")
        with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
            self.install("--no-service")
        self.archive.write_bytes(raw)
        for field, value in (("publisher", "other/repo"), ("target", "wrong"), ("size", 128 * installer.MIB + 1)):
            with self.subTest(field=field):
                self.sign()
                data = json.loads(self.manifest.read_bytes())
                data[field] = value
                self.manifest.write_text(json.dumps(data))
                subprocess.run(["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", self.root / "private.pem",
                                "-in", self.manifest, "-out", self.sig], check=True)
                with self.assertRaisesRegex(ValueError, "does not match"):
                    self.install("--no-service")
        for bad in ("link", "escape"):
            with self.subTest(bad=bad):
                self.make_archive(bad)
                self.sign()
                with self.assertRaisesRegex(ValueError, "invalid entry"):
                    self.install("--no-service")
        self.assertFalse(self.binary.exists())
        self.assertFalse(self.receipt.exists())

    def test_receipt_and_lock_prevent_wrong_install(self):
        self.install("--no-service")
        with self.assertRaisesRegex(ValueError, "matching receipt"):
            self.install("--prefix", str(self.root / "other"), "--uninstall")
        with installer.transaction_lock(self.binary.parent), self.assertRaisesRegex(ValueError, "update is in progress"):
            self.install("--uninstall")
        self.binary.write_bytes(b"foreign binary")
        with self.assertRaisesRegex(ValueError, "does not match"):
            self.install("--uninstall")
        self.assertEqual(self.binary.read_bytes(), b"foreign binary")

    def test_linux_service_fixtures(self):
        for arch in ("aarch64", "x86_64"):
            with self.subTest(arch=arch), patch.object(sys, "platform", "linux"), patch.object(platform, "machine", return_value=arch):
                self.target = "linux-" + arch
                self.name = f"iphone-control-v{VERSION}-{self.target}"
                self.archive = self.dist / (self.name + ".tar.gz")
                self.make_archive()
                self.sign()
                self.install()
                self.assertEqual(json.loads(self.receipt.read_bytes())["target"], self.target)
                self.assertEqual(json.loads(self.receipt.read_bytes())["service"], "systemd")
                self.install("--uninstall")

    def test_generated_and_unbound_bootstrap(self):
        bound = self.dist / "install.sh"
        cmd = [sys.executable, ROOT / "packaging/release.py", "installer", bound]
        subprocess.run(cmd, check=True)
        subprocess.run(["sh", bound, "--help"], stdout=subprocess.DEVNULL, check=True)
        self.assertIn(f"PUBLISHER = {REPO!r}", bound.read_text())
        self.assertNotIn("PRIVATE KEY", bound.read_text())
        # Exercise the generated shell entry point in a child process; substitute only
        # the HTTPS transport. All verification, staging and receipt code runs as shipped.
        module_dir = self.root / "fixture-transport"
        module_dir.mkdir()
        (module_dir / "sitecustomize.py").write_text(
            "from pathlib import Path\nimport urllib.request\n"
            "def fixture(self, url, **kwargs):\n"
            f"    assert url.startswith('https://github.com/{REPO}/releases/download/v{VERSION}/')\n"
            f"    return (Path({str(self.dist)!r}) / url.rsplit('/', 1)[-1]).open('rb')\n"
            "urllib.request.OpenerDirector.open = fixture\n")
        with patch.dict(os.environ, {"PYTHONPATH": str(module_dir)}):
            subprocess.run(["sh", bound, "--no-service", "--version", VERSION, "--prefix", self.prefix], check=True)
            self.assertEqual(json.loads(self.receipt.read_bytes())["sha256"], installer.digest(self.binary))
            subprocess.run(["sh", bound, "--uninstall", "--prefix", self.prefix], check=True)
        with patch.dict(os.environ, {"GITHUB_REPOSITORY": "other/repo"}):
            self.assertNotEqual(subprocess.run(cmd, capture_output=True).returncode, 0)
        unbound = subprocess.run(["sh", ROOT / "install.sh", "--no-service", "--prefix", self.prefix], capture_output=True)
        self.assertNotEqual(unbound.returncode, 0)
        self.assertFalse(self.binary.exists())


if __name__ == "__main__":
    unittest.main()
