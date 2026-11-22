# kbd-rgb

RGB keyboard control for System76 laptops: set colors and effects from the command line or with
Menu-key combos, and get a caps lock indicator.

It's written for, and only tested on, the Serval WS serw14 (`/sys/class/dmi/id/product_version`
shows which model you have). Its keyboard has four lighting zones (left, center, right, and
the numpad), which blend into each other. Other System76 laptops that use the `system76` driver have
the same color files and may work, but they haven't been tested: their keyboards may have fewer zones,
and the Menu combos' key-to-zone layout follows the serw14's.

**Heads up:** the daemon takes over the built-in keyboard and passes your keys on through a virtual
keyboard. Other programs that read or grab the built-in keyboard (key remappers, hotkey tools) get
nothing from it, and keyboard-specific features like touchpad disable-while-typing may stop applying.
See [Using it with other input tools](#using-it-with-other-input-tools).

## Install

Requirements: System76's `system76` driver (from `system76-dkms`) with a 4-zone RGB keyboard, Python 3
with `python3-evdev`, `zenity` (for the color dialog), and systemd.

```sh
git clone https://github.com/jtibbertsma/kbd-rgb.git
cd kbd-rgb
sudo ./install.sh
```

This installs the `kbdrgb` command, a udev rule that lets your user write the keyboard's colors, and the
daemon, started as the `kbd-rgb` systemd service. It doesn't start at boot until you enable it, so if
anything goes wrong a reboot undoes it. Once it's working:

```sh
sudo systemctl enable kbd-rgb
```

Re-run `sudo ./install.sh` after changing the code.

**If the built-in keyboard stops responding**, reboot if you haven't enabled the service yet. Otherwise,
run `sudo systemctl stop kbd-rgb` from an external USB keyboard (the daemon only takes over the built-in
one), the desktop's on-screen keyboard, or SSH. Stopping the daemon hands the keyboard straight back.

## Command line

```sh
kbdrgb color 1E90FF                        # all zones one color
kbdrgb color FF0000 -z left -t 2           # fade the left zone to red over 2 seconds
kbdrgb zones FF0000 00FF00 0000FF FF00FF   # left, center, right, numpad
kbdrgb cycle AA00AA 00FF00                 # fade between 2 to 16 colors, over and over
kbdrgb rainbow -t 0.3                      # hues flowing across the zones, twice as fast
kbdrgb pulse 00FFFF -z extra -d 10         # breathe the numpad for 10 seconds
kbdrgb pulse 00FFFF -t 1.5 -e ease-in-out  # breathe slowly, easing in and out
kbdrgb brightness 128                      # 0-255, whole keyboard
kbdrgb status                              # each zone's color or effect (the default)
```

| Option | Meaning | Commands |
|---|---|---|
| `-z`, `--zone ZONE` | `left`, `center`, `right`, `extra` (numpad), or `all` (the default) | `color`, `cycle`, `rainbow`, `pulse` |
| `-t`, `--time SECONDS` | seconds to fade from one color to the next | `color` (default 0: at once), `zones`, `cycle` (1), `rainbow` (0.6), `pulse` (0.5) |
| `-d`, `--for SECONDS` | stop after this long, keeping the last frame | `cycle`, `rainbow`, `pulse` |
| `-e`, `--ease EASE` | how each fade speeds up and slows down (see [Easing](#easing)) | `color`, `zones`, `cycle`, `rainbow`, `pulse` |

Effects keep going, even after a reboot, until their zone gets a new color. Each zone has its own effect
and time. Effects with the same colors and `--time` stay in step, however far apart you start them.

`zones` takes each zone's color or effect the way the [color dialog](#color-dialog) does, with commas or
quoted spaces between the words. `-t` and `-e` apply to the zones that don't give their own time or
curve, and zones left out are white:

```sh
kbdrgb zones FF0000,0000FF 0000FF,FF0000 pulse,00FF00 rainbow   # an effect for each zone
kbdrgb zones "FF0000 0000FF 3 ease-in" 0000FF,FF0000,0.5        # each with its own time and curve
```

`status` shows each zone's setting the same way (a timed effect says how long it has left), and ends with
a `kbdrgb zones` command that sets them all again, so you can keep a combination you like.

Without the daemon running, `kbdrgb` can only set plain colors, set the brightness, and show the colors.

### Easing

By default each fade moves at a steady pace. An easing curve changes that, as in CSS: `ease-in` starts
slow and speeds up, `ease-out` starts fast and slows down, `ease-in-out` does both, `ease` is a gentle
`ease-in-out`, and `linear` is the default. For your own curve, give a cubic bezier's two control points
as `X1,Y1,X2,Y2`, each from 0 to 1, like `0.4,0,0.2,1` ([cubic-bezier.com](https://cubic-bezier.com)
draws them). Each zone shows up to 10 frames a second (see `--frame`), so curves show best on fades of a
second or more.

`--ease` sets the curve for every step. `COLOR:CURVE` sets the curve for fading into that one color, in
`color`, `cycle`, and `zones`:

```sh
# ease into yellow, snap into blue, then back to red at a steady pace
kbdrgb cycle FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1 -t 2
```

## Keyboard controls

Hold **Menu** (the key between right Alt and right Ctrl) and:

| Press | Effect |
|---|---|
| Any letter, symbol, F-key, or numpad key | Select that key's zone |
| **Space** | Select the whole keyboard |
| **0–9** (top row) | Set the selected zone's [color](#palette). With nothing selected, colors the whole keyboard |
| **0–9** while still holding the zone key | Build a cycle through those colors |
| The same key twice | Open the [color dialog](#color-dialog) for that zone (Space twice: the whole keyboard) |
| **Backspace** | Undo |
| **Shift + Backspace** | Redo |

The zone stays selected while you hold Menu, so you can try several colors in a row. Tapping Menu on its
own still opens the context menu.

**Cycles:** keep holding the zone key and press two or more numbers, then let go of the zone key. The
time between your first two numbers sets how long each color lasts. For example, Menu + hold **Q** +
**1**, (wait a second) **3**, **7**, then let go of **Q**: the left zone cycles red, yellow, blue, a
second per color.

**Undo** goes back through the last 100 changes, from Menu combos, the dialog, or `kbdrgb`. Everything
in one hold of Menu is one change, and so is one `kbdrgb` command. A timed effect (`--for`) is never
brought back: its zone goes back to what it showed before. Redo works until you make a new change. The
history starts fresh when the daemon restarts.

**Caps lock:** while it's on, the left zone stays red with a quick flash of white and the other zones
are white. Your colors and effects come back when you turn it off.

### Color dialog

Hold Menu and press a zone's key twice to type any color or effect. The box lists the palette and starts
with what the zone has now. Enter applies it; Escape leaves the zone as it was. Separate words with
spaces or commas, and put a time in seconds at the end:

| Type | Effect |
|---|---|
| `FF8800` | Set the color at once |
| `FF8800 2` | Fade to it over 2 seconds |
| `FF0000 0000FF 3` | Cycle through 2 to 16 colors, 3 seconds per color (default 1) |
| `rainbow 0.3` | Rainbow, 0.3 seconds per hue (default 0.6) |
| `pulse 00FFAA 1.5` | Fade between the color and a dim copy of it (default 0.5 seconds each way) |
| `FF0000 0000FF 2 ease-in-out` | Any of these with an [easing curve](#easing) |
| `FF0000 FFFF00:ease-in 0000FF` | A curve for fading into one color |

### Palette

| # | Color | # | Color |
|---|---|---|---|
| 0 | white `FFFFFF` | 5 | teal `00FFAA` |
| 1 | red `FF0000` | 6 | sky blue `0088FF` |
| 2 | orange `FF6600` | 7 | blue `0000FF` |
| 3 | yellow `FFFF00` | 8 | purple `8800FF` |
| 4 | green `00FF00` | 9 | pink `FF00AA` |

## Settings

The daemon's settings are command line options, which `install.sh` passes on to the service. Lists are
comma-separated:

```sh
sudo ./install.sh --caps-colors FF0000,0000FF --caps-delay 0.5 --mod-key RIGHTMETA
```

Each run replaces the last run's options, and no options means the defaults. They include:

- `--palette`, `--palette-names`: the colors for 0–9, and their names in the color dialog
- `--caps-colors`, `--caps-delay`, `--caps-ease`, `--caps-other`: the caps lock indicator
- `--undo-depth`: how many changes undo goes back through
- `--cycle-delay`, `--rainbow`, `--rainbow-delay`, `--rainbow-lag`, `--pulse-delay`, `--pulse-dim`: the
  effects' defaults
- `--frame`, `--visible-step`: how often fades are drawn, and how much a color has to change before it's
  drawn again. Smoother fades cost more CPU.
- `--mod-key`: the key to hold for the combos (default `COMPOSE`, the Menu key)
- `--device`: the keyboard to grab (see below)

`python3 kbd_rgb_daemon.py --help` lists them all with their defaults.

### Using it with other input tools

To use another tool that reads or grabs keyboard input, such as a key remapper, either point it at the
virtual keyboard named `kbd-rgb virtual keyboard`, or let it grab the built-in keyboard and point the
daemon at the tool's virtual keyboard:

```sh
sudo ./install.sh --device "keyd virtual keyboard"
```

`--device` takes a device name (from `/proc/bus/input/devices`) or a `/dev/input/...` path. If the device
isn't there, the daemon keeps retrying, and you get a desktop notification after a few failed starts.
`journalctl -u kbd-rgb` shows the details.

## Tests

```sh
python3 -I test_daemon.py
uvx mypy --python-executable /usr/bin/python3
```

The tests use fake input devices and LED files, so they don't need root or the hardware.

## Uninstall

```sh
sudo ./uninstall.sh
```

This stops the daemon and removes everything `install.sh` added. The keyboard keeps its current colors.

## License

MIT. See [LICENSE](LICENSE).
