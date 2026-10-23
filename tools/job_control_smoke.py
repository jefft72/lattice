#!/usr/bin/env python3
"""Exercise real Unix shell job control and terminal restoration in a PTY.

The interactive parent shell owns the controlling terminal and puts lattice in
its own foreground process group. Check the shell's stty state while lattice is
stopped, not only after exit. No third-party Python packages required.
"""
import argparse
import fcntl
import os
import pty
import re
import shlex
import signal
import struct
import subprocess
import termios
import time
from pathlib import Path

from pty_smoke import drain


PROMPT = b"__lattice_job_control_shell__> "
ENTER = b"\x1b[?1049h"
LEAVE = b"\x1b[?1049l"
MOUSE_ON = b"\x1b[?1000h"
MOUSE_OFF = b"\x1b[?1000l"
CURSOR_ON = b"\x1b[?25h"


class Shell:
    def __init__(self):
        self.app_pid = None
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvpe("bash", ["bash", "--noprofile", "--norc", "-i"], {
                **os.environ,
                "TERM": "xterm-256color",
                "PS1": PROMPT.decode(),
                "PS2": "",
                "PROMPT_COMMAND": "",
                "HISTFILE": "/dev/null",
                "INPUTRC": "/dev/null",
            })
        self.resize(44, 144)
        self.until(PROMPT)
        self.original = self.settings()

    def resize(self, rows, columns):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ,
                    struct.pack("HHHH", rows, columns, 0, 0))

    def send(self, data):
        os.write(self.fd, data)

    def until(self, marker, timeout=4):
        output = bytearray()
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            output.extend(drain(self.fd, 0.05))
            if marker in output:
                return bytes(output)
        raise AssertionError(f"timed out waiting for {marker!r}: {bytes(output)!r}")

    def command(self, command):
        self.send(command.encode() + b"\n")
        return self.until(PROMPT)

    def settings(self):
        output = self.command("printf '\\n__TTY_BEGIN__\\n'; stty -g; printf '__TTY_END__\\n'")
        match = re.search(rb"__TTY_BEGIN__\r?\n([^\r\n]+)\r?\n__TTY_END__", output)
        assert match, f"could not read parent shell terminal state: {output!r}"
        return match[1]

    def assert_restored(self, output):
        assert LEAVE in output, "alternate screen not left before shell resumed"
        assert MOUSE_OFF in output, "mouse capture not disabled before shell resumed"
        assert CURSOR_ON in output, "cursor not shown before shell resumed"
        assert os.tcgetpgrp(self.fd) == self.pid, "shell did not regain foreground terminal"
        assert self.settings() == self.original, "parent shell terminal settings changed"

    def assert_running(self, output):
        assert ENTER in output, "alternate screen not entered"
        assert MOUSE_ON in output, "mouse capture not enabled"
        assert not termios.tcgetattr(self.fd)[3] & (termios.ICANON | termios.ECHO), "raw mode not entered"
        assert os.tcgetpgrp(self.fd) == self.app_pid, "lattice not in foreground"

    def launch(self, binary):
        # The small exec wrapper reports the PID without starting lattice as a
        # background job (which could race terminal initialization with fg).
        wrapper = 'printf "__LATTICE_PID=%s\\n" "$$"; exec "$@"'
        self.send((f"sh -c {shlex.quote(wrapper)} lattice {shlex.quote(str(binary))} "
                   "--demo --interval 30\n").encode())
        output = self.until(ENTER)
        output += drain(self.fd, 0.2)
        match = re.search(rb"__LATTICE_PID=(\d+)\r?\n", output)
        assert match, f"child PID missing: {output!r}"
        self.app_pid = int(match[1])
        self.assert_running(output)
        # An unchanged, paused frame must still be fully repainted after fg.
        self.send(b" ")
        self.until(b"PAUSED")

    def cycle(self, trigger, index):
        drain(self.fd, 0.1)
        if isinstance(trigger, bytes):
            self.send(trigger)
        else:
            os.kill(self.app_pid, trigger)
        output = self.until(PROMPT)
        self.assert_restored(output)
        jobs = self.command("jobs -s -p")
        assert re.search(rb"[\r\n]" + str(self.app_pid).encode() + rb"\r?\n", jobs), \
            f"lattice is not a stopped shell job: {jobs!r}"
        if index == 1:
            self.resize(24, 80)
        self.send(b"fg\n")  # The shell sends SIGCONT and restores foreground ownership.
        output = self.until(b"PAUSED")
        output += drain(self.fd, 0.1)
        self.assert_running(output)
        assert b"lattice" in output, "unchanged header was not fully redrawn"

    def finish(self, stop):
        drain(self.fd, 0.1)
        if isinstance(stop, bytes):
            self.send(stop)
        else:
            os.kill(self.app_pid, stop)
        output = self.until(PROMPT)
        status = self.command("printf '__EXIT_STATUS=%s\\n' \"$?\"")
        assert b"__EXIT_STATUS=0\r\n" in status, f"unclean exit: {status!r}"
        self.assert_restored(output)
        self.app_pid = None

    def close(self):
        if self.app_pid is not None:
            try:
                os.kill(self.app_pid, signal.SIGKILL)  # Only our disposable child.
            except ProcessLookupError:
                pass
        try:
            os.kill(self.pid, signal.SIGKILL)  # Only our disposable shell.
        except ProcessLookupError:
            pass
        os.waitpid(self.pid, 0)
        os.close(self.fd)


def run_case(binary, name, stop):
    shell = Shell()
    try:
        shell.launch(binary)
        for index, trigger in enumerate([signal.SIGTSTP, b"\x1a", signal.SIGTSTP]):
            shell.cycle(trigger, index)
        shell.finish(stop)
        print(f"PASS {name}: three suspend/resume cycles, parent shell state, resize, full redraw, clean exit")
    finally:
        shell.close()


def check_stopped_terminal(binary):
    # Shells can repair termios themselves when reclaiming a stopped job. Inspect
    # the slave directly as well, before any shell could mask missing app cleanup.
    master, slave = pty.openpty()
    original = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    process = subprocess.Popen(
        [str(binary), "--demo"], stdin=slave, stdout=slave, stderr=slave,
        env={**os.environ, "TERM": "xterm-256color"},
    )
    try:
        drain(master, 0.5)
        for index in range(3):
            assert process.poll() is None, "child exited unexpectedly"
            assert termios.tcgetattr(slave) != original, "raw mode not entered"
            process.send_signal(signal.SIGTSTP)
            end = time.monotonic() + 3
            while True:
                pid, status = os.waitpid(process.pid, os.WNOHANG | os.WUNTRACED)
                if pid:
                    assert os.WIFSTOPPED(status), f"child did not stop: {status}"
                    break
                assert time.monotonic() < end, "child did not suspend"
                time.sleep(0.01)
            output = drain(master, 0.1)
            assert termios.tcgetattr(slave) == original, "terminal still raw while process is stopped"
            assert LEAVE in output and MOUSE_OFF in output and CURSOR_ON in output, \
                "screen, mouse, or cursor not restored before process stopped"
            if index == 2:
                # A pending shutdown must not be lost when SIGCONT arrives.
                process.send_signal(signal.SIGQUIT)
            process.send_signal(signal.SIGCONT)
            output = drain(master, 0.3)
            if index < 2:
                assert ENTER in output and MOUSE_ON in output, "terminal not re-entered on SIGCONT"
            else:
                process.wait(timeout=3)
                assert process.returncode == 0, "pending SIGQUIT did not exit cleanly"
                assert termios.tcgetattr(slave) == original, "terminal not restored on pending SIGQUIT"
        print("PASS direct PTY: terminal restored at actual stop, repeated SIGCONT, pending SIGQUIT cleanup")
    finally:
        if process.poll() is None:
            process.kill()  # Only our disposable child, even if it is still stopped.
            process.wait()
        os.close(master)
        os.close(slave)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/lattice", type=Path)
    binary = parser.parse_args().binary.resolve(strict=True)
    check_stopped_terminal(binary)
    for name, stop in [("q after resume", b"q"), ("Ctrl-C after resume", b"\x03"),
                       ("SIGQUIT after resume", signal.SIGQUIT)]:
        run_case(binary, name, stop)


if __name__ == "__main__":
    main()
