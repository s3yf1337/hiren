PREFIX ?= /usr/local
BINDIR = $(PREFIX)/bin
SYSDIR = $(PREFIX)/lib/systemd/user

.PHONY: all build install uninstall daemon client clean

all: build

build:
	cargo build --release

install: build
	install -Dm755 target/release/hiren-daemon $(DESTDIR)$(BINDIR)/hiren-daemon
	install -Dm755 target/release/hiren-client $(DESTDIR)$(BINDIR)/hiren-client
	install -Dm644 hiren-daemon.service $(DESTDIR)$(SYSDIR)/hiren-daemon.service
	@echo "Install complete."
	@echo "Enable daemon:  systemctl --user enable --now hiren-daemon"
	@echo "Launch client:  hiren-client"

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/hiren-daemon
	rm -f $(DESTDIR)$(BINDIR)/hiren-client
	rm -f $(DESTDIR)$(SYSDIR)/hiren-daemon.service
	@echo "Uninstall complete."

clean:
	cargo clean
