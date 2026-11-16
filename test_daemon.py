#!/usr/bin/python3 -I
"""Tests for kbd-rgb-daemon.py using fake LED files and fake input devices.

Run: python3 -I test_daemon.py   (no root, no hardware needed; touches only a temp dir)
"""
from __future__ import annotations

import importlib.util, json, os, pwd, stat, subprocess, tempfile, threading, time, types
from collections.abc import Callable, Iterator
from typing import Any
from evdev import ecodes as e, InputEvent

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("d", os.path.join(HERE, "kbd-rgb-daemon.py"))
assert spec is not None and spec.loader is not None
# Loaded from a file (the name has a hyphen), so mypy can't see its contents: treat as Any.
d: Any = importlib.util.module_from_spec(spec)
spec.loader.exec_module(d)

TMP = tempfile.mkdtemp(prefix="kbd-rgb-test-")
d.LED = os.path.join(TMP, "led")
os.makedirs(d.LED)
for z in d.ZONES:
    open(f"{d.LED}/color_{z}", "w").write("FFFFFF")
open(f"{d.LED}/brightness", "w").write("255")

M = "COMPOSE"  # the Menu key

# Never show real desktop notifications from tests; record them instead.
NOTIFIED: list[tuple[str, str]] = []


def record_notification(summary: str, body: str) -> bool:
    NOTIFIED.append((summary, body))
    return True


d.desktop_notify = record_notification
d.PROBLEM_FILE = os.path.join(TMP, "run", "problem.json")


def chk(cond: object, msg: str) -> None:
    assert cond, msg
    print("ok:", msg)


def leds() -> dict[str, str]:
    return {z: open(f"{d.LED}/color_{z}").read() for z in d.ZONES}


def test_state_migration() -> None:
    d.STATE_FILE = os.path.join(TMP, "old.json")
    json.dump({"left": 7, "center": 0, "right": 0, "extra": 0}, open(d.STATE_FILE, "w"))
    chk(d.Lights().color["left"] == "0000FF", "old palette-index state migrates to hex")


def test_parse_hex() -> None:
    for s, want in [("#ff8800", "FF8800"), ("f80", "FF8800"), (" 00aaff\n", "00AAFF"),
                    ("xyz", None), ("12345", None), (None, None)]:
        chk(d.parse_hex(s) == want, f"parse_hex({s!r}) == {want!r}")


def test_session_user() -> None:
    real_run = d.subprocess.run
    def fake(replies: list[str]) -> Callable[..., Any]:
        it = iter(replies)
        return lambda *a, **k: types.SimpleNamespace(stdout=next(it))
    try:
        d.subprocess.run = fake(["3\n", "root\n"])
        chk(d.session_user() == "root", "session_user reads active seat0 session's user")
        d.subprocess.run = fake(["\n"])
        chk(d.session_user() is None, "session_user: no active session -> None")
        d.subprocess.run = fake(["3\n", "no-such-user-xyz\n"])
        chk(d.session_user() is None, "session_user: unknown user -> None")
    finally:
        d.subprocess.run = real_run


def test_handler() -> None:
    d.STATE_FILE = os.path.join(TMP, "state", "state.json")
    L = d.Lights()
    L.set(d.ALL, "FFFFFF")
    out: list[tuple[str, int]] = []
    dialogs: list[str] = []
    h = d.Handler(L, lambda c, v: out.append((e.KEY[c], v)), dialogs.append)

    def press(*keys: tuple[str, int]) -> None:
        for name, v in keys:
            if h.key(InputEvent(0, 0, e.EV_KEY, getattr(e, "KEY_" + name), v)):
                out.append(("KEY_" + name, v))

    press(("A", 1), ("A", 0))
    chk(out == [("KEY_A", 1), ("KEY_A", 0)], "normal typing passes through"); out.clear()
    press((M, 1), (M, 0))
    chk(out == [("KEY_COMPOSE", 1), ("KEY_COMPOSE", 0)], "lone Menu tap passes through"); out.clear()

    press((M, 1), ("3", 1), ("3", 0))
    chk(set(leds().values()) == {"FFFF00"} and h.sel is None, "digit with no zone selected -> whole keyboard")
    press(("0", 1), ("0", 0))
    chk(set(leds().values()) == {"FFFFFF"}, "then 0 -> whole keyboard white")
    press(("Q", 1), ("Q", 0), ("3", 1), ("3", 0))
    chk(leds()["left"] == "FFFF00", "Q, 3 -> left yellow")
    press(("7", 1), ("7", 0))
    chk(leds()["left"] == "0000FF" and h.sel == "left", "7 retries color on left (top-row digits never select)")
    press(("9", 1), ("9", 0))
    chk(leds()["left"] == "FF00AA" and leds()["right"] == "FFFFFF", "9 is a color, not the right zone")
    press(("K", 1), ("K", 0), ("1", 1), ("1", 0))
    chk(leds()["center"] == "FF0000", "K selects center, 1 -> red")
    press(("KP6", 1), ("KP6", 0))
    chk(h.sel == "extra" and leds()["center"] == "FF0000", "numpad 6 selects numpad zone (not a color)")
    press(("6", 1), ("6", 0))
    chk(leds()["extra"] == "0088FF", "then top-row 6 -> numpad zone sky blue")
    press(("KP6", 1), ("KP6", 0))
    chk(dialogs == ["extra"], "numpad 6 again -> dialog for numpad zone"); dialogs.clear()
    press((M, 0))
    chk(out == [], "nothing leaked to apps while Menu held")

    press((M, 1), ("5", 1), ("5", 0))
    chk(h.sel is None and set(leds().values()) == {"00FFAA"}, "Menu release clears selection: 5 -> whole keyboard")
    press((M, 0), ("2", 1), ("2", 0))
    chk(out == [("KEY_2", 1), ("KEY_2", 0)], "without Menu, digits type normally"); out.clear()

    press((M, 1), ("SPACE", 1), ("SPACE", 0), ("8", 1), ("8", 0))
    chk(set(leds().values()) == {"8800FF"}, "Space, 8 -> all purple")
    press(("SPACE", 1), ("SPACE", 0), ("SPACE", 1), ("SPACE", 0))
    chk(dialogs == [d.ALL], "Space twice -> dialog for whole keyboard")
    press(("ENTER", 1), ("ENTER", 0), ("ENTER", 1), ("ENTER", 0))
    chk(dialogs == [d.ALL, "right"], "Enter twice -> dialog for right")
    press(("Q", 1), ("Q", 0), ("4", 1), ("4", 0), ("Q", 1), ("Q", 0))
    chk(dialogs[-1] == "left", "Q, 4, Q -> dialog for left")
    press(("W", 1), (M, 0), ("W", 0))
    chk(out == [], "key released after Menu is still swallowed")
    press(("X", 1), (M, 1), ("X", 0), (M, 0))
    chk(out == [("KEY_X", 1), ("KEY_X", 0), ("KEY_COMPOSE", 1), ("KEY_COMPOSE", 0)],
        "key held before Menu releases normally"); out.clear()

    press((M, 1), ("Q", 1), ("Q", 0), ("9", 1), ("9", 0), ("BACKSPACE", 1), ("BACKSPACE", 0))
    chk(set(leds().values()) == {"FFFFFF"} and h.sel is None, "Menu+Backspace resets all to white")
    press(("3", 1), ("3", 0))
    chk(set(leds().values()) == {"FFFF00"}, "after reset, selection cleared: 3 -> whole keyboard")
    press((M, 0), ("BACKSPACE", 1), ("BACKSPACE", 0))
    chk(out[-2:] == [("KEY_BACKSPACE", 1), ("KEY_BACKSPACE", 0)], "Backspace without Menu types normally")


class FakeClock:
    """Stands in for d.now so fade tests can pick the time."""
    def __init__(self) -> None:
        self.t = 1000.0
        self.real: Callable[[], float] = d.now
    def __call__(self) -> float:
        return self.t
    def __enter__(self) -> FakeClock:
        d.now = self
        return self
    def __exit__(self, *exc: object) -> None:
        d.now = self.real


def green(zone: str) -> int:
    """A zone's green channel. Retries a read that lands between the daemon thread truncating
    the fake color file and writing it (real sysfs files don't have that gap)."""
    while not (c := leds()[zone]):
        time.sleep(0.001)
    return int(c[2:4], 16)


def test_fade() -> None:
    chk(d.mix("000000", "FFFFFF", 0) == "000000" and d.mix("000000", "FFFFFF", 1) == "FFFFFF"
        and d.mix("000000", "FFFFFF", 0.5) == "808080" and d.mix("FF0000", "0000FF", 0.25) == "BF0040",
        "mix blends each channel")
    f = d.Fade(["FF0000", "FFFFFF", "0000FF"], 2.0, 100.0)
    chk([f.at(t) for t in (99.0, 100.0, 101.0, 102.0, 104.0, 105.0, 106.0)] ==
        ["FF0000", "FF0000", "FF8080", "FFFFFF", "0000FF", "800080", "FF0000"],
        "fade: period seconds from each color to the next, looping, holding before start")


def draw_all(L: Any) -> None:
    """For tests that check colors rather than timing: end due timed effects, then draw
    every fading zone at the current time (tick() draws one zone at a time)."""
    L.tick()
    with L.lock:
        L._draw([z for z in d.ZONES if L.active_fade(z)], d.now())


def test_frame_slots() -> None:
    """Frames: one zone per write, writes at least 1 / WRITE_RATE apart, each zone up to once
    per FRAME, longest-waiting zone first."""
    d.STATE_FILE = os.path.join(TMP, "slot-state.json")
    with FakeClock() as clock:
        L = d.Lights()
        L.set(d.ALL, "FFFFFF")
        writes: list[tuple[float, str]] = []
        real_write = L.write

        def record(zone: str, color: str) -> None:
            writes.append((clock.t, zone))
            real_write(zone, color)
        L.write = record

        def run_for(seconds: float) -> None:  # step like render() does
            writes.clear()
            end = clock.t + seconds
            while L.next_due() is not None and L.next_due() < end:
                clock.t = L.next_due()
                L.tick()

        def per_zone() -> dict[str, list[float]]:
            return {z: [t for t, w in writes if w == z] for z in d.ZONES}

        run(L, "rainbow")
        run_for(10.0)
        gaps = [b - a for (a, _), (b, _) in zip(writes, writes[1:])]
        chk(abs(len(writes) / 10.0 - d.WRITE_RATE) < 1 and min(gaps) >= 1 / d.WRITE_RATE - 1e-9,
            "four fading zones: WRITE_RATE writes a second, never closer together")
        counts = [len(ts) for ts in per_zone().values()]
        chk(max(counts) - min(counts) <= 1, "the zones share the writes evenly")
        chk(all(len({z for _, z in writes[i:i + 4]}) == 4 for i in range(len(writes) - 4)), "zones take turns")

        for n in (1, 2, 3):
            run(L, "set FFFFFF")
            for z in d.ZONES[:n]:
                run(L, f"cycle zone={z} AA00AA 00FF00")
            run_for(10.0)
            rates = [len(ts) / 10.0 for z, ts in per_zone().items() if z in d.ZONES[:n]]
            chk(all(abs(r - 1 / d.FRAME) <= 1 for r in rates) and {z for _, z in writes} == set(d.ZONES[:n]),
                f"{n} fading zone(s): each drawn every FRAME seconds")
        clock.t += 1000
        L.tick()
        chk(clock.t < L.next_due() <= clock.t + 1 / d.WRITE_RATE + 1e-9, "after a stall, frames carry on soon")
        run(L, "set FFFFFF")


def test_caps_and_persistence() -> None:
    d.STATE_FILE = os.path.join(TMP, "state", "state.json")
    with FakeClock() as clock:
        L = d.Lights()
        for z in d.ZONES:
            L.set(z, "8800FF")
        chk(L.next_due() is None, "nothing fading -> no timeout")
        red, white = d.CAPS_COLORS
        L.set_caps(True)
        chk(leds() == {"left": red, "center": "8800FF", "right": "8800FF", "extra": "8800FF"},
            "caps on: left starts red, other zones keep their colors")
        chk(clock.t < L.next_due() <= clock.t + d.FRAME, "caps on -> a frame within FRAME seconds")

        def at(t: float) -> str:
            clock.t = 1000.0 + t * d.CAPS_DELAY
            L.tick()
            return leds()["left"]
        chk(at(0.5 * d.FRAME) == red, "no frame before FRAME is up")
        chk(at(0.5) == d.mix(red, white, 0.5), "halfway through CAPS_DELAY: halfway to white")
        chk(at(1) == white and at(1.75) == d.mix(white, red, 0.75) and at(2) == red, "fades to white and back")
        chk(at(1000.5) == d.mix(red, white, 0.5) and clock.t < L.next_due() <= clock.t + d.FRAME,
            "after a stall, frames carry on from now")
        L.set("center", "00FF00")
        chk(leds()["center"] == "00FF00", "zone colors can change while caps is on")
        L.set("left", "0000FF")
        chk(at(1000.75) == d.mix(red, white, 0.75), "left keeps fading after setting its color")
        L.set_caps(True)
        chk(L.caps_fade.start == 1000.0, "repeated caps on doesn't restart the fade")
        L.set_caps(False)
        chk(leds()["left"] == "0000FF" and L.next_due() is None, "caps off shows left's color, stops frames")
        chk(at(1001) == "0000FF", "tick with nothing fading does nothing")
        L.set(d.ALL, "8800FF")
        L.set_caps(True); L.set_caps(False)
        chk(set(leds().values()) == {"8800FF"}, "caps off restores saved colors")
        chk(json.load(open(d.STATE_FILE))["extra"] == "8800FF", "colors saved as hex")
        chk(d.Lights().color["center"] == "8800FF", "colors reload from state file")


def test_held_cycles() -> None:
    """Menu + hold a zone key + digits: a cycle, timed by the gap between the first two digits."""
    d.STATE_FILE = os.path.join(TMP, "held-state.json")
    with FakeClock() as clock:
        L = d.Lights()
        L.set(d.ALL, "FFFFFF")
        out: list[tuple[str, int]] = []
        dialogs: list[str] = []
        h = d.Handler(L, lambda c, v: out.append((e.KEY[c], v)), dialogs.append)

        def press(*keys: tuple[str, int] | float) -> None:  # a number waits that many seconds
            for k in keys:
                if isinstance(k, float):
                    clock.t += k
                    continue
                name, v = k
                if h.key(InputEvent(0, 0, e.EV_KEY, getattr(e, "KEY_" + name), v)):
                    out.append(("KEY_" + name, v))

        def cycle(zone: str) -> tuple[list[str], float] | None:
            f = L.fades.get(zone)
            return (f.colors, f.period) if f else None

        press((M, 1), ("Q", 1), ("1", 1), ("1", 0), 1.5, ("3", 1), ("3", 0))
        chk(leds()["left"] == "FFFF00" and L.fades == {}, "while the zone key is held, digits show their color")
        press(0.2, ("7", 1), ("7", 0), ("Q", 0))
        chk(cycle("left") == (["FF0000", "FFFF00", "0000FF"], 1.5) and set(L.fades) == {"left"},
            "letting go of the zone key starts the cycle; the first gap sets its time")
        press(("Q", 1))
        chk(dialogs == [] and h.sel == "left", "pressing the zone key again starts over instead of the dialog")
        press(("4", 1), ("4", 0), 0.5, ("5", 1), ("5", 0), ("Q", 0))
        chk(cycle("left") == (["00FF00", "00FFAA"], 0.5), "a new cycle replaces the old one")

        press(("Q", 1), ("Q", 0), ("1", 1), ("1", 0), 1.0, ("3", 1), ("3", 0))
        chk(L.fades == {} and leds()["left"] == "FFFF00", "digits after letting go just set the color, as before")
        press(("Q", 1), ("Q", 0))
        chk(dialogs == ["left"], "and the zone key again opens the dialog, as before"); dialogs.clear()
        press(("Q", 1), ("6", 1), ("6", 0), ("Q", 0), ("Q", 1), ("Q", 0))
        chk(L.fades == {} and leds()["left"] == "0088FF" and dialogs == ["left"],
            "one digit while holding just sets the color"); dialogs.clear()

        press(("SPACE", 1), ("2", 1), ("2", 0), 2.0, ("8", 1), ("8", 0), ("SPACE", 0))
        chk(all(cycle(z) == (["FF6600", "8800FF"], 2.0) for z in d.ZONES), "holding Space: cycle the whole keyboard")
        press(("K", 1), ("1", 1), ("1", 0), 0.01, ("2", 1), ("2", 0))
        press(("L", 1), ("K", 0))
        chk(cycle("center") == (["FF0000", "FF6600"], 0.1), "pressing another zone key finishes the cycle; time >= 0.1 s")
        press(("4", 1), ("4", 0), 100.0, ("5", 1), ("5", 0), ("L", 0))
        chk(cycle("right") == (["00FF00", "00FFAA"], 60.0), "time <= 60 s")
        press(("E", 1), ("1", 1), ("1", 0), 1.0, ("2", 1), ("2", 0), ("BACKSPACE", 1), ("BACKSPACE", 0), ("E", 0))
        chk(L.fades == {} and set(leds().values()) == {"FFFFFF"}, "Backspace while holding drops the picks")
        press(("E", 1), *[k for n in range(18) for k in (0.25, (str(n % 10), 1), (str(n % 10), 0))], ("E", 0))
        left = cycle("left")
        chk(left is not None and len(left[0]) == d.MAX_CYCLE, "at most MAX_CYCLE colors")
        press(("Z", 1), ("1", 1), ("1", 0), 1.0, ("3", 1), ("3", 0), (M, 0))
        chk(cycle("left") == (["FF0000", "FFFF00"], 1.0), "letting go of Menu first still starts the cycle")
        press(("Z", 0))
        chk(out == [], "nothing leaked to apps")


def test_dialog() -> None:
    """The Menu-key color dialog: colors, cycles, effects, and times in one line."""
    ok = {
        "FF8800": "set zone=left FF8800",
        "#ff8800": "set zone=left FF8800",
        "FF8800 2": "set zone=left time=2 FF8800",
        "FF8800 .5": "set zone=left time=0.5 FF8800",
        "FF0000 0000FF": "cycle zone=left FF0000 0000FF",
        "FF0000, 0000FF, 3": "cycle zone=left time=3 FF0000 0000FF",
        "cycle FF0000 0000FF 1.25": "cycle zone=left time=1.25 FF0000 0000FF",
        "rainbow": "rainbow zone=left",
        "Rainbow 0.3": "rainbow zone=left time=0.3",
        "pulse 00ffaa": "pulse zone=left 00FFAA",
        "pulse 00FFAA 1.5": "pulse zone=left time=1.5 00FFAA",
        "  FF8800  ": "set zone=left FF8800",
        "112233 445566": "cycle zone=left 112233 445566",
    }
    for text, line in ok.items():
        chk(d.parse_dialog(text, "left") == (line, ""), f"dialog: {text!r} -> {line}")
    chk(d.parse_dialog("FF8800 2", d.ALL)[0] == "set zone=all time=2 FF8800", "dialog for the whole keyboard")
    bad = {"": "Type a color", "zzz": "isn't a color", "FF8800 nope": "isn't a color", "2": "Type a color",
           "FF8800 61": "from 0 to 60", "FF0000 0000FF 0": "from 0.1 to 60", "rainbow FF0000": "no colors",
           "pulse": "one color", "pulse FF0000 00FF00": "one color", "cycle FF0000": "2 to 16",
           "FF0000 " * 17: "2 to 16", "<b>&": "isn't a color"}
    for text, why in bad.items():
        line, problem = d.parse_dialog(text, "left")
        chk(line is None and why in problem, f"dialog rejects {text!r}: {problem}")

    chk(d.DIALOG_HELP.isascii(), "dialog help is ASCII (zenity rejects other text outside a UTF-8 locale)")
    chk(all(c in d.DIALOG_HELP for c in d.PALETTE) and len(d.PALETTE_NAMES) == len(d.PALETTE),
        "the dialog lists every palette color")
    seen: list[list[str]] = []

    def fake_run(cmd: list[str], **kw: object) -> subprocess.CompletedProcess[str]:
        seen.append(cmd)
        return subprocess.CompletedProcess(cmd, 0, "", "")
    real_run = d.subprocess.run
    d.subprocess.run = fake_run
    try:
        d.run_as_user(pwd.getpwuid(os.getuid()).pw_name, ["zenity", "--entry"])
    finally:
        d.subprocess.run = real_run
    chk("LANG=C.UTF-8" in seen[0] and seen[0][-2:] == ["zenity", "--entry"],
        "desktop commands run with a UTF-8 locale")

    d.STATE_FILE = os.path.join(TMP, "dialog-state.json")
    with FakeClock():
        L = d.Lights()
        for line, shown in [("set zone=left 123456", "123456"), ("cycle zone=left time=2 AA00AA 00FF00", "AA00AA 00FF00 2"),
                            ("rainbow zone=left time=0.3", "rainbow 0.3"), ("pulse zone=left 00FFAA", "pulse 00FFAA 0.5"),
                            ("pulse zone=left for=5 00FFAA", "00FFAA")]:
            run(L, line)
            chk(d.describe(L, "left") == shown and d.describe(L, d.ALL) == shown, f"dialog starts from {shown!r}")
            chk(d.parse_dialog(shown, "left")[0] is not None, f"what the dialog starts from is valid: {shown!r}")

        answers = [(0, "<i>zzz"), (0, "FF0000 0000FF 61"), (0, "FF0000 0000FF 2")]
        shown_texts: list[tuple[str, ...]] = []

        def fake_zenity(user: str, *args: str) -> tuple[int, str]:
            shown_texts.append(args)
            return answers.pop(0) if "--entry" in args else (0, "")
        real = d.session_user, d.run_zenity
        d.session_user, d.run_zenity = (lambda: "someone"), fake_zenity
        try:
            run(L, "set zone=right 445566")
            d.hex_dialog(L, "right")
            entries = [a for a in shown_texts if "--entry" in a]
            errors = [a for a in shown_texts if "--error" in a]
            chk("--entry-text=445566" in entries[0] and "--entry-text=<i>zzz" in entries[1],
                "the dialog starts from the zone's color, then from what was typed")
            chk(len(errors) == 2 and "&lt;i&gt;" in " ".join(errors[0]) and "60 seconds" in " ".join(errors[1]),
                "bad input shows why (markup escaped) and asks again")
            chk(L.fades["right"].colors == ["FF0000", "0000FF"] and L.fades["right"].period == 2.0,
                "the dialog can start a cycle")
            answers[:] = [(1, "")]
            d.hex_dialog(L, "right")
            chk(L.fades["right"].period == 2.0, "cancelling changes nothing")
        finally:
            d.session_user, d.run_zenity = real


def run(L: Any, line: str) -> None:
    """Run a kbdrgb command line on L; it must be valid."""
    assert d.run_command(L, line), line


def test_commands() -> None:
    """Parsing the command lines kbdrgb sends."""
    d.STATE_FILE = os.path.join(TMP, "cmd-state.json")
    with FakeClock():
        L = d.Lights()
        good = ["set FF0000", "set zone=left 00ff00", "set zone=all time=2.5 #0000FF", "set time=0 112233",
                "cycle AA00AA 00FF00", "cycle zone=center time=0.1 for=0.1 AA00AA 00FF00 0000FF",
                "rainbow", "rainbow zone=extra time=60 for=86400", "pulse zone=right time=1.5 00FFAA"]
        for line in good:
            chk(d.run_command(L, line), f"valid: {line}")
        bad = ["", "nope", "set", "set FF0000 00FF00", "set for=3 FF0000", "set zone=top FF0000",
               "set time=61 FF0000", "set time=-1 FF0000", "set time=abc FF0000", "set time=1e3 FF0000",
               "set time=nan FF0000", "set color=FF0000", "cycle AA00AA", "cycle " + "AA00AA " * 17,
               "cycle time=0 AA00AA 00FF00", "cycle time=0.05 AA00AA 00FF00", "cycle for=0 AA00AA 00FF00",
               "rainbow FF0000", "rainbow for=86401", "pulse", "pulse zzzzzz", "pulse FF0000 00FF00",
               "rainbow zone=", "rainbow time=", "sync", "stop"]
        for line in bad:
            chk(not d.run_command(L, line), f"rejected: {line!r}")
        d.handle_messages(L, b"set zone=left 010101\n\nbogus\n\xff\xfe\nset zone=right 020202\n")
        chk(L.color["left"] == "010101" and L.color["right"] == "020202", "several lines in one read, bad ones skipped")


def test_cycles() -> None:
    d.STATE_FILE = os.path.join(TMP, "cycle-state.json")
    if os.path.exists(d.STATE_FILE):
        os.unlink(d.STATE_FILE)

    def cycles(L: Any) -> dict[str, list[str]]:
        return {z: f.colors for z, f in L.fades.items()}

    with FakeClock() as clock:
        L = d.Lights()
        run(L, "set 8800FF")

        def at(t: float) -> dict[str, str]:  # t: seconds since 1000.0
            clock.t = 1000.0 + t
            draw_all(L)
            return leds()

        run(L, "cycle zone=center AA00AA 00FF00")
        chk(leds()["center"] == "AA00AA" and leds()["left"] == "8800FF", "cycle on one zone")
        chk(clock.t < L.next_due() <= clock.t + d.FRAME, "cycling -> a frame within FRAME seconds")
        chk(at(0.5 * d.CYCLE_DELAY)["center"] == d.mix("AA00AA", "00FF00", 0.5), "fades halfway in half a CYCLE_DELAY")
        chk(at(d.CYCLE_DELAY)["center"] == "00FF00" and at(2 * d.CYCLE_DELAY)["center"] == "AA00AA",
            "reaches the second color and fades back")
        chk(leds()["left"] == "8800FF", "other zones stay put")
        chk(json.load(open(d.STATE_FILE))["fades"] ==
            {"center": {"colors": ["AA00AA", "00FF00"], "period": d.CYCLE_DELAY, "offset": 0.0}}, "cycle saved")
        L2 = d.Lights()
        chk(cycles(L2) == {"center": ["AA00AA", "00FF00"]} and L2.shown("center") == L.shown("center"),
            "cycle resumes after restart, in step with where it was")

        # Same colors and time, started at different times: in step. Different time: its own pace.
        clock.t = 1002.37
        run(L, "cycle zone=right AA00AA 00FF00")
        draw_all(L)
        chk(leds()["right"] == leds()["center"], "a cycle started later joins in step with an equal one")
        for t in (3.2, 3.9, 4.6):
            at(t)
            chk(leds()["right"] == leds()["center"], f"equal cycles stay in step ({t}s)")
        run(L, "cycle zone=left time=3 AA00AA 00FF00")
        slow, fast = (d.Fade(["AA00AA", "00FF00"], p, d.EPOCH) for p in (3.0, d.CYCLE_DELAY))
        at(5.3)
        chk(leds()["left"] == slow.at(clock.t) and leds()["center"] == fast.at(clock.t)
            and leds()["left"] != leds()["center"], "each zone keeps its own transition time")

        run(L, "set zone=center 0000FF")
        chk(leds()["center"] == "0000FF" and set(L.fades) == {"left", "right"}, "a zone's new color ends only its effect")
        run(L, "set FFFFFF")
        chk(L.fades == {} and L.next_due() is None and set(leds().values()) == {"FFFFFF"},
            "setting all zones ends all effects")

        run(L, "cycle zone=left AA00AA 00FF00")
        L.set_caps(True)
        red, white = d.CAPS_COLORS
        clock.t += 0.5 * d.CAPS_DELAY
        L.tick()
        chk(leds()["left"] == d.mix(red, white, 0.5), "caps lock fade wins over a left-zone cycle")
        L.set_caps(False)
        chk(leds()["left"] == L.fades["left"].at(clock.t), "left cycle shows again when caps lock turns off")
        run(L, "set FFFFFF")

        good = {"colors": ["AA00AA", "00FF00"], "period": 1.0}
        json.dump({"left": "FFFFFF", "fades": {
            "left": {"colors": ["AA00AA"], "period": 1.0}, "nowhere": good,
            "center": {**good, "period": 0}, "right": {**good, "offset": 5}, "extra": {**good, "period": "x"}}},
            open(d.STATE_FILE, "w"))
        chk(d.Lights().fades == {}, "bad effects in the state file are ignored")
        json.dump({"left": "FFFFFF", "fades": {"right": {**good, "offset": 1.5}}}, open(d.STATE_FILE, "w"))
        chk(d.Lights().fades["right"].offset == 1.5, "saved effect keeps its offset")
        json.dump({"left": "FFFFFF", "fades": ["junk"]}, open(d.STATE_FILE, "w"))
        chk(d.Lights().color["left"] == "FFFFFF", "malformed fades entry doesn't break loading")


def test_transitions() -> None:
    """set with a time: fade from what the zone shows to the new color."""
    d.STATE_FILE = os.path.join(TMP, "transition-state.json")
    with FakeClock() as clock:
        L = d.Lights()
        run(L, "set 000000")
        start = clock.t
        run(L, "set zone=left time=2 FFFFFF")
        chk(leds()["left"] == "000000" and leds()["center"] == "000000", "transition starts from the current color")
        chk(json.load(open(d.STATE_FILE))["left"] == "FFFFFF", "the new color is saved right away")
        clock.t = start + 1
        L.tick()
        chk(leds()["left"] == "808080", "halfway through the time, halfway there")
        run(L, "set zone=right time=0.5 FF0000")
        clock.t = start + 1.5
        L.tick()
        chk(leds()["left"] == "BFBFBF" and leds()["right"] == "FF0000", "zones fade with their own times")
        clock.t = start + 2.05
        L.tick()
        chk(leds()["left"] == "FFFFFF" and L.fades == {} and L.next_due() is None, "ends exactly on the new color")

        run(L, "cycle zone=center 0000FF 00FF00")
        frame = L.shown("center")
        run(L, "set zone=center time=1 FF0000")
        chk(leds()["center"] == frame, "transition out of an effect starts from its current frame")
        clock.t += 1.05
        L.tick()
        chk(leds()["center"] == "FF0000" and "center" not in L.fades, "and ends on the new color")
        run(L, "set zone=left time=1 00FF00")
        run(L, "set zone=left 0000FF")
        chk(leds()["left"] == "0000FF" and L.fades == {}, "an instant color cuts a transition short")


def test_effects() -> None:
    """rainbow and pulse: endless or timed, on any zone, cleared by new colors."""
    d.STATE_FILE = os.path.join(TMP, "effect-state.json")
    with FakeClock() as clock:
        L = d.Lights()
        run(L, "set FFFFFF")
        run(L, "rainbow")
        expect = {z: d.Fade(d.RAINBOW, d.RAINBOW_DELAY, d.EPOCH, i * d.RAINBOW_LAG).at(clock.t)
                  for i, z in enumerate(d.ZONES)}
        chk(leds() == expect and len(set(expect.values())) == 4, "rainbow: each zone a step further around the wheel")
        before = leds()
        clock.t += 6 * d.RAINBOW_DELAY
        draw_all(L)
        chk(leds() == before, "rainbow loops around the wheel")
        chk(len(json.load(open(d.STATE_FILE))["fades"]) == 4, "endless rainbow saved, to resume after a restart")
        run(L, "set zone=center 123456")
        chk(set(L.fades) == {"left", "right", "extra"}, "a new zone color clears the rainbow there")
        run(L, "rainbow zone=center")
        chk(set(L.fades) == set(d.ZONES), "rainbow on one zone")
        chk(L.fades["center"].offset == d.RAINBOW_LAG, "a one-zone rainbow keeps the zone's place on the wheel")
        run(L, "set FFFFFF")

        run(L, "pulse zone=extra time=1 for=3 00ff88")
        start = clock.t
        dim = d.mix("000000", "00FF88", d.PULSE_DIM)
        pulse = d.Fade(["00FF88", dim], 1.0, d.EPOCH)
        chk(leds()["extra"] == pulse.at(start) and leds()["left"] == "FFFFFF", "pulse on one zone")
        clock.t = start + 0.5
        L.tick()
        chk(leds()["extra"] == pulse.at(clock.t), "pulse fades toward a dim copy")
        chk(json.load(open(d.STATE_FILE))["fades"] == {}, "timed effects aren't saved")
        clock.t = start + 2.9
        L.tick()
        chk(L.fades != {}, "timed pulse still running before its time is up")
        clock.t = start + 3.2
        L.tick()
        last = pulse.at(start + 3)
        chk(L.fades == {} and L.next_due() is None and leds()["extra"] == last
            and json.load(open(d.STATE_FILE))["extra"] == last,
            "timed pulse ends on the frame at its end time, and that color is saved")

        run(L, "rainbow for=2")
        clock.t += 1
        run(L, "cycle zone=center AA00AA 00FF00")
        clock.t += 1.5
        L.tick()
        chk(set(L.fades) == {"center"}, "timed rainbow ends; an endless cycle started on top keeps going")
        run(L, "set FFFFFF")


class FakeFd:
    """A fake event source backed by a real pipe, so select() really waits on it."""

    def __init__(self) -> None:
        self.fd, self._w = os.pipe()
        self._q: list[InputEvent] = []
        self._dead, self._lock = False, threading.Lock()

    def push(self, *evs: InputEvent) -> None:
        with self._lock:
            self._q += evs
        os.write(self._w, b"x")

    def kill(self) -> None:  # next read raises, like a device that disappeared
        self._dead = True
        os.write(self._w, b"x")

    def read(self) -> Iterator[InputEvent]:
        os.read(self.fd, 4096)
        if self._dead:
            raise OSError("device gone")
        with self._lock:
            evs, self._q = self._q, []
        return iter(evs)


def open_fds() -> dict[int, str]:
    """This process's open fds and what they point to."""
    out = {}
    for name in os.listdir("/proc/self/fd"):
        try:
            out[int(name)] = os.readlink(f"/proc/self/fd/{name}")
        except OSError:
            pass  # the fd listdir itself used
    return out


def fds_to(path: str) -> list[int]:
    return [fd for fd, target in open_fds().items() if target == os.path.realpath(path)]


def wait_for(cond: Callable[[], object], msg: str, timeout: float = 3.0) -> None:
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if cond():
            return chk(True, msg)
        time.sleep(0.01)
    chk(False, msg)


def test_main_loop() -> None:
    """main(): real select() loop over keyboard, virtual keyboard LEDs, and the notify pipe."""
    d.STATE_FILE = os.path.join(TMP, "main-state.json")
    d.NOTIFY = os.path.join(TMP, "run", "notify")
    d.CAPS_GLOB = os.path.join(TMP, "no-such-caps-led-*")
    d.KBD = os.path.join(TMP, "fake-kbd")
    open(d.KBD, "w").close()
    for z in d.ZONES:
        open(f"{d.LED}/color_{z}", "w").write("FFFFFF")
    calls, keys, errors = [], [], []

    class Dev(FakeFd):
        path, name = "/dev/fake", "fake"
        def grab(self) -> None: calls.append("grab")
        def ungrab(self) -> None: calls.append("ungrab")
        def close(self) -> None: calls.append("dev-close")

    class UI(FakeFd):
        device = types.SimpleNamespace(path="/dev/fakeui")
        def write_event(self, ev: InputEvent) -> None:
            assert ev.type != e.EV_MSC, "scancode forwarded"
            if ev.type == e.EV_KEY:
                keys.append((e.KEY[ev.code], ev.value))
        def write(self, t: int, c: int, v: int) -> None: keys.append((e.KEY[c], v))
        def syn(self) -> None: pass
        def close(self) -> None: calls.append("ui-close")

    dev, ui = Dev(), UI()
    real_dev, real_from = d.evdev.InputDevice, d.evdev.UInput.from_device
    d.evdev.InputDevice = lambda p: dev
    d.evdev.UInput.from_device = staticmethod(lambda _dev, name=None: ui)

    exits = []

    def run() -> None:
        try:
            d.main()
        except SystemExit as ex:
            exits.append(ex.code)  # fake device "disappeared"
        except Exception as ex:
            errors.append(ex)

    t = threading.Thread(target=run, daemon=True)
    real_stable, real_caps, real_cycle = d.STABLE_AFTER, d.CAPS_DELAY, d.CYCLE_DELAY
    d.STABLE_AFTER, d.CAPS_DELAY, d.CYCLE_DELAY = 0.3, 0.5, 0.5
    os.makedirs(os.path.dirname(d.PROBLEM_FILE), exist_ok=True)
    json.dump({"key": "not-found:x", "count": 5, "notified": True}, open(d.PROBLEM_FILE, "w"))
    NOTIFIED.clear()
    try:
        t.start()
        wait_for(lambda: "grab" in calls, "main grabs the keyboard")
        chk(len(fds_to(d.NOTIFY)) == 1, "notify pipe open while running")
        st = os.stat(d.NOTIFY)
        chk(stat.S_ISFIFO(st.st_mode) and stat.S_IMODE(st.st_mode) == 0o660
            and st.st_gid == os.stat(f"{d.LED}/color_left").st_gid,
            "notify pipe created, group read+write for the color files' group")

        def K(c: int, v: int) -> list[InputEvent]:  # one key event as the kernel sends it
            return [InputEvent(0, 0, e.EV_MSC, e.MSC_SCAN, 99),
                    InputEvent(0, 0, e.EV_KEY, c, v), InputEvent(0, 0, e.EV_SYN, 0, 0)]
        dev.push(*K(e.KEY_A, 1), *K(e.KEY_A, 0), *K(e.KEY_COMPOSE, 1), *K(e.KEY_Q, 1), *K(e.KEY_Q, 0),
                 *K(e.KEY_3, 1), *K(e.KEY_3, 0), *K(e.KEY_COMPOSE, 0), *K(e.KEY_B, 1), *K(e.KEY_B, 0))
        wait_for(lambda: keys == [("KEY_A", 1), ("KEY_A", 0), ("KEY_B", 1), ("KEY_B", 0)],
                 "keys forwarded, Menu+Q+3 swallowed, scancodes dropped")
        chk(leds()["left"] == "FFFF00", "Menu+Q+3 set left yellow")

        ui.push(InputEvent(0, 0, e.EV_LED, e.LED_CAPSL, 1))
        red, white = d.CAPS_COLORS
        wait_for(lambda: leds()["left"] == red and leds()["center"] == "FFFFFF",
                 "caps LED event on virtual keyboard -> left red, others unchanged")
        wait_for(lambda: 0x20 < green("left") < 0xE0, "main loop draws in-between fade frames")
        wait_for(lambda: green("left") >= 0xE0, "left fades to white")
        wait_for(lambda: green("left") <= 0x20, "and back to red")
        ui.push(InputEvent(0, 0, e.EV_LED, e.LED_NUML, 1))
        ui.push(InputEvent(0, 0, e.EV_LED, e.LED_CAPSL, 0))
        wait_for(lambda: leds()["left"] == "FFFF00", "caps off event -> colors restored (num lock ignored)")

        env = dict(os.environ, KBDRGB_LED=d.LED, KBDRGB_NOTIFY=d.NOTIFY)
        subprocess.run([os.path.join(HERE, "kbdrgb"), "zones", "112233", "445566", "778899", "AABBCC"],
                       env=env, check=True, timeout=5)
        wait_for(lambda: os.path.exists(d.STATE_FILE) and json.load(open(d.STATE_FILE)) ==
                 {"left": "112233", "center": "445566", "right": "778899", "extra": "AABBCC", "fades": {}},
                 "real kbdrgb zones goes through the daemon, which saves the colors")
        chk(leds()["right"] == "778899", "and writes them")

        subprocess.run([os.path.join(HERE, "kbdrgb"), "cycle", "AA00AA", "00ff00", "-z", "center"],
                       env=env, check=True, timeout=5)
        wait_for(lambda: green("center") <= 0x20, "real kbdrgb cycle starts the cycle")
        wait_for(lambda: green("center") >= 0xE0, "main loop fades to the second color")
        wait_for(lambda: green("center") <= 0x20, "and back")
        subprocess.run([os.path.join(HERE, "kbdrgb"), "color", "445566"], env=env, check=True, timeout=5)
        wait_for(lambda: json.load(open(d.STATE_FILE)) == {**{z: "445566" for z in d.ZONES}, "fades": {}},
                 "real kbdrgb color ends the cycle and its color is saved")
        time.sleep(3 * d.CYCLE_DELAY)
        chk(set(leds().values()) == {"445566"}, "no cycle frame lands after kbdrgb color")
        subprocess.run([os.path.join(HERE, "kbdrgb"), "color", "000000", "-z", "left", "-t", "1"],
                       env=env, check=True, timeout=5)
        wait_for(lambda: leds()["left"] not in ("", "445566", "000000"), "kbdrgb color --time fades")
        wait_for(lambda: leds()["left"] == "000000", "and ends on the new color")

        wait_for(lambda: len(NOTIFIED) == 1 and "working again" in NOTIFIED[0][0],
                 "after running STABLE_AFTER seconds, a reported problem counts as recovered")
        dev.kill()
        t.join(3)
        chk(not t.is_alive() and not errors and exits == [1], "main exits with error when the keyboard goes away")
        chk(d._load_problem().get("key") == "crash:OSError", "keyboard going away is recorded as a problem")
        chk(calls == ["grab", "ungrab", "ui-close", "dev-close"], "main ungrabs and closes on exit, in reverse order")
        chk(fds_to(d.NOTIFY) == [], "main closes the notify pipe on exit")
    finally:
        d.evdev.InputDevice, d.evdev.UInput.from_device = real_dev, real_from
        d.STABLE_AFTER, d.CAPS_DELAY, d.CYCLE_DELAY = real_stable, real_caps, real_cycle
        if os.path.exists(d.PROBLEM_FILE):
            os.unlink(d.PROBLEM_FILE)


def test_grab_failure() -> None:
    """A failed grab still closes everything opened before it."""
    d.STATE_FILE = os.path.join(TMP, "grab-state.json")
    d.NOTIFY = os.path.join(TMP, "run3", "notify")
    d.CAPS_GLOB = os.path.join(TMP, "no-such-caps-led-*")
    real_kbd, d.KBD = d.KBD, os.path.join(TMP, "fake-kbd")
    open(d.KBD, "w").close()
    calls: list[str] = []

    class Dev(FakeFd):
        def grab(self) -> None: raise OSError("device busy")
        def close(self) -> None: calls.append("dev-close")

    class UI(FakeFd):
        def close(self) -> None: calls.append("ui-close")

    real_dev, real_from, real_sleep = d.evdev.InputDevice, d.evdev.UInput.from_device, d.time.sleep
    d.evdev.InputDevice = lambda p: Dev()
    d.evdev.UInput.from_device = staticmethod(lambda _dev, name=None: UI())
    d.time.sleep = lambda s: None
    try:
        try:
            d.main()
            chk(False, "main exits when the grab fails")
        except SystemExit as ex:
            chk(ex.code == 1, "main exits with error when the grab fails")
        chk(calls == ["ui-close", "dev-close"], "failed grab closes the virtual and real keyboard")
        chk(fds_to(d.NOTIFY) == [], "failed grab closes the notify pipe")
        chk(d._load_problem().get("key") == "crash:OSError", "failed grab is recorded as a problem")
    finally:
        d.evdev.InputDevice, d.evdev.UInput.from_device, d.time.sleep = real_dev, real_from, real_sleep
        d.KBD = real_kbd
        if os.path.exists(d.PROBLEM_FILE):
            os.unlink(d.PROBLEM_FILE)


def test_find_keyboard() -> None:
    path = os.path.join(TMP, "fake-event")
    open(path, "w").close()
    chk(d.find_keyboard(path) == os.path.realpath(path), "find_keyboard: existing path")
    chk(d.find_keyboard(os.path.join(TMP, "missing-event")) is None, "find_keyboard: missing path -> None")

    class Dev:
        def __init__(self, p: str) -> None:
            if p == "/dev/input/event9":
                raise OSError("permission denied")
            self.name = {"/dev/input/event3": "AT Translated Set 2 keyboard",
                         "/dev/input/event7": "keyd virtual keyboard"}[p]
        def close(self) -> None: pass
    real_list, real_dev = d.evdev.list_devices, d.evdev.InputDevice
    d.evdev.list_devices = lambda: ["/dev/input/event9", "/dev/input/event3", "/dev/input/event7"]
    d.evdev.InputDevice = Dev
    try:
        chk(d.find_keyboard("keyd virtual keyboard") == "/dev/input/event7", "find_keyboard: by device name")
        chk(d.find_keyboard("nope") is None, "find_keyboard: unknown name -> None")
    finally:
        d.evdev.list_devices, d.evdev.InputDevice = real_list, real_dev

    real_kbd = d.KBD
    fds_before = open_fds()
    d.STATE_FILE = os.path.join(TMP, "find-state.json")
    d.NOTIFY = os.path.join(TMP, "run2", "notify")
    d.CAPS_GLOB = os.path.join(TMP, "no-such-caps-led-*")
    try:
        for spec, what in [(os.path.join(TMP, "missing-event"), "missing keyboard"), (d.VIRTUAL_NAME, "own virtual keyboard")]:
            d.KBD = spec
            try:
                d.main()
                chk(False, f"main exits for {what}")
            except SystemExit as ex:
                chk(ex.code == 1, f"main exits with error for {what} (systemd retries)")
        chk(open_fds() == fds_before, "early exits leave no fds open")
    finally:
        d.KBD = real_kbd


def test_problem_reports() -> None:
    NOTIFIED.clear()
    for i in range(1, 3):
        d.report_problem("not-found:x", "Can't find x.")
    chk(NOTIFIED == [], "no notification for the first 2 failed starts")
    d.report_problem("not-found:x", "Can't find x.")
    chk(len(NOTIFIED) == 1 and "Can't find x." in NOTIFIED[0][1], "notify on the 3rd failed start in a row")
    d.report_problem("not-found:x", "Can't find x.")
    chk(len(NOTIFIED) == 1, "only one notification per problem")
    d.report_recovered()
    chk(len(NOTIFIED) == 2 and "working again" in NOTIFIED[1][0] and not os.path.exists(d.PROBLEM_FILE),
        "recovery notification after a reported problem, marker cleared")
    d.report_recovered()
    chk(len(NOTIFIED) == 2, "no recovery notification when nothing was wrong")

    NOTIFIED.clear()
    d.report_problem("crash:OSError", "a"); d.report_problem("crash:OSError", "a")
    d.report_problem("not-found:y", "b"); d.report_problem("not-found:y", "b")
    chk(NOTIFIED == [], "a different problem restarts the count")
    d.report_problem("not-found:y", "b")
    chk(len(NOTIFIED) == 1, "new problem notifies after its own 3 failures")
    d.report_recovered(); NOTIFIED.clear()

    real = d.desktop_notify
    shown: list[str] = []
    d.desktop_notify = lambda s, b: False  # nobody logged in yet (e.g. at boot)
    try:
        for _ in range(4):
            d.report_problem("not-found:z", "c")
        def user_logged_in(summary: str, body: str) -> bool:
            shown.append(summary)
            return True
        d.desktop_notify = user_logged_in
        d.report_problem("not-found:z", "c")
        chk(len(shown) == 1, "notification retried until someone is logged in to see it")
        d.report_recovered()
    finally:
        d.desktop_notify = real
    d.report_problem("crash:X", "q"); d.report_recovered()
    chk(not os.path.exists(d.PROBLEM_FILE) and NOTIFIED == [], "unreported problem clears silently on recovery")


def test_kbdrgb_cli() -> None:
    """kbdrgb's options and what it sends the daemon, against a stand-in reader."""
    kbdrgb = os.path.join(HERE, "kbdrgb")
    fifo = os.path.join(TMP, "cli-notify")
    os.mkfifo(fifo, 0o600)
    env = dict(os.environ, KBDRGB_LED=d.LED, KBDRGB_NOTIFY=fifo)

    def k(*args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run([kbdrgb, *args], env=env, timeout=5, capture_output=True, text=True)

    rd = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)  # stand in for the daemon reading the pipe
    try:
        for args in (["color", "ff0000"], ["color", "#00FF00", "-z", "left", "-t", "2"],
                     ["zones", "111111", "222222", "--time=0.5"], ["cycle", "AA00AA", "00FF00", "--zone", "center"],
                     ["cycle", "AA00AA", "00FF00", "0000FF", "-t", "2", "-d", "30"], ["rainbow"],
                     ["rainbow", "-z", "extra", "--for", "20"], ["pulse", "00ffaa", "--zone=right", "--time", "1.5"]):
            out = k(*args)
            chk(out.returncode == 0 and out.stderr == "", f"kbdrgb {' '.join(args)}")
        chk(os.read(rd, 65536).decode().splitlines() == [
            "set FF0000", "set zone=left time=2 00FF00",
            "set zone=left time=0.5 111111", "set zone=center time=0.5 222222",
            "set zone=right time=0.5 FFFFFF", "set zone=extra time=0.5 FFFFFF",
            "cycle zone=center AA00AA 00FF00", "cycle time=2 for=30 AA00AA 00FF00 0000FF",
            "rainbow", "rainbow zone=extra for=20", "pulse zone=right time=1.5 00FFAA"],
            "kbdrgb sends each command to the daemon")
        with FakeClock():
            L = d.Lights()
            for line in ["set FF0000", "set zone=left time=2 00FF00", "cycle time=2 for=30 AA00AA 00FF00 0000FF",
                         "rainbow zone=extra for=20", "pulse zone=right time=1.5 00FFAA"]:
                chk(d.run_command(L, line), f"the daemon accepts what kbdrgb sends: {line}")
        for args in (["color"], ["color", "FF0000", "-d", "3"], ["zones", "-z", "left", "FF0000"],
                     ["zones", "1", "2", "3", "4", "5"], ["cycle", "AA00AA"], ["cycle", *["AA00AA"] * 17],
                     ["cycle", "AA00AA", "zzz"], ["rainbow", "FF0000"], ["rainbow", "-t", "0"], ["rainbow", "-t", "61"],
                     ["rainbow", "-d", "0.05"], ["pulse", "FF0000", "-z", "top"], ["color", "FF0000", "-t", "abc"],
                     ["color", "FF0000", "-t", "-1"], ["color", "FF0000", "-q"], ["cycle", "AA00AA", "00FF00", "-t"],
                     ["brightness", "300"], ["brightness"], ["status", "-z", "left"], ["bogus"]):
            out = k(*args)
            chk(out.returncode == 1 and out.stderr != "", f"kbdrgb rejects {' '.join(args)}")
        chk(_pending(rd) == b"", "rejected commands send nothing")
        out = k("-h")
        chk(out.returncode == 0 and "--zone" in out.stdout, "kbdrgb -h shows usage")
    finally:
        os.close(rd)

    # No daemon: the pipe is missing, or there but nobody reads it.
    for notify, what in [(os.path.join(TMP, "missing", "notify"), "no pipe"), (fifo, "nobody reading the pipe")]:
        env["KBDRGB_NOTIFY"] = notify
        t0 = time.monotonic()
        out = k("color", "ABCDEF")
        chk(out.returncode == 0 and set(leds().values()) == {"ABCDEF"} and time.monotonic() - t0 < 2,
            f"kbdrgb color works without the daemon ({what})")
        chk(k("color", "123123", "-z", "right").returncode == 0 and leds()["right"] == "123123"
            and leds()["left"] == "ABCDEF", f"kbdrgb color --zone without the daemon ({what})")
        chk(k("zones", "010101", "020202").returncode == 0
            and leds() == {"left": "010101", "center": "020202", "right": "FFFFFF", "extra": "FFFFFF"},
            f"kbdrgb zones without the daemon ({what})")
        for args in (["rainbow"], ["cycle", "AA00AA", "00FF00"], ["pulse", "FF0000"], ["color", "FF0000", "-t", "1"]):
            out = k(*args)
            chk(out.returncode == 1 and "daemon" in out.stderr, f"kbdrgb {' '.join(args)} says it needs the daemon ({what})")
    for mode, what in [(0o200, "write-only"), (0o000, "no access")]:
        os.chmod(fifo, mode)
        out = k("color", "321321")
        chk(out.returncode == 0 and out.stderr == "" and leds()["left"] == "321321",
            f"kbdrgb color works when it can't use the pipe ({what})")
    out = k("zones", "999999", "nope")
    chk(out.returncode == 1 and leds()["left"] != "999999", "kbdrgb zones checks every color before writing any")
    chk(k("brightness", "100").returncode == 0 and open(f"{d.LED}/brightness").read() == "100", "kbdrgb brightness")
    out = k("status")
    chk(out.returncode == 0 and "color_left: 321321" in out.stdout and "brightness: 100" in out.stdout, "kbdrgb status")


def _pending(fd: int) -> bytes:
    """What's waiting in a non-blocking pipe (b"" if nothing, even with no writers left)."""
    try:
        return os.read(fd, 65536)
    except BlockingIOError:
        return b""


if __name__ == "__main__":
    import shutil
    try:
        test_state_migration()
        test_parse_hex()
        test_session_user()
        test_handler()
        test_held_cycles()
        test_fade()
        test_frame_slots()
        test_caps_and_persistence()
        test_commands()
        test_dialog()
        test_cycles()
        test_transitions()
        test_effects()
        test_main_loop()
        test_grab_failure()
        test_find_keyboard()
        test_problem_reports()
        test_kbdrgb_cli()
        print("ALL OK")
    finally:
        shutil.rmtree(TMP, ignore_errors=True)
