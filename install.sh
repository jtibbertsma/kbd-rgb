#!/bin/sh
# Installs kbd-rgb. Run with sudo from your own account: sudo ./install.sh
#  1. udev rule: makes the keyboard's sysfs color files writable by you, so kbdrgb works
#     without sudo and can tell the daemon to save colors.
#  2. Daemon: copied to /usr/local/lib/kbd-rgb (out of your home dir, so only root can
#     modify what runs as root) and (re)started as the kbd-rgb systemd service.
# Re-run after changing the code.
set -e
cd "$(dirname "$0")"

user="${SUDO_USER:-$1}"
if [ -z "$user" ] || [ "$user" = root ] || ! id "$user" >/dev/null 2>&1; then
    echo "Run this with sudo from your own account, or pass a username: sudo $0 USER" >&2
    exit 1
fi

# udev rule first: the daemon gives its notify pipe the color files' group.
sed "s/@USER@/$user/g" 99-system76-kbd-rgb.rules > /etc/udev/rules.d/99-system76-kbd-rgb.rules
chmod 644 /etc/udev/rules.d/99-system76-kbd-rgb.rules
udevadm control --reload
udevadm trigger --action=add /sys/class/leds/system76::kbd_backlight
udevadm settle

install -d -m 755 /usr/local/lib/kbd-rgb
install -m 644 kbd-rgb-daemon.py /usr/local/lib/kbd-rgb/
install -m 644 kbd-rgb.service /etc/systemd/system/
systemctl daemon-reload
systemctl restart kbd-rgb.service
sleep 1
systemctl --no-pager status kbd-rgb.service | head -12
