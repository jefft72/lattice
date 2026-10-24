#!/usr/bin/env python3
"""Unix terminal lifecycle smoke checks. Run after cargo build --release.

Only drives the supplied lattice binary inside a disposable pseudo-terminal.
No third-party Python packages required. It never signals unrelated processes.
"""
import argparse
import fcntl
import os
import pty
import select
import signal
import struct
import subprocess
import termios
import time
from pathlib import Path


def drain(fd, duration=0.15):
    output = bytearray()
    end = time.monotonic() + duration
    while time.monotonic() < end:
        if select.select([fd], [], [], min(0.05, max(0, end-time.monotonic())))[0]:
            try:
                output.extend(os.read(fd, 65536))
            except OSError:
                break
    return bytes(output)


def run_case(binary, name, stop, live=False):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 44, 144, 0, 0))
    original = termios.tcgetattr(slave)
    process = subprocess.Popen(
        [str(binary), "--all" if live else "--demo", "--interval", "0.5"],
        stdin=slave, stdout=slave, stderr=slave,
        env={**os.environ, "TERM": "xterm-256color", "COLORTERM": "truecolor"},
    )
    output = bytearray()
    try:
        output.extend(drain(master, 0.8))
        assert process.poll() is None, f"{name}: exited at startup: {output!r}"
        assert termios.tcgetattr(slave) != original, f"{name}: raw mode not entered"
        # Navigation, repeated focus, sorting, pause/resume, help close and resize.
        for keys in [b"j", b"/codex", b"\r", b"\r", b"j", b"\x1b[D", b"\x1b[C", b"E", b"C", b"\t", b"\x1b", b"*", b"s", b" ", b" ", b"?", b"\x1b", b"/q", b"\x1b"]:
            os.write(master, keys)
            output.extend(drain(master, 0.06))
            assert process.poll() is None, f"{name}: unexpectedly exited during {keys!r}"
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 16, 40, 0, 0))
        process.send_signal(signal.SIGWINCH)
        output.extend(drain(master, 0.2))
        os.write(master,b" ")
        compact = drain(master,0.2)
        output.extend(compact)
        assert b"PAUSED" in compact, f"{name}: compact pause status missing"
        os.write(master,b" ")
        output.extend(drain(master,0.2))
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        process.send_signal(signal.SIGWINCH)
        output.extend(drain(master, 0.2))
        if isinstance(stop, bytes):
            os.write(master, stop)
        else:
            process.send_signal(stop)
        process.wait(timeout=3)
        output.extend(drain(master, 0.1))
        assert process.returncode == 0, f"{name}: exit code {process.returncode}"
        assert termios.tcgetattr(slave) == original, f"{name}: terminal flags not restored"
        assert b"\x1b[?1049l" in output, f"{name}: alternate screen not left"
        assert b"\x1b[?1000l" in output, f"{name}: mouse capture not disabled"
        print(f"PASS {name}: navigation, resize, clean exit, terminal + mouse restoration")
    finally:
        if process.poll() is None:
            process.kill()  # Only our disposable test child, never a monitored process.
            process.wait()
        os.close(master)
        os.close(slave)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/lattice", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    for name, stop in [("q", b"q"), ("Ctrl-C key", b"\x03"), ("SIGINT", signal.SIGINT),
                       ("SIGTERM", signal.SIGTERM), ("SIGHUP", signal.SIGHUP),
                       ("SIGQUIT", signal.SIGQUIT)]:
        run_case(binary, name, stop)
    run_case(binary, "live all-families", b"q", live=True)


if __name__ == "__main__":
    main()
