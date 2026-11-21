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
| The same key twice | Open the [color dialog](#color-dialog) for that zone (Space twice: the whole keyboard) |
| **Backspace** | Undo: go back to the colors and effects before the last change (see below) |
| **Shift + Backspace** | Redo: go forward again through what you undid |

Top-row numbers only set colors; they never select a zone. The zone stays selected while you hold Menu,
so you can try several colors in a row. Letting go of Menu clears the selection. Tapping Menu on its own
still opens the context menu as usual.

### Undo and redo

Menu + **Backspace** goes back to how the keyboard looked before the last change, and pressing it again
goes further back, up to 100 changes. It covers changes from Menu combos, the dialog, and `kbdrgb`, and
brings effects back in step, as if they never stopped. Everything you do in one hold of Menu is one
change, however many colors you try along the way, and so is `kbdrgb zones`. Undo never brings back a timed effect (`--for`): its zone goes back to what it
showed before the effect started, while other zones keep their own history. With nothing left to go back
to, the whole keyboard turns white.

Menu + **Shift + Backspace** goes forward again through what you undid, until you make a new change. Shift
can go down before or after Menu.

The history lives in memory, so it starts fresh when the daemon restarts.

### Cycles

To make a cycle, keep holding the zone key (or Space) and press two or more numbers. Each one shows its
color as you press it. When you let go of the zone key, the zone starts cycling through those colors.
The time between your first two numbers sets how long each color lasts (0.1 to 60 seconds), so tap
quickly for a fast cycle or wait a few seconds for a slow one. Press the zone key again to build
another. For example, Menu + hold **Q** + **1**, (wait a second) **3**, **7**, then let go of **Q**:
the left zone cycles red, yellow, blue, a second per color.

### Color dialog

Hold Menu and press a zone's key twice (or Space twice for the whole keyboard) to open a box where you
can type any color or effect. The box lists the [numbered colors](#palette) with their hex codes, and
starts filled in with what the zone is showing now (for the whole keyboard, what the left zone shows),
so you can tweak it or type over it. Press Enter to apply it, or Escape to close the box and leave
the zone as it was.

Colors are 6 hex digits, like `FF8800` (a leading `#` and lowercase work too). Separate words with
spaces or commas. A plain number at the end is the time in seconds:

| Type | Effect |
|---|---|
| `FF8800` | Set the color at once |
| `FF8800 2` | Fade to it over 2 seconds (up to 60) |
| `FF0000 0000FF` | Cycle through 2 to 16 colors, a second per color |
| `FF0000 0000FF 3` | Cycle, 3 seconds per color (0.1 to 60) |
| `rainbow` | Rainbow, 0.6 seconds per hue |
| `rainbow 0.3` | Rainbow at a different speed |
| `pulse 00FFAA` | Fade between the color and a dim copy of it, half a second each way |
| `pulse 00FFAA 1.5` | Pulse at a different speed |
| `FF0000 0000FF 2 ease-in-out` | Any of these with an [easing curve](#easing) |
| `FF0000 FFFF00:ease-in 0000FF 2` | A curve for fading into one color |

Effects keep going until the zone gets a new color, and match what `kbdrgb cycle`, `rainbow`, and
`pulse` do (see [Command-line tool](#command-line-tool)). If what you typed doesn't work, an error says
why, and the box opens again with your text so you can fix it. Only one dialog opens at a time.

### Palette

| # | Color | # | Color |
|---|---|---|---|
| 0 | white `FFFFFF` | 5 | teal `00FFAA` |
| 1 | red `FF0000` | 6 | sky blue `0088FF` |
| 2 | orange `FF6600` | 7 | blue `0000FF` |
| 3 | yellow `FFFF00` | 8 | purple `8800FF` |
| 4 | green `00FF00` | 9 | pink `FF00AA` |

These are the defaults; `--palette` changes them (see [Settings](#settings)).

### Caps lock and saving

While caps lock is on, the left zone stays red with a quick flash of white every 1.4 seconds, and the
other zones are solid white. Your colors and effects come back when you turn caps lock off. Effects keep time
underneath, and colors you set while caps lock is on show then.

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
they're created. Running `install.sh` without `--device` goes back to the built-in keyboard. If the device
isn't present when the daemon starts, it retries every 2 seconds.

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
kbdrgb zones FF0000,0000FF 0000FF,FF0000 pulse,00FF00 rainbow  # an effect for each zone
kbdrgb cycle AA00AA 00FF00                 # fade between 2 to 16 colors, over and over
kbdrgb rainbow -t 0.3                      # hues flowing across the zones, twice as fast
kbdrgb pulse 00FFFF -z extra -d 10         # breathe the numpad for 10 seconds
kbdrgb pulse 00FFFF -t 1.5 -e ease-in-out  # breathe slowly, easing in and out
kbdrgb brightness 128                      # 0-255, whole keyboard
kbdrgb status
```

Every color command takes the same options:

| Option | Meaning | Commands |
|---|---|---|
| `-z`, `--zone ZONE` | `left`, `center`, `right`, `extra` (numpad), or `all` (the default) | `color`, `cycle`, `rainbow`, `pulse` |
| `-t`, `--time SECONDS` | seconds to fade from one color to the next | `color` (default 0: at once), `zones`, `cycle` (1), `rainbow` (0.6), `pulse` (0.5) |
| `-d`, `--for SECONDS` | stop after this long, keeping the last frame | `cycle`, `rainbow`, `pulse` |
| `-e`, `--ease EASE` | how each fade speeds up and slows down (see [Easing](#easing)) | `color` and `zones` (with `-t`), `cycle`, `rainbow`, `pulse` |

`zones` takes each zone's color or effect the way the [color dialog](#color-dialog) does, with commas
(or quoted spaces) between the words: `00FF00,0000FF` cycles, `"00FF00 0000FF 3"` cycles with 3
seconds per color, `pulse,00FFAA` pulses. `-t` and `-e` apply to the zones that don't give their own
time or curve. Zones left out are white.

The daemon runs the effects. They keep going, even after a reboot, until their zone gets a new color from
`kbdrgb`, a Menu combo, or the color dialog. Each zone has its own effect and time, so you can combine
them, for example a slow cycle on the left and a fast one on the numpad.

Effects with the same colors and `--time` stay in step, however far apart you start them, because they
all count from the same moment. Rotating the colors gives each zone a step of its own, which makes a
rainbow chase:

```sh
kbdrgb zones FF0000,FFFF00,00FF00,0000FF 0000FF,FF0000,FFFF00,00FF00 00FF00,0000FF,FF0000,FFFF00 FFFF00,00FF00,0000FF,FF0000
```

Without the daemon, `kbdrgb` can still set plain colors at once (`color`, and `zones` with only colors), set brightness,
and show the status.

While caps lock is on, the left zone shows the caps lock fade and the others are white; a color or
effect you give any zone shows once caps lock turns off.

### Easing

By default each fade moves from one color to the next at a steady pace. An easing curve changes that,
as in CSS: `ease-in` starts slow and speeds up, `ease-out` starts fast and slows down, `ease-in-out`
does both, and `ease` is a gentle version of `ease-in-out`. `linear` is the steady default. For your own
curve, give the two control points of a cubic bezier as `X1,Y1,X2,Y2`, each from 0 to 1, like
`0.4,0,0.2,1` (tools like [cubic-bezier.com](https://cubic-bezier.com) draw them). The keyboard shows 10
frames a second per zone, so curves show best on fades of a second or more.

`--ease` sets the curve for every step. To give one step its own curve, write it after the color as
`COLOR:CURVE`: that's the curve for fading into that color, and it takes the place of `--ease` there.
This works for `cycle`, `color`, and `zones` (`pulse` and `rainbow` take one curve for all their steps):

```sh
# ease into yellow, snap into blue, then back to red at a steady pace
kbdrgb cycle FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1 -t 2
# every step eases in, except the fade into green
kbdrgb cycle FF0000 00FF00:linear 0000FF -t 2 -e ease-in
```

## How it works

`kbd_rgb_daemon.py` runs as a root systemd service. It grabs the built-in keyboard
(`/dev/input/by-path/platform-i8042-serio-0-event-kbd`, or the `--device` you installed with) so key
presses go only to it, handles Menu combos itself, and forwards every other key through a uinput virtual
keyboard. It writes colors to the sysfs files and saves them to `/var/lib/kbd-rgb/state.json`.

The color dialog runs `zenity` as the user of the active desktop session (found with `loginctl`).

## Settings

The daemon's settings are command line options, which `install.sh` takes too and passes on to the
service. Lists of colors or names are comma-separated:

```sh
sudo ./install.sh --caps-colors FF0000,0000FF --caps-delay 0.5 --mod-key RIGHTMETA
```

Each run replaces the options from the last one, and running `install.sh` without options goes back to
the defaults. `install.sh` checks them before installing anything. They include:

- `--palette`, `--palette-names`: the ten colors for 0–9, and their names in the color dialog
- `--caps-colors`, `--caps-delay`, `--caps-ease`: the colors the left zone fades through while caps lock
  is on (a repeated color holds), the seconds from each to the next, and the easing curve into each
- `--caps-other`: the color of the other zones while caps lock is on
- `--undo-depth`: how many changes undo can go back through (and redo forward through)
- `--cycle-delay`, `--rainbow`, `--rainbow-delay`, `--rainbow-lag`, `--pulse-delay`, `--pulse-dim`: the
  `cycle`, `rainbow`, and `pulse` effects
- `--mod-key`: the key to hold for the color combos (Menu, `COMPOSE`, by default)
- `--device`: the keyboard to grab (see [Using it with other input tools](#using-it-with-other-input-tools))

`python3 kbd_rgb_daemon.py --help` lists every option with its default, including paths the tests
change. `kbdrgb` only finds the default socket and LED paths.

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
