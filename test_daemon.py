#!/usr/bin/python3 -I
"""Tests for kbd-rgb-daemon.py using fake LED files and fake input devices.

Run: python3 -I test_daemon.py   (no root, no hardware needed; touches only a temp dir)
"""
from __future__ import annotations

import importlib.util, json, os, stat, subprocess, tempfile, threading, time, types
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
        chk(L.next_due() == clock.t + d.FRAME, "caps on -> frames every FRAME seconds")

        def at(t: float) -> str:
            clock.t = 1000.0 + t * d.CAPS_DELAY
            L.tick()
            return leds()["left"]
        chk(at(0.5 * d.FRAME) == red, "no frame before FRAME is up")
        chk(at(0.5) == d.mix(red, white, 0.5), "halfway through CAPS_DELAY: halfway to white")
        chk(at(1) == white and at(1.75) == d.mix(white, red, 0.75) and at(2) == red, "fades to white and back")
        chk(at(1000.5) == d.mix(red, white, 0.5) and L.next_due() == clock.t + d.FRAME,
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


def test_external_changes() -> None:
    d.STATE_FILE = os.path.join(TMP, "state", "state.json")
    L = d.Lights()
    L.set(d.ALL, "FFFFFF")
    def saved() -> dict[str, str]:
        result: dict[str, str] = json.load(open(d.STATE_FILE))
        return result

    def ext(**zones: str) -> None:  # simulate kbdrgb writing sysfs directly
        for z, c in zones.items():
            open(f"{d.LED}/color_{z}", "w").write(c + "\n")

    chk(L.sync_external() is False, "sync with no external change does nothing")
    ext(left="123456", center="ABCDEF")
    chk(L.sync_external() is True and saved()["left"] == "123456" and saved()["center"] == "ABCDEF",
        "sync adopts and saves external colors immediately")
    L.set_caps(True)
    chk(L.sync_external() is False and saved()["left"] == "123456", "blinking left zone isn't adopted as a color")
    ext(right="00FF00")
    chk(L.sync_external() is True and saved()["right"] == "00FF00", "sync adopts other zones during caps lock")
    ext(left="654321")
    chk(L.sync_external() is True and saved()["left"] == "654321", "sync adopts a color kbdrgb wrote to left")
    L.set_caps(False)
    chk(leds()["left"] == "654321" and leds()["right"].strip() == "00FF00", "caps off shows the adopted colors")


def test_cycles() -> None:
    d.STATE_FILE = os.path.join(TMP, "cycle-state.json")
    if os.path.exists(d.STATE_FILE):
        os.unlink(d.STATE_FILE)

    def cycles(L: Any) -> dict[str, list[str]]:
        return {z: f.colors for z, f in L.fades.items()}

    def cycle(target: str, colors: list[str]) -> None:
        L.animate(d.cycle_fades(target, colors), "test cycle")

    with FakeClock() as clock:
        L = d.Lights()
        L.set(d.ALL, "8800FF")

        def at(t: float) -> dict[str, str]:  # t: CYCLE_DELAYs since 1000.0
            clock.t = 1000.0 + t * d.CYCLE_DELAY
            L.tick()
            return leds()

        cycle("center", ["AA00AA", "00FF00"])
        chk(leds()["center"] == "AA00AA" and leds()["left"] == "8800FF", "cycle starts on its first color, one zone")
        chk(L.next_due() == clock.t + d.FRAME, "cycling -> frames every FRAME seconds")
        chk(at(0.5)["center"] == d.mix("AA00AA", "00FF00", 0.5), "fades halfway in half a CYCLE_DELAY")
        chk(at(1)["center"] == "00FF00" and at(2)["center"] == "AA00AA", "reaches the second color and fades back")
        chk(leds()["left"] == "8800FF", "other zones stay put")
        chk(json.load(open(d.STATE_FILE))["fades"] ==
            {"center": {"colors": ["AA00AA", "00FF00"], "period": d.CYCLE_DELAY, "offset": 0.0}}, "cycle saved")
        L2 = d.Lights()
        chk(cycles(L2) == {"center": ["AA00AA", "00FF00"]} and L2.next_due() is not None, "cycle resumes after restart")

        cycle("right", ["111111", "222222", "333333"])  # at 2
        chk(leds()["center"] == "AA00AA" and leds()["right"] == "111111", "a new cycle restarts all cycles together")
        chk(at(4)["center"] == "AA00AA" and leds()["right"] == "333333", "cycles of different lengths")
        L.set("center", "0000FF")
        chk(leds()["center"] == "0000FF" and cycles(L) == {"right": ["111111", "222222", "333333"]},
            "setting a zone's color ends only its cycle")
        chk(at(5)["center"] == "0000FF" and leds()["right"] == "111111", "ended zone stays put, others keep cycling")
        L.set(d.ALL, "FFFFFF")
        chk(L.fades == {} and L.next_due() is None, "setting all zones ends all cycles")

        cycle(d.ALL, ["AA00AA", "00FF00"])  # at 5
        chk(set(leds().values()) == {"AA00AA"}, "cycle all zones")
        at(5.25)
        L.stop_fades()
        chk(L.fades == {} and L.next_due() is None and set(L.color.values()) == {d.mix("AA00AA", "00FF00", 0.25)},
            "stop leaves each zone on the color it was showing")
        chk(json.load(open(d.STATE_FILE))["fades"] == {}, "stopped cycles saved")

        cycle("left", ["AA00AA", "00FF00"])  # at 5.25
        L.set_caps(True)
        red, white = d.CAPS_COLORS
        clock.t = 1000.0 + 5.25 * d.CYCLE_DELAY + 0.5 * d.CAPS_DELAY
        L.tick()
        chk(leds()["left"] == d.mix(red, white, 0.5), "caps lock fade wins over a left-zone cycle")
        chk(at(6.25)["left"] != "00FF00", "left cycle stays hidden during caps lock")
        L.set_caps(False)
        chk(leds()["left"] == "00FF00", "left cycle shows again when caps lock turns off")
        L.set(d.ALL, "FFFFFF")

        d.handle_messages(L, b"cycle extra 123abc 456DEF\n")
        chk(cycles(L) == {"extra": ["123ABC", "456DEF"]}, "cycle message")
        for bad in [b"cycle extra 123ABC\n", b"cycle nowhere 123ABC 456DEF\n", b"cycle extra 123ABC zzzzzz\n",
                    b"cycle extra " + b"123ABC " * 17 + b"\n", b"bogus\n", b"\xff\xfe\n"]:
            d.handle_messages(L, bad)
        chk(cycles(L) == {"extra": ["123ABC", "456DEF"]}, "bad messages are ignored")
        d.handle_messages(L, b"stop\nsync\n")
        chk(L.fades == {}, "stop and sync in one read")
        good = {"colors": ["AA00AA", "00FF00"], "period": 1.0}
        json.dump({"left": "FFFFFF", "fades": {
            "left": {"colors": ["AA00AA"], "period": 1.0}, "nowhere": good,
            "center": {**good, "period": 0}, "right": {**good, "offset": 5}, "extra": {**good, "period": "x"}}},
            open(d.STATE_FILE, "w"))
        chk(d.Lights().fades == {}, "bad effects in the state file are ignored")
        json.dump({"left": "FFFFFF", "fades": {"right": {**good, "offset": 1.5}}}, open(d.STATE_FILE, "w"))
        chk(d.Lights().shown("right") == d.mix("00FF00", "AA00AA", 0.5), "saved effect resumes at its offset")
        json.dump({"left": "FFFFFF", "fades": ["junk"]}, open(d.STATE_FILE, "w"))
        chk(d.Lights().color["left"] == "FFFFFF", "malformed cycle entry doesn't break loading")


def test_effects() -> None:
    """rainbow and pulse: endless or timed, cleared by new colors."""
    d.STATE_FILE = os.path.join(TMP, "effect-state.json")
    with FakeClock() as clock:
        L = d.Lights()
        L.set(d.ALL, "FFFFFF")
        d.handle_messages(L, b"rainbow\n")
        chk(leds() == {"left": "FF0000", "center": d.mix("FF0000", "FFFF00", d.RAINBOW_LAG),
                       "right": d.mix("FFFF00", "00FF00", 2 * d.RAINBOW_LAG - 1), "extra": "00FF00"},
            "rainbow: each zone a step further around the wheel")
        clock.t += 6 * d.RAINBOW_DELAY
        L.tick()
        chk(leds()["left"] == "FF0000" and leds()["extra"] == "00FF00", "rainbow loops around the wheel")
        chk(len(json.load(open(d.STATE_FILE))["fades"]) == 4, "endless rainbow saved, to resume after a restart")
        chk(d.Lights().fades.keys() == L.fades.keys(), "rainbow resumes after restart")
        clock.t += 1000
        L.tick()
        chk(L.next_due() is not None, "endless rainbow keeps going")
        L.set("center", "123456")
        chk(set(L.fades) == {"left", "right", "extra"}, "a new zone color clears the rainbow there")
        L.set(d.ALL, "FFFFFF")

        d.handle_messages(L, b"pulse 00ff88 3\n")
        start = clock.t
        dim = d.mix("000000", "00FF88", d.PULSE_DIM)
        chk(set(leds().values()) == {"00FF88"}, "pulse starts on its color")
        clock.t = start + d.PULSE_DELAY
        L.tick()
        chk(set(leds().values()) == {dim}, "pulse fades to a dim copy")
        chk(json.load(open(d.STATE_FILE))["fades"] == {}, "timed effects aren't saved")
        clock.t = start + 2.9
        L.tick()
        chk(L.fades != {}, "timed pulse still running before its seconds are up")
        clock.t = start + 3.2
        L.tick()
        last = d.Fade(["00FF88", dim], d.PULSE_DELAY, start).at(start + 3)
        chk(L.fades == {} and L.next_due() is None and set(leds().values()) == {last}
            and json.load(open(d.STATE_FILE))["left"] == last,
            "timed pulse ends on the frame at its end time, and that color is saved")

        d.handle_messages(L, b"rainbow 2\n")
        cycle_at = clock.t
        d.handle_messages(L, b"cycle center AA00AA 00FF00\n")
        clock.t = cycle_at + 2.5
        L.tick()
        chk(set(L.fades) == {"center"}, "timed rainbow ends, an endless cycle started on top keeps going")
        L.set(d.ALL, "FFFFFF")
        for bad in [b"rainbow 0\n", b"rainbow -3\n", b"rainbow 1.5\n", b"rainbow 999999\n", b"rainbow 5 5\n",
                    b"pulse\n", b"pulse nope\n", b"pulse FF0000 x\n"]:
            d.handle_messages(L, bad)
        chk(L.fades == {}, "bad rainbow and pulse messages are ignored")


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
                 "real kbdrgb run notifies daemon, which saves its colors")

        subprocess.run([os.path.join(HERE, "kbdrgb"), "cycle", "AA00AA", "00ff00", "center"],
                       env=env, check=True, timeout=5)
        wait_for(lambda: green("center") <= 0x20, "real kbdrgb cycle starts the cycle")
        wait_for(lambda: green("center") >= 0xE0, "main loop fades to the second color")
        wait_for(lambda: green("center") <= 0x20, "and back")
        subprocess.run([os.path.join(HERE, "kbdrgb"), "color", "445566"], env=env, check=True, timeout=5)
        wait_for(lambda: json.load(open(d.STATE_FILE)) == {**{z: "445566" for z in d.ZONES}, "fades": {}},
                 "real kbdrgb color ends the cycle and its color is saved")
        time.sleep(3 * d.CYCLE_DELAY)
        chk(set(leds().values()) == {"445566"}, "no cycle step lands after kbdrgb color")

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


def test_kbdrgb_without_daemon() -> None:
    env = dict(os.environ, KBDRGB_LED=d.LED, KBDRGB_NOTIFY=os.path.join(TMP, "missing", "notify"))
    r = subprocess.run([os.path.join(HERE, "kbdrgb"), "color", "ABCDEF"], env=env, timeout=5)
    chk(r.returncode == 0 and set(leds().values()) == {"ABCDEF"}, "kbdrgb works when the daemon isn't running")
    fifo = os.path.join(TMP, "stale-notify")
    os.mkfifo(fifo)  # pipe exists but nobody is reading it
    r = subprocess.run([os.path.join(HERE, "kbdrgb"), "color", "123123"],
                       env=dict(env, KBDRGB_NOTIFY=fifo), timeout=5)
    chk(r.returncode == 0, "kbdrgb doesn't hang when no daemon is reading the pipe")
    for mode, what in [(0o200, "write-only"), (0o000, "no access")]:
        os.chmod(fifo, mode)
        out = subprocess.run([os.path.join(HERE, "kbdrgb"), "color", "321321"], env=dict(env, KBDRGB_NOTIFY=fifo),
                             timeout=5, capture_output=True, text=True)
        chk(out.returncode == 0 and out.stderr == "" and leds()["left"] == "321321",
            f"kbdrgb succeeds silently when it can't open the pipe ({what})")
    kbdrgb = os.path.join(HERE, "kbdrgb")
    out = subprocess.run([kbdrgb, "cycle", "AA00AA", "00FF00"], env=env, timeout=5, capture_output=True, text=True)
    chk(out.returncode == 1 and "daemon" in out.stderr, "kbdrgb cycle says it needs the daemon")
    for args, what in [(["AA00AA"], "one color"), (["AA00AA", "zzz"], "bad color"),
                       (["AA00AA", "00FF00", "left", "right"], "two zones"), (["AA00AA"] * 17, "too many colors")]:
        out = subprocess.run([kbdrgb, "cycle", *args], env=dict(env, KBDRGB_NOTIFY=fifo),
                             timeout=5, capture_output=True, text=True)
        chk(out.returncode == 1 and out.stderr != "", f"kbdrgb cycle rejects {what}")
    os.chmod(fifo, 0o600)
    rd = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)  # stand in for the daemon reading the pipe
    try:
        subprocess.run([kbdrgb, "cycle", "aa00aa", "#00FF00", "left"], env=dict(env, KBDRGB_NOTIFY=fifo),
                       check=True, timeout=5)
        subprocess.run([kbdrgb, "cycle", "AA00AA", "00FF00", "0000FF"], env=dict(env, KBDRGB_NOTIFY=fifo),
                       check=True, timeout=5)
        subprocess.run([kbdrgb, "zones", "111111"], env=dict(env, KBDRGB_NOTIFY=fifo), check=True, timeout=5)
        for args in (["rainbow"], ["rainbow", "20"], ["pulse", "00ffaa"], ["pulse", "00FFAA", "5"]):
            subprocess.run([kbdrgb, *args], env=dict(env, KBDRGB_NOTIFY=fifo), check=True, timeout=5)
        chk(os.read(rd, 4096) == b"cycle left AA00AA 00FF00\ncycle all AA00AA 00FF00 0000FF\nstop\nsync\n"
            b"rainbow\nrainbow 20\npulse 00FFAA\npulse 00FFAA 5\n",
            "kbdrgb sends effect messages, and stop before writing colors")
    finally:
        os.close(rd)
    for args in (["rainbow", "0"], ["rainbow", "x"], ["pulse", "nope"], ["pulse", "FF0000", "1.5"]):
        out = subprocess.run([kbdrgb, *args], env=env, timeout=5, capture_output=True, text=True)
        chk(out.returncode == 1 and out.stderr != "", f"kbdrgb rejects {' '.join(args)}")
    subprocess.run([kbdrgb, "color", "FFFFFF"], env=env, check=True, timeout=5)
    t0 = time.monotonic()
    subprocess.run([kbdrgb, "rainbow", "1"], env=env, check=True, timeout=10)
    chk(time.monotonic() - t0 >= 0.5 and leds()["left"] != "FFFFFF", "without the daemon, kbdrgb runs rainbow itself")
    out = subprocess.run([kbdrgb, "zones", "999999", "nope"], env=env, timeout=5, capture_output=True, text=True)
    chk(out.returncode == 1 and leds()["left"] != "999999", "kbdrgb zones checks every color before writing any")


if __name__ == "__main__":
    import shutil
    try:
        test_state_migration()
        test_parse_hex()
        test_session_user()
        test_handler()
        test_fade()
        test_caps_and_persistence()
        test_external_changes()
        test_cycles()
        test_effects()
        test_main_loop()
        test_grab_failure()
        test_find_keyboard()
        test_problem_reports()
        test_kbdrgb_without_daemon()
        print("ALL OK")
    finally:
        shutil.rmtree(TMP, ignore_errors=True)
