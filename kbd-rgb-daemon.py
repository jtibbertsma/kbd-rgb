#!/usr/bin/python3 -I
"""System76 keyboard color daemon (runs as root).

While holding Menu (release Menu at any point to start over):
  non-digit key  -> select that key's zone (Space selects the whole keyboard)
  0-9            -> set the selected zone to PALETTE[n] (0 = white); top-row digits only
                    (numpad digits select the numpad zone).
                    With no zone selected, digits color the whole keyboard.
                    Digits never select a zone. The zone stays selected, so more digits
                    retry colors; another non-digit key selects a different zone.
  same key again -> open a dialog on the desktop to type a hex color for that selection
  Backspace      -> reset the whole keyboard to white (any time)
Tap Menu alone   -> normal Menu key
kbdrgb cycle, rainbow, pulse
                 -> zones fade through colors until a zone gets a new color (Menu combo,
                    hex dialog, or another kbdrgb color command), or for the seconds given
Caps lock on     -> left zone fades through CAPS_COLORS, CAPS_DELAY seconds each; other
                    zones keep their colors (left returns when caps lock turns off)

Keys pressed while Menu is held are not passed to apps. The keyboard is
grabbed exclusively and all other input is re-emitted via a uinput device.
Zone colors (hex) persist in STATE_FILE.

The main loop sleeps in select() on:
  - the keyboard            -> key events
  - the uinput device       -> LED events the desktop sends it (caps lock on/off)
  - the NOTIFY named pipe   -> messages from the kbdrgb CLI, one per line:
                                 stop               end effects (it's about to write colors)
                                 sync               it wrote colors; adopt and save them
                                 cycle ZONE C1 C2…  cycle ZONE (or "all") through the colors
                                 rainbow [SECONDS]  hues around the keyboard
                                 pulse COLOR [SECONDS]  fade between COLOR and a dim copy
                               Effects run until a zone gets a new color, or for SECONDS.
Fades (caps lock and effects) are drawn by a separate thread, Lights.render(), every FRAME
seconds while anything is fading, so slow sysfs writes never delay key forwarding.
"""
from __future__ import annotations

import contextlib, glob, json, os, pwd, re, select, subprocess, sys, threading, time, traceback
from collections.abc import Callable, Iterator, Sequence
from typing import Protocol, TypeAlias, TypedDict

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
CAPS_DELAY = 1.0   # seconds to fade from one caps lock color to the next
CYCLE_DELAY = 1.0  # seconds to fade from one kbdrgb cycle color to the next
FRAME = 0.1        # seconds between fade frames: 10 per second. At 20, the keyboard controller
                   # falls behind and the right and numpad zones stop updating.
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


def load_fade(v: object, t: float) -> Fade | None:
    """A Fade from the state file, or None if it isn't valid."""
    if not isinstance(v, dict):
        return None
    colors, period, offset = parse_cycle(v.get("colors")), v.get("period"), v.get("offset", 0.0)
    if colors and isinstance(period, (int, float)) and 0.05 <= period <= 60 \
            and isinstance(offset, (int, float)) and 0 <= offset < len(colors):
        return Fade(colors, float(period), t, float(offset))
    return None


def cycle_fades(target: Target, colors: list[Color]) -> dict[Zone, Fade]:
    return {z: Fade(colors, CYCLE_DELAY, 0.0) for z in (ZONES if target == ALL else [target])}


def rainbow_fades() -> dict[Zone, Fade]:
    """Hues around the color wheel, each zone RAINBOW_LAG colors behind the one before it."""
    return {z: Fade(RAINBOW, RAINBOW_DELAY, 0.0, offset=i * RAINBOW_LAG) for i, z in enumerate(ZONES)}


def pulse_fades(color: Color) -> dict[Zone, Fade]:
    """All zones fading between color and a dim copy of it."""
    return {z: Fade([color, mix("000000", color, PULSE_DIM)], PULSE_DELAY, 0.0) for z in ZONES}


class Lights:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.frames_due = threading.Condition(self.lock)  # wakes render() when fading starts
        self.color: dict[Zone, Color] = {z: PALETTE[0] for z in ZONES}
        self.caps = False
        self.caps_fade: Fade | None = None     # left zone while caps lock is on
        self.fades: dict[Zone, Fade] = {}      # zones running an effect (kbdrgb cycle, rainbow, pulse)
        self.written: dict[Zone, Color] = {}   # last color written to each zone
        self.next_frame: float | None = None   # when the next fade frame is due, if any
        t = now()
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
                if z in ZONES and (fade := load_fade(v, t)):
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
        except OSError as ex:
            log("write failed:", zone, ex)

    def apply(self) -> None:
        t = now()
        for z in ZONES:
            self.write(z, self.shown(z, t))

    def active_fade(self, zone: Zone) -> Fade | None:
        """The fade a zone is showing: caps lock's on the left zone wins over an effect."""
        if self.caps and zone == "left":
            return self.caps_fade
        return self.fades.get(zone)

    def shown(self, zone: Zone, t: float | None = None) -> Color:
        """What a zone displays at time t (default now): its fade's color, or its color."""
        fade = self.active_fade(zone)
        return fade.at(now() if t is None else t) if fade else self.color[zone]

    def _schedule(self) -> None:
        """Start or stop frames: they run while caps lock or any effect is fading (lock held)."""
        if self.caps_fade or self.fades:
            if self.next_frame is None:
                self.next_frame = now() + FRAME
                self.frames_due.notify()
        else:
            self.next_frame = None

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

    def set(self, target: Target, color: Color) -> None:
        with self.lock:
            for z in (ZONES if target == ALL else [target]):
                self.color[z] = color
                self.fades.pop(z, None)  # a new color ends the zone's effect
            self._schedule()
            self.apply(); self.save()
        log(f"{target} -> {color}")

    def animate(self, fades: dict[Zone, Fade], what: str, seconds: float | None = None) -> None:
        """Run an effect on some zones until set() or stop_fades(), or for `seconds`. Restarts
        every running effect from the top, so zones stay in step."""
        with self.lock:
            t = now()
            for z, fade in fades.items():
                fade.until = None if seconds is None else t + seconds
                self.fades[z] = fade
                self.color[z] = fade.at(t)
            for fade in self.fades.values():
                fade.start = t
            self._schedule()
            self.apply(); self.save()
        log(what, f"for {seconds:g}s" if seconds else "until cleared")

    def _end(self, zone: Zone, t: float) -> None:
        """End a zone's effect, leaving it on the color it's showing (lock held)."""
        self.color[zone] = self.fades.pop(zone).at(t)

    def stop_fades(self) -> None:
        """End all effects (kbdrgb is about to write new colors and sends "sync" when done)."""
        with self.lock:
            if not self.fades:
                return
            t = now()
            for z in list(self.fades):
                self._end(z, t)
            self._schedule()
            self.save()
        log("effects stopped")

    def sync_external(self) -> bool:
        """Adopt colors another program (e.g. kbdrgb) wrote to sysfs, and save them.

        Called when kbdrgb sends "sync" on the NOTIFY pipe. A fading zone still showing the
        frame we last wrote (e.g. the left zone during caps lock) counts as unchanged.
        Returns True if anything changed.
        """
        with self.lock:
            changed = False
            for z in ZONES:
                try:
                    with open(f"{LED}/color_{z}") as f:
                        c = parse_hex(f.read())
                except OSError:
                    continue
                if self.active_fade(z) and c == self.written.get(z):
                    continue  # our own fade frame, not a change
                if c and c != self.color[z]:
                    self.color[z] = c
                    changed = True
            if changed:
                self.save()
                log("saved external colors:", " ".join(f"{z}={self.color[z]}" for z in ZONES))
            return changed

    def set_caps(self, on: bool) -> None:
        with self.lock:
            if on != self.caps:
                self.caps = on
                self.caps_fade = Fade(CAPS_COLORS, CAPS_DELAY, now()) if on else None
                self._schedule()
                self.write("left", self.shown("left"))

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
        """Write the next frame of every fading zone, if a frame is due, and end timed
        effects whose time is up."""
        with self.lock:
            t = now()
            if self.next_frame is None or t < self.next_frame:
                return
            self.next_frame = None
            expired = [z for z, f in self.fades.items() if f.until is not None and t >= f.until]
            for z in expired:
                self._end(z, self.fades[z].until or t)
            self._schedule()
            if self.next_frame is not None:
                self.next_frame = t + FRAME  # after a stall (e.g. suspend), carry on from now
            for z in ZONES:
                if (self.active_fade(z) or z in expired) and (c := self.shown(z, t)) != self.written.get(z):
                    self.write(z, c)
            if expired:
                self.save()
                log("effect ended:", " ".join(f"{z}={self.color[z]}" for z in expired))


def parse_cycle(colors: object) -> list[Color] | None:
    """Validated cycle colors (2 to MAX_CYCLE hex colors), or None."""
    if not isinstance(colors, list) or not 2 <= len(colors) <= MAX_CYCLE:
        return None
    parsed = [parse_hex(c) if isinstance(c, str) else None for c in colors]
    return [c for c in parsed if c] if all(parsed) else None


def parse_seconds(words: list[str]) -> tuple[bool, float | None]:
    """An optional trailing duration (whole seconds, 1 to a day): (valid, seconds or None)."""
    if not words:
        return True, None
    if len(words) == 1 and words[0].isdigit() and 1 <= int(words[0]) <= 86400:
        return True, float(words[0])
    return False, None


def handle_messages(lights: Lights, data: bytes) -> None:
    """Act on messages kbdrgb wrote to the NOTIFY pipe (see the module docstring)."""
    for line in data.decode("ascii", errors="replace").splitlines():
        words = line.split()
        if words == ["sync"]:
            lights.sync_external()
        elif words == ["stop"]:
            lights.stop_fades()
        elif len(words) >= 2 and words[0] == "cycle" and words[1] in (*ZONES, ALL) \
                and (colors := parse_cycle(words[2:])):
            lights.animate(cycle_fades(words[1], colors), f"{words[1]} cycling {' '.join(colors)}")
        elif words[:1] == ["rainbow"] and (secs := parse_seconds(words[1:]))[0]:
            lights.animate(rainbow_fades(), "rainbow", secs[1])
        elif len(words) >= 2 and words[0] == "pulse" and (c := parse_hex(words[1])) \
                and (secs := parse_seconds(words[2:]))[0]:
            lights.animate(pulse_fades(c), f"pulse {c}", secs[1])
        elif words:
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
           f"HOME={pw.pw_dir}", f"USER={user}", "PATH=/usr/bin:/bin"]
    sockets = sorted(p for p in glob.glob(f"{rt}/wayland-*") if not p.endswith(".lock"))
    if sockets:
        env.append(f"WAYLAND_DISPLAY={os.path.basename(sockets[0])}")
    return subprocess.run(["runuser", "-u", user, "--", "env", "-i", *env, *cmd],
                          capture_output=True, text=True, timeout=timeout)


def run_zenity(user: str, *args: str) -> tuple[int, str]:
    """Run zenity in the user's desktop session. Returns (returncode, stdout)."""
    r = run_as_user(user, ["zenity", *args])
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


def hex_dialog(lights: Lights, target: Target) -> None:
    if not _dialog_busy.acquire(blocking=False):
        return  # one dialog at a time
    try:
        user = session_user()
        if not user:
            log("hex dialog: no active desktop session found")
            return
        label = "the whole keyboard" if target == ALL else f"the {target} zone"
        current = lights.get(target)
        while True:
            rc, out = run_zenity(user, "--entry", "--title=Keyboard color",
                                 f"--text=Hex color for {label} (e.g. FF8800):",
                                 f"--entry-text={current}")
            if rc != 0:
                return  # cancelled / closed
            color = parse_hex(out)
            if color:
                lights.set(target, color)
                return
            run_zenity(user, "--error", "--title=Keyboard color",
                       f"--text=\"{out.strip()}\" isn't a hex color. Use 6 digits like FF8800.")
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
        self.swallowed: set[int] = set()  # keys whose press we ate; eat their repeat/release too

    def key(self, ev: InputEvent) -> bool:
        """Return True if ev should be forwarded to apps."""
        code, val = ev.code, ev.value  # val: 1 down, 0 up, 2 repeat
        if code == MOD_KEY:
            if val == 1:
                self.mod_down, self.mod_used = True, False
                self.sel = self.sel_key = None
            elif val == 0:
                self.mod_down = False
                self.sel = self.sel_key = None
                if not self.mod_used:  # lone tap -> real Menu key
                    self.emit(MOD_KEY, 1); self.emit(MOD_KEY, 0)
            return False

        if code in self.swallowed:
            if val == 0:
                self.swallowed.discard(code)
            return False

        if not (self.mod_down and val == 1):
            return True

        self.mod_used = True
        self.swallowed.add(code)
        if code == e.KEY_BACKSPACE:
            self.sel = self.sel_key = None
            self.lights.set(ALL, PALETTE[0])
        elif code in DIGIT:  # digits only ever set colors, never select a zone
            # no zone selected -> whole keyboard
            self.lights.set(self.sel if self.sel is not None else ALL, PALETTE[DIGIT[code]])
        elif self.sel is not None and code == self.sel_key:
            target = self.sel
            self.sel = self.sel_key = None
            log(f"hex dialog for {target}")
            self.open_dialog(target)
        elif code in KEY_ZONE:
            self.sel, self.sel_key = KEY_ZONE[code], code
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
