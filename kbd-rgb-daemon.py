#!/usr/bin/python3 -I
"""System76 keyboard color daemon (runs as root).

While holding Menu (release Menu at any point to start over):
  non-digit key  -> select that key's zone (Space selects the whole keyboard)
  0-9            -> set the selected zone to PALETTE[n] (0 = white); top-row digits only
                    (numpad digits select the numpad zone).
                    With no zone selected, digits color the whole keyboard.
                    Digits never select a zone. The zone stays selected, so more digits
                    retry colors; another non-digit key selects a different zone.
  digits while still holding the zone key
                 -> cycle through those colors, starting when you let go of the zone key.
                    The time between the first two digits sets how long each color lasts.
                    Press the zone key again to start over.
  same key again -> open a dialog on the desktop to type a color or effect for that selection
                    (see README.md)
  Backspace      -> reset the whole keyboard to white (any time)
Tap Menu alone   -> normal Menu key
kbdrgb           -> colors and effects (color, cycle, rainbow, pulse) for any zone, each with
                    its own transition time. Effects run until the zone gets a new color
                    (kbdrgb, Menu combo, color dialog), or for the seconds given.
Caps lock on     -> left zone fades through CAPS_COLORS, CAPS_DELAY seconds each; other
                    zones show CAPS_OTHER. Their colors and effects come back when caps lock
                    turns off (changes made meanwhile show then)

Keys pressed while Menu is held are not passed to apps. The keyboard is
grabbed exclusively and all other input is re-emitted via a uinput device.
Zone colors (hex) persist in STATE_FILE.

The main loop sleeps in select() on:
  - the keyboard            -> key events
  - the uinput device       -> LED events the desktop sends it (caps lock on/off)
  - the NOTIFY named pipe   -> commands from the kbdrgb CLI, one per line:
                                 NAME [zone=Z] [time=SECONDS] [for=SECONDS] [COLOR...]
                               See COMMANDS. zone: a zone or "all" (default). time: seconds to
                               fade from one color to the next. for: end the effect after that
                               long (otherwise it runs until the zone gets a new color).
Fades (caps lock and effects) are drawn by a separate thread, Lights.render(), while anything
is fading: each zone up to once per FRAME, and at most WRITE_RATE writes a second in all. Slow
sysfs writes never delay key forwarding.
"""
from __future__ import annotations

import contextlib, glob, json, math, os, pwd, re, select, subprocess, sys, threading, time, traceback
from collections.abc import Callable, Iterator, Sequence
from typing import NamedTuple, Protocol, TypeAlias, TypedDict

import evdev
from evdev import InputEvent, ecodes as e

Zone: TypeAlias = str    # one of ZONES
Target: TypeAlias = str  # a zone, or ALL for the whole keyboard
Color: TypeAlias = str   # "RRGGBB", uppercase hex

# Keyboard to grab: a device path or a device name. Set KBD_DEVICE (install.sh --device) to
# use another device, e.g. a key remapper's virtual keyboard. Default: the built-in keyboard.
KBD: str = os.environ.get("KBD_DEVICE") or "/dev/input/by-path/platform-i8042-serio-0-event-kbd"
VIRTUAL_NAME = "kbd-rgb virtual keyboard"
LED = "/sys/class/leds/system76::kbd_backlight"
STATE_FILE = "/var/lib/kbd-rgb/state.json"
NOTIFY = "/run/kbd-rgb/notify"
PROBLEM_FILE = "/run/kbd-rgb/problem.json"  # current startup problem, for desktop notifications
REPORT_AFTER = 3     # failed starts in a row before notifying the desktop user
STABLE_AFTER = 30.0  # seconds of running before a reported problem counts as fixed
CAPS_GLOB = "/sys/class/leds/input*::capslock/brightness"
MOD_KEY = e.KEY_COMPOSE
CAPS_COLORS: list[Color] = ["FF0000", "FFFFFF"]  # left zone fades through these
CAPS_DELAY = 0.7   # seconds to fade from one caps lock color to the next
CAPS_OTHER: Color = "FFFFFF"  # the other zones while caps lock is on
CYCLE_DELAY = 1.0  # seconds to fade from one kbdrgb cycle color to the next
FRAME = 0.1        # shortest time between one zone's fade frames (10 per second)
WRITE_RATE = 30.0  # most color writes per second across all zones. The keyboard controller
                   # keeps up with 30 but not 40: zones written later (the numpad) fall behind.
RAINBOW: list[Color] = ["FF0000", "FFFF00", "00FF00", "00FFFF", "0000FF", "FF00FF"]  # hue wheel
RAINBOW_DELAY = 0.6  # seconds per rainbow color (once around the wheel in 3.6 s)
RAINBOW_LAG = 2 / 3  # how many rainbow colors each zone trails the one before it (40 degrees)
PULSE_DELAY = 0.5    # seconds to fade between the pulse color and its dim copy
PULSE_DIM = 0.08     # brightness of the dim copy
MAX_CYCLE = 16     # most colors in one cycle
PALETTE: list[Color] = ["FFFFFF", "FF0000", "FF6600", "FFFF00", "00FF00",
                         "00FFAA", "0088FF", "0000FF", "8800FF", "FF00AA"]
ZONES: list[Zone] = ["left", "center", "right", "extra"]
ALL: Target = "all"

# Approximate key -> zone mapping (zones blend, so this only needs to be close).
_Z: dict[Zone, str] = {
    "left": """ESC F1 F2 F3 F4 GRAVE TAB Q W E R CAPSLOCK A S D F
               LEFTSHIFT Z X C V B LEFTCTRL LEFTMETA LEFTALT 102ND""",
    "center": """F5 F6 F7 F8 F9 T Y U I G H J K N M COMMA""",
    "right": """F10 F11 F12 SYSRQ PRINT INSERT DELETE
                MINUS EQUAL O P LEFTBRACE RIGHTBRACE BACKSLASH L SEMICOLON
                APOSTROPHE ENTER DOT SLASH RIGHTSHIFT RIGHTALT COMPOSE RIGHTCTRL
                UP DOWN LEFT RIGHT""",
    "extra": """HOME END PAGEUP PAGEDOWN NUMLOCK KPSLASH KPASTERISK KPMINUS KPPLUS KPENTER KPDOT
                KP0 KP1 KP2 KP3 KP4 KP5 KP6 KP7 KP8 KP9""",
}
KEY_ZONE: dict[int, Target] = {}
for zone, names in _Z.items():
    for n in names.split():
        code = e.ecodes.get("KEY_" + n)
        if code is not None:
            KEY_ZONE[code] = zone
KEY_ZONE[e.KEY_SPACE] = ALL

DIGIT: dict[int, int] = {getattr(e, f"KEY_{n}"): n for n in range(10)}


class ProblemState(TypedDict, total=False):
    key: str        # which problem, e.g. "not-found:<device>" or "crash:OSError"
    count: int      # failed starts in a row with this problem
    notified: bool  # whether the desktop user has been told


class EventSource(Protocol):
    """An input device we read events from: the keyboard or our uinput device."""
    fd: int
    def read(self) -> Iterator[InputEvent]: ...


def log(*a: object) -> None:
    print(*a, flush=True)


def parse_hex(s: str | None) -> Color | None:
    s = (s or "").strip().lstrip("#")
    if re.fullmatch(r"[0-9A-Fa-f]{3}", s):
        s = "".join(c * 2 for c in s)
    return s.upper() if re.fullmatch(r"[0-9A-Fa-f]{6}", s) else None


def now() -> float:
    """Clock for fades (tests replace it)."""
    return time.monotonic()


def mix(a: Color, b: Color, f: float) -> Color:
    """The color a fraction f (0-1) of the way from a to b."""
    ca, cb = bytes.fromhex(a), bytes.fromhex(b)
    return "".join(f"{int(x + (y - x) * f + 0.5):02X}" for x, y in zip(ca, cb))


class Fade:
    """Colors a zone fades through, looping: `period` seconds from each color to the next.
    Used for the caps lock indicator and the kbdrgb cycle, rainbow, and pulse effects.
    `offset` (in colors) starts the zone partway along; `until` ends a timed effect."""

    def __init__(self, colors: Sequence[Color], period: float, start: float,
                 offset: float = 0.0, until: float | None = None) -> None:
        self.colors, self.period, self.start = list(colors), period, start
        self.offset, self.until = offset, until

    def at(self, t: float) -> Color:
        steps = max(0.0, t - self.start) / self.period + self.offset
        i = int(steps)
        n = len(self.colors)
        return mix(self.colors[i % n], self.colors[(i + 1) % n], steps - i)

    def saved(self) -> dict[str, object]:
        return {"colors": self.colors, "period": self.period, "offset": self.offset}


def load_fade(v: object) -> Fade | None:
    """A Fade from the state file, or None if it isn't valid."""
    if not isinstance(v, dict):
        return None
    colors, period, offset = parse_cycle(v.get("colors")), v.get("period"), v.get("offset", 0.0)
    if colors and isinstance(period, (int, float)) and 0.05 <= period <= 60 \
            and isinstance(offset, (int, float)) and 0 <= offset < len(colors):
        return Fade(colors, float(period), EPOCH, float(offset))
    return None


class Command(NamedTuple):
    colors: tuple[int, int]        # how many colors it takes (min, max)
    time: Callable[[], float]      # default transition time
    min_time: float                # shortest transition time allowed
    timed: bool                    # whether it takes for=SECONDS


# Commands kbdrgb sends. Defaults are looked up when a command runs, so settings changes apply.
COMMANDS: dict[str, Command] = {
    "set":     Command((1, 1), lambda: 0.0, 0.0, False),   # fade to a color (time=0: at once)
    "cycle":   Command((2, MAX_CYCLE), lambda: CYCLE_DELAY, 0.1, True),
    "rainbow": Command((0, 0), lambda: RAINBOW_DELAY, 0.1, True),
    "pulse":   Command((1, 1), lambda: PULSE_DELAY, 0.1, True),
}
MAX_TIME = 60.0       # longest transition time
MAX_FOR = 86400.0     # longest timed effect


EPOCH = 0.0  # effects count from here on the now() clock, so equal effects stay in step


def effect_fade(name: str, zone: Zone, colors: list[Color], period: float) -> Fade:
    """The fade a looping effect gives one zone."""
    if name == "rainbow":  # each zone RAINBOW_LAG colors further around the wheel than the last
        return Fade(RAINBOW, period, EPOCH, offset=ZONES.index(zone) * RAINBOW_LAG)
    if name == "pulse":    # between the color and a dim copy of it
        return Fade([colors[0], mix("000000", colors[0], PULSE_DIM)], period, EPOCH)
    return Fade(colors, period, EPOCH)  # cycle


class Lights:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.frames_due = threading.Condition(self.lock)  # wakes render() when fading starts
        self.color: dict[Zone, Color] = {z: PALETTE[0] for z in ZONES}
        self.caps = False
        self.caps_fade: Fade | None = None     # left zone while caps lock is on
        self.fades: dict[Zone, Fade] = {}      # zones running an effect (kbdrgb cycle, rainbow, pulse)
        self.written: dict[Zone, Color] = {}   # last color written to each zone
        self.next_frame: float | None = None   # when the next zone frame is due, if any
        self.drawn: dict[Zone, float] = {}     # when each zone was last drawn
        self.last_write = -1.0                 # when any zone was last written
        try:
            with open(STATE_FILE) as f:
                saved = json.load(f)
            for z in ZONES:
                v = saved.get(z)
                if isinstance(v, int):  # old format: palette index
                    self.color[z] = PALETTE[v % len(PALETTE)]
                elif (c := parse_hex(v)) is not None:
                    self.color[z] = c
            for z, v in dict(saved.get("fades") or {}).items():
                if z in ZONES and (fade := load_fade(v)):
                    self.fades[z] = fade
        except (OSError, ValueError, AttributeError, TypeError):
            pass
        with self.lock:
            self._schedule()
            self.apply()

    def write(self, zone: Zone, color: Color) -> None:
        try:
            with open(f"{LED}/color_{zone}", "w") as f:
                f.write(color)
            self.written[zone] = color
            self.drawn[zone] = self.last_write = now()
        except OSError as ex:
            log("write failed:", zone, ex)

    def apply(self) -> None:
        t = now()
        for z in ZONES:
            self.write(z, self.shown(z, t))

    def active_fade(self, zone: Zone) -> Fade | None:
        """The fade a zone is showing. While caps lock is on, that's caps lock's on the left
        zone, and none on the others (they show CAPS_OTHER); their effects carry on unseen."""
        if self.caps:
            return self.caps_fade if zone == "left" else None
        return self.fades.get(zone)

    def shown(self, zone: Zone, t: float | None = None) -> Color:
        """What a zone displays at time t (default now): its fade's color, or its color."""
        if self.caps and zone != "left":
            return CAPS_OTHER
        fade = self.active_fade(zone)
        return fade.at(now() if t is None else t) if fade else self.color[zone]

    def _schedule(self) -> None:
        """When the next zone frame is due, or None if nothing is fading (lock held): once some
        fading zone is FRAME past its last frame, and 1 / WRITE_RATE past the last write.
        Wakes render() if frames just started."""
        fading = [z for z in ZONES if self.active_fade(z)]
        was = self.next_frame
        if fading:
            ready = min(self.drawn.get(z, -math.inf) + FRAME for z in fading)
            self.next_frame = max(ready, self.last_write + 1 / WRITE_RATE)
        else:
            self.next_frame = None
        if was is None and self.next_frame is not None:
            self.frames_due.notify()

    def save(self) -> None:
        """Save colors, and effects that run until cleared (timed ones end on their own)."""
        try:
            os.makedirs(os.path.dirname(STATE_FILE), exist_ok=True)
            tmp = STATE_FILE + ".tmp"
            fades = {z: f.saved() for z, f in self.fades.items() if f.until is None}
            with open(tmp, "w") as f:
                json.dump({**self.color, "fades": fades}, f)
            os.replace(tmp, STATE_FILE)
        except OSError as ex:
            log("save failed:", ex)

    def get(self, target: Target) -> Color:
        return self.color["left" if target == ALL else target]

    def set(self, target: Target, color: Color, seconds: float = 0.0) -> None:
        """Give zones a new color, ending their effects. With seconds, fade there from what
        each zone is showing."""
        with self.lock:
            t = now()
            zones = ZONES if target == ALL else [target]
            for z in zones:
                fade = self.fades.pop(z, None)  # a new color ends the zone's effect
                if seconds > 0:
                    start = fade.at(t) if fade else self.color[z]
                    self.fades[z] = Fade([start, color], seconds, t, until=t + seconds)
                self.color[z] = color
            self._schedule()
            self._draw(zones, t); self.save()
        log(f"{target} -> {color}" + (f" over {seconds:g}s" if seconds else ""))

    def _draw(self, zones: list[Zone], t: float) -> None:
        """Write what zones show now (lock held)."""
        for z in zones:
            self.write(z, self.shown(z, t))

    def animate(self, fades: dict[Zone, Fade], what: str, seconds: float | None = None) -> None:
        """Run an effect on some zones until set(), or for `seconds`. Effect fades count from
        EPOCH, not from when they start, so effects with the same colors and time match
        however far apart their commands ran."""
        with self.lock:
            t = now()
            for z, fade in fades.items():
                fade.until = None if seconds is None else t + seconds
                self.fades[z] = fade
                self.color[z] = fade.at(t)
            self._schedule()
            self._draw(list(fades), t); self.save()
        log(what, f"for {seconds:g}s" if seconds else "until cleared")

    def _end(self, zone: Zone, t: float) -> None:
        """End a zone's effect, leaving it on the color it's showing (lock held)."""
        self.color[zone] = self.fades.pop(zone).at(t)

    def set_caps(self, on: bool) -> None:
        with self.lock:
            if on != self.caps:
                self.caps = on
                self.caps_fade = Fade(CAPS_COLORS, CAPS_DELAY, now()) if on else None
                self._schedule()
                self._draw(ZONES, now())

    def next_due(self) -> float | None:
        """When the next frame is due, or None if nothing is fading."""
        return self.next_frame

    def render(self) -> None:
        """Draw fade frames forever. Runs in its own thread: each sysfs color write takes
        several milliseconds, and the main loop has keys to forward."""
        while True:
            try:
                with self.lock:
                    while self.next_frame is None:
                        self.frames_due.wait()
                    delay = self.next_frame - now()
                if delay > 0:
                    time.sleep(delay)
                self.tick()
            except Exception:
                log("frame failed:\n" + traceback.format_exc())
                time.sleep(1)

    def tick(self) -> None:
        """Draw one fading zone's frame, the one that has waited longest, and end timed
        effects whose time is up. One zone per call, at most WRITE_RATE calls a second (see
        _schedule), so the keyboard controller keeps up: one to three fading zones get FRAME
        frames each, and four share WRITE_RATE."""
        with self.lock:
            t = now()
            if self.next_frame is None or t < self.next_frame:
                return
            expired = [z for z, f in self.fades.items() if f.until is not None and t >= f.until]
            for z in expired:  # final colors go out right away
                self._end(z, self.fades[z].until or t)
                if (c := self.shown(z, t)) != self.written.get(z):
                    self.write(z, c)
            ready = [z for z in ZONES if self.active_fade(z) and t - self.drawn.get(z, -math.inf) >= FRAME - 1e-9]
            if ready:
                z = min(ready, key=lambda z: self.drawn.get(z, -math.inf))
                if (c := self.shown(z, t)) != self.written.get(z):
                    self.write(z, c)
                self.drawn[z] = t
            self._schedule()
            if expired:
                self.save()
                log("effect ended:", " ".join(f"{z}={self.color[z]}" for z in expired))



def parse_cycle(colors: object) -> list[Color] | None:
    """Validated cycle colors (2 to MAX_CYCLE hex colors), or None."""
    if not isinstance(colors, list) or not 2 <= len(colors) <= MAX_CYCLE:
        return None
    parsed = [parse_hex(c) if isinstance(c, str) else None for c in colors]
    return [c for c in parsed if c] if all(parsed) else None


def parse_number(s: str, low: float, high: float) -> float | None:
    """A plain decimal number (like 2 or 0.5) from low to high, or None."""
    if not re.fullmatch(r"[0-9]{1,6}(\.[0-9]{1,6})?", s):
        return None
    n = float(s)
    return n if low <= n <= high else None


def run_command(lights: Lights, line: str) -> bool:
    """Run one command line from kbdrgb (see the module docstring). Returns False if it
    isn't a valid command."""
    words = line.split()
    if not words or words[0] not in COMMANDS:
        return False
    name, cmd = words[0], COMMANDS[words[0]]
    target: Target = ALL
    period: float | None = cmd.time()
    seconds: float | None = None
    colors: list[Color] = []
    for w in words[1:]:
        key, eq, value = w.partition("=")
        if not eq:
            if (c := parse_hex(w)) is None:
                return False
            colors.append(c)
        elif key == "zone" and value in (*ZONES, ALL):
            target = value
        elif key == "time":
            period = parse_number(value, cmd.min_time, MAX_TIME)
        elif key == "for" and cmd.timed:
            seconds = parse_number(value, 0.1, MAX_FOR)
            if seconds is None:
                return False
        else:
            return False
    if period is None or not cmd.colors[0] <= len(colors) <= cmd.colors[1]:
        return False
    zones = ZONES if target == ALL else [target]
    if name == "set":
        lights.set(target, colors[0], period)
    else:
        fades = {z: effect_fade(name, z, colors, period) for z in zones}
        lights.animate(fades, f"{target} {name} {' '.join(colors)}".rstrip() + f" every {period:g}s", seconds)
    return True


def handle_messages(lights: Lights, data: bytes) -> None:
    """Run the commands kbdrgb wrote to the NOTIFY pipe."""
    for line in data.decode("ascii", errors="replace").splitlines():
        if line.strip() and not run_command(lights, line):
            log("ignored message:", line[:100])


def caps_lock_on() -> bool:
    """Current caps lock state, read once at startup (later changes arrive as LED events)."""
    for path in glob.glob(CAPS_GLOB):
        try:
            with open(path) as f:
                if f.read().strip() != "0":
                    return True
        except OSError:
            pass
    return False


def open_notify() -> int:
    """Create the NOTIFY pipe for the group that owns the sysfs color files (the user the
    udev rule grants access to). Group needs read too: kbdrgb opens it read-write so the
    open never blocks. Returns a non-blocking read fd."""
    os.makedirs(os.path.dirname(NOTIFY), mode=0o755, exist_ok=True)
    try:
        os.unlink(NOTIFY)
    except FileNotFoundError:
        pass
    os.mkfifo(NOTIFY, 0o600)
    try:
        os.chown(NOTIFY, -1, os.stat(f"{LED}/color_left").st_gid)
        os.chmod(NOTIFY, 0o660)
    except OSError as ex:
        log("notify pipe permissions:", ex)
    # O_RDWR keeps a writer open ourselves, so select() never sees EOF when kbdrgb closes it.
    return os.open(NOTIFY, os.O_RDWR | os.O_NONBLOCK)


def find_keyboard(spec: str) -> str | None:
    """Resolve a device path or device name to an event device path, or None."""
    if spec.startswith("/"):
        return os.path.realpath(spec) if os.path.exists(spec) else None
    for path in evdev.list_devices():
        try:
            dev = evdev.InputDevice(path)
            name = dev.name
            dev.close()
        except OSError:
            continue
        if name == spec:
            return str(path)
    return None


def read_events(device: EventSource) -> list[InputEvent]:
    try:
        return list(device.read())
    except BlockingIOError:
        return []


_dialog_busy = threading.Lock()


def session_user() -> str | None:
    """Username of the active graphical session on seat0, or None."""
    try:
        sid = subprocess.run(["loginctl", "show-seat", "seat0", "-p", "ActiveSession", "--value"],
                             capture_output=True, text=True, timeout=5).stdout.strip()
        if not sid:
            return None
        name = subprocess.run(["loginctl", "show-session", sid, "-p", "Name", "--value"],
                              capture_output=True, text=True, timeout=5).stdout.strip()
        pwd.getpwnam(name)  # must be a real user
        return name
    except (OSError, subprocess.SubprocessError, KeyError):
        return None


def run_as_user(user: str, cmd: Sequence[str],
                timeout: float | None = None) -> subprocess.CompletedProcess[str]:
    """Run cmd as `user` inside their desktop session (Wayland + session D-Bus)."""
    pw = pwd.getpwnam(user)
    rt = f"/run/user/{pw.pw_uid}"
    env = [f"XDG_RUNTIME_DIR={rt}", f"DBUS_SESSION_BUS_ADDRESS=unix:path={rt}/bus",
           f"HOME={pw.pw_dir}", f"USER={user}", "PATH=/usr/bin:/bin",
           "LANG=C.UTF-8"]  # without a UTF-8 locale, zenity rejects non-ASCII arguments
    sockets = sorted(p for p in glob.glob(f"{rt}/wayland-*") if not p.endswith(".lock"))
    if sockets:
        env.append(f"WAYLAND_DISPLAY={os.path.basename(sockets[0])}")
    return subprocess.run(["runuser", "-u", user, "--", "env", "-i", *env, *cmd],
                          capture_output=True, text=True, timeout=timeout)


def run_zenity(user: str, *args: str) -> tuple[int, str]:
    """Run zenity in the user's desktop session. Returns (returncode, stdout)."""
    r = run_as_user(user, ["zenity", *args])
    if r.returncode not in (0, 1):  # 1: cancelled or closed
        log(f"zenity failed ({r.returncode}):", r.stderr.strip()[-300:])
    return r.returncode, r.stdout


def _gvariant_str(s: str) -> str:
    return "'" + s.replace("\\", "\\\\").replace("'", "\\'") + "'"


def desktop_notify(summary: str, body: str) -> bool:
    """Show a desktop notification to the active session's user. Returns True if shown."""
    user = session_user()
    if not user:
        return False
    try:
        r = run_as_user(user, [
            "gdbus", "call", "--session", "--dest", "org.freedesktop.Notifications",
            "--object-path", "/org/freedesktop/Notifications",
            "--method", "org.freedesktop.Notifications.Notify", "--",  # "--": -1 below isn't an option
            "'kbd-rgb'", "0", "'input-keyboard'", _gvariant_str(summary), _gvariant_str(body),
            "[]", "{}", "-1"], timeout=10)
        return r.returncode == 0
    except (OSError, subprocess.SubprocessError, KeyError) as ex:
        log("desktop notification failed:", ex)
        return False


def _load_problem() -> ProblemState:
    try:
        with open(PROBLEM_FILE) as f:
            state: ProblemState = json.load(f)
            return state
    except (OSError, ValueError):
        return {}


def report_problem(key: str, message: str) -> None:
    """Record a failed start. Notifies the desktop user once per problem, after
    REPORT_AFTER failures in a row (so quick blips don't notify). Retries the
    notification on later failures if nobody was logged in to see it."""
    state = _load_problem()
    if state.get("key") != key:
        state = {"key": key, "count": 0, "notified": False}
    state["count"] = state.get("count", 0) + 1
    if not state.get("notified") and state["count"] >= REPORT_AFTER:
        state["notified"] = desktop_notify(
            "Keyboard colors stopped working",
            f"{message} The kbd-rgb service keeps retrying; see journalctl -u kbd-rgb.")
    try:
        os.makedirs(os.path.dirname(PROBLEM_FILE), mode=0o755, exist_ok=True)
        with open(PROBLEM_FILE, "w") as f:
            json.dump(state, f)
    except OSError as ex:
        log("can't record problem:", ex)


def report_recovered() -> None:
    """Called once the daemon has run for STABLE_AFTER seconds."""
    state = _load_problem()
    if not state:
        return
    try:
        os.unlink(PROBLEM_FILE)
    except OSError:
        pass
    if state.get("notified"):
        desktop_notify("Keyboard colors are working again", "The kbd-rgb service is running.")
    log("recovered from:", state.get("key"))


PALETTE_NAMES = ["white", "red", "orange", "yellow", "green",
                 "teal", "sky blue", "blue", "purple", "pink"]


# The dialog's text: the palette, one color a line. Plain text: zenity shows --entry text as
# is (no markup, proportional font, so columns side by side don't line up), and in ASCII only
# (it rejects other text outside a UTF-8 locale). What the dialog takes is in the README.
DIALOG_HELP = "\n".join(f"{n}   {c}   {name}" for n, (c, name) in enumerate(zip(PALETTE, PALETTE_NAMES)))


def _markup(s: str) -> str:
    """Text for zenity's --text, which reads Pango markup."""
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def parse_dialog(text: str, target: Target) -> tuple[str | None, str]:
    """Turn what was typed in the color dialog into a command line for run_command:
    (line, "") or (None, what's wrong). A trailing plain number is the time; one color
    sets it, several cycle; "rainbow" and "pulse" start those effects."""
    words = text.replace(",", " ").split()
    name = words.pop(0).lower() if words and words[0].lower() in ("rainbow", "pulse", "cycle") else None
    time: str | None = None
    if words and re.fullmatch(r"[0-9]*\.?[0-9]+", words[-1]) and len(words[-1]) != 6:
        time = words.pop()
        time = "0" + time if time.startswith(".") else time
    colors = []
    for w in words:
        c = parse_hex(w)
        if c is None:
            return None, f'"{w}" isn\'t a color. Use 6 hex digits like FF8800.'
        colors.append(c)
    if name is None:
        if not colors:
            return None, "Type a color like FF8800."
        name = "set" if len(colors) == 1 else "cycle"
    cmd = COMMANDS[name]
    if not cmd.colors[0] <= len(colors) <= cmd.colors[1]:
        need = {"cycle": f"2 to {MAX_CYCLE} colors", "rainbow": "no colors", "pulse": "one color"}
        return None, f"{name} takes {need.get(name, 'one color')}."
    if time is not None and parse_number(time, cmd.min_time, MAX_TIME) is None:
        return None, f"The time must be from {cmd.min_time:g} to {MAX_TIME:g} seconds."
    words = [name, f"zone={target}"] + ([f"time={time}"] if time else []) + colors
    return " ".join(words), ""


def describe(lights: Lights, target: Target) -> str:
    """What a zone is doing, written the way the dialog takes it (for the whole keyboard,
    the left zone's)."""
    z = "left" if target == ALL else target
    with lights.lock:
        f = lights.fades.get(z)
        if f is None or f.until is not None:
            return lights.color[z]
        t = f"{f.period:g}"
        if f.colors == RAINBOW:
            return f"rainbow {t}"
        if len(f.colors) == 2 and f.colors[1] == mix("000000", f.colors[0], PULSE_DIM):
            return f"pulse {f.colors[0]} {t}"
        return f"{' '.join(f.colors)} {t}"


def hex_dialog(lights: Lights, target: Target) -> None:
    """Ask for a color or effect for target (see README.md), starting from what it shows."""
    if not _dialog_busy.acquire(blocking=False):
        return  # one dialog at a time
    try:
        user = session_user()
        if not user:
            log("color dialog: no active desktop session found")
            return
        label = "the whole keyboard" if target == ALL else f"the {target} zone"
        current = describe(lights, target)
        while True:
            rc, out = run_zenity(user, "--entry", "--title=Keyboard color",
                                 f"--text=Color or effect for {label}:\n\n{DIALOG_HELP}",
                                 f"--entry-text={current}")
            if rc != 0:
                return  # cancelled / closed
            line, problem = parse_dialog(out, target)
            if line and run_command(lights, line):
                return
            run_zenity(user, "--error", "--title=Keyboard color",
                       f"--text={_markup(problem or 'That did not work.')}")
            current = out.strip()
    except Exception as ex:
        log("dialog failed:", ex)
    finally:
        _dialog_busy.release()


class Handler:
    """Key event logic, separated from device I/O for testing."""

    def __init__(self, lights: Lights, emit: Callable[[int, int], None],
                 open_dialog: Callable[[Target], None]) -> None:
        self.lights, self.emit, self.open_dialog = lights, emit, open_dialog
        self.mod_down = False
        self.mod_used = False
        self.sel: Target | None = None    # selected zone or ALL
        self.sel_key: int | None = None   # key that made the selection
        self.held: int | None = None      # that key, while it's still held down
        self.picks: list[tuple[Color, float]] = []  # digits pressed while it's held: color, time
        self.swallowed: set[int] = set()  # keys whose press we ate; eat their repeat/release too

    def _finish_cycle(self) -> None:
        """Start a cycle through the colors picked while the zone key was held (if 2 or
        more). The gap between the first two picks sets the time per color."""
        picks, self.picks = self.picks, []
        if len(picks) < 2 or self.sel is None:
            return
        period = min(MAX_TIME, max(COMMANDS["cycle"].min_time, picks[1][1] - picks[0][1]))
        colors = " ".join(c for c, _ in picks[:MAX_CYCLE])
        run_command(self.lights, f"cycle zone={self.sel} time={period:.3f} {colors}")
        self.sel_key = None  # pressing the zone key again starts over, not the dialog

    def key(self, ev: InputEvent) -> bool:
        """Return True if ev should be forwarded to apps."""
        code, val = ev.code, ev.value  # val: 1 down, 0 up, 2 repeat
        if code == MOD_KEY:
            if val == 1:
                self.mod_down, self.mod_used = True, False
                self.sel = self.sel_key = self.held = None
            elif val == 0:
                self._finish_cycle()  # Menu let go before the zone key: keep what was picked
                self.mod_down = False
                self.sel = self.sel_key = self.held = None
                if not self.mod_used:  # lone tap -> real Menu key
                    self.emit(MOD_KEY, 1); self.emit(MOD_KEY, 0)
            return False

        if code in self.swallowed:
            if val == 0:
                self.swallowed.discard(code)
                if code == self.held:
                    self.held = None
                    self._finish_cycle()
            return False

        if not (self.mod_down and val == 1):
            return True

        self.mod_used = True
        self.swallowed.add(code)
        if code == e.KEY_BACKSPACE:
            self.sel = self.sel_key = self.held = None
            self.picks = []
            self.lights.set(ALL, PALETTE[0])
        elif code in DIGIT:  # digits only ever set colors, never select a zone
            # no zone selected -> whole keyboard
            color = PALETTE[DIGIT[code]]
            self.lights.set(self.sel if self.sel is not None else ALL, color)
            if self.held is not None:
                self.picks.append((color, now()))
        elif self.sel is not None and code == self.sel_key:
            target = self.sel
            self.sel = self.sel_key = None
            log(f"color dialog for {target}")
            self.open_dialog(target)
        elif code in KEY_ZONE:
            self._finish_cycle()  # a new zone key while the last one is still held
            self.sel, self.sel_key, self.held = KEY_ZONE[code], code, code
            log(f"selected {self.sel}")
        else:
            log("unmapped key:", e.KEY.get(code, code))
        return False


def main() -> None:
    try:
        run()
    except SystemExit:
        raise
    except Exception as ex:
        log("crashed:\n" + traceback.format_exc())
        report_problem(f"crash:{type(ex).__name__}", f"The kbd-rgb daemon crashed ({type(ex).__name__}: {ex}).")
        sys.exit(1)


def run() -> None:
    # Each cleanup is registered right after its resource is acquired, so every exit path
    # (early sys.exit, a failed grab, a crash in the loop) releases what was opened so far.
    with contextlib.ExitStack() as cleanup:
        serve(cleanup)


def ungrab(dev: evdev.InputDevice) -> None:
    try:
        dev.ungrab()
    except OSError:
        pass  # device already gone


def serve(cleanup: contextlib.ExitStack) -> None:
    lights = Lights()
    lights.set_caps(caps_lock_on())

    if KBD == VIRTUAL_NAME:
        log(f"KBD_DEVICE is our own virtual keyboard ({VIRTUAL_NAME!r}); pick the device it should read")
        report_problem("config:self", f"It's set to read its own virtual keyboard ({VIRTUAL_NAME}). "
                       "Re-run install.sh with a different --device.")
        sys.exit(1)
    path = find_keyboard(KBD)
    if not path:
        log(f"keyboard not found: {KBD!r} (systemd will retry)")
        report_problem(f"not-found:{KBD}", f"It can't find the keyboard {KBD!r}.")
        sys.exit(1)

    notify_fd = open_notify()
    cleanup.callback(os.close, notify_fd)
    dev = evdev.InputDevice(path)
    cleanup.callback(dev.close)
    ui = evdev.UInput.from_device(dev, name=VIRTUAL_NAME)
    cleanup.callback(ui.close)
    time.sleep(0.5)  # let keys held at startup (e.g. Enter from launching) be released
    dev.grab()
    cleanup.callback(ungrab, dev)
    log(f"grabbed {dev.path} ({dev.name}); forwarding via {ui.device.path}")
    threading.Thread(target=lights.render, daemon=True, name="frames").start()

    def emit(code: int, val: int) -> None:
        ui.write(e.EV_KEY, code, val); ui.syn()

    def open_dialog(target: Target) -> None:
        threading.Thread(target=hex_dialog, args=(lights, target), daemon=True).start()

    h = Handler(lights, emit, open_dialog)
    stable_at: float | None = time.monotonic() + STABLE_AFTER  # one-shot: report recovery once running a while
    while True:
        timeout = None if stable_at is None else max(0.0, stable_at - time.monotonic())
        ready, _, _ = select.select([dev.fd, ui.fd, notify_fd], [], [], timeout)
        if stable_at is not None and time.monotonic() >= stable_at:
            stable_at = None
            report_recovered()
        if dev.fd in ready:
            for ev in read_events(dev):
                if ev.type == e.EV_MSC:
                    continue  # drop raw scancodes; apps use EV_KEY
                if ev.type != e.EV_KEY or h.key(ev):
                    ui.write_event(ev)
        if ui.fd in ready:  # LED changes the desktop sends to our virtual keyboard
            for ev in read_events(ui):
                if ev.type == e.EV_LED and ev.code == e.LED_CAPSL:
                    lights.set_caps(ev.value != 0)
        if notify_fd in ready:
            data = b""
            try:
                while chunk := os.read(notify_fd, 4096):
                    data += chunk
            except BlockingIOError:
                pass
            handle_messages(lights, data)


if __name__ == "__main__":
    main()
