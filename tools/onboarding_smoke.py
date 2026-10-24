#!/usr/bin/env python3
"""Assert the rendered onboarding flows in real Unix PTYs, using only the stdlib.

The small screen decoder handles the cursor/erase sequences emitted by this UI;
it is not intended to be a general terminal emulator. Only disposable test
children receive signals. Run after cargo build --release.
"""
import argparse
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import termios
import time
import unicodedata


class Terminal:
    def __init__(self, binary, width, height, tour=True, no_motion=True, interval="30", extra_args=None, color=True):
        self.width, self.height = width, height
        self.master, self.slave = pty.openpty()
        self.original = termios.tcgetattr(self.slave)
        self.output = bytearray()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        env = {**os.environ, "TERM": "xterm-256color"}
        if color:
            env.pop("NO_COLOR", None)
            env["COLORTERM"] = "truecolor"
        else:
            env["NO_COLOR"] = "1"
            env.pop("COLORTERM", None)
        self.process = subprocess.Popen(
            [str(binary), *(extra_args if extra_args is not None else ["--tour" if tour else "--demo"]), "--interval", interval,
             *(["--no-motion"] if no_motion else [])],
            stdin=self.slave, stdout=self.slave, stderr=self.slave,
            env=env,
        )
        self.read(0.4)

    def read(self, seconds=0.2):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([self.master], [], [], 0.02)[0]:
                try:
                    self.output.extend(os.read(self.master, 65536))
                except OSError:
                    break

    def key(self, key, seconds=0.2):
        os.write(self.master, key)
        self.read(max(seconds, len(key) * 0.09))
        assert self.process.poll() is None, "unexpected exit during input"

    def resize(self, width, height, seconds=0.25):
        """Resize the same PTY, retaining its output and cleanup evidence."""
        self.width, self.height = width, height
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        self.process.send_signal(signal.SIGWINCH)  # Only this script's child.
        self.read(seconds)
        assert self.process.poll() is None, f"unexpected exit after resize to {width}x{height}"

    def screen(self, backgrounds=False):
        rows = [[" "] * self.width for _ in range(self.height)]
        colors = [[None] * self.width for _ in range(self.height)] if backgrounds else None
        background = None
        x = y = index = 0
        text = self.output.decode("utf-8", "replace")
        while index < len(text):
            char = text[index]
            if char == "\x1b":
                match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", text[index:])
                if match:
                    raw, _, command = match.groups()
                    nums = [int(v) if v else 0 for v in raw.lstrip("?").split(";")]
                    count = nums[0] or 1
                    if command in "Hf":
                        y = (nums[0] or 1) - 1
                        x = ((nums[1] if len(nums) > 1 else 1) or 1) - 1
                    elif command == "J" and nums[0] in (2, 3):
                        rows = [[" "] * self.width for _ in range(self.height)]
                        if backgrounds:
                            colors = [[background] * self.width for _ in range(self.height)]
                    elif command == "K" and 0 <= y < self.height:
                        for column in range(max(0, x), self.width):
                            rows[y][column] = " "
                            if backgrounds:
                                colors[y][column] = background
                    elif command == "m" and backgrounds:
                        cursor = 0
                        while cursor < len(nums):
                            value = nums[cursor]
                            if value in (0, 49):
                                background = None
                            elif 40 <= value <= 47 or 100 <= value <= 107:
                                background = (value,)
                            elif value in (38, 48) and cursor + 1 < len(nums):
                                size = 4 if nums[cursor + 1] == 2 else 2
                                if value == 48:
                                    background = tuple(nums[cursor + 1:cursor + size + 1])
                                cursor += size
                            cursor += 1
                    elif command == "G":
                        x = count - 1
                    elif command == "C":
                        x += count
                    elif command == "D":
                        x -= count
                    elif command == "A":
                        y -= count
                    elif command == "B":
                        y += count
                    index += len(match.group())
                    continue
                index += 2
                continue
            if char == "\r":
                x = 0
            elif char == "\n":
                y += 1
            elif char >= " ":
                if 0 <= y < self.height and 0 <= x < self.width:
                    rows[y][x] = char
                    if backgrounds:
                        colors[y][x] = background
                x += 0 if unicodedata.combining(char) else 2 if unicodedata.east_asian_width(char) in "WF" else 1
            index += 1
        screen = "\n".join("".join(row) for row in rows)
        return (screen, colors) if backgrounds else screen

    def expect(self, text):
        screen = self.screen()
        assert text in screen, f"expected {text!r}\n{screen}"

    def close(self, success):
        try:
            if self.process.poll() is None:
                if success:
                    os.write(self.master, b"q")
                    self.process.wait(timeout=3)
                else:
                    self.process.send_signal(signal.SIGTERM)
                    self.process.wait(timeout=3)
            self.read(0.1)
            assert termios.tcgetattr(self.slave) == self.original, "terminal state not restored"
            assert b"\x1b[?1049l" in self.output, "alternate screen not left"
            assert b"\x1b[?1000l" in self.output, "mouse capture not disabled"
            if success:
                assert self.process.returncode == 0
        finally:
            if self.process.poll() is None:
                self.process.kill()  # Only this script's disposable child.
                self.process.wait()
            os.close(self.master)
            os.close(self.slave)


def run(binary, width, height):
    terminal = Terminal(binary, width, height)
    success = False
    try:
        terminal.expect("DEMO TOUR")
        terminal.expect("1/3")
        terminal.key(b"\r")
        terminal.expect("2/3")
        terminal.key(b"\r")
        terminal.expect("3/3")
        terminal.key(b"\r")
        terminal.expect("DEMO")
        assert "DEMO TOUR" not in terminal.screen()
        terminal.key(b"/q")
        terminal.expect("/ q")  # q must be text while searching, not quit.
        terminal.key(b"\x1b")
        terminal.key(b"/zz-no-match\r")
        terminal.expect("No filter matches")
        terminal.key(b"\x1b")
        terminal.key(b"*")
        terminal.expect("* claude")
        terminal.expect("#4100")
        terminal.key(b"\r")
        terminal.expect("PROCESS EXPLORER")
        terminal.key(b"/zz-no-match\r")
        terminal.expect("No filter matches")
        terminal.key(b"\x1b")  # First Esc leaves inspector; second clears the filter.
        terminal.key(b"\x1b")
        terminal.key(b"\r")
        terminal.key(b"?")
        terminal.expect("FIELD GUIDE")
        terminal.expect("INSPECTOR")
        terminal.key(b"\x1b[F")  # End reaches the final guide content at every size.
        terminal.expect("--doctor")
        terminal.key(b"\x1b[H")
        terminal.expect("MOVE THROUGH YOUR WORK")
        terminal.key(b"\x1b")
        terminal.key(b" ")
        terminal.expect("PAUSED")
        terminal.key(b"a")
        terminal.expect("AGENTS → ALL")
        frozen = terminal.screen()
        terminal.read(1.1)
        assert terminal.screen() == frozen, "paused view changed without an input"
        terminal.key(b" ", 0.5)
        terminal.expect("ALL FAMILIES")
        assert "SYNCING" not in terminal.screen()
        terminal.expect("DEMO")
        terminal.expect("q quit")
        success = True
        print(f"PASS {width}x{height}: tour, search, pin/PID, inspector, guide, paused scope, resume")
    finally:
        terminal.close(success)


def motion_and_guide_cases(binary):
    for static in (False, True):
        terminal = Terminal(binary, 144, 44, tour=False, no_motion=static, interval="0.5")
        success = False
        try:
            marks = set()
            for _ in range(12):
                match = re.search("([●·]) DEMO", terminal.screen())
                assert match, terminal.screen()
                marks.add(match.group(1))
                terminal.read(0.15)
            # The v3 status indicator is stable in both modes. Motion belongs to
            # brief view transitions, not a continuously blinking sample marker.
            assert marks == {"●"}, marks
            terminal.key(b" ")
            frozen = terminal.screen()
            terminal.read(1.1)
            assert terminal.screen() == frozen
            terminal.key(b"a")
            terminal.expect("AGENTS → ALL")
            terminal.key(b" ", 0.4)
            terminal.expect("ALL FAMILIES")
            terminal.key(b" ")
            terminal.key(b"a")
            terminal.expect("ALL → AGENTS")
            frozen = terminal.screen()
            terminal.read(1.1)
            assert terminal.screen() == frozen
            success = True
            print(f"PASS stable sample indicator, no-motion={static}, pause freeze, reverse scope")
        finally:
            terminal.close(success)
    terminal = Terminal(binary, 80, 24)
    success = False
    try:
        terminal.key(b"\x1b")
        terminal.expect("DEMO")
        assert "LIVE" not in terminal.screen(), "skipping tour must not switch to live"
        terminal.key(b"?")
        terminal.key(b"\x1b[F")
        terminal.expect("--doctor")
        terminal.key(b"\x1b[5~")
        assert "--doctor" not in terminal.screen()
        terminal.key(b"\x1b[H")
        terminal.expect("FIELD GUIDE 1/")
        terminal.key(b"\x1b[<65;8;8M")
        terminal.expect("FIELD GUIDE 2/")
        terminal.key(b"\x1b[<64;8;8M")
        terminal.expect("FIELD GUIDE 1/")
        success = True
        print("PASS skipped tour stays demo; guide paging and mouse scrolling")
    finally:
        terminal.close(success)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/lattice", type=Path)
    binary = parser.parse_args().binary.resolve(strict=True)
    for width, height in [(144, 44), (80, 24), (58, 19), (40, 16)]:
        run(binary, width, height)
    motion_and_guide_cases(binary)


if __name__ == "__main__":
    main()
