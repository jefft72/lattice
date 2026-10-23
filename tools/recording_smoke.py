#!/usr/bin/env python3
"""Opt-in recorder workflows in disposable Unix PTYs, using only the stdlib.

Run after cargo build --release. All data stays in a temporary directory.
Signals are sent only to this script's lattice children, never monitored PIDs.
SIGKILL cannot restore a terminal; that test restores its disposable PTY itself.
"""
import argparse
import csv
import os
from pathlib import Path
import signal
import resource
import subprocess
import tempfile
import termios
import time
from unittest.mock import patch

from onboarding_smoke import Terminal


def command(binary, *args, ok=True):
    result = subprocess.run([str(binary), *map(str, args)], capture_output=True, text=True, timeout=15)
    assert result.returncode == 0 if ok else result.returncode != 0, (args, result.stdout, result.stderr)
    assert "\x1b" not in result.stdout + result.stderr, "CLI output must be terminal-safe"
    return result.stdout + result.stderr


def files(root):
    return sorted(p for p in root.rglob("*") if p.is_file())


def recorded_samples(path):
    # The format has one complete line per durable record. The implementation
    # schema is identified below; partial trailing lines are never counted.
    return sum(line.startswith("S\t") for line in path.read_text().splitlines())


def launch(binary, root, name=None, interval="0.5", live=False):
    args = [*(["--all"] if live else ["--demo"]), "--record-dir", str(root), "--record-every", "1"]
    if name is not None:
        args += ["--record", name]
    return Terminal(binary, 144, 44, tour=False, interval=interval, extra_args=args)


def no_implicit_recording(binary, root):
    terminal = launch(binary, root)
    success = False
    try:
        terminal.expect("DEMO")
        terminal.expect("R start local recording")
        terminal.read(1.1)
        assert not root.exists(), "normal startup created recording storage"
        terminal.key(b"R")
        terminal.expect("START SIMULATED RECORDING")
        terminal.key(b"CalHacks q")
        terminal.expect("CalHacks q")
        assert not root.exists(), "typing a name must not create recording storage"
        terminal.key(b"\x1b")
        assert not root.exists(), "cancel must not create recording storage"
        success = True
        print("PASS default startup and canceled name modal never write recording files; q is editable text")
    finally:
        terminal.close(success)


def named_recording(binary, root):
    terminal = launch(binary, root, "CalHacks")
    success = False
    path = None
    try:
        terminal.expect("SIM REC")
        terminal.expect("CalHacks")
        terminal.read(1.3)
        paths = files(root)
        assert len(paths) == 1, paths
        path = paths[0]
        assert path.parent.name == "demo", path
        count = recorded_samples(path)
        assert count >= 1, path.read_text()
        terminal.key(b" ")
        terminal.expect("PAUSED")
        terminal.expect("REC GAP")
        frozen = recorded_samples(path)
        terminal.read(1.3)
        assert recorded_samples(path) == frozen, "pause fabricated samples"
        terminal.key(b" ", 0.5)
        terminal.read(1.3)
        assert recorded_samples(path) > frozen, "resume did not record a new observed sample"
        terminal.key(b"R")
        terminal.expect("Recording saved")
        saved = path.read_bytes()
        terminal.read(1.3)
        assert path.read_bytes() == saved, "stop continued writing"
        # Starting again always creates a separate file, never appends silently.
        terminal.key(b"R")
        terminal.expect("START SIMULATED RECORDING")
        terminal.key(b"Second q\r", 0.5)
        terminal.expect("Second q")
        terminal.read(0.7)
        assert len(files(root)) == 2, files(root)
        success = True
    finally:
        terminal.close(success)
    assert path is not None
    listing = command(binary, "recordings", "list", "--record-dir", root)
    assert path.name in listing, listing
    report = command(binary, "recordings", "show", path)
    assert "SIMULATED" in report.upper(), report
    assert "gap" in report.lower(), report
    assert "peak" in report.lower(), report
    csv_path = root.parent / "calhacks-report.csv"
    command(binary, "recordings", "export", path, "--output", csv_path)
    with csv_path.open(newline="") as stream:
        rows = list(csv.reader(stream))
    assert len(rows) > 1, rows
    for row in rows:
        for cell in row:
            assert "\x1b" not in cell and "\x00" not in cell
    original = csv_path.read_bytes()
    if hasattr(os, "symlink"):
        linked = root.parent / "existing-symlink.csv"
        linked.symlink_to(csv_path)
        command(binary, "recordings", "export", path, "--output", linked, ok=False)
        assert linked.is_symlink() and csv_path.read_bytes() == original
    if os.name == "posix":
        assert path.stat().st_mode & 0o777 == 0o600, "recording permissions are not private"
        assert path.parent.stat().st_mode & 0o777 == 0o700, "new recording directory permissions are not private"
    command(binary, "recordings", "export", path, "--output", csv_path, ok=False)
    assert csv_path.read_bytes() == original, "export overwrote an existing file"
    print("PASS named recording, paused gap, resume, stop, separate restart, local list/review/CSV and no overwrite")
    return path


def termination(binary, root):
    terminal = launch(binary, root, "Signal exit")
    success = False
    try:
        terminal.read(1.2)
        terminal.process.send_signal(signal.SIGTERM)
        terminal.process.wait(timeout=3)
        terminal.read(0.1)
        assert terminal.process.returncode == 0
        success = True
    finally:
        terminal.close(success)
    path = files(root)[0]
    report = command(binary, "recordings", "show", path)
    assert "interrupted" not in report.lower() and "unfinalized" not in report.lower(), report
    print("PASS SIGTERM finalizes recording and restores terminal, alternate screen and mouse capture")


def abrupt_recovery(binary, root):
    terminal = launch(binary, root, "Interrupted")
    try:
        terminal.read(1.2)
        terminal.process.kill()
        terminal.process.wait(timeout=3)
        terminal.read(0.1)
        path = files(root)[0]
    finally:
        # SIGKILL gives applications no cleanup opportunity. This is an isolated
        # test PTY, and resetting it here is not evidence of application cleanup.
        if terminal.process.poll() is None:
            terminal.process.kill()
            terminal.process.wait()
        termios.tcsetattr(terminal.slave, termios.TCSANOW, terminal.original)
        os.close(terminal.master)
        os.close(terminal.slave)
    prefix = path.read_bytes()
    assert recorded_samples(path) >= 1
    report = command(binary, "recordings", "show", path)
    assert any(word in report.lower() for word in ("interrupted", "unfinalized", "incomplete")), report
    with path.open("ab") as stream:
        stream.write(b"S\tpartial-tail-without-newline")
    torn_report = command(binary, "recordings", "show", path)
    assert any(word in torn_report.lower() for word in ("partial", "truncated", "tail", "incomplete")), torn_report
    assert path.read_bytes().startswith(prefix), "review mutated a recording"
    print("PASS SIGKILL retains complete prefix; review recognizes interrupted and torn-tail data without mutating it")


def disk_limit_failure(binary, root):
    """OS-enforced short writes, without filling disks or changing user files."""
    if not hasattr(resource, "RLIMIT_FSIZE") or not hasattr(signal, "SIGXFSZ"):
        print("SKIP platform lacks file-size-limit injection")
        return
    original_popen = subprocess.Popen
    def limit_file_writes():
        signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
        resource.setrlimit(resource.RLIMIT_FSIZE, (2048, 2048))
    def spawn(*args, **kwargs):
        return original_popen(*args, **kwargs, preexec_fn=limit_file_writes)
    with patch("subprocess.Popen", spawn):
        terminal = launch(binary, root, "Disk limit")
    success = False
    try:
        terminal.read(5.0)
        terminal.expect("RECORDING STOPPED")
        assert terminal.process.poll() is None, "disk failure crashed monitoring"
        terminal.key(b"2")
        terminal.expect("Process Map")
        path = files(root)[0]
        assert path.stat().st_size <= 2048
        saved = path.read_bytes()
        terminal.read(1.2)
        assert path.read_bytes() == saved, "failed recorder kept attempting writes"
        success = True
    finally:
        terminal.close(success)
    report = command(binary, "recordings", "show", path)
    assert "RECOVERED PREFIX" in report or "UNFINALIZED" in report, report
    print("PASS OS file-size failure stops recording visibly, preserves readable prefix and leaves monitoring usable")


def live_segregation(binary, root):
    terminal = launch(binary, root, "Live isolation", live=True)
    success = False
    try:
        terminal.expect("LIVE")
        terminal.read(0.8)
        path = files(root)[0]
        assert path.parent.name == "live", path
        success = True
    finally:
        terminal.close(success)
    report = command(binary, "recordings", "show", path)
    assert "LIVE local samples" in report and "SIMULATED DATA" not in report, report
    print("PASS real live samples are labeled and stored separately from simulated recordings")


def bad_directory(binary, root):
    root.write_text("not a directory")
    terminal = launch(binary, root, "Blocked")
    try:
        terminal.process.wait(timeout=3)
        terminal.read(0.1)
        assert terminal.process.returncode != 0
        assert termios.tcgetattr(terminal.slave) == terminal.original, "failed recording start leaked raw mode"
        assert b"\x1b[?1049h" not in terminal.output, "invalid directory should fail before TUI setup"
        assert root.read_text() == "not a directory"
    finally:
        if terminal.process.poll() is None:
            terminal.process.kill()
            terminal.process.wait()
        os.close(terminal.master)
        os.close(terminal.slave)
    print("PASS invalid storage fails before raw mode and preserves existing data")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/lattice", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="lattice-recorder-pty-") as directory:
        root = Path(directory)
        no_implicit_recording(binary, root / "off")
        named_recording(binary, root / "named")
        termination(binary, root / "signal")
        abrupt_recovery(binary, root / "crash")
        bad_directory(binary, root / "blocked")
        disk_limit_failure(binary, root / "disk-limited")
        live_segregation(binary, root / "live-scope")
    print("All recording PTY workflows passed.")


if __name__ == "__main__":
    main()
