# Builds and installs kbd-rgb.
#
#   make                 builds the daemon and the kbdrgb command, as you
#   sudo make install    installs them and starts the kbd-rgb service
#   sudo make uninstall  stops the service and removes everything that install added
#
# "sudo make install" never builds, so that cargo never runs as root. It stops if the binaries are
# missing or older than the code.

SOURCES := Cargo.toml Cargo.lock $(shell find src -name '*.rs')
# cargo doesn't touch a binary that it didn't have to rebuild, so this file records the last build.
BUILT := target/release/.built

LED := /sys/class/leds/system76::kbd_backlight
LED_FILES := brightness trigger color_left color_center color_right color_extra
SERVICE := /etc/systemd/system/kbd-rgb.service
# The file in which the Python version's install.sh put the daemon's options.
OLD_OPTIONS := $(SERVICE).d/options.conf
SETTINGS := /etc/kbd-rgb/config.toml

# The user who gets to write the keyboard's colors. Under sudo, that is the user who ran it.
INSTALL_USER ?= $(SUDO_USER)

.PHONY: build install uninstall

build: $(BUILT)

$(BUILT): $(SOURCES)
	cargo build --release --locked
	touch $@

install:
	@$(MAKE) --question --no-print-directory $(BUILT) || \
		{ echo "The binaries are missing or older than the code. Run make first." >&2; exit 1; }
	@[ "$$(id -u)" -eq 0 ] || { echo "Run this with sudo: sudo make install" >&2; exit 1; }
	@[ -n "$(INSTALL_USER)" ] && [ "$(INSTALL_USER)" != root ] && id "$(INSTALL_USER)" >/dev/null 2>&1 || \
		{ echo "Run this with sudo from your own account, or name a user: make install INSTALL_USER=name" >&2; exit 1; }
	@# A settings file that doesn't work stops the install here, before anything is changed.
	[ ! -e $(SETTINGS) ] || target/release/kbd-rgb-daemon --check
	@# The udev rule goes first, because the daemon gives its socket the color files' group.
	sed "s/@USER@/$(INSTALL_USER)/g" 99-system76-kbd-rgb.rules > /etc/udev/rules.d/99-system76-kbd-rgb.rules
	chmod 644 /etc/udev/rules.d/99-system76-kbd-rgb.rules
	udevadm control --reload
	udevadm trigger --action=add $(LED)
	udevadm settle
	install -D -m 755 target/release/kbd-rgb-daemon /usr/local/lib/kbd-rgb/kbd-rgb-daemon
	install -m 755 target/release/kbdrgb /usr/local/bin/kbdrgb
	install -m 644 kbd-rgb.service $(SERVICE)
	@# Your settings file is never replaced.
	[ -e $(SETTINGS) ] || install -D -m 644 config.toml $(SETTINGS)
	@# What the Python version installed.
	rm -f /usr/local/lib/kbd-rgb/kbd_rgb_daemon.py /run/kbd-rgb/notify
	@if [ -e $(OLD_OPTIONS) ]; then \
		echo "The Python version was installed with these options:"; \
		sed -n 's/^ExecStart=.*kbd_rgb_daemon\.py */  /p' $(OLD_OPTIONS); \
		echo "They no longer apply. Put the ones you want to keep in $(SETTINGS)."; \
		rm -f $(OLD_OPTIONS); \
		rmdir $(SERVICE).d 2>/dev/null || true; \
	fi
	systemctl daemon-reload
	systemctl restart kbd-rgb.service
	@sleep 1
	systemctl --no-pager status kbd-rgb.service | head -12

uninstall:
	@[ "$$(id -u)" -eq 0 ] || { echo "Run this with sudo: sudo make uninstall" >&2; exit 1; }
	@# Stopping the daemon gives the keyboard back right away.
	-systemctl disable --now kbd-rgb.service 2>/dev/null
	rm -f $(SERVICE)
	rm -rf $(SERVICE).d
	systemctl daemon-reload
	-systemctl reset-failed kbd-rgb.service 2>/dev/null
	rm -rf /usr/local/lib/kbd-rgb /var/lib/kbd-rgb /run/kbd-rgb /etc/kbd-rgb
	rm -f /etc/default/kbd-rgb /usr/local/bin/kbdrgb
	rm -f /etc/udev/rules.d/99-system76-kbd-rgb.rules
	udevadm control --reload
	@# This puts the color files back to root-only now, and not at the next reboot.
	@if [ -d $(LED) ]; then \
		for file in $(LED_FILES); do \
			if [ -e $(LED)/$$file ]; then chgrp root $(LED)/$$file && chmod g-w $(LED)/$$file; fi; \
		done; \
	fi
	@echo "kbd-rgb uninstalled."
