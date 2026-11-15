#!/bin/sh
# Installs the keyboard color daemon as a root system service.
# The script is copied out of your home dir so only root can modify what runs as root.
set -e
cd "$(dirname "$0")"
install -d -m 755 /usr/local/lib/kbd-rgb
install -m 644 kbd-rgb-daemon.py /usr/local/lib/kbd-rgb/
install -m 644 kbd-rgb.service /etc/systemd/system/
systemctl daemon-reload
systemctl restart kbd-rgb.service
sleep 1
systemctl --no-pager status kbd-rgb.service | head -12
