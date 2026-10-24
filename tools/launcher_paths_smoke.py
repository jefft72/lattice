#!/usr/bin/env python3
"""Compare the real launcher and binary from an unrelated directory with spaces.

Run after cargo build --release. Uses the caller's existing Cargo toolchain and
only disposable files/PTYs. Covers every user-facing relative file path.
"""
import argparse
import fcntl
import os
from pathlib import Path
import pty
import struct
import subprocess
import tempfile
import termios
import time

from pty_smoke import drain


def run(executable, directory, *arguments):
    result = subprocess.run(
        [str(executable), *arguments], cwd=directory,
        capture_output=True, text=True, timeout=60,
    )
    assert result.returncode == 0, (arguments, result.stdout, result.stderr)
    return result.stdout


def record(executable, directory, relative_root):
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 44, 144, 0, 0))
    process = subprocess.Popen(
        [str(executable), "--demo", "--interval", "0.5", "--record", "Caller paths",
         "--record-every", "1", "--record-dir", relative_root],
        cwd=directory, stdin=slave, stdout=slave, stderr=slave,
        env={**os.environ, "TERM": "xterm-256color"},
    )
    output = bytearray()
    try:
        deadline = time.monotonic() + 60
        paths = []
        while time.monotonic() < deadline:
            output.extend(drain(master, 0.1))
            assert process.poll() is None, output.decode(errors="replace")
            paths = list((directory / relative_root).rglob("*.lattice"))
            if paths and b"\nS\t" in paths[0].read_bytes():
                break
        assert len(paths) == 1, (paths, output.decode(errors="replace"))
        assert termios.tcgetattr(slave) != before, "recording did not enter raw mode"
        os.write(master, b"q")
        deadline = time.monotonic() + 10
        while process.poll() is None and time.monotonic() < deadline:
            output.extend(drain(master, 0.1))
        process.wait(timeout=1)
        output.extend(drain(master, 0.1))
        assert process.returncode == 0, output.decode(errors="replace")
        assert termios.tcgetattr(slave) == before, "recording leaked terminal flags"
        assert b"\x1b[?1049l" in output and b"\x1b[?1000l" in output
        assert paths[0].parent == directory / relative_root / "demo", paths
        assert paths[0].read_text().splitlines()[-1].endswith("\tapp_exit")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/lattice", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    project = Path(__file__).resolve().parents[1]
    launcher = project / "lattice"
    with tempfile.TemporaryDirectory(prefix="lattice caller paths ") as temporary:
        caller = Path(temporary) / "unrelated working directory with spaces"
        caller.mkdir()
        fixture = caller / "relative input.lattice"
        fixture.write_text(
            "LATTICE_RECORDING\t1\tCaller paths\tSIMULATED\t1700000000.000\t5.000\n"
            "S\t1\t1700000000.000\t0.000\t0\t20.000\t1000\t500\t0\t100\t1\t0\t0\t0\t42,1699999999,agent,100,50.000\n"
            "F\t1.000\tapp_exit\n"
        )
        outputs = {}
        for label, executable in [("direct", binary), ("launcher", launcher)]:
            snapshot = f"{label} relative snapshot.svg"
            run(executable, caller, "--demo", "--snapshot", snapshot)
            assert (caller / snapshot).exists(), f"{label}: relative snapshot escaped caller directory"
            outputs[label] = (caller / snapshot).read_bytes()
            report = run(executable, caller, "recordings", "show", fixture.name)
            outputs[f"{label} review"] = report
            csv_name = f"{label} relative export.csv"
            run(executable, caller, "recordings", "export", fixture.name, "--output", csv_name)
            outputs[f"{label} CSV"] = (caller / csv_name).read_bytes()
            # Populate a storage tree with an identical fixture for list comparison.
            storage = caller / f"{label} relative storage"
            (storage / "demo").mkdir(parents=True)
            (storage / "demo" / fixture.name).write_bytes(fixture.read_bytes())
            listing = run(executable, caller, "recordings", "list", "--record-dir", storage.name)
            assert fixture.name in listing, listing
            record(executable, caller, f"{label} relative recording")
        assert outputs["direct"] == outputs["launcher"], "launcher changed snapshot output"
        assert outputs["direct review"] == outputs["launcher review"], "launcher changed recording review"
        assert outputs["direct CSV"] == outputs["launcher CSV"], "launcher changed CSV export"
    print("PASS real Cargo launcher preserves caller CWD for snapshots, recording input, list/storage, recording creation and export paths with spaces")


if __name__ == "__main__":
    main()
