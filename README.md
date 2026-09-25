# Zeke

A native Linux TIDAL player for hi-res lossless playback (up to 24-bit/192 kHz,
bit-perfect through exclusive ALSA), written in Rust with GTK4 and libadwaita.

Zeke is heavily based on [SONE](https://github.com/lullabyX/sone), a TIDAL
player built with Tauri and React: its UI follows SONE's, and the TIDAL client,
authentication, persistence and audio engine are derived from SONE's Rust
backend (GPL-3.0). `NOTICE` lists the derived files. Zeke is not affiliated
with SONE or TIDAL.

**Status:** MVP. PKCE login, Home, search, favorites, album/playlist/artist/mix
pages, gapless hi-res playback (optional exclusive ALSA), a saved queue,
MPRIS and keyboard shortcuts.

## Layout

| Path | Role |
|---|---|
| `crates/tidal` | TIDAL API, auth, settings, persistence |
| `crates/engine` | GStreamer/ALSA audio engine |
| `crates/player` | queue and playback state |
| `crates/cli` | headless test tool |
| `app` | GTK4 + libadwaita application |

## Building (openSUSE)

```sh
sudo zypper in gtk4-devel libadwaita-devel gstreamer-devel gstreamer-plugins-base-devel \
  gstreamer-plugins-good gstreamer-plugins-bad gstreamer-utils alsa-devel blueprint-compiler \
  webkitgtk4-devel libsoup-devel
cargo run -p zeke
```

## Installing (current user)

```sh
make install     # release build; binary, desktop entry, icon and metainfo under ~/.local
make uninstall
make validate    # desktop-file-validate and appstreamcli validate --no-net
```

Zeke then shows up in the desktop's app menu. `PREFIX=/usr/local` installs elsewhere.

## Debugging

- `ZEKE_DEBUG=1`: debug logging (every TIDAL request's turn at the client lock, queue saves).
- `ZEKE_STALLS=1`: log each main-loop stall over 16 ms.

Playback will need GStreamer's `dashdemux` (from gstreamer-plugins-bad), not `dashdemux2`.

## License

GPL-3.0. See `LICENSE` and `NOTICE`.
