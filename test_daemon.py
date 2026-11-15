#!/usr/bin/python3 -I
"""Tests for kbd-rgb-daemon.py using fake LED files and fake input devices.

Run: python3 -I test_daemon.py   (no root, no hardware needed; touches only a temp dir)
"""
import importlib.util, json, os, tempfile, types
from evdev import ecodes as e, InputEvent

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("d", os.path.join(HERE, "kbd-rgb-daemon.py"))
d = importlib.util.module_from_spec(spec)
spec.loader.exec_module(d)

TMP = tempfile.mkdtemp(prefix="kbd-rgb-test-")
d.LED = os.path.join(TMP, "led")
os.makedirs(d.LED)
for z in d.ZONES:
    open(f"{d.LED}/color_{z}", "w").write("FFFFFF")

M = "COMPOSE"  # the Menu key


def chk(cond, msg):
    assert cond, msg
    print("ok:", msg)


def leds():
    return {z: open(f"{d.LED}/color_{z}").read() for z in d.ZONES}


def test_state_migration():
    d.STATE_FILE = os.path.join(TMP, "old.json")
    json.dump({"left": 7, "center": 0, "right": 0, "extra": 0}, open(d.STATE_FILE, "w"))
    chk(d.Lights().color["left"] == "0000FF", "old palette-index state migrates to hex")


def test_parse_hex():
    for s, want in [("#ff8800", "FF8800"), ("f80", "FF8800"), (" 00aaff\n", "00AAFF"),
                    ("xyz", None), ("12345", None), (None, None)]:
        chk(d.parse_hex(s) == want, f"parse_hex({s!r}) == {want!r}")


def test_session_user():
    real_run = d.subprocess.run
    def fake(replies):
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


def test_handler():
    d.STATE_FILE = os.path.join(TMP, "state", "state.json")
    L = d.Lights()
    L.set(d.ALL, "FFFFFF")
    out, dialogs = [], []
    h = d.Handler(L, lambda c, v: out.append((e.KEY[c], v)), dialogs.append)

    def press(*keys):
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


def test_caps_and_persistence():
    d.STATE_FILE = os.path.join(TMP, "state", "state.json")
    L = d.Lights()
    for z in d.ZONES:
        L.set(z, "8800FF")
    L.set_caps(True)
    chk(leds() == {"left": d.CAPS_COLOR, "center": "FFFFFF", "right": "FFFFFF", "extra": "FFFFFF"},
        "caps overlay: left CAPS_COLOR, others white")
    L.set_caps(False)
    chk(set(leds().values()) == {"8800FF"}, "caps off restores saved colors")
    chk(json.load(open(d.STATE_FILE))["extra"] == "8800FF", "colors saved as hex")
    chk(d.Lights().color["center"] == "8800FF", "colors reload from state file")


def test_main_wiring():
    """main(): scancodes dropped, keys forwarded, combos swallowed, grab released at end."""
    d.STATE_FILE = os.path.join(TMP, "state", "state.json")
    d.time.sleep = lambda s: None
    real_thread = d.threading.Thread
    d.threading.Thread = lambda **k: types.SimpleNamespace(start=lambda: None)
    events = []
    for code, val in [(e.KEY_A, 1), (e.KEY_A, 0), (e.KEY_COMPOSE, 1), (e.KEY_Q, 1), (e.KEY_Q, 0),
                      (e.KEY_COMPOSE, 0), (e.KEY_B, 1), (e.KEY_B, 0)]:
        events += [InputEvent(0, 0, e.EV_MSC, e.MSC_SCAN, 99), InputEvent(0, 0, e.EV_KEY, code, val),
                   InputEvent(0, 0, e.EV_SYN, 0, 0)]
    calls, keys = [], []

    class Dev:
        path, name = "/dev/fake", "fake"
        def read_loop(self): yield from events
        def grab(self): calls.append("grab")
        def ungrab(self): calls.append("ungrab")

    class UI:
        device = types.SimpleNamespace(path="/dev/fakeui")
        def write_event(self, ev):
            assert ev.type != e.EV_MSC, "scancode forwarded"
            if ev.type == e.EV_KEY:
                keys.append((e.KEY[ev.code], ev.value))
        def write(self, t, c, v): keys.append((e.KEY[c], v))
        def syn(self): pass
        def close(self): calls.append("close")

    real_dev, real_from = d.evdev.InputDevice, d.evdev.UInput.from_device
    d.evdev.InputDevice = lambda p: Dev()
    d.evdev.UInput.from_device = staticmethod(lambda dev, name=None: UI())
    try:
        d.main()
    finally:
        d.evdev.InputDevice, d.evdev.UInput.from_device = real_dev, real_from
        d.threading.Thread = real_thread
    chk(keys == [("KEY_A", 1), ("KEY_A", 0), ("KEY_B", 1), ("KEY_B", 0)], "main forwards keys, swallows Menu+Q")
    chk(calls == ["grab", "ungrab", "close"], "main grabs, then ungrabs and closes on exit")


if __name__ == "__main__":
    import shutil
    try:
        test_state_migration()
        test_parse_hex()
        test_session_user()
        test_handler()
        test_caps_and_persistence()
        test_main_wiring()
        print("ALL OK")
    finally:
        shutil.rmtree(TMP, ignore_errors=True)
