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
import time
from pathlib import Path
from pty_smoke import drain


def check(binary, paused=False, startup=False, recording=False, startup_delay_ms=0):
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 44, 144, 0, 0))
    recording_directory = tempfile.TemporaryDirectory(prefix="lattice-fault-recording-")
    recording_root = Path(recording_directory.name)
    fault_directory = tempfile.TemporaryDirectory(prefix="lattice-fault-handshake-")
    ready = Path(fault_directory.name) / "ready"
    release = Path(fault_directory.name) / "release"
    extra = ["--record", "Fault test", "--record-dir", str(recording_root)] if recording else []
    process = subprocess.Popen(
        [str(binary), "--demo", "--interval", "0.5", *extra], stdin=slave, stdout=slave, stderr=slave,
        env={**os.environ, "TERM": "xterm-256color", "LATTICE_FAULT_READY": str(ready),
             "LATTICE_FAULT_RELEASE": str(release), "LATTICE_FAULT_START_DELAY_MS": str(startup_delay_ms)},
    )
    output = bytearray()

    def diagnostics():
        return (f"ready={ready.exists()}, paused={b'PAUSED' in output}, release={release.exists()}, "
                f"panic={b'test sampler failure' in output}, returncode={process.poll()}, "
                f"terminal_restored={termios.tcgetattr(slave) == before}; output={output[-1500:]!r}")

    def until(predicate, timeout, message):
        # Keep consuming the PTY while waiting: a full output buffer must not
        # prevent the child from rendering PAUSED or restoring its terminal.
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            output.extend(drain(master, min(0.05, max(0, deadline - time.monotonic()))))
            if predicate():
                return
            assert process.poll() is None, f"{message}: child exited early; {diagnostics()}"
        raise AssertionError(f"{message} after {timeout}s: {diagnostics()}")

    try:
        # The disposable sampler explicitly acknowledges the fault boundary.
        # Wall-clock guesses can pause it before that boundary is ever reached.
        until(lambda: ready.exists(), 5, "sampler did not reach injected fault")
        if startup:
            process.send_signal(signal.SIGTERM)
            until(lambda: process.poll() is not None, 1, "startup cancellation blocked")
            assert process.returncode == 0
        else:
            until(lambda: termios.tcgetattr(slave) != before and b"\x1b[?1000h" in output,
                  2, "terminal did not enter interactive mode")
            if paused:
                os.write(master, b" ")
                until(lambda: b"PAUSED" in output, 2, "pause was not rendered before sampler failure")
            assert process.poll() is None
            release.touch()
            # Bound cleanup from the actual injected panic, not process startup.
            # This deadline is stricter than the old 1.7s drain + 2s wait.
            until(lambda: process.poll() is not None, 2, "sampler failure did not exit promptly")
            output.extend(drain(master, 0.05))
            assert process.returncode != 0, "sampler panic was reported as success"
            assert b"test sampler failure" in output, "injected panic did not occur"
            assert b"process sampler stopped unexpectedly" in output, "sampler disconnect was not reported"
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
        fault_directory.cleanup()


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
        # File handshakes live only in the disposable injected source. They do
        # not depend on PTY output ordering and add no production fault hooks.
        ready = 'std::fs::write(std::env::var_os("LATTICE_FAULT_READY").unwrap(), b"ready").unwrap();'
        wait_release = ('while !std::path::Path::new(&std::env::var_os("LATTICE_FAULT_RELEASE").unwrap()).exists() '
                        '{ std::thread::sleep(Duration::from_millis(10)); }')
        startup_delay = ('        if self.frame == 0 { std::thread::sleep(Duration::from_millis('
                         'std::env::var("LATTICE_FAULT_START_DELAY_MS").unwrap().parse().unwrap())); }\n')
        scenarios = [
            startup_delay + '        if self.frame == 2 { ' + ready + wait_release + ' panic!("test sampler failure"); }\n',
            '        if self.frame == 0 { ' + ready + ' std::thread::sleep(Duration::from_secs(5)); }\n',
        ]
        for index, injection in enumerate(scenarios):
            source.write_text(original.replace(needle, injection + needle))
            subprocess.run(["cargo", "build", "--quiet", "--locked", "--manifest-path", str(copy / "Cargo.toml"), "--target-dir", str(target)], check=True)
            binary = target / "debug" / "lattice"
            if index == 0:
                check(binary)
                # A delayed startup used to let Space prevent the fault entirely.
                check(binary, paused=True, startup_delay_ms=700)
                check(binary, recording=True)
                check(binary, paused=True, recording=True, startup_delay_ms=1300)
            else:
                check(binary, startup=True)
                check(binary, startup=True, recording=True)


if __name__ == "__main__":
    main()
