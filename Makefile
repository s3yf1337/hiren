# hiren — build, install and headless verification.
#
# DESTDIR is a staging prefix: it qualifies file paths only. It must never be
# embedded in installed file contents (notably the systemd unit's ExecStart).

PREFIX ?= /usr/local
BINDIR = $(PREFIX)/bin
SHAREDIR = $(PREFIX)/share/hiren/themes

CARGO ?= cargo
# Respect an externally provided CARGO_TARGET_DIR (used by CI/packaging).
CARGO_TARGET_DIR ?= target
RELEASE_DIR = $(CARGO_TARGET_DIR)/release
SERVICE_UNIT = hiren-daemon.service

# Под sudo $(HOME) раскрывается в /root, поэтому для systemd --user берём
# домашний каталог реального пользователя (SUDO_USER), иначе сервис-файл
# уходит в /root/.config, а systemctl --user ищет в /home/<user>/.
ifeq ($(shell id -u),0)
  ifneq ($(SUDO_USER),)
    SYSDIR = $(shell echo ~$(SUDO_USER))/.config/systemd/user
    INSTALL_USER = $(SUDO_USER)
  else
    SYSDIR = /root/.config/systemd/user
  endif
else
  SYSDIR = $(HOME)/.config/systemd/user
endif

# Everything below is a staging path; contents are generated from the
# unqualified PREFIX/SYSDIR values above.
SERVICE_DIR = $(DESTDIR)$(SYSDIR)
THEME_DEST = $(DESTDIR)$(SHAREDIR)

.PHONY: all build install uninstall test check validate screenshot clean

all: build

build:
	$(CARGO) build --release -p hiren-daemon
	$(CARGO) build --release -p hiren-client --features layer-shell

install: build
	install -Dm755 $(RELEASE_DIR)/hiren-daemon $(DESTDIR)$(BINDIR)/hiren-daemon
	install -Dm755 $(RELEASE_DIR)/hiren-client $(DESTDIR)$(BINDIR)/hiren-client
	# ExecStart is templated with the real PREFIX bindir. DESTDIR is a staging
	# directory only: baking it in produces ExecStart=/tmp/stage/usr/local/bin/...
	# in the installed unit, which cannot work after the package is unpacked.
	install -d "$(SERVICE_DIR)"
	sed 's|@BINDIR@|$(BINDIR)|g' $(SERVICE_UNIT) > "$(SERVICE_DIR)/$(SERVICE_UNIT)"
	chmod 644 "$(SERVICE_DIR)/$(SERVICE_UNIT)"
	# Install the complete theme directory, not just theme.toml + fonts/.
	# Themes may ship assets/, extra fonts or any other runtime data.
	install -d "$(THEME_DEST)"
	@for t in hiren-client/themes/*/; do \
		[ -f "$$t/theme.toml" ] || continue; \
		name=$$(basename "$${t%/}"); \
		install -d "$(THEME_DEST)/$$name"; \
		cp -R "$${t%/}/." "$(THEME_DEST)/$$name/"; \
		echo "Installed theme $$name"; \
	done
	# Only the user service directory is chowned, and only for an un-staged
	# install as root. A staged tree belongs to the package manager; system
	# paths ($(BINDIR), $(SHAREDIR)) are never chowned to INSTALL_USER.
	@if [ -n "$(INSTALL_USER)" ] && [ -z "$(DESTDIR)" ]; then \
		chown -R "$(INSTALL_USER):" "$(SERVICE_DIR)"; \
		echo "Fixed ownership of $(SERVICE_DIR) to $(INSTALL_USER)"; \
	elif [ -n "$(INSTALL_USER)" ]; then \
		echo "Staged install (DESTDIR=$(DESTDIR)): leaving ownership to the package manager"; \
	fi
	@echo "Install complete."
	@echo "Reload the user manager:  systemctl --user daemon-reload"
	@echo "Enable daemon:            systemctl --user enable --now hiren-daemon"
	@echo "Launch client:            hiren-client"

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/hiren-daemon"
	rm -f "$(DESTDIR)$(BINDIR)/hiren-client"
	rm -f "$(SERVICE_DIR)/$(SERVICE_UNIT)"
	# Remove only themes that this repository installs; a user theme with an
	# unrelated name in $(SHAREDIR) is left untouched. User themes under
	# ~/.config/hiren/themes/ are never removed.
	@for t in hiren-client/themes/*/; do \
		[ -f "$$t/theme.toml" ] || continue; \
		name=$$(basename "$${t%/}"); \
		rm -rf "$(THEME_DEST)/$$name"; \
	done
	@rmdir "$(THEME_DEST)" 2>/dev/null || true
	@echo "Uninstall complete."
	@echo "If the user service was enabled, run:"
	@echo "  systemctl --user disable --now hiren-daemon"
	@echo "  systemctl --user daemon-reload"
	@echo "User themes in ~/.config/hiren/themes/ were left untouched."

test:
	$(CARGO) test --workspace

# Headless verification: no compositor, no window, no display server required.
check:
	$(CARGO) check --workspace --all-targets
	$(CARGO) test --workspace
	$(CARGO) run --quiet -p hiren-client -- --validate-themes
	@mkdir -p "$(CARGO_TARGET_DIR)"
	$(CARGO) run --quiet -p hiren-client -- --screenshot default --out "$(CARGO_TARGET_DIR)/hiren-smoke.png"

validate:
	$(CARGO) run --quiet -p hiren-client -- --validate-themes

screenshot:
	@mkdir -p "$(CARGO_TARGET_DIR)"
	$(CARGO) run --quiet -p hiren-client -- --screenshot default --out "$(CARGO_TARGET_DIR)/hiren-smoke.png"

clean:
	$(CARGO) clean
