CARGO ?= cargo
PREFIX ?= /usr/local
DESTDIR ?=
BINDIR := $(DESTDIR)$(PREFIX)/bin
DATADIR := $(DESTDIR)$(PREFIX)/share
SYSTEMD_USER_DIR := $(DESTDIR)$(PREFIX)/lib/systemd/user
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
ARCH := $(shell dpkg --print-architecture 2>/dev/null || echo amd64)
DEB_ROOT := target/debian/animatrix_$(VERSION)_$(ARCH)
DEB_FILE := target/debian/animatrix_$(VERSION)_$(ARCH).deb
CONFIG_FILE := $(or $(XDG_CONFIG_HOME),$(HOME)/.config)/animatrix/config.json

# Copies data/gifs/**/*.gif below $(1)/gifs. find passes names with spaces
# and quotes as plain arguments, so they are never re-parsed by a shell.
define install_gifs
	cd data && find gifs -type f -iname '*.gif' -exec install -Dm644 {} "$(abspath $(1))/{}" \;
endef

.PHONY: all release run test test-core install uninstall deb upgrade clean

all: release

release:
	$(CARGO) build --release

run:
	$(CARGO) run

test:
	$(CARGO) test --all-features

test-core:
	$(CARGO) test --no-default-features

install: release
	install -Dm755 target/release/animatrix $(BINDIR)/animatrix
	install -Dm644 data/net._512mb.Animatrix.desktop $(DATADIR)/applications/net._512mb.Animatrix.desktop
	install -Dm644 data/net._512mb.Animatrix.png $(DATADIR)/icons/hicolor/64x64/apps/net._512mb.Animatrix.png
	install -Dm644 data/animatrix.service $(SYSTEMD_USER_DIR)/animatrix.service
	$(call install_gifs,$(DATADIR)/animatrix)

uninstall:
	rm -f $(BINDIR)/animatrix
	rm -f $(DATADIR)/applications/net._512mb.Animatrix.desktop
	rm -f $(DATADIR)/icons/hicolor/64x64/apps/net._512mb.Animatrix.png
	rm -f $(SYSTEMD_USER_DIR)/animatrix.service
	rm -rf $(DATADIR)/animatrix/gifs

deb: release
	rm -rf $(DEB_ROOT)
	install -Dm755 target/release/animatrix $(DEB_ROOT)/usr/bin/animatrix
	install -Dm644 data/net._512mb.Animatrix.desktop $(DEB_ROOT)/usr/share/applications/net._512mb.Animatrix.desktop
	install -Dm644 data/net._512mb.Animatrix.png $(DEB_ROOT)/usr/share/icons/hicolor/64x64/apps/net._512mb.Animatrix.png
	install -Dm644 data/animatrix.service $(DEB_ROOT)/usr/lib/systemd/user/animatrix.service
	$(call install_gifs,$(DEB_ROOT)/usr/share/animatrix)
	install -d $(DEB_ROOT)/DEBIAN
	printf '%s\n' \
		'Package: animatrix' \
		'Version: $(VERSION)' \
		'Section: utils' \
		'Priority: optional' \
		'Architecture: $(ARCH)' \
		'Depends: asusctl, fonts-dejavu-core, libgtk-4-1' \
		'Maintainer: crt <crt@512mb.net>' \
		'Description: GTK4 controller for the ASUS AniMe Matrix' \
		' Build profiles from clock, text, GIF, flashlight, and battery elements' \
		' and manage AniMe lifecycle behavior.' \
		> $(DEB_ROOT)/DEBIAN/control
	dpkg-deb --root-owner-group --build $(DEB_ROOT) $(DEB_FILE)
	@echo "Built $(DEB_FILE)"

# Debian/Ubuntu: back up the config, build and install the package, then
# restart the user service. Run as your user; apt is invoked through sudo.
upgrade:
	@if [ ! -f /etc/debian_version ] || ! command -v apt >/dev/null; then \
		echo "make upgrade needs a Debian-based system with apt; use 'sudo make install' instead" >&2; \
		exit 1; \
	fi
	@if [ -f "$(CONFIG_FILE)" ]; then \
		cp "$(CONFIG_FILE)" "$(CONFIG_FILE).bak" && echo "Backed up config to $(CONFIG_FILE).bak"; \
	fi
	$(MAKE) deb
	sudo apt install --reinstall ./$(DEB_FILE)
	systemctl --user daemon-reload
	@# A copy started by hand would keep the display and absorb the new one.
	-pkill -x animatrix
	systemctl --user enable animatrix.service
	systemctl --user restart animatrix.service
	@echo "Animatrix $(VERSION) installed and running"

clean:
	$(CARGO) clean
