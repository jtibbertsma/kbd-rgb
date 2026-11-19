#!/bin/sh
# Installs kbd-rgb. Run with sudo from your own account:
#   sudo ./install.sh                     the defaults: grab the built-in keyboard
#   sudo ./install.sh --OPTION VALUE ...  the daemon's options, such as --device DEVICE (a
#                                         /dev/input path, or a device name such as a key
#                                         remapper's virtual keyboard) or --caps-delay 0.3.
#                                         python3 kbd_rgb_daemon.py --help lists them all.
#  1. udev rule: makes the keyboard's sysfs color files writable by you, so kbdrgb works
#     without sudo and can tell the daemon to save colors.
#  2. kbdrgb command: copied to /usr/local/bin so it's on your PATH.
#  3. Daemon: copied to /usr/local/lib/kbd-rgb (out of your home dir, so only root can
#     modify what runs as root) and (re)started as the kbd-rgb systemd service, with the
#     options given.
# Re-run after changing the code. Running without options goes back to the defaults.
set -e
cd "$(dirname "$0")"

usage() { sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 1; }

# Sort the arguments: a username (if any) goes in $user, and the daemon's options stay in "$@".
# Every daemon option takes one value: --OPTION VALUE or --OPTION=VALUE.
user="" device="" value_of=""
n=$#
while [ "$n" -gt 0 ]; do
    arg=$1; shift; n=$((n - 1))
    case $arg in
        *'"'*|*'\'*|*'
'*) echo "Options can't contain quotes, backslashes, or newlines: $arg" >&2; exit 1 ;;
    esac
    if [ -n "$value_of" ]; then
        [ "$value_of" = --device ] && device=$arg
        value_of=""
        set -- "$@" "$arg"
        continue
    fi
    case $arg in
        -h|--help|--check) usage ;;
        --device=*) device=${arg#--device=}; set -- "$@" "$arg" ;;
        --*=*)      set -- "$@" "$arg" ;;
        --*)        [ "$n" -gt 0 ] || usage; value_of=$arg; set -- "$@" "$arg" ;;
        -*)         usage ;;
        *)          user=$arg ;;
    esac
done

user="${SUDO_USER:-$user}"
if [ -z "$user" ] || [ "$user" = root ] || ! id "$user" >/dev/null 2>&1; then
    echo "Run this with sudo from your own account, or pass a username: sudo $0 USER" >&2
    exit 1
fi
# The daemon checks its options, so a mistake shows now rather than when the service starts.
/usr/bin/python3 -I kbd_rgb_daemon.py --check "$@"

# udev rule first: the daemon gives its socket the color files' group.
sed "s/@USER@/$user/g" 99-system76-kbd-rgb.rules > /etc/udev/rules.d/99-system76-kbd-rgb.rules
chmod 644 /etc/udev/rules.d/99-system76-kbd-rgb.rules
udevadm control --reload
udevadm trigger --action=add /sys/class/leds/system76::kbd_backlight
udevadm settle

if [ -n "$device" ] && [ ! -e "$device" ] && ! grep -qxF "N: Name=\"$device\"" /proc/bus/input/devices; then
    echo "Note: no input device named or at \"$device\" right now; the daemon will keep retrying." >&2
fi

install -m 755 kbdrgb /usr/local/bin/kbdrgb
rm -f /run/kbd-rgb/notify  # the pipe kbdrgb used before the socket

install -d -m 755 /usr/local/lib/kbd-rgb
install -m 644 kbd_rgb_daemon.py /usr/local/lib/kbd-rgb/
install -m 644 kbd-rgb.service /etc/systemd/system/

# The options go on the daemon's command line, in a drop-in that replaces the service's
# ExecStart. Each one is quoted for systemd, which also reads % and $ in it, so those double.
dropin=/etc/systemd/system/kbd-rgb.service.d
if [ $# -gt 0 ]; then
    exec_start="/usr/bin/python3 -I /usr/local/lib/kbd-rgb/kbd_rgb_daemon.py"
    for arg in "$@"; do
        exec_start="$exec_start \"$(printf '%s' "$arg" | sed 's/%/%%/g; s/\$/$$/g')\""
    done
    install -d -m 755 "$dropin"
    printf '[Service]\nExecStart=\nExecStart=%s\n' "$exec_start" > "$dropin/options.conf"
    chmod 644 "$dropin/options.conf"
else
    rm -f "$dropin/options.conf"
    rmdir "$dropin" 2>/dev/null || true
fi
systemctl daemon-reload
systemctl restart kbd-rgb.service
sleep 1
systemctl --no-pager status kbd-rgb.service | head -12
