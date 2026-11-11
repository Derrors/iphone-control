"""Signed offline install fixture. The service manager, HOME, and network are isolated."""
import contextlib
import fcntl
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
                            'case "$0 $*" in\n'
                            '  *"launchctl print "*) echo "Could not find service" >&2; exit 1;;\n'
                            '  *" -p LoadState -p ActiveState") echo LoadState=not-found; echo ActiveState=inactive;;\n'
                            '  *" --user show iphone-control "*) echo "FragmentPath=$XDG_CONFIG_HOME/systemd/user/iphone-control.service"; echo DropInPaths=;;\n'
                            '  *pgrep*|*" --user cat "*) exit 1;;\nesac\n')
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
            exe = (b'#!/bin/sh\nif [ "$1" = --version ]; then echo iphone-control 0.2.1; exit; fi\n'
                   b'echo "$*" >> "$INSTALL_LOG"\n'
                   b'job="$XDG_CONFIG_HOME/iphone-control/update-job.sh"\n'
                   b'case "$*" in "update schedule"|"update policy notify")\n'
                   b'  printf "#!/bin/sh\\n# iphone-control update job\\nexe=\'%s\'\\n" "$0" > "$job";;\n'
                   b'  "update policy manual") rm -f "$job";;\nesac\n'
                   b'if [ "$*" = "update check --json" ]; then\n'
                   b'  printf \'{"availability":"%s","install_method":"managed","guidance":"checked"}\\n\' '
                   b'"${FIXTURE_AVAILABILITY:-available}"\nfi\n')
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

    def test_managed_reinstall_current_ahead_and_pinned(self):
        self.install()
        for status in ("current", "ahead"):
            with patch.dict(os.environ, FIXTURE_AVAILABILITY=status):
                self.install()
        self.assertNotIn("update apply", self.log.read_text())
        self.install("--uninstall")
        self.install("--version", VERSION)
        self.log.write_text("")
        self.install()
        self.assertTrue(json.loads(self.receipt.read_bytes())["pinned"])
        self.assertNotIn("update check", self.log.read_text())
        self.assertNotIn("update apply", self.log.read_text())

    def test_removed_daemon_reinstall_and_uninstall(self):
        with patch.dict(os.environ, FIXTURE_AVAILABILITY="current"):
            self.install()
            _, service = installer.service_files(self.config)
            service.unlink()
            self.install()
            self.assertTrue(service.is_file())
            service.unlink()
            self.log.write_text("")
            self.install("--uninstall")
        self.assertFalse(self.receipt.exists())
        self.assertFalse(self.binary.exists())
        self.assertIn("update policy manual", self.log.read_text())
        self.assertNotIn("bootout", self.log.read_text())
        self.assertNotIn("disable --now iphone-control", self.log.read_text())

    def test_replaced_service_is_left_alone(self):
        self.install()
        _, service = installer.service_files(self.config)
        service.unlink()
        other = self.root / "foreign service"
        other.write_text("a different service")
        service.symlink_to(other)
        self.log.write_text("")
        for args in ((), ("--uninstall",)):
            with self.assertRaisesRegex(ValueError, "no longer matches"):
                self.install(*args)
        self.assertEqual(self.log.read_text(), "")
        self.assertTrue(service.is_symlink())
        self.assertTrue(self.binary.is_file())

    def test_manual_daemon_removes_only_its_own_schedule_and_policy(self):
        self.install("--no-service")
        subprocess.run([self.binary, "update", "policy", "notify"], check=True)
        job = self.config / "update-job.sh"
        self.assertTrue(job.exists())
        key = hashlib.sha256(os.fsencode(self.binary)).hexdigest()
        policy = self.config / "updates" / REPO / (key + ".json")
        policy.parent.mkdir(parents=True)
        policy.write_text('"notify"')
        other = policy.parent / "other.json"
        other.write_text('"manual"')
        self.install("--uninstall")
        self.assertFalse(policy.exists())
        self.assertFalse(job.exists())
        self.assertTrue(other.exists())
        self.install("--no-service")
        contents = "#!/bin/sh\n# iphone-control update job\nexe=/other/iphone-control\n"
        job.write_text(contents)
        self.log.write_text("")
        self.install("--uninstall")
        self.assertEqual(job.read_text(), contents)
        self.assertNotIn("update policy manual", self.log.read_text())

    def test_uninstall_serializes_with_schedule_reenable(self):
        self.install()
        _, service = installer.service_files(self.config)
        run = subprocess.run

        def reenable_after_manual(args, **kwargs):
            if args == [self.binary, "update", "policy", "manual"]:
                with (self.config / "update-schedule.lock").open("a+b") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    result = run(args, **kwargs)
                run([self.binary, "update", "policy", "notify"], check=True)
                return result
            return run(args, **kwargs)

        with patch.object(subprocess, "run", reenable_after_manual):
            with self.assertRaisesRegex(ValueError, "re-enabled"):
                self.install("--uninstall")
        self.assertTrue(installer.owns_update_job(self.binary, self.config))
        self.assertTrue(service.exists() and self.binary.exists() and self.receipt.exists())

        unlink = Path.unlink
        def check_lock_on_delete(path, *args, **kwargs):
            if path in (self.binary, self.receipt):
                with (self.config / "update-schedule.lock").open("rb+") as lock:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return unlink(path, *args, **kwargs)

        with patch.object(Path, "unlink", check_lock_on_delete):
            self.install("--uninstall")
        self.assertFalse(self.binary.exists() or self.receipt.exists() or (self.config / "update-job.sh").exists())

    def test_cli_recreated_service_must_point_to_this_binary(self):
        binary = self.root / "bin/iphone-control"
        path = self.root / "daemon-service"
        script = f'set -a; f="$HOME/.config/iphone-control/env"; [ -f "$f" ] && . "$f"; set +a; exec "{binary}" daemon'
        path.write_bytes(plistlib.dumps(dict(Label=installer.LABEL, ProgramArguments=["/bin/sh", "-c", script])))
        self.assertTrue(installer.owns_service(path, binary, "launchd"))
        self.assertFalse(installer.owns_service(path, self.binary, "launchd"))
        path.write_text(f"# Written by iphone-control. Settings: ~/.config/iphone-control/env\n"
                        f"[Service]\nExecStart={binary} daemon\n")
        self.assertTrue(installer.owns_service(path, binary, "systemd"))
        self.assertFalse(installer.owns_service(path, self.binary, "systemd"))

    def test_uninstall_removes_verified_update_files(self):
        self.install()
        receipt = json.loads(self.receipt.read_bytes())
        state = self.binary.parent / ".iphone-control-update"
        state.mkdir(mode=0o700)
        backup = state / "previous"
        backup.write_bytes(b"previous verified executable")
        os.link(backup, state / "recovery")
        old = dict(receipt, sha256=installer.digest(backup), version="0.2.0")
        (state / "previous.json").write_text(json.dumps(dict(before_digest=receipt["sha256"], receipt=old)))
        (state / "outcome.json").write_text(json.dumps(dict(
            status="manual_migration", target_version=VERSION, detail="operator needed",
            suppressed_version=VERSION, migration_source="0.2.0")))
        stage = state / "stage"
        stage.mkdir(mode=0o700)
        (stage / "archive").write_bytes(b"interrupted archive")
        (state / "personal.bak").write_bytes(b"user file")
        other = self.root / "user notes"
        other.write_text("keep")
        (state / "notes").symlink_to(other)
        (state / "swap-link").symlink_to(other)
        self.install("--uninstall")
        self.assertEqual({p.name for p in state.iterdir()}, {"notes", "personal.bak", "swap-link"})
        self.assertEqual(other.read_text(), "keep")

    def test_rollback_source_is_signed_and_precedes_release(self):
        command = [sys.executable, ROOT / "packaging/release.py", "sign", self.dist, self.target]
        with patch.dict(os.environ, ICTL_ROLLBACK_COMPATIBLE_TAG="v" + VERSION):
            for source in ("", VERSION, "0.2.2"):
                with self.subTest(source=source), patch.dict(os.environ, ICTL_ROLLBACK_FROM_VERSION=source):
                    self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)
            with patch.dict(os.environ, ICTL_ROLLBACK_FROM_VERSION="0.2.0"):
                self.sign()
        data = installer.verify_manifest(self.root, self.manifest, self.sig, REPO, installer.PUBLIC_KEYS,
                                         VERSION, self.target)
        self.assertTrue(data["rollback_compatible"])
        self.assertEqual(data["rollback_from"], "0.2.0")

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
