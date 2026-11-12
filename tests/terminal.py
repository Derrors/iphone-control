#!/usr/bin/env python3
"""Offline menu regression on Linux/macOS; only Python's standard library.

python3 tests/terminal.py --binary target/debug/iphone-control \
    --test-dir target/debug/deps

Run cargo test --workspace --all-features --locked first (or use --no-run).
--test-dir discovers the newest executable that lists the menu test with --list;
--test-binary can instead select an exact test executable for local debugging.
CLI cases cancel before service/device work. All settings use temporary XDG dirs.
"""
import argparse
import errno
import fcntl
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time


class Session:
    def __init__(self, command, env, size=(24, 80), redirect=False):
        self.master, self.slave = pty.openpty()
        self.resize(*size)
        self.original = termios.tcgetattr(self.slave)
        self.output = b""
        self.proc = subprocess.Popen(
            command, stdin=self.slave,
            stdout=subprocess.PIPE if redirect else self.slave,
            stderr=self.slave, env=env,
        )
        self.redirect = redirect
        self.reader = self.proc.stdout.fileno() if redirect else self.master

    def resize(self, rows, columns):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))

    def pump(self, timeout=0.05):
        if select.select([self.reader], [], [], timeout)[0]:
            try:
                self.output += os.read(self.reader, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise

    def expect(self, text):
        target = text.encode()
        deadline = time.monotonic() + 8
        while target not in self.output and time.monotonic() < deadline:
            self.pump()
            if self.proc.poll() is not None:
                self.pump(0)
                break
        assert target in self.output, (text, self.output.decode(errors="replace"))

    def send(self, data, raw=True):
        if raw:
            deadline = time.monotonic() + 8
            while termios.tcgetattr(self.slave)[3] & termios.ICANON:
                assert time.monotonic() < deadline, "menu did not start reading keys"
                self.pump(0.01)
        os.write(self.master, data)

    def finish(self, result):
        self.expect(result)
        assert self.proc.wait(timeout=8) == 0, self.output.decode(errors="replace")
        assert termios.tcgetattr(self.slave) == self.original, "terminal flags were not restored"
        assert b"\x1b[?25l" not in self.output, "menu unexpectedly hid cursor"

    def close(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        if self.proc.stdout:
            self.proc.stdout.close()
        os.close(self.master)
        os.close(self.slave)


def exercise(command, env, keys, result, ready="Esc", size=(24, 80), redirect=False):
    session = Session(command, env, size, redirect)
    try:
        session.expect(ready)
        session.send(keys, raw=ready != "Choice:")
        session.finish(result)
        return session.output
    finally:
        session.close()


MENU_TEST = "guide::terminal::tests::pty_menu"


def discover_test_binary(directory):
    candidates = sorted(
        (path for path in directory.glob("iphone_control-*")
         if path.is_file() and os.access(path, os.X_OK)),
        key=lambda path: path.stat().st_mtime_ns, reverse=True,
    )
    for candidate in candidates:
        # Cargo also puts ordinary binaries in deps. Never invoke one without
        # arguments: --list safely fails clap parsing instead of starting setup.
        try:
            listed = subprocess.run(
                [str(candidate.resolve()), "--list"], stdin=subprocess.DEVNULL,
                capture_output=True, timeout=8,
            )
        except (OSError, subprocess.TimeoutExpired):
            continue
        if listed.returncode == 0 and f"{MENU_TEST}: test".encode() in listed.stdout.splitlines():
            return candidate
    raise ValueError(f"no executable listing {MENU_TEST} in {directory}; run cargo test --workspace --all-features --locked --no-run")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    tests = parser.add_mutually_exclusive_group()
    tests.add_argument("--test-binary", type=Path)
    tests.add_argument("--test-dir", type=Path)
    args = parser.parse_args()
    if args.test_dir:
        try:
            args.test_binary = discover_test_binary(args.test_dir)
        except ValueError as error:
            parser.error(str(error))
        print(f"Menu test executable: {args.test_binary}")
    if not args.binary and not args.test_binary:
        parser.error("provide --binary and/or --test-dir (or --test-binary)")
    with tempfile.TemporaryDirectory(prefix="ictl-terminal-") as directory:
        root = Path(directory)
        env = dict(os.environ, XDG_CONFIG_HOME=str(root / "config"),
                   XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"),
                   IPHONE_CONTROL_LANG="en", TERM="xterm-256color", NO_COLOR="1")
        if args.binary:
            command = [str(args.binary.resolve()), "--data-dir", str(root / "data"),
                       "--socket", str(root / "daemon.sock"), "setup", "--reconfigure"]
            for key in [b"\x1b", b"\x03"]:
                output = exercise(command, env, key, "Saved settings and completed steps are kept")
                assert b"Done." not in output
            for keys in [b"\x1b[B\r", b"2\r"]:
                session = Session(command, env)
                try:
                    session.expect("What do you want to set up again?")
                    session.send(keys)
                    session.expect("界面语言 / Language")
                    session.send(b"\x1b")
                    session.finish("Saved settings and completed steps are kept")
                finally:
                    session.close()
            # Language and proxy are saved together before daemon restart.
            # Cancelling proxy must leave both settings and setup-done untouched.
            settings = root / "config" / "iphone-control" / "env"
            settings.parent.mkdir(parents=True, exist_ok=True)
            settings.write_text("IPHONE_CONTROL_LANG=zh\n")
            before = settings.read_bytes()
            session = Session(command, env)
            try:
                session.expect("What do you want to set up again?")
                session.send(b"1\r")
                session.expect("界面语言 / Language")
                session.send(b"2\r")
                session.expect("Use a proxy?")
                session.send(b"\x1b")
                session.finish("Saved settings and completed steps are kept")
            finally:
                session.close()
            assert settings.read_bytes() == before
            assert not settings.with_name("setup-done").exists()
            exercise(command, env, b"\x1b", "Saved settings and completed steps are kept")
            assert settings.read_bytes() == before
            completed = subprocess.run(command, input=b"2\n", capture_output=True, env=env, timeout=8)
            assert completed.returncode != 0
            assert b"Setup is interactive" in completed.stderr + completed.stdout
            print("PASS CLI cancellation, arrows, numbers, saved-step persistence, non-TTY restriction")
        if args.test_binary:
            command = [str(args.test_binary.resolve()), "--ignored", "--exact",
                       MENU_TEST, "--nocapture", "--test-threads=1"]
            for keys, result in [(b"\r", 1), (b"\x1b[A\r", 0), (b"\x1b[B\r", 2),
                                 (b"12\r", 11), (b"99\x7f\r", 8), (b"\x1b[F\r", 29)]:
                exercise(command, env, keys, f"MENU_RESULT={result}")
            for key in [b"\x1b", b"\x03"]:
                exercise(command, env, key, "MENU_CANCELLED")
            confirm_env = dict(env, ICTL_TEST_MENU="confirm")
            for keys, result in [(b"\r", 0), (b"\x1b[D\r", 1), (b"\x1b[D\x1b[C\r", 0),
                                 (b"1\r", 1), (b"y\r", 1)]:
                exercise(command, confirm_env, keys, f"MENU_RESULT={result}")
            for fallback_env, size, redirect in [(dict(env, TERM="dumb"), (24, 80), False),
                                                  (env, (5, 30), False), (env, (24, 80), True)]:
                output = exercise(command, fallback_env, b"12\r", "MENU_RESULT=11",
                                  ready="Choice:", size=size, redirect=redirect)
                assert b"\x1b[" not in output
            completed = subprocess.run(command, input=b"12\n", capture_output=True, env=env, timeout=8)
            assert completed.returncode == 0
            assert b"MENU_RESULT=11" in completed.stdout
            completed = subprocess.run(command, input=b"invalid\nyes\n", capture_output=True,
                                       env=confirm_env, timeout=8)
            assert completed.returncode == 0
            assert b"MENU_RESULT=1" in completed.stdout
            session = Session(command, env)
            try:
                session.expect("Menu test")
                session.resize(8, 40)
                session.send(b"\x1b[F")
                session.expect("> 30)")
                session.send(b"\r")
                session.finish("MENU_RESULT=29")
            finally:
                session.close()
            yes_env = dict(confirm_env, ICTL_TEST_DEFAULT="yes")
            for text in ["否", "不"]:
                exercise(command, yes_env, (text + "\r").encode(), "MENU_RESULT=0")
            # Send one byte at a time, well apart: a single write is not a
            # fragmented-input test. Every fragment stays within the 250ms budget.
            for fragmented_env, keys, result in [(env, b"\x1b[B", 2),
                                                  (env, b"\x1bOB", 2),
                                                  (yes_env, "否".encode(), 0),
                                                  (yes_env, "不".encode(), 0)]:
                session = Session(command, fragmented_env)
                try:
                    session.expect("Esc")
                    for byte in keys:
                        session.send(bytes([byte]))
                        time.sleep(0.04)
                    session.send(b"\r")
                    session.finish(f"MENU_RESULT={result}")
                finally:
                    session.close()
            # A partial control sequence that times out is ignored, not Esc.
            session = Session(command, env)
            try:
                session.expect("Esc")
                session.send(b"\x1b[")
                time.sleep(0.35)
                session.pump(0.05)
                assert session.proc.poll() is None
                assert b"MENU_CANCELLED" not in session.output
                session.send(b"\x03")
                session.finish("MENU_CANCELLED")
            finally:
                session.close()
            for invalid_env in [env, yes_env]:
                session = Session(command, invalid_env)
                try:
                    session.expect("Esc")
                    session.send(b"1x\r")
                    session.expect("Invalid input")
                    session.pump(0.1)
                    assert b"MENU_RESULT" not in session.output
                    session.resize(4, 20)
                    session.expect("Enlarge window")
                    session.send(b"\r")
                    session.pump(0.15)
                    assert b"MENU_RESULT" not in session.output
                    session.resize(24, 80)
                    session.send(b"\r")
                    session.pump(0.15)
                    assert b"MENU_RESULT" not in session.output
                    session.send(b"\x7f\r")
                    session.finish("MENU_RESULT=1" if invalid_env == yes_env else "MENU_RESULT=0")
                finally:
                    session.close()
            session = Session(command, dict(env, ICTL_TEST_MENU="text-then-menu"))
            try:
                session.expect("Text:")
                session.send(b"pasted text\n12\n", raw=False)
                session.expect("TEXT_RESULT=pasted text")
                session.finish("MENU_RESULT=11")
            finally:
                session.close()
            session = Session(command, env)
            try:
                session.expect("Esc")
                session.send(b"1" + b" " * 300 + b"\r")
                session.expect("Input too long")
                assert b"MENU_RESULT" not in session.output
                session.send(b"\x1b[B\r")
                session.finish("MENU_RESULT=1")
            finally:
                session.close()
            for data in [b"", b"invalid", b"x" * 4097 + b"\n"]:
                completed = subprocess.run(command, input=data, capture_output=True, env=env, timeout=8)
                assert completed.returncode != 0
                assert b"MENU_RESULT" not in completed.stdout
            print("PASS menu/confirmation keys, Chinese No with default Yes, invalid/oversized input, fragmented ESC/UTF-8, text-to-menu paste, resize, termios, fallback and EOF")


if __name__ == "__main__":
    main()
