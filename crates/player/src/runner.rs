//! The player's I/O: runs `Core` on a tokio task, carries out its effects
//! against the engine and TIDAL, and feeds the results back.
//!
//! Engine calls block (each waits for the audio thread's reply, and
//! `play_url` tears a pipeline down first), so they run in order on one
//! dedicated thread. TIDAL requests run as tokio tasks. Position is polled
//! every 250 ms for the just-in-time prefetch.
//!
//! With a queue file, the queue is saved when it or the current track
//! changes, on pause and stop, every `SAVE_EVERY` while playing, and when
//! the player shuts down. Saves are written on their own thread.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use zeke_engine::audio::AudioPlayer;
use zeke_engine::events::EngineEvent;
use zeke_tidal::client_lock::{self, Caller};
use zeke_tidal::commands::playback::resolve_play_uri;
use zeke_tidal::{AppState, TidalError};

use crate::core::{
    Config, Core, Effect, ErrorKind, Input, PlaybackState, PlayerCommand, PlayerEvent, ResolveError, Resolved,
    StreamFormat,
};
use crate::persist::{Save, Writer};
use crate::queue::QueueItem;

const TICK: Duration = Duration::from_millis(250);
/// How often the position is saved while playing.
const SAVE_EVERY: Duration = Duration::from_secs(10);

/// What the player publishes: its own events, and the engine events it
/// doesn't consume (signal path, resampling, bit-depth changes) for the UI.
#[derive(Debug, Clone)]
pub enum Update {
    Player(PlayerEvent),
    Engine(EngineEvent),
}

pub struct PlayerConfig {
    pub core: Config,
    /// Shuffle seed; `None` seeds from the clock.
    pub seed: Option<u64>,
    /// Where to save the queue (`PersistedQueue::path`); `None` saves nothing.
    pub queue_file: Option<PathBuf>,
}

/// The running player. Commands in, updates out; dropping `commands` (or
/// calling `shutdown`) stops playback and ends the task.
pub struct Player {
    pub commands: async_channel::Sender<PlayerCommand>,
    pub updates: async_channel::Receiver<Update>,
    task: tokio::task::JoinHandle<()>,
}

impl Player {
    /// Must be called inside a tokio runtime. Configure the engine first, or
    /// send `SetOutput`, `SetGapless` and `SetVolume` before playing.
    pub fn spawn(
        state: Arc<AppState>,
        engine: Arc<AudioPlayer>,
        engine_events: async_channel::Receiver<EngineEvent>,
        config: PlayerConfig,
    ) -> Self {
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (upd_tx, upd_rx) = async_channel::unbounded();
        let task = tokio::spawn(run(state, engine, engine_events, config, cmd_rx, upd_tx));
        Self { commands: cmd_tx, updates: upd_rx, task }
    }

    /// Stop playback and wait for the engine to be released.
    pub async fn shutdown(self) {
        self.commands.close();
        let _ = self.task.await;
    }
}

type EngineJob = Box<dyn FnOnce(&AudioPlayer) -> Option<Input> + Send>;

async fn run(
    state: Arc<AppState>,
    engine: Arc<AudioPlayer>,
    engine_events: async_channel::Receiver<EngineEvent>,
    config: PlayerConfig,
    commands: async_channel::Receiver<PlayerCommand>,
    updates: async_channel::Sender<Update>,
) {
    let seed = config.seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    });
    let mut core = Core::new(config.core, seed);
    let started = Instant::now();
    let mut saver = config.queue_file.map(Saver::new);
    let (input_tx, input_rx) = async_channel::unbounded::<Input>();

    // The engine thread: runs jobs in order, posts their results as inputs.
    let (job_tx, job_rx) = mpsc::channel::<EngineJob>();
    let engine_thread = {
        let engine = Arc::clone(&engine);
        let input_tx = input_tx.clone();
        std::thread::Builder::new()
            .name("player-engine".into())
            .spawn(move || {
                for job in job_rx {
                    if let Some(input) = job(&engine) {
                        let _ = input_tx.send_blocking(input);
                    }
                }
            })
            .expect("spawn player-engine thread")
    };
    let tick_pending = Arc::new(AtomicBool::new(false));
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let ctx = Ctx {
        state,
        input_tx,
        job_tx,
        updates: updates.clone(),
    };

    loop {
        let input = tokio::select! {
            c = commands.recv() => match c {
                Ok(c) => Input::Command(c),
                Err(_) => break, // closed: shut down
            },
            ev = engine_events.recv() => match ev {
                Ok(EngineEvent::TrackAdvanced { track_id, qid, replay_gain, peak_amplitude }) => {
                    log::info!("[player] engine: track-advanced to {track_id} ({qid})");
                    Input::TrackAdvanced { track_id, qid, replay_gain, peak_amplitude }
                }
                Ok(EngineEvent::TrackFinished) => {
                    log::info!("[player] engine: track-finished");
                    Input::TrackFinished
                }
                Ok(EngineEvent::AudioError { kind, message }) => {
                    log::error!("[player] engine: audio-error {kind}{}", message.as_deref().map(|m| format!(": {m}")).unwrap_or_default());
                    Input::AudioError { kind, message }
                }
                Ok(other) => {
                    let _ = updates.send(Update::Engine(other)).await;
                    continue;
                }
                Err(_) => {
                    log::error!("[player] engine event channel closed");
                    break;
                }
            },
            i = input_rx.recv() => match i {
                Ok(i) => i,
                Err(_) => break,
            },
            _ = tick.tick() => {
                if let Some(saver) = &mut saver {
                    saver.periodic(&core);
                }
                if matches!(core.state(), PlaybackState::Playing | PlaybackState::Paused)
                    && !tick_pending.swap(true, Ordering::AcqRel)
                {
                    let pending = Arc::clone(&tick_pending);
                    let track = core.track_seq();
                    ctx.engine(move |e| {
                        pending.store(false, Ordering::Release);
                        e.get_position().ok().map(|p| Input::Tick { position: f64::from(p), track })
                    });
                }
                continue;
            }
        };
        let now = started.elapsed().as_secs_f64();
        for effect in core.handle(input, now) {
            ctx.apply(effect).await;
        }
        if let Some(saver) = &mut saver {
            saver.after_input(&core);
        }
    }

    // Save where playback stands, and wait for the write (off the async
    // worker: dropping the writer joins its thread).
    if let Some(mut saver) = saver {
        saver.save(&core, "quit");
        let _ = tokio::task::spawn_blocking(move || drop(saver)).await;
    }

    // Release the device.
    let _ = ctx.job_tx.send(Box::new(|e: &AudioPlayer| {
        if let Err(err) = e.stop() {
            log::warn!("[player] engine stop: {err}");
        }
        None
    }));
    drop(ctx);
    let _ = tokio::task::spawn_blocking(move || engine_thread.join()).await;
    log::info!("[player] stopped");
}

struct Ctx {
    state: Arc<AppState>,
    input_tx: async_channel::Sender<Input>,
    job_tx: mpsc::Sender<EngineJob>,
    updates: async_channel::Sender<Update>,
}

impl Ctx {
    fn engine(&self, job: impl FnOnce(&AudioPlayer) -> Option<Input> + Send + 'static) {
        let _ = self.job_tx.send(Box::new(job));
    }

    /// An engine call whose only outcome worth knowing is a failure.
    fn engine_call(&self, what: &'static str, call: impl FnOnce(&AudioPlayer) -> Result<(), String> + Send + 'static) {
        self.engine(move |e| {
            if let Err(err) = call(e) {
                log::warn!("[player] engine {what}: {err}");
            }
            None
        });
    }

    async fn apply(&self, effect: Effect) {
        match effect {
            Effect::Resolve { load, item, use_track_gain, normalization, quality } => {
                let (state, tx) = (Arc::clone(&self.state), self.input_tx.clone());
                tokio::spawn(async move {
                    let result = resolve(&state, &quality, normalization, &item, use_track_gain).await;
                    let _ = tx.send(Input::PlayResolved { load, result }).await;
                });
            }
            Effect::ResolveNext { prefetch, item, use_track_gain, normalization, quality } => {
                let (state, tx) = (Arc::clone(&self.state), self.input_tx.clone());
                tokio::spawn(async move {
                    let result = resolve(&state, &quality, normalization, &item, use_track_gain).await;
                    let _ = tx.send(Input::NextResolved { prefetch, result }).await;
                });
            }
            Effect::Play { load, uri, norm_gain, start } => self.engine(move |e| {
                // Gain before play_url, so the pipeline starts at the right
                // level.
                let result = e
                    .set_normalization_gain(norm_gain)
                    .and_then(|()| e.play_url(&uri, start.map(|s| s as f32)));
                Some(Input::PlayStarted { load, result })
            }),
            Effect::ArmNext { uri, norm_gain, track_id, qid, replay_gain, peak_amplitude, is_dash } => {
                self.engine_call("set_next_track", move |e| {
                    e.set_next_track(uri, norm_gain, track_id, qid, replay_gain, peak_amplitude, is_dash)
                })
            }
            Effect::ClearNext => self.engine_call("clear_next_track", |e| e.clear_next_track()),
            Effect::Pause => self.engine_call("pause", |e| e.pause()),
            Effect::Resume => self.engine_call("resume", |e| e.resume()),
            Effect::Stop => self.engine_call("stop", |e| e.stop()),
            Effect::Seek(t) => self.engine_call("seek", move |e| e.seek(t as f32)),
            Effect::SetNormGain(g) => self.engine_call("set_normalization_gain", move |e| e.set_normalization_gain(g)),
            Effect::SetVolume(v) => self.engine_call("set_volume", move |e| e.set_volume(v)),
            Effect::SetGapless(on) => self.engine_call("set_gapless", move |e| e.set_gapless(on)),
            Effect::ConfigureOutput { output, exclusive, device, bit_perfect } => self.engine(move |e| {
                log::info!(
                    "[player] output: exclusive={exclusive} bit_perfect={bit_perfect} device={}",
                    device.as_deref().unwrap_or("-")
                );
                if let Err(err) = e.set_exclusive_mode(exclusive, device).and_then(|()| e.set_bit_perfect(bit_perfect)) {
                    log::warn!("[player] engine output: {err}");
                }
                // Only bit-perfect needs the rates, and probing opens the
                // device, so leave it alone otherwise.
                let rates = if bit_perfect {
                    e.device_rates()
                        .inspect(|r| log::info!("[player] device rates: {r:?}"))
                        .inspect_err(|err| log::warn!("[player] device rates unknown: {err}"))
                        .ok()
                } else {
                    None
                };
                Some(Input::DeviceRates { output, rates })
            }),
            Effect::Emit(event) => {
                let _ = self.updates.send(Update::Player(event)).await;
            }
        }
    }
}

/// `resolve_play_uri` (quality fallback, DASH → data: URI, ReplayGain choice)
/// plus the track's length from its metadata, for the prefetch window.
async fn resolve(
    state: &AppState,
    max_quality: &str,
    normalization: bool,
    item: &QueueItem,
    use_track_gain: bool,
) -> Result<Resolved, ResolveError> {
    let (info, uri, norm_gain, replay_gain, peak_amplitude, is_dash) =
        resolve_play_uri(&state.tidal_client, max_quality, normalization, item.track_id, use_track_gain)
            .await
            .map_err(|e| ResolveError { message: describe(&e), kind: error_kind(&e) })?;
    log::info!(
        "[player] {}: {} gain {} dB, peak {}, normalization {} (x{norm_gain:.3})",
        item.track_id,
        if use_track_gain { "track" } else { "album" },
        if replay_gain.is_nan() { "-".into() } else { format!("{replay_gain:.2}") },
        if peak_amplitude.is_nan() { "-".into() } else { format!("{peak_amplitude:.4}") },
        if normalization { "on" } else { "off" },
    );
    // The manifest's length is exact; TIDAL's metadata rounds to seconds.
    // A page-queued track brought its metadata along; only an ID-only one
    // costs a request (and a turn at the client lock).
    let duration = match info.manifest.as_deref().and_then(mpd_duration) {
        Some(d) => Some(d),
        None if item.info.as_ref().is_some_and(|i| i.duration.is_some()) => item.info.as_ref().and_then(|i| i.duration),
        None => match client_lock::lock(&state.tidal_client, Caller::Other("track length")).await.get_track(item.track_id).await {
            Ok(meta) => meta["duration"].as_f64(),
            Err(e) => {
                log::warn!("[player] no length for track {}: {}", item.track_id, describe(&e));
                None
            }
        },
    };
    let format = StreamFormat { codec: info.codec.clone(), bit_depth: info.bit_depth, sample_rate: info.sample_rate };
    let dash = |o: &Option<String>| o.clone().unwrap_or_else(|| "-".into());
    let summary = format!(
        "{} {} {}-bit/{} Hz{}",
        dash(&info.audio_quality),
        dash(&info.codec),
        info.bit_depth.map_or("-".into(), |b| b.to_string()),
        info.sample_rate.map_or("-".into(), |r| r.to_string()),
        if is_dash { " DASH" } else { "" },
    );
    Ok(Resolved { uri, norm_gain, replay_gain, peak_amplitude, is_dash, duration, format, summary })
}

/// What the UI should say about a failed resolve.
fn error_kind(e: &TidalError) -> ErrorKind {
    if e.is_terminal_unplayable() {
        ErrorKind::Unplayable
    } else if e.is_auth_expired() {
        ErrorKind::LoginExpired
    } else if e.is_network() {
        ErrorKind::Network
    } else {
        ErrorKind::Other
    }
}

/// Decides when the queue is saved, and hands saves to the writer thread.
struct Saver {
    writer: Writer,
    /// What the last save saw: queue revision, state and position (ms).
    saved: Option<(u64, PlaybackState, u64)>,
    last: Instant,
}

impl Saver {
    fn new(path: PathBuf) -> Self {
        Self { writer: Writer::start(path), saved: None, last: Instant::now() }
    }

    fn seen(core: &Core) -> (u64, PlaybackState, u64) {
        (core.queue().revision(), core.state(), (core.saved_position() * 1000.0) as u64)
    }

    fn save(&mut self, core: &Core, why: &str) {
        self.saved = Some(Self::seen(core));
        self.last = Instant::now();
        match core.persisted() {
            Some(q) => {
                log::debug!("[player] saving the queue ({why}): {} tracks, at {} ms", q.tracks.len(), q.position_ms);
                self.writer.send(Save::Write(Box::new(q)));
            }
            None => self.writer.send(Save::Remove),
        }
    }

    /// After every input: the queue or the current track changed (both bump
    /// the revision), playback paused or stopped, or a seek moved a track
    /// that isn't playing (a playing one is saved every `SAVE_EVERY`).
    fn after_input(&mut self, core: &Core) {
        let seen = Self::seen(core);
        let (revision, state, position) = seen;
        let Some((saved_revision, saved_state, saved_position)) = self.saved else {
            // The first input after start: nothing to compare with yet.
            self.saved = Some(seen);
            return;
        };
        let still = matches!(state, PlaybackState::Paused | PlaybackState::Restored);
        if revision != saved_revision {
            self.save(core, "queue changed");
        } else if state != saved_state && matches!(state, PlaybackState::Paused | PlaybackState::Stopped) {
            self.save(core, "paused or stopped");
        } else if still && position != saved_position {
            self.save(core, "moved while not playing");
        } else if state != saved_state {
            self.saved = Some(seen);
        }
    }

    /// While playing, the position every `SAVE_EVERY`.
    fn periodic(&mut self, core: &Core) {
        if core.state() == PlaybackState::Playing && self.last.elapsed() >= SAVE_EVERY {
            self.save(core, "position");
        }
    }
}

/// `mediaPresentationDuration` of a DASH manifest, e.g. `PT3M45.123S`, in
/// seconds. Only the H/M/S parts: a track has no day or month component.
fn mpd_duration(mpd: &str) -> Option<f64> {
    let key = "mediaPresentationDuration=\"";
    let start = mpd.find(key)? + key.len();
    let value = &mpd[start..start + mpd[start..].find('"')?];
    let mut rest = value.strip_prefix("PT")?;
    let mut secs = 0.0;
    while !rest.is_empty() {
        let unit_at = rest.find(|c: char| c.is_ascii_alphabetic())?;
        let n: f64 = rest[..unit_at].parse().ok()?;
        secs += n * match &rest[unit_at..=unit_at] {
            "H" => 3600.0,
            "M" => 60.0,
            "S" => 1.0,
            _ => return None,
        };
        rest = &rest[unit_at + 1..];
    }
    Some(secs)
}

/// `log_safe`, cut before a parse error's body or manifest (stream URLs).
fn describe(e: &TidalError) -> String {
    let msg = e.log_safe();
    match e {
        TidalError::Parse(_) => [" - Body:", " - Manifest:"]
            .iter()
            .filter_map(|cut| msg.find(cut))
            .min()
            .map_or(msg.clone(), |at| format!("{} (response body not shown)", &msg[..at])),
        _ => msg,
    }
}

#[cfg(test)]
mod tests {
    use super::mpd_duration;

    #[test]
    fn parses_mpd_durations() {
        let mpd = |d: &str| format!(r#"<MPD mediaPresentationDuration="{d}" minBufferTime="PT1.5S">"#);
        assert_eq!(mpd_duration(&mpd("PT3M45.123S")), Some(225.123));
        assert_eq!(mpd_duration(&mpd("PT1H0M2S")), Some(3602.0));
        assert_eq!(mpd_duration(&mpd("PT58.5S")), Some(58.5));
        assert_eq!(mpd_duration(&mpd("P1DT1S")), None);
        assert_eq!(mpd_duration(&mpd("PT3X")), None);
        assert_eq!(mpd_duration("<MPD>"), None);
    }
}
