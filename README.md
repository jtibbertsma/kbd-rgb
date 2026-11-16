# kbd-rgb

RGB keyboard control for the System76 Serval WS: change zone colors from the keyboard with Menu-key
combos, get a caps lock indicator, and pick any color or effect with a quick dialog.

The Serval WS keyboard has four lighting zones (left, center, right, and the numpad). It doesn't have
per-key lighting, and the zones blend into each other. The `system76_acpi` kernel driver exposes them
at `/sys/class/leds/system76::kbd_backlight/`.

Other System76 laptops with the same 4-zone driver may work, but the key-to-zone layout is tuned for
the Serval WS and they haven't been tested.

**Heads up:** the daemon takes over the built-in keyboard and passes your keys on through a virtual
keyboard. Most things work as before, but other programs that read or grab the built-in keyboard directly
(key remappers, hotkey tools) get nothing from it, and the desktop sees your typing as coming from a
separate USB keyboard. That can stop keyboard-specific features like touchpad disable-while-typing from
applying. See [Using it with other input tools](#using-it-with-other-input-tools).

## Controls

Hold **Menu** (the key between right Alt and right Ctrl) and:

| Press | Effect |
|---|---|
| Any letter, symbol, F-key, or numpad key | Select that key's zone |
| **Space** | Select the whole keyboard |
| **0–9** (top row) | Set the selected zone's color. With nothing selected, colors the whole keyboard |
| **0–9** while still holding the zone key | Build a cycle through those colors (see below) |
| The same key twice | Open a dialog to type a color or effect (see below) |
| **Backspace** | Reset the whole keyboard to white |

Top-row numbers only set colors; they never select a zone. The zone stays selected while you hold Menu,
so you can try several colors in a row. Letting go of Menu clears the selection. Tapping Menu on its own
still opens the context menu as usual.

To make a cycle, keep holding the zone key (or Space) and press two or more numbers. Each one shows its
color as you press it. When you let go of the zone key, the zone starts cycling through those colors.
The time between your first two numbers sets how long each color lasts (0.1 to 60 seconds), so tap
quickly for a fast cycle or wait a few seconds for a slow one. Press the zone key again to build
another. For example, Menu + hold **Q** + **1**, (wait a second) **3**, **7**, then let go of **Q**:
the left zone cycles red, yellow, blue, a second per color.

The dialog starts with what the zone is showing now, and takes a color or an effect. A number at the
end is the time in seconds:

| Type | Effect |
|---|---|
| `FF8800` | Set the color |
| `FF8800 2` | Fade to it over 2 seconds |
| `FF0000 0000FF` | Cycle through the colors (2 to 16) |
| `FF0000 0000FF 3` | Cycle, 3 seconds per color |
| `rainbow`, `rainbow 0.3` | Rainbow, optionally at a different speed |
| `pulse 00FFAA`, `pulse 00FFAA 1.5` | Pulse |

| # | Color | # | Color |
|---|---|---|---|
| 0 | white `FFFFFF` | 5 | teal `00FFAA` |
| 1 | red `FF0000` | 6 | sky blue `0088FF` |
| 2 | orange `FF6600` | 7 | blue `0000FF` |
| 3 | yellow `FFFF00` | 8 | purple `8800FF` |
| 4 | green `00FF00` | 9 | pink `FF00AA` |

While caps lock is on, the left zone fades between red and white, a second each way, and the other zones
keep their colors. The left zone's color comes back when you turn caps lock off.

Colors are saved and restored after a reboot, including colors set with the `kbdrgb` command-line tool
(see below).

## Requirements

- A System76 laptop using the `system76_acpi` driver with a 4-zone RGB keyboard
- Python 3 with `python3-evdev`
- `zenity` for the color dialog
- systemd and `loginctl`

## Install

```sh
git clone https://github.com/jtibbertsma/kbd-rgb.git
cd kbd-rgb
sudo ./install.sh
```

This installs three things:

- a udev rule that makes the keyboard's sysfs color files writable by your user, for the `kbdrgb` tool
- the `kbdrgb` command, copied to `/usr/local/bin/`
- the daemon, copied to `/usr/local/lib/kbd-rgb/` and started as the `kbd-rgb` systemd service

The service starts at boot only once you enable it, so if anything goes wrong a reboot undoes it. Once
it's working:

```sh
sudo systemctl enable kbd-rgb
```

Re-run `sudo ./install.sh` after changing the code; it copies the new version and restarts the service.

### Using it with other input tools

The daemon grabs the built-in keyboard, so it's the only program that receives that keyboard's events.
It passes keys on through a virtual keyboard named `kbd-rgb virtual keyboard`, which the desktop reads.
To use another tool that reads or grabs keyboard input, such as a key remapper, either:

- point that tool at `kbd-rgb virtual keyboard`, or
- let the tool grab the built-in keyboard and point the daemon at the tool's virtual keyboard:

  ```sh
  sudo ./install.sh --device "keyd virtual keyboard"
  ```

`--device` takes a device name (as listed in `/proc/bus/input/devices`) or a `/dev/input/...` path.
Names are the better choice for virtual keyboards, whose `/dev/input/eventN` number can change each time
they're created. The setting is saved in `/etc/default/kbd-rgb`; running `install.sh` without `--device`
goes back to the built-in keyboard. If the device isn't present when the daemon starts, it retries every
2 seconds.

**If your keyboard stops responding**, run `sudo systemctl stop kbd-rgb` (or reboot, if you haven't
enabled it yet). The daemon takes exclusive control of the built-in keyboard and passes keys through a
virtual keyboard, so stopping it hands the keyboard straight back.

If the daemon fails to start 3 times in a row (for example, the keyboard it's set to read isn't there),
or keeps crashing, you get a desktop notification, and another one once it has been running again for 30
seconds. systemd keeps retrying every 2 seconds in the meantime; `journalctl -u kbd-rgb` shows the
details. Your keyboard keeps working normally while the daemon is down.

## Command-line tool

`kbdrgb` sets colors and effects from a terminal:

```sh
kbdrgb color 1E90FF                        # all zones one color
kbdrgb color FF0000 -z left -t 2           # fade the left zone to red over 2 seconds
kbdrgb zones FF0000 00FF00 0000FF FF00FF   # left, center, right, numpad
kbdrgb cycle AA00AA 00FF00                 # fade between 2 to 16 colors, over and over
kbdrgb rainbow -t 0.3                      # hues flowing across the zones, twice as fast
kbdrgb pulse 00FFFF -z extra -d 10         # breathe the numpad for 10 seconds
kbdrgb brightness 128                      # 0-255, whole keyboard
kbdrgb status
```

Every color command takes the same options:

| Option | Meaning | Commands |
|---|---|---|
| `-z`, `--zone ZONE` | `left`, `center`, `right`, `extra` (numpad), or `all` (the default) | `color`, `cycle`, `rainbow`, `pulse` |
| `-t`, `--time SECONDS` | seconds to fade from one color to the next | `color` and `zones` (default 0: at once), `cycle` (1), `rainbow` (0.6), `pulse` (0.5) |
| `-d`, `--for SECONDS` | stop after this long, keeping the last frame | `cycle`, `rainbow`, `pulse` |

The daemon runs the effects. They keep going, even after a reboot, until their zone gets a new color from
`kbdrgb`, a Menu combo, or the color dialog. Each zone has its own effect and time, so you can combine
them, for example a slow cycle on the left and a fast one on the numpad.

Effects with the same colors and `--time` stay in step, however far apart you start them, because they
all count from the same moment. Rotating the colors gives each zone a step of its own, which makes a
rainbow chase:

```sh
kbdrgb cycle FF0000 FFFF00 00FF00 0000FF -z left
kbdrgb cycle 0000FF FF0000 FFFF00 00FF00 -z center
kbdrgb cycle 00FF00 0000FF FF0000 FFFF00 -z right
kbdrgb cycle FFFF00 00FF00 0000FF FF0000 -z extra
```

Without the daemon, `kbdrgb` can still set colors at once, set brightness, and show the status.

While caps lock is on, the left zone shows the caps lock fade; a color or effect you give it shows once
caps lock turns off.

## How it works

`kbd-rgb-daemon.py` runs as a root systemd service. It grabs the built-in keyboard
(`/dev/input/by-path/platform-i8042-serio-0-event-kbd`, or the `--device` you installed with) so key
presses go only to it, handles Menu
combos itself, and forwards every other key through a uinput virtual keyboard. It writes colors to the
sysfs files and saves them to `/var/lib/kbd-rgb/state.json`.

The main loop sleeps in `select()` until one of these has input:

- the keyboard (key presses)
- the virtual keyboard, which receives the caps lock LED changes the desktop sends it
- the named pipe `/run/kbd-rgb/notify`, where `kbdrgb` sends commands like
  `cycle zone=left time=2 AA00AA 00FF00` (`set`, `cycle`, `rainbow`, `pulse`)

While caps lock is on or an effect is running, a second thread draws the fading zones, one write at a
time and at most 30 writes a second in all: the keyboard controller falls behind beyond that, and the
numpad zone stops matching. Up to three fading zones get 10 frames a second each; four share the 30. It's separate because each color write takes several
milliseconds, and the main loop has keys to forward.
Caps lock and the effects all use the same fade code: a list of colors, the seconds from each to the
next, and where each zone starts in the list.

The color dialog runs `zenity` as the user of the active desktop session (found with `loginctl`).

Settings are at the top of `kbd-rgb-daemon.py`:

- `PALETTE`: the ten colors for 0–9
- `CAPS_COLORS`, `CAPS_DELAY`: the colors the left zone fades through while caps lock is on, and the
  seconds from each to the next
- `CYCLE_DELAY`: seconds from each color to the next for `kbdrgb cycle`
- `RAINBOW`, `RAINBOW_DELAY`, `RAINBOW_LAG`, `PULSE_DELAY`, `PULSE_DIM`: the `rainbow` and `pulse` effects
- `_Z`: which keys belong to which zone

## Tests

```sh
python3 -I test_daemon.py
```

The tests use fake input devices and LED files, so they don't need root or the hardware.

To type-check the Python code with mypy (settings are in `mypy.ini`):

```sh
uvx mypy --python-executable /usr/bin/python3
```

`--python-executable` points mypy at the system Python, where `python3-evdev` is installed.

## Uninstall

```sh
sudo ./uninstall.sh
```

This stops the daemon (handing the keyboard straight back) and removes everything `install.sh` added,
along with the saved colors. The keyboard keeps whatever colors it has at that moment.

## License

MIT. See [LICENSE](LICENSE).
