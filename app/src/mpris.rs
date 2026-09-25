//! MPRIS (media keys, `playerctl`, desktop media widgets).
//!
//! `mpris-server`'s `Player` is `!Send`, so it lives on its own thread with a
//! current-thread tokio runtime and a `LocalSet`, not on the glib
//! loop. It is connected over channels: controls go straight to the player
//! as `PlayerCommand`s (window requests go to the UI), and state comes in as
//! `MprisCommand`s from the session's hub.

use std::rc::Rc;
use std::time::Instant;

use mpris_server::{LoopStatus, Metadata, PlaybackStatus, Player, Time, TrackId};
use tokio::sync::mpsc;
use zeke_player::{PlaybackState, PlayerCommand, RepeatMode};

use crate::covers;
use crate::session::{TrackMeta, UiEvent};

/// The D-Bus name is `org.mpris.MediaPlayer2.<this>` (`playerctl -p` takes
/// the suffix), and the desktop entry is the app ID.
const BUS_NAME: &str = "io.github.hcuadrado.Zeke";
const DESKTOP_ENTRY: &str = "io.github.hcuadrado.Zeke";

#[derive(Debug)]
pub enum MprisCommand {
    Metadata {
        track_id: u64,
        title: String,
        artist: String,
        album: String,
        art_url: Option<String>,
        length: Option<f64>,
    },
    Status(PlaybackState),
    /// The player's position, polled; a jump is reported as `Seeked`.
    Position(f64),
    Volume(f64),
    Modes { shuffle: bool, repeat: RepeatMode },
}

impl MprisCommand {
    /// `length`: the stream's exact length when known, else the metadata's.
    pub fn metadata(meta: &TrackMeta, length: Option<f64>) -> Self {
        Self::Metadata {
            track_id: meta.track_id,
            title: meta.title.clone(),
            artist: meta.artist.clone(),
            album: meta.album.clone(),
            art_url: meta.cover.as_deref().map(|c| covers::url(c, covers::Kind::Album, covers::SHEET)),
            length: length.or(meta.duration),
        }
    }
}

#[derive(Clone)]
pub struct MprisHandle {
    tx: mpsc::UnboundedSender<MprisCommand>,
}

impl MprisHandle {
    pub fn start(
        player_commands: async_channel::Sender<PlayerCommand>,
        ui: async_channel::Sender<UiEvent>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("zeke-mpris".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        log::error!("[mpris] no runtime: {e}");
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, serve(rx, player_commands, ui));
            })
            .expect("spawn the MPRIS thread");
        Self { tx }
    }

    pub fn send(&self, command: MprisCommand) {
        self.tx.send(command).ok();
    }
}

fn secs(t: Time) -> f64 {
    t.as_micros() as f64 / 1_000_000.0
}

fn time(secs: f64) -> Time {
    Time::from_micros((secs * 1_000_000.0) as i64)
}

async fn serve(
    mut rx: mpsc::UnboundedReceiver<MprisCommand>,
    commands: async_channel::Sender<PlayerCommand>,
    ui: async_channel::Sender<UiEvent>,
) {
    let player = match Player::builder(BUS_NAME)
        .identity("Zeke")
        .desktop_entry(DESKTOP_ENTRY)
        .can_play(true)
        .can_pause(true)
        .can_go_next(true)
        .can_go_previous(true)
        .can_seek(true)
        .can_control(true)
        .can_quit(true)
        .can_raise(true)
        .can_set_fullscreen(false)
        .has_track_list(false)
        .supported_uri_schemes(Vec::<String>::new())
        .supported_mime_types(Vec::<String>::new())
        .rate(1.0)
        .minimum_rate(1.0)
        .maximum_rate(1.0)
        .shuffle(false)
        .loop_status(LoopStatus::None)
        .playback_status(PlaybackStatus::Stopped)
        .build()
        .await
    {
        Ok(p) => Rc::new(p),
        Err(e) => {
            log::error!("[mpris] could not start the D-Bus server: {e}");
            return;
        }
    };

    // Controls → player (and window requests → the UI). The channels are
    // unbounded, so `try_send` only fails once the app is shutting down.
    let send = |c: PlayerCommand| {
        let commands = commands.clone();
        move || {
            let _ = commands.try_send(c.clone());
        }
    };
    let f = send(PlayerCommand::TogglePause);
    player.connect_play_pause(move |_| f());
    let f = send(PlayerCommand::Resume);
    player.connect_play(move |_| f());
    let f = send(PlayerCommand::Pause);
    player.connect_pause(move |_| f());
    let f = send(PlayerCommand::Stop);
    player.connect_stop(move |_| f());
    let f = send(PlayerCommand::Next);
    player.connect_next(move |_| f());
    let f = send(PlayerCommand::Previous);
    player.connect_previous(move |_| f());
    let c = commands.clone();
    player.connect_seek(move |_, offset| {
        let _ = c.try_send(PlayerCommand::SeekBy(secs(offset)));
    });
    let c = commands.clone();
    player.connect_set_position(move |p, track_id, position| {
        // The spec: ignore a SetPosition for a track that is no longer current.
        if p.metadata().trackid().is_some_and(|t| &t == track_id) {
            let _ = c.try_send(PlayerCommand::Seek(secs(position)));
        }
    });
    let c = commands.clone();
    player.connect_set_shuffle(move |_, on| {
        let _ = c.try_send(PlayerCommand::SetShuffle(on));
    });
    let c = commands.clone();
    player.connect_set_loop_status(move |_, status| {
        let mode = match status {
            LoopStatus::None => RepeatMode::Off,
            LoopStatus::Playlist => RepeatMode::All,
            LoopStatus::Track => RepeatMode::One,
        };
        let _ = c.try_send(PlayerCommand::SetRepeat(mode));
    });
    let u = ui.clone();
    player.connect_set_volume(move |_, volume| {
        let _ = u.try_send(UiEvent::Volume(volume.clamp(0.0, 1.0)));
    });
    let u = ui.clone();
    player.connect_raise(move |_| {
        let _ = u.try_send(UiEvent::Raise);
    });
    let u = ui.clone();
    player.connect_quit(move |_| {
        let _ = u.try_send(UiEvent::Quit);
    });
    player.connect_set_rate(|_, _| {}); // fixed 1.0×: MinimumRate = MaximumRate = 1.0
    player.connect_open_uri(|_, _| {});

    tokio::task::spawn_local(player.run());
    log::info!("[mpris] D-Bus server started as org.mpris.MediaPlayer2.{BUS_NAME}");

    // Where the last position report left the track, to tell a seek from
    // playback moving on.
    let mut last: Option<(f64, Instant)> = None;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            MprisCommand::Metadata { track_id, title, artist, album, art_url, length } => {
                let mut m = Metadata::new();
                if let Ok(id) = TrackId::try_from(format!("/io/github/hcuadrado/Zeke/Track/{track_id}")) {
                    m.set_trackid(Some(id));
                }
                m.set_title(Some(title));
                m.set_artist(Some([artist]));
                m.set_album(Some(album));
                m.set_art_url(art_url);
                m.set_length(length.map(time));
                player.set_metadata(m).await.ok();
                player.set_position(Time::ZERO);
                last = Some((0.0, Instant::now()));
            }
            MprisCommand::Status(state) => {
                let status = match state {
                    PlaybackState::Playing | PlaybackState::Loading => PlaybackStatus::Playing,
                    PlaybackState::Paused | PlaybackState::Restored => PlaybackStatus::Paused,
                    PlaybackState::Stopped => PlaybackStatus::Stopped,
                };
                if status == PlaybackStatus::Stopped {
                    player.set_position(Time::ZERO);
                }
                player.set_playback_status(status).await.ok();
            }
            MprisCommand::Position(p) => {
                let playing = player.playback_status() == PlaybackStatus::Playing;
                let expected = last.map(|(at, when)| {
                    if playing {
                        at + when.elapsed().as_secs_f64()
                    } else {
                        at
                    }
                });
                player.set_position(time(p));
                if expected.is_some_and(|e| (p - e).abs() > 1.5) {
                    player.seeked(time(p)).await.ok();
                }
                last = Some((p, Instant::now()));
            }
            MprisCommand::Volume(v) => {
                player.set_volume(v).await.ok();
            }
            MprisCommand::Modes { shuffle, repeat } => {
                let status = match repeat {
                    RepeatMode::Off => LoopStatus::None,
                    RepeatMode::All => LoopStatus::Playlist,
                    RepeatMode::One => LoopStatus::Track,
                };
                if player.shuffle() != shuffle {
                    player.set_shuffle(shuffle).await.ok();
                }
                if player.loop_status() != status {
                    player.set_loop_status(status).await.ok();
                }
            }
        }
    }
}
