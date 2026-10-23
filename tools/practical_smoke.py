#!/usr/bin/env python3
"""Exercise practical features in real Unix PTYs, with only the Python stdlib.

Run after cargo build --release. Demo terminals run concurrently to keep the
normal test under 20 seconds. Live attachment targets only this script's own
disposable Python root/child/grandchild fixture, stopped cooperatively over
stdin. Failure cleanup may signal only processes created by this script.
"""

import argparse
import json
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import sys
import termios
import time

from onboarding_smoke import Terminal


class TerminalPump:
    """One cooperative reader drains every active PTY, preserving stream order.

    A focused key/read or a fixture wait must not leave the other demos blocked
    on a full output queue. Everything runs on the main thread, so screen
    snapshots and terminal closes cannot race a second reader.
    """

    def __init__(self):
        self.terminals = []

    def read(self, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            by_fd = {t.master: t for t in self.terminals if hasattr(t, "master")}
            timeout = min(0.02, max(0, deadline - time.monotonic()))
            ready = select.select(list(by_fd), [], [], timeout)[0]
            for fd in ready:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    # A terminated child can close its PTY while cleanup drains it.
                    continue
                by_fd[fd].output.extend(data)

    def wait(self, predicate, timeout, description):
        deadline = time.monotonic() + timeout
        while not predicate():
            if time.monotonic() >= deadline:
                raise TimeoutError(f"timed out waiting for {description}")
            self.read(min(0.02, max(0, deadline - time.monotonic())))

    def wait_exit(self, process, timeout):
        self.wait(lambda: process.poll() is not None, timeout, f"test child {process.pid}")


class PumpedTerminal(Terminal):
    """Reuse the screen decoder and assertions, but centralize every PTY read."""

    def __init__(self, pump, *args, **kwargs):
        self.pump = pump
        pump.terminals.append(self)
        try:
            super().__init__(*args, **kwargs)
        except BaseException:
            pump.terminals.remove(self)
            raise

    def read(self, seconds=0.2):
        self.pump.read(seconds)

    def close(self, success):
        try:
            if self.process.poll() is None:
                if success:
                    os.write(self.master, b"q")
                else:
                    self.process.send_signal(signal.SIGTERM)  # Only our test child.
                self.pump.wait_exit(self.process, 3)
        finally:
            try:
                # Preserve the inherited failure cleanup even if cooperative
                # waiting times out; do not leave children or descriptors behind.
                super().close(success and self.process.poll() is not None)
            finally:
                self.pump.terminals.remove(self)


TREE_PROGRAM = r"""
import json, os, subprocess, sys
depth = int(sys.argv[1])
child = None
pids = [os.getpid()]
try:
    if depth:
        child = subprocess.Popen(
            [sys.executable, '-u', '-c', sys.argv[2], str(depth - 1), sys.argv[2]],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        pids.extend(json.loads(child.stdout.readline()))
    print(json.dumps(pids), flush=True)
    sys.stdin.readline()
finally:
    if child:
        child.stdin.close()
        child.wait(timeout=3)
"""


class FixtureTree:
    """Three existing processes with arbitrary, non-agent executable names."""

    def __init__(self, pump):
        self.pump = pump
        self.process = subprocess.Popen(
            [sys.executable, "-u", "-c", TREE_PROGRAM, "2", TREE_PROGRAM],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
        )
        self.pids = []
        try:
            pump.wait(
                lambda: bool(select.select([self.process.stdout], [], [], 0)[0]),
                3, "fixture startup",
            )
            self.pids = json.loads(self.process.stdout.readline())
            assert len(self.pids) == len(set(self.pids)) == 3
            assert self.pids[0] == self.process.pid
            assert self.process.poll() is None
        except BaseException:
            self.close()
            raise

    def close(self):
        if self.process.stdin and not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.pump.wait_exit(self.process, 3)
        except TimeoutError:
            # Only this script's fixture. Closing its pipe also releases children.
            self.process.terminate()
            try:
                self.pump.wait_exit(self.process, 1)
            except TimeoutError:
                self.process.kill()
                self.process.wait()
        finally:
            if self.process.stdout:
                self.process.stdout.close()


def wait_for(terminal, predicate, description, timeout=2):
    deadline = time.monotonic() + timeout
    while True:
        screen = terminal.screen()
        if predicate(screen):
            return screen
        assert terminal.process.poll() is None, f"exited while waiting for {description}\n{screen}"
        assert time.monotonic() < deadline, f"timed out waiting for {description}\n{screen}"
        terminal.read(0.05)


def key(terminal, data):
    terminal.key(data, seconds=0.04)


def live_attachment(binary, pump):
    fixture = FixtureTree(pump)
    terminal = None
    success = False
    try:
        terminal = PumpedTerminal(
            pump, binary, 144, 44, tour=False, interval="0.5",
            extra_args=["--pid", str(fixture.pids[0])],
        )
        terminal.expect("LIVE")
        terminal.expect("PID FAMILY")
        screen = terminal.screen()
        assert re.search(r"SESSIONS\s+01\b", screen), screen
        terminal.expect("3 processes")
        assert f"#{os.getpid()}" not in screen, "fixture's unrelated parent was included"
        key(terminal, b"\r")
        terminal.expect("PROCESS EXPLORER")
        for pid in fixture.pids:
            assert re.search(rf"\b{pid}\b", terminal.screen()), terminal.screen()
        key(terminal, b"a")
        terminal.read(0.55)  # Cross a refresh, not merely the immediate input frame.
        terminal.expect("PID FAMILY")
        assert re.search(r"SESSIONS\s+01\b", terminal.screen())
        terminal.expect("3 processes")
        assert "ALL FAMILIES" not in terminal.screen()

        fixture.close()
        assert fixture.process.returncode == 0
        wait_for(terminal, lambda s: "PID UNSEEN" in s, "root disappearance")
        terminal.expect("Root no longer observed")
        key(terminal, b"a")
        terminal.read(0.55)
        terminal.expect("PID UNSEEN")
        screen = terminal.screen()
        assert re.search(r"SESSIONS\s+00\b", screen), screen
        assert "PID FAMILY" not in screen and "ALL FAMILIES" not in screen
        assert not re.search(r"crashed|killed|out of memory", screen, re.I), screen
        success = True
        print("PASS live PID: owned root/child/grandchild only, locked scope, unseen stays detached")
    finally:
        try:
            if terminal is not None:
                terminal.close(success)  # q and restored termios/alternate screen/mouse.
        finally:
            fixture.close()


def missing_pid_before_raw_mode(binary, pump):
    terminal = PumpedTerminal(
        pump, binary, 80, 24, tour=False, interval="0.5",
        extra_args=["--pid", str(2**32 - 1)],
    )
    try:
        pump.wait_exit(terminal.process, 3)
        terminal.read(0.05)
        assert terminal.process.returncode != 0
        assert b"could not attach to PID 4294967295" in terminal.output, terminal.output
        assert termios.tcgetattr(terminal.slave) == terminal.original, "raw mode was not restored"
        assert b"\x1b[?1049h" not in terminal.output, "invalid PID entered alternate screen"
        assert b"\x1b[?1000h" not in terminal.output, "invalid PID enabled mouse capture"
        print("PASS missing maximum PID: clear error before raw mode or alternate screen")
    finally:
        if terminal.process.poll() is None:
            terminal.process.kill()  # Only this script's disposable lattice child.
            terminal.process.wait()
        os.close(terminal.master)
        os.close(terminal.slave)
        pump.terminals.remove(terminal)


def rename_and_search(terminal):
    terminal.expect("DEMO")
    key(terminal, b"r")
    terminal.expect("NAME SESSION")
    key(terminal, b"q/")
    terminal.expect("q▏")  # q is text; '/' is rejected by the safe-name alphabet.
    assert "q/" not in terminal.screen()
    key(terminal, b"\x1b")
    assert "NAME SESSION" not in terminal.screen()
    key(terminal, b"r")
    assert "q▏" not in terminal.screen(), "cancel committed the edit"
    key(terminal, b"qjob\r")
    assert "NAME SESSION" not in terminal.screen()
    terminal.expect("qjob")
    key(terminal, b"/qjob\r")
    terminal.expect("/ qjob")
    terminal.expect("#4100")
    assert "No filter matches" not in terminal.screen()
    assert "#4200" not in terminal.screen(), "alias search failed to filter other roots"
    key(terminal, b"\x1b")
    assert "/ qjob" not in terminal.screen()


def changes_interactions(terminal):
    key(terminal, b"e")
    terminal.expect("WHAT CHANGED")
    wait_for(
        terminal, lambda s: re.search(r"WHAT CHANGED · [1-9][0-9]* kept", s),
        "an observed demo change", timeout=7,
    )
    terminal.expect("Observed, not causes")
    # Every size must permit navigation and opening the event's live family.
    for data in (b"\x1b[F", b"\x1b[H", b"jk", b"\x1b[6~", b"\x1b[5~"):
        key(terminal, data)
        terminal.expect("WHAT CHANGED")
    key(terminal, b"\r")
    terminal.expect("PROCESS EXPLORER")
    assert "WHAT CHANGED" not in terminal.screen()
    key(terminal, b"\x1b")
    key(terminal, b"e")
    terminal.expect("WHAT CHANGED")
    key(terminal, b"e")
    assert "WHAT CHANGED" not in terminal.screen()
    terminal.expect("DEMO")
    print(
        f"PASS {terminal.width}x{terminal.height}: rename q/sanitization/cancel/save, "
        "alias search, changes navigation/inspect/close"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/lattice", type=Path)
    binary = parser.parse_args().binary.resolve(strict=True)
    started = time.monotonic()
    pump = TerminalPump()
    demos = []
    success = False
    try:
        # Concurrent simulated clocks avoid waiting separately for each event.
        for width, height in [(144, 44), (80, 24), (40, 16)]:
            demos.append(PumpedTerminal(pump, binary, width, height, tour=False, interval="0.5"))
        received = [len(terminal.output) for terminal in demos]
        live_attachment(binary, pump)
        assert all(len(t.output) > old for t, old in zip(demos, received)), \
            "an unfocused demo PTY was not drained during live attachment"
        print("PASS every background demo PTY kept draining during the live fixture")
        missing_pid_before_raw_mode(binary, pump)
        for terminal in demos:
            terminal.read(0.02)
            rename_and_search(terminal)
        for terminal in demos:
            terminal.read(0.02)
            changes_interactions(terminal)
        success = True
    finally:
        # Close every PTY even if another one fails its restoration assertions.
        failures = []
        for terminal in demos:
            try:
                terminal.close(success)
            except BaseException as error:
                failures.append(error)
        if failures and success:
            raise failures[0]
    print(f"PASS all practical PTY checks and q restoration ({time.monotonic() - started:.1f}s)")


if __name__ == "__main__":
    main()
