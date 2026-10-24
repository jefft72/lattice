#!/usr/bin/env python3
"""Inject recording faults into a disposable source copy, never production hooks.

Four-second initialization, write, periodic-sync and final-sync stalls must not
block input or terminal cleanup. Includes backpressure, restart and job control.
Only this script's isolated PTY children receive keys or signals.
"""
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import termios
import time

from navigation_smoke import expect_view
from onboarding_smoke import Terminal

MARKER = b"RECORDING_IO_FAULT_BLOCKED"


def replace_once(source, needle, replacement):
    assert source.count(needle) == 1, f"recording fault injection point changed: {needle!r}"
    return source.replace(needle, replacement)


def inject(source):
    delay = 'eprintln!("RECORDING_IO_FAULT_BLOCKED"); std::thread::sleep(Duration::from_secs(4));'
    source = replace_once(source, "        let name = validate_name(&options.name)?;",
        '        if options.name.starts_with("delay-init") { ' + delay + ' }\n'
        + "        let name = validate_name(&options.name)?;")
    source = replace_once(source, "        if line.len() > MAX_LINE_BYTES",
        '        if line.starts_with("S\\t2\\t") && self.status.name == "delay-write" { ' + delay + ' }\n'
        '        if line.starts_with("S\\t2\\t") && self.status.name == "panic-write" { panic!("test recorder panic"); }\n'
        + "        if line.len() > MAX_LINE_BYTES")
    source = replace_once(source, "        if self.last_sync.elapsed() >= SYNC_INTERVAL {",
        '        if self.last_sync.elapsed() >= SYNC_INTERVAL\n'
        '            || (self.status.name == "delay-sync" && line.starts_with("S\\t2\\t")) {\n'
        '            if self.status.name == "delay-sync" { ' + delay + ' }')
    source = replace_once(source, "            .and_then(|()| self.file.sync_data());",
        '            .and_then(|()| {\n'
        '                if self.status.name == "delay-finish" { ' + delay + ' }\n'
        '                if self.status.name == "fail-finish" { return Err(io::Error::other("injected final sync failure")); }\n'
        '                self.file.sync_data()\n'
        '            });')
    return source


def launch(binary, root, name):
    return Terminal(binary, 144, 44, tour=False, interval="0.5", extra_args=[
        "--demo", "--record", name, "--record-dir", str(root), "--record-every", "1",
    ])


def has_final_footer(root, reason):
    paths = list(root.rglob("*.lattice"))
    if not paths:
        return False
    data = paths[0].read_text()
    lines = data.splitlines()
    return bool(lines) and data.endswith("\n") and lines[-1].startswith("F\t") and lines[-1].endswith(f"\t{reason}")


def until(terminal, predicate, timeout, message):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        terminal.read(0.02)
        if predicate():
            return
    raise AssertionError(f"{message}\n{terminal.screen()}")


def blocked(terminal):
    until(terminal, lambda: MARKER in terminal.output, 3, "worker did not enter injected delay")
    assert terminal.process.poll() is None
    assert termios.tcgetattr(terminal.slave) != terminal.original


def restored(terminal, start):
    until(terminal, lambda: termios.tcgetattr(terminal.slave) == terminal.original, 0.9,
          "slow recording I/O blocked raw-mode restoration")
    terminal.read(0.03)
    assert time.monotonic() - start < 1.0, "terminal cleanup took a second or more"
    assert b"\x1b[?1049l" in terminal.output, "alternate screen remained active"
    assert b"\x1b[?1000l" in terminal.output, "mouse capture remained active"


def cleanup(terminal):
    if terminal.process.poll() is None:
        terminal.process.kill()
        terminal.process.wait()
    termios.tcsetattr(terminal.slave, termios.TCSANOW, terminal.original)
    os.close(terminal.master)
    os.close(terminal.slave)


def check_delay(binary, root, mode, exit_action):
    terminal = launch(binary, root, f"delay-{mode}")
    try:
        if mode == "finish":
            terminal.key(b"R", 0.1)
        blocked(terminal)
        start = time.monotonic()
        terminal.key(b"2", 0.15)
        expect_view(terminal, "map")  # Content, not an inactive tab label.
        assert time.monotonic() - start < 0.7, "navigation waited for storage"
        start = time.monotonic()
        if isinstance(exit_action, bytes):
            os.write(terminal.master, exit_action)
        else:
            terminal.process.send_signal(exit_action)
        restored(terminal, start)
        assert terminal.process.poll() is None, "test never observed final I/O pending after cleanup"
        terminal.process.wait(timeout=8)
        terminal.read(0.05)
        assert terminal.process.returncode == 0, terminal.output[-1500:]
        paths = list(root.rglob("*.lattice"))
        assert len(paths) == 1
        data = paths[0].read_text()
        assert data.splitlines()[-1].startswith("F\t"), data
        # Footer runtime ends at user stop, not after the four-second backlog.
        runtime = float(data.splitlines()[-1].split("\t")[1])
        assert runtime < 3.5, f"storage backlog inflated recording runtime: {runtime}"
        print(f"PASS {mode} stall: responsive navigation and immediate cleanup for {exit_action!r}")
    finally:
        cleanup(terminal)


def check_restart_and_overflow(binary, root):
    terminal = launch(binary, root, "delay-init-overflow")
    try:
        blocked(terminal)
        # Each toggle adds an ordered gap command; a stalled consumer must stop,
        # not allocate an unbounded queue or silently discard accepted samples.
        os.write(terminal.master, b" " * 40)
        until(terminal, lambda: "queue filled" in terminal.screen(), 1, "queue overflow not visible")
        terminal.key(b"R", 0.1)
        assert "START SIMULATED RECORDING" not in terminal.screen(), "started another worker before finalization"
        terminal.key(b"2", 0.1)
        expect_view(terminal, "map")
        until(terminal, lambda: has_final_footer(root, "queue_overflow"),
              6, "overflow prefix was not finalized with its failure reason")
        terminal.read(0.2)
        terminal.expect("queue filled")  # A successful sync must not erase failure.
        assert "Recording saved" not in terminal.screen()
        terminal.key(b"R", 0.1)
        terminal.expect("START SIMULATED RECORDING")
        terminal.key(b"Recovered\r", 0.1)
        until(terminal, lambda: len(list(root.rglob("*.lattice"))) == 2, 2, "restart did not create separate file")
        terminal.key(b"R", 0.1)
        until(terminal, lambda: "Recording saved" in terminal.screen(), 2, "restart did not finalize")
        os.write(terminal.master, b"q")
        terminal.process.wait(timeout=3)
        assert terminal.process.returncode == 0
        print("PASS bounded overflow stays visible, marks prefix, and permits restart only after finalization")
    finally:
        cleanup(terminal)


def check_stop_starting(binary, root):
    terminal = launch(binary, root, "delay-init-stop")
    try:
        blocked(terminal)
        terminal.key(b"R", 0.1)
        terminal.expect("final save pending")
        terminal.key(b"R", 0.1)
        assert "START SIMULATED RECORDING" not in terminal.screen()
        assert "Recording saved" not in terminal.screen()
        until(terminal, lambda: "Recording saved" in terminal.screen(), 6, "pending start/stop never completed")
        data = next(root.rglob("*.lattice")).read_text()
        assert data.splitlines()[-1].endswith("\tuser_stop")
        assert float(data.splitlines()[-1].split("\t")[1]) < 2
        os.write(terminal.master, b"q")
        terminal.process.wait(timeout=3)
        assert terminal.process.returncode == 0
        print("PASS stopping during delayed initialization is nonblocking and never falsely reports saved")
    finally:
        cleanup(terminal)


def check_final_error(binary, root):
    terminal = launch(binary, root, "fail-finish")
    try:
        terminal.key(b"R", 0.1)
        until(terminal, lambda: "injected final sync failure" in terminal.screen(), 2, "final-sync error not surfaced")
        assert "Recording saved" not in terminal.screen()
        terminal.key(b"2", 0.1)
        expect_view(terminal, "map")
        os.write(terminal.master, b"q")
        terminal.process.wait(timeout=3)
        terminal.read(0.1)
        assert b"injected final sync failure" in terminal.output
        assert terminal.process.returncode == 0
        print("PASS final-sync failure remains visible and monitoring continues")
    finally:
        cleanup(terminal)


def check_panic(binary, root, ui):
    terminal = launch(binary, root, "delay-init-panic" if ui else "panic-write")
    try:
        if ui:
            blocked(terminal)
            os.write(terminal.master, b"/panic-test\r")
        start = time.monotonic()
        if not ui:
            until(terminal, lambda: b"test recorder panic" in terminal.output, 3, "injected worker panic did not fire")
            start = time.monotonic()
        restored(terminal, start)
        terminal.process.wait(timeout=8)
        assert terminal.process.returncode != 0, "panic was reported as success"
        if ui:
            data = next(root.rglob("*.lattice")).read_text()
            assert data.splitlines()[-1].endswith("\tapp_error")
        print(f"PASS {'UI' if ui else 'recording worker'} panic restores terminal without waiting for I/O")
    finally:
        cleanup(terminal)


def check_suspend(binary, root):
    terminal = launch(binary, root, "delay-init-suspend")
    try:
        blocked(terminal)
        for action in (b"\x1a", signal.SIGTSTP):
            offset = len(terminal.output)
            start = time.monotonic()
            if isinstance(action, bytes):
                os.write(terminal.master, action)
            else:
                terminal.process.send_signal(action)
            restored(terminal, start)
            state = []
            def stopped():
                pid, status = os.waitpid(terminal.process.pid, os.WUNTRACED | os.WNOHANG)
                if pid:
                    state.append(status)
                return bool(state)
            until(terminal, stopped, 1, "application did not stop after restoring terminal")
            assert os.WIFSTOPPED(state[-1]), state
            terminal.process.send_signal(signal.SIGCONT)
            until(terminal, lambda: termios.tcgetattr(terminal.slave) != terminal.original, 1, "continue did not reenter raw mode")
            terminal.read(0.1)
            assert b"\x1b[?1049h" in terminal.output[offset:]
        terminal.key(b"2", 0.1)
        expect_view(terminal, "map")
        start = time.monotonic()
        os.write(terminal.master, b"q")
        restored(terminal, start)
        terminal.process.wait(timeout=8)
        assert terminal.process.returncode == 0
        data = next(root.rglob("*.lattice")).read_text()
        assert any(line.startswith("G\t") for line in data.splitlines()), data
        print("PASS repeated Ctrl-Z/SIGTSTP/CONT cycles remain responsive while recording initialization stalls")
    finally:
        cleanup(terminal)


def main():
    root = Path(__file__).resolve().parents[1]
    target = root / "target" / "recording-io-smoke"
    with tempfile.TemporaryDirectory(prefix="lattice-recording-io-") as temporary:
        copy = Path(temporary)
        shutil.copytree(root / "src", copy / "src")
        for name in ("Cargo.toml", "Cargo.lock", "README.md"):
            shutil.copy2(root / name, copy / name)
        source = copy / "src" / "recording.rs"
        source.write_text(inject(source.read_text()))
        source = copy / "src" / "app.rs"
        source.write_text(replace_once(source.read_text(), "            dirty |= poll_recording(&mut app, &mut recorder)?;",
            '            if app.query == "panic-test" { panic!("test UI panic"); }\n'
            '            dirty |= poll_recording(&mut app, &mut recorder)?;'))
        subprocess.run(["cargo", "build", "--quiet", "--locked", "--manifest-path", str(copy / "Cargo.toml"), "--target-dir", str(target)], check=True)
        binary = target / "debug" / "lattice"
        for mode in ("init", "write", "sync", "finish"):
            for index, action in enumerate((b"q", b"\x03", signal.SIGQUIT)):
                check_delay(binary, copy / f"{mode}-{index}", mode, action)
        check_stop_starting(binary, copy / "stop-starting")
        check_restart_and_overflow(binary, copy / "overflow")
        check_final_error(binary, copy / "final-error")
        check_panic(binary, copy / "worker-panic", ui=False)
        check_panic(binary, copy / "ui-panic", ui=True)
        check_suspend(binary, copy / "suspend")
    print("All bounded recording I/O fault checks passed.")


if __name__ == "__main__":
    main()
