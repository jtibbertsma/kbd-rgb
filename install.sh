#!/bin/sh
# Installs kbd-rgb. Run with sudo from your own account:
#   sudo ./install.sh                     grab the built-in keyboard
#   sudo ./install.sh --device DEVICE     grab DEVICE instead: a /dev/input path, or a device
#                                         name such as a key remapper's virtual keyboard
#  1. udev rule: makes the keyboard's sysfs color files writable by you, so kbdrgb works
#     without sudo and can tell the daemon to save colors.
#  2. kbdrgb command: copied to /usr/local/bin so it's on your PATH.
#  3. Daemon: copied to /usr/local/lib/kbd-rgb (out of your home dir, so only root can
#     modify what runs as root) and (re)started as the kbd-rgb systemd service.
# Re-run after changing the code. Running without --device goes back to the built-in keyboard.
set -e
cd "$(dirname "$0")"

usage() { sed -n '2,5p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 1; }
user="" device=""
while [ $# -gt 0 ]; do
    case $1 in
        --device)   [ $# -ge 2 ] || usage; device=$2; shift 2 ;;
        --device=*) device=${1#--device=}; shift ;;
        -h|--help)  usage ;;
        -*)         usage ;;
        *)          user=$1; shift ;;
    esac
done

user="${SUDO_USER:-$user}"
if [ -z "$user" ] || [ "$user" = root ] || ! id "$user" >/dev/null 2>&1; then
    echo "Run this with sudo from your own account, or pass a username: sudo $0 USER" >&2
    exit 1
fi
case $device in
    *'"'*|*'\'*|*'
'*) echo "--device can't contain quotes, backslashes, or newlines" >&2; exit 1 ;;
esac

# udev rule first: the daemon gives its notify pipe the color files' group.
sed "s/@USER@/$user/g" 99-system76-kbd-rgb.rules > /etc/udev/rules.d/99-system76-kbd-rgb.rules
chmod 644 /etc/udev/rules.d/99-system76-kbd-rgb.rules
udevadm control --reload
udevadm trigger --action=add /sys/class/leds/system76::kbd_backlight
udevadm settle

if [ -n "$device" ]; then
    printf 'KBD_DEVICE="%s"\n' "$device" > /etc/default/kbd-rgb
    if [ ! -e "$device" ] && ! grep -qxF "N: Name=\"$device\"" /proc/bus/input/devices; then
        echo "Note: no input device named or at \"$device\" right now; the daemon will keep retrying." >&2
    fi
else
    rm -f /etc/default/kbd-rgb
fi

install -m 755 kbdrgb /usr/local/bin/kbdrgb

install -d -m 755 /usr/local/lib/kbd-rgb
install -m 644 kbd_rgb_daemon.py /usr/local/lib/kbd-rgb/
rm -f /usr/local/lib/kbd-rgb/kbd-rgb-daemon.py  # its old name
install -m 644 kbd-rgb.service /etc/systemd/system/
systemctl daemon-reload
systemctl restart kbd-rgb.service
sleep 1
systemctl --no-pager status kbd-rgb.service | head -12
