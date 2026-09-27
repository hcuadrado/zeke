# Build Zeke and install it for the current user, so it shows up in the
# desktop's app menu. `make install PREFIX=/usr/local` for another prefix.

APP_ID := io.github.hcuadrado.Zeke
PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin
DATADIR := $(PREFIX)/share
DESKTOP := target/$(APP_ID).desktop
ICON_SIZES := 32 48 64 128 256 512

.PHONY: all build check desktop install uninstall validate

all: build

build:
	cargo build --release -p zeke

# Run before each merge; there is no CI. Every crate with no features,
# each feature alone and the defaults (cargo-hack: `cargo install
# cargo-hack --locked`), then the tests with everything on and the app's
# tests without plugins. Last, the
# build without plugins must not pull any plugin in; the same check on a
# build with Cast shows it can see one.
check:
	cargo hack --workspace --each-feature clippy --all-targets -- -D warnings
	cargo test --workspace --all-features
	cargo test -p zeke --no-default-features --features webview
	@if cargo tree -p zeke -e normal --no-default-features --features webview | grep -q zeke-plugin-cast; then \
		echo "check: the build without plugins depends on zeke-plugin-cast"; exit 1; \
	fi
	@cargo tree -p zeke -e normal --features cast | grep -q zeke-plugin-cast || \
		{ echo "check: cargo tree doesn't show zeke-plugin-cast even with --features cast"; exit 1; }
	@echo "check: no plugin in the build without plugins"

# The desktop entry names the installed binary by its full path: a desktop
# session's PATH may not include ~/.local/bin. Written on every run, so it
# follows PREFIX.
desktop:
	mkdir -p target
	sed 's|@BINDIR@|$(BINDIR)|g' data/$(APP_ID).desktop.in > $(DESKTOP)

install: build desktop
	install -Dm755 target/release/zeke $(DESTDIR)$(BINDIR)/zeke
	install -Dm644 $(DESKTOP) $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
	for s in $(ICON_SIZES); do \
		install -Dm644 data/icons/$$s/$(APP_ID).png $(DESTDIR)$(DATADIR)/icons/hicolor/$${s}x$$s/apps/$(APP_ID).png; \
	done
	# The icon was an SVG before; a leftover one would shadow the PNGs.
	rm -f $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	install -Dm644 data/$(APP_ID).metainfo.xml $(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	-update-desktop-database -q $(DESTDIR)$(DATADIR)/applications
	-gtk4-update-icon-cache -q -t -f $(DESTDIR)$(DATADIR)/icons/hicolor

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/zeke
	rm -f $(DESTDIR)$(DATADIR)/applications/$(APP_ID).desktop
	for s in $(ICON_SIZES); do rm -f $(DESTDIR)$(DATADIR)/icons/hicolor/$${s}x$$s/apps/$(APP_ID).png; done
	rm -f $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	rm -f $(DESTDIR)$(DATADIR)/metainfo/$(APP_ID).metainfo.xml
	-update-desktop-database -q $(DESTDIR)$(DATADIR)/applications
	-gtk4-update-icon-cache -q -t -f $(DESTDIR)$(DATADIR)/icons/hicolor

validate: desktop
	desktop-file-validate $(DESKTOP)
	appstreamcli validate --no-net data/$(APP_ID).metainfo.xml
