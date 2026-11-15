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
Caps lock on     -> left zone shows CAPS_COLOR, other zones white
                    (saved colors return when caps lock turns off)

Keys pressed while Menu is held are not passed to apps. The keyboard is
grabbed exclusively and all other input is re-emitted via a uinput device.
Zone colors (hex) persist in STATE_FILE.

The main loop sleeps in select() on:
  - the keyboard            -> key events
  - the uinput device       -> LED events the desktop sends it (caps lock on/off)
  - the NOTIFY named pipe   -> "sync" from the kbdrgb CLI after it writes colors,
                               so the daemon saves them
"""
import glob, json, os, pwd, re, select, subprocess, threading, time
import evdev
from evdev import ecodes as e

KBD = "/dev/input/by-path/platform-i8042-serio-0-event-kbd"
LED = "/sys/class/leds/system76::kbd_backlight"
STATE_FILE = "/var/lib/kbd-rgb/state.json"
NOTIFY = "/run/kbd-rgb/notify"
CAPS_GLOB = "/sys/class/leds/input*::capslock/brightness"
MOD_KEY = e.KEY_COMPOSE
CAPS_COLOR = "FF6600"
PALETTE = ["FFFFFF", "FF0000", "FF6600", "FFFF00", "00FF00",
           "00FFAA", "0088FF", "0000FF", "8800FF", "FF00AA"]
ZONES = ["left", "center", "right", "extra"]
ALL = "all"

# Approximate key -> zone mapping (zones blend, so this only needs to be close).
_Z = {
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
KEY_ZONE = {}
for zone, names in _Z.items():
    for n in names.split():
        code = e.ecodes.get("KEY_" + n)
        if code is not None:
            KEY_ZONE[code] = zone
KEY_ZONE[e.KEY_SPACE] = ALL

DIGIT = {getattr(e, f"KEY_{n}"): n for n in range(10)}


def log(*a):
    print(*a, flush=True)


def parse_hex(s):
    s = (s or "").strip().lstrip("#")
    if re.fullmatch(r"[0-9A-Fa-f]{3}", s):
        s = "".join(c * 2 for c in s)
    return s.upper() if re.fullmatch(r"[0-9A-Fa-f]{6}", s) else None


class Lights:
    def __init__(self):
        self.lock = threading.Lock()
        self.color = {z: PALETTE[0] for z in ZONES}
        self.caps = False
        try:
            with open(STATE_FILE) as f:
                saved = json.load(f)
            for z in ZONES:
                v = saved.get(z)
                if isinstance(v, int):  # old format: palette index
                    self.color[z] = PALETTE[v % len(PALETTE)]
                elif parse_hex(v):
                    self.color[z] = parse_hex(v)
        except (OSError, ValueError, AttributeError):
            pass
        self.apply()

    def write(self, zone, color):
        try:
            with open(f"{LED}/color_{zone}", "w") as f:
                f.write(color)
        except OSError as ex:
            log("write failed:", zone, ex)

    def apply(self):
        for z in ZONES:
            if self.caps:  # caps overlay: left zone CAPS_COLOR, others white; saved colors untouched
                color = CAPS_COLOR if z == "left" else "FFFFFF"
            else:
                color = self.color[z]
            self.write(z, color)

    def save(self):
        try:
            os.makedirs(os.path.dirname(STATE_FILE), exist_ok=True)
            tmp = STATE_FILE + ".tmp"
            with open(tmp, "w") as f:
                json.dump(self.color, f)
            os.replace(tmp, STATE_FILE)
        except OSError as ex:
            log("save failed:", ex)

    def get(self, target):
        return self.color["left" if target == ALL else target]

    def set(self, target, color):
        with self.lock:
            for z in (ZONES if target == ALL else [target]):
                self.color[z] = color
            self.apply(); self.save()
        log(f"{target} -> {color}")

    def sync_external(self):
        """Adopt colors another program (e.g. kbdrgb) wrote to sysfs, and save them.

        Called when kbdrgb sends "sync" on the NOTIFY pipe. Ignored while the caps
        lock overlay is showing. Returns True if anything changed.
        """
        with self.lock:
            if self.caps:
                return False
            changed = False
            for z in ZONES:
                try:
                    with open(f"{LED}/color_{z}") as f:
                        c = parse_hex(f.read())
                except OSError:
                    continue
                if c and c != self.color[z]:
                    self.color[z] = c
                    changed = True
            if changed:
                self.save()
                log("saved external colors:", " ".join(f"{z}={self.color[z]}" for z in ZONES))
            return changed

    def set_caps(self, on):
        with self.lock:
            if on != self.caps:
                self.caps = on
                self.apply()


def caps_lock_on():
    """Current caps lock state, read once at startup (later changes arrive as LED events)."""
    for path in glob.glob(CAPS_GLOB):
        try:
            with open(path) as f:
                if f.read().strip() != "0":
                    return True
        except OSError:
            pass
    return False


def open_notify():
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


def read_events(device):
    try:
        return list(device.read())
    except BlockingIOError:
        return []


_dialog_busy = threading.Lock()


def session_user():
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


def run_zenity(user, *args):
    """Run zenity in the user's desktop session. Returns (returncode, stdout)."""
    pw = pwd.getpwnam(user)
    rt = f"/run/user/{pw.pw_uid}"
    env = [f"XDG_RUNTIME_DIR={rt}", f"DBUS_SESSION_BUS_ADDRESS=unix:path={rt}/bus",
           f"HOME={pw.pw_dir}", f"USER={user}", "PATH=/usr/bin:/bin"]
    sockets = sorted(p for p in glob.glob(f"{rt}/wayland-*") if not p.endswith(".lock"))
    if sockets:
        env.append(f"WAYLAND_DISPLAY={os.path.basename(sockets[0])}")
    r = subprocess.run(["runuser", "-u", user, "--", "env", "-i", *env, "zenity", *args],
                       capture_output=True, text=True)
    return r.returncode, r.stdout


def hex_dialog(lights, target):
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

    def __init__(self, lights, emit, open_dialog):
        self.lights, self.emit, self.open_dialog = lights, emit, open_dialog
        self.mod_down = False
        self.mod_used = False
        self.sel = None        # selected zone or ALL
        self.sel_key = None    # key that made the selection
        self.swallowed = set() # keys whose press we ate; eat their repeat/release too

    def key(self, ev):
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


def main():
    lights = Lights()
    lights.set_caps(caps_lock_on())
    notify_fd = open_notify()

    dev = evdev.InputDevice(os.path.realpath(KBD))
    ui = evdev.UInput.from_device(dev, name="kbd-rgb virtual keyboard")
    time.sleep(0.5)  # let keys held at startup (e.g. Enter from launching) be released
    dev.grab()
    log(f"grabbed {dev.path} ({dev.name}); forwarding via {ui.device.path}")

    def emit(code, val):
        ui.write(e.EV_KEY, code, val); ui.syn()

    def open_dialog(target):
        threading.Thread(target=hex_dialog, args=(lights, target), daemon=True).start()

    h = Handler(lights, emit, open_dialog)
    try:
        while True:
            ready, _, _ = select.select([dev.fd, ui.fd, notify_fd], [], [])
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
                try:
                    while os.read(notify_fd, 4096):
                        pass
                except BlockingIOError:
                    pass
                lights.sync_external()
    finally:
        os.close(notify_fd)
        try:
            dev.ungrab()
        except OSError:
            pass
        ui.close()


if __name__ == "__main__":
    main()
