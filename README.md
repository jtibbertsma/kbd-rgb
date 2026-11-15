# kbd-rgb

RGB keyboard control for the System76 Serval WS: change zone colors from the keyboard with Menu-key
combos, get a caps lock indicator, and pick any color with a hex color dialog.

The Serval WS keyboard has four lighting zones (left, center, right, and the numpad). It doesn't have
per-key lighting, and the zones blend into each other. The `system76_acpi` kernel driver exposes them
at `/sys/class/leds/system76::kbd_backlight/`.

Other System76 laptops with the same 4-zone driver may work, but the key-to-zone layout is tuned for
the Serval WS and they haven't been tested.

## Controls

Hold **Menu** (the key between right Alt and right Ctrl) and:

| Press | Effect |
|---|---|
| Any letter, symbol, F-key, or numpad key | Select that key's zone |
| **Space** | Select the whole keyboard |
| **0–9** (top row) | Set the selected zone's color. With nothing selected, colors the whole keyboard |
| The same key twice | Open a dialog to type a hex color (e.g. `FF8800`) |
| **Backspace** | Reset the whole keyboard to white |

Top-row numbers only set colors; they never select a zone. The zone stays selected while you hold Menu,
so you can try several colors in a row. Letting go of Menu clears the selection. Tapping Menu on its own
still opens the context menu as usual.

| # | Color | # | Color |
|---|---|---|---|
| 0 | white `FFFFFF` | 5 | teal `00FFAA` |
| 1 | red `FF0000` | 6 | sky blue `0088FF` |
| 2 | orange `FF6600` | 7 | blue `0000FF` |
| 3 | yellow `FFFF00` | 8 | purple `8800FF` |
| 4 | green `00FF00` | 9 | pink `FF00AA` |

While caps lock is on, the left zone turns orange and the other zones turn white. Your colors come back
when you turn caps lock off.

Colors are saved and restored after a reboot.

## Requirements

- A System76 laptop using the `system76_acpi` driver with a 4-zone RGB keyboard
- Python 3 with `python3-evdev`
- `zenity` for the hex color dialog
- systemd and `loginctl`

## Install

```sh
git clone https://github.com/jtibbertsma/kbd-rgb.git
cd kbd-rgb
sudo ./install-daemon.sh
```

This copies the daemon to `/usr/local/lib/kbd-rgb/`, installs `kbd-rgb.service`, and starts it. It
doesn't start at boot yet, so if anything goes wrong a reboot undoes it. Once it's working:

```sh
sudo systemctl enable kbd-rgb
```

Re-run `sudo ./install-daemon.sh` after changing the code; it copies the new version and restarts the
service.

**If your keyboard stops responding**, run `sudo systemctl stop kbd-rgb` (or reboot, if you haven't
enabled it yet). The daemon takes exclusive control of the built-in keyboard and passes keys through a
virtual keyboard, so stopping it hands the keyboard straight back.

## Command-line tool (optional)

`kbdrgb` sets colors and effects from a terminal:

```sh
./kbdrgb color 1E90FF                        # all zones one color
./kbdrgb zones FF0000 00FF00 0000FF FF00FF   # left, center, right, numpad
./kbdrgb brightness 128                      # 0-255
./kbdrgb rainbow 20                          # cycle colors for 20 seconds
./kbdrgb pulse 00FFFF 10                     # fade in and out for 10 seconds
./kbdrgb trigger kbd-capslock                # let the kernel drive the backlight; "none" to stop
./kbdrgb status
```

The keyboard's sysfs files are only writable by root. To use `kbdrgb` without sudo, install the udev
rule, which makes them writable by your user:

```sh
sudo ./install.sh
```

## How it works

`kbd-rgb-daemon.py` runs as a root systemd service. It grabs the built-in keyboard
(`/dev/input/by-path/platform-i8042-serio-0-event-kbd`) so key presses go only to it, handles Menu
combos itself, and forwards every other key through a uinput virtual keyboard. It writes colors to the
sysfs files, watches the caps lock LED state, and saves colors to `/var/lib/kbd-rgb/state.json`.

The hex color dialog runs `zenity` as the user of the active desktop session (found with `loginctl`).

Settings are at the top of `kbd-rgb-daemon.py`:

- `PALETTE`: the ten colors for 0–9
- `CAPS_COLOR`: the caps lock color
- `_Z`: which keys belong to which zone

## Tests

```sh
python3 -I test_daemon.py
```

The tests use fake input devices and LED files, so they don't need root or the hardware.

## Uninstall

```sh
sudo systemctl disable --now kbd-rgb
sudo rm -rf /etc/systemd/system/kbd-rgb.service /usr/local/lib/kbd-rgb /var/lib/kbd-rgb
sudo rm -f /etc/udev/rules.d/99-system76-kbd-rgb.rules
sudo systemctl daemon-reload
```

## License

MIT. See [LICENSE](LICENSE).
