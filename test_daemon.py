#!/usr/bin/python3 -I
"""Tests for kbd_rgb_daemon.py using fake LED files and fake input devices.

Run: python3 -I test_daemon.py   (no root, no hardware needed; touches only a temp dir)
"""
from __future__ import annotations

import contextlib, dataclasses, io, json, os, pwd, shlex, stat, subprocess, sys, tempfile, threading, time, types
from unittest import mock
import evdev
from collections.abc import Callable, Iterator
from typing import Any
from evdev import ecodes as e, InputEvent

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # python3 -I leaves the script's directory off the path
import kbd_rgb_daemon as d  # noqa: E402

TMP = tempfile.mkdtemp(prefix="kbd-rgb-test-")
LED = os.path.join(TMP, "led")
os.makedirs(LED)
for z in d.ZONES:
    open(f"{LED}/color_{z}", "w").write("FFFFFF")
open(f"{LED}/brightness", "w").write("255")

# The daemon's settings for tests: its defaults, with files in TMP.
CONFIG = d.Config(led=LED, state_file=os.path.join(TMP, "state.json"), socket=os.path.join(TMP, "run", "socket"),
                  problem_file=os.path.join(TMP, "run", "problem.json"),
                  caps_glob=os.path.join(TMP, "no-such-caps-led-*"))


def config(**changes: Any) -> d.Config:
    return dataclasses.replace(CONFIG, **changes)


def argv(c: d.Config) -> list[str]:
    """The daemon's command line for c: an option for each setting that isn't the default."""
    defaults = d.Config()
    return [w for f in dataclasses.fields(c) if getattr(c, f.name) != getattr(defaults, f.name)
            for w in ("--" + f.name.replace("_", "-"), f.metadata["text"](getattr(c, f.name)))]


M = "COMPOSE"  # the Menu key

# Never show real desktop notifications from tests; record them instead.
NOTIFIED: list[tuple[str, str]] = []


def record_notification(summary: str, body: str) -> bool:
    NOTIFIED.append((summary, body))
    return True


d.desktop_notify = record_notification


def chk(cond: object, msg: str) -> None:
    assert cond, msg
    print("ok:", msg)


def leds() -> dict[str, str]:
    return {z: open(f"{LED}/color_{z}").read() for z in d.ZONES}


def test_state_migration() -> None:
    C = config(state_file=os.path.join(TMP, "old.json"))
    json.dump({"left": 7, "center": 0, "right": 0, "extra": 0}, open(C.state_file, "w"))
    chk(d.Lights(C).scene["left"] == "0000FF", "old palette-index state migrates to hex")


def test_parse_hex() -> None:
    for s, want in [("#ff8800", "FF8800"), ("f80", "FF8800"), (" 00aaff\n", "00AAFF"),
                    ("xyz", None), ("12345", None), (None, None)]:
        chk(d.parse_hex(s) == want, f"parse_hex({s!r}) == {want!r}")


def test_session_user() -> None:
    def fake(replies: list[str]) -> Callable[..., Any]:
        it = iter(replies)
        return lambda *a, **k: types.SimpleNamespace(stdout=next(it))
    with mock.patch.object(subprocess, "run", fake(["3\n", "root\n"])):
        chk(d.session_user() == "root", "session_user reads active seat0 session's user")
    with mock.patch.object(subprocess, "run", fake(["\n"])):
        chk(d.session_user() is None, "session_user: no active session -> None")
    with mock.patch.object(subprocess, "run", fake(["3\n", "no-such-user-xyz\n"])):
        chk(d.session_user() is None, "session_user: unknown user -> None")


def test_handler() -> None:
    C = config(state_file=os.path.join(TMP, "state", "state.json"))
    L = d.Lights(C)
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
    chk(leds()["left"] == "0000FF" and h.sel is not None and h.sel.target == "left", "7 retries color on left (top-row digits never select)")
    press(("9", 1), ("9", 0))
    chk(leds()["left"] == "FF00AA" and leds()["right"] == "FFFFFF", "9 is a color, not the right zone")
    press(("K", 1), ("K", 0), ("1", 1), ("1", 0))
    chk(leds()["center"] == "FF0000", "K selects center, 1 -> red")
    press(("KP6", 1), ("KP6", 0))
    chk(h.sel is not None and h.sel.target == "extra" and leds()["center"] == "FF0000", "numpad 6 selects numpad zone (not a color)")
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

    before = leds()
    press((M, 1), ("Q", 1), ("Q", 0), ("9", 1), ("9", 0))
    chk(leds()["left"] == "FF00AA", "Q, 9 -> left pink")
    press(("BACKSPACE", 1), ("BACKSPACE", 0))
    chk(leds() == before and h.sel is None, "Menu+Backspace undoes it")
    press(("LEFTSHIFT", 1), ("BACKSPACE", 1), ("BACKSPACE", 0), ("LEFTSHIFT", 0))
    chk(leds()["left"] == "FF00AA", "Menu+Shift+Backspace redoes it")
    press(("BACKSPACE", 1), ("BACKSPACE", 0))
    chk(leds() == before, "Backspace after Shift is let go undoes again")
    press((M, 0), ("RIGHTSHIFT", 1), (M, 1), ("BACKSPACE", 1), ("BACKSPACE", 0), (M, 0), ("RIGHTSHIFT", 0))
    chk(leds()["left"] == "FF00AA", "Shift held before Menu works too")
    chk(out[-2:] == [("KEY_RIGHTSHIFT", 1), ("KEY_RIGHTSHIFT", 0)], "and that Shift still reaches apps"); out.clear()
    press((M, 1), ("BACKSPACE", 1), ("BACKSPACE", 0))
    press(("3", 1), ("3", 0))
    chk(set(leds().values()) == {"FFFF00"}, "after undo, selection cleared: 3 -> whole keyboard")

    press((M, 0), (M, 1), ("1", 1), ("1", 0), (M, 0))
    before = leds()
    press((M, 1), ("2", 1), ("2", 0), ("Q", 1), ("Q", 0), ("4", 1), ("4", 0), ("K", 1), ("K", 0), ("7", 1), ("7", 0), (M, 0))
    chk(leds() == {"left": "00FF00", "center": "0000FF", "right": "FF6600", "extra": "FF6600"}, "several changes in one Menu hold")
    press((M, 1), ("BACKSPACE", 1), ("BACKSPACE", 0), (M, 0))
    chk(leds() == before, "one undo takes back everything from one Menu hold")
    press((M, 1), ("BACKSPACE", 1), ("BACKSPACE", 0), (M, 0))
    chk(set(leds().values()) == {"FFFF00"}, "and the next undo the hold before it")
    out.clear()
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
    curves = list(d.EASES.values()) + [(0.4, 0.0, 0.2, 1.0), (1.0, 0.0, 0.0, 1.0), (0.0, 1.0, 1.0, 0.0)]
    chk(all(abs(d.ease(c, 0)) < 1e-6 and abs(d.ease(c, 1) - 1) < 1e-6 for c in curves), "ease: starts at 0, ends at 1")
    chk(all(d.ease(d.LINEAR, x) == x for x in (0, 0.1, 0.5, 0.93)), "linear ease changes nothing")
    xs = [i / 20 for i in range(21)]
    chk(all(all(d.ease(c, a) <= d.ease(c, b) + 1e-9 for a, b in zip(xs, xs[1:])) for c in curves),
        "ease never goes backwards")
    chk(abs(d.ease(d.EASES["ease-in-out"], 0.5) - 0.5) < 1e-6, "ease-in-out is halfway at half time")
    chk(d.ease(d.EASES["ease-in"], 0.3) < 0.3 < d.ease(d.EASES["ease-out"], 0.3), "ease-in starts slow, ease-out fast")
    chk(abs(d.ease(d.EASES["ease"], 0.25) - 0.4085) < 0.002, "matches CSS: ease at 25% is about 41%")
    chk(d.parse_ease("ease-out") == d.EASES["ease-out"] and d.parse_ease("0.4,0,0.2,1") == (0.4, 0.0, 0.2, 1.0),
        "parse_ease: names and control points")
    chk(all(d.parse_ease(s) is None for s in ("bouncy", "0.4,0,0.2", "0.4,0,0.2,1.5", "0.4,-1,0.2,1", "1,1,1,1,", "")),
        "parse_ease rejects other names, wrong counts, and points outside 0-1")
    chk(d.ease_text(d.EASES["ease-in"]) == "ease-in" and d.ease_text((0.4, 0.0, 0.2, 1.0)) == "0.4,0,0.2,1",
        "ease_text writes what parse_ease reads")
    eased = d.Fade(("000000", "FFFFFF"), 2.0, 100.0, eases=d.curves_for(2, [d.EASES["ease-in"]]))
    chk(eased.at(101.0) == d.mix("000000", "FFFFFF", d.ease(d.EASES["ease-in"], 0.5)) and eased.at(102.0) == "FFFFFF",
        "a fade eases each step from one color to the next")
    chk(d.Fade.load(eased.saved()) == dataclasses.replace(eased, start=d.EPOCH), "an eased effect saves and loads")
    chk(d.Fade.load({**eased.saved(), "ease": "wobbly"}) is None, "a bad saved ease is ignored")
    chk("ease" not in d.Fade(("000000", "FFFFFF"), 2.0).saved(), "a linear effect saves as before")
    ein, snap = d.EASES["ease-in"], (0.9, 0.0, 0.1, 1.0)
    steps = d.Fade(("FF0000", "FFFF00", "0000FF"), 1.0, 0.0, eases=(d.LINEAR, ein, snap))
    chk(steps.at(0.5) == d.mix("FF0000", "FFFF00", d.ease(ein, 0.5))
        and steps.at(1.5) == d.mix("FFFF00", "0000FF", d.ease(snap, 0.5))
        and steps.at(2.5) == d.mix("0000FF", "FF0000", 0.5), "each step uses the curve into the color it goes to")
    chk(steps.saved()["ease"] == ["linear", "ease-in", "0.9,0,0.1,1"] and d.Fade.load(steps.saved()) == steps,
        "different curves save as a list and load back")
    chk(d.Fade.load({**steps.saved(), "ease": ["ease-in", "linear"]}) is None, "a list of the wrong length is ignored")
    chk(d.curves_for(3, [d.LINEAR]) == () and d.curves_for(2, [ein]) == (ein, ein), "curves_for: one curve for every step")
    chk(d.mix("000000", "FFFFFF", 0) == "000000" and d.mix("000000", "FFFFFF", 1) == "FFFFFF"
        and d.mix("000000", "FFFFFF", 0.5) == "808080" and d.mix("FF0000", "0000FF", 0.25) == "BF0040",
        "mix blends each channel")
    f = d.Fade(("FF0000", "FFFFFF", "0000FF"), 2.0, 100.0)
    chk([f.at(t) for t in (99.0, 100.0, 101.0, 102.0, 104.0, 105.0, 106.0)] ==
        ["FF0000", "FF0000", "FF8080", "FFFFFF", "0000FF", "800080", "FF0000"],
        "fade: period seconds from each color to the next, looping, holding before start")


def draw_all(L: Any) -> None:
    """For tests that check colors rather than timing: end due timed effects, then draw
    every fading zone at the current time (tick() draws one zone at a time)."""
    L.tick()
    with L.lock:
        L._draw(list(L.running()), d.now())


def test_frame_slots() -> None:
    """Frames: one zone per write, writes at least 1 / WRITE_RATE apart, each zone up to once
    per FRAME, longest-waiting zone first."""
    C = config(state_file=os.path.join(TMP, "slot-state.json"))
    with FakeClock() as clock, contextlib.ExitStack() as stack:
        L = d.Lights(C)
        L.set(d.ALL, "FFFFFF")
        writes: list[tuple[float, str]] = []
        real_write = L.leds.write

        def record(zone: d.Zone, color: str) -> bool:
            writes.append((clock.t, zone))
            return real_write(zone, color)
        stack.enter_context(mock.patch.object(L.leds, "write", record))

        def run_for(seconds: float) -> None:  # step like render() does
            writes.clear()
            end = clock.t + seconds
            while (due := L.next_due()) is not None and due < end:
                clock.t = due
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
            run(L, "color FFFFFF")
            for z in d.ZONES[:n]:
                run(L, f"cycle -z {z} AA00AA 00FF00")
            run_for(10.0)
            rates = [len(ts) / 10.0 for z, ts in per_zone().items() if z in d.ZONES[:n]]
            chk(all(abs(r - 1 / d.FRAME) <= 1 for r in rates) and {z for _, z in writes} == set(d.ZONES[:n]),
                f"{n} fading zone(s): each drawn every FRAME seconds")
        clock.t += 1000
        L.tick()
        chk(due_within(L, clock.t, 1 / d.WRITE_RATE + 1e-9), "after a stall, frames carry on soon")
        run(L, "color FFFFFF")


def test_caps_and_persistence() -> None:
    red, white = "FF0000", "FFFFFF"  # a plain two-color caps fade for these checks; the real one is in test_cycles
    C = config(state_file=os.path.join(TMP, "state", "state.json"), caps_colors=(red, white), caps_delay=0.7,
               caps_ease=d.LINEAR)
    with FakeClock() as clock:
        L = d.Lights(C)
        for z in d.ZONES:
            L.set(z, "8800FF")
        chk(L.next_due() is None, "nothing fading -> no timeout")
        L.set_caps(True)
        chk(leds() == {"left": red, "center": C.caps_other, "right": C.caps_other, "extra": C.caps_other},
            "caps on: left starts red, other zones turn white")
        chk(due_within(L, clock.t, d.FRAME), "caps on -> a frame within FRAME seconds")

        def at(t: float) -> str:
            clock.t = 1000.0 + t * C.caps_delay
            L.tick()
            return leds()["left"]
        chk(at(0.5 * d.FRAME) == red, "no frame before FRAME is up")
        chk(at(0.5) == d.mix(red, white, 0.5), "halfway through CAPS_DELAY: halfway to white")
        chk(at(1) == white and at(1.75) == d.mix(white, red, 0.75) and at(2) == red, "fades to white and back")
        def near(a: str, b: str) -> bool:  # within rounding
            return all(abs(x - y) <= 1 for x, y in zip(bytes.fromhex(a), bytes.fromhex(b)))
        chk(near(at(1000.5), d.mix(red, white, 0.5)) and due_within(L, clock.t, d.FRAME),
            "after a stall, frames carry on from now")
        L.set("center", "00FF00")
        chk(leds()["center"] == C.caps_other and L.scene["center"] == "00FF00",
            "colors set while caps is on wait for it to turn off")
        L.set("left", "0000FF")
        chk(at(1000.75) == d.mix(red, white, 0.75), "left keeps fading after setting its color")
        L.set_caps(True)
        chk(L.caps_fade is not None and L.caps_fade.start == 1000.0, "repeated caps on doesn't restart the fade")
        L.set_caps(False)
        chk(leds()["left"] == "0000FF" and leds()["center"] == "00FF00" and L.next_due() is None,
            "caps off shows the zones' colors, stops frames")
        chk(at(1001) == "0000FF", "tick with nothing fading does nothing")
        L.set(d.ALL, "8800FF")
        L.set_caps(True); L.set_caps(False)
        chk(set(leds().values()) == {"8800FF"}, "caps off restores saved colors")
        chk(json.load(open(C.state_file))["extra"] == "8800FF", "colors saved as hex")
        chk(d.Lights(C).scene["center"] == "8800FF", "colors reload from state file")


def test_held_cycles() -> None:
    """Menu + hold a zone key + digits: a cycle, timed by the gap between the first two digits."""
    C = config(state_file=os.path.join(TMP, "held-state.json"))
    with FakeClock() as clock:
        L = d.Lights(C)
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

        def cycle(zone: d.Zone) -> tuple[list[str], float] | None:
            f = L.running().get(zone)
            return (list(f.colors), f.period) if f else None

        press((M, 1), ("Q", 1), ("1", 1), ("1", 0), 1.5, ("3", 1), ("3", 0))
        chk(leds()["left"] == "FFFF00" and L.running() == {}, "while the zone key is held, digits show their color")
        press(0.2, ("7", 1), ("7", 0), ("Q", 0))
        chk(cycle("left") == (["FF0000", "FFFF00", "0000FF"], 1.5) and set(L.running()) == {"left"},
            "letting go of the zone key starts the cycle; the first gap sets its time")
        press(("Q", 1))
        chk(dialogs == [] and h.sel is not None and h.sel.target == "left", "pressing the zone key again starts over instead of the dialog")
        press(("4", 1), ("4", 0), 0.5, ("5", 1), ("5", 0), ("Q", 0))
        chk(cycle("left") == (["00FF00", "00FFAA"], 0.5), "a new cycle replaces the old one")

        press(("Q", 1), ("Q", 0), ("1", 1), ("1", 0), 1.0, ("3", 1), ("3", 0))
        chk(L.running() == {} and leds()["left"] == "FFFF00", "digits after letting go just set the color, as before")
        press(("Q", 1), ("Q", 0))
        chk(dialogs == ["left"], "and the zone key again opens the dialog, as before"); dialogs.clear()
        press(("Q", 1), ("6", 1), ("6", 0), ("Q", 0), ("Q", 1), ("Q", 0))
        chk(L.running() == {} and leds()["left"] == "0088FF" and dialogs == ["left"],
            "one digit while holding just sets the color"); dialogs.clear()

        press(("SPACE", 1), ("2", 1), ("2", 0), 2.0, ("8", 1), ("8", 0), ("SPACE", 0))
        chk(all(cycle(z) == (["FF6600", "8800FF"], 2.0) for z in d.ZONES), "holding Space: cycle the whole keyboard")
        press(("K", 1), ("1", 1), ("1", 0), 0.01, ("2", 1), ("2", 0))
        press(("L", 1), ("K", 0))
        chk(cycle("center") == (["FF0000", "FF6600"], 0.1), "pressing another zone key finishes the cycle; time >= 0.1 s")
        press(("4", 1), ("4", 0), 100.0, ("5", 1), ("5", 0), ("L", 0))
        chk(cycle("right") == (["00FF00", "00FFAA"], 60.0), "time <= 60 s")
        press(("E", 1), ("1", 1), ("1", 0), 1.0, ("2", 1), ("2", 0), ("BACKSPACE", 1), ("BACKSPACE", 0), ("E", 0))
        chk(L.running() == {} and set(leds().values()) == {"FFFFFF"},
            "Backspace while holding drops the picks and goes back to before Menu went down")
        press(("LEFTSHIFT", 1), ("BACKSPACE", 1), ("BACKSPACE", 0), ("LEFTSHIFT", 0))
        chk(cycle("right") == (["00FF00", "00FFAA"], 60.0) and cycle("center") == (["FF0000", "FF6600"], 0.1),
            "and redo brings the whole hold's changes back")
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
        "FF8800": "color -z left FF8800",
        "#ff8800": "color -z left FF8800",
        "FF8800 2": "color -z left -t 2 FF8800",
        "FF8800 .5": "color -z left -t 0.5 FF8800",
        "FF0000 0000FF": "cycle -z left FF0000 0000FF",
        "FF0000, 0000FF, 3": "cycle -z left -t 3 FF0000 0000FF",
        "cycle FF0000 0000FF 1.25": "cycle -z left -t 1.25 FF0000 0000FF",
        "rainbow": "rainbow -z left",
        "Rainbow 0.3": "rainbow -z left -t 0.3",
        "pulse 00ffaa": "pulse -z left 00FFAA",
        "pulse 00FFAA 1.5": "pulse -z left -t 1.5 00FFAA",
        "  FF8800  ": "color -z left FF8800",
        "112233 445566": "cycle -z left 112233 445566",
        "FF0000 0000FF 2 ease-in-out": "cycle -z left -t 2 -e ease-in-out FF0000 0000FF",
        "pulse Ease-Out 00FFAA": "pulse -z left -e ease-out 00FFAA",
        "FF8800 3 0.4,0,0.2,1": "color -z left -t 3 -e 0.4,0,0.2,1 FF8800",
        "rainbow 0.4 linear": "rainbow -z left -t 0.4 -e linear",
        "FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1 2": "cycle -z left -t 2 FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1",
        "FF0000, ffff00:Ease-Out, 2 ease-in": "cycle -z left -t 2 -e ease-in FF0000 FFFF00:ease-out",
        "FF8800:ease-in 3": "color -z left -t 3 FF8800:ease-in",
    }
    for text, line in ok.items():
        action, problem = d.Action.from_dialog(text, "left", CONFIG)
        chk(str(action) == line and problem == "", f"dialog: {text!r} -> {line}")
    chk(str(d.Action.from_dialog("FF8800 2", d.ALL, CONFIG)[0]) == "color -z all -t 2 FF8800", "dialog for the whole keyboard")
    bad = {"": "Type a color", "zzz": "isn't a color", "FF8800 nope": "isn't a color", "2": "Type a color",
           "FF8800 61": "from 0 to 60", "FF0000 0000FF 0": "from 0.1 to 60", "rainbow FF0000": "no colors",
           "pulse": "one color", "pulse FF0000 00FF00": "one color", "cycle FF0000": "2 to 16",
           "FF0000 " * 17: "2 to 16", "<b>&": "isn't a color",
           "FF0000 0000FF 0.4,0,0.2,1.5": "isn't an easing curve", "FF0000 0000FF 0.4,0,,1": "isn't an easing curve",
           "FF0000 bouncy": "isn't a color", "FF0000 FFFF00:bouncy": "isn't an easing curve",
           "pulse 00FFAA:ease-in": "one curve for all of its steps", "zzz:ease-in FF0000": "isn't a color"}
    for text, why in bad.items():
        rejected, problem = d.Action.from_dialog(text, "left", CONFIG)
        chk(rejected is None and why in problem, f"dialog rejects {text!r}: {problem}")

    help_text = d.dialog_help(CONFIG)
    chk(help_text.isascii(), "dialog help is ASCII (zenity rejects other text outside a UTF-8 locale)")
    chk(all(c in help_text for c in CONFIG.palette) and len(CONFIG.palette_names) == len(CONFIG.palette),
        "the dialog lists every palette color")
    seen: list[list[str]] = []

    def fake_run(cmd: list[str], **kw: object) -> subprocess.CompletedProcess[str]:
        seen.append(cmd)
        return subprocess.CompletedProcess(cmd, 0, "", "")
    with mock.patch.object(subprocess, "run", fake_run):
        d.run_as_user(pwd.getpwuid(os.getuid()).pw_name, ["zenity", "--entry"])
    chk("LANG=C.UTF-8" in seen[0] and seen[0][-2:] == ["zenity", "--entry"],
        "desktop commands run with a UTF-8 locale")

    C = config(state_file=os.path.join(TMP, "dialog-state.json"))
    with FakeClock():
        L = d.Lights(C)
        for line, shown in [("color -z left 123456", "123456"), ("cycle -z left -t 2 AA00AA 00FF00", "AA00AA 00FF00 2"),
                            ("rainbow -z left -t 0.3", "rainbow 0.3"), ("pulse -z left 00FFAA", "pulse 00FFAA 0.5"),
                            ("pulse -z left -d 5 00FFAA", "pulse 00FFAA 0.5"),
                            ("cycle -z left -t 2 -e ease-in AA00AA 00FF00", "AA00AA 00FF00 2 ease-in"),
                            ("pulse -z left -e 0.4,0,0.2,1 00FFAA", "pulse 00FFAA 0.5 0.4,0,0.2,1"),
                            ("cycle -z left FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1",
                             "FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1 1"),
                            ("cycle -z left -e ease-out FF0000:ease-out 0000FF", "FF0000 0000FF 1 ease-out"), ("color -z left -t 3 112233", "112233")]:
            run(L, line)
            chk(all(d.Action.current(L, t).dialog_text() == shown for t in ("left", d.ALL)),
                f"dialog starts from {shown!r}")
            chk(d.Action.from_dialog(shown, "left", CONFIG)[0] is not None, f"what the dialog starts from is valid: {shown!r}")

        answers = [(0, "<i>zzz"), (0, "FF0000 0000FF 61"), (0, "FF0000 0000FF 2")]
        shown_texts: list[tuple[str, ...]] = []

        def fake_zenity(user: str, *args: str) -> tuple[int, str]:
            shown_texts.append(args)
            return answers.pop(0) if "--entry" in args else (0, "")
        real = d.session_user, d.run_zenity
        d.session_user, d.run_zenity = (lambda: "someone"), fake_zenity
        try:
            run(L, "color -z right 445566")
            d.hex_dialog(L, "right")
            entries = [a for a in shown_texts if "--entry" in a]
            errors = [a for a in shown_texts if "--error" in a]
            chk("--entry-text=445566" in entries[0] and "--entry-text=<i>zzz" in entries[1],
                "the dialog starts from the zone's color, then from what was typed")
            chk(len(errors) == 2 and "&lt;i&gt;" in " ".join(errors[0]) and "60 seconds" in " ".join(errors[1]),
                "bad input shows why (markup escaped) and asks again")
            chk(L.running()["right"].colors == ("FF0000", "0000FF") and L.running()["right"].period == 2.0,
                "the dialog can start a cycle")
            answers[:] = [(1, "")]
            d.hex_dialog(L, "right")
            chk(L.running()["right"].period == 2.0, "cancelling changes nothing")
        finally:
            d.session_user, d.run_zenity = real


def run(L: Any, line: str) -> None:
    """Run a kbdrgb command line on L; it must be valid."""
    assert d.run_request(L, line.split()) == "", line


def test_undo() -> None:
    """Menu+Backspace goes back through earlier scenes, kept in memory."""
    C = config(state_file=os.path.join(TMP, "undo-state.json"))
    with FakeClock() as clock:
        L = d.Lights(C)
        L.set(d.ALL, "FFFFFF")

        def cmd(line: str) -> None:  # as kbdrgb sends it
            d.run_request(L, line.split())

        L.undo()
        chk(set(leds().values()) == {"FFFFFF"}, "nothing to go back to -> all white")
        cmd("color FF0000")
        cmd("zones 00FF00 0000FF FFFF00 FF00AA")
        cmd("rainbow -z right")
        cmd("pulse -z left -d 5 00FFAA")  # timed: kept as what left showed before it
        cmd("color -z center 0000AA")
        cmd("pulse -z left -d 100 FF0000")  # timed on timed: still what was there before both
        cmd("color -z center 0000BB")
        clock.t += 101
        L.tick()  # the pulse ends; left keeps its last color
        pulsed = leds()["left"]
        cmd("color -z center -t 2 AA00AA")  # in a transition: counts as where it's going
        cmd("cycle -z extra 112233 445566")
        clock.t += 0.3
        L.undo()
        chk(leds()["extra"] == "FF00AA" and "extra" not in L.running() and leds()["center"] == "AA00AA",
            "undo restores the last scene (a transition's target color)")
        L.undo()
        chk(leds()["center"] == "0000BB" and leds()["left"] == pulsed, "once a timed effect ends, its last color is kept")
        L.undo()
        chk(leds()["center"] == "0000AA" and leds()["left"] == "00FF00" and "left" not in L.running(),
            "while a timed effect runs, other zones' changes are kept; its zone is kept as before the effect")
        L.undo()
        chk(leds()["center"] == "0000FF" and leds()["left"] == "00FF00" and L.running()["right"].colors == C.rainbow,
            "undo never brings back a timed effect")
        chk(leds()["right"] == L.running()["right"].at(clock.t), "a restored effect is in step, as if it never stopped")
        L.undo()
        chk(leds() == {"left": "00FF00", "center": "0000FF", "right": "FFFF00", "extra": "FF00AA"} and L.running() == {},
            "undo ends effects; kbdrgb zones is one step")
        L.undo()
        chk(set(leds().values()) == {"FF0000"}, "and so on back")
        L.undo()
        chk(set(leds().values()) == {"FFFFFF"}, "back to the start")
        L.undo()
        chk(set(leds().values()) == {"FFFFFF"}, "then white")
        chk(json.load(open(C.state_file))["left"] == "FFFFFF", "undo saves the colors")

        L.redo()
        chk(set(leds().values()) == {"FF0000"}, "redo goes forward to what undo left")
        cmd("color FFFFFF")
        L.redo()
        chk(set(leds().values()) == {"FFFFFF"}, "a new change ends redo: nothing to redo -> no change")
        cmd("color 111111"); cmd("cycle -z left 112233 445566"); cmd("color -z right 222222")
        L.undo(); L.undo()
        chk(set(leds().values()) == {"111111"} and L.running() == {}, "undo twice")
        clock.t += 7.3
        L.redo()
        chk(L.running()["left"].colors == ("112233", "445566") and leds()["left"] == L.running()["left"].at(clock.t)
            and leds()["right"] == "111111", "redo goes forward one, effects in step")
        L.redo()
        chk(leds()["right"] == "222222" and "left" in L.running(), "and another")
        L.redo()
        chk(leds()["right"] == "222222", "past the end of redo -> no change")
        L.undo(); L.undo(); L.redo()
        chk("left" in L.running() and leds()["right"] == "111111", "undo and redo go back and forth")
        cmd("color -z center 333333")
        L.redo()
        chk(leds()["right"] == "111111" and leds()["center"] == "333333", "a new change ends redo")
        L.undo()
        chk(leds()["center"] == "111111" and "left" in L.running(), "and undo goes back from the new change")
        cmd("color 444444")
        L = d.Lights(C)  # a restart: the scene comes back, its history doesn't (cmd uses the new L)
        chk(set(leds().values()) == {"444444"}, "the scene is there after a restart")
        L.undo()
        chk(set(leds().values()) == {"FFFFFF"}, "undo history isn't saved across restarts: undo -> white")
        L.redo()
        chk(set(leds().values()) == {"444444"}, "and redo brings back what was there")

        cmd("color FF0000"); cmd("color FF0000"); cmd("color 00FF00"); cmd("nonsense")
        L.undo()
        chk(set(leds().values()) == {"FF0000"}, "a message that changes nothing doesn't cost an undo")
        L.undo()
        chk(set(leds().values()) == {"444444"}, "repeating a scene doesn't keep it twice")

        for i in range(C.undo_depth + 10):
            cmd(f"color {i:06X}")
        for _ in range(C.undo_depth):
            L.undo()
        chk(set(leds().values()) == {"000009"}, "UNDO_DEPTH undos reach the oldest scene kept")
        L.undo()
        chk(set(leds().values()) == {"FFFFFF"}, "older ones are gone: one more undo -> white")


def test_history() -> None:
    """History on its own: copies of scenes, no LEDs."""
    h = d.History(CONFIG)
    red, green, blue = d.solid_scene("FF0000"), d.solid_scene("00FF00"), d.solid_scene("0000FF")
    h.remember(red)
    red["left"] = "123456"
    chk(list(h.past) == [d.solid_scene("FF0000")], "remember keeps a copy, not the caller's scene")
    h.remember(green)
    chk(h.undo(blue) == green and h.undo(green) == d.solid_scene("FF0000"), "undo goes back one at a time")
    chk(h.redo(d.solid_scene("FF0000")) == green and h.redo(green) == blue, "redo comes forward again")
    chk(h.redo(blue) is None, "nothing to redo -> None")
    chk(h.undo(d.solid_scene("FF0000")) == green, "undo skips scenes equal to now")
    h.remember(blue)
    chk(not h.future and str(h) == "2 to undo, 0 to redo", "a new change ends redo")


def test_commands() -> None:
    """Parsing kbdrgb's commands."""
    C = config(state_file=os.path.join(TMP, "cmd-state.json"))
    with FakeClock():
        L = d.Lights(C)
        good = ["color FF0000", "color -z left 00ff00", "color -z all -t 2.5 #0000FF", "color -t 0 112233",
                "cycle AA00AA 00FF00", "cycle -z center -t 0.1 -d 0.1 AA00AA 00FF00 0000FF",
                "rainbow", "rainbow -z extra -t 60 -d 86400", "pulse -z right -t 1.5 00FFAA",
                "pulse -e ease-in-out 00FFAA", "color -t 2 -e 0,0,1,1 FF0000", "cycle -e 1,0,0,1 AA00AA 00FF00",
                "cycle FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1", "color -t 2 FF0000:ease-out",
                "cycle -e ease-in AA00AA:linear 00FF00"]
        for line in good:
            chk(d.run_request(L, line.split()) == "", f"valid: {line}")
            action, = d.parse_command(line.split(), CONFIG)
            chk(d.parse_command(str(action).split(), CONFIG) == [action], f"an action written out reads back the same: {action}")
        bad = ["", "nope", "color", "color FF0000 00FF00", "color -d 3 FF0000", "color -z top FF0000",
               "color -t 61 FF0000", "color -t -1 FF0000", "color -t abc FF0000", "color -t 1e3 FF0000",
               "color -t nan FF0000", "color color=FF0000", "cycle AA00AA", "cycle " + "AA00AA " * 17,
               "cycle -t 0 AA00AA 00FF00", "cycle -t 0.05 AA00AA 00FF00", "cycle -d 0 AA00AA 00FF00",
               "rainbow FF0000", "rainbow -d 86401", "pulse", "pulse zzzzzz", "pulse FF0000 00FF00",
               "rainbow -z", "rainbow -t", "sync", "stop", "pulse -e bouncy FF0000",
               "pulse -e 0.4,0,0.2 FF0000", "pulse -e 0.4,0,0.2,2 FF0000", "pulse -e FF0000",
               "pulse FF0000:ease-in", "cycle FF0000:bouncy 00FF00", "cycle FF0000: 00FF00", "cycle zzz:ease-in 00FF00"]
        for line in bad:
            chk(d.run_request(L, line.split()) != "", f"rejected: {line!r}")
        for args, why in [(["zones", "-z", "left", "FF0000"], "zones doesn't take --zone"),
                          (["color", "FF0000", "-d", "3"], "color doesn't take --for"),
                          (["cycle", "AA00AA"], "cycle takes 2 to 16 colors"),
                          (["color", "-t", "abc", "FF0000"], '"abc" isn\'t a number of seconds'),
                          (["color", "FF0000", "-q"], "unrecognized arguments: -q"),
                          (["zones", *["111111"] * 5], "zones takes 1 to 4 colors")]:
            chk(why in d.run_request(L, args), f"kbdrgb {' '.join(args)}: {why}")
        d.run_request(L, ["zones", "010101", "020202"])
        chk(L.scene == {"left": "010101", "center": "020202", "right": "FFFFFF", "extra": "FFFFFF"},
            "zones fills the rest with white")
        d.run_request(L, ["color", "-z", "left", "a b"])
        chk(L.scene["left"] == "010101", "a rejected command changes nothing")
        for line in ["cycle -z left -e ease-in 00FF00 0000FF", "pulse -z center -t 2 -d 30 8800FF",
                     "rainbow -z right", "color -z extra -t 5 FF00AA"]:
            d.run_request(L, line.split())
        L.set_caps(True)
        status = d.status_text(L).splitlines()
        chk(status[1:5] == ["left:   00FF00 0000FF 1 ease-in", "center: pulse 8800FF 2  (for 30s more)",
                            "right:  rainbow 0.6", "extra:  FF00AA"],
            "status: each zone's setting as the dialog writes it, ignoring caps lock and transitions")
        chk(status[5] == "kbdrgb zones '00FF00 0000FF 1 ease-in' 'pulse 8800FF 2' 'rainbow 0.6' FF00AA",
            "status: a zones command that sets them again")
        chk(d.parse_command(shlex.split(status[5])[1:], C)[0] == d.Action.current(L, "left"),
            "the zones command reads back as what the zone has")


def test_cycles() -> None:
    C = config(state_file=os.path.join(TMP, "cycle-state.json"))
    if os.path.exists(C.state_file):
        os.unlink(C.state_file)

    def cycles(L: Any) -> dict[str, list[str]]:
        return {z: list(f.colors) for z, f in L.running().items()}

    with FakeClock() as clock:
        L = d.Lights(C)
        run(L, "color 8800FF")

        def at(t: float) -> dict[str, str]:  # t: seconds since 1000.0
            clock.t = 1000.0 + t
            draw_all(L)
            return leds()

        run(L, "cycle -z center AA00AA 00FF00")
        chk(leds()["center"] == "AA00AA" and leds()["left"] == "8800FF", "cycle on one zone")
        chk(due_within(L, clock.t, d.FRAME), "cycling -> a frame within FRAME seconds")
        chk(at(0.5 * C.cycle_delay)["center"] == d.mix("AA00AA", "00FF00", 0.5), "fades halfway in half a CYCLE_DELAY")
        chk(at(C.cycle_delay)["center"] == "00FF00" and at(2 * C.cycle_delay)["center"] == "AA00AA",
            "reaches the second color and fades back")
        chk(leds()["left"] == "8800FF", "other zones stay put")
        chk(json.load(open(C.state_file))["fades"] ==
            {"center": {"colors": ["AA00AA", "00FF00"], "period": C.cycle_delay, "offset": 0.0}}, "cycle saved")
        L2 = d.Lights(C)
        chk(cycles(L2) == {"center": ["AA00AA", "00FF00"]} and L2.shown("center") == L.shown("center"),
            "cycle resumes after restart, in step with where it was")

        # Same colors and time, started at different times: in step. Different time: its own pace.
        clock.t = 1002.37
        run(L, "cycle -z right AA00AA 00FF00")
        draw_all(L)
        chk(leds()["right"] == leds()["center"], "a cycle started later joins in step with an equal one")
        for t in (3.2, 3.9, 4.6):
            at(t)
            chk(leds()["right"] == leds()["center"], f"equal cycles stay in step ({t}s)")
        run(L, "cycle -z left -t 3 AA00AA 00FF00")
        slow, fast = (d.Fade(("AA00AA", "00FF00"), p, d.EPOCH) for p in (3.0, C.cycle_delay))
        at(5.3)
        chk(leds()["left"] == slow.at(clock.t) and leds()["center"] == fast.at(clock.t)
            and leds()["left"] != leds()["center"], "each zone keeps its own transition time")

        run(L, "color -z center 0000FF")
        chk(leds()["center"] == "0000FF" and set(L.running()) == {"left", "right"}, "a zone's new color ends only its effect")
        run(L, "color FFFFFF")
        chk(L.running() == {} and L.next_due() is None and set(leds().values()) == {"FFFFFF"},
            "setting all zones ends all effects")

        caps = d.Fade(C.caps_colors, C.caps_delay, 0.0, eases=d.curves_for(len(C.caps_colors), [C.caps_ease]))
        around = len(C.caps_colors) * C.caps_delay
        greens = [int(caps.at(i / 1000)[2:4], 16) for i in range(int(around * 1000))]
        chk(sum(g <= 0x10 for g in greens) > len(greens) / 2, "caps lock: the left zone is red most of the time")
        chk(all(sum(int(caps.at(phase / 100 + k * d.FRAME)[2:4], 16) >= 0xC0 for k in range(int(around / d.FRAME))) >= 2
                for phase in range(10)), "and flashes white for at least 2 frames, however frames line up")

        run(L, "cycle -z left AA00AA 00FF00")
        L.set_caps(True)
        clock.t += 9 * C.caps_delay
        L.tick()
        chk(L.caps_fade is not None and leds()["left"] == L.caps_fade.at(clock.t),
            "caps lock fade wins over a left-zone cycle")
        L.set_caps(False)
        chk(leds()["left"] == L.running()["left"].at(clock.t), "left cycle shows again when caps lock turns off")
        run(L, "color FFFFFF")

        run(L, "cycle -z center AA00AA 00FF00")
        run(L, "pulse -z right -d 1 00FFAA")
        L.set_caps(True)
        for _ in range(30):
            clock.t += 0.1
            L.tick()
        chk(leds()["center"] == C.caps_other and leds()["right"] == C.caps_other,
            "other zones stay white through their effects while caps is on")
        chk("right" not in L.running() and "center" in L.running(), "timed effects still end while caps is on")
        L.set_caps(False)
        chk(leds()["center"] == L.running()["center"].at(clock.t) and leds()["right"] == L.scene["right"],
            "caps off: effects carry on where they'd be, ended ones show their last color")
        run(L, "color FFFFFF")

        good = {"colors": ["AA00AA", "00FF00"], "period": 1.0}
        json.dump({"left": "FFFFFF", "fades": {
            "left": {"colors": ["AA00AA"], "period": 1.0}, "nowhere": good,
            "center": {**good, "period": 0}, "right": {**good, "offset": 5}, "extra": {**good, "period": "x"}}},
            open(C.state_file, "w"))
        chk(d.Lights(C).running() == {}, "bad effects in the state file are ignored")
        json.dump({"left": "FFFFFF", "fades": {"right": {**good, "offset": 1.5}}}, open(C.state_file, "w"))
        chk(d.Lights(C).running()["right"].offset == 1.5, "saved effect keeps its offset")
        json.dump({"left": "FFFFFF", "fades": ["junk"]}, open(C.state_file, "w"))
        chk(d.Lights(C).scene["left"] == "FFFFFF", "malformed fades entry doesn't break loading")


def test_transitions() -> None:
    """kbdrgb color with a time: fade from what the zone shows to the new color."""
    C = config(state_file=os.path.join(TMP, "transition-state.json"))
    with FakeClock() as clock:
        L = d.Lights(C)
        run(L, "color 000000")
        start = clock.t
        run(L, "color -z left -t 2 FFFFFF")
        chk(leds()["left"] == "000000" and leds()["center"] == "000000", "transition starts from the current color")
        chk(json.load(open(C.state_file))["left"] == "FFFFFF", "the new color is saved right away")
        clock.t = start + 1
        L.tick()
        chk(leds()["left"] == "808080", "halfway through the time, halfway there")
        run(L, "color -z right -t 0.5 FF0000")
        clock.t = start + 1.5
        L.tick()
        chk(leds()["left"] == "BFBFBF" and leds()["right"] == "FF0000", "zones fade with their own times")
        clock.t = start + 2.05
        L.tick()
        chk(leds()["left"] == "FFFFFF" and L.running() == {} and L.next_due() is None, "ends exactly on the new color")

        run(L, "cycle -z center 0000FF 00FF00")
        frame = L.shown("center")
        run(L, "color -z center -t 1 FF0000")
        chk(leds()["center"] == frame, "transition out of an effect starts from its current frame")
        clock.t += 1.05
        L.tick()
        chk(leds()["center"] == "FF0000" and "center" not in L.running(), "and ends on the new color")
        run(L, "color -z left -t 1 00FF00")
        run(L, "color -z left 0000FF")
        chk(leds()["left"] == "0000FF" and L.running() == {}, "an instant color cuts a transition short")

        run(L, "color 000000")
        start = clock.t
        run(L, "color -z left -t 2 -e ease-in FFFFFF")
        clock.t = start + 1
        L.tick()
        chk(leds()["left"] == d.mix("000000", "FFFFFF", d.ease(d.EASES["ease-in"], 0.5)) and leds()["left"] < "808080",
            "an ease-in transition is less than halfway there at half time")
        clock.t = start + 2.05
        L.tick()
        chk(leds()["left"] == "FFFFFF" and L.running() == {}, "and still ends exactly on the new color")


def test_effects() -> None:
    """rainbow and pulse: endless or timed, on any zone, cleared by new colors."""
    C = config(state_file=os.path.join(TMP, "effect-state.json"))
    with FakeClock() as clock:
        L = d.Lights(C)
        run(L, "color FFFFFF")
        run(L, "rainbow")
        expect = {z: d.Fade(C.rainbow, C.rainbow_delay, d.EPOCH, i * C.rainbow_lag).at(clock.t)
                  for i, z in enumerate(d.ZONES)}
        chk(leds() == expect and len(set(expect.values())) == 4, "rainbow: each zone a step further around the wheel")
        before = leds()
        clock.t += 6 * C.rainbow_delay
        draw_all(L)
        chk(leds() == before, "rainbow loops around the wheel")
        chk(len(json.load(open(C.state_file))["fades"]) == 4, "endless rainbow saved, to resume after a restart")
        run(L, "color -z center 123456")
        chk(set(L.running()) == {"left", "right", "extra"}, "a new zone color clears the rainbow there")
        run(L, "rainbow -z center")
        chk(set(L.running()) == set(d.ZONES), "rainbow on one zone")
        chk(L.running()["center"].offset == C.rainbow_lag, "a one-zone rainbow keeps the zone's place on the wheel")
        run(L, "color FFFFFF")

        run(L, "pulse -z extra -t 1 -d 3 00ff88")
        start = clock.t
        dim = d.mix("000000", "00FF88", C.pulse_dim)
        pulse = d.Fade(("00FF88", dim), 1.0, d.EPOCH)
        chk(leds()["extra"] == pulse.at(start) and leds()["left"] == "FFFFFF", "pulse on one zone")
        clock.t = start + 0.5
        L.tick()
        chk(leds()["extra"] == pulse.at(clock.t), "pulse fades toward a dim copy")
        chk(json.load(open(C.state_file))["fades"] == {}, "timed effects aren't saved")
        clock.t = start + 2.9
        L.tick()
        chk(L.running() != {}, "timed pulse still running before its time is up")
        clock.t = start + 3.2
        L.tick()
        last = pulse.at(start + 3)
        chk(L.running() == {} and L.next_due() is None and leds()["extra"] == last
            and json.load(open(C.state_file))["extra"] == last,
            "timed pulse ends on the frame at its end time, and that color is saved")

        run(L, "rainbow -d 2")
        clock.t += 1
        run(L, "cycle -z center AA00AA 00FF00")
        clock.t += 1.5
        L.tick()
        chk(set(L.running()) == {"center"}, "timed rainbow ends; an endless cycle started on top keeps going")
        run(L, "color FFFFFF")


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


def socket_fds() -> list[int]:
    return [fd for fd, target in open_fds().items() if target.startswith("socket:")]


def wait_for(cond: Callable[[], object], msg: str, timeout: float = 3.0) -> None:
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if cond():
            return chk(True, msg)
        time.sleep(0.01)
    chk(False, msg)


def test_main_loop() -> None:
    """main(): real select() loop over keyboard, virtual keyboard LEDs, and kbdrgb's socket,
    with its settings from the command line."""
    C = config(state_file=os.path.join(TMP, "main-state.json"), device=os.path.join(TMP, "fake-kbd"),
               stable_after=0.3, caps_delay=0.5, cycle_delay=0.5, caps_colors=("FF0000", "FFFFFF"),
               caps_ease=d.LINEAR)
    open(C.device, "w").close()
    for z in d.ZONES:
        open(f"{LED}/color_{z}", "w").write("FFFFFF")
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
    real_dev, real_from = evdev.InputDevice, evdev.UInput.from_device
    evdev.InputDevice = lambda p: dev
    evdev.UInput.from_device = staticmethod(lambda _dev, name=None: ui)

    exits = []

    def run() -> None:
        try:
            d.main(argv(C))
        except SystemExit as ex:
            exits.append(ex.code)  # fake device "disappeared"
        except Exception as ex:
            errors.append(ex)

    t = threading.Thread(target=run, daemon=True)
    os.makedirs(os.path.dirname(C.problem_file), exist_ok=True)
    json.dump({"key": "not-found:x", "count": 5, "notified": True}, open(C.problem_file, "w"))
    NOTIFIED.clear()
    try:
        t.start()
        wait_for(lambda: "grab" in calls, "main grabs the keyboard")
        st = os.stat(C.socket)
        chk(stat.S_ISSOCK(st.st_mode) and stat.S_IMODE(st.st_mode) == 0o660
            and st.st_gid == os.stat(f"{LED}/color_left").st_gid,
            "socket created, group read+write for the color files' group")

        def K(c: int, v: int) -> list[InputEvent]:  # one key event as the kernel sends it
            return [InputEvent(0, 0, e.EV_MSC, e.MSC_SCAN, 99),
                    InputEvent(0, 0, e.EV_KEY, c, v), InputEvent(0, 0, e.EV_SYN, 0, 0)]
        dev.push(*K(e.KEY_A, 1), *K(e.KEY_A, 0), *K(e.KEY_COMPOSE, 1), *K(e.KEY_Q, 1), *K(e.KEY_Q, 0),
                 *K(e.KEY_3, 1), *K(e.KEY_3, 0), *K(e.KEY_COMPOSE, 0), *K(e.KEY_B, 1), *K(e.KEY_B, 0))
        wait_for(lambda: keys == [("KEY_A", 1), ("KEY_A", 0), ("KEY_B", 1), ("KEY_B", 0)],
                 "keys forwarded, Menu+Q+3 swallowed, scancodes dropped")
        chk(leds()["left"] == "FFFF00", "Menu+Q+3 set left yellow")

        ui.push(InputEvent(0, 0, e.EV_LED, e.LED_CAPSL, 1))
        red = "FF0000"
        wait_for(lambda: leds()["left"] == red and leds()["center"] == C.caps_other,
                 "caps LED event on virtual keyboard -> left red, others white")
        wait_for(lambda: 0x20 < green("left") < 0xE0, "main loop draws in-between fade frames")
        wait_for(lambda: green("left") >= 0xE0, "left fades to white")
        wait_for(lambda: green("left") <= 0x20, "and back to red")
        ui.push(InputEvent(0, 0, e.EV_LED, e.LED_NUML, 1))
        ui.push(InputEvent(0, 0, e.EV_LED, e.LED_CAPSL, 0))
        wait_for(lambda: leds()["left"] == "FFFF00", "caps off event -> colors restored (num lock ignored)")

        env = dict(os.environ, KBDRGB_LED=LED, KBDRGB_SOCKET=C.socket)
        subprocess.run([os.path.join(HERE, "kbdrgb"), "zones", "112233", "445566", "778899", "AABBCC"],
                       env=env, check=True, timeout=5)
        wait_for(lambda: os.path.exists(C.state_file) and json.load(open(C.state_file)) ==
                 {"left": "112233", "center": "445566", "right": "778899", "extra": "AABBCC", "fades": {}},
                 "real kbdrgb zones goes through the daemon, which saves the colors")
        chk(leds()["right"] == "778899", "and writes them")

        subprocess.run([os.path.join(HERE, "kbdrgb"), "cycle", "AA00AA", "00ff00", "-z", "center"],
                       env=env, check=True, timeout=5)
        wait_for(lambda: green("center") <= 0x20, "real kbdrgb cycle starts the cycle")
        wait_for(lambda: green("center") >= 0xE0, "main loop fades to the second color")
        wait_for(lambda: green("center") <= 0x20, "and back")
        subprocess.run([os.path.join(HERE, "kbdrgb"), "color", "445566"], env=env, check=True, timeout=5)
        saved: dict[str, object] = {**{z: "445566" for z in d.ZONES}, "fades": {}}
        wait_for(lambda: json.load(open(C.state_file)) == saved,
                 "real kbdrgb color ends the cycle and its color is saved")
        time.sleep(3 * C.cycle_delay)
        chk(set(leds().values()) == {"445566"}, "no cycle frame lands after kbdrgb color")
        subprocess.run([os.path.join(HERE, "kbdrgb"), "color", "000000", "-z", "left", "-t", "1"],
                       env=env, check=True, timeout=5)
        wait_for(lambda: leds()["left"] not in ("", "445566", "000000"), "kbdrgb color --time fades")
        wait_for(lambda: leds()["left"] == "000000", "and ends on the new color")

        wait_for(lambda: len(NOTIFIED) == 1 and "working again" in NOTIFIED[0][0],
                 "after running --stable-after seconds, a reported problem counts as recovered")
        subprocess.run([os.path.join(HERE, "kbdrgb"), "cycle", "AA00AA", "00ff00"], env=env, check=True, timeout=5)
        wait_for(lambda: 0x20 < green("left") < 0xE0, "a cycle is fading when the keyboard goes away")
        dev.kill()
        t.join(3)
        chk(not any(th.name == "frames" for th in threading.enumerate()), "main stops the frame thread on exit")
        stopped = leds()
        time.sleep(3 * d.FRAME)
        chk(leds() == stopped, "no frames are drawn after main exits")
        chk(not t.is_alive() and not errors and exits == [1], "main exits with error when the keyboard goes away")
        chk(d._load_problem(C).get("key") == "crash:OSError", "keyboard going away is recorded as a problem")
        chk(calls == ["grab", "ungrab", "ui-close", "dev-close"], "main ungrabs and closes on exit, in reverse order")
        chk(not any(th.name == "commands" for th in threading.enumerate()) and not os.path.exists(C.socket),
            "main stops answering kbdrgb and removes the socket on exit")
    finally:
        evdev.InputDevice, evdev.UInput.from_device = real_dev, real_from
        if os.path.exists(C.problem_file):
            os.unlink(C.problem_file)


def test_grab_failure() -> None:
    """A failed grab still closes everything opened before it."""
    C = config(state_file=os.path.join(TMP, "grab-state.json"), socket=os.path.join(TMP, "run3", "socket"),
               device=os.path.join(TMP, "fake-kbd"))
    sockets_before = socket_fds()
    open(C.device, "w").close()
    calls: list[str] = []

    class Dev(FakeFd):
        def grab(self) -> None: raise OSError("device busy")
        def close(self) -> None: calls.append("dev-close")

    class UI(FakeFd):
        def close(self) -> None: calls.append("ui-close")

    real_dev, real_from = evdev.InputDevice, evdev.UInput.from_device
    evdev.InputDevice = lambda p: Dev()
    evdev.UInput.from_device = staticmethod(lambda _dev, name=None: UI())
    sleeping = mock.patch.object(time, "sleep", lambda s: None)
    sleeping.start()
    try:
        try:
            d.main(argv(C))
            chk(False, "main exits when the grab fails")
        except SystemExit as ex:
            chk(ex.code == 1, "main exits with error when the grab fails")
        chk(calls == ["ui-close", "dev-close"], "failed grab closes the virtual and real keyboard")
        chk(socket_fds() == sockets_before and not os.path.exists(C.socket), "failed grab closes the socket")
        chk(d._load_problem(C).get("key") == "crash:OSError", "failed grab is recorded as a problem")
    finally:
        sleeping.stop()
        evdev.InputDevice, evdev.UInput.from_device = real_dev, real_from
        if os.path.exists(C.problem_file):
            os.unlink(C.problem_file)


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
    real_list, real_dev = evdev.list_devices, evdev.InputDevice
    evdev.list_devices = lambda: ["/dev/input/event9", "/dev/input/event3", "/dev/input/event7"]
    evdev.InputDevice = Dev
    try:
        chk(d.find_keyboard("keyd virtual keyboard") == "/dev/input/event7", "find_keyboard: by device name")
        chk(d.find_keyboard("nope") is None, "find_keyboard: unknown name -> None")
    finally:
        evdev.list_devices, evdev.InputDevice = real_list, real_dev

    fds_before = open_fds()
    C = config(state_file=os.path.join(TMP, "find-state.json"), socket=os.path.join(TMP, "run2", "socket"))
    for spec, what, code in [(os.path.join(TMP, "missing-event"), "missing keyboard", 1),
                             (C.virtual_name, "own virtual keyboard", 2)]:  # 2: argparse's, for bad options
        err = io.StringIO()
        try:
            with contextlib.redirect_stderr(err):
                d.main(argv(dataclasses.replace(C, device=spec)))
            chk(False, f"main exits for {what}")
        except SystemExit as ex:
            chk(ex.code == code, f"main exits with error for {what} (systemd retries)")
    chk("own virtual keyboard" in err.getvalue() and d._load_problem(C).get("key", "").startswith("config:"),
        "options that can't work are reported as a problem, like other failed starts")
    chk(open_fds() == fds_before, "early exits leave no fds open")


def test_config() -> None:
    """The daemon's command line: an option for each Config field."""
    parser = d.Config.parser()

    def parse(*args: str) -> d.Config | str:
        """The config for args, or argparse's error."""
        err = io.StringIO()
        try:
            with contextlib.redirect_stderr(err):
                c = d.Config.from_args(parser.parse_args(list(args)))
        except SystemExit:
            return err.getvalue().strip().splitlines()[-1]
        return c.problem() or c

    chk(parse() == d.Config(), "no options -> the defaults")
    c = d.Config(palette=("000000",) * 9 + ("123456",), palette_names=tuple(f"name {i}" for i in range(10)),
                 caps_ease=d.EASES["ease-in"], caps_delay=0.25, mod_key=e.KEY_RIGHTMETA,
                 device="keyd virtual keyboard", rainbow=("FF0000", "00FF00"))
    chk(parse(*argv(c)) == c, "every option reads back what it writes")
    chk(parse("--caps-colors", "ff0000,#00FF00", "--caps-ease", "0.4,0,0.2,1", "--mod-key", "rightalt")
        == d.Config(caps_colors=("FF0000", "00FF00"), caps_ease=(0.4, 0.0, 0.2, 1.0), mod_key=e.KEY_RIGHTALT),
        "colors, curves, and key names are read like the dialog and kbdrgb read them")
    for args, why in [(["--palette", "FF0000"], "--palette takes 10 colors"),
                      (["--palette-names", "a,b"], "--palette-names takes a name for each"),
                      (["--palette-names", "a,,b"], "has an empty name"),
                      (["--caps-delay", "0"], "must be more than 0"), (["--undo-depth", "-1"], "whole number"),
                      (["--caps-colors", "FF0000,zzz"], '"zzz" isn\'t a color'),
                      (["--caps-ease", "bouncy"], "isn't an easing curve"), (["--mod-key", "nope"], "isn't a key name"),
                      (["--rainbow", "FF0000"], "at least 2 colors"), (["--pulse-dim", "2"], "from 0 to 1"),
                      (["--device", d.Config().virtual_name], "own virtual keyboard"),
                      (["--max-cycle", "4"], "unrecognized arguments"), (["--bogus", "1"], "unrecognized arguments")]:
        got = parse(*args)
        chk(isinstance(got, str) and why in got, f"daemon rejects {' '.join(args)}: {got}")
    C = config(socket=os.path.join(TMP, "check", "socket"))
    d.main([*argv(C), "--check"])
    chk(not os.path.exists(C.socket), "--check only checks: the daemon doesn't start")
    err = io.StringIO()
    with contextlib.redirect_stderr(err), contextlib.suppress(SystemExit):
        d.main(["--check", "--palette", "FF0000"])
    chk("--palette takes 10 colors" in err.getvalue(), "--check reports what's wrong")


def test_problem_reports() -> None:
    NOTIFIED.clear()
    for i in range(1, 3):
        d.report_problem("not-found:x", "Can't find x.", CONFIG)
    chk(NOTIFIED == [], "no notification for the first 2 failed starts")
    d.report_problem("not-found:x", "Can't find x.", CONFIG)
    chk(len(NOTIFIED) == 1 and "Can't find x." in NOTIFIED[0][1], "notify on the 3rd failed start in a row")
    d.report_problem("not-found:x", "Can't find x.", CONFIG)
    chk(len(NOTIFIED) == 1, "only one notification per problem")
    d.report_recovered(CONFIG)
    chk(len(NOTIFIED) == 2 and "working again" in NOTIFIED[1][0] and not os.path.exists(CONFIG.problem_file),
        "recovery notification after a reported problem, marker cleared")
    d.report_recovered(CONFIG)
    chk(len(NOTIFIED) == 2, "no recovery notification when nothing was wrong")

    NOTIFIED.clear()
    d.report_problem("crash:OSError", "a", CONFIG); d.report_problem("crash:OSError", "a", CONFIG)
    d.report_problem("not-found:y", "b", CONFIG); d.report_problem("not-found:y", "b", CONFIG)
    chk(NOTIFIED == [], "a different problem restarts the count")
    d.report_problem("not-found:y", "b", CONFIG)
    chk(len(NOTIFIED) == 1, "new problem notifies after its own 3 failures")
    d.report_recovered(CONFIG); NOTIFIED.clear()

    real = d.desktop_notify
    shown: list[str] = []
    d.desktop_notify = lambda summary, body: False  # nobody logged in yet (e.g. at boot)
    try:
        for _ in range(4):
            d.report_problem("not-found:z", "c", CONFIG)
        def user_logged_in(summary: str, body: str) -> bool:
            shown.append(summary)
            return True
        d.desktop_notify = user_logged_in
        d.report_problem("not-found:z", "c", CONFIG)
        chk(len(shown) == 1, "notification retried until someone is logged in to see it")
        d.report_recovered(CONFIG)
    finally:
        d.desktop_notify = real
    d.report_problem("crash:X", "q", CONFIG); d.report_recovered(CONFIG)
    chk(not os.path.exists(CONFIG.problem_file) and NOTIFIED == [], "unreported problem clears silently on recovery")


def test_kbdrgb_cli() -> None:
    """kbdrgb against the daemon's socket code: what it runs, what it rejects, and what kbdrgb
    does on its own without the daemon."""
    kbdrgb = os.path.join(HERE, "kbdrgb")
    C = config(state_file=os.path.join(TMP, "cli-state.json"), socket=os.path.join(TMP, "cli", "socket"))
    env = dict(os.environ, KBDRGB_LED=LED, KBDRGB_SOCKET=C.socket)

    def k(*args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run([kbdrgb, *args], env=env, timeout=5, capture_output=True, text=True)

    ran: list[str] = []
    with FakeClock(), mock.patch.object(d.Action, "run", lambda self, lights: ran.append(str(self))):
        commands = d.CommandSocket(d.Lights(C))
        answering = threading.Thread(target=commands.answer)
        answering.start()
        try:
            for args in (["color", "ff0000"], ["color", "#00FF00", "-z", "left", "-t", "2"],
                         ["zones", "111111", "222222", "--time=0.5"], ["cycle", "AA00AA", "00FF00", "--zone", "center"],
                         ["cycle", "AA00AA", "00FF00", "0000FF", "-t", "2", "-d", "30"], ["rainbow"],
                         ["rainbow", "-z", "extra", "--for", "20"], ["pulse", "00ffaa", "--zone=right", "--time", "1.5"],
                         ["pulse", "00FFAA", "-e", "ease-in-out"], ["color", "112233", "-t", "2", "--ease=0.4,0,0.2,1"],
                         ["zones", "111111", "-e", "ease-out"], ["cycle", "ff0000", "FFFF00:ease-in", "#0000FF:0.9,0,0.1,1"],
                         ["zones", "111111:ease-in", "222222", "-t", "1"],
                         ["zones", "00FF00,0000FF", "0000FF 00FF00", "pulse,8800FF", "RAINBOW", "-t", "2"],
                         ["zones", "FF0000 00FF00 3 ease-in", "112233", "-t", "1", "-e", "ease-out"]):
                out = k(*args)
                chk(out.returncode == 0 and out.stderr == "", f"kbdrgb {' '.join(args)}")
            chk(ran == [
                "color -z all FF0000", "color -z left -t 2 00FF00",
                *[f"color -z {z} -t 0.5 {c}" for z, c in zip(d.ZONES, ["111111", "222222", "FFFFFF", "FFFFFF"])],
                "cycle -z center AA00AA 00FF00", "cycle -z all -t 2 -d 30 AA00AA 00FF00 0000FF",
                "rainbow -z all", "rainbow -z extra -d 20", "pulse -z right -t 1.5 00FFAA",
                "pulse -z all -e ease-in-out 00FFAA", "color -z all -t 2 -e 0.4,0,0.2,1 112233",
                *[f"color -z {z} -e ease-out {c}" for z, c in zip(d.ZONES, ["111111"] + ["FFFFFF"] * 3)],
                "cycle -z all FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1",
                *[f"color -z {z} -t 1 {c}" for z, c in zip(d.ZONES, ["111111:ease-in", "222222", "FFFFFF", "FFFFFF"])],
                "cycle -z left -t 2 00FF00 0000FF", "cycle -z center -t 2 0000FF 00FF00", "pulse -z right -t 2 8800FF",
                "rainbow -z extra -t 2", "cycle -z left -t 3 -e ease-in FF0000 00FF00",
                *[f"color -z {z} -t 1 -e ease-out {c}" for z, c in zip(d.ZONES[1:], ["112233", "FFFFFF", "FFFFFF"])]],
                "the daemon runs what kbdrgb asks for")
            ran.clear()
            for args in (["color"], ["color", "FF0000", "-d", "3"], ["zones", "-z", "left", "FF0000"],
                         ["zones", "1", "2", "3", "4", "5"], ["cycle", "AA00AA"], ["cycle", *["AA00AA"] * 17],
                         ["cycle", "AA00AA", "zzz"], ["rainbow", "FF0000"], ["rainbow", "-t", "0"], ["rainbow", "-t", "61"],
                         ["rainbow", "-d", "0.05"], ["pulse", "FF0000", "-z", "top"], ["color", "FF0000", "-t", "abc"],
                         ["color", "FF0000", "-t", "-1"], ["color", "FF0000", "-q"], ["cycle", "AA00AA", "00FF00", "-t"],
                         ["brightness", "300"], ["brightness"], ["status", "-z", "left"], ["bogus"],
                         ["cycle", "AA00AA", "00FF00", "-e", "bouncy"], ["pulse", "FF0000", "-e", "0.4,0,0.2"],
                         ["pulse", "FF0000", "-e", "0.4,0,0.2,1.5"], ["pulse", "FF0000", "-e", "0.4,0,0.2,1,"],
                         ["pulse", "FF0000", "-e", "0.4,-1,0.2,1"], ["brightness", "9", "-e", "ease"],
                         ["pulse", "FF0000:ease-in"], ["cycle", "FF0000:bouncy", "00FF00"], ["cycle", "FF0000:", "00FF00"],
                         ["cycle", "FF0000:0.4,0,0.2,2", "00FF00"], ["cycle", "zzz:ease-in", "00FF00"],
                         ["color", "FF0000\nFF0000"], ["color", "FF0000 00FF00"], ["zones", "FF0000,zzz"],
                         ["zones", "pulse,FF0000,00FF00"], ["zones", "FF0000,00FF00", "-t", "0"], ["zones", ""],
                         ["zones", "FF0000", "-d", "5"]):
                out = k(*args)
                chk(out.returncode == 1 and out.stderr.startswith("kbdrgb: "), f"kbdrgb rejects {' '.join(args)!r}")
            chk(ran == [], "rejected commands run nothing")
            for args, why in [(["cycle", "AA00AA"], "kbdrgb: cycle takes 2 to 16 colors.\n"),
                              (["zones", "-z", "left", "FF0000"], "kbdrgb: zones doesn't take --zone\n"),
                              (["zones", "FF0000", "rainbow,FF0000"], "kbdrgb: center zone: rainbow takes no colors.\n")]:
                chk(k(*args).stderr == why, f"kbdrgb shows the daemon's reason: {why.strip()}")
            out = k("status")
            chk(out.returncode == 0 and out.stdout.startswith("brightness: ") and "\nleft:   " in out.stdout
                and out.stdout.splitlines()[-1].startswith("kbdrgb zones "), "kbdrgb status asks the daemon")
            for args in (["-h"], ["cycle", "-h"], ["help"]):
                out = k(*args)
                chk(out.returncode == 0 and "--zone" in out.stdout, f"kbdrgb {' '.join(args)} shows usage")
            os.chmod(C.socket, 0o000)
            out = k("color", "321321")
            chk(out.returncode == 0 and out.stderr == "" and leds()["left"] == "321321" and ran == [],
                "kbdrgb color writes the LEDs itself when it can't use the socket")
            os.chmod(C.socket, 0o660)
        finally:
            commands.stop()
            answering.join(1)
            commands.listener.close()  # the socket file stays: a daemon that died without cleaning up
    chk(not answering.is_alive(), "stop() ends answer()")

    for what in ["nobody listening on the socket", "no socket"]:
        t0 = time.monotonic()
        out = k("color", "ABCDEF")
        chk(out.returncode == 0 and set(leds().values()) == {"ABCDEF"} and time.monotonic() - t0 < 2,
            f"kbdrgb color works without the daemon ({what})")
        chk(k("color", "123123", "-z", "right").returncode == 0 and leds()["right"] == "123123"
            and leds()["left"] == "ABCDEF", f"kbdrgb color --zone without the daemon ({what})")
        chk(k("zones", "010101", "020202").returncode == 0
            and leds() == {"left": "010101", "center": "020202", "right": "FFFFFF", "extra": "FFFFFF"},
            f"kbdrgb zones without the daemon ({what})")
        for args in (["rainbow"], ["cycle", "AA00AA", "00FF00"], ["pulse", "FF0000"], ["color", "FF0000", "-t", "1"],
                     ["color", "FF0000", "-e", "ease-in"], ["color", "FF0000:ease-in"], ["zones", "-z", "left", "FF0000"],
                     ["zones", "FF0000", "00FF00,0000FF"], ["zones", "Rainbow"], ["zones", "pulse 00FF00"]):
            out = k(*args)
            chk(out.returncode == 1 and "daemon" in out.stderr, f"kbdrgb {' '.join(args)} says it needs the daemon ({what})")
        os.unlink(C.socket) if os.path.exists(C.socket) else None
    for args in (["zones", "999999", "nope"], ["color", "999999", "-z", "top"], ["color", "999999", "888888"]):
        out = k(*args)
        chk(out.returncode == 1 and leds()["left"] != "999999", f"kbdrgb {' '.join(args)} without the daemon: rejected")
    chk(k("brightness", "100").returncode == 0 and open(f"{LED}/brightness").read() == "100", "kbdrgb brightness")
    out = k("status")
    chk(out.returncode == 0 and "left:   010101" in out.stdout and "brightness: 100" in out.stdout
        and out.stdout.endswith("kbdrgb zones 010101 020202 FFFFFF FFFFFF\n"), "kbdrgb status without the daemon")


def due_within(L: d.Lights, t: float, span: float) -> bool:
    """Whether L's next frame is due after t and no later than t + span."""
    due = L.next_due()
    return due is not None and t < due <= t + span


if __name__ == "__main__":
    import shutil
    try:
        test_state_migration()
        test_parse_hex()
        test_session_user()
        test_handler()
        test_held_cycles()
        test_undo()
        test_history()
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
        test_config()
        test_problem_reports()
        test_kbdrgb_cli()
        print("ALL OK")
    finally:
        shutil.rmtree(TMP, ignore_errors=True)
