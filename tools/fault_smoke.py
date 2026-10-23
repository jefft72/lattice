#!/usr/bin/env python3
"""Build disposable fault-injected copies to check sampler failure cleanup.

The original source and release artifact are never modified. Requires cargo and
Unix PTYs. Run from any directory with `python3 tools/fault_smoke.py`.
"""
import fcntl
import os
import pty
import shutil
import signal
import struct
import subprocess
import tempfile
import termios
from pathlib import Path
from pty_smoke import drain


def check(binary, paused=False, startup=False, recording=False):
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 44, 144, 0, 0))
    recording_directory = tempfile.TemporaryDirectory(prefix="lattice-fault-recording-")
    recording_root = Path(recording_directory.name)
    extra = ["--record", "Fault test", "--record-dir", str(recording_root)] if recording else []
    process = subprocess.Popen(
        [str(binary), "--demo", "--interval", "0.5", *extra], stdin=slave, stdout=slave, stderr=slave,
        env={**os.environ, "TERM": "xterm-256color"},
    )
    try:
        if startup:
            output = drain(master, 0.3)
            process.send_signal(signal.SIGTERM)
            process.wait(timeout=1)
            assert process.returncode == 0
        else:
            output = drain(master, 1.1 if paused else 0.3)
            assert process.poll() is None
            if paused:
                os.write(master, b" ")
            output += drain(master, 1.7)
            process.wait(timeout=2)
            assert process.returncode != 0, "sampler panic was reported as success"
            assert b"\x1b[?1049l" in output, "alternate screen not restored"
            assert b"\x1b[?1000l" in output, "mouse capture not restored"
        assert termios.tcgetattr(slave) == before, "terminal flags not restored"
        if recording:
            files = list(recording_root.rglob("*.lattice"))
            if startup:
                assert not files, "cancellation before initial sample created a recording"
            else:
                assert len(files) == 1, files
                data = files[0].read_text()
                assert data.splitlines()[-1].endswith("\tapp_error"), data
                assert data.splitlines()[-1].startswith("F\t"), "sampler failure did not finalize recording"
        print(f"PASS {'startup cancellation' if startup else 'paused sampler panic' if paused else 'sampler panic'}{' with opt-in recording' if recording else ''}")
    finally:
        if process.poll() is None:
            process.kill()  # Only this script's disposable child.
            process.wait()
        os.close(master)
        os.close(slave)
        recording_directory.cleanup()


def main():
    root = Path(__file__).resolve().parents[1]
    target = root / "target" / "fault-smoke"
    with tempfile.TemporaryDirectory(prefix="lattice-fault-") as temporary:
        copy = Path(temporary)
        shutil.copytree(root / "src", copy / "src")
        for name in ["Cargo.toml", "Cargo.lock", "README.md"]:
            shutil.copy2(root / name, copy / name)
        source = copy / "src" / "sampling.rs"
        original = source.read_text()
        needle = "        let frame = self.frame;"
        assert original.count(needle) == 1, "demo injection point changed; update this test"
        scenarios = [
            '        if self.frame == 2 { std::thread::sleep(Duration::from_millis(500)); panic!("test sampler failure"); }\n',
            '        if self.frame == 0 { std::thread::sleep(Duration::from_secs(5)); }\n',
        ]
        for index, injection in enumerate(scenarios):
            source.write_text(original.replace(needle, injection + needle))
            subprocess.run(["cargo", "build", "--quiet", "--locked", "--manifest-path", str(copy / "Cargo.toml"), "--target-dir", str(target)], check=True)
            binary = target / "debug" / "lattice"
            if index == 0:
                check(binary)
                check(binary, paused=True)
                check(binary, recording=True)
                check(binary, paused=True, recording=True)
            else:
                check(binary, startup=True)
                check(binary, startup=True, recording=True)


if __name__ == "__main__":
    main()
