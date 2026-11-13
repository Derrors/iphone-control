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
import signal
import struct
import subprocess
import sys
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

    def wait_for_raw(self):
        deadline = time.monotonic() + 8
        while termios.tcgetattr(self.slave)[3] & termios.ICANON:
            assert time.monotonic() < deadline, "menu did not start reading keys"
            self.pump(0.01)

    def send(self, data, raw=True):
        if raw:
            self.wait_for_raw()
        os.write(self.master, data)

    def assert_restored(self):
        actual = termios.tcgetattr(self.slave)
        expected = self.original.copy()
        if sys.platform == "darwin":
            # Darwin sets this transient kernel bit when canonical input resumes.
            actual[3] &= ~termios.PENDIN
            expected[3] &= ~termios.PENDIN
        assert actual == expected, ("terminal flags were not restored", actual, expected)
        assert b"\x1b[?25l" not in self.output, "menu unexpectedly hid cursor"

    def finish(self, result, status=0):
        self.expect(result + "\r\n" if result.startswith("MENU_RESULT=") and not self.redirect else result)
        assert self.proc.wait(timeout=8) == status, self.output.decode(errors="replace")
        self.assert_restored()

    def close(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()
        if self.proc.stdout:
            self.proc.stdout.close()
        os.close(self.master)
        os.close(self.slave)


def exercise(command, env, keys, result, ready="Esc", size=(24, 80), redirect=False, status=0):
    session = Session(command, env, size, redirect)
    try:
        session.expect(ready)
        session.send(keys, raw=ready != "Choice:")
        session.finish(result, status)
        return session.output
    finally:
        session.close()


def escape_with_separate_input_terminal(command, env):
    # The terminal that receives keys can differ from the one showing the menu.
    key_stream, key_tty = pty.openpty()
    display, screen_tty = pty.openpty()
    try:
        fcntl.ioctl(screen_tty, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        with subprocess.Popen(command, stdin=key_tty, stdout=screen_tty, stderr=screen_tty, env=env) as proc:
            try:
                output = b""
                while b"Ctrl+C cancel" not in output:
                    assert select.select([display], [], [], 5)[0], "split-terminal menu did not appear"
                    output += os.read(display, 8192)
                os.write(key_stream, b"\x1b")
                assert proc.wait(timeout=3) == 0
            finally:
                if proc.poll() is None:
                    proc.terminate()
    finally:
        for fd in (key_stream, key_tty, display, screen_tty):
            os.close(fd)


def held_arrow_during_redraw(command, env):
    session = Session(command, env)
    try:
        session.expect("Ctrl+C cancel")
        session.wait_for_raw()
        # Do not drain output between repeats. A slow renderer must keep echo
        # disabled even when the next arrow arrives during a frame write.
        for _ in range(50):
            os.write(session.master, b"\x1b[B")
            time.sleep(0.025)
            assert not termios.tcgetattr(session.slave)[3] & termios.ECHO, "TTY echo enabled during a held Down key"
        session.send(b"\x1b", raw=False)
        session.finish("Setup cancelled.")
        assert b"^[[B" not in session.output and b"\x1b[B" not in session.output, "Down was echoed into the menu"
        # Fifty Down presses wrap the nine-item menu five times and select item 6.
        assert session.output.rsplit(b"What do you want to set up again?", 1)[-1].count(b"> 6) Phone") == 1
        assert b"\n" not in session.output.replace(b"\r\n", b""), "a menu line did not return to column one"
    finally:
        session.close()


MENU_TEST = "guide::terminal::tests::pty_menu"


def discover_test_binary(directory, env):
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
                capture_output=True, timeout=8, env=env,
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
    if not (args.binary or args.test_binary or args.test_dir):
        parser.error("provide --binary and/or --test-dir (or --test-binary)")
    with tempfile.TemporaryDirectory(prefix="ictl-terminal-") as directory:
        root = Path(directory)
        env = dict(os.environ, HOME=str(root), XDG_CONFIG_HOME=str(root / "config"),
                   XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"),
                   XDG_RUNTIME_DIR=str(root / "runtime"),
                   IPHONE_CONTROL_LANG="en", TERM="xterm-256color", NO_COLOR="1")
        if args.test_dir:
            try:
                args.test_binary = discover_test_binary(args.test_dir, env)
            except ValueError as error:
                parser.error(str(error))
            print(f"Menu test executable: {args.test_binary}")
        if args.binary and os.geteuid() == 0:
            print("SKIP real CLI setup as root: it uses the system maintenance lock")
        elif args.binary:
            command = [str(args.binary.resolve()), "--data-dir", str(root / "data"),
                       "--socket", str(root / "daemon.sock"), "setup", "--reconfigure"]
            escape_with_separate_input_terminal(command, env)
            held_arrow_during_redraw(command, env)
            for key, status in [(b"\x1b", 0), (b"\x03", 130)]:
                output = exercise(command, env, key, "Saved settings and completed steps are kept", status=status)
                assert b"Done." not in output
            for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
                session = Session(command, env)
                try:
                    session.wait_for_raw()
                    session.proc.send_signal(sig)
                    assert session.proc.wait(timeout=8) in (-sig, 128 + sig)
                    session.assert_restored()
                finally:
                    session.close()
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
            marker = settings.with_name("setup-done")
            assert not marker.exists()
            marker.write_bytes(b"previously completed\n")
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
            assert marker.read_bytes() == b"previously completed\n"
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
                                 (b"12\r", 11), (b"12\x08\r", 0), (b"99\x7f\r", 8), (b"\x1b[F\r", 29)]:
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
            # An idle menu (no keys) must not repaint: the 100ms wakeups only
            # poll for a resize, so a steady frame produces zero extra output.
            session = Session(command, env)
            try:
                session.expect("Ctrl+C cancel\r\n")
                baseline = len(session.output)
                time.sleep(0.5)
                session.pump(0.15)
                assert len(session.output) == baseline, (
                    "idle menu repainted", session.output[baseline:].decode(errors="replace"))
                session.send(b"\r")
                session.finish("MENU_RESULT=1")
            finally:
                session.close()
            # Shrinking an active menu to 1-3 columns must not panic: fit()
            # once underflowed inside console::truncate_str below the "..." tail.
            for columns in (1, 2, 3):
                session = Session(command, env)
                try:
                    session.expect("Esc")
                    session.resize(24, columns)
                    # Several 100ms idle wakeups at the hostile width, no keys.
                    time.sleep(0.5)
                    session.pump(0.05)
                    assert session.proc.poll() is None, session.output.decode(errors="replace")
                    assert b"panicked" not in session.output
                    session.resize(24, 80)
                    session.send(b"\r")
                    session.finish("MENU_RESULT=1")
                finally:
                    session.close()
            yes_env = dict(confirm_env, ICTL_TEST_DEFAULT="yes")
            for text in ["否", "不"]:
                exercise(command, yes_env, (text + "\r").encode(), "MENU_RESULT=0")
            session = Session(command, yes_env)
            try:
                session.expect("Esc")
                answer = "否".encode()
                session.send(answer[:1])
                time.sleep(0.35)
                session.send(answer[1:] + b"\r")
                session.finish("MENU_RESULT=0")
            finally:
                session.close()
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
