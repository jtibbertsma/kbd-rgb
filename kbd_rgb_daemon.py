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
  Backspace      -> undo: back to the scene before the last change, and so on
                    back through UNDO_DEPTH of them (in memory only). A zone running a timed
                    effect goes back to what it showed before that effect. With nothing to
                    go back to, the whole keyboard turns white
  Shift+Backspace-> redo: forward again through what Backspace undid, until a new change
                    (Shift can go down before or after Menu)
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
                                 NAME [zone=Z] [time=SECONDS] [for=SECONDS] [ease=EASE] [COLOR...]
                               See COMMANDS. zone: a zone or "all" (default). time: seconds to
                               fade from one color to the next. for: end the effect after that
                               long (otherwise it runs until the zone gets a new color). ease: how
                               each fade speeds up and slows down (see EASES).
Fades (caps lock and effects) are drawn by a separate thread, Lights.render(), while anything
is fading: each zone up to once per FRAME, and at most WRITE_RATE writes a second in all. Slow
sysfs writes never delay key forwarding.
"""
from __future__ import annotations

import contextlib, glob, json, math, os, pwd, re, select, subprocess, sys, threading, time, traceback
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass, field, replace
from typing import Literal, NamedTuple, Protocol, TypeAlias, TypedDict

import evdev
from evdev import InputEvent, ecodes as e

Zone: TypeAlias = Literal["left", "center", "right", "extra"]  # the keyboard's color zones
Target: TypeAlias = Zone | Literal["all"]  # a zone, or ALL for the whole keyboard
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
# Caps lock: the left zone holds red, then flashes white. A repeated color holds for a step.
CAPS_COLORS: tuple[Color, ...] = ("FF0000",) * 5 + ("FFFFFF",) * 2
CAPS_DELAY = 0.2   # seconds from one caps lock color to the next (1.4 s around)
CAPS_EASE: tuple[float, float, float, float] = (0.0, 0.0, 0.2, 1.0)  # each change goes fast, then settles
CAPS_OTHER: Color = "FFFFFF"  # the other zones while caps lock is on
CYCLE_DELAY = 1.0  # seconds to fade from one kbdrgb cycle color to the next
FRAME = 0.1        # shortest time between one zone's fade frames (10 per second)
WRITE_RATE = 30.0  # most color writes per second across all zones. The keyboard controller
                   # keeps up with 30 but not 40: zones written later (the numpad) fall behind.
RAINBOW: tuple[Color, ...] = ("FF0000", "FFFF00", "00FF00", "00FFFF", "0000FF", "FF00FF")  # hue wheel
RAINBOW_DELAY = 0.6  # seconds per rainbow color (once around the wheel in 3.6 s)
RAINBOW_LAG = 2 / 3  # how many rainbow colors each zone trails the one before it (40 degrees)
PULSE_DELAY = 0.5    # seconds to fade between the pulse color and its dim copy
PULSE_DIM = 0.08     # brightness of the dim copy
MAX_CYCLE = 16     # most colors in one cycle
UNDO_DEPTH = 100   # most scenes Menu+Backspace can go back through
PALETTE: list[Color] = ["FFFFFF", "FF0000", "FF6600", "FFFF00", "00FF00",
                         "00FFAA", "0088FF", "0000FF", "8800FF", "FF00AA"]
ZONES: list[Zone] = ["left", "center", "right", "extra"]
ALL: Literal["all"] = "all"
TARGETS: list[Target] = [*ZONES, ALL]


def zones_of(target: Target) -> list[Zone]:
    """The zones a target covers."""
    return [z for z in ZONES if target in (z, ALL)]


def parse_target(s: str) -> Target | None:
    """A zone name or "all" as a Target, or None if it isn't one."""
    return next((t for t in TARGETS if t == s), None)

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


Ease: TypeAlias = tuple[float, float, float, float]  # cubic bezier control points x1, y1, x2, y2

# Named easing curves, as in CSS. A curve maps how far through a fade the time is (0-1) to how
# far between the colors it shows: "ease-in" starts slow, "ease-out" ends slow.
EASES: dict[str, Ease] = {
    "linear":      (0.0, 0.0, 1.0, 1.0),
    "ease":        (0.25, 0.1, 0.25, 1.0),
    "ease-in":     (0.42, 0.0, 1.0, 1.0),
    "ease-out":    (0.0, 0.0, 0.58, 1.0),
    "ease-in-out": (0.42, 0.0, 0.58, 1.0),
}
LINEAR = EASES["linear"]


def ease(curve: Ease, x: float) -> float:
    """How far between the colors a fade shows when it's x (0-1) of the way through its time:
    the cubic bezier from (0, 0) to (1, 1) through control points (x1, y1) and (x2, y2), as
    CSS cubic-bezier() does it."""
    x1, y1, x2, y2 = curve
    if x1 == y1 and x2 == y2:  # a straight line
        return x

    def bezier(a: float, b: float, t: float) -> float:  # one coordinate at curve parameter t
        return 3 * (1 - t) ** 2 * t * a + 3 * (1 - t) * t * t * b + t ** 3

    lo, hi = 0.0, 1.0  # x grows with t (x1, x2 are in 0-1), so bisect for the t that gives x
    for _ in range(30):
        t = (lo + hi) / 2
        if bezier(x1, x2, t) < x:
            lo = t
        else:
            hi = t
    return bezier(y1, y2, (lo + hi) / 2)


def parse_ease(s: str) -> Ease | None:
    """An easing curve: a name from EASES, or X1,Y1,X2,Y2 with each from 0 to 1."""
    if s in EASES:
        return EASES[s]
    parts = s.split(",")
    nums = [parse_decimal(p) for p in parts]
    if len(nums) == 4 and all(n is not None and n <= 1 for n in nums):
        x1, y1, x2, y2 = (n or 0.0 for n in nums)
        return (x1, y1, x2, y2)
    return None


def ease_text(curve: Ease) -> str:
    """An easing curve the way parse_ease reads it."""
    return next((name for name, c in EASES.items() if c == curve), ",".join(f"{n:g}" for n in curve))


def curves_for(n: int, curves: Sequence[Ease]) -> tuple[Ease, ...]:
    """Curves for a fade through n colors, one into each: () if they're all linear, n copies
    of a single curve, or the n curves given."""
    if len(curves) == 1:
        curves = tuple(curves) * n
    return () if all(c == LINEAR for c in curves) else tuple(curves)


@dataclass(frozen=True)
class Fade:
    """Colors a zone fades through, looping: `period` seconds from each color to the next,
    counting from `start`. `offset` (in colors) starts the zone partway along. Effects (kbdrgb
    cycle, rainbow, pulse) count from EPOCH, so equal effects stay in step however far apart
    they start. `until` ends a timed effect or a `transition` from one color to the next.
    `eases` shape the steps: eases[i] is the curve for fading into colors[i] (see curves_for;
    () means all linear)."""
    colors: tuple[Color, ...]
    period: float
    start: float = 0.0
    offset: float = 0.0
    until: float | None = None
    transition: bool = False
    eases: tuple[Ease, ...] = ()

    def at(self, t: float) -> Color:
        steps = max(0.0, t - self.start) / self.period + self.offset
        i = int(steps)
        n = len(self.colors)
        return mix(self.colors[i % n], self.colors[(i + 1) % n], ease(self.curve_into((i + 1) % n), steps - i))

    def curve_into(self, i: int) -> Ease:
        """The curve for fading into colors[i]."""
        return self.eases[i] if self.eases else LINEAR

    def saved(self) -> dict[str, object]:
        saved: dict[str, object] = {"colors": list(self.colors), "period": self.period, "offset": self.offset}
        if self.eases:
            texts = [ease_text(c) for c in self.eases]
            saved["ease"] = texts[0] if len(set(texts)) == 1 else texts
        return saved

    @classmethod
    def load(cls, v: object) -> Fade | None:
        """An effect from the state file, or None if it isn't valid."""
        if not isinstance(v, dict):
            return None
        colors, period, offset = parse_cycle(v.get("colors")), v.get("period"), v.get("offset", 0.0)
        texts = v.get("ease", "linear")
        texts = [texts] if isinstance(texts, str) else texts
        curves = [parse_ease(t) if isinstance(t, str) else None for t in texts] if isinstance(texts, list) else [None]
        if colors and isinstance(period, (int, float)) and 0.05 <= period <= 60 \
                and isinstance(offset, (int, float)) and 0 <= offset < len(colors) \
                and len(curves) in (1, len(colors)) and all(curves):
            eases = curves_for(len(colors), [c for c in curves if c])
            return cls(tuple(colors), float(period), EPOCH, float(offset), eases=eases)
        return None

    @classmethod
    def effect(cls, name: str, zone: Zone, colors: list[Color], period: float,
               curves: Sequence[Ease] = (LINEAR,)) -> Fade:
        """The fade a looping effect gives one zone. curves: one for every step, or (for a
        cycle) one into each color."""
        if name == "rainbow":  # each zone RAINBOW_LAG colors further around the wheel than the last
            return cls(RAINBOW, period, EPOCH, offset=ZONES.index(zone) * RAINBOW_LAG,
                       eases=curves_for(len(RAINBOW), curves))
        if name == "pulse":    # between the color and a dim copy of it
            return cls((colors[0], mix("000000", colors[0], PULSE_DIM)), period, EPOCH, eases=curves_for(2, curves))
        return cls(tuple(colors), period, EPOCH, eases=curves_for(len(colors), curves))  # cycle

    @classmethod
    def transition_to(cls, start: Color, color: Color, seconds: float, t: float, curve: Ease = LINEAR) -> Fade:
        """A fade from start to color over seconds, beginning at t, that ends there."""
        return cls((start, color), seconds, t, until=t + seconds, transition=True, eases=curves_for(2, [curve]))


class Scene(TypedDict):
    """What the keyboard is set to: each zone's color, or the effect it runs until cleared (a
    Fade from EPOCH, without `until`). This is what's saved, and what undo and redo go between."""
    left: Color | Fade
    center: Color | Fade
    right: Color | Fade
    extra: Color | Fade


def solid_scene(v: Color) -> Scene:
    """A scene with every zone the same color."""
    return Scene(left=v, center=v, right=v, extra=v)


ActionName: TypeAlias = Literal["set", "cycle", "rainbow", "pulse"]


class Command(NamedTuple):
    colors: tuple[int, int]        # how many colors it takes (min, max)
    takes: str                     # that, in words
    time: Callable[[], float]      # default transition time
    min_time: float                # shortest transition time allowed
    timed: bool                    # whether it takes a duration


# What each action takes. Defaults are looked up when an action runs, so settings changes apply.
COMMANDS: dict[ActionName, Command] = {
    "set":     Command((1, 1), "one color", lambda: 0.0, 0.0, False),  # fade to a color (0: at once)
    "cycle":   Command((2, MAX_CYCLE), f"2 to {MAX_CYCLE} colors", lambda: CYCLE_DELAY, 0.1, True),
    "rainbow": Command((0, 0), "no colors", lambda: RAINBOW_DELAY, 0.1, True),
    "pulse":   Command((1, 1), "one color", lambda: PULSE_DELAY, 0.1, True),
}
MAX_TIME = 60.0       # longest transition time
MAX_FOR = 86400.0     # longest timed effect


EPOCH = 0.0  # effects count from here on the now() clock, so equal effects stay in step


@dataclass(frozen=True)
class Action:
    """A color or effect to show: what kbdrgb sends, what the dialog takes, and what a held-key
    cycle makes. Written as text, it's the command line kbdrgb sends (see run_command)."""
    name: ActionName
    target: Target = ALL
    colors: tuple[Color, ...] = ()
    time: float | None = None      # seconds to fade to the color, or per effect color; None: default
    duration: float | None = None  # effects: stop after this long; None: run until cleared
    ease: Ease | None = None       # how each fade speeds up and slows down; None: linear
    eases: tuple[Ease | None, ...] = ()  # set, cycle: a curve into each color (COLOR:EASE), or None for `ease`

    def curves(self) -> list[Ease]:
        """The curve into each color: its own, or the action's."""
        default = LINEAR if self.ease is None else self.ease
        return [self.eases[i] or default if i < len(self.eases) else default for i in range(len(self.colors))]

    def steps(self) -> list[str]:
        """The colors as written, each with its own curve if it has one."""
        own = list(self.eases) + [None] * (len(self.colors) - len(self.eases))
        return [c + (f":{ease_text(curve)}" if curve else "") for c, curve in zip(self.colors, own)]

    def problem(self) -> str:
        """What's wrong with this action, in words, or "" if it can run."""
        cmd = COMMANDS[self.name]
        if not cmd.colors[0] <= len(self.colors) <= cmd.colors[1]:
            return f"{self.name} takes {cmd.takes}."
        if self.time is not None and not cmd.min_time <= self.time <= MAX_TIME:
            return f"The time must be from {cmd.min_time:g} to {MAX_TIME:g} seconds."
        if self.duration is not None and not cmd.timed:
            return f"{self.name} doesn't take a duration."
        if self.duration is not None and not 0.1 <= self.duration <= MAX_FOR:
            return f"The duration must be from 0.1 to {MAX_FOR:g} seconds."
        if any(self.eases) and self.name not in ("set", "cycle"):
            return f"{self.name} takes one curve for all of its steps, not COLOR:CURVE."
        if len(self.eases) > len(self.colors):
            return "There are more curves than colors."
        return ""

    def run(self, lights: Lights) -> None:
        period = COMMANDS[self.name].time() if self.time is None else self.time
        default = LINEAR if self.ease is None else self.ease
        if self.name == "set":
            lights.set(self.target, self.colors[0], period, self.curves()[0])
        else:
            curves = self.curves() if self.name == "cycle" else [default]
            fades = {z: Fade.effect(self.name, z, list(self.colors), period, curves) for z in zones_of(self.target)}
            what = f"{self.target} {self.name} {' '.join(self.steps())}".rstrip() + f" every {period:g}s"
            what += f" {ease_text(default)}" if default != LINEAR else ""
            lights.animate(fades, what, self.duration)

    def __str__(self) -> str:
        words = [self.name, f"zone={self.target}"]
        words += [f"time={self.time:g}"] if self.time is not None else []
        words += [f"for={self.duration:g}"] if self.duration is not None else []
        words += [f"ease={ease_text(self.ease)}"] if self.ease is not None else []
        return " ".join(words + self.steps())

    @classmethod
    def parse(cls, line: str) -> Action | None:
        """An action from a command line kbdrgb sent: NAME [zone=TARGET] [time=SECONDS]
        [for=SECONDS] [ease=EASE] [COLOR[:EASE]...]. None if it isn't a valid one."""
        words = line.split()
        name = parse_name(words[0]) if words else None
        if name is None:
            return None
        target: Target = ALL
        time: float | None = None
        duration: float | None = None
        curve: Ease | None = None
        colors: list[Color] = []
        eases: list[Ease | None] = []
        for w in words[1:]:
            key, eq, value = w.partition("=")
            color, colon, step_ease = w.partition(":")
            if not eq and (c := parse_hex(color)) is not None:
                step_curve = parse_ease(step_ease) if colon else None
                if colon and step_curve is None:
                    return None
                colors.append(c)
                eases.append(step_curve)
            elif key == "zone" and (t := parse_target(value)) is not None:
                target = t
            elif key == "time" and (n := parse_decimal(value)) is not None:
                time = n
            elif key == "for" and (n := parse_decimal(value)) is not None:
                duration = n
            elif key == "ease" and (parsed := parse_ease(value)) is not None:
                curve = parsed
            else:
                return None
        action = cls(name, target, tuple(colors), time, duration, curve, tuple(eases) if any(eases) else ())
        return None if action.problem() else action

    @classmethod
    def from_dialog(cls, text: str, target: Target) -> tuple[Action | None, str]:
        """Turn what was typed in the color dialog into an action: (action, "") or (None, what's
        wrong). A trailing plain number is the time; one color sets it, several cycle; "rainbow"
        and "pulse" start those effects. A curve on its own (a name from EASES, or X1,Y1,X2,Y2)
        is for every step; COLOR:CURVE is for fading into that color."""
        curve: Ease | None = None
        items: list[tuple[str, Ease | None]] = []  # words in order, with a color's own curve
        for token in text.split():
            token = token.strip(",")
            word, colon, step_ease = token.partition(":")
            if colon:
                if (step_curve := parse_ease(step_ease.lower())) is None:
                    return None, (f'"{step_ease}" isn\'t an easing curve. Use a name like ease-in, or 4 '
                                  "numbers from 0 to 1 like 0.4,0,0.2,1.")
                items.append((word, step_curve))
            elif (parsed := parse_ease(token.lower())) is not None:
                curve = parsed
            elif re.fullmatch(r"[0-9.]*(,[0-9.]*){3}", token):
                return None, f'"{token}" isn\'t an easing curve. Use 4 numbers from 0 to 1, like 0.4,0,0.2,1.'
            else:
                items += [(w, None) for w in token.replace(",", " ").split()]
        name = None
        if items and items[0][1] is None and items[0][0].lower() in ("rainbow", "pulse", "cycle"):
            name = parse_name(items.pop(0)[0].lower())
        time: float | None = None
        last = items[-1][0] if items else ""
        if items and items[-1][1] is None and re.fullmatch(r"[0-9]*\.?[0-9]+", last) and len(last) != 6:
            items.pop()
            time = parse_decimal("0" + last if last.startswith(".") else last)
            if time is None:
                time = math.inf  # too many digits: out of range
        colors = []
        for w, _ in items:
            c = parse_hex(w)
            if c is None:
                return None, f'"{w}" isn\'t a color. Use 6 hex digits like FF8800.'
            colors.append(c)
        if name is None:
            if not colors:
                return None, "Type a color like FF8800."
            name = "set" if len(colors) == 1 else "cycle"
        eases = tuple(step_curve for _, step_curve in items)
        action = cls(name, target, tuple(colors), time, ease=curve, eases=eases if any(eases) else ())
        problem = action.problem()
        return (None, problem) if problem else (action, "")


def parse_name(s: str) -> ActionName | None:
    """An action's name as an ActionName, or None if it isn't one."""
    return next((n for n in COMMANDS if n == s), None)


def load_scene() -> Scene:
    """The scene saved in STATE_FILE, or a white keyboard. Zones the file doesn't have, or
    has in a form this version can't read, are white."""
    scene = solid_scene(PALETTE[0])
    try:
        with open(STATE_FILE) as f:
            saved = json.load(f)
        for z in ZONES:
            v = saved.get(z)
            if isinstance(v, int):  # old format: palette index
                scene[z] = PALETTE[v % len(PALETTE)]
            elif (c := parse_hex(v)) is not None:
                scene[z] = c
        fades = dict(saved.get("fades") or {})
        for z in ZONES:
            if (fade := Fade.load(fades.get(z))):
                scene[z] = fade
    except (OSError, ValueError, AttributeError, TypeError):
        pass
    return scene


def save_scene(scene: Scene) -> None:
    """Save a scene to STATE_FILE. Each effect is saved with its colors at this moment too,
    for versions that only read colors."""
    try:
        os.makedirs(os.path.dirname(STATE_FILE), exist_ok=True)
        tmp = STATE_FILE + ".tmp"
        t = now()
        colors = {z: v if isinstance(v := scene[z], str) else v.at(t) for z in ZONES}
        fades = {z: v.saved() for z in ZONES if isinstance(v := scene[z], Fade)}
        with open(tmp, "w") as f:
            json.dump({**colors, "fades": fades}, f)
        os.replace(tmp, STATE_FILE)
    except OSError as ex:
        log("save failed:", ex)


class History:
    """Scenes for undo and redo, in memory only. The caller keeps the current scene and
    passes it in; `past` and `future` hold copies, at most UNDO_DEPTH each."""

    def __init__(self) -> None:
        self.past: list[Scene] = []    # scenes to go back to, oldest first
        self.future: list[Scene] = []  # scenes undo() left, for redo(), latest last

    def remember(self, scene: Scene) -> None:
        """Keep a scene before it changes (unless it repeats the last one kept). A new change
        ends what redo() could go forward to."""
        self._keep_past(scene)
        self.future.clear()

    def undo(self, scene: Scene) -> Scene:
        """The last scene kept that differs from `scene`, or a white keyboard if there isn't
        one. redo() can come back to `scene`."""
        while self.past and self.past[-1] == scene:
            self.past.pop()
        back = self.past.pop() if self.past else solid_scene(PALETTE[0])
        if back != scene:
            self._keep(self.future, scene)
        return back

    def redo(self, scene: Scene) -> Scene | None:
        """The scene the last undo() left, or None if there isn't one."""
        if not self.future:
            return None
        self._keep_past(scene)
        return self.future.pop()

    def _keep_past(self, scene: Scene) -> None:
        if not self.past or self.past[-1] != scene:
            self._keep(self.past, scene)

    @staticmethod
    def _keep(stack: list[Scene], scene: Scene) -> None:
        stack.append(scene.copy())
        del stack[:-UNDO_DEPTH]

    def __str__(self) -> str:
        return f"{len(self.past)} to undo, {len(self.future)} to redo"


class Leds:
    """The keyboard's LED color files, and when to write the next fade frame. The keyboard
    controller keeps up with about WRITE_RATE writes a second, so frames go out one zone
    at a time: each fading zone at most once per FRAME, and any two writes at least
    1 / WRITE_RATE apart. Not thread-safe: Lights calls it with its lock held."""

    def __init__(self) -> None:
        self.written: dict[Zone, Color] = {}   # last color written to each zone
        self.drawn: dict[Zone, float] = {}     # when each zone was last drawn
        self.last_write = -1.0                 # when any zone was last written
        self.next_frame: float | None = None   # when the next zone frame is due, if any

    def write(self, zone: Zone, color: Color) -> None:
        try:
            with open(f"{LED}/color_{zone}", "w") as f:
                f.write(color)
            self.written[zone] = color
            self.drawn[zone] = self.last_write = now()
        except OSError as ex:
            log("write failed:", zone, ex)

    def show(self, zone: Zone, color: Color) -> None:
        """Write a zone's color if it isn't already showing."""
        if color != self.written.get(zone):
            self.write(zone, color)

    def schedule(self, fading: Sequence[Zone]) -> bool:
        """Work out when the next frame is due: once some fading zone is FRAME past its last
        frame, and 1 / WRITE_RATE past the last write; None if nothing is fading. Returns
        True if frames just started."""
        was = self.next_frame
        if fading:
            ready = min(self.drawn.get(z, -math.inf) + FRAME for z in fading)
            self.next_frame = max(ready, self.last_write + 1 / WRITE_RATE)
        else:
            self.next_frame = None
        return was is None and self.next_frame is not None

    def due(self, t: float) -> bool:
        return self.next_frame is not None and t >= self.next_frame

    def next_zone(self, fading: Sequence[Zone], t: float) -> Zone | None:
        """The fading zone to draw at time t: of those FRAME past their last frame, the one
        that has waited longest."""
        ready = [z for z in fading if t - self.drawn.get(z, -math.inf) >= FRAME - 1e-9]
        if not ready:
            return None
        return min(ready, key=lambda z: self.drawn.get(z, -math.inf))


class Lights:
    """What the keyboard shows. `scene` is what each zone is set to; on top of it, a zone can
    have an `overlay` that ends on its own (a transition to its new color, or a timed
    effect), and caps lock shows over everything while it's on. Changes save the scene,
    and remember() keeps it in `history` for undo. One lock covers all of it: keys and
    kbdrgb messages change it on the main thread, and render() draws fades on its own."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.frames_due = threading.Condition(self.lock)  # wakes render() when fading starts
        self.leds = Leds()
        self.history = History()
        self.scene = load_scene()
        self.overlays: dict[Zone, Fade] = {}   # transitions and timed effects, until they end
        self.caps = False
        self.caps_fade: Fade | None = None     # left zone while caps lock is on
        with self.lock:
            self._changed(ZONES, now(), save=False)

    # What each zone shows

    def running(self) -> dict[Zone, Fade]:
        """The zones fading, apart from caps lock: each one's overlay, or its effect."""
        return {z: f for z in ZONES if (f := self._fade(z))}

    def setting(self, zone: Zone) -> Color | Fade:
        """What a zone is set to, counting a transition as the color it's heading to: its
        timed effect, or what the scene has."""
        o = self.overlays.get(zone)
        return o if o is not None and not o.transition else self.scene[zone]

    def _fade(self, zone: Zone) -> Fade | None:
        if (f := self.overlays.get(zone)) is not None:
            return f
        v = self.scene[zone]
        return v if isinstance(v, Fade) else None

    def _visible_fade(self, zone: Zone) -> Fade | None:
        """The fade a zone shows. While caps lock is on, that's caps lock's on the left zone
        and none on the others (they show CAPS_OTHER); their own carry on unseen."""
        if self.caps:
            return self.caps_fade if zone == "left" else None
        return self._fade(zone)

    def _below_caps(self, zone: Zone, t: float) -> Color:
        """What a zone would show at time t without caps lock."""
        f = self._fade(zone)
        if f is not None:
            return f.at(t)
        v = self.scene[zone]
        assert isinstance(v, str)
        return v

    def shown(self, zone: Zone, t: float | None = None) -> Color:
        """What a zone displays at time t (default now)."""
        t = now() if t is None else t
        if self.caps:
            return self.caps_fade.at(t) if zone == "left" and self.caps_fade else CAPS_OTHER
        return self._below_caps(zone, t)

    def _fading(self) -> list[Zone]:
        return [z for z in ZONES if self._visible_fade(z)]

    def _changed(self, zones: Sequence[Zone], t: float, save: bool = True) -> None:
        """After zones changed (lock held): show them, reschedule frames, and save."""
        if self.leds.schedule(self._fading()):
            self.frames_due.notify()
        self._draw(zones, t)
        if save:
            save_scene(self.scene)

    def _draw(self, zones: Sequence[Zone], t: float) -> None:
        """Write what zones show now (lock held)."""
        for z in zones:
            self.leds.write(z, self.shown(z, t))

    # Changes

    def set(self, target: Target, color: Color, seconds: float = 0.0, curve: Ease = LINEAR) -> None:
        """Give zones a new color, ending their effects. With seconds, fade there from what
        each zone is showing, along the easing curve."""
        with self.lock:
            t = now()
            zones = zones_of(target)
            for z in zones:
                start = self._below_caps(z, t)
                self.scene[z] = color
                self.overlays.pop(z, None)
                if seconds > 0:
                    self.overlays[z] = Fade.transition_to(start, color, seconds, t, curve)
            self._changed(zones, t)
        log(f"{target} -> {color}" + (f" over {seconds:g}s" if seconds else ""))

    def animate(self, fades: dict[Zone, Fade], what: str, seconds: float | None = None) -> None:
        """Run an effect on some zones until set(), or for `seconds`. A timed effect is an
        overlay: the scene underneath stays as it was, and gets the effect's last frame
        when it ends."""
        with self.lock:
            t = now()
            for z, fade in fades.items():
                if seconds is None:
                    self.scene[z] = fade
                    self.overlays.pop(z, None)
                else:
                    self.overlays[z] = replace(fade, until=t + seconds)
            self._changed(list(fades), t)
        log(what, f"for {seconds:g}s" if seconds else "until cleared")

    def set_caps(self, on: bool) -> None:
        with self.lock:
            if on != self.caps:
                self.caps = on
                self.caps_fade = Fade(CAPS_COLORS, CAPS_DELAY, now(),
                                      eases=curves_for(len(CAPS_COLORS), [CAPS_EASE])) if on else None
                self._changed(ZONES, now(), save=False)

    # Undo and redo

    def remember(self) -> None:
        """Keep the scene now for undo(), before a change."""
        with self.lock:
            self.history.remember(self.scene)

    def undo(self) -> None:
        """Go back to the last scene remember() kept that differs from now, or turn the
        whole keyboard white if there isn't one. redo() can come back to now."""
        with self.lock:
            scene = self.history.undo(self.scene)
            self._restore(scene)
        self._log("undo", scene)

    def redo(self) -> None:
        """Go forward to the scene the last undo() left, if there is one."""
        with self.lock:
            scene = self.history.redo(self.scene)
            if scene is None:
                log("redo: nothing to redo")
                return
            self._restore(scene)
        self._log("redo", scene)

    def _restore(self, scene: Scene) -> None:
        """Switch to a scene from undo or redo, ending overlays (lock held)."""
        self.scene = scene.copy()
        self.overlays.clear()
        self._changed(ZONES, now())

    def _log(self, what: str, scene: Scene) -> None:
        log(f"{what}:", " ".join(f"{z}={v if isinstance(v := scene[z], str) else ' '.join(v.colors)}" for z in ZONES),
            f"({self.history})")

    # Fade frames

    def next_due(self) -> float | None:
        """When the next frame is due, or None if nothing is fading."""
        return self.leds.next_frame

    def render(self) -> None:
        """Draw fade frames forever. Runs in its own thread: each sysfs color write takes
        several milliseconds, and the main loop has keys to forward."""
        while True:
            try:
                with self.lock:
                    while self.leds.next_frame is None:
                        self.frames_due.wait()
                    delay = self.leds.next_frame - now()
                if delay > 0:
                    time.sleep(delay)
                self.tick()
            except Exception:
                log("frame failed:\n" + traceback.format_exc())
                time.sleep(1)

    def tick(self) -> None:
        """Draw one fading zone's frame (see Leds), and end overlays whose time is up."""
        with self.lock:
            t = now()
            if not self.leds.due(t):
                return
            ended = [z for z, f in self.overlays.items() if f.until is not None and t >= f.until]
            for z in ended:  # final colors go out right away
                f = self.overlays.pop(z)
                if not f.transition:  # a timed effect stays on its last frame
                    self.scene[z] = f.at(f.until or t)
                self.leds.show(z, self.shown(z, t))
            if (frame := self.leds.next_zone(self._fading(), t)) is not None:
                self.leds.show(frame, self.shown(frame, t))
                self.leds.drawn[frame] = t
            self.leds.schedule(self._fading())
            if ended:
                save_scene(self.scene)
                log("ended:", " ".join(f"{z}={self.scene[z]}" for z in ended))


def parse_cycle(colors: object) -> list[Color] | None:
    """Validated cycle colors (2 to MAX_CYCLE hex colors), or None."""
    if not isinstance(colors, list) or not 2 <= len(colors) <= MAX_CYCLE:
        return None
    parsed = [parse_hex(c) if isinstance(c, str) else None for c in colors]
    return [c for c in parsed if c] if all(parsed) else None


def parse_decimal(s: str) -> float | None:
    """A plain decimal number like 2 or 0.5 (no signs, exponents, inf, or nan), or None."""
    return float(s) if re.fullmatch(r"[0-9]{1,6}(\.[0-9]{1,6})?", s) else None


def run_command(lights: Lights, line: str) -> bool:
    """Run one command line from kbdrgb. Returns False if it isn't a valid command."""
    action = Action.parse(line)
    if action is not None:
        action.run(lights)
    return action is not None


def handle_messages(lights: Lights, data: bytes) -> None:
    """Run the commands kbdrgb wrote to the NOTIFY pipe. One undo step covers them all
    (kbdrgb zones sends four)."""
    lights.remember()
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


def describe(lights: Lights, target: Target) -> str:
    """What a zone is doing, written the way the dialog takes it (for the whole keyboard,
    the left zone's). A zone in a transition counts as the color it's heading to."""
    z = zones_of(target)[0]
    with lights.lock:
        f = lights.setting(z)
        if isinstance(f, str):
            return f
        curves = [f.curve_into(i) for i in range(len(f.colors))]
        uniform = len(set(curves)) == 1
        t = f"{f.period:g}" + (f" {ease_text(curves[0])}" if uniform and curves[0] != LINEAR else "")
        if f.colors == RAINBOW:
            return f"rainbow {t}"
        if len(f.colors) == 2 and f.colors[1] == mix("000000", f.colors[0], PULSE_DIM):
            return f"pulse {f.colors[0]} {t}"
        if uniform:
            return f"{' '.join(f.colors)} {t}"
        steps = [c + (f":{ease_text(curve)}" if curve != LINEAR else "") for c, curve in zip(f.colors, curves)]
        return f"{' '.join(steps)} {t}"


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
            action, problem = Action.from_dialog(out, target)
            if action is not None:
                lights.remember()
                action.run(lights)
                return
            run_zenity(user, "--error", "--title=Keyboard color", f"--text={_markup(problem)}")
            current = out.strip()
    except Exception as ex:
        log("dialog failed:", ex)
    finally:
        _dialog_busy.release()


@dataclass
class Selection:
    """What Menu + a zone key (or Space) selected."""
    target: Target
    key: int                # the key that selected it
    held: bool = True       # that key is still down: digits build a cycle
    picks: list[tuple[Color, float]] = field(default_factory=list)  # those digits: color, time
    reopen: bool = True     # pressing the key again opens the dialog (not after a cycle)


class Handler:
    """Key event logic, separated from device I/O for testing."""

    def __init__(self, lights: Lights, emit: Callable[[int, int], None],
                 open_dialog: Callable[[Target], None]) -> None:
        self.lights, self.emit, self.open_dialog = lights, emit, open_dialog
        self.mod_down = False
        self.mod_used = False
        self.remembered = False           # this Menu hold's changes are one undo step, kept already
        self.sel: Selection | None = None
        self.swallowed: set[int] = set()  # keys whose press we ate; eat their repeat/release too
        self.shifts: set[int] = set()     # Shift keys held down (for Menu+Shift+Backspace)

    def _finish_cycle(self) -> None:
        """Start a cycle through the colors picked while the zone key was held (if 2 or
        more). The gap between the first two picks sets the time per color."""
        sel = self.sel
        if sel is None:
            return
        picks, sel.picks = sel.picks, []
        if len(picks) < 2:
            return
        period = min(MAX_TIME, max(COMMANDS["cycle"].min_time, picks[1][1] - picks[0][1]))
        Action("cycle", sel.target, tuple(c for c, _ in picks[:MAX_CYCLE]), period).run(self.lights)
        sel.reopen = False  # pressing the zone key again starts over, not the dialog

    def key(self, ev: InputEvent) -> bool:
        """Return True if ev should be forwarded to apps."""
        code, val = ev.code, ev.value  # val: 1 down, 0 up, 2 repeat
        if code in (e.KEY_LEFTSHIFT, e.KEY_RIGHTSHIFT):  # tracked whether apps get it or not
            if val == 0:
                self.shifts.discard(code)
            else:
                self.shifts.add(code)
        if code == MOD_KEY:
            if val == 1:
                self.mod_down, self.mod_used, self.remembered, self.sel = True, False, False, None
            elif val == 0:
                self._finish_cycle()  # Menu let go before the zone key: keep what was picked
                self.mod_down, self.sel = False, None
                if not self.mod_used:  # lone tap -> real Menu key
                    self.emit(MOD_KEY, 1); self.emit(MOD_KEY, 0)
            return False

        if code in self.swallowed:
            if val == 0:
                self.swallowed.discard(code)
                if self.sel is not None and self.sel.held and code == self.sel.key:
                    self.sel.held = False
                    self._finish_cycle()
            return False

        if not (self.mod_down and val == 1):
            return True

        self.mod_used = True
        self.swallowed.add(code)
        sel = self.sel
        if code == e.KEY_BACKSPACE:
            self.sel = None
            self.remembered = False  # a change after this is a new step
            if self.shifts:
                self.lights.redo()
            else:
                self.lights.undo()
        elif code in DIGIT:  # digits only ever set colors, never select a zone
            color = PALETTE[DIGIT[code]]
            if not self.remembered:  # everything until Menu is let go is one undo step
                self.lights.remember()
                self.remembered = True
            self.lights.set(ALL if sel is None else sel.target, color)  # no zone selected -> whole keyboard
            if sel is not None and sel.held:
                sel.picks.append((color, now()))
        elif sel is not None and sel.reopen and code == sel.key:
            self.sel = None
            log(f"color dialog for {sel.target}")
            self.open_dialog(sel.target)
        elif code in KEY_ZONE:
            self._finish_cycle()  # a new zone key while the last one is still held
            self.sel = Selection(KEY_ZONE[code], code)
            log(f"selected {self.sel.target}")
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
