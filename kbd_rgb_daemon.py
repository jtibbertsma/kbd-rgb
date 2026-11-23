#!/usr/bin/python3 -I
"""Runs in the background as root and controls the keyboard lights on a System76 laptop, through
the color files of System76's system76 driver. It's written for, and only tested on, the
Serval WS serw14 and its 4-zone keyboard. See README.md for how to use it.

To catch the Menu-key combos, the daemon grabs a keyboard: the built-in one by default, or
another device given with --device, such as a key remapper's virtual keyboard. Keys that aren't
part of a combo go on to the desktop through the daemon's own virtual keyboard.

Since typing on that keyboard passes through the daemon, a hang in the daemon would stall typing,
so the main loop, which forwards the keys, shouldn't wait on anything slow. The slower work happens in other
threads:

  - drawing fade animations, one frame at a time (Lights.render)
  - answering the kbdrgb command (CommandSocket.answer)
  - showing the color dialog and waiting for the user to type in it (hex_dialog)

The colors can be changed in three ways:

  - Menu-key combos, handled by Handler
  - the kbdrgb command, which sends its arguments to CommandSocket
  - the color dialog, whose text Action.from_dialog reads

Each of these produces an Action: a color or effect for some part of the keyboard. Running the
Action changes Lights, which keeps track of what each zone is set to, remembers past settings
for undo, and writes the colors to the keyboard. The keyboard can only take about 30 color
writes a second, so Lights spaces the writes out.

All of these threads change Lights, so Lights has one lock that covers all of its state.

The daemon's settings, like the palette or which keyboard to grab, come from command line
options and are stored in Config. Run with --help to list them. install.sh passes the options
on to the service.
"""
from __future__ import annotations

import argparse, contextlib, glob, json, math, os, pwd, re, select, shlex, socket, subprocess, sys, threading, time, traceback
from collections import deque
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass, field, fields, replace
from typing import Any, Literal, NamedTuple, NoReturn, Protocol, TypeAlias, TypedDict, get_args

import evdev
from evdev import InputEvent, ecodes as e

Zone: TypeAlias = Literal["left", "center", "right", "extra"]  # the keyboard's color zones
Target: TypeAlias = Zone | Literal["all"]  # a zone, or ALL for the whole keyboard
Color: TypeAlias = str   # "RRGGBB", uppercase hex
# the two control points (x1, y1) and (x2, y2) of a cubic bezier from (0, 0) to (1, 1), as in CSS
Ease: TypeAlias = tuple[float, float, float, float]

ZONES: list[Zone] = ["left", "center", "right", "extra"]
ALL: Literal["all"] = "all"
TARGETS: list[Target] = [*ZONES, ALL]


# The named curves from CSS, with the same control points.
EASES: dict[str, Ease] = {
    "linear":      (0.0, 0.0, 1.0, 1.0),
    "ease":        (0.25, 0.1, 0.25, 1.0),
    "ease-in":     (0.42, 0.0, 1.0, 1.0),
    "ease-out":    (0.0, 0.0, 0.58, 1.0),
    "ease-in-out": (0.42, 0.0, 0.58, 1.0),
}
LINEAR = EASES["linear"]

# Limits
MAX_CYCLE = 16        # most colors in one cycle
MAX_TIME = 60.0       # longest transition time
MAX_FOR = 86400.0     # longest timed effect
# The most color writes per second that fades send, across all zones. At 40 a second, the
# keyboard controller misses some writes. The numpad is written last, so its fades look choppy.
WRITE_RATE = 30.0


# --- Basics ---

def log(*a: object) -> None:
    print(*a, flush=True)


def now() -> float:
    """Separate from time.monotonic() so tests can replace it with a fake clock."""
    return time.monotonic()


def zones_of(target: Target) -> list[Zone]:
    return [z for z in ZONES if target in (z, ALL)]


def parse_target(s: str) -> Target | None:
    return next((t for t in TARGETS if t == s), None)


# --- Colors and easing curves ---

def parse_hex(s: str | None) -> Color | None:
    s = (s or "").strip().lstrip("#")
    if re.fullmatch(r"[0-9A-Fa-f]{3}", s):
        s = "".join(c * 2 for c in s)
    return s.upper() if re.fullmatch(r"[0-9A-Fa-f]{6}", s) else None


def mix(a: Color, b: Color, f: float) -> Color:
    """Blends two colors channel by channel. `f` = 0 gives `a`, and `f` = 1 gives `b`."""
    ca, cb = bytes.fromhex(a), bytes.fromhex(b)
    return "".join(f"{int(x + (y - x) * f + 0.5):02X}" for x, y in zip(ca, cb))


def looks_different(a: Color | None, b: Color, fraction: int) -> bool:
    """Whether `b` differs enough from `a` to be worth writing (see --visible-step). `a` is None
    when nothing has been written yet."""
    if a is None:
        return True
    x, y = bytes.fromhex(a), bytes.fromhex(b)
    step = max(1.0, max(*x, *y) / fraction)
    return any(abs(i - j) >= step for i, j in zip(x, y))


def parse_decimal(s: str) -> float | None:
    """Unsigned decimals like 2 or 0.5, with up to 6 digits on each side of the point. Unlike
    float(), it rejects signs, exponents, inf and nan, which times, settings and curve points never
    need."""
    return float(s) if re.fullmatch(r"[0-9]{1,6}(\.[0-9]{1,6})?", s) else None


def bezier(a: float, b: float, t: float) -> float:
    """One coordinate of a cubic bezier whose end points are 0 and 1, with control points `a` and
    `b`."""
    return 3 * (1 - t) ** 2 * t * a + 3 * (1 - t) * t * t * b + t ** 3


def ease(curve: Ease, x: float) -> float:
    """Maps how far through its time a fade is (`x`, from 0 to 1) to how far between its colors it
    is, like CSS cubic-bezier()."""
    x1, y1, x2, y2 = curve
    if x1 == y1 and x2 == y2:  # a straight line
        return x

    lo, hi = 0.0, 1.0  # x grows with t (x1, x2 are in 0-1), so bisect for the t that gives x
    for _ in range(30):
        t = (lo + hi) / 2
        if bezier(x1, x2, t) < x:
            lo = t
        else:
            hi = t
    return bezier(y1, y2, (lo + hi) / 2)


def parse_ease(s: str) -> Ease | None:
    if s.lower() in EASES:
        return EASES[s.lower()]
    parts = s.split(",")
    nums = [parse_decimal(p) for p in parts]
    if len(nums) == 4 and all(n is not None and n <= 1 for n in nums):
        x1, y1, x2, y2 = (n or 0.0 for n in nums)
        return (x1, y1, x2, y2)
    return None


def ease_text(curve: Ease) -> str:
    """The inverse of parse_ease."""
    for name, named in EASES.items():
        if named == curve:
            return name
    return ",".join(f"{n:g}" for n in curve)


def curves_for(n: int, curves: Sequence[Ease]) -> tuple[Ease, ...]:
    """`curves[i]` is for fading into color `i`. () means all linear (see Fade)."""
    if len(curves) == 1:
        curves = tuple(curves) * n
    return () if all(c == LINEAR for c in curves) else tuple(curves)


# --- Reading colors and curves from the dialog and kbdrgb ---

class ParseError(argparse.ArgumentTypeError):
    pass


def parse_color(word: str) -> Color:
    if (c := parse_hex(word)) is None:
        raise ParseError(f'"{word}" isn\'t a color. Use 6 hex digits like FF8800.')
    return c


def parse_curve(word: str) -> Ease:
    if (curve := parse_ease(word)) is None:
        raise ParseError(f'"{word}" isn\'t an easing curve. Use {", ".join(EASES)}, '
                         "or 4 numbers from 0 to 1 like 0.4,0,0.2,1.")
    return curve


def parse_step(word: str) -> tuple[Color, Ease | None]:
    """COLOR or COLOR:CURVE, where CURVE is for fading into COLOR."""
    color, colon, curve = word.partition(":")
    return parse_color(color), parse_curve(curve) if colon else None


# --- Config: settings and the system, from the daemon's command line ---

def parse_colors(word: str) -> tuple[Color, ...]:
    return tuple(parse_color(c) for c in word.split(","))


def parse_names(word: str) -> tuple[str, ...]:
    names = tuple(n.strip() for n in word.split(","))
    if not all(names):
        raise ParseError(f'"{word}" has an empty name. Separate names with commas, like red,sky blue.')
    return names


def parse_number(word: str) -> float:
    if (n := parse_decimal(word)) is None:
        raise ParseError(f'"{word}" isn\'t a number like 2 or 0.5.')
    return n


def parse_positive(word: str) -> float:
    if (n := parse_number(word)) <= 0:
        raise ParseError(f'"{word}" must be more than 0.')
    return n


def parse_count(word: str) -> int:
    if not re.fullmatch(r"[0-9]{1,9}", word) or int(word) == 0:
        raise ParseError(f'"{word}" isn\'t a whole number from 1 up.')
    return int(word)


def parse_key(word: str) -> int:
    code = e.ecodes.get("KEY_" + word.upper().removeprefix("KEY_"))
    if not isinstance(code, int):
        raise ParseError(f'"{word}" isn\'t a key name like COMPOSE.')
    return code


def key_text(code: int) -> str:
    """The inverse of parse_key. evdev gives a list of aliases for some codes, and this uses the
    first one."""
    names: str | list[str] = e.KEY.get(code, str(code))
    return (names[0] if isinstance(names, list) else names).removeprefix("KEY_")


def _setting(default: object, parse: Callable[[str], object], text: Callable[[Any], str], help: str) -> Any:
    """A Config field, with what Config.parser() needs to make it a command line option."""
    return field(default=default, metadata={"parse": parse, "text": text, "help": help})


def _join(words: Sequence[str]) -> str:
    return ",".join(words)


def _num(n: float) -> str:
    return f"{n:g}"


@dataclass(frozen=True)
class Config:
    # Settings: what the keyboard does
    palette: tuple[Color, ...] = _setting(
        ("FFFFFF", "FF0000", "FF6600", "FFFF00", "00FF00", "00FFAA", "0088FF", "0000FF", "8800FF", "FF00AA"),
        parse_colors, _join, "the 10 colors for Menu+0 to Menu+9")
    palette_names: tuple[str, ...] = _setting(
        ("white", "red", "orange", "yellow", "green", "teal", "sky blue", "blue", "purple", "pink"),
        parse_names, _join, "a name for each palette color, for the color dialog")
    caps_colors: tuple[Color, ...] = _setting(
        ("FF0000",) * 5 + ("FFFFFF",) * 2, parse_colors, _join,
        "the colors the left zone fades through while caps lock is on")
    caps_delay: float = _setting(0.2, parse_positive, _num, "seconds from one caps lock color to the next")
    caps_ease: Ease = _setting((0.0, 0.0, 0.2, 1.0), parse_curve, ease_text,
                               "the easing curve into each caps lock color")
    caps_other: Color = _setting("FFFFFF", parse_color, str, "the other zones' color while caps lock is on")
    cycle_delay: float = _setting(1.0, parse_positive, _num,
                                  "default seconds per color for cycles from kbdrgb and the color dialog")
    rainbow: tuple[Color, ...] = _setting(
        ("FF0000", "FFFF00", "00FF00", "00FFFF", "0000FF", "FF00FF"), parse_colors, _join,
        "the colors a rainbow goes through (by default, around the hue wheel)")
    rainbow_delay: float = _setting(0.6, parse_positive, _num, "default seconds per rainbow color")
    rainbow_lag: float = _setting(2 / 3, parse_number, _num,
                                  "the rainbow's offset from each zone to the next")
    pulse_delay: float = _setting(0.5, parse_positive, _num,
                                  "default seconds to fade between the pulse color and its dim copy")
    pulse_dim: float = _setting(0.08, parse_number, _num, "brightness of the pulse's dim copy, from 0 to 1")
    undo_depth: int = _setting(100, parse_count, str, "most changes Menu+Backspace can go back through")
    frame: float = _setting(0.1, parse_positive, _num,
                            "shortest time between one zone's fade frames, in seconds. Lowering it makes fades "
                            "smoother, at the cost of more CPU")
    visible_step: int = _setting(  # each write costs the kernel a few dozen ACPI interrupts
        32, parse_count, str,
        "a fading color is written again only once a channel has moved 1/N of the brightest channel. "
        "Raising it makes slow fades smoother, at the cost of more CPU")

    # The system: devices and files
    device: str = _setting("/dev/input/by-path/platform-i8042-serio-0-event-kbd", str, str,
                           "the keyboard to grab, as a device path or a device name, such as a key "
                           "remapper's virtual keyboard")
    virtual_name: str = _setting("kbd-rgb virtual keyboard", str, str,
                                 "the name of the virtual keyboard the daemon passes keys on through")
    mod_key: int = _setting(e.KEY_COMPOSE, parse_key, key_text, "the key to hold for the color combos")
    led: str = _setting("/sys/class/leds/system76::kbd_backlight", str, str, "the keyboard's LED directory")
    caps_glob: str = _setting("/sys/class/leds/input*::capslock/brightness", str, str,
                              "the caps lock LEDs, to read caps lock at startup")
    state_file: str = _setting("/var/lib/kbd-rgb/state.json", str, str,
                               "the file that saves each zone's color or effect, so they come back after a restart")
    socket: str = _setting("/run/kbd-rgb/socket", str, str, "the Unix socket kbdrgb sends its commands to")
    max_request: int = _setting(65536, parse_count, str, "longest kbdrgb command, in bytes")
    request_timeout: float = _setting(1.0, parse_positive, _num, "seconds kbdrgb gets to send its command")
    problem_file: str = _setting("/run/kbd-rgb/problem.json", str, str,
                                 "where the current startup problem is kept between failed starts")
    report_after: int = _setting(3, parse_count, str,
                                 "failed starts in a row before notifying the desktop user")
    stable_after: float = _setting(30.0, parse_positive, _num,
                                   "seconds of running before a reported problem counts as fixed")

    @classmethod
    def parser(cls) -> argparse.ArgumentParser:
        """Each field becomes a command line option, such as --caps-delay for `caps_delay`."""
        parser = argparse.ArgumentParser(
            prog="kbd_rgb_daemon.py", usage="%(prog)s [--check] [--OPTION VALUE ...]",
            description="System76 keyboard color daemon. Lists of colors or names are "
                        "comma-separated, like FF0000,FFFFFF.")
        for f in fields(cls):
            parser.add_argument("--" + f.name.replace("_", "-"), dest=f.name, type=f.metadata["parse"],
                                default=argparse.SUPPRESS, metavar=f.name.upper(),
                                help=f"{f.metadata['help']} (default: {f.metadata['text'](f.default)})")
        return parser

    @classmethod
    def from_args(cls, args: argparse.Namespace) -> Config:
        names = {f.name for f in fields(cls)}
        return cls(**{k: v for k, v in vars(args).items() if k in names})

    def problem(self) -> str:
        """Problems that each option's parser can't catch on its own."""
        if len(self.palette) != 10:
            return "--palette requires 10 colors, for Menu+0 to Menu+9."
        if len(self.palette_names) != len(self.palette):
            return "--palette-names requires 10 names, one for each --palette color."
        if len(self.rainbow) < 2:
            return "--rainbow requires at least 2 colors."
        if self.pulse_dim > 1:
            return "--pulse-dim must be from 0 to 1."
        if self.device == self.virtual_name:
            return "--device can't be the daemon's own virtual keyboard."
        return ""


# --- Fades: a zone's colors over time ---

def saved_colors(colors: object) -> list[Color] | None:
    if not isinstance(colors, list) or not 2 <= len(colors) <= MAX_CYCLE:
        return None
    parsed = [parse_hex(c) if isinstance(c, str) else None for c in colors]
    return [c for c in parsed if c] if all(parsed) else None


EPOCH = 0.0  # effects count from here on the now() clock, so equal effects stay in step


@dataclass(frozen=True)
class Fade:
    """A zone's color changing over time. It blends from each color in `colors` to the next, then
    loops back to the first.

    `period` is the seconds from one color to the next, counted from `start`. `offset` is how far
    along its colors the zone starts. For example, 1.5 starts it halfway between the second color
    and the third."""
    colors: tuple[Color, ...]
    period: float
    start: float = 0.0
    offset: float = 0.0
    until: float | None = None        # when a timed effect or transition ends; None: never
    transition: bool = False          # a one-time change to a new color, instead of a looping effect
    eases: tuple[Ease, ...] = ()      # eases[i] is the curve for blending into colors[i]; (): all linear

    def at(self, t: float) -> Color:
        steps = max(0.0, t - self.start) / self.period + self.offset
        i = int(steps)
        n = len(self.colors)
        here, after = i % n, (i + 1) % n
        progress = ease(self.curve_into(after), steps - i)
        return mix(self.colors[here], self.colors[after], progress)

    def curve_into(self, i: int) -> Ease:
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
        colors, period, offset = saved_colors(v.get("colors")), v.get("period"), v.get("offset", 0.0)
        texts = v.get("ease", "linear")  # one curve for every step, or a list, one into each color
        if isinstance(texts, str):
            texts = [texts]
        if not isinstance(texts, list):
            return None
        curves = [parse_ease(t) if isinstance(t, str) else None for t in texts]
        if colors and isinstance(period, (int, float)) and 0.05 <= period <= MAX_TIME \
                and isinstance(offset, (int, float)) and 0 <= offset < len(colors) \
                and len(curves) in (1, len(colors)) and all(curves):
            eases = curves_for(len(colors), [c for c in curves if c])
            return cls(tuple(colors), float(period), EPOCH, float(offset), eases=eases)
        return None

    @classmethod
    def effect(cls, name: str, zone: Zone, colors: list[Color], period: float, config: Config,
               curves: Sequence[Ease] = (LINEAR,)) -> Fade:
        """`curves` can be a single curve, which applies to every step. For a cycle, it can also be
        one curve per color, each for fading into that color."""
        if name == "rainbow":  # each zone rainbow_lag colors further around the wheel than the last
            return cls(config.rainbow, period, EPOCH, offset=ZONES.index(zone) * config.rainbow_lag,
                       eases=curves_for(len(config.rainbow), curves))
        if name == "pulse":    # between the color and a dim copy of it
            dim = mix("000000", colors[0], config.pulse_dim)
            return cls((colors[0], dim), period, EPOCH, eases=curves_for(2, curves))
        return cls(tuple(colors), period, EPOCH, eases=curves_for(len(colors), curves))  # cycle

    @classmethod
    def transition_to(cls, start: Color, color: Color, seconds: float, t: float, curve: Ease = LINEAR) -> Fade:
        return cls((start, color), seconds, t, until=t + seconds, transition=True, eases=curves_for(2, [curve]))


# --- Scenes: what the keyboard is set to ---

class Scene(TypedDict):
    """What each zone is set to: a color, or an effect that runs until the zone gets a new color.
    Saving, undo and redo all work on scenes. Transitions and timed effects aren't part of one, so
    they're never saved."""
    left: Color | Fade
    center: Color | Fade
    right: Color | Fade
    extra: Color | Fade


def solid_scene(v: Color) -> Scene:
    return Scene(left=v, center=v, right=v, extra=v)


def load_scene(config: Config) -> Scene:
    scene = solid_scene(config.palette[0])
    try:
        with open(config.state_file) as f:
            saved = json.load(f)
        for z in ZONES:
            v = saved.get(z)
            if isinstance(v, int):  # old format: palette index
                scene[z] = config.palette[v % len(config.palette)]
            elif (c := parse_hex(v)) is not None:
                scene[z] = c
        fades = dict(saved.get("fades") or {})
        for z in ZONES:
            if (fade := Fade.load(fades.get(z))):
                scene[z] = fade
    except (OSError, ValueError, AttributeError, TypeError):
        pass
    return scene


def save_scene(scene: Scene, config: Config) -> None:
    try:
        os.makedirs(os.path.dirname(config.state_file), exist_ok=True)
        tmp = config.state_file + ".tmp"
        t = now()
        colors = {z: v if isinstance(v := scene[z], str) else v.at(t) for z in ZONES}
        fades = {z: v.saved() for z in ZONES if isinstance(v := scene[z], Fade)}
        with open(tmp, "w") as f:
            json.dump({**colors, "fades": fades}, f)
        os.replace(tmp, config.state_file)
    except OSError as ex:
        log("save failed:", ex)


# --- Actions: colors and effects to show ---

ActionName: TypeAlias = Literal["set", "cycle", "rainbow", "pulse"]


ACTION_NAMES: tuple[ActionName, ...] = get_args(ActionName)


class Command(NamedTuple):
    colors: tuple[int, int]        # how many colors it accepts (min, max)
    time: float                    # seconds per fade when --time isn't given
    min_time: float                # shortest transition time allowed
    timed: bool                    # whether it accepts a duration (--for)


def command(name: ActionName, config: Config) -> Command:
    if name == "set":  # fade to a color (0: at once)
        return Command((1, 1), 0.0, 0.0, False)
    if name == "cycle":
        return Command((2, MAX_CYCLE), config.cycle_delay, 0.1, True)
    if name == "rainbow":
        return Command((0, 0), config.rainbow_delay, 0.1, True)
    return Command((1, 1), config.pulse_delay, 0.1, True)  # pulse


def parse_name(s: str) -> ActionName | None:
    return next((n for n in ACTION_NAMES if n == s), None)


@dataclass(frozen=True)
class Action:
    """A color or effect that a Menu combo, the dialog or kbdrgb asks for. As a string, it's the
    arguments you'd type after kbdrgb to get it, such as `cycle -z all -t 2 FF0000 0000FF`."""
    name: ActionName
    target: Target = ALL
    colors: tuple[Color, ...] = ()
    # seconds to fade to the color, or for an effect, seconds per color; None: the command's
    # default (Command.time)
    time: float | None = None
    duration: float | None = None  # effects: stop after this long; None: runs forever
    # the curve for every fade, except into colors that have their own in `eases`; None: linear
    ease: Ease | None = None
    # the curve each color was given with COLOR:CURVE; None: use `ease`. () if no color has one.
    # Only set and cycle actions can have these.
    eases: tuple[Ease | None, ...] = ()

    def _own_curves(self) -> list[Ease | None]:
        return list(self.eases) + [None] * (len(self.colors) - len(self.eases))

    def curves(self) -> list[Ease]:
        default = LINEAR if self.ease is None else self.ease
        return [curve or default for curve in self._own_curves()]

    def steps(self) -> list[str]:
        return [c + (f":{ease_text(curve)}" if curve else "") for c, curve in zip(self.colors, self._own_curves())]

    def problem(self, config: Config) -> str:
        cmd = command(self.name, config)
        low, high = cmd.colors
        if not low <= len(self.colors) <= high:
            if high == 0:
                return f"{self.name} doesn't have color arguments."
            return f"{self.name} requires {'one color' if high == 1 else f'{low} to {high} colors'}."
        if self.time is not None and not cmd.min_time <= self.time <= MAX_TIME:
            return f"The time must be from {cmd.min_time:g} to {MAX_TIME:g} seconds."
        if self.duration is not None and not cmd.timed:
            return f"A duration doesn't work with {self.name}."
        if self.duration is not None and not 0.1 <= self.duration <= MAX_FOR:
            return f"The duration must be from 0.1 seconds to a day ({MAX_FOR:g} seconds)."
        if any(self.eases) and self.name not in ("set", "cycle"):
            return f"{self.name} uses one curve for every step, so COLOR:CURVE doesn't work with it."
        if len(self.eases) > len(self.colors):
            return "There are more curves than colors."
        return ""

    def run(self, daemon: Daemon) -> None:
        lights, config = daemon.lights, daemon.config
        period = command(self.name, config).time if self.time is None else self.time
        default = LINEAR if self.ease is None else self.ease
        if self.name == "set":
            lights.set(self.target, self.colors[0], period, self.curves()[0])
        else:
            curves = self.curves() if self.name == "cycle" else [default]
            fades = {z: Fade.effect(self.name, z, list(self.colors), period, config, curves)
                     for z in zones_of(self.target)}
            what = f"{self.target} {self.name} {' '.join(self.steps())}".rstrip() + f" every {period:g}s"
            what += f" {ease_text(default)}" if default != LINEAR else ""
            lights.animate(fades, what, self.duration)

    def __str__(self) -> str:
        words = ["color" if self.name == "set" else self.name, "-z", self.target]
        words += ["-t", f"{self.time:g}"] if self.time is not None else []
        words += ["-d", f"{self.duration:g}"] if self.duration is not None else []
        words += ["-e", ease_text(self.ease)] if self.ease is not None else []
        return " ".join(words + self.steps())

    @classmethod
    def from_dialog(cls, text: str, target: Target, config: Config) -> tuple[Action | None, str]:
        """Words are separated by spaces or commas. The first word can be "rainbow", "pulse" or
        "cycle" to start that effect. Otherwise, a single color becomes a `set` action, and two or
        more colors become a `cycle`.

        A number at the end is the time, unless it has 6 digits. A 6-digit number is read as a
        color.

        A curve on its own can go anywhere, and it applies to every step. If there is more than one,
        the last one is used. COLOR:CURVE applies only to fading into that color."""
        try:
            return cls._read_dialog(text, target, config), ""
        except ParseError as ex:
            return None, str(ex)

    @classmethod
    def _read_dialog(cls, text: str, target: Target, config: Config) -> Action:
        curve: Ease | None = None
        items: list[tuple[str, Ease | None]] = []  # words in order; a COLOR:CURVE with its curve
        for token in text.split():
            token = token.strip(",")
            if ":" in token:
                items.append(parse_step(token))
            elif (parsed := parse_ease(token)) is not None:
                curve = parsed
            elif re.fullmatch(r"[0-9.]*(,[0-9.]*){3}", token):
                parse_curve(token)  # looks like a curve but isn't one: says why
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
        colors = [parse_color(w) for w, _ in items]
        if name is None:
            if not colors:
                raise ParseError("Type a color like FF8800.")
            name = "set" if len(colors) == 1 else "cycle"
        eases = tuple(step_curve for _, step_curve in items)
        action = cls(name, target, tuple(colors), time, ease=curve, eases=eases if any(eases) else ())
        if problem := action.problem(config):
            raise ParseError(problem)
        return action

    def dialog_text(self) -> str:
        """The inverse of from_dialog. The dialog has no zone or duration, so those are left out."""
        words: list[str] = [self.name] if self.name in ("rainbow", "pulse") else []
        words += self.steps()
        words += [f"{self.time:g}"] if self.time is not None else []
        words += [ease_text(self.ease)] if self.ease is not None else []
        return " ".join(words)

    @classmethod
    def current(cls, daemon: Daemon, target: Target) -> Action:
        """What `target` is set to now, as an Action. If `target` is the whole keyboard, the action
        is based on the left zone. If a zone is in a transition, the action sets the color that the
        transition ends on. If a zone has a timed effect, the action runs that effect with no time
        limit."""
        f = daemon.lights.setting(zones_of(target)[0])
        if isinstance(f, str):
            return cls("set", target, (f,))
        curves = [f.curve_into(i) for i in range(len(f.colors))]
        uniform = len(set(curves)) == 1
        ease = curves[0] if uniform and curves[0] != LINEAR else None
        if f.colors == daemon.config.rainbow:
            return cls("rainbow", target, (), f.period, ease=ease)
        if len(f.colors) == 2 and f.colors[1] == mix("000000", f.colors[0], daemon.config.pulse_dim):
            return cls("pulse", target, f.colors[:1], f.period, ease=ease)
        eases = () if uniform else tuple(c if c != LINEAR else None for c in curves)
        return cls("cycle", target, f.colors, f.period, ease=ease, eases=eases)


# --- The lights ---

class History:
    """Tracks scenes for undo and redo."""

    def __init__(self, config: Config) -> None:
        self.past: deque[Scene] = deque(maxlen=config.undo_depth)
        self.future: deque[Scene] = deque(maxlen=config.undo_depth)
        # when there's nothing left to undo, undo() turns the whole keyboard this color
        self.white = config.palette[0]

    def remember(self, scene: Scene) -> None:
        self._keep_past(scene)
        self.future.clear()

    def undo(self, scene: Scene) -> Scene:
        """Goes back to the most recent scene that's different from `scene`."""
        while self.past and self.past[-1] == scene:
            self.past.pop()
        back = self.past.pop() if self.past else solid_scene(self.white)
        if back != scene:
            self.future.append(scene.copy())
        return back

    def redo(self, scene: Scene) -> Scene | None:
        if not self.future:
            return None
        self._keep_past(scene)
        return self.future.pop()

    def _keep_past(self, scene: Scene) -> None:
        if not self.past or self.past[-1] != scene:
            self.past.append(scene.copy())  # the oldest drops off past undo_depth

    def __str__(self) -> str:
        return f"{len(self.past)} to undo, {len(self.future)} to redo"


class Leds:
    """Writes zone colors to the configured LED files. It isn't thread-safe, so Lights only uses it
    with its lock held."""

    def __init__(self, config: Config) -> None:
        self.config = config
        self.written: dict[Zone, Color] = {}  # last color written to each zone

    def write(self, zone: Zone, color: Color) -> bool:
        try:
            with open(f"{self.config.led}/color_{zone}", "w") as f:
                f.write(color)
        except OSError as ex:
            log("write failed:", zone, ex)
            return False
        self.written[zone] = color
        return True

    def show(self, zone: Zone, color: Color) -> bool:
        return color != self.written.get(zone) and self.write(zone, color)


class Frames:
    """Decides when to write a fading zone's next color, so fades don't send the keyboard controller
    more than WRITE_RATE writes a second. A fading zone is written at most once every `frame`
    seconds.

    It isn't thread-safe, so Lights only uses it with its lock held."""

    def __init__(self, frame: float) -> None:
        # shortest time between two writes to the same fading zone, in seconds
        self.frame = frame
        self.drawn: dict[Zone, float] = {}     # when each zone was last written or picked by next()
        self.last_write = -math.inf            # when any zone was last written
        self.due_at: float | None = None       # when the next fade write is due; None: nothing is fading

    def wrote(self, zone: Zone, t: float) -> None:
        """Records a write to `zone` at time `t`."""
        self.drawn[zone] = self.last_write = t

    def schedule(self, fading: Sequence[Zone]) -> bool:
        """Works out when the next fade write is due, given the zones in `fading`. Returns True if
        one is now due and none was before, so the caller knows to wake render()."""
        was = self.due_at
        if fading:
            ready = min(self.drawn.get(z, -math.inf) + self.frame for z in fading)
            self.due_at = max(ready, self.last_write + 1 / WRITE_RATE)
        else:
            self.due_at = None
        return was is None and self.due_at is not None

    def due(self, t: float) -> bool:
        return self.due_at is not None and t >= self.due_at

    def wait(self, t: float) -> float | None:
        return None if self.due_at is None else max(0.0, self.due_at - t)

    def next(self, fading: Sequence[Zone], t: float) -> Zone | None:
        ready = [z for z in fading if t - self.drawn.get(z, -math.inf) >= self.frame - 1e-9]
        if not ready:
            return None
        zone = min(ready, key=lambda z: self.drawn.get(z, -math.inf))
        self.drawn[zone] = t
        return zone


class Lights:
    """What the keyboard shows.

    `scene` is what each zone is set to. On top of it, a zone can have an entry in `overlays` that
    ends on its own, either a transition to its new color or a timed effect. Caps lock shows over
    everything while it's on.

    Several threads use it, so one lock covers all of it."""

    def __init__(self, config: Config) -> None:
        self._config = config
        self.lock = threading.Lock()
        self.frames_due = threading.Condition(self.lock)  # wakes render() when fading starts
        self.leds = Leds(config)
        self.frames = Frames(config.frame)
        self.history = History(config)
        self.scene = load_scene(config)
        self.overlays: dict[Zone, Fade] = {}   # transitions and timed effects, until they end
        self.caps = False
        self.caps_fade: Fade | None = None     # the left zone's fade while caps lock is on
        self.stopping = False
        with self.lock:
            self._changed(ZONES, now(), save=False)

    # What each zone shows

    def running(self) -> dict[Zone, Fade]:
        """The zones that are fading, with each one's overlay or effect. Ignores caps lock."""
        return {z: f for z in ZONES if (f := self._fade(z))}

    def setting(self, zone: Zone) -> Color | Fade:
        with self.lock:
            o = self.overlays.get(zone)
            return o if o is not None and not o.transition else self.scene[zone]

    def time_left(self, zone: Zone) -> float | None:
        with self.lock:
            o = self.overlays.get(zone)
            if o is None or o.transition or o.until is None:
                return None
            return max(0.0, o.until - now())

    def _fade(self, zone: Zone) -> Fade | None:
        if (f := self.overlays.get(zone)) is not None:
            return f
        v = self.scene[zone]
        return v if isinstance(v, Fade) else None

    def _visible_fade(self, zone: Zone) -> Fade | None:
        if self.caps:
            return self.caps_fade if zone == "left" else None
        return self._fade(zone)

    def _below_caps(self, zone: Zone, t: float) -> Color:
        f = self._fade(zone)
        if f is not None:
            return f.at(t)
        v = self.scene[zone]
        assert isinstance(v, str)
        return v

    def shown(self, zone: Zone, t: float | None = None) -> Color:
        t = now() if t is None else t
        if self.caps:
            return self.caps_fade.at(t) if zone == "left" and self.caps_fade else self._config.caps_other
        return self._below_caps(zone, t)

    def _fading(self) -> list[Zone]:
        return [z for z in ZONES if self._visible_fade(z)]

    def _changed(self, zones: Sequence[Zone], t: float, save: bool = True) -> None:
        """After a change, writes the new colors of `zones` to their LED files, reschedules fade
        writes and saves the scene. The lock must be held."""
        if self.frames.schedule(self._fading()):
            self.frames_due.notify()
        self._draw(zones, t)
        if save:
            save_scene(self.scene, self._config)

    def _draw(self, zones: Sequence[Zone], t: float) -> None:
        for z in zones:
            self._write(z, self.shown(z, t), always=True)

    def _write(self, zone: Zone, color: Color, always: bool = False) -> None:
        """Writes `color` to the zone's LED file, unless `always` is False and `color` was the last
        color written there. The lock must be held."""
        if (self.leds.write if always else self.leds.show)(zone, color):
            self.frames.wrote(zone, now())

    # Changes

    def set(self, target: Target, color: Color, seconds: float = 0.0, curve: Ease = LINEAR) -> None:
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
        """Starts an effect on `fades`' zones. If `seconds` is given, the effect goes in an overlay.
        When the time is up, tick() sets the scene to the effect's last color."""
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
                c = self._config
                self.caps_fade = Fade(c.caps_colors, c.caps_delay, now(),
                                      eases=curves_for(len(c.caps_colors), [c.caps_ease])) if on else None
                self._changed(ZONES, now(), save=False)

    # Undo and redo

    def remember(self) -> None:
        """Saves the current scene for undo. Call it before a change."""
        with self.lock:
            self.history.remember(self.scene)

    def undo(self) -> None:
        with self.lock:
            scene = self.history.undo(self.scene)
            self._restore(scene)
        self._log("undo", scene)

    def redo(self) -> None:
        with self.lock:
            scene = self.history.redo(self.scene)
            if scene is None:
                log("redo: nothing to redo")
                return
            self._restore(scene)
        self._log("redo", scene)

    def _restore(self, scene: Scene) -> None:
        self.scene = scene.copy()
        self.overlays.clear()
        self._changed(ZONES, now())

    def _log(self, what: str, scene: Scene) -> None:
        def text(v: Color | Fade) -> str:
            return v if isinstance(v, str) else " ".join(v.colors)
        log(f"{what}:", " ".join(f"{z}={text(scene[z])}" for z in ZONES), f"({self.history})")

    # Fade frames

    def next_due(self) -> float | None:
        """When the next fade write is due, or None if nothing is fading. Only the tests use this."""
        return self.frames.due_at

    def render(self) -> None:
        """Draws fades until stop()."""
        while True:
            try:
                with self.lock:
                    while not self.stopping and (wait := self.frames.wait(now())) != 0:
                        self.frames_due.wait(wait)  # None: until a fade starts and notifies frames_due
                    if self.stopping:
                        return
                self.tick()
            except Exception:
                log("frame failed:\n" + traceback.format_exc())
                with self.lock:
                    self.frames_due.wait_for(lambda: self.stopping, 1)

    def stop(self) -> None:
        with self.lock:
            self.stopping = True
            self.frames_due.notify_all()

    def tick(self) -> None:
        with self.lock:
            t = now()
            if not self.frames.due(t):
                return
            ended = [z for z, f in self.overlays.items() if f.until is not None and t >= f.until]
            for z in ended:  # final colors go out right away
                f = self.overlays.pop(z)
                if not f.transition:  # a timed effect stays on its last frame
                    self.scene[z] = f.at(f.until or t)
                self._write(z, self.shown(z, t))
            if (frame := self.frames.next(self._fading(), t)) is not None:
                color = self.shown(frame, t)
                if looks_different(self.leds.written.get(frame), color, self._config.visible_step):
                    self._write(frame, color)
            self.frames.schedule(self._fading())
            if ended:
                save_scene(self.scene, self._config)
                log("ended:", " ".join(f"{z}={self.scene[z]}" for z in ended))


@dataclass(frozen=True)
class Daemon:
    """What the parts of the daemon share: its settings and the lights."""
    config: Config
    lights: Lights


# --- kbdrgb's commands ---

class UsageError(Exception):
    pass


class CommandParser(argparse.ArgumentParser):
    """The argparse parser for kbdrgb's commands. It raises UsageError where argparse would normally
    exit, which would stop the daemon."""

    def error(self, message: str) -> NoReturn:
        raise UsageError(message)

    def exit(self, status: int = 0, message: str | None = None) -> NoReturn:
        raise UsageError(message or "")


def _seconds(s: str) -> float:
    if (n := parse_decimal(s)) is None:
        raise ParseError(f'"{s}" isn\'t a number of seconds like 2 or 0.5.')
    return n


def _zone(s: str) -> Target:
    if (t := parse_target(s)) is None:
        raise ParseError(f'"{s}" isn\'t a zone. Use left, center, right, extra (the numpad), or all.')
    return t


# Each command's number of colors (argparse nargs), and the letters of the options it takes,
# out of --zone, --time, --for and --ease.
CLI_COMMANDS: dict[str, tuple[int | str, str]] = {
    "color": (1, "zte"), "zones": ("+", "te"), "cycle": ("+", "zted"), "rainbow": (0, "zted"), "pulse": (1, "zted")}
CLI_OPTIONS = {"z": ("zone", "--zone"), "t": ("time", "--time"), "d": ("duration", "--for"), "e": ("ease", "--ease")}


def _command_parser() -> CommandParser:
    parser = CommandParser(prog="kbdrgb", add_help=False)
    commands = parser.add_subparsers(dest="command", required=True, metavar="COMMAND")
    for name, (colors, _) in CLI_COMMANDS.items():
        p = commands.add_parser(name, prog=f"kbdrgb {name}", add_help=False)
        p.add_argument("-z", "--zone", type=_zone)
        p.add_argument("-t", "--time", type=_seconds)
        p.add_argument("-d", "--for", dest="duration", type=_seconds)
        p.add_argument("-e", "--ease", type=parse_curve)
        if colors:
            p.add_argument("colors", metavar="COLOR", nargs=colors, type=str if name == "zones" else parse_step)
        else:
            p.set_defaults(colors=[])
    return parser


COMMAND_LINE = _command_parser()


def parse_command(args: Sequence[str], config: Config) -> list[Action]:
    ns = COMMAND_LINE.parse_args(list(args))
    for letter, (dest, option) in CLI_OPTIONS.items():
        if getattr(ns, dest) is not None and letter not in CLI_COMMANDS[ns.command][1]:
            raise UsageError(f"{option} doesn't work with {ns.command}.")
    if ns.command == "zones":
        actions = zone_actions(ns.colors, ns.time, ns.ease, config)
    else:
        name = parse_name("set" if ns.command == "color" else ns.command)
        assert name is not None
        steps: list[tuple[Color, Ease | None]] = ns.colors
        eases = tuple(curve for _, curve in steps)
        actions = [Action(name, ns.zone or ALL, tuple(c for c, _ in steps), ns.time, ns.duration, ns.ease,
                          eases if any(eases) else ())]
    for action in actions:
        if problem := action.problem(config):
            raise UsageError(problem)
    return actions


def zone_actions(texts: Sequence[str], time: float | None, ease: Ease | None, config: Config) -> list[Action]:
    """Turns the arguments of `kbdrgb zones` into one action for each zone. Each string in `texts`
    is one zone's color or effect, read like text typed in the color dialog. Zones without a string
    are set to white. `time` and `ease` apply to any zone whose text doesn't give its own."""
    if len(texts) > len(ZONES):
        raise UsageError("zones requires 1 to 4 colors or effects, for left, center, right and the numpad. "
                         "Zones left out are white.")
    actions = []
    for zone, text in zip(ZONES, [*texts, *["FFFFFF"] * (len(ZONES) - len(texts))]):
        action, problem = Action.from_dialog(text, zone, config)
        if action is None:
            raise UsageError(f"{zone} zone: {problem}")
        actions.append(replace(action, time=time if action.time is None else action.time,
                               ease=ease if action.ease is None else action.ease))
    return actions


def run_request(daemon: Daemon, args: Sequence[str]) -> str:
    try:
        actions = parse_command(args, daemon.config)
    except UsageError as ex:
        log("kbdrgb", " ".join(args)[:200], "->", ex)
        return str(ex) or "bad command"
    daemon.lights.remember()
    for action in actions:
        action.run(daemon)
    return ""


def status_text(daemon: Daemon) -> str:
    try:
        with open(f"{daemon.config.led}/brightness") as f:
            lines = [f"brightness: {f.read().strip()}"]
    except OSError as ex:
        lines = [f"brightness: unknown ({ex.strerror})"]
    texts = []
    for zone in ZONES:
        texts.append(text := Action.current(daemon, zone).dialog_text())
        left = daemon.lights.time_left(zone)
        lines.append(f"{zone + ':':8}{text}" + (f"  (for {left:.0f}s more)" if left is not None else ""))
    lines.append("kbdrgb zones " + " ".join(shlex.quote(t) for t in texts))
    return "\n".join(lines)


class CommandSocket:
    """Runs the commands that kbdrgb sends over a Unix socket.

    kbdrgb connects once for each command. It sends its command line arguments separated by NUL
    bytes, then closes its end for writing. The daemon replies with "ok", followed by the output for
    status, or with a message saying what's wrong."""

    def __init__(self, daemon: Daemon) -> None:
        self.daemon, self.config = daemon, daemon.config
        self.stopping = threading.Event()
        path = self.config.socket
        os.makedirs(os.path.dirname(path), mode=0o755, exist_ok=True)
        with contextlib.suppress(FileNotFoundError):
            os.unlink(path)
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            self.listener.bind(path)
            try:
                # kbdrgb needs write permission on the socket to connect. The udev rule sets the color
                # files' group to the user's own group, so giving the socket that group too lets the same
                # user run kbdrgb without root.
                os.chown(path, -1, os.stat(f"{self.config.led}/color_left").st_gid)
                os.chmod(path, 0o660)
            except OSError as ex:
                log("socket permissions:", ex)
            self.listener.listen(8)
        except BaseException:
            self.listener.close()
            raise

    def answer(self) -> None:
        while True:
            try:
                conn, _ = self.listener.accept()
            except OSError as ex:
                if self.stopping.is_set():
                    return
                log("accept failed:", ex)
                self.stopping.wait(1)
                continue
            with conn:
                try:
                    conn.settimeout(self.config.request_timeout)
                    conn.sendall(self._reply(conn).encode() + b"\n")
                except OSError as ex:
                    log("kbdrgb request failed:", ex)
                except Exception:
                    log("kbdrgb request failed:\n" + traceback.format_exc())

    def _reply(self, conn: socket.socket) -> str:
        data = b""
        while len(data) <= self.config.max_request and (chunk := conn.recv(4096)):
            data += chunk
        if len(data) > self.config.max_request:
            return "command too long"
        if data and not data.endswith(b"\0"):
            return "bad request"
        args = data.decode("utf-8", errors="replace").split("\0")[:-1]
        if args[:1] == ["status"]:  # the reply's lines after "ok" are for kbdrgb to show
            return "ok\n" + status_text(self.daemon) if len(args) == 1 else "status takes no arguments"
        return run_request(self.daemon, args) or "ok"

    def stop(self) -> None:
        self.stopping.set()
        with contextlib.suppress(OSError):
            self.listener.shutdown(socket.SHUT_RDWR)

    def close(self) -> None:
        """Closes the socket and removes its file. Without the file, kbdrgb knows the daemon isn't
        running."""
        self.listener.close()
        with contextlib.suppress(FileNotFoundError):
            os.unlink(self.config.socket)


# --- The desktop session ---

def session_user() -> str | None:
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
    """Runs `cmd` as `user` in `user`'s desktop session, with an empty environment plus the
    variables for reaching that session's Wayland display and D-Bus."""
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


# --- The color dialog ---

def dialog_help_text(config: Config) -> str:
    """Builds the text shown above the color dialog's entry box: the palette, one color per line.
    zenity shows this text without markup and in a proportional font, so it can't be formatted or
    split into columns."""
    colors = zip(config.palette, config.palette_names)
    return "\n".join(f"{n}   {c}   {name}" for n, (c, name) in enumerate(colors))


def _markup(s: str) -> str:
    """Text for zenity's --text, which reads Pango markup."""
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


_dialog_busy = threading.Lock()


def hex_dialog(daemon: Daemon, target: Target) -> None:
    """Asks the user for a color or effect for `target`, and runs it."""
    if not _dialog_busy.acquire(blocking=False):
        return  # one dialog at a time
    try:
        user = session_user()
        if not user:
            log("color dialog: no active desktop session found")
            return
        label = "the whole keyboard" if target == ALL else f"the {target} zone"
        current = Action.current(daemon, target).dialog_text()
        while True:
            rc, out = run_zenity(user, "--entry", "--title=Keyboard color",
                                 f"--text=Color or effect for {label}:\n\n{dialog_help_text(daemon.config)}",
                                 f"--entry-text={current}")
            if rc != 0:
                return  # cancelled / closed
            action, problem = Action.from_dialog(out, target, daemon.config)
            if action is not None:
                daemon.lights.remember()
                action.run(daemon)
                return
            run_zenity(user, "--error", "--title=Keyboard color", f"--text={_markup(problem)}")
            current = out.strip()
    except Exception as ex:
        log("dialog failed:", ex)
    finally:
        _dialog_busy.release()


# --- Menu key combos ---

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


@dataclass
class Selection:
    target: Target
    key: int                # the key code of the zone key (or Space) pressed with Menu
    held: bool = True       # whether that key is still down. While it is, digits add colors to a cycle.
    # each picked color, with when its digit was pressed
    picks: list[tuple[Color, float]] = field(default_factory=list)
    reopen: bool = True     # whether pressing the key again opens the dialog. Picking a cycle turns this off.


class Handler:
    """Key event logic, separated from device I/O for testing."""

    def __init__(self, daemon: Daemon, emit: Callable[[int, int], None],
                 open_dialog: Callable[[Target], None]) -> None:
        self.daemon, self.emit, self.open_dialog = daemon, emit, open_dialog
        self.lights, self.config = daemon.lights, daemon.config
        self.mod_down = False
        self.mod_used = False
        # whether the scene is already saved for undo during this Menu hold, so its changes are one undo step
        self.remembered = False
        self.sel: Selection | None = None
        # keys whose press wasn't passed on, so their repeats and release aren't passed on either
        self.swallowed: set[int] = set()
        self.shifts: set[int] = set()     # Shift keys held down (for Menu+Shift+Backspace)

    def _finish_cycle(self) -> None:
        sel = self.sel
        if sel is None:
            return
        picks, sel.picks = sel.picks, []
        if len(picks) < 2:
            return
        period = min(MAX_TIME, max(command("cycle", self.config).min_time, picks[1][1] - picks[0][1]))
        Action("cycle", sel.target, tuple(c for c, _ in picks[:MAX_CYCLE]), period).run(self.daemon)
        sel.reopen = False  # pressing the zone key again starts over, not the dialog

    def key(self, ev: InputEvent) -> bool:
        """Return True if ev should be forwarded to apps."""
        code, val = ev.code, ev.value  # val: 1 down, 0 up, 2 repeat
        if code in (e.KEY_LEFTSHIFT, e.KEY_RIGHTSHIFT):  # tracked whether apps get it or not
            if val == 0:
                self.shifts.discard(code)
            else:
                self.shifts.add(code)
        if code == self.config.mod_key:
            if val == 1:
                self.mod_down, self.mod_used, self.remembered, self.sel = True, False, False, None
            elif val == 0:
                self._finish_cycle()  # Menu let go before the zone key: keep what was picked
                self.mod_down, self.sel = False, None
                if not self.mod_used:  # lone tap -> real Menu key
                    self.emit(code, 1); self.emit(code, 0)
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
            color = self.config.palette[DIGIT[code]]
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


# --- Startup problems, shown as desktop notifications ---

class ProblemState(TypedDict, total=False):
    key: str        # which problem, e.g. "not-found:<device>" or "crash:OSError"
    count: int      # failed starts in a row with this problem
    notified: bool  # whether the desktop user has been notified


def _load_problem(config: Config) -> ProblemState:
    try:
        with open(config.problem_file) as f:
            state: ProblemState = json.load(f)
            return state
    except (OSError, ValueError):
        return {}


def report_problem(key: str, message: str, config: Config) -> None:
    """Records a failed start. Once the same problem has caused `report_after` failed starts in a
    row, the desktop user gets one notification about it, so a brief hiccup doesn't notify anyone.
    If the notification couldn't be shown, for example because nobody was logged in, the next failed
    start tries again."""
    state = _load_problem(config)
    if state.get("key") != key:
        state = {"key": key, "count": 0, "notified": False}
    state["count"] = state.get("count", 0) + 1
    if not state.get("notified") and state["count"] >= config.report_after:
        state["notified"] = desktop_notify(
            "Keyboard colors stopped working",
            f"{message} The kbd-rgb service keeps retrying; see journalctl -u kbd-rgb.")
    try:
        os.makedirs(os.path.dirname(config.problem_file), mode=0o755, exist_ok=True)
        with open(config.problem_file, "w") as f:
            json.dump(state, f)
    except OSError as ex:
        log("can't record problem:", ex)


def report_recovered(config: Config) -> None:
    """Called once the daemon has run for stable_after seconds."""
    state = _load_problem(config)
    if not state:
        return
    try:
        os.unlink(config.problem_file)
    except OSError:
        pass
    if state.get("notified"):
        desktop_notify("Keyboard colors are working again", "The kbd-rgb service is running.")
    log("recovered from:", state.get("key"))


# --- Input devices ---

class EventSource(Protocol):
    """An input device we read events from: the keyboard or our uinput device."""
    fd: int
    def read(self) -> Iterator[InputEvent]: ...


def find_keyboard(spec: str) -> str | None:
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


def caps_lock_on(config: Config) -> bool:
    """Whether caps lock is on, read from its LED files. It's only used at startup. After that,
    changes arrive as LED events."""
    for path in glob.glob(config.caps_glob):
        try:
            with open(path) as f:
                if f.read().strip() != "0":
                    return True
        except OSError:
            pass
    return False


def ungrab(dev: evdev.InputDevice) -> None:
    try:
        dev.ungrab()
    except OSError:
        pass  # device already gone


# --- Running ---
    
def main(argv: Sequence[str] | None = None) -> None:
    parser = Config.parser()
    parser.add_argument("--check", action="store_true", help="check the options, then exit (install.sh uses this)")
    args = parser.parse_args(argv)
    config = Config.from_args(args)
    if problem := config.problem():
        if not args.check:  # the service itself: tell the desktop user, as for other failed starts
            report_problem(f"config:{problem}", f"The kbd-rgb daemon's options don't work: {problem} "
                           "Run install.sh again with different ones.", config)
        parser.error(problem)
    if args.check:
        return
    try:
        device = config.device
        path = find_keyboard(device)
        if not path:
            log(f"keyboard not found: {device!r} (systemd will retry)")
            report_problem(f"not-found:{device}", f'The kbd-rgb daemon can\'t find the keyboard "{device}".', config)
            sys.exit(1)
        with contextlib.ExitStack() as cleanup:
            serve(cleanup, config, path)
    except SystemExit:
        raise
    except Exception as ex:
        log("crashed:\n" + traceback.format_exc())
        kind = type(ex).__name__
        report_problem(f"crash:{kind}", f"The kbd-rgb daemon crashed ({kind}: {ex}).", config)
        sys.exit(1)


def serve(cleanup: contextlib.ExitStack, config: Config, keyboard_path: str) -> None:
    lights = Lights(config)
    lights.set_caps(caps_lock_on(config))
    daemon = Daemon(config, lights)
    commands = CommandSocket(daemon)
    cleanup.callback(commands.close)
    dev = evdev.InputDevice(keyboard_path)
    cleanup.callback(dev.close)
    ui = evdev.UInput.from_device(dev, name=config.virtual_name)
    cleanup.callback(ui.close)
    time.sleep(0.5)  # let keys held at startup (e.g. Enter from launching) be released
    dev.grab()
    cleanup.callback(ungrab, dev)
    log(f"grabbed {dev.path} ({dev.name}); forwarding via {ui.device.path}")
    render = threading.Thread(target=lights.render, daemon=True, name="frames")
    render.start()

    def stop_render() -> None:
        lights.stop(); render.join(1.0)

    cleanup.callback(stop_render)
    answer = threading.Thread(target=commands.answer, daemon=True, name="commands")
    answer.start()

    def stop_commands() -> None:
        commands.stop(); answer.join(1.0)

    cleanup.callback(stop_commands)

    def emit(code: int, val: int) -> None:
        ui.write(e.EV_KEY, code, val); ui.syn()

    def open_dialog(target: Target) -> None:
        threading.Thread(target=hex_dialog, args=(daemon, target), daemon=True).start()

    h = Handler(daemon, emit, open_dialog)
    stable_at: float | None = time.monotonic() + config.stable_after  # one-shot: report recovery once running a while
    while True:
        timeout = None if stable_at is None else max(0.0, stable_at - time.monotonic())
        ready, _, _ = select.select([dev.fd, ui.fd], [], [], timeout)
        if stable_at is not None and time.monotonic() >= stable_at:
            stable_at = None
            report_recovered(config)
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


if __name__ == "__main__":
    main()
