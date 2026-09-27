<p align="center">
  <img src="data/icons/256/io.github.hcuadrado.Zeke.png" alt="Zeke icon" width="256" height="256">
</p>

A native Linux TIDAL player for hi-res lossless playback (up to 24-bit/192 kHz,
bit-perfect through exclusive ALSA), written in Rust with GTK4 and libadwaita.

Zeke is heavily based on [SONE](https://github.com/lullabyX/sone), a TIDAL
player built with Tauri and React: its UI follows SONE's, and the TIDAL client,
authentication, persistence and audio engine are derived from SONE's Rust
backend (GPL-3.0). `NOTICE` lists the derived files. Zeke is not affiliated
with SONE or TIDAL.

## Status

MVP: the core player is complete. What it does today:

- **Sign-in:** TIDAL login in a window of Zeke's own; the session is kept
  and refreshed.
- **Browse:** the personalized Home feed, search, favorites (tracks,
  albums, artists, playlists), and album, playlist, artist and mix pages.
- **Playback:** gapless, up to 24-bit/192 kHz, with a quality cap,
  ReplayGain, and an output menu in the player bar: the system default or
  a sound device played exclusively, bit-perfect if you like (see
  [Audio output](#audio-output)).
- **Queue:** shuffle, repeat and seek; a now-playing sheet; the queue is
  saved across restarts. Long playlists start playing before they have
  fully loaded.
- **Desktop:** a quality badge (e.g. FLAC 24/192), MPRIS media controls,
  keyboard shortcuts, light and dark styles.

### Next

- Lyrics
- Animated covers
- Video playback

## Audio output

The speaker button in the player bar chooses where Zeke plays. A choice
applies from the next track.

- **System Default** plays through the desktop's sound server (PipeWire),
  like any other app. It shares the card with other apps, follows the
  output chosen in the system settings, and PipeWire mixes and resamples
  as needed.
- **A device** (an ALSA hw device such as `hw:CARD=sofhdadsp,DEV=0`) is
  played exclusively. Zeke writes to the card directly, with no mixing,
  and with that device's Bit-perfect switch on, with no resampling or
  format conversion either. While it plays, no other app can use the
  card: on a laptop with a single card the system shows "Dummy Output".
  Speakers and the headphone jack are usually the same device.

**How Zeke takes the card:** PipeWire keeps a card open for as long as it
owns the card's `org.freedesktop.ReserveDevice1` D-Bus name. Before
opening a device, Zeke queues for that name and asks WirePlumber to
release it, and the bus hands the name to Zeke as WirePlumber lets the
card go. Zeke keeps the card until you switch back to System Default,
stop or quit.

**When the card is busy:** if another app has the card open directly (for
example `aplay` or JACK), Zeke says so, switches to System Default and
plays the track there. The saved device is also checked at startup: if
it's missing or another app holds it directly, Zeke starts on System
Default.

## Layout

| Path | Role |
|---|---|
| `crates/tidal` | TIDAL API, auth, settings, persistence |
| `crates/engine` | GStreamer/ALSA audio engine |
| `crates/player` | queue and playback state |
| `crates/cli` | headless test tool |
| `crates/plugins/api` | what a plugin is, and what the app gives it |
| `crates/plugins/cast` | the Cast plugin (feature `cast`; empty for now) |
| `app` | GTK4 + libadwaita application |

## Building (openSUSE Tumbleweed/Slowroll)

```sh
sudo zypper in gtk4-devel libadwaita-devel gstreamer-devel gstreamer-plugins-base-devel \
  gstreamer-plugins-good gstreamer-plugins-bad gstreamer-utils alsa-devel blueprint-compiler \
  webkitgtk4-devel libsoup-devel rustup
rustup default stable
rustup component add rust-analyzer rust-src # Optional but recommended
cargo run -p zeke
```

`make check` runs clippy on every feature combination, the tests, and a
check that a build without plugins pulls none in. It needs `cargo-hack`
(`cargo install cargo-hack --locked`).

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

## Writing a plugin

A plugin is compiled into Zeke. Each one is a crate under `crates/plugins`,
built only when its Cargo feature is on, and has a switch in Preferences ›
Plugins that is off by default. Zeke runs a plugin while its switch is on
and someone is signed in. It stops the plugin when the switch goes off, on
sign-out and at exit, and restarts it when the account changes. A start
always waits for the stop before it, so there is never more than one
instance. The app reaches a plugin only through the `Plugin` trait in
`crates/plugins/api`; the `cast` plugin is an example.

**1. The crate.** Create `crates/plugins/hello`, named `zeke-plugin-hello`,
and add it to the workspace `members` in the top-level `Cargo.toml`:

```toml
[package]
name = "zeke-plugin-hello"
version.workspace = true
edition.workspace = true
license.workspace = true
publish = false

[dependencies]
log = "0.4"
tokio = { version = "1", features = ["rt", "time"] }
zeke-plugin = { path = "../api" }
```

Add `gtk` and `adw` (same versions as `crates/plugins/cast`) if it has widgets.

**2. The trait.**

```rust
use std::time::Duration;

use zeke_plugin::{Plugin, PluginUi, Services};

pub struct Hello;

/// What a running Hello keeps.
pub struct Running {
    task: tokio::task::JoinHandle<()>,
}

impl Plugin for Hello {
    type Handle = Running;

    fn id(&self) -> &'static str {
        "hello"
    }

    fn name(&self) -> &'static str {
        "Hello"
    }

    async fn start(&self, _services: Services) -> Running {
        let task = tokio::spawn(async {
            loop {
                log::debug!("[hello] still here");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        Running { task }
    }

    fn ui(&self, _: &Running) -> PluginUi {
        PluginUi::default()
    }

    async fn stop(&self, running: &Running) {
        running.task.abort();
    }
}
```

- `id` keys the plugin's entry in the settings file, so never change it.
  `name` is the switch's title.
- `start` and `stop` run on the tokio runtime. `start` returns the handle
  that `ui` and `stop` get back. `stop` must end every task the plugin
  started; at exit, Zeke waits at most 2 s for the plugins to stop.
- `ui` runs on the GTK main loop, after `start`. `PluginUi::page` is shown
  in Preferences under the switch, and `PluginUi::output_section` in the
  player bar's output menu, under the local outputs. Zeke removes both when
  the plugin stops.

**3. The feature.** In `app/Cargo.toml`, add the optional dependency and a
feature named after the plugin:

```toml
[features]
hello = ["dep:zeke-plugin-hello"]

[dependencies]
zeke-plugin-hello = { path = "../crates/plugins/hello", optional = true }
```

Add it to `full` only when it is ready for users: `full` is in the default
build, and every plugin in it gets a switch.

**4. The registry.** Add it to `registry()` in `app/src/plugins/mod.rs`,
the only place in the app that names a plugin:

```rust
#[cfg(feature = "hello")]
Arc::new(zeke_plugin_hello::Hello),
```

**5. Try it.** `ZEKE_DEBUG=1 cargo run -p zeke --features hello`, sign in,
then turn it on in Preferences › Plugins. Before a merge, run `make check`,
and copy its `zeke-plugin-cast` lines in the `Makefile` for the new crate,
so a build without plugins is checked not to pull it in.

**The TIDAL session.** A plugin gets only what it lists in `needs`. With
`Need::Credential` in it, `Services::credential` is a `CredentialSource`:
- `current()` returns the access token, country and user id;
- `watch()` changes on sign-in, refresh, sign-out and account change;
- `fresh()` refreshes first when the token expires within 5 minutes; call
  it before handing the token to anything else;
- `refresh_now(&failed)` after TIDAL refuses a token: concurrent callers
  share one refresh.

`Credential`'s `Debug` hides the token, so logging one is safe; never log
`access_token` itself. The refresh token never leaves Zeke.

Not there yet: a plugin can't read or write its own settings (its entry
holds only the switch, as `enabled`), and `RemoteTargetFactory`, for
playing the queue somewhere else, has no methods yet.

## License

GPL-3.0. See `LICENSE` and `NOTICE`.
