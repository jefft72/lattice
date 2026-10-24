#!/usr/bin/env python3
"""Exercise the v3 view deck in real Unix PTYs using only the Python stdlib.

Run after cargo build --release. Every terminal is a disposable demo child;
cleanup retains the shared raw-mode, alternate-screen, and mouse assertions.
Horizontal-wheel coverage injects standard SGR terminal events, not OS gestures.
"""

import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
import xml.etree.ElementTree as ET

from onboarding_smoke import Terminal


VIEWS = ("overview", "map", "history", "changes")
TITLES = {
    "overview": "Overview",
    "map": "Process Map",
    "history": "History",
    "changes": "WHAT CHANGED",
}


def expect_view(terminal, view, inspector=False):
    screen = terminal.screen()
    title = "PROCESS EXPLORER" if inspector else TITLES[view]
    # The tabs contain all four names at all times. Assert the content region,
    # not a matching inactive tab, to prove the requested screen actually opened.
    body = "\n".join(screen.splitlines()[5:-4])
    assert title in body, f"expected {view}, inspector={inspector}\n{screen}"
    if view == "map" and not inspector:
        assert "PROCESS EXPLORER" not in screen, screen
    terminal.expect("DEMO")
    terminal.expect("q quit")
    return screen


def press(terminal, key, view=None, inspector=False):
    terminal.key(key, 0.04)
    if view:
        return expect_view(terminal, view, inspector)
    return terminal.screen()


def mouse(terminal, code, x, y):
    """SGR coordinates are one based; callers use visible cell coordinates."""
    os.write(terminal.master, f"\x1b[<{code};{x + 1};{y + 1}M".encode())
    terminal.read(0.15)
    assert terminal.process.poll() is None, "unexpected exit during mouse input"


def tab_position(terminal, view):
    # Find visible tab labels rather than freezing their exact column widths.
    label = {"overview": "Home" if terminal.width < 72 else "Overview", "map": "Map" if terminal.width < 72 else "Process Map", "history": "History", "changes": "Changes"}[view]
    for y, line in enumerate(terminal.screen().splitlines()[:5]):
        match = re.search(rf"\b{label}\b", line)
        if match:
            return match.start(), y
    raise AssertionError(f"missing {view} tab\n{terminal.screen()}")


def check_tabs(terminal):
    for view in VIEWS:
        tab_position(terminal, view)


def expect_active_tab(terminal, view):
    # Search replaces the content title. Verify the selected tab's actual cell
    # background rather than accidentally accepting a matching inactive label.
    screen, backgrounds = terminal.screen(backgrounds=True)
    colors = {}
    for candidate in VIEWS:
        x, y = tab_position(terminal, candidate)
        colors[candidate] = backgrounds[y][x]
    inactive = {color for candidate, color in colors.items() if candidate != view}
    assert len(inactive) == 1 and colors[view] not in inactive, \
        f"expected active tab {view}, got {colors}\n{screen}"


def navigation_and_mouse(binary, width, height):
    terminal = Terminal(binary, width, height, tour=False)
    success = False
    try:
        expect_view(terminal, "overview")
        check_tabs(terminal)
        # Direct view selection must not implicitly open the inspector.
        for key, view in ((b"2", "map"), (b"3", "history"), (b"4", "changes"), (b"1", "overview")):
            press(terminal, key, view)
            check_tabs(terminal)
        # Exercise both boundaries repeatedly, not only neighboring views.
        for _ in range(3):
            for view in ("map", "history", "changes", "overview"):
                press(terminal, b"]", view)
            for view in ("changes", "history", "map", "overview"):
                press(terminal, b"[", view)
        for view in ("map", "history", "changes", "overview"):
            press(terminal, b"\x1b[C", view)
        for view in ("changes", "history", "map", "overview"):
            press(terminal, b"\x1b[D", view)
        press(terminal, b"e", "changes")
        press(terminal, b"\x1b", "overview")
        for view in ("history", "map", "changes", "overview"):
            mouse(terminal, 0, *tab_position(terminal, view))
            expect_view(terminal, view)
        # Buttons 66/67 are the standard horizontal wheel events. Terminals
        # choose whether trackpad gestures generate these events at all.
        mouse(terminal, 67, 4, 8)
        expect_view(terminal, "map")
        mouse(terminal, 66, 4, 8)
        expect_view(terminal, "overview")
        success = True
        print(f"PASS {width}x{height}: direct/wrapped views, persistent tabs/status/footer, clicks, horizontal wheel")
    finally:
        terminal.close(success)


def edit_and_state_preservation(binary):
    terminal = Terminal(binary, 144, 44, tour=False)
    success = False
    try:
        # Search editing has priority over navigation and quitting everywhere.
        for number, view in enumerate(VIEWS, 1):
            press(terminal, str(number).encode(), view)
            press(terminal, b"/1234[]hlqgm")
            terminal.expect("/ 1234[]hlqgm")
            expect_active_tab(terminal, view)
            for sequence in (b"\x1b[D", b"\x1b[C"):
                press(terminal, sequence)
                terminal.expect("/ 1234[]hlqgm")
                expect_active_tab(terminal, view)
            mouse(terminal, 0, *tab_position(terminal, "overview" if view != "overview" else "history"))
            expect_active_tab(terminal, view)
            mouse(terminal, 67, 4, 8)
            expect_active_tab(terminal, view)
            terminal.expect("/ 1234[]hlqgm")
            press(terminal, b"\x1b", view)
            assert "/ 1234[]hlqgm" not in terminal.screen(), "cancel retained new search text"

        press(terminal, b"1", "overview")
        press(terminal, b"/4100\r")
        terminal.expect("#4100")
        press(terminal, b"\x1b", "overview")
        for number, view in enumerate(VIEWS, 1):
            press(terminal, str(number).encode(), view)
            click_target = tab_position(terminal, "history" if view != "history" else "overview")
            press(terminal, b"r1234[]hlqgm")
            terminal.expect("NAME SESSION")
            terminal.expect("1234hlqgm▏")
            assert "1234[]hlqgm" not in terminal.screen(), "rename allowed unsafe punctuation"
            press(terminal, b"\x1b[D")
            press(terminal, b"\x1b[C")
            # Rename owns a full-screen overlay, so use the previously visible
            # tab position. Check clicks and wheel separately after closing it
            # to ensure two erroneous navigation actions cannot cancel out.
            mouse(terminal, 0, *click_target)
            terminal.expect("1234hlqgm▏")
            press(terminal, b"\x1b", view)
            assert "NAME SESSION" not in terminal.screen()
            press(terminal, b"r")
            mouse(terminal, 66, 4, 8)
            terminal.expect("NAME SESSION")
            press(terminal, b"\x1b", view)
            assert "NAME SESSION" not in terminal.screen()

        press(terminal, b"1", "overview")
        press(terminal, b"rqa1234hl\r")
        press(terminal, b"*")
        press(terminal, b"/qa1234hl\r")
        terminal.expect("#4100")
        for number, view in ((2, "map"), (3, "history"), (4, "changes"), (1, "overview")):
            press(terminal, str(number).encode(), view)
            terminal.expect("/ qa1234hl")
        terminal.expect("qa1234hl")
        assert re.search(r"\*\s*qa1234hl", terminal.screen()), terminal.screen()
        terminal.expect("#4100")
        assert "#4200" not in terminal.screen(), "filter was lost during navigation"
        # Enter opens the same family's inspector. Left/right fold its root,
        # never navigate the deck. The selected root begins expanded.
        press(terminal, b"\r", "map", inspector=True)
        terminal.expect("▾ claude")
        press(terminal, b"\x1b[D", "map", inspector=True)
        terminal.expect("▸ claude")
        assert "▾ claude" not in terminal.screen()
        press(terminal, b"\x1b[C", "map", inspector=True)
        terminal.expect("▾ claude")
        press(terminal, b"h", "map", inspector=True)
        terminal.expect("▸ claude")
        press(terminal, b"l", "map", inspector=True)
        terminal.expect("▾ claude")
        press(terminal, b"3", "history")
        press(terminal, b"2", "map", inspector=True)
        terminal.expect("/ qa1234hl")
        press(terminal, b"i", "map")
        press(terminal, b"i", "map", inspector=True)
        press(terminal, b"\x1b", "map")
        terminal.expect("/ qa1234hl")
        press(terminal, b"1", "overview")
        terminal.expect("qa1234hl")
        terminal.expect("#4100")
        terminal.expect("/ qa1234hl")
        success = True
        print("PASS search/rename priority, ignored editor mouse navigation, nickname/pin/filter/session preservation, inspector folds")
    finally:
        terminal.close(success)


def resize_views(binary):
    terminal = Terminal(binary, 144, 44, tour=False)
    success = False
    try:
        for number, view in enumerate(VIEWS, 1):
            press(terminal, str(number).encode(), view)
            for width, height in ((80, 24), (40, 16), (20, 6), (1, 1), (40, 16), (144, 44)):
                terminal.resize(width, height)
                if width >= 40 and height >= 16:
                    expect_view(terminal, view)
                    check_tabs(terminal)
                elif width >= 20:
                    assert terminal.screen().strip(), f"tiny {width}x{height} screen is blank"
        press(terminal, b"1", "overview")
        press(terminal, b"\r", "map", inspector=True)
        for width, height in ((40, 16), (80, 24), (144, 44)):
            terminal.resize(width, height)
            expect_view(terminal, "map", inspector=True)
        success = True
        print("PASS repeated resize in every view and inspector, compact recovery, tiny-screen survival")
    finally:
        terminal.close(success)


def graph_and_metric_modes(binary, width, height):
    terminal = Terminal(binary, width, height, tour=False)
    success = False
    try:
        press(terminal, b"/4100\r")
        press(terminal, b"rgraphqa\r")
        press(terminal, b"*")
        press(terminal, b"2", "map")
        terminal.expect("graphqa")
        terminal.expect("#4100")
        assert "HEAT MAP" not in terminal.screen()
        press(terminal, b"g", "map")
        terminal.expect("HEAT MAP · RAM")
        terminal.expect("#4100")
        press(terminal, b"m", "map")
        terminal.expect("HEAT MAP · CPU")
        press(terminal, b"3", "history")
        terminal.expect("CPU TRAIL")
        terminal.expect("graphqa")
        terminal.expect("#4100")
        press(terminal, b"m", "history")
        terminal.expect("RAM TRAIL · RSS")
        press(terminal, b"2", "map")
        terminal.expect("HEAT MAP · RAM")
        terminal.expect("/ 4100")
        for target_width, target_height in ((40, 16), (144, 44)):
            terminal.resize(target_width, target_height)
            expect_view(terminal, "map")
            terminal.expect("HEAT MAP · RAM")
            terminal.expect("#4100")
        press(terminal, b"g", "map")
        assert "HEAT MAP" not in terminal.screen(), "graph toggle did not return to constellation"
        press(terminal, b"3", "history")
        press(terminal, b"2", "map")
        assert "HEAT MAP" not in terminal.screen(), "navigation reset graph choice"
        press(terminal, b"\r", "map", inspector=True)
        # Changing graph style returns to its full-width surface, not an
        # invisible setting hidden behind the inspector on smaller terminals.
        press(terminal, b"g", "map")
        terminal.expect("HEAT MAP · RAM")
        press(terminal, b"m", "map")
        terminal.expect("HEAT MAP · CPU")
        press(terminal, b"1", "overview")
        terminal.expect("graphqa")
        terminal.expect("#4100")
        assert re.search(r"\*\s*graphqa", terminal.screen()), terminal.screen()
        success = True
        print(f"PASS {width}x{height}: constellation/heat map, RSS/CPU lens, identity/pin/filter and mode retention")
    finally:
        terminal.close(success)


def selected_heatmap_pid(terminal):
    lines = terminal.screen().splitlines()
    for y, line in enumerate(lines[5:-4], 5):
        x = line.find("› ")
        if x < 0:
            continue
        # Compact heatmap rows include the PID on the name line; wide tiles
        # put it directly below. Only one process has the explicit marker.
        for candidate in (line[x:], lines[y + 1][x:]):
            match = re.search(r"#(\d+)\b", candidate)
            if match:
                return int(match.group(1))
    raise AssertionError(f"no selected process marker in heat map\n{terminal.screen()}")


def visible_heatmap_pids(terminal):
    body = "\n".join(terminal.screen().splitlines()[5:-4])
    assert "HEAT MAP" in body, body
    return [int(pid) for pid in re.findall(r"#(\d+)\b", body.split("HEAT MAP", 1)[1])]


def heatmap_process_selection(binary):
    terminal = Terminal(binary, 144, 44, tour=False)
    success = False
    try:
        press(terminal, b"2", "map")
        press(terminal, b"g", "map")
        assert selected_heatmap_pid(terminal) == 4100
        press(terminal, b"i", "map", inspector=True)
        press(terminal, b"C", "map", inspector=True)
        terminal.expect("▸ claude")
        press(terminal, b"i", "map")
        order = visible_heatmap_pids(terminal)
        assert len(order) == len(set(order)) == 7, order
        root_index = order.index(4100)
        direction = 1 if root_index + 1 < len(order) else -1
        press(terminal, b"j" if direction > 0 else b"k", "map")
        selected = selected_heatmap_pid(terminal)
        assert selected == order[root_index + direction], f"heat map did not follow visible tile order: {order}, selected={selected}"
        terminal.expect("claude · #4100")
        press(terminal, b"m", "map")
        assert selected_heatmap_pid(terminal) == selected, "metric sorting changed process identity"
        press(terminal, b"3", "history")
        press(terminal, b"2", "map")
        assert selected_heatmap_pid(terminal) == selected, "view navigation lost process selection"
        press(terminal, b"\t", "map")
        press(terminal, b"j", "map")
        terminal.expect("codex · #4200")
        press(terminal, b"\x1b[Z", "map")  # BackTab returns to process movement.
        order = visible_heatmap_pids(terminal)
        assert len(order) == len(set(order)) == 6, order
        root_index = order.index(4200)
        direction = 1 if root_index + 1 < len(order) else -1
        press(terminal, b"j" if direction > 0 else b"k", "map")
        selected = selected_heatmap_pid(terminal)
        assert selected == order[root_index + direction]
        press(terminal, b"i", "map", inspector=True)
        press(terminal, b"i", "map")
        assert selected_heatmap_pid(terminal) == selected
        terminal.resize(80, 24)
        expect_view(terminal, "map")
        assert selected_heatmap_pid(terminal) == selected
        press(terminal, b"k", "map")
        assert selected_heatmap_pid(terminal) == order[max(0, order.index(selected) - 1)]
        success = True
        print("PASS heat-map process cursor, metric/view identity, Tab family focus, inspector and resize retention")
    finally:
        terminal.close(success)


def motion_and_idle(binary):
    transition_bytes = {}
    for no_motion in (True, False):
        terminal = Terminal(binary, 144, 44, tour=False, no_motion=no_motion)
        success = False
        try:
            terminal.read(0.25)
            before = len(terminal.output)
            os.write(terminal.master, b"2")
            terminal.read(0.3)
            transition_bytes[no_motion] = len(terminal.output) - before
            expect_view(terminal, "map")
            # Sampling is 30 seconds. Once a transition has settled there must
            # not be a continuous animation or unbounded idle redraw stream.
            stable = terminal.screen()
            before = len(terminal.output)
            terminal.read(1.2)
            idle_bytes = len(terminal.output) - before
            assert terminal.screen() == stable, "idle screen changed without a sample or input"
            assert idle_bytes < 1024, f"unbounded idle output: {idle_bytes} bytes in 1.2 seconds"
            # Pausing remains live for navigation while all sampled readings
            # and the settled display remain fixed until the next input.
            press(terminal, b" ")
            for number, view in ((3, "history"), (4, "changes"), (1, "overview"), (2, "map")):
                press(terminal, str(number).encode())
                paused_frame = terminal.screen()
                before_settle = len(terminal.output)
                terminal.read(0.22)
                assert terminal.screen() == paused_frame, "paused navigation unexpectedly animated"
                assert len(terminal.output) - before_settle < 128, "paused navigation emitted intermediate frames"
                terminal.expect("PAUSED")
                # Expect the view without imposing DEMO while status is paused.
                terminal.expect(TITLES[view])
                terminal.expect("q quit")
            frozen = terminal.screen()
            before = len(terminal.output)
            terminal.read(1.2)
            assert terminal.screen() == frozen, "paused view changed without input"
            assert len(terminal.output) - before < 1024, "paused output was not bounded"
            success = True
            print(f"PASS no-motion={no_motion}: settled transitions, idle output {idle_bytes} bytes, pause/navigation freeze")
        finally:
            terminal.close(success)
    assert transition_bytes[False] > transition_bytes[True], \
        f"animated navigation did not produce intermediate frames: {transition_bytes}"
    print(f"PASS reduced motion removes intermediate transition output: {transition_bytes}")


def snapshot_views(binary):
    with tempfile.TemporaryDirectory(prefix="lattice-navigation-") as directory:
        for name, extra, title in [
            *((view, ["--view", view], TITLES[view]) for view in VIEWS),
            ("heatmap-cpu", ["--view", "map", "--graph", "heatmap", "--metric", "cpu"], "HEAT MAP · CPU"),
            ("history-cpu", ["--view", "history", "--metric", "cpu"], "CPU TRAIL"),
            ("legacy-inspect", ["--inspect"], "PROCESS EXPLORER"),
            ("legacy-changes", ["--changes"], "WHAT CHANGED"),
        ]:
            target = Path(directory) / f"{name}.svg"
            command = [str(binary), "--demo", "--no-motion", "--snapshot", str(target), *extra]
            subprocess.run(command, check=True, capture_output=True, text=True, timeout=10)
            settled = target.read_bytes()
            # Default-motion exports must be the same settled, deterministic
            # image, not a wall-clock-dependent intermediate animation frame.
            subprocess.run([arg for arg in command if arg != "--no-motion"],
                           check=True, capture_output=True, text=True, timeout=10)
            assert target.read_bytes() == settled, f"{name}: snapshot depended on motion mode or timing"
            root = ET.parse(target).getroot()
            # Each visible cell is a separate SVG text element. Preserve blank
            # cell gaps using its coordinates before asserting complete titles.
            cells = {}
            for element in root.iter("{http://www.w3.org/2000/svg}text"):
                cells.setdefault(float(element.attrib["y"]), []).append((float(element.attrib["x"]), element.text or ""))
            lines = []
            for _, row in sorted(cells.items()):
                line = [" "] * 144
                for x, text in row:
                    index = round((x - 24) / 9)
                    if 0 <= index < len(line):
                        line[index] = text
                lines.append("".join(line))
            screen = "\n".join(lines)
            assert title in screen, f"{name}: missing {title}\n{screen}"
            assert "DEMO" in screen and "q quit" in screen, screen
            if name == "map":
                assert "PROCESS EXPLORER" not in screen, "--view map unexpectedly opened inspector"
    print("PASS deterministic snapshots for all --view values, graph/metric options, and legacy --inspect/--changes")


def no_color_compatibility(binary):
    terminal = Terminal(binary, 80, 24, tour=False, color=False)
    success = False
    try:
        for number, view in enumerate(VIEWS, 1):
            press(terminal, str(number).encode(), view)
            check_tabs(terminal)
        press(terminal, b"2", "map")
        press(terminal, b"g", "map")
        terminal.expect("HEAT MAP · RAM")
        press(terminal, b"m", "map")
        terminal.expect("HEAT MAP · CPU")
        assert b"38;2;" not in terminal.output and b"48;2;" not in terminal.output, \
            "NO_COLOR unexpectedly emitted truecolor attributes"
        success = True
        print("PASS NO_COLOR compatibility: all views, persistent controls, heatmap numeric labels")
    finally:
        terminal.close(success)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/lattice", type=Path)
    binary = parser.parse_args().binary.resolve(strict=True)
    started = time.monotonic()
    for width, height in ((144, 44), (80, 24), (40, 16)):
        navigation_and_mouse(binary, width, height)
    edit_and_state_preservation(binary)
    resize_views(binary)
    for width, height in ((144, 44), (80, 24), (40, 16)):
        graph_and_metric_modes(binary, width, height)
    heatmap_process_selection(binary)
    motion_and_idle(binary)
    snapshot_views(binary)
    no_color_compatibility(binary)
    print(f"PASS all navigation PTY checks and q restoration ({time.monotonic() - started:.1f}s)")


if __name__ == "__main__":
    main()
