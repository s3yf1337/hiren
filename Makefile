PREFIX ?= /usr/local
BINDIR = $(PREFIX)/bin
SHAREDIR = $(PREFIX)/share/hiren/themes

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

.PHONY: all build install uninstall daemon client clean

all: build

build:
	cargo build --release

install: build
	install -Dm755 target/release/hiren-daemon $(DESTDIR)$(BINDIR)/hiren-daemon
	install -Dm755 target/release/hiren-client $(DESTDIR)$(BINDIR)/hiren-client
	# ExecStart is templated: the service must point at the PREFIX it was
	# installed with, not a hardcoded /usr/local (breaks user-local installs).
	@mkdir -p $(DESTDIR)$(SYSDIR)
	@sed "s|@BINDIR@|$(DESTDIR)$(BINDIR)|g" hiren-daemon.service > $(DESTDIR)$(SYSDIR)/hiren-daemon.service
	@mkdir -p $(DESTDIR)$(SHAREDIR)
	@for t in hiren-client/themes/*; do \
		if [ -d "$$t" ]; then \
			name=$$(basename "$$t"); \
			mkdir -p $(DESTDIR)$(SHAREDIR)/$$name; \
			install -Dm644 $$t/theme.toml $(DESTDIR)$(SHAREDIR)/$$name/theme.toml; \
			echo "Installed theme $$name"; \
		fi; \
	done
	@if [ -n "$(INSTALL_USER)" ]; then \
		chown -R $(INSTALL_USER): $(SYSDIR); \
		echo "Fixed ownership of $(SYSDIR) to $(INSTALL_USER)"; \
	fi
	@echo "Install complete."
	@echo "Enable daemon:  systemctl --user enable --now hiren-daemon"
	@echo "Launch client:  hiren-client"

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/hiren-daemon
	rm -f $(DESTDIR)$(BINDIR)/hiren-client
	rm -f $(DESTDIR)$(SYSDIR)/hiren-daemon.service
	rm -rf $(DESTDIR)$(SHAREDIR)
	@echo "Uninstall complete."

clean:
	cargo clean
