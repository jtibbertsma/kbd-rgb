#!/bin/sh
# Uninstalls kbd-rgb: run with sudo ./uninstall.sh
# Stops and removes the daemon and its service (with its options), the kbdrgb command, the
# udev rule, and the saved colors. The keyboard keeps its current colors.
set -e
if [ "$(id -u)" -ne 0 ]; then
    echo "Run this with sudo: sudo $0" >&2
    exit 1
fi

# Stopping the daemon releases the keyboard right away.
systemctl disable --now kbd-rgb.service 2>/dev/null || true
rm -f /etc/systemd/system/kbd-rgb.service
rm -rf /etc/systemd/system/kbd-rgb.service.d
systemctl daemon-reload
systemctl reset-failed kbd-rgb.service 2>/dev/null || true

rm -rf /usr/local/lib/kbd-rgb /var/lib/kbd-rgb /run/kbd-rgb
rm -f /etc/default/kbd-rgb /usr/local/bin/kbdrgb

rm -f /etc/udev/rules.d/99-system76-kbd-rgb.rules
udevadm control --reload
# Put the sysfs files back to root-only now instead of at the next reboot.
led=/sys/class/leds/system76::kbd_backlight
if [ -d "$led" ]; then
    for f in brightness trigger color_left color_center color_right color_extra; do
        [ -e "$led/$f" ] && chgrp root "$led/$f" && chmod g-w "$led/$f"
    done
fi

echo "kbd-rgb uninstalled."
