# kbd-rgb — System76 Serval WS keyboard lighting

Hardware: 4 zones only (left, center, right, extra = numpad), no per-key control.
Zones blend into each other on a slight diagonal. Driver: `system76_acpi`, sysfs at
`/sys/class/leds/system76::kbd_backlight/` (`color_left|center|right|extra` take `RRGGBB`,
`brightness` 0-255, `trigger`).

## Pieces

| File | What | Runs as |
|---|---|---|
| `99-system76-kbd-rgb.rules` + `install.sh` | udev rule template that makes the sysfs color/brightness/trigger files group-writable by the installing user (`install.sh` fills in `@USER@` from `$SUDO_USER` and installs it to `/etc/udev/rules.d/`), so no sudo is needed to set colors. Only the `kbdrgb` CLI needs this; the daemon runs as root | udev at boot |
| `kbdrgb` | CLI: `color`, `zones`, `brightness`, `rainbow`, `pulse`, `trigger`, `status` | user |
| `kbd-rgb-daemon.py` + `kbd-rgb.service` | Menu-key color combos + caps lock indicator. Grabs `/dev/input/event4` exclusively and re-emits keys via uinput | root system service |
| `install-daemon.sh` | Copies the daemon to `/usr/local/lib/kbd-rgb/` and the unit to `/etc/systemd/system/`, then starts it | sudo |

## Daemon behavior

All while holding Menu (`KEY_COMPOSE`, between right Alt and right Ctrl); releasing Menu resets the selection.
- Press a non-digit key: selects that key's zone (Space = whole keyboard).
- Press 0-9 (top row only; numpad digits are numpad-zone selectors): sets the selected zone to
  `PALETTE[n]` (0 = white). Top-row digits never select a zone (they're excluded from `_Z`); with nothing selected they color the whole keyboard. The zone stays
  selected, so more digits retry colors.
- Press the selecting key again: opens a zenity hex-entry dialog in the active desktop session
  (user found via `loginctl` on seat0, launched via `runuser` with a minimal Wayland env).
- Backspace: reset the whole keyboard to white at any point (Backspace is not part of any zone).
- Lone Menu tap passes through as a normal Menu key.
- Caps lock on: left zone `CAPS_COLOR`, others white; saved colors return when it turns off.
- Zone colors persist in `/var/lib/kbd-rgb/state.json` as hex (old palette-index format is migrated).
- Key→zone mapping is the `_Z` dict; config constants are at the top of the file. Key logic lives in
  `Handler` so it can be tested without devices.

## Changing things

- Edit the copy in this folder, then the user must re-run `sudo ~/Source/kbd-rgb/install-daemon.sh`
  in a real terminal (it copies the files and restarts the service). The root copy in `/usr/local/lib` is
  deliberately not user-writable.
- sudo has no passwordless access and `! sudo ...` in Claude Code fails (no TTY), so root steps
  must be handed to the user to run in a terminal.
- A bug in the daemon can swallow keystrokes. Test event handling with fake devices before
  asking the user to install. Recovery: `sudo systemctl stop kbd-rgb` or reboot (if not enabled at boot).
