//! The app's side of the player: starts the engine and `crates/player` on
//! the tokio runtime, fans the player's updates out to the UI and MPRIS, and
//! persists settings. The UI talks to playback only through `PlayerCommand`s
//! sent here; nothing in this module runs engine or network calls on the GTK
//! main thread.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use gtk::glib;
use zeke_engine::audio::{self, AudioDevice, AudioPlayer};
use zeke_engine::events::{self, EngineEvent};
use zeke_engine::pipeline_probe::PipelineProbe;
use zeke_engine::{devices, SignalPath, SignalPathTracker};
use zeke_player::{
    Config, ErrorKind, PersistedQueue, PlaybackState, Player, PlayerCommand, PlayerConfig, PlayerEvent, QueueItem,
    RepeatMode, StreamFormat, TrackInfo, Transition, Update,
};
use zeke_tidal::client_lock::{self, Caller};
use zeke_tidal::{AppState, ColorScheme, Settings, TidalError};

use crate::mpris::{MprisCommand, MprisHandle};
use crate::runtime::runtime;

/// What the UI shows for a track, from TIDAL's track metadata.
#[derive(Debug, Clone)]
pub struct TrackMeta {
    pub track_id: u64,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Image id of the album cover (`resources.tidal.com`).
    pub cover: Option<String>,
    pub duration: Option<f64>,
}

impl TrackMeta {
    /// What to show until the metadata arrives.
    pub fn placeholder(track_id: u64) -> Self {
        Self {
            track_id,
            title: format!("Track {track_id}"),
            artist: String::new(),
            album: String::new(),
            cover: None,
            duration: None,
        }
    }

    /// What the page that queued the track knew.
    pub fn from_info(track_id: u64, info: &TrackInfo) -> Self {
        Self {
            track_id,
            title: info.title.clone(),
            artist: info.artists.clone(),
            album: info.album.clone(),
            cover: info.cover.clone(),
            duration: info.duration,
        }
    }

    /// The entry's own metadata, if a page queued it.
    pub fn of_item(item: &QueueItem) -> Option<Self> {
        item.info.as_deref().map(|info| Self::from_info(item.track_id, info))
    }

    fn from_json(track_id: u64, v: &serde_json::Value) -> Self {
        let title = match (v["title"].as_str(), v["version"].as_str()) {
            (Some(t), Some(ver)) if !ver.is_empty() => format!("{t} ({ver})"),
            (Some(t), _) => t.to_string(),
            (None, _) => format!("Track {track_id}"),
        };
        let artists: Vec<&str> = v["artists"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect())
            .unwrap_or_default();
        let artist = if artists.is_empty() {
            v["artist"]["name"].as_str().unwrap_or("").to_string()
        } else {
            artists.join(", ")
        };
        Self {
            track_id,
            title,
            artist,
            album: v["album"]["title"].as_str().unwrap_or("").to_string(),
            cover: v["album"]["cover"].as_str().map(str::to_string),
            duration: v["duration"].as_f64(),
        }
    }
}

/// Updates for the GTK main loop.
#[derive(Debug)]
pub enum UiEvent {
    State(PlaybackState),
    TrackStarted {
        item: QueueItem,
        duration: Option<f64>,
        format: Option<StreamFormat>,
        meta: Option<TrackMeta>,
    },
    Queue {
        items: Vec<QueueItem>,
        current: usize,
        shuffle: bool,
        repeat: RepeatMode,
    },
    /// Metadata for a track, fetched after it was first shown.
    Meta(TrackMeta),
    Position(f64),
    SignalPath(Box<SignalPath>),
    /// The player stopped on an error, or skipped a track: show a toast.
    /// Text for people (`errors`), never a raw error.
    Error(String),
    Notice(String),
    /// A stream couldn't be resolved because the login expired.
    LoginExpired,
    /// The exclusive device was picked automatically at startup.
    DeviceChosen(String),
    /// From MPRIS: bring the window up, quit, set the volume.
    Raise,
    Quit,
    Volume(f64),
}

/// The main thread's handle on the session.
pub struct Session {
    pub state: Arc<AppState>,
    commands: async_channel::Sender<PlayerCommand>,
    /// The main thread's copy of the settings; changes are written through.
    settings: RefCell<Settings>,
    mpris: MprisHandle,
    volume_save: RefCell<Option<glib::SourceId>>,
    /// Signalled once the player has stopped and released the device.
    stopped: std::sync::mpsc::Receiver<()>,
    /// One thread writes settings changes, in the order they were made.
    writer: RefCell<Option<(std::sync::mpsc::Sender<SettingsChange>, std::thread::JoinHandle<()>)>>,
}

type SettingsChange = Box<dyn FnOnce(&mut Settings) + Send>;

/// Applies changes to the settings file one at a time, in order, until the
/// sender is dropped.
fn settings_writer(state: Arc<AppState>) -> (std::sync::mpsc::Sender<SettingsChange>, std::thread::JoinHandle<()>) {
    let (tx, rx) = std::sync::mpsc::channel::<SettingsChange>();
    let handle = std::thread::Builder::new()
        .name("zeke-settings".into())
        .spawn(move || {
            for change in rx {
                if let Err(e) = state.update_settings(change) {
                    log::error!("[app] saving settings failed: {}", e.log_safe());
                }
            }
        })
        .expect("spawn the settings writer");
    (tx, handle)
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl Session {
    /// Start the engine, the player and MPRIS. Call once, on the main thread.
    pub fn start(state: Arc<AppState>, settings: Settings) -> (Self, async_channel::Receiver<UiEvent>) {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        // Commands queue here until the player is up.
        let (cmd_tx, cmd_rx) = async_channel::unbounded::<PlayerCommand>();
        let mpris = MprisHandle::start(cmd_tx.clone(), ui_tx.clone());
        let (stopped_tx, stopped) = std::sync::mpsc::channel();
        let run = run(Arc::clone(&state), settings.clone(), cmd_rx, ui_tx, mpris.clone());
        runtime().spawn(async move {
            run.await;
            let _ = stopped_tx.send(());
        });
        let session = Self {
            state: Arc::clone(&state),
            commands: cmd_tx,
            settings: RefCell::new(settings),
            mpris,
            volume_save: RefCell::new(None),
            stopped,
            writer: RefCell::new(Some(settings_writer(Arc::clone(&state)))),
        };
        (session, ui_rx)
    }

    /// Stop playback and wait (briefly) for the device to be released. For
    /// app shutdown, when the main loop has nothing left to run.
    pub fn shutdown(&self) {
        if let Some(id) = self.volume_save.take() {
            id.remove();
            let volume = self.settings.borrow().volume;
            self.change_settings(move |s| s.volume = volume);
        }
        self.commands.close();
        if self.stopped.recv_timeout(Duration::from_secs(3)).is_err() {
            log::warn!("[app] the player did not stop in time");
        }
        // Flush the pending settings writes.
        if let Some((tx, handle)) = self.writer.take() {
            drop(tx);
            let _ = handle.join();
        }
    }

    pub fn send(&self, command: PlayerCommand) {
        // Unbounded: never blocks. Err only once the player has shut down.
        let _ = self.commands.try_send(command);
    }

    pub fn settings(&self) -> Settings {
        self.settings.borrow().clone()
    }

    /// Apply `change` to the settings here and, in order, in the encrypted
    /// file (on the writer thread).
    fn change_settings(&self, change: impl Fn(&mut Settings) + Send + 'static) {
        change(&mut self.settings.borrow_mut());
        if let Some((tx, _)) = &*self.writer.borrow() {
            let _ = tx.send(Box::new(change));
        }
    }

    pub fn set_color_scheme(&self, scheme: ColorScheme) {
        if self.settings.borrow().color_scheme != scheme {
            self.change_settings(move |s| s.color_scheme = scheme);
        }
    }

    /// Live; saved once the slider rests for a moment.
    pub fn set_volume(self: &std::rc::Rc<Self>, volume: f32) {
        self.send(PlayerCommand::SetVolume(volume));
        self.mpris.send(MprisCommand::Volume(f64::from(volume)));
        self.settings.borrow_mut().volume = volume;
        if let Some(id) = self.volume_save.take() {
            id.remove();
        }
        let this = std::rc::Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(600), move || {
            if let Some(this) = this.upgrade() {
                this.volume_save.take();
                this.change_settings(move |s| s.volume = volume);
            }
        });
        self.volume_save.replace(Some(id));
    }

    pub fn set_max_quality(&self, quality: &str) {
        self.send(PlayerCommand::SetMaxQuality(quality.to_string()));
        let quality = quality.to_string();
        self.change_settings(move |s| s.max_quality = quality.clone());
    }

    /// `device: None` keeps the saved device (e.g. before the list loaded).
    pub fn set_output(&self, exclusive: bool, device: Option<String>, bit_perfect: bool) {
        let device = device.or_else(|| self.settings.borrow().exclusive_device.clone());
        self.send(PlayerCommand::SetOutput { exclusive, device: device.clone(), bit_perfect });
        self.change_settings(move |s| {
            s.exclusive_mode = exclusive;
            if device.is_some() {
                s.exclusive_device = device.clone();
            }
            s.bit_perfect = bit_perfect;
        });
    }

    pub fn set_gapless(&self, on: bool) {
        self.send(PlayerCommand::SetGapless(on));
        self.change_settings(move |s| s.gapless = on);
    }

    pub fn set_normalization(&self, on: bool) {
        self.send(PlayerCommand::SetNormalization(on));
        self.change_settings(move |s| s.volume_normalization = on);
    }

    /// Record the device picked at startup (already applied to the player).
    pub fn device_chosen(&self, device: String) {
        self.settings.borrow_mut().exclusive_device = Some(device);
    }
}

/// ALSA playback devices for the preferences, with stable ids
/// (`hw:CARD=<id>,DEV=<n>`). Blocks for up to ~2 s: run it off the main thread.
pub fn list_devices() -> Result<Vec<AudioDevice>, String> {
    let mut out: Vec<AudioDevice> = Vec::new();
    for d in audio::list_alsa_devices()? {
        let id = devices::stable_name(&d.id).unwrap_or(d.id);
        if !out.iter().any(|o| o.id == id) {
            out.push(AudioDevice { id, name: d.name });
        }
    }
    Ok(out)
}

/// `log_safe`, cut before a parse error's body or manifest, then redacted.
pub fn describe(e: &TidalError) -> String {
    let msg = e.log_safe();
    let msg = match e {
        TidalError::Parse(_) => [" - Body:", " - Manifest:"]
            .iter()
            .filter_map(|cut| msg.find(cut))
            .min()
            .map_or(msg.clone(), |at| format!("{} (response body not shown)", &msg[..at])),
        _ => msg,
    };
    zeke_tidal::redact::redact(&msg)
}

async fn run(
    state: Arc<AppState>,
    settings: Settings,
    commands: async_channel::Receiver<PlayerCommand>,
    ui: async_channel::Sender<UiEvent>,
    mpris: MprisHandle,
) {
    // The engine's constructor initialises GStreamer and starts its thread.
    let (events_tx, events_rx) = events::channel();
    let signal_path = Arc::new(SignalPathTracker::new(events_tx.clone()));
    let engine = {
        let (signal_path, proxy) = (Arc::clone(&signal_path), settings.proxy.clone());
        match tokio::task::spawn_blocking(move || Arc::new(AudioPlayer::new(events_tx, signal_path, proxy))).await {
            Ok(engine) => engine,
            Err(e) => {
                log::error!("[app] engine failed to start: {e}");
                let _ = ui.send(UiEvent::Error("The audio engine failed to start".into())).await;
                return;
            }
        }
    };
    signal_path.set_normalization_enabled(settings.volume_normalization);
    let probe = Arc::new(PipelineProbe::new(Arc::clone(&signal_path), Arc::clone(&engine)));

    let queue_file = state.settings_path.parent().map(PersistedQueue::path);
    let player = Player::spawn(
        Arc::clone(&state),
        engine,
        events_rx,
        PlayerConfig {
            core: Config {
                gapless: settings.gapless,
                normalization: settings.volume_normalization,
                max_quality: settings.max_quality.clone(),
                bit_perfect: settings.exclusive_mode && settings.bit_perfect,
                ..Config::default()
            },
            seed: None,
            queue_file: queue_file.clone(),
        },
    );

    let device = match settings.exclusive_device.clone().filter(|d| !d.is_empty()) {
        None => pick_device(&state, &ui).await,
        // Names saved by early builds (`hw:0,0`) move to the stable form.
        Some(saved) => match devices::stable_name(&saved).filter(|stable| *stable != saved) {
            Some(stable) => {
                log::info!("[app] exclusive device {saved} is now saved as {stable}");
                save_device(&state, &ui, stable.clone()).await;
                Some(stable)
            }
            None => Some(saved),
        },
    };
    for command in [
        PlayerCommand::SetOutput { exclusive: settings.exclusive_mode, device, bit_perfect: settings.bit_perfect },
        PlayerCommand::SetGapless(settings.gapless),
        PlayerCommand::SetVolume(settings.volume),
    ] {
        let _ = player.commands.send(command).await;
    }
    mpris.send(MprisCommand::Volume(f64::from(settings.volume)));
    // The last session's queue, paused where it was. Its
    // entries carry their metadata, so this asks TIDAL for nothing.
    if let Some(path) = queue_file.filter(|_| settings.auth_tokens.is_some())
        && let Some(saved) = load_queue(path).await
    {
        let _ = player.commands.send(PlayerCommand::Restore(Box::new(saved))).await;
    }

    let hub = tokio::spawn(hub(state, player.updates.clone(), ui, mpris, probe));
    // UI and MPRIS → player, until the app shuts down.
    while let Ok(c) = commands.recv().await {
        if player.commands.send(c).await.is_err() {
            break;
        }
    }
    player.shutdown().await;
    let _ = hub.await;
}

/// The saved queue, if there is one that reads.
async fn load_queue(path: std::path::PathBuf) -> Option<PersistedQueue> {
    let shown = path.display().to_string();
    match tokio::task::spawn_blocking(move || PersistedQueue::load(&path)).await {
        Ok(Ok(Some(q))) => {
            let bare = q.tracks.iter().filter(|t| t.info.is_none()).count();
            log::info!(
                "[app] restoring the saved queue: {} tracks ({bare} without metadata), entry {} at {:.1} s",
                q.tracks.len(),
                q.active_index + 1,
                q.position_ms as f64 / 1000.0
            );
            Some(q)
        }
        Ok(Ok(None)) => None,
        Ok(Err(e)) => {
            log::warn!("[app] the saved queue ({shown}) can't be read, starting empty: {e}");
            None
        }
        Err(_) => None,
    }
}

/// Pick the first analog device, save it, and tell the UI.
async fn pick_device(state: &Arc<AppState>, ui: &async_channel::Sender<UiEvent>) -> Option<String> {
    let picked = tokio::task::spawn_blocking(|| {
        let list = audio::list_alsa_devices().inspect_err(|e| log::warn!("[app] listing devices: {e}")).ok()?;
        devices::pick_default_device(&list)
    })
    .await
    .ok()
    .flatten();
    let Some(device) = picked else {
        log::warn!("[app] no analog playback device found for exclusive mode");
        return None;
    };
    log::info!("[app] exclusive device not set; picked {device}");
    save_device(state, ui, device.clone()).await;
    Some(device)
}

/// Save a device chosen at startup and tell the UI's copy of the settings.
/// Runs before the UI can change the output (it has no device list yet).
async fn save_device(state: &Arc<AppState>, ui: &async_channel::Sender<UiEvent>, device: String) {
    let (state, saved) = (Arc::clone(state), device.clone());
    let result = tokio::task::spawn_blocking(move || state.update_settings(|s| s.exclusive_device = Some(saved))).await;
    if let Ok(Err(e)) = result {
        log::error!("[app] saving the exclusive device failed: {}", e.log_safe());
    }
    let _ = ui.send(UiEvent::DeviceChosen(device)).await;
}

/// Metadata requests beyond the current track: this many upcoming tracks
/// and this many of the history, nearest first.
const META_AHEAD: usize = 50;
const META_BEHIND: usize = 10;
/// A track whose metadata failed isn't asked for again for this long.
const META_RETRY: Duration = Duration::from_secs(60);

/// Fan the player's updates out to the UI and MPRIS, adding track metadata.
async fn hub(
    state: Arc<AppState>,
    updates: async_channel::Receiver<Update>,
    ui: async_channel::Sender<UiEvent>,
    mpris: MprisHandle,
    probe: Arc<PipelineProbe>,
) {
    let mut cache: HashMap<u64, TrackMeta> = HashMap::new();
    let mut failed: HashMap<u64, std::time::Instant> = HashMap::new();
    // The current track and its stream length; the play order and its index.
    let mut current: Option<(u64, Option<f64>)> = None;
    let mut order: (Vec<u64>, usize) = (Vec::new(), 0);
    let mut playing = false;
    let mut probe_tick = tokio::time::interval(Duration::from_secs(5));
    probe_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // One fetcher works through the latest wish list, one request at a
    // time, and hands results back here.
    let (wants_tx, wants_rx) = tokio::sync::watch::channel(Vec::<u64>::new());
    let (meta_tx, mut meta_rx) = tokio::sync::mpsc::unbounded_channel();
    let fetcher = tokio::spawn(fetch_meta(Arc::clone(&state), wants_rx, meta_tx));

    let refresh_probe = |probe: &Arc<PipelineProbe>, delay: Duration| {
        let probe = Arc::clone(probe);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tokio::task::spawn_blocking(move || probe.refresh()).await;
        });
    };

    loop {
        let update = tokio::select! {
            u = updates.recv() => match u {
                Ok(u) => u,
                Err(_) => break,
            },
            Some((id, result)) = meta_rx.recv() => {
                match result {
                    Ok(meta) => {
                        if let Some((_, length)) = current.filter(|(now, _)| *now == id) {
                            mpris.send(MprisCommand::metadata(&meta, length));
                        }
                        cache.insert(id, meta.clone());
                        failed.remove(&id);
                        let _ = ui.send(UiEvent::Meta(meta)).await;
                    }
                    Err(()) => {
                        failed.insert(id, std::time::Instant::now());
                    }
                }
                continue;
            }
            _ = probe_tick.tick() => {
                if playing {
                    refresh_probe(&probe, Duration::ZERO);
                }
                continue;
            }
        };
        match update {
            Update::Player(PlayerEvent::State(s)) => {
                playing = s == PlaybackState::Playing;
                mpris.send(MprisCommand::Status(s));
                let _ = ui.send(UiEvent::State(s)).await;
            }
            Update::Player(PlayerEvent::TrackStarted { item, duration, format, index, len, via, .. }) => {
                log::info!(
                    "[app] now playing {} ({}/{len}) via {via:?} ({})",
                    item.track_id,
                    index + 1,
                    writer_counters()
                );
                current = Some((item.track_id, duration));
                // A restored entry arrives before its QueueChanged: cache the
                // metadata it carries now, or the fetcher would ask for it.
                if let (std::collections::hash_map::Entry::Vacant(slot), Some(meta)) =
                    (cache.entry(item.track_id), TrackMeta::of_item(&item))
                {
                    slot.insert(meta);
                }
                let meta = cache.get(&item.track_id).cloned();
                // Something right away, so widgets never keep the last
                // track's title (or its trackid for SetPosition).
                let shown = meta.clone().unwrap_or_else(|| TrackMeta::placeholder(item.track_id));
                mpris.send(MprisCommand::metadata(&shown, duration));
                let _ = ui.send(UiEvent::TrackStarted { item, duration, format, meta }).await;
                wants_tx.send_replace(wish_list(&order, current, &cache, &failed));
                // The writer has (re)negotiated by then; read the DAC. A
                // restored entry isn't loaded: nothing to read.
                if via != Transition::Restore {
                    refresh_probe(&probe, Duration::from_millis(1500));
                }
            }
            Update::Player(PlayerEvent::QueueChanged { items, current: at, shuffle, repeat }) => {
                mpris.send(MprisCommand::Modes { shuffle, repeat });
                // Tracks queued from a page carry their metadata; only
                // ID-only ones are left for the fetcher.
                for item in items.iter().filter(|i| i.info.is_some()) {
                    if let std::collections::hash_map::Entry::Vacant(slot) = cache.entry(item.track_id) {
                        slot.insert(TrackMeta::of_item(item).expect("has info"));
                    }
                }
                order = (items.iter().map(|i| i.track_id).collect(), at);
                wants_tx.send_replace(wish_list(&order, current, &cache, &failed));
                let _ = ui.send(UiEvent::Queue { items, current: at, shuffle, repeat }).await;
            }
            Update::Player(PlayerEvent::Position(p)) => {
                mpris.send(MprisCommand::Position(p));
                let _ = ui.send(UiEvent::Position(p)).await;
            }
            Update::Player(PlayerEvent::Error { kind, message }) => {
                log::error!("[app] player stopped ({kind:?}): {}", zeke_tidal::redact::redact(&message));
                let event = match kind {
                    ErrorKind::LoginExpired => UiEvent::LoginExpired,
                    _ => UiEvent::Error(crate::errors::player(kind, &message)),
                };
                let _ = ui.send(event).await;
            }
            Update::Player(PlayerEvent::Skipped { item, error }) => {
                log::warn!("[app] skipped unplayable track {}: {error}", item.track_id);
                let title = cache
                    .get(&item.track_id)
                    .map(|m| m.title.clone())
                    .or_else(|| item.info.as_ref().map(|i| i.title.clone()));
                let text = match title {
                    Some(t) => format!("Skipped “{t}”: TIDAL can’t play it"),
                    None => "Skipped a track TIDAL can’t play".to_string(),
                };
                let _ = ui.send(UiEvent::Notice(text)).await;
            }
            Update::Player(PlayerEvent::QueueEnded) => {
                log::info!("[app] end of queue ({})", writer_counters());
            }
            Update::Player(PlayerEvent::PrefetchStarted { item, remaining }) => {
                log::info!(
                    "[app] resolving next track {} ({:?} s before the end; {})",
                    item.track_id,
                    remaining.map(|r| r.round()),
                    writer_counters()
                );
            }
            Update::Player(PlayerEvent::PrefetchArmed { item, summary, .. }) => {
                log::info!("[app] next track {} armed ({summary})", item.track_id);
            }
            Update::Player(PlayerEvent::PrefetchNotArmed { item, reason }) => {
                log::info!("[app] next track {} not armed: {reason}", item.track_id);
            }
            Update::Player(PlayerEvent::PrefetchCleared { item, reason }) => {
                log::info!("[app] next-track slot for {} cleared: {reason}", item.track_id);
            }
            Update::Player(PlayerEvent::PrefetchFailed { item, error }) => {
                log::warn!("[app] could not resolve next track {}: {error}", item.track_id);
            }
            Update::Engine(EngineEvent::SignalPathChanged(p)) => {
                let _ = ui.send(UiEvent::SignalPath(p)).await;
            }
            Update::Engine(other) => log::debug!("[app] engine: {other:?}"),
        }
    }
    fetcher.abort();
}

/// The tracks whose metadata to fetch, most wanted first: the current one,
/// then upcoming, then recent history. Skips cached and recently failed ones.
fn wish_list(
    (order, at): &(Vec<u64>, usize),
    current: Option<(u64, Option<f64>)>,
    cache: &HashMap<u64, TrackMeta>,
    failed: &HashMap<u64, std::time::Instant>,
) -> Vec<u64> {
    let ahead = order.iter().skip(at + 1).take(META_AHEAD);
    let behind = order[..(*at).min(order.len())].iter().rev().take(META_BEHIND);
    let mut out: Vec<u64> = Vec::new();
    for id in current.map(|(id, _)| id).iter().chain(order.get(*at)).chain(ahead).chain(behind) {
        let wanted = !cache.contains_key(id) && failed.get(id).is_none_or(|t| t.elapsed() > META_RETRY);
        if wanted && !out.contains(id) {
            out.push(*id);
        }
    }
    out
}

/// The ALSA writer's silence writes and xruns so far, for the log (a
/// gapless boundary must write no silence).
fn writer_counters() -> String {
    let (silence, xruns) = audio::writer_counters();
    format!("writer: silence writes {silence}, xruns {xruns}")
}

/// Fetch metadata for the head of the latest wish list, one request at a
/// time (the TIDAL client is shared with the player's stream resolution).
async fn fetch_meta(
    state: Arc<AppState>,
    mut wants: tokio::sync::watch::Receiver<Vec<u64>>,
    results: tokio::sync::mpsc::UnboundedSender<(u64, Result<TrackMeta, ()>)>,
) {
    let mut done: HashSet<u64> = HashSet::new();
    loop {
        let next = wants.borrow_and_update().iter().copied().find(|id| !done.contains(id));
        let Some(id) = next else {
            done.clear();
            if wants.changed().await.is_err() {
                return;
            }
            continue;
        };
        done.insert(id);
        let result = client_lock::lock(&state.tidal_client, Caller::Other("track metadata")).await.get_track(id).await;
        let result = match result {
            Ok(v) => Ok(TrackMeta::from_json(id, &v)),
            Err(e) => {
                log::warn!("[app] no metadata for track {id}: {}", describe(&e));
                Err(())
            }
        };
        if results.send((id, result)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_from_tidal_json() {
        let v = serde_json::json!({
            "title": "One More Time",
            "version": "Radio Edit",
            "duration": 320,
            "artists": [{"name": "Daft Punk"}, {"name": "Romanthony"}],
            "artist": {"name": "Daft Punk"},
            "album": {"title": "Discovery", "cover": "ab-cd-ef"},
        });
        let m = TrackMeta::from_json(1550546, &v);
        assert_eq!(m.title, "One More Time (Radio Edit)");
        assert_eq!(m.artist, "Daft Punk, Romanthony");
        assert_eq!(m.album, "Discovery");
        assert_eq!(m.duration, Some(320.0));
        assert_eq!(m.cover.as_deref(), Some("ab-cd-ef"));
        let bare = TrackMeta::from_json(7, &serde_json::json!({"artist": {"name": "X"}}));
        assert_eq!((bare.title.as_str(), bare.artist.as_str()), ("Track 7", "X"));
    }

    #[test]
    fn metadata_is_fetched_current_first_then_ahead_then_behind() {
        let order = ((1..=200).collect::<Vec<u64>>(), 100); // current: 101
        let mut cache = HashMap::new();
        cache.insert(102, TrackMeta::placeholder(102));
        let mut failed = HashMap::new();
        failed.insert(103, std::time::Instant::now());
        let wants = wish_list(&order, Some((101, None)), &cache, &failed);
        assert_eq!(&wants[..3], &[101, 104, 105], "cached and just-failed ids are skipped");
        assert_eq!(wants.len(), 1 + (META_AHEAD - 2) + META_BEHIND);
        assert_eq!(wants[wants.len() - META_BEHIND], 100, "history nearest first");
    }
}
