# Build Zeke and install it for the current user, so it shows up in the
# desktop's app menu. `make install PREFIX=/usr/local` for another prefix.

APP_ID := io.github.hcuadrado.Zeke
PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin
DATADIR := $(PREFIX)/share
DESKTOP := target/$(APP_ID).desktop

.PHONY: all build desktop install uninstall validate

all: build

build:
	cargo build --release -p zeke

# The desktop entry names the installed binary by its full path: a desktop
# session's PATH may not include ~/.local/bin. Written on every run, so it
# follows PREFIX.
desktop:
	mkdir -p target
	sed 's|@BINDIR@|$(BINDIR)|g' data/$(APP_ID).desktop.in > $(DESKTOP)

install: build desktop
	install -Dm755 target/release/zeke $(DESTDIR)$(BINDIR)/zeke
	install -Dm644 $(DESKTOP) $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
	install -Dm644 data/$(APP_ID).svg $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 data/$(APP_ID).metainfo.xml $(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	-update-desktop-database -q $(DESTDIR)$(DATADIR)/applications
	-gtk4-update-icon-cache -q -t -f $(DESTDIR)$(DATADIR)/icons/hicolor

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/zeke
	rm -f $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
	rm -f $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	rm -f $(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	-update-desktop-database -q $(DESTDIR)$(DATADIR)/applications
	-gtk4-update-icon-cache -q -t -f $(DESTDIR)$(DATADIR)/icons/hicolor

validate: desktop
	desktop-file-validate $(DESKTOP)
	appstreamcli validate --no-net data/$(APP_ID).metainfo.xml
