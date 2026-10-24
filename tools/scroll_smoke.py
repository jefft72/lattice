#!/usr/bin/env python3
"""Exercise bounded SGR wheel bursts in disposable Unix PTYs (stdlib only).

Run after cargo build --release. These are terminal-protocol events, not physical
trackpad gestures. SIGSTOP lets a finite burst arrive before the child consumes
it; only this script's own children are signalled. Rate bounds use measured burst
duration, not sleeps timed to the throttle boundary. Rust tests cover exact time
boundaries. The existing Terminal owns rendering and terminal-cleanup checks.
"""

import argparse
import math
import os
from pathlib import Path
import re
import signal
import time

from navigation_smoke import TITLES, VIEWS, expect_active_tab, tab_position
from onboarding_smoke import Terminal


FRAME_END = b"\x1b[?25l"  # ratatui ends each rendered frame by hiding the cursor.
BURST = 32
VERTICAL_INTERVAL = 0.090
HORIZONTAL_INTERVAL = 0.250


def sgr(code, x=4, y=8):
    """SGR coordinates are one based; callers use visible cell coordinates."""
    return f"\x1b[<{code};{x + 1};{y + 1}M".encode()


def selected_family(screen):
    match = re.search(r"›[^\n]*#(\d+)", screen)
    assert match, f"missing selected family\n{screen}"
    return int(match[1])


def family_order(terminal):
    order = [int(pid) for pid in re.findall(r"#(4[1-4]00)\b", terminal.screen())]
    if len(order) != 4:
        # Compact layouts show only the selected family. Establish the frozen
        # fixture order through ordinary keyboard navigation, returning to start.
        order = []
        for _ in range(4):
            order.append(selected_family(terminal.screen()))
            terminal.key(b"j", 0.04)
        assert selected_family(terminal.screen()) == order[0]
    assert len(order) == len(set(order)) == 4, f"incomplete family fixture: {order}"
    return order


def selected_process(screen):
    match = re.search(r"PROCESS EXPLORER #(\d+)", screen)
    assert match, f"missing selected process\n{screen}"
    return int(match[1])


def guide_position(screen):
    match = re.search(r"FIELD GUIDE (\d+)/(\d+)", screen)
    assert match, f"missing guide position\n{screen}"
    return int(match[1])


def selected_view(screen):
    body = "\n".join(screen.splitlines()[5:-4])
    found = [view for view in VIEWS if TITLES[view] in body]
    assert len(found) == 1, f"ambiguous view: {found}\n{screen}"
    return found[0]


def frame_states(terminal, start, extract):
    """Decode every completed draw, including frames coalesced in one PTY read.

    Looking only at the final screen would miss a whole lap through four views
    or families. Decode after draining the burst so this work cannot slow down
    the UI and accidentally turn a burst into spaced-out input.
    """
    output = terminal.output
    states = []
    try:
        cursor = start
        while True:
            end = output.find(FRAME_END, cursor)
            if end < 0:
                return states
            cursor = end + len(FRAME_END)
            terminal.output = output[:cursor]
            states.append(extract(terminal.screen()))
    finally:
        terminal.output = output


def changed_states(terminal, start, extract, before):
    """Retain every observable transition, ignoring identical redraws only."""
    changes = []
    for state in frame_states(terminal, start, extract):
        if state != before:
            changes.append(state)
            before = state
    return changes


def queued_input(terminal, data, pause_fence=False):
    """Queue a finite packet while stopped; resume and drain its rendered result.

    In ordinary views, a trailing pause key is an observable FIFO fence: PAUSED
    proves every preceding wheel/key/click was consumed, and freezes demo data.
    Overlays consume pause keys, so those cases drain to a quiet output period
    and subsequently check every idle frame for delayed movement.
    """
    if pause_fence:
        assert "PAUSED" not in terminal.screen(), "pause fence needs a running view"
        data += b" "
    assert len(data) <= 2048, "burst must fit comfortably in the PTY input queue"
    start = len(terminal.output)
    terminal.process.send_signal(signal.SIGSTOP)
    stopped = False
    try:
        deadline = time.monotonic() + 3
        while not stopped:
            pid, status = os.waitpid(terminal.process.pid, os.WNOHANG | os.WUNTRACED)
            if pid:
                assert os.WIFSTOPPED(status), f"child exited before burst: {status}"
                stopped = True
            else:
                assert time.monotonic() < deadline, "child did not stop for burst"
                time.sleep(0.005)
        assert os.write(terminal.master, data) == len(data), "partial PTY burst write"
    finally:
        # Always resume our own child, including after a failed assertion, so
        # inherited cleanup cannot leave it stopped or wait forever on SIGTERM.
        if terminal.process.poll() is None:
            # Include any parent descheduling after resume in the rate budget.
            resumed = time.monotonic()
            terminal.process.send_signal(signal.SIGCONT)
    last_output = resumed
    deadline = resumed + 4
    while True:
        previous = len(terminal.output)
        terminal.read(0.02)
        now = time.monotonic()
        if len(terminal.output) != previous:
            last_output = now
        assert terminal.process.poll() is None, "unexpected exit during queued input"
        complete_frame = terminal.output.endswith(FRAME_END)
        if pause_fence:
            if complete_frame and "PAUSED" in terminal.screen():
                return start, now - resumed
        elif complete_frame and now - last_output >= 0.15:
            return start, max(0, last_output - resumed)
        assert now < deadline, f"queued input did not finish\n{terminal.screen()}"


def bounded(terminal, before, start, elapsed, extract, interval, next_state):
    states = frame_states(terminal, start, extract)
    assert states, "burst produced no completed frame"
    previous, moves = before, 0
    for state in states:
        if state != previous:
            assert state == next_state(previous), f"non-adjacent wheel movement: {previous} -> {state}"
            moves += 1
            previous = state
    # This upper bound includes scheduling/PTY observation overhead, making it
    # conservative on slow runners. It does not demand a particular event rate.
    budget = 1 + math.floor(elapsed / interval)
    assert 1 <= moves <= budget, f"{moves} wheel actions in {elapsed:.3f}s (budget {budget})"
    assert moves < BURST, f"all {BURST} wheel events acted; burst was not reduced"
    assert extract(terminal.screen()) == previous
    return moves


def no_idle_drift(terminal, extract):
    before = extract(terminal.screen())
    start = len(terminal.output)
    terminal.read(0.65)  # Longer than both limits; there must be no queued action.
    states = frame_states(terminal, start, extract)
    assert all(state == before for state in states), f"wheel action drifted after input ended: {states}"
    assert extract(terminal.screen()) == before, "wheel action arrived after input ended"


def family_and_view_bursts(binary, width, height):
    for horizontal in (False, True):
        terminal = Terminal(binary, width, height, tour=False)
        success = False
        try:
            extract = selected_view if horizontal else selected_family
            order = list(VIEWS) if horizontal else family_order(terminal)
            interval = HORIZONTAL_INTERVAL if horizontal else VERTICAL_INTERVAL
            before = extract(terminal.screen())
            start, elapsed = queued_input(terminal, sgr(67 if horizontal else 65) * BURST, True)
            moves = bounded(terminal, before, start, elapsed, extract, interval,
                            lambda state: order[(order.index(state) + 1) % len(order)])
            # Every frame is counted, so a full lap cannot hide behind the same
            # final view. A short horizontal burst cannot complete a full lap.
            if horizontal and elapsed < 3 * HORIZONTAL_INTERVAL:
                assert moves < len(VIEWS), "one short wheel burst cycled all views"
            no_idle_drift(terminal, extract)
            success = True
            kind = "horizontal views" if horizontal else "family list"
            print(f"PASS {width}x{height} {kind}: {BURST} SGR events -> {moves} action(s), no idle drift")
        finally:
            terminal.close(success)


def inspector_and_help_bursts(binary):
    terminal = Terminal(binary, 144, 44, tour=False)
    success = False
    try:
        terminal.key(b"\rE", 0.04)
        terminal.expect("PROCESS EXPLORER #4100")
        # Derive the visible tree order from its actual PID column. Tree order
        # is not numeric PID order (children are nested under their parents).
        rows = [int(match[1]) for line in terminal.screen().splitlines()
                if (match := re.search(r"(?:▾|▸|·).*?\s(41\d\d)\s+", line))]
        assert len(rows) >= 5 and rows[0] == 4100, f"incomplete inspector fixture: {rows}"
        start, elapsed = queued_input(terminal, sgr(65) * BURST, True)
        moves = bounded(terminal, 4100, start, elapsed, selected_process, VERTICAL_INTERVAL,
                        lambda state: rows[min(rows.index(state) + 1, len(rows) - 1)])
        no_idle_drift(terminal, selected_process)
        # Ordinary keyboard repeats must still reach the last process promptly.
        queued_input(terminal, b"j" * BURST)
        assert selected_process(terminal.screen()) == rows[-1]
        terminal.key(b"?", 0.04)
        assert guide_position(terminal.screen()) == 1
        start, elapsed = queued_input(terminal, sgr(65) * BURST)
        bounded(terminal, 1, start, elapsed, guide_position, VERTICAL_INTERVAL, lambda state: state + 1)
        no_idle_drift(terminal, guide_position)
        # Test directions separately and use odd packet sizes: a whole view
        # cycle or opposite-direction cancellation must not mask an action
        # incorrectly applied underneath the full-screen guide.
        process = rows[-1]
        for index, code in enumerate((67, 66)):
            if index:
                terminal.key(b"?", 0.04)
            frozen = terminal.screen()
            queued_input(terminal, sgr(code) * (BURST + 1))
            assert terminal.screen() == frozen, "horizontal guide wheel changed the overlay"
            terminal.key(b"\x1b", 0.04)
            expect_active_tab(terminal, "map")
            terminal.expect("PROCESS EXPLORER")
            assert selected_process(terminal.screen()) == process
        success = True
        print(f"PASS inspector ({moves} burst action(s)), guide, keyboard process repeat, blocked guide horizontal wheel")
    finally:
        terminal.close(success)


def keyboard_clicks_and_editors(binary):
    terminal = Terminal(binary, 144, 44, tour=False)
    success = False
    try:
        order = family_order(terminal)
        expected = order[35 % len(order)]
        start, _ = queued_input(terminal, b"j" * 35, True)
        assert changed_states(terminal, start, selected_family, order[0]) == [
            order[index % len(order)] for index in range(1, 36)
        ], "keyboard family burst did not preserve every move"
        assert selected_family(terminal.screen()) == expected
        start, _ = queued_input(terminal, b"]" * 7)
        assert changed_states(terminal, start, selected_view, "overview") == [
            VIEWS[index % len(VIEWS)] for index in range(1, 8)
        ], "keyboard view burst did not preserve every move"
        assert selected_view(terminal.screen()) == "changes"
        # Distinct rapid clicks must all act, rather than sharing a wheel gate.
        clicks = ("history", "map", "overview")
        start, _ = queued_input(terminal, b"".join(sgr(0, *tab_position(terminal, view))
                                                   for view in clicks))
        assert changed_states(terminal, start, selected_view, "changes") == list(clicks), \
            "rapid tab clicks did not preserve every selection"
        assert selected_view(terminal.screen()) == "overview"
        assert selected_family(terminal.screen()) == expected
        terminal.key(b" ", 0.04)  # Resume before entering the recording-name editor.
        for opener, value, text in ((b"/", b"4", "/ 4"), (b"r", b"wheel-test", "NAME SESSION"),
                                    (b"R", b"wheel-test", "START SIMULATED RECORDING")):
            terminal.key(opener + value, 0.04)
            terminal.expect(text)
            frozen = terminal.screen()
            queued_input(terminal, sgr(65) * (BURST + 1) + sgr(67) * (BURST + 1))
            assert terminal.screen() == frozen, f"wheel changed {opener!r} editor"
            if opener == b"/":
                expect_active_tab(terminal, "overview")
            terminal.key(b"\x1b", 0.04)
            assert selected_view(terminal.screen()) == "overview"
            assert selected_family(terminal.screen()) == expected
        success = True
        print("PASS unrestricted keyboard/view bursts and rapid tab clicks; search, rename, and recording editors block wheel")
    finally:
        terminal.close(success)


def tour_blocks_wheel(binary):
    terminal = Terminal(binary, 80, 24, tour=True)
    success = False
    try:
        frozen = terminal.screen()
        queued_input(terminal, sgr(65) * (BURST + 1) + sgr(67) * (BURST + 1))
        assert terminal.screen() == frozen, "wheel changed onboarding guide"
        terminal.expect("1/3")
        terminal.key(b"\x1b", 0.04)
        assert selected_view(terminal.screen()) == "overview"
        assert selected_family(terminal.screen()) == 4100
        success = True
        print("PASS onboarding guide blocks wheel bursts")
    finally:
        terminal.close(success)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/lattice")
    args = parser.parse_args()
    binary = Path(args.binary).resolve()
    assert binary.is_file(), f"build the release binary first: {binary}"
    for size in ((144, 44), (80, 24)):
        family_and_view_bursts(binary, *size)
    inspector_and_help_bursts(binary)
    keyboard_clicks_and_editors(binary)
    tour_blocks_wheel(binary)


if __name__ == "__main__":
    main()
