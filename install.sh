#!/bin/sh
# Installs the udev rule that lets you set keyboard colors without root.
# Run with sudo from your own account: sudo ./install.sh
set -e
user="${SUDO_USER:-$1}"
if [ -z "$user" ] || [ "$user" = root ] || ! id "$user" >/dev/null 2>&1; then
    echo "Run this with sudo from your own account, or pass a username: sudo $0 USER" >&2
    exit 1
fi
sed "s/@USER@/$user/g" "$(dirname "$0")/99-system76-kbd-rgb.rules" > /etc/udev/rules.d/99-system76-kbd-rgb.rules
chmod 644 /etc/udev/rules.d/99-system76-kbd-rgb.rules
udevadm control --reload
udevadm trigger --action=add /sys/class/leds/system76::kbd_backlight
ls -l /sys/class/leds/system76::kbd_backlight/
