//! The playback state machine, without I/O: inputs in, effects out.
//!
//! Play, next, previous and advance-to-track, the gapless prefetch, the
//! track-advanced/track-finished handling and the ReplayGain bookkeeping.
//! The runner (`runner.rs`) carries out the effects against the engine and
//! TIDAL and feeds the results back as inputs, so everything here is
//! testable without either.
//!
//! The next track is resolved just in time, within `prefetch_window`
//! seconds of the current track's end, not as soon as it is predicted,
//! because stream URLs expire.
//!
//! For the same reason a track paused for `reload_after_pause` seconds is
//! resolved again on resume and picks up where it was, and a track whose
//! stream starts answering 403 mid-play is reloaded once the same way.
//!
//! Continuous playback: with repeat off, once the queue's last track is
//! within `radio_window` of its end, its track radio is fetched and
//! appended, so the prefetch finds a next track and the first radio track
//! follows gaplessly. If the queue ends before the radio arrives, the
//! player waits for it (`Loading`, nothing loading) instead of stopping.

use zeke_tidal::commands::playback::compute_norm_gain;

use crate::persist::PersistedQueue;
use crate::queue::{Advance, NextUp, Origin, Queue, QueueItem, QueueTrack, RepeatMode};

/// Unplayable tracks skipped in a row before playback stops.
const MAX_CONSECUTIVE_PLAY_FAILS: u32 = 3;
/// A next track that failed to resolve is not retried for this long.
const FAILURE_MEMO_SECS: f64 = 10.0;
/// Past this position, Previous restarts the track.
const RESTART_THRESHOLD_SECS: f64 = 3.0;
/// History kept when a radio is appended, so a queue that runs on radio
/// for days stays small.
const HISTORY_KEPT: usize = 200;
/// A second expired-URL error this soon after a reload stops playback.
const RELOAD_MEMO_SECS: f64 = 60.0;

#[derive(Debug, Clone)]
pub struct Config {
    /// Settings' `gapless`.
    pub gapless: bool,
    /// Settings' `volume_normalization` (ReplayGain).
    pub normalization: bool,
    /// Resolve the next track's URL this close to the current track's end.
    pub prefetch_window: f64,
    /// Settings' `max_quality`, the ceiling for `resolve_play_uri`.
    pub max_quality: String,
    /// Bit-perfect output (exclusive mode only): a track plays only at a
    /// rate the device takes, and gapless only follows a track of the same
    /// format.
    pub bit_perfect: bool,
    /// Exclusive ALSA output, as last set with `SetOutput`.
    pub exclusive: bool,
    /// Settings' `continuous`: when the queue runs out with repeat off,
    /// keep going with the last track's radio.
    pub continuous: bool,
    /// Fetch that radio this close to the last track's end: ahead of
    /// `prefetch_window`, so the first radio track can be armed in time.
    pub radio_window: f64,
    /// Resolve the current track again when it is resumed after a pause
    /// this long: by then its stream URLs may have expired.
    pub reload_after_pause: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gapless: true,
            normalization: false,
            prefetch_window: 30.0,
            max_quality: "HI_RES_LOSSLESS".into(),
            bit_perfect: false,
            exclusive: false,
            continuous: false,
            radio_window: 75.0,
            reload_after_pause: 600.0,
        }
    }
}

/// What TIDAL says a stream decodes to. Fields are `None` when the playback
/// info leaves them out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamFormat {
    pub codec: Option<String>,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<u32>,
}

/// The engine's own error for a rate the device lacks in bit-perfect mode
/// (`configure_alsa_hwparams`), so the player's early check reads the same.
pub fn unsupported_rate_error(rate: u32) -> String {
    format!("DAC doesn't support {}kHz — turn off bit-perfect mode for compatibility", rate / 1000)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    /// Resolving and starting a track.
    Loading,
    Playing,
    Paused,
    /// A saved queue came back: the current track is shown paused at its
    /// saved position, but nothing is loaded (the device stays free) until
    /// it is resumed.
    Restored,
}

/// Why playback stopped, for the UI's message. The event's text is for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// TIDAL couldn't be reached.
    Network,
    /// The session can't be refreshed: sign in again.
    LoginExpired,
    /// TIDAL won't play this track (and the tracks after it failed too).
    Unplayable,
    /// Another program holds the exclusive device.
    DeviceBusy,
    /// Bit-perfect, and the device lacks the track's rate.
    UnsupportedRate,
    /// The device went away or refused the format.
    Device,
    Other,
}

impl ErrorKind {
    /// The engine's `audio-error` kinds and `play_url` errors.
    pub fn of_engine(kind: &str, message: &str) -> Self {
        if kind == "device_busy" || message.contains("device_busy") {
            ErrorKind::DeviceBusy
        } else if message.contains("doesn't support") && message.contains("bit-perfect") {
            ErrorKind::UnsupportedRate
        } else if matches!(kind, "device_disconnected" | "device_changed" | "format_change_failed")
            || message.contains("ALSA device")
        {
            ErrorKind::Device
        } else {
            ErrorKind::Other
        }
    }
}

/// How a track became current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The first track of a new queue.
    Start,
    /// The engine switched to the prerolled next track with no gap.
    Gapless,
    /// The previous track ended without a prerolled next (a new pipeline).
    AfterEnd,
    /// Repeat-one replay.
    Repeat,
    /// Repeat-all started the queue over.
    Wrap,
    /// The user skipped forward.
    Skip,
    /// The user went back.
    Previous,
    /// The user picked a track in the queue.
    Jump,
    /// A saved queue was restored (nothing plays yet).
    Restore,
    /// The current track was resolved again (its stream URLs expired) and
    /// picks up where it was.
    Reload,
}

#[derive(Debug, Clone)]
pub enum PlayerCommand {
    /// Replace the queue and play `start` (with shuffle and no start, a
    /// random track). `album_mode`: the tracks are one
    /// album in order, so album ReplayGain applies.
    Load {
        tracks: Vec<QueueTrack>,
        start: Option<usize>,
        album_mode: bool,
        shuffle: bool,
        repeat: RepeatMode,
    },
    Pause,
    /// Resume when paused; when stopped, play the current track again.
    Resume,
    /// Pause when playing, else as `Resume`.
    TogglePause,
    Next,
    Previous,
    Seek(f64),
    /// Seek relative to the current position (MPRIS `Seek`).
    SeekBy(f64),
    Stop,
    /// Play this queue entry (a click in the queue list). By qid, since the
    /// play order may have changed since the list was drawn.
    JumpTo(String),
    SetShuffle(bool),
    SetRepeat(RepeatMode),
    SetNormalization(bool),
    SetVolume(f32),
    SetGapless(bool),
    SetMaxQuality(String),
    /// Output mode and device. The playing track keeps its output; the
    /// change takes effect from the next track, which starts a new pipeline.
    SetOutput {
        exclusive: bool,
        device: Option<String>,
        bit_perfect: bool,
    },
    Append(Vec<QueueTrack>),
    PlayNext(QueueTrack),
    /// Remove the n-th upcoming track (0 = the next one).
    RemoveUpcoming(usize),
    /// Replace the queue with a saved one, `Restored` at its saved position:
    /// nothing is resolved or played until `Resume`.
    Restore(Box<PersistedQueue>),
    /// Continuous playback on or off (`Config::continuous`).
    SetContinuous(bool),
}

/// A resolved stream, as the runner reports it.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub uri: String,
    pub norm_gain: f64,
    /// NaN when TIDAL has none.
    pub replay_gain: f64,
    pub peak_amplitude: f64,
    pub is_dash: bool,
    /// Track length in seconds, from TIDAL's metadata.
    pub duration: Option<f64>,
    pub format: StreamFormat,
    /// Quality summary for the log (e.g. "HI_RES_LOSSLESS FLAC 24/96000").
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct ResolveError {
    pub message: String,
    /// `Unplayable`: TIDAL says the track can't be played, so skip it
    /// rather than stop.
    pub kind: ErrorKind,
}

#[derive(Debug, Clone)]
pub enum Input {
    Command(PlayerCommand),
    /// Engine `track-advanced`.
    TrackAdvanced {
        track_id: u64,
        qid: String,
        replay_gain: f64,
        peak_amplitude: f64,
    },
    /// Engine `track-finished`.
    TrackFinished,
    /// Engine `audio-error`.
    AudioError { kind: String, message: Option<String> },
    /// The engine's position in the current track, polled by the runner.
    /// `track` is `Core::track_seq` when the poll was sent; a poll that
    /// straddled a track change is dropped.
    Tick { position: f64, track: u64 },
    PlayResolved { load: u64, result: Result<Resolved, ResolveError> },
    /// `play_url`'s own result (errors found before any audio flows).
    PlayStarted { load: u64, result: Result<(), String> },
    NextResolved { prefetch: u64, result: Result<Resolved, ResolveError> },
    /// The rates the exclusive device takes, probed after `ConfigureOutput`
    /// in bit-perfect mode; `None` when unknown (not probed, or busy).
    DeviceRates { output: u64, rates: Option<Vec<u32>> },
    /// A radio's playable tracks, in TIDAL's order, for `FetchRadio`.
    RadioFetched { fetch: u64, result: Result<Vec<QueueTrack>, ResolveError> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Resolve a track to play now; answer with `PlayResolved`.
    Resolve { load: u64, item: QueueItem, use_track_gain: bool, normalization: bool, quality: String },
    /// Resolve the next track for the gapless slot; answer with `NextResolved`.
    ResolveNext { prefetch: u64, item: QueueItem, use_track_gain: bool, normalization: bool, quality: String },
    /// Set the gain, then `play_url` (from `start` seconds); answer with
    /// `PlayStarted`.
    Play { load: u64, uri: String, norm_gain: f64, start: Option<f64> },
    ArmNext {
        uri: String,
        norm_gain: f64,
        track_id: u64,
        qid: String,
        replay_gain: f64,
        peak_amplitude: f64,
        is_dash: bool,
    },
    ClearNext,
    Pause,
    Resume,
    Stop,
    Seek(f64),
    SetNormGain(f64),
    SetVolume(f32),
    SetGapless(bool),
    /// Configure the engine's output; in bit-perfect mode then probe the
    /// device's rates and answer with `DeviceRates` for this `output`.
    ConfigureOutput { output: u64, exclusive: bool, device: Option<String>, bit_perfect: bool },
    /// Fetch the track radio of `seed`; answer with `RadioFetched`, always
    /// (a failure or a timeout too), or a wait for it never ends.
    FetchRadio { fetch: u64, seed: QueueItem },
    Emit(PlayerEvent),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    State(PlaybackState),
    TrackStarted {
        item: QueueItem,
        /// Place in the play order, and its length.
        index: usize,
        len: usize,
        via: Transition,
        summary: Option<String>,
        duration: Option<f64>,
        format: Option<StreamFormat>,
    },
    /// The play order, the current track's place in it, shuffle and repeat;
    /// sent whenever any of them changes.
    QueueChanged {
        items: Vec<QueueItem>,
        current: usize,
        shuffle: bool,
        repeat: RepeatMode,
    },
    /// Position in the current track, in seconds (polled, and after a seek).
    Position(f64),
    /// The next track's URL is being resolved, `remaining` seconds before the
    /// current track ends (`None` if its length is unknown).
    PrefetchStarted { item: QueueItem, remaining: Option<f64> },
    PrefetchArmed { item: QueueItem, remaining: Option<f64>, summary: String },
    PrefetchCleared { item: QueueItem, reason: &'static str },
    PrefetchFailed { item: QueueItem, error: String },
    /// Resolved but deliberately not armed: the track will start with a new
    /// pipeline after the current one ends.
    PrefetchNotArmed { item: QueueItem, reason: String },
    QueueEnded,
    /// A track was skipped because TIDAL can't play it.
    Skipped { item: QueueItem, error: String },
    /// Playback stopped. `message` is for the log.
    Error { kind: ErrorKind, message: String },
    /// The exclusive `device` (`None`: the engine's default) was busy when
    /// a track opened it, so output went back to the system default and the
    /// track is played again there. The saved output is the caller's to
    /// change.
    OutputFellBack { device: Option<String> },
    /// The output the playing track opened, sent when a track starts on an
    /// output other than the last one reported. A gapless switch opens
    /// nothing, so a next track prerolled before a `SetOutput` still plays
    /// on the old output; the track after it starts on the new one.
    OutputActive { exclusive: bool, device: Option<String> },
    /// Continuous playback is fetching `seed`'s radio.
    RadioFetchStarted { seed: QueueItem },
    /// `count` tracks of `seed`'s radio were appended to the queue.
    RadioAppended { seed: QueueItem, count: usize },
    /// Something the user should hear about that is not an error.
    Notice(String),
}

#[derive(Debug, Clone)]
struct Current {
    duration: Option<f64>,
    format: Option<StreamFormat>,
    /// Kept for a live normalization toggle.
    replay_gain: f64,
    peak_amplitude: f64,
}

#[derive(Debug, Clone)]
enum Slot {
    Resolving,
    Armed(Resolved),
    /// Resolved, but bit-perfect can't switch to it gaplessly.
    NotArmed,
}

#[derive(Debug, Clone)]
struct Prefetch {
    item: QueueItem,
    slot: Slot,
}

/// Continuous playback's radio for the current track: at most one fetch
/// per track, whatever its outcome.
#[derive(Debug, Clone)]
struct Radio {
    /// The entry the radio was fetched for.
    qid: String,
    /// `radio_gen` of the fetch; an answer for another is stale.
    fetch: u64,
    status: RadioStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RadioStatus {
    Pending,
    Appended,
    /// Failed, or nothing new in it: not fetched again for this track.
    Failed,
}

/// The queue ended while its radio was on the way.
#[derive(Debug, Clone)]
struct Waiting {
    /// How the radio's first track will have become current.
    via: Transition,
}

#[derive(Debug, Clone)]
struct Loading {
    item: QueueItem,
    via: Transition,
    resolved: Option<Resolved>,
    /// Start this far in (resuming a restored queue).
    start_at: Option<f64>,
    /// This load resumes a restored entry saved at this position: if it
    /// fails or is cancelled, the entry goes back to `Restored` there.
    restored_at: Option<f64>,
    /// `output_gen` when its `Play` was issued.
    output: u64,
    /// The output that `Play` opens: (exclusive, device).
    target: (bool, Option<String>),
}

pub struct Core {
    queue: Queue,
    /// For queues rebuilt from a saved one.
    seed: u64,
    config: Config,
    state: PlaybackState,
    /// Album in order → album gain; anything else → track gain.
    use_track_gain: bool,
    current: Option<Current>,
    position: f64,
    /// Generation of track loads; a result for an older one is stale.
    load: u64,
    loading: Option<Loading>,
    /// Generation of prefetches.
    prefetch_gen: u64,
    prefetch: Option<Prefetch>,
    /// The last failed prefetch: (qid, retry after).
    failed: Option<(String, f64)>,
    consecutive_fails: u32,
    /// Bumped whenever a track starts, to drop position polls of the last one.
    track_seq: u64,
    /// Monotonic seconds, from the last input.
    now: f64,
    /// See `Input::DeviceRates`.
    device_rates: Option<Vec<u32>>,
    /// Generation of `SetOutput`s; rates probed for an older one are stale.
    output_gen: u64,
    /// The output changed while a track played: the next track starts a new
    /// pipeline (so it gets the new output) instead of a gapless switch.
    output_changed: bool,
    /// The exclusive device, as last set with `SetOutput`.
    device: Option<String>,
    /// The output last reported with `OutputActive`.
    active: Option<(bool, Option<String>)>,
    /// `Queue::revision` last published as `QueueChanged`.
    published_queue: Option<u64>,
    /// Cleared with the load and the prefetch whenever another track
    /// starts loading or playback stops.
    radio: Option<Radio>,
    /// Generation of radio fetches.
    radio_gen: u64,
    waiting: Option<Waiting>,
    /// `now` when playback was last paused.
    paused_at: Option<f64>,
    /// `now` of the last reload after an expired-URL error.
    reloaded_at: Option<f64>,
}

impl Core {
    pub fn new(config: Config, seed: u64) -> Self {
        Self {
            queue: Queue::new(seed),
            seed,
            config,
            state: PlaybackState::Stopped,
            use_track_gain: true,
            current: None,
            position: 0.0,
            load: 0,
            loading: None,
            prefetch_gen: 0,
            prefetch: None,
            failed: None,
            consecutive_fails: 0,
            track_seq: 0,
            now: 0.0,
            device_rates: None,
            output_gen: 0,
            output_changed: false,
            device: None,
            active: None,
            published_queue: None,
            radio: None,
            radio_gen: 0,
            waiting: None,
            paused_at: None,
            reloaded_at: None,
        }
    }

    /// Stamp for `Input::Tick`.
    pub fn track_seq(&self) -> u64 {
        self.track_seq
    }

    pub fn state(&self) -> PlaybackState {
        self.state
    }

    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    /// The queue as it should be saved now, or `None` when it is empty.
    /// The position is where playback would pick up: the current track's,
    /// or where a load in flight will start.
    pub fn persisted(&self) -> Option<PersistedQueue> {
        self.queue.current()?;
        let position = self.saved_position();
        Some(self.queue.to_persisted((position.max(0.0) * 1000.0) as u64, !self.use_track_gain))
    }

    /// Where playback would pick up the current track, in seconds.
    pub fn saved_position(&self) -> f64 {
        match self.state {
            PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Restored => self.position,
            PlaybackState::Loading => self.loading.as_ref().and_then(|l| l.restored_at.or(l.start_at)).unwrap_or(0.0),
            PlaybackState::Stopped => 0.0,
        }
    }

    /// Handle one input at monotonic time `now` (seconds).
    pub fn handle(&mut self, input: Input, now: f64) -> Vec<Effect> {
        self.now = now;
        let mut fx = Vec::new();
        match input {
            Input::Command(c) => self.command(c, &mut fx),
            Input::TrackAdvanced { track_id, qid, replay_gain, peak_amplitude } => {
                self.track_advanced(track_id, qid, replay_gain, peak_amplitude, &mut fx)
            }
            Input::TrackFinished => self.track_finished(&mut fx),
            Input::AudioError { kind, message } => self.audio_error(kind, message, &mut fx),
            Input::Tick { position, track } => {
                // Only a loaded track has a position in the engine.
                if track == self.track_seq && matches!(self.state, PlaybackState::Playing | PlaybackState::Paused) {
                    self.position = position;
                    fx.push(Effect::Emit(PlayerEvent::Position(position)));
                    self.maybe_prefetch(&mut fx);
                }
            }
            Input::PlayResolved { load, result } => self.play_resolved(load, result, &mut fx),
            Input::PlayStarted { load, result } => self.play_started(load, result, &mut fx),
            Input::NextResolved { prefetch, result } => self.next_resolved(prefetch, result, &mut fx),
            Input::DeviceRates { output, rates } => {
                if output == self.output_gen {
                    // An empty probe says nothing; don't refuse every rate.
                    self.device_rates = rates.filter(|r| !r.is_empty());
                }
            }
            Input::RadioFetched { fetch, result } => self.radio_fetched(fetch, result, &mut fx),
        }
        if self.published_queue != Some(self.queue.revision()) {
            self.published_queue = Some(self.queue.revision());
            let (current, _) = self.queue.position();
            fx.push(Effect::Emit(PlayerEvent::QueueChanged {
                items: self.queue.in_order().cloned().collect(),
                current,
                shuffle: self.queue.shuffle(),
                repeat: self.queue.repeat(),
            }));
        }
        fx
    }

    /// An engine error: an expired stream is reloaded once, anything else
    /// stops playback and is reported.
    fn audio_error(&mut self, kind: String, message: Option<String>, fx: &mut Vec<Effect>) {
        // Restored: nothing is loaded, so the error isn't this queue's.
        if matches!(self.state, PlaybackState::Stopped | PlaybackState::Restored) {
            return;
        }
        let text = message.as_deref().unwrap_or("");
        if matches!(self.state, PlaybackState::Playing | PlaybackState::Paused)
            && expired_url(text)
            && self.reloaded_at.is_none_or(|t| self.now - t >= RELOAD_MEMO_SECS)
        {
            log::warn!("[player] the stream answered 403 (its URLs expired?): reloading at {:.1} s", self.position);
            self.reloaded_at = Some(self.now);
            return self.reload(fx);
        }
        let error = ErrorKind::of_engine(&kind, text);
        let msg = match message {
            Some(m) => format!("{kind}: {m}"),
            None => kind,
        };
        self.halt(fx);
        fx.push(Effect::Emit(PlayerEvent::Error { kind: error, message: msg }));
    }

    /// In bit-perfect mode, the engine's error for a rate the device lacks.
    /// `None` when it plays, or when the rate or the device's rates are
    /// unknown (the engine's own check still stops it then).
    fn unsupported_by_device(&self, format: &StreamFormat) -> Option<String> {
        let rate = format.sample_rate?;
        let rates = self.device_rates.as_ref()?;
        (self.config.bit_perfect && !rates.contains(&rate)).then(|| unsupported_rate_error(rate))
    }

    /// In bit-perfect mode, why `next` can't follow the current track
    /// gaplessly: the engine only switches between identical formats.
    fn gapless_mismatch(&self, next: &StreamFormat) -> Option<String> {
        if !self.config.bit_perfect {
            return None;
        }
        let show = |f: &StreamFormat| {
            format!(
                "{} {}-bit/{} Hz",
                f.codec.as_deref().unwrap_or("?"),
                f.bit_depth.map_or("?".into(), |b| b.to_string()),
                f.sample_rate.map_or("?".into(), |r| r.to_string())
            )
        };
        let Some(current) = self.current.as_ref().and_then(|c| c.format.as_ref()) else {
            return Some(format!("bit-perfect: the current format is unknown; {} starts fresh", show(next)));
        };
        // Anything unknown counts as different: arming is only safe when
        // both are known to decode the same.
        let known = |f: &StreamFormat| f.codec.is_some() && f.sample_rate.is_some() && f.bit_depth.is_some();
        let same = |a: &Option<String>, b: &Option<String>| {
            a.as_deref().map(str::to_ascii_lowercase) == b.as_deref().map(str::to_ascii_lowercase)
        };
        let matches = known(current)
            && known(next)
            && same(&current.codec, &next.codec)
            && current.sample_rate == next.sample_rate
            && current.bit_depth == next.bit_depth;
        (!matches).then(|| format!("bit-perfect: {} differs from the current {}", show(next), show(current)))
    }

    fn set_state(&mut self, state: PlaybackState, fx: &mut Vec<Effect>) {
        if self.state != state {
            self.state = state;
            fx.push(Effect::Emit(PlayerEvent::State(state)));
        }
    }

    fn command(&mut self, c: PlayerCommand, fx: &mut Vec<Effect>) {
        match c {
            PlayerCommand::Load { tracks, start, album_mode, shuffle, repeat } => {
                self.use_track_gain = !album_mode;
                self.queue.set_repeat(repeat);
                match self.queue.load(tracks, start, shuffle) {
                    Some(item) => self.start_load(item, Transition::Start, fx),
                    None => self.stop(fx),
                }
            }
            PlayerCommand::Pause => {
                if self.state == PlaybackState::Playing {
                    fx.push(Effect::Pause);
                    self.paused_at = Some(self.now);
                    self.set_state(PlaybackState::Paused, fx);
                }
            }
            PlayerCommand::Resume => self.resume(fx),
            PlayerCommand::TogglePause => match self.state {
                PlaybackState::Playing => self.command(PlayerCommand::Pause, fx),
                // Shown as playing; the button must not look dead.
                PlaybackState::Loading => self.halt(fx),
                PlaybackState::Paused | PlaybackState::Stopped | PlaybackState::Restored => self.resume(fx),
            },
            PlayerCommand::SeekBy(d) => {
                if matches!(self.state, PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Restored) {
                    let target = self.position + d;
                    // Past the end is a skip (MPRIS Seek).
                    if self.current.as_ref().and_then(|c| c.duration).is_some_and(|len| target >= len) {
                        self.command(PlayerCommand::Next, fx);
                    } else {
                        self.seek(target, fx);
                    }
                }
            }
            PlayerCommand::JumpTo(qid) => {
                self.consecutive_fails = 0;
                let at = self.queue.in_order().position(|t| t.qid == qid);
                if let Some(item) = at.and_then(|at| self.queue.jump_to(at)) {
                    self.start_load(item, Transition::Jump, fx);
                }
            }
            PlayerCommand::SetGapless(on) => {
                self.config.gapless = on;
                fx.push(Effect::SetGapless(on));
                self.maybe_prefetch(fx);
            }
            PlayerCommand::SetMaxQuality(quality) => {
                self.config.max_quality = quality;
                // The slot was resolved at the old ceiling.
                self.clear_prefetch("quality changed", fx);
                self.maybe_prefetch(fx);
            }
            // In bit-perfect mode the runner probes the device's rates here,
            // before the card is reserved, so the probe can find it busy
            // while PipeWire has it open. The early rate check is skipped
            // then; the engine still refuses an unsupported rate at open.
            PlayerCommand::SetOutput { exclusive, device, bit_perfect } => {
                self.config.bit_perfect = exclusive && bit_perfect;
                self.config.exclusive = exclusive;
                self.device = device.clone();
                self.device_rates = None;
                self.output_gen += 1;
                // The engine drops a prerolled next track on any mode or
                // device change; the next track starts on the new output.
                self.clear_prefetch("output changed", fx);
                self.output_changed = self.state != PlaybackState::Stopped;
                fx.push(Effect::ConfigureOutput {
                    output: self.output_gen,
                    exclusive,
                    device,
                    bit_perfect: exclusive && bit_perfect,
                });
            }
            PlayerCommand::Next => {
                self.consecutive_fails = 0;
                match self.queue.advance(true) {
                    Advance::Next(item) | Advance::Wrapped(item) | Advance::Same(item) => {
                        self.start_load(item, Transition::Skip, fx)
                    }
                    Advance::End => self.queue_ended(Transition::Skip, fx),
                }
            }
            PlayerCommand::Previous => {
                self.consecutive_fails = 0;
                if self.waiting.is_some() {
                    // Nothing is loaded to seek in: play the track before,
                    // or the last one again.
                    if let Some(item) = self.queue.back().or_else(|| self.queue.current().cloned()) {
                        self.start_load(item, Transition::Previous, fx);
                    }
                } else if self.position > RESTART_THRESHOLD_SECS || self.current.is_none() {
                    self.seek(0.0, fx);
                } else if let Some(item) = self.queue.back() {
                    self.start_load(item, Transition::Previous, fx);
                } else {
                    self.seek(0.0, fx);
                }
            }
            PlayerCommand::Seek(t) => self.seek(t, fx),

            PlayerCommand::Stop => self.stop(fx),
            PlayerCommand::SetShuffle(on) => {
                self.queue.set_shuffle(on);
                self.maybe_prefetch(fx);
            }
            PlayerCommand::SetRepeat(mode) => {
                self.queue.set_repeat(mode);
                if mode != RepeatMode::Off && self.waiting.take().is_some() {
                    // Repeat now says what comes after the last track.
                    match self.queue.advance(false) {
                        Advance::Same(item) => self.start_load(item, Transition::Repeat, fx),
                        Advance::Wrapped(item) => self.start_load(item, Transition::Wrap, fx),
                        Advance::Next(item) => self.start_load(item, Transition::Skip, fx),
                        Advance::End => self.end_queue(fx),
                    }
                    return;
                }
                if mode != RepeatMode::Off {
                    // Repeat decides what follows now: a radio still on
                    // the way is never appended.
                    self.radio = None;
                }
                self.maybe_prefetch(fx);
            }
            PlayerCommand::Append(tracks) => {
                self.queue.append(tracks);
                self.play_pick_if_waiting(fx);
                self.maybe_prefetch(fx);
            }
            PlayerCommand::PlayNext(track) => {
                self.queue.play_next(track);
                self.play_pick_if_waiting(fx);
                self.maybe_prefetch(fx);
            }
            PlayerCommand::RemoveUpcoming(n) => {
                self.queue.remove_upcoming(n);
                self.maybe_prefetch(fx);
            }
            PlayerCommand::SetNormalization(on) => {
                self.config.normalization = on;
                // Recompute from the stored ReplayGain of what is playing now.
                let gain = match (&self.current, on) {
                    (Some(c), true) => compute_norm_gain(finite(c.replay_gain), finite(c.peak_amplitude)),
                    _ => 1.0,
                };
                fx.push(Effect::SetNormGain(gain));
                // The armed next track's gain was computed under the old
                // setting; re-resolve it.
                self.clear_prefetch("normalization changed", fx);
                self.maybe_prefetch(fx);
            }
            PlayerCommand::SetVolume(v) => fx.push(Effect::SetVolume(v)),
            PlayerCommand::Restore(saved) => self.restore(&saved, fx),
            PlayerCommand::SetContinuous(on) => {
                self.config.continuous = on;
                if on {
                    self.maybe_fetch_radio(fx);
                    return;
                }
                // A radio still on the way is never appended now.
                self.radio = None;
                if let Some(waiting) = self.waiting.take() {
                    match self.queue.advance(true) {
                        Advance::Next(item) | Advance::Wrapped(item) | Advance::Same(item) => {
                            self.start_load(item, waiting.via, fx)
                        }
                        Advance::End => self.end_queue(fx),
                    }
                }
            }
        }
    }

    /// While waiting for a radio, a track the user queues plays at once;
    /// the radio's answer is then stale.
    fn play_pick_if_waiting(&mut self, fx: &mut Vec<Effect>) {
        let Some(waiting) = self.waiting.take() else { return };
        match self.queue.advance(true) {
            Advance::Next(item) | Advance::Wrapped(item) | Advance::Same(item) => self.start_load(item, waiting.via, fx),
            Advance::End => self.waiting = Some(waiting),
        }
    }

    fn end_queue(&mut self, fx: &mut Vec<Effect>) {
        self.stop(fx);
        fx.push(Effect::Emit(PlayerEvent::QueueEnded));
    }

    /// The queue ran out (`via`: a skip past its end, or its last track
    /// ended). With continuous playback, unless this track's radio already
    /// came and went, wait for it, fetching it first if none is on the way.
    fn queue_ended(&mut self, via: Transition, fx: &mut Vec<Effect>) {
        let status = self.radio_status();
        let wait = self.config.continuous
            && self.queue.repeat() == RepeatMode::Off
            && self.queue.current().is_some()
            && matches!(status, None | Some(RadioStatus::Pending));
        if !wait {
            return self.end_queue(fx);
        }
        if status.is_none() {
            self.fetch_radio(fx);
        }
        if via == Transition::Skip {
            // The track skipped from must not keep playing.
            fx.push(Effect::Stop);
        }
        self.clear_prefetch("waiting for the radio", fx);
        self.load += 1; // a late result for the last load is stale
        self.loading = None;
        self.position = 0.0;
        self.set_state(PlaybackState::Loading, fx);
        self.waiting = Some(Waiting { via });
    }

    /// The radio's status, if it was fetched for the current entry.
    fn radio_status(&self) -> Option<RadioStatus> {
        let radio = self.radio.as_ref()?;
        (self.queue.current()?.qid == radio.qid).then_some(radio.status)
    }

    fn fetch_radio(&mut self, fx: &mut Vec<Effect>) {
        let Some(seed) = self.queue.current().cloned() else { return };
        self.radio_gen += 1;
        self.radio = Some(Radio { qid: seed.qid.clone(), fetch: self.radio_gen, status: RadioStatus::Pending });
        fx.push(Effect::Emit(PlayerEvent::RadioFetchStarted { seed: seed.clone() }));
        fx.push(Effect::FetchRadio { fetch: self.radio_gen, seed });
    }

    /// Fetch the radio once the last track is within `radio_window` of its
    /// end, while it plays: a pause at the end of an album fills nothing.
    /// On `peek_next`, not the prefetch's prediction, so it fetches with
    /// gapless off or after an output change too.
    fn maybe_fetch_radio(&mut self, fx: &mut Vec<Effect>) {
        let due = self.config.continuous
            && self.queue.repeat() == RepeatMode::Off
            && self.state == PlaybackState::Playing
            && self.queue.current().is_some()
            && self.queue.peek_next().is_none()
            && self.radio_status().is_none()
            // Unknown length: fetch now rather than risk the end.
            && self.remaining().is_none_or(|r| r <= self.config.radio_window);
        if due {
            self.fetch_radio(fx);
        }
    }

    fn radio_fetched(&mut self, fetch: u64, result: Result<Vec<QueueTrack>, ResolveError>, fx: &mut Vec<Effect>) {
        let Some(radio) = &self.radio else { return };
        let Some(seed) = self.queue.current().cloned() else { return };
        if radio.fetch != fetch || radio.status != RadioStatus::Pending || radio.qid != seed.qid {
            return; // superseded
        }
        let failure = match result {
            Ok(tracks) => {
                // Leave out anything queued already, the seed (the radio's
                // first track) included, and repeats within the radio.
                let mut seen: std::collections::HashSet<u64> = self.queue.in_order().map(|i| i.track_id).collect();
                let fresh: Vec<QueueTrack> = tracks.into_iter().filter(|t| seen.insert(t.id)).collect();
                if fresh.is_empty() {
                    log::info!("[player] the radio of {} has nothing that isn't queued already", seed.track_id);
                    Some(None)
                } else {
                    let count = fresh.len();
                    self.queue.trim_history(HISTORY_KEPT);
                    self.queue.append_in_order(fresh, Origin::Radio { seed: seed.track_id });
                    // No longer one album in order.
                    self.use_track_gain = true;
                    fx.push(Effect::Emit(PlayerEvent::RadioAppended { seed, count }));
                    None
                }
            }
            Err(e) => {
                log::warn!("[player] the radio of {} failed: {}", seed.track_id, e.message);
                Some(Some(e))
            }
        };
        if let Some(radio) = self.radio.as_mut() {
            radio.status = if failure.is_some() { RadioStatus::Failed } else { RadioStatus::Appended };
        }
        let Some(waiting) = self.waiting.take() else {
            return self.maybe_prefetch(fx);
        };
        match self.queue.advance(true) {
            Advance::Next(item) | Advance::Wrapped(item) | Advance::Same(item) => self.start_load(item, waiting.via, fx),
            // A failed radio is an error, like an unplayable load: no
            // QueueEnded, so a headless caller doesn't read it as success.
            Advance::End => {
                self.stop(fx);
                match failure.flatten() {
                    Some(e) => fx.push(Effect::Emit(PlayerEvent::Error { kind: e.kind, message: e.message })),
                    None => {
                        fx.push(Effect::Emit(PlayerEvent::Notice("No more tracks: no radio to continue with".into())));
                        fx.push(Effect::Emit(PlayerEvent::QueueEnded));
                    }
                }
            }
        }
    }

    fn restore(&mut self, saved: &PersistedQueue, fx: &mut Vec<Effect>) {
        let Some(queue) = Queue::from_persisted(saved, self.seed) else {
            log::warn!("[player] the saved queue is inconsistent; starting empty");
            return;
        };
        self.stop(fx); // clears the radio and any wait for one
        // Revisions restart at zero; keep them moving forward so the new
        // queue is published.
        let published = self.queue.revision();
        self.queue = queue;
        self.queue.bump_revision_past(published);
        self.use_track_gain = !saved.album_mode;
        let Some(item) = self.queue.current().cloned() else { return };
        let duration = item.info.as_ref().and_then(|i| i.duration);
        self.position = saved.position_ms as f64 / 1000.0;
        if let Some(len) = duration {
            self.position = self.position.min(len);
        }
        self.track_seq += 1;
        self.current = Some(Current { duration, format: None, replay_gain: f64::NAN, peak_amplitude: f64::NAN });
        self.set_state(PlaybackState::Restored, fx);
        let (index, len) = self.queue.position();
        fx.push(Effect::Emit(PlayerEvent::TrackStarted {
            item,
            index,
            len,
            via: Transition::Restore,
            summary: None,
            duration,
            format: None,
        }));
        fx.push(Effect::Emit(PlayerEvent::Position(self.position)));
    }

    fn resume(&mut self, fx: &mut Vec<Effect>) {
        match self.state {
            PlaybackState::Paused => {
                let paused_for = self.paused_at.map_or(0.0, |t| self.now - t);
                if paused_for >= self.config.reload_after_pause {
                    log::info!("[player] resuming after {paused_for:.0} s paused: reloading at {:.1} s", self.position);
                    self.reload(fx);
                } else {
                    fx.push(Effect::Resume);
                    self.set_state(PlaybackState::Playing, fx);
                }
            }
            PlaybackState::Stopped => {
                if let Some(item) = self.queue.current().cloned() {
                    self.start_load(item, Transition::Start, fx);
                }
            }
            PlaybackState::Restored => {
                if let Some(item) = self.queue.current().cloned() {
                    self.load_at(item, self.position, Transition::Start, fx);
                }
            }
            PlaybackState::Playing | PlaybackState::Loading => {}
        }
    }

    /// Resolve the current track again and play it from where it is, with
    /// fresh stream URLs.
    fn reload(&mut self, fx: &mut Vec<Effect>) {
        match self.queue.current().cloned() {
            Some(item) => self.load_at(item, self.position, Transition::Reload, fx),
            None => self.stop(fx),
        }
    }

    /// `start_load` from `at` seconds. If the load fails or is cancelled,
    /// the entry goes `Restored` at `at`.
    fn load_at(&mut self, item: QueueItem, at: f64, via: Transition, fx: &mut Vec<Effect>) {
        self.start_load(item, via, fx);
        if let Some(loading) = self.loading.as_mut() {
            loading.start_at = (at > 0.0).then_some(at);
            loading.restored_at = Some(at);
        }
    }

    fn start_load(&mut self, item: QueueItem, via: Transition, fx: &mut Vec<Effect>) {
        self.load += 1;
        self.radio = None;
        self.waiting = None;
        // Whatever was armed belongs to the track being replaced.
        self.clear_prefetch("a new track is loading", fx);
        self.loading = Some(Loading {
            item: item.clone(),
            via,
            resolved: None,
            start_at: None,
            restored_at: None,
            output: self.output_gen,
            target: self.output_target(),
        });
        self.set_state(PlaybackState::Loading, fx);
        fx.push(Effect::Resolve {
            load: self.load,
            item,
            use_track_gain: self.use_track_gain,
            normalization: self.config.normalization,
            quality: self.config.max_quality.clone(),
        });
    }

    fn stop(&mut self, fx: &mut Vec<Effect>) {
        self.load += 1; // results of an in-flight load are now stale
        self.loading = None;
        self.radio = None;
        self.waiting = None;
        self.clear_prefetch("stopped", fx);
        fx.push(Effect::Stop);
        self.position = 0.0;
        self.set_state(PlaybackState::Stopped, fx);
    }

    /// Stop, after an error or a cancel. A load that was resuming a
    /// restored entry goes back to `Restored` at its saved position instead,
    /// so a failed resume (offline, device busy, expired login) keeps it.
    fn halt(&mut self, fx: &mut Vec<Effect>) {
        let restored_at = match self.state {
            PlaybackState::Loading => self.loading.as_ref().and_then(|l| l.restored_at),
            _ => None,
        };
        self.stop_at(restored_at, fx);
    }

    /// `stop`, or back to `Restored` at `restored_at`.
    fn stop_at(&mut self, restored_at: Option<f64>, fx: &mut Vec<Effect>) {
        let Some(at) = restored_at else { return self.stop(fx) };
        self.load += 1;
        self.loading = None;
        self.radio = None;
        self.waiting = None;
        self.clear_prefetch("stopped", fx);
        // Releases whatever the failed start opened.
        fx.push(Effect::Stop);
        self.position = at;
        self.set_state(PlaybackState::Restored, fx);
        fx.push(Effect::Emit(PlayerEvent::Position(at)));
    }

    fn seek(&mut self, t: f64, fx: &mut Vec<Effect>) {
        if self.state == PlaybackState::Restored {
            // Nothing is loaded: move where it will start.
            self.position = t.max(0.0);
            fx.push(Effect::Emit(PlayerEvent::Position(self.position)));
            return;
        }
        if !matches!(self.state, PlaybackState::Playing | PlaybackState::Paused) {
            return;
        }
        let t = t.max(0.0);
        fx.push(Effect::Seek(t));
        self.position = t;
        fx.push(Effect::Emit(PlayerEvent::Position(t)));
        // A slot armed near the end would sit on an open stream for as long as
        // the seek moved away from it; drop it and resolve again in time.
        if self.prefetch.is_some() && self.remaining().is_some_and(|r| r > self.config.prefetch_window) {
            self.clear_prefetch("seek moved out of the prefetch window", fx);
        }
        self.maybe_prefetch(fx);
    }

    fn remaining(&self) -> Option<f64> {
        let duration = self.current.as_ref()?.duration?;
        Some((duration - self.position).max(0.0))
    }

    fn clear_prefetch(&mut self, reason: &'static str, fx: &mut Vec<Effect>) {
        self.prefetch_gen += 1; // an in-flight resolve is now stale
        if let Some(p) = self.prefetch.take() {
            if matches!(p.slot, Slot::Armed(_)) {
                fx.push(Effect::ClearNext);
            }
            fx.push(Effect::Emit(PlayerEvent::PrefetchCleared { item: p.item, reason }));
        }
    }

    /// The entry the gapless slot should hold. A repeat of the current entry
    /// (repeat-one, or repeat-all over one track) goes into the slot under
    /// a qid of its own for this pass (`pass_qid`): the engine won't arm the
    /// qid it is playing, and `track-advanced` must tell the passes apart.
    /// Repeats are prefetched too.
    fn predicted_next(&self) -> Option<QueueItem> {
        Some(match self.queue.peek_next()? {
            NextUp::Next(item) | NextUp::Wrap(item) => item,
            NextUp::Again(item) => QueueItem { qid: pass_qid(&item.qid, self.track_seq), ..item },
        })
    }

    /// Run on every tick and queue change: keep the slot on the predicted
    /// next track, and start resolving
    /// it once the current track is within the prefetch window of its end.
    fn maybe_prefetch(&mut self, fx: &mut Vec<Effect>) {
        // First: the returns below are exactly the case the radio is for.
        self.maybe_fetch_radio(fx);
        if !matches!(self.state, PlaybackState::Playing | PlaybackState::Paused) {
            return;
        }
        let gapless = self.config.gapless && !self.output_changed;
        let predicted = if gapless { self.predicted_next() } else { None };
        if let Some(p) = &self.prefetch {
            if predicted.as_ref().is_some_and(|n| n.qid == p.item.qid) {
                return; // already resolving or armed for the right track
            }
            self.clear_prefetch("the next track changed", fx);
        }
        let Some(next) = predicted else { return };
        if self
            .failed
            .as_ref()
            .is_some_and(|(qid, until)| *qid == next.qid && self.now < *until)
        {
            return;
        }
        let remaining = self.remaining();
        // Unknown length: resolve now rather than risk missing the boundary.
        if remaining.is_some_and(|r| r > self.config.prefetch_window) {
            return;
        }
        self.prefetch_gen += 1;
        self.prefetch = Some(Prefetch { item: next.clone(), slot: Slot::Resolving });
        fx.push(Effect::Emit(PlayerEvent::PrefetchStarted { item: next.clone(), remaining }));
        fx.push(Effect::ResolveNext {
            prefetch: self.prefetch_gen,
            item: next,
            use_track_gain: self.use_track_gain,
            normalization: self.config.normalization,
            quality: self.config.max_quality.clone(),
        });
    }

    fn next_resolved(&mut self, prefetch: u64, result: Result<Resolved, ResolveError>, fx: &mut Vec<Effect>) {
        if prefetch != self.prefetch_gen {
            return; // superseded
        }
        let Some(p) = self.prefetch.as_mut() else { return };
        if !matches!(p.slot, Slot::Resolving) {
            return;
        }
        match result {
            Ok(r) => {
                self.failed = None;
                if let Some(reason) = self.gapless_mismatch(&r.format) {
                    // Leave the slot in place so it isn't resolved again; the
                    // track starts fresh at the boundary (and is checked
                    // against the device then).
                    let p = self.prefetch.as_mut().expect("checked above");
                    p.slot = Slot::NotArmed;
                    let item = p.item.clone();
                    fx.push(Effect::Emit(PlayerEvent::PrefetchNotArmed { item, reason }));
                    return;
                }
                let p = self.prefetch.as_mut().expect("checked above");
                fx.push(Effect::ArmNext {
                    uri: r.uri.clone(),
                    norm_gain: r.norm_gain,
                    track_id: p.item.track_id,
                    qid: p.item.qid.clone(),
                    replay_gain: r.replay_gain,
                    peak_amplitude: r.peak_amplitude,
                    is_dash: r.is_dash,
                });
                let item = p.item.clone();
                let summary = r.summary.clone();
                p.slot = Slot::Armed(r);
                let remaining = self.remaining();
                fx.push(Effect::Emit(PlayerEvent::PrefetchArmed { item, remaining, summary }));
            }
            Err(e) => {
                let item = self.prefetch.take().expect("checked above").item;
                self.failed = Some((item.qid.clone(), self.now + FAILURE_MEMO_SECS));
                fx.push(Effect::Emit(PlayerEvent::PrefetchFailed { item, error: e.message }));
            }
        }
    }

    fn play_resolved(&mut self, load: u64, result: Result<Resolved, ResolveError>, fx: &mut Vec<Effect>) {
        if load != self.load {
            return;
        }
        if self.loading.is_none() {
            return;
        }
        match result {
            Ok(r) => {
                if let Some(error) = self.unsupported_by_device(&r.format) {
                    // Never started, so never announced: stop as the engine would.
                    self.halt(fx);
                    fx.push(Effect::Emit(PlayerEvent::Error { kind: ErrorKind::UnsupportedRate, message: error }));
                    return;
                }
                let start = self.loading.as_ref().and_then(|l| l.start_at);
                fx.push(Effect::Play { load, uri: r.uri.clone(), norm_gain: r.norm_gain, start });
                let target = self.output_target();
                let loading = self.loading.as_mut().expect("checked above");
                loading.resolved = Some(r);
                loading.output = self.output_gen;
                loading.target = target;
            }
            Err(e) => {
                let loading = self.loading.take().expect("checked above");
                self.load_failed(loading.item, loading.restored_at, e, fx);
            }
        }
    }

    /// The skip loop: an unplayable track is skipped, up to
    /// `MAX_CONSECUTIVE_PLAY_FAILS` in a row; anything else stops playback.
    fn load_failed(&mut self, item: QueueItem, restored_at: Option<f64>, e: ResolveError, fx: &mut Vec<Effect>) {
        if e.kind == ErrorKind::Unplayable {
            self.consecutive_fails += 1;
            fx.push(Effect::Emit(PlayerEvent::Skipped { item, error: e.message }));
            if self.consecutive_fails < MAX_CONSECUTIVE_PLAY_FAILS {
                match self.queue.advance(true) {
                    Advance::Next(next) | Advance::Wrapped(next) | Advance::Same(next) => {
                        return self.start_load(next, Transition::Skip, fx);
                    }
                    Advance::End => return self.queue_ended(Transition::Skip, fx),
                }
            }
            self.consecutive_fails = 0;
            self.stop(fx);
            fx.push(Effect::Emit(PlayerEvent::Error {
                kind: ErrorKind::Unplayable,
                message: "multiple tracks failed to play — stopped".into(),
            }));
            return;
        }
        self.stop_at(restored_at, fx);
        fx.push(Effect::Emit(PlayerEvent::Error { kind: e.kind, message: e.message }));
    }

    fn play_started(&mut self, load: u64, result: Result<(), String>, fx: &mut Vec<Effect>) {
        if load != self.load {
            return;
        }
        let Some(loading) = self.loading.take() else { return };
        match result {
            Ok(()) => {
                let r = loading.resolved.expect("PlayStarted follows PlayResolved");
                self.consecutive_fails = 0;
                self.position = loading.start_at.unwrap_or(0.0);
                self.track_seq += 1;
                // A pick that came in while this track loaded still applies
                // from the next one.
                self.output_changed = loading.output != self.output_gen;
                self.current = Some(Current {
                    duration: r.duration,
                    format: Some(r.format.clone()),
                    replay_gain: r.replay_gain,
                    peak_amplitude: r.peak_amplitude,
                });
                self.set_state(PlaybackState::Playing, fx);
                if self.active.as_ref() != Some(&loading.target) {
                    let (exclusive, device) = loading.target.clone();
                    self.active = Some(loading.target);
                    fx.push(Effect::Emit(PlayerEvent::OutputActive { exclusive, device }));
                }
                let (index, len) = self.queue.position();
                fx.push(Effect::Emit(PlayerEvent::TrackStarted {
                    item: loading.item,
                    index,
                    len,
                    via: loading.via,
                    summary: Some(r.summary),
                    duration: r.duration,
                    format: Some(r.format),
                }));
                if loading.start_at.is_some() {
                    fx.push(Effect::Emit(PlayerEvent::Position(self.position)));
                }
                self.maybe_prefetch(fx);
            }
            Err(e) => {
                let kind = ErrorKind::of_engine("", &e);
                // An exclusive open that found the device busy.
                if kind == ErrorKind::DeviceBusy && loading.target.0 {
                    if loading.output == self.output_gen {
                        self.fall_back(loading, fx);
                    } else {
                        // The output changed since this Play was issued: try
                        // the newer one.
                        self.replay(loading, fx);
                    }
                    return;
                }
                self.stop_at(loading.restored_at, fx);
                fx.push(Effect::Emit(PlayerEvent::Error { kind, message: e }));
            }
        }
    }

    /// The output a `Play` issued now opens.
    fn output_target(&self) -> (bool, Option<String>) {
        (self.config.exclusive, self.device.clone())
    }

    /// Switch to the system default and play `loading` again there.
    fn fall_back(&mut self, loading: Loading, fx: &mut Vec<Effect>) {
        self.config.exclusive = false;
        self.config.bit_perfect = false;
        self.device_rates = None;
        self.output_gen += 1;
        let device = self.device.take();
        fx.push(Effect::ConfigureOutput { output: self.output_gen, exclusive: false, device: None, bit_perfect: false });
        self.replay(loading, fx);
        fx.push(Effect::Emit(PlayerEvent::OutputFellBack { device }));
    }

    /// Issue `loading`'s `Play` again, on the current output, and keep it
    /// as the load in flight. Stamping it with the current output means a
    /// busy answer to this `Play` falls back rather than replaying again.
    /// On a newer output, the stream is checked against that device first,
    /// as `play_resolved` does.
    fn replay(&mut self, mut loading: Loading, fx: &mut Vec<Effect>) {
        let r = loading.resolved.as_ref().expect("PlayStarted follows PlayResolved");
        if loading.output != self.output_gen
            && let Some(error) = self.unsupported_by_device(&r.format)
        {
            self.stop_at(loading.restored_at, fx);
            fx.push(Effect::Emit(PlayerEvent::Error { kind: ErrorKind::UnsupportedRate, message: error }));
            return;
        }
        self.load += 1;
        fx.push(Effect::Play { load: self.load, uri: r.uri.clone(), norm_gain: r.norm_gain, start: loading.start_at });
        loading.output = self.output_gen;
        loading.target = self.output_target();
        self.loading = Some(loading);
    }

    /// The engine is already playing the armed track; catch the queue up
    /// with it.
    fn track_advanced(&mut self, track_id: u64, qid: String, rg: f64, peak: f64, fx: &mut Vec<Effect>) {
        if !matches!(self.state, PlaybackState::Playing | PlaybackState::Paused) {
            // A load is replacing the pipeline anyway.
            log::debug!("[player] ignoring track-advanced to {track_id} while {:?}", self.state);
            return;
        }
        let slot = self.prefetch.take();
        let armed = match &slot {
            Some(Prefetch { item, slot: Slot::Armed(r) }) if item.qid == qid => Some(r.clone()),
            _ => None,
        };
        // Normally the armed track is the next one (or a repeat pass, or the
        // repeat-all wrap). The engine commits to a switch seconds before it
        // is heard, so the queue may have changed in between (ClearNext is a
        // no-op then): find the entry further on, or put it back after the
        // current one. Repeat changes in the window don't matter: the pass
        // or the wrap the engine took is followed as it is.
        let (item, via) = if let Some(again) = self.queue.current().filter(|c| is_pass_of(&qid, &c.qid)).cloned() {
            (again, Transition::Repeat)
        } else if let Some(item) = self.queue.advance_to_qid(&qid) {
            (item, Transition::Gapless)
        } else if self.queue.upcoming().next().is_none() && self.queue.first().is_some_and(|f| f.qid == qid) {
            (self.queue.wrap_to(&qid).expect("the entry is in the queue"), Transition::Wrap)
        } else {
            log::warn!("[player] track-advanced to {track_id} ({qid}), which left the queue; re-adding it");
            self.queue.play_next(track_id);
            let next = self.queue.upcoming().next().expect("just inserted").qid.clone();
            (self.queue.advance_to_qid(&next).expect("just inserted"), Transition::Gapless)
        };
        // A slot filled in that window names the successor of the track now
        // playing: keep it if it is still the next one, else clear it.
        match slot {
            Some(p) if p.item.qid != qid => {
                if self.predicted_next().is_some_and(|n| n.qid == p.item.qid) {
                    self.prefetch = Some(p);
                } else {
                    self.prefetch_gen += 1;
                    if matches!(p.slot, Slot::Armed(_)) {
                        fx.push(Effect::ClearNext);
                    }
                    fx.push(Effect::Emit(PlayerEvent::PrefetchCleared {
                        item: p.item,
                        reason: "it was armed for the track that just ended",
                    }));
                }
            }
            _ => self.prefetch_gen += 1,
        }
        self.position = 0.0;
        self.consecutive_fails = 0;
        self.track_seq += 1;
        self.current = Some(Current {
            duration: armed.as_ref().and_then(|r| r.duration),
            // Without the slot, the engine can only have switched to a track it
            // matched against the one before (bit-perfect); keep that format.
            format: armed
                .as_ref()
                .map(|r| r.format.clone())
                .or_else(|| self.current.as_ref().and_then(|c| c.format.clone())),
            replay_gain: rg,
            peak_amplitude: peak,
        });
        let (index, len) = self.queue.position();
        fx.push(Effect::Emit(PlayerEvent::TrackStarted {
            item,
            index,
            len,
            via,
            summary: armed.as_ref().map(|r| r.summary.clone()),
            duration: armed.as_ref().and_then(|r| r.duration),
            format: armed.map(|r| r.format),
        }));
        self.maybe_prefetch(fx);
    }

    /// The track ended with nothing prerolled, so the next one needs a new
    /// pipeline.
    fn track_finished(&mut self, fx: &mut Vec<Effect>) {
        if !matches!(self.state, PlaybackState::Playing | PlaybackState::Paused) {
            return; // stale: a load already replaced the pipeline
        }
        // If a slot was armed the engine didn't take it (bit-perfect format
        // mismatch, or it failed); it is gone with the pipeline.
        if let Some(p) = self.prefetch.take() {
            self.prefetch_gen += 1;
            fx.push(Effect::Emit(PlayerEvent::PrefetchCleared {
                item: p.item,
                reason: "the engine ended the track without switching to it",
            }));
        }
        match self.queue.advance(false) {
            Advance::Same(item) => self.start_load(item, Transition::Repeat, fx),
            Advance::Next(item) => self.start_load(item, Transition::AfterEnd, fx),
            Advance::Wrapped(item) => self.start_load(item, Transition::Wrap, fx),
            Advance::End => self.queue_ended(Transition::AfterEnd, fx),
        }
    }
}

/// An engine error from a stream URL TIDAL no longer honours (GStreamer's
/// souphttpsrc: "Forbidden (403), URL: …").
fn expired_url(message: &str) -> bool {
    message.contains("(403)")
}

/// The qid a repeat pass of `qid` is armed under; `seq` is the
/// `track_seq` of the pass before it, so consecutive passes differ.
fn pass_qid(qid: &str, seq: u64) -> String {
    format!("{qid}~{seq}")
}

/// `qid` names a repeat pass of the entry `base`.
fn is_pass_of(qid: &str, base: &str) -> bool {
    qid.strip_prefix(base).and_then(|rest| rest.strip_prefix('~')).is_some_and(|seq| seq.parse::<u64>().is_ok())
}

fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Origin, TrackInfo};

    fn resolved(tag: &str, duration: f64) -> Resolved {
        resolved_as(tag, duration, 24, 48000)
    }

    fn resolved_as(tag: &str, duration: f64, bits: u32, rate: u32) -> Resolved {
        Resolved {
            uri: format!("uri:{tag}"),
            norm_gain: 1.0,
            replay_gain: -7.5,
            peak_amplitude: 0.9,
            is_dash: true,
            duration: Some(duration),
            format: StreamFormat { codec: Some("FLAC".into()), bit_depth: Some(bits), sample_rate: Some(rate) },
            summary: tag.to_string(),
        }
    }

    /// Drives a Core and records its effects.
    struct Harness {
        core: Core,
        now: f64,
    }

    impl Harness {
        fn new() -> Self {
            Self { core: Core::new(Config::default(), 1), now: 0.0 }
        }

        fn send(&mut self, input: Input) -> Vec<Effect> {
            self.now += 0.1;
            self.core.handle(input, self.now)
        }

        fn cmd(&mut self, c: PlayerCommand) -> Vec<Effect> {
            self.send(Input::Command(c))
        }

        /// Complete a pending Resolve → Play → PlayStarted with the given length.
        fn finish_load(&mut self, fx: &[Effect], duration: f64) -> Vec<Effect> {
            self.finish_load_len(fx, Some(duration))
        }

        /// As `finish_load`; `None`: TIDAL gave no length.
        fn finish_load_len(&mut self, fx: &[Effect], duration: Option<f64>) -> Vec<Effect> {
            let (load, item) = fx
                .iter()
                .find_map(|e| match e {
                    Effect::Resolve { load, item, .. } => Some((*load, item.clone())),
                    _ => None,
                })
                .expect("a Resolve effect");
            let mut r = resolved(&item.track_id.to_string(), 0.0);
            r.duration = duration;
            let fx = self.send(Input::PlayResolved { load, result: Ok(r) });
            assert!(fx.iter().any(|e| matches!(e, Effect::Play { .. })));
            self.send(Input::PlayStarted { load, result: Ok(()) })
        }

        fn load(&mut self, tracks: &[u64], repeat: RepeatMode, album: bool) -> Vec<Effect> {
            let fx = self.cmd(PlayerCommand::Load {
                tracks: QueueTrack::from_ids(tracks),
                start: Some(0),
                album_mode: album,
                shuffle: false,
                repeat,
            });
            self.finish_load(&fx, 200.0)
        }

        fn current(&self) -> u64 {
            self.core.queue().current().unwrap().track_id
        }
    }

    fn resolve_next(fx: &[Effect]) -> Option<(u64, QueueItem)> {
        fx.iter().find_map(|e| match e {
            Effect::ResolveNext { prefetch, item, .. } => Some((*prefetch, item.clone())),
            _ => None,
        })
    }

    fn resolve(fx: &[Effect]) -> Option<QueueItem> {
        fx.iter().find_map(|e| match e {
            Effect::Resolve { item, .. } => Some(item.clone()),
            _ => None,
        })
    }

    fn started(fx: &[Effect]) -> Option<(u64, Transition)> {
        fx.iter().find_map(|e| match e {
            Effect::Emit(PlayerEvent::TrackStarted { item, via, .. }) => Some((item.track_id, *via)),
            _ => None,
        })
    }

    fn has(fx: &[Effect], want: &Effect) -> bool {
        fx.iter().any(|e| e == want)
    }

    #[test]
    fn load_plays_the_first_track() {
        let mut h = Harness::new();
        let fx = h.cmd(PlayerCommand::Load {
            tracks: QueueTrack::from_ids(&[1, 2, 3]),
            start: Some(1),
            album_mode: false,
            shuffle: false,
            repeat: RepeatMode::Off,
        });
        assert_eq!(resolve(&fx).unwrap().track_id, 2);
        assert!(has(&fx, &Effect::Emit(PlayerEvent::State(PlaybackState::Loading))));
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((2, Transition::Start)));
        assert_eq!(h.core.state(), PlaybackState::Playing);
    }

    #[test]
    fn album_mode_selects_album_gain() {
        let mut h = Harness::new();
        for (album, track_gain) in [(true, false), (false, true)] {
            let fx = h.cmd(PlayerCommand::Load {
                tracks: QueueTrack::from_ids(&[1, 2]),
                start: Some(0),
                album_mode: album,
                shuffle: false,
                repeat: RepeatMode::Off,
            });
            assert!(fx.iter().any(|e| matches!(e, Effect::Resolve { use_track_gain, .. } if *use_track_gain == track_gain)));
        }
    }

    #[test]
    fn next_and_previous_at_the_queue_edges() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        // Previous at the first track, near its start: restart it.
        let fx = h.cmd(PlayerCommand::Previous);
        assert!(has(&fx, &Effect::Seek(0.0)));
        assert!(resolve(&fx).is_none());
        // Next to the last track, then Next again: the queue ends.
        let fx = h.cmd(PlayerCommand::Next);
        assert_eq!(resolve(&fx).unwrap().track_id, 2);
        h.finish_load(&fx, 200.0);
        let fx = h.cmd(PlayerCommand::Next);
        assert!(has(&fx, &Effect::Stop));
        assert!(has(&fx, &Effect::Emit(PlayerEvent::QueueEnded)));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    #[test]
    fn previous_restarts_after_three_seconds_else_goes_back() {
        let mut h = Harness::new();
        let fx = h.cmd(PlayerCommand::Load {
            tracks: QueueTrack::from_ids(&[1, 2, 3]),
            start: Some(2),
            album_mode: false,
            shuffle: false,
            repeat: RepeatMode::Off,
        });
        h.finish_load(&fx, 200.0);
        h.send(Input::Tick { position: 12.0, track: h.core.track_seq() });
        let fx = h.cmd(PlayerCommand::Previous);
        assert!(has(&fx, &Effect::Seek(0.0)));
        assert_eq!(h.current(), 3);
        h.send(Input::Tick { position: 1.0, track: h.core.track_seq() });
        let fx = h.cmd(PlayerCommand::Previous);
        assert_eq!(resolve(&fx).unwrap().track_id, 2);
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((2, Transition::Previous)));
    }

    #[test]
    fn natural_end_without_prefetch_plays_next_then_stops() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        let fx = h.send(Input::TrackFinished);
        assert_eq!(resolve(&fx).unwrap().track_id, 2);
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((2, Transition::AfterEnd)));
        let fx = h.send(Input::TrackFinished);
        assert!(has(&fx, &Effect::Emit(PlayerEvent::QueueEnded)));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
        // A late track-finished after stopping does nothing.
        assert!(h.send(Input::TrackFinished).is_empty());
    }

    #[test]
    fn repeat_one_replays_and_skip_moves_on() {
        let mut h = Harness::new();
        h.load(&[1, 2, 3], RepeatMode::One, false);
        // An end without a switch (nothing armed, e.g. the resolve failed)
        // replays with a new pipeline.
        for _ in 0..2 {
            let fx = h.send(Input::TrackFinished);
            assert_eq!(resolve(&fx).unwrap().track_id, 1);
            let fx = h.finish_load(&fx, 200.0);
            assert_eq!(started(&fx), Some((1, Transition::Repeat)));
        }
        let fx = h.cmd(PlayerCommand::Next);
        assert_eq!(resolve(&fx).unwrap().track_id, 2);
    }

    #[test]
    fn repeat_all_wraps_after_the_last_track() {
        let mut h = Harness::new();
        h.load(&[1, 2, 3], RepeatMode::All, false);
        let mut seen = vec![h.current()];
        for _ in 0..4 {
            let fx = h.send(Input::TrackFinished);
            let fx = h.finish_load(&fx, 200.0);
            let (id, via) = started(&fx).unwrap();
            seen.push(id);
            if id == 1 {
                assert_eq!(via, Transition::Wrap);
            }
        }
        assert_eq!(seen, vec![1, 2, 3, 1, 2]);
        assert_eq!(h.core.state(), PlaybackState::Playing);
    }

    #[test]
    fn shuffle_order_is_stable_across_playback() {
        let mut h = Harness::new();
        let fx = h.cmd(PlayerCommand::Load {
            tracks: (1..=6).map(QueueTrack::from).collect(),
            start: Some(0),
            album_mode: false,
            shuffle: true,
            repeat: RepeatMode::Off,
        });
        h.finish_load(&fx, 200.0);
        let order: Vec<u64> = std::iter::once(h.current())
            .chain(h.core.queue().upcoming().map(|t| t.track_id))
            .collect();
        let mut played = vec![h.current()];
        loop {
            h.cmd(PlayerCommand::SetRepeat(RepeatMode::Off)); // must not reshuffle
            let fx = h.send(Input::TrackFinished);
            if resolve(&fx).is_none() {
                break;
            }
            let fx = h.finish_load(&fx, 200.0);
            played.push(started(&fx).unwrap().0);
        }
        assert_eq!(played, order);
    }

    /// The just-in-time gapless path: nothing before the window, resolve
    /// inside it, arm, then the engine's switch advances the queue.
    #[test]
    fn prefetch_resolves_inside_the_window_and_advances_gaplessly() {
        let mut h = Harness::new();
        h.load(&[1, 2, 3], RepeatMode::Off, true);
        assert!(resolve_next(&h.send(Input::Tick { position: 150.0, track: h.core.track_seq() })).is_none(), "50 s left");
        let fx = h.send(Input::Tick { position: 171.0, track: h.core.track_seq() });
        let (pf, item) = resolve_next(&fx).expect("29 s left: resolve");
        assert_eq!(item.track_id, 2);
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::PrefetchStarted { remaining: Some(r), .. }) if (*r - 29.0).abs() < 1e-9)));
        assert!(fx.iter().any(|e| matches!(e, Effect::ResolveNext { use_track_gain: false, .. })), "album gain");
        // A second tick doesn't resolve again (in-flight dedup).
        assert!(resolve_next(&h.send(Input::Tick { position: 172.0, track: h.core.track_seq() })).is_none());
        let fx = h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved("two", 180.0)) });
        assert!(fx.iter().any(|e| matches!(e, Effect::ArmNext { track_id: 2, qid, .. } if *qid == item.qid)));
        // The engine switches.
        let fx = h.send(Input::TrackAdvanced {
            track_id: 2,
            qid: item.qid.clone(),
            replay_gain: -3.0,
            peak_amplitude: 0.5,
        });
        assert_eq!(started(&fx), Some((2, Transition::Gapless)));
        assert_eq!(h.current(), 2);
        // The new track's length came with the armed slot: the next prefetch
        // waits for its window.
        assert!(resolve_next(&h.send(Input::Tick { position: 100.0, track: h.core.track_seq() })).is_none());
        assert_eq!(resolve_next(&h.send(Input::Tick { position: 151.0, track: h.core.track_seq() })).unwrap().1.track_id, 3);
    }

    #[test]
    fn queue_change_rebuilds_the_prefetch() {
        let mut h = Harness::new();
        h.load(&[1, 2, 3], RepeatMode::Off, false);
        let (pf, _) = resolve_next(&h.send(Input::Tick { position: 180.0, track: h.core.track_seq() })).unwrap();
        h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved("two", 180.0)) });
        // Insert a track to play next: the armed slot is cleared and the new
        // next track resolved at once (still inside the window).
        let fx = h.cmd(PlayerCommand::PlayNext(9.into()));
        assert!(has(&fx, &Effect::ClearNext));
        assert_eq!(resolve_next(&fx).unwrap().1.track_id, 9);
        // Removing it rebuilds again, back to track 2.
        let fx = h.cmd(PlayerCommand::RemoveUpcoming(0));
        assert!(!has(&fx, &Effect::ClearNext), "nothing armed yet, only resolving");
        let (pf2, item) = resolve_next(&fx).unwrap();
        assert_eq!(item.track_id, 2);
        // The stale result for 9 is ignored; the fresh one arms.
        assert!(h.send(Input::NextResolved { prefetch: pf2 - 1, result: Ok(resolved("nine", 1.0)) }).is_empty());
        let fx = h.send(Input::NextResolved { prefetch: pf2, result: Ok(resolved("two", 180.0)) });
        assert!(fx.iter().any(|e| matches!(e, Effect::ArmNext { track_id: 2, .. })));
        // Repeat-one: the slot moves to another pass of the current track.
        let fx = h.cmd(PlayerCommand::SetRepeat(RepeatMode::One));
        assert!(has(&fx, &Effect::ClearNext));
        let (_, again) = resolve_next(&fx).unwrap();
        assert_eq!(again.track_id, h.current());
    }

    #[test]
    fn seek_rebuilds_the_prefetch() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        let (pf, _) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved("two", 180.0)) });
        // Seeking back out of the window drops the armed slot…
        let fx = h.cmd(PlayerCommand::Seek(20.0));
        assert!(has(&fx, &Effect::Seek(20.0)));
        assert!(has(&fx, &Effect::ClearNext));
        assert!(resolve_next(&fx).is_none());
        // …a seek inside the window keeps nothing stale and resolves at once…
        let fx = h.cmd(PlayerCommand::Seek(190.0));
        assert_eq!(resolve_next(&fx).unwrap().1.track_id, 2);
        // …and a seek within the window leaves an in-flight resolve alone.
        let fx = h.cmd(PlayerCommand::Seek(192.0));
        assert!(!has(&fx, &Effect::ClearNext));
        assert!(resolve_next(&fx).is_none());
    }

    #[test]
    fn failed_prefetch_is_not_retried_for_ten_seconds() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        let (pf, _) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        let err = ResolveError { message: "429".into(), kind: ErrorKind::Other };
        let fx = h.send(Input::NextResolved { prefetch: pf, result: Err(err) });
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::PrefetchFailed { .. }))));
        assert!(resolve_next(&h.send(Input::Tick { position: 186.0, track: h.core.track_seq() })).is_none());
        h.now += 10.0;
        assert!(resolve_next(&h.send(Input::Tick { position: 196.0, track: h.core.track_seq() })).is_some());
    }

    #[test]
    fn unarmed_end_after_a_prefetch_falls_back_to_a_new_pipeline() {
        // Bit-perfect refuses a next track in another format: the engine ends
        // the track instead of switching, and the player plays it fresh.
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        let (pf, _) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved("two", 180.0)) });
        let fx = h.send(Input::TrackFinished);
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::PrefetchCleared { .. }))));
        assert_eq!(resolve(&fx).unwrap().track_id, 2);
        // The fresh play fails in the engine: a clean stop with the error.
        let fx = h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved("two", 180.0)) });
        assert!(fx.iter().any(|e| matches!(e, Effect::Play { .. })));
        let fx = h.send(Input::AudioError {
            kind: "format_change_failed".into(),
            message: Some("DAC doesn't support 96kHz — turn off bit-perfect mode".into()),
        });
        assert!(has(&fx, &Effect::Stop));
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::Error { kind: ErrorKind::UnsupportedRate, message: m }) if m.contains("96kHz"))));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    #[test]
    fn unplayable_tracks_are_skipped_up_to_three_times() {
        let mut h = Harness::new();
        let fx = h.cmd(PlayerCommand::Load {
            tracks: QueueTrack::from_ids(&[1, 2, 3, 4, 5]),
            start: Some(0),
            album_mode: false,
            shuffle: false,
            repeat: RepeatMode::Off,
        });
        let unplayable = || Err(ResolveError { message: "404".into(), kind: ErrorKind::Unplayable });
        let mut fx = fx;
        for want in [2, 3] {
            let load = h.core.load;
            assert!(resolve(&fx).is_some());
            fx = h.send(Input::PlayResolved { load, result: unplayable() });
            assert_eq!(resolve(&fx).unwrap().track_id, want);
        }
        let load = h.core.load;
        let fx = h.send(Input::PlayResolved { load, result: unplayable() });
        assert!(has(&fx, &Effect::Stop), "third failure in a row stops");
        // A network error stops at once.
        let fx = h.cmd(PlayerCommand::Load {
            tracks: QueueTrack::from_ids(&[7, 8]),
            start: Some(0),
            album_mode: false,
            shuffle: false,
            repeat: RepeatMode::Off,
        });
        assert!(resolve(&fx).is_some());
        let load = h.core.load;
        let fx = h.send(Input::PlayResolved {
            load,
            result: Err(ResolveError { message: "network".into(), kind: ErrorKind::Network }),
        });
        assert!(has(&fx, &Effect::Stop));
    }

    #[test]
    fn stale_results_are_ignored() {
        let mut h = Harness::new();
        let first = h.cmd(PlayerCommand::Load {
            tracks: QueueTrack::from_ids(&[1, 2]),
            start: Some(0),
            album_mode: false,
            shuffle: false,
            repeat: RepeatMode::Off,
        });
        let old = match first.iter().find(|e| matches!(e, Effect::Resolve { .. })) {
            Some(Effect::Resolve { load, .. }) => *load,
            _ => unreachable!(),
        };
        // The user skips before the first track resolved.
        let fx = h.cmd(PlayerCommand::Next);
        assert_eq!(resolve(&fx).unwrap().track_id, 2);
        assert!(h.send(Input::PlayResolved { load: old, result: Ok(resolved("one", 1.0)) }).is_empty());
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((2, Transition::Skip)));
    }

    #[test]
    fn track_advanced_during_a_load_is_ignored() {
        let mut h = Harness::new();
        h.load(&[1, 2, 3], RepeatMode::Off, false);
        let (pf, item) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved("two", 180.0)) });
        let fx = h.cmd(PlayerCommand::Next); // user skips while 2 is armed
        assert!(has(&fx, &Effect::ClearNext));
        let fx = h.send(Input::TrackAdvanced { track_id: 2, qid: item.qid, replay_gain: 0.0, peak_amplitude: 1.0 });
        assert!(fx.is_empty());
        assert_eq!(h.core.state(), PlaybackState::Loading);
    }

    /// The engine commits to a switch seconds before track-advanced reaches
    /// the player. A track queued to play next in that window must still play
    /// next, and the slot armed for it (the switching track's successor in
    /// the engine) must survive the advance.
    #[test]
    fn play_next_during_the_switch_window_is_kept() {
        let mut h = Harness::new();
        h.load(&[1, 2, 3], RepeatMode::Off, false);
        let (pf, two) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved("two", 180.0)) });
        // (concat switches to 2 here; the player doesn't know yet)
        let fx = h.cmd(PlayerCommand::PlayNext(9.into()));
        let (pf9, nine) = resolve_next(&fx).unwrap();
        assert_eq!(nine.track_id, 9);
        h.send(Input::NextResolved { prefetch: pf9, result: Ok(resolved("nine", 100.0)) });
        let fx = h.send(Input::TrackAdvanced { track_id: 2, qid: two.qid, replay_gain: 0.0, peak_amplitude: 1.0 });
        assert_eq!(started(&fx), Some((2, Transition::Gapless)));
        assert!(!has(&fx, &Effect::ClearNext), "9 is armed as 2's successor and still next");
        assert_eq!(h.core.queue().peek_next().unwrap().item().track_id, 9);
        assert_eq!(h.core.queue().upcoming().map(|t| t.track_id).collect::<Vec<_>>(), vec![9, 3]);
        // Nothing to resolve: 9 is armed.
        assert!(resolve_next(&h.send(Input::Tick { position: 170.0, track: h.core.track_seq() })).is_none());
    }

    #[test]
    fn a_slot_armed_in_the_window_for_another_track_is_cleared() {
        let mut h = Harness::new();
        h.load(&[1, 2, 3], RepeatMode::Off, false);
        let (pf, two) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved("two", 180.0)) });
        // In the window: 9 queued and armed, then removed again (3 is armed).
        let (pf9, _) = resolve_next(&h.cmd(PlayerCommand::PlayNext(9.into()))).unwrap();
        h.send(Input::NextResolved { prefetch: pf9, result: Ok(resolved("nine", 100.0)) });
        h.cmd(PlayerCommand::RemoveUpcoming(0)); // removes 9; 2 is next again
        let fx = h.send(Input::TrackAdvanced { track_id: 2, qid: two.qid, replay_gain: 0.0, peak_amplitude: 1.0 });
        assert_eq!(started(&fx), Some((2, Transition::Gapless)));
        assert_eq!(h.core.queue().peek_next().unwrap().item().track_id, 3);
    }

    #[test]
    fn a_position_poll_from_the_previous_track_is_dropped() {
        let mut h = Harness::new();
        h.load(&[1, 2, 3], RepeatMode::Off, false);
        let old = h.core.track_seq();
        let (pf, two) = resolve_next(&h.send(Input::Tick { position: 185.0, track: old })).unwrap();
        h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved("two", 180.0)) });
        h.send(Input::TrackAdvanced { track_id: 2, qid: two.qid, replay_gain: 0.0, peak_amplitude: 1.0 });
        // A poll sent before the advance reports the old track's position.
        let fx = h.send(Input::Tick { position: 199.0, track: old });
        assert!(resolve_next(&fx).is_none(), "3 must not be prefetched at the start of 2");
    }

    #[test]
    fn normalization_toggle_uses_the_current_tracks_gain() {
        let mut h = Harness::new();
        h.load(&[1], RepeatMode::Off, false);
        let fx = h.cmd(PlayerCommand::SetNormalization(true));
        let want = compute_norm_gain(Some(-7.5), Some(0.9));
        assert!(has(&fx, &Effect::SetNormGain(want)));
        let fx = h.cmd(PlayerCommand::SetNormalization(false));
        assert!(has(&fx, &Effect::SetNormGain(1.0)));
    }

    #[test]
    fn pause_and_resume() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        let fx = h.cmd(PlayerCommand::Pause);
        assert!(has(&fx, &Effect::Pause));
        assert_eq!(h.core.state(), PlaybackState::Paused);
        assert!(h.cmd(PlayerCommand::Pause).is_empty());
        // The slot may still be armed while paused.
        assert!(resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).is_some());
        let fx = h.cmd(PlayerCommand::Resume);
        assert!(has(&fx, &Effect::Resume));
        assert_eq!(h.core.state(), PlaybackState::Playing);
    }

    /// The engine's error for an expired segment URL.
    fn forbidden() -> Input {
        Input::AudioError {
            kind: "playback_error".into(),
            message: Some("Forbidden: gst_soup_http_src_parse_status (): Forbidden (403), URL: https://x/y".into()),
        }
    }

    /// The Resolve in `fx` for the current track, answered; returns `Play`'s start.
    fn reloaded_from(h: &mut Harness, fx: &[Effect]) -> Option<f64> {
        let (load, item) = fx
            .iter()
            .find_map(|e| match e {
                Effect::Resolve { load, item, .. } => Some((*load, item.clone())),
                _ => None,
            })
            .expect("resolves the track again");
        assert_eq!(item.track_id, h.current());
        let fx = h.send(Input::PlayResolved { load, result: Ok(resolved("again", 200.0)) });
        let start = fx.iter().find_map(|e| match e {
            Effect::Play { start, .. } => Some(*start),
            _ => None,
        });
        let fx = h.send(Input::PlayStarted { load, result: Ok(()) });
        assert_eq!(started(&fx).map(|(_, via)| via), Some(Transition::Reload));
        start.expect("a Play")
    }

    #[test]
    fn a_long_pause_reloads_the_track_where_it_was() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        h.send(Input::Tick { position: 120.0, track: h.core.track_seq() });
        h.cmd(PlayerCommand::Pause);
        h.now += 599.0;
        let fx = h.cmd(PlayerCommand::Resume);
        assert!(has(&fx, &Effect::Resume), "a short pause just resumes");
        h.cmd(PlayerCommand::Pause);
        h.now += 600.0;
        let fx = h.cmd(PlayerCommand::TogglePause);
        assert!(!has(&fx, &Effect::Resume));
        assert_eq!(h.core.state(), PlaybackState::Loading);
        assert_eq!(h.core.saved_position(), 120.0, "a quit while reloading keeps the position");
        assert_eq!(reloaded_from(&mut h, &fx), Some(120.0));
        assert_eq!(h.core.state(), PlaybackState::Playing);
        assert_eq!(h.current(), 1);
    }

    #[test]
    fn a_failed_reload_keeps_the_place() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        h.send(Input::Tick { position: 120.0, track: h.core.track_seq() });
        h.cmd(PlayerCommand::Pause);
        h.now += 3600.0;
        let fx = h.cmd(PlayerCommand::Resume);
        let (load, _) = fx.iter().find_map(|e| match e {
            Effect::Resolve { load, item, .. } => Some((*load, item.clone())),
            _ => None,
        }).unwrap();
        let fx = h.send(Input::PlayResolved {
            load,
            result: Err(ResolveError { message: "offline".into(), kind: ErrorKind::Network }),
        });
        assert_eq!(errors(&fx), [ErrorKind::Network]);
        assert_eq!(h.core.state(), PlaybackState::Restored);
        assert_eq!(h.core.saved_position(), 120.0);
    }

    #[test]
    fn an_expired_stream_is_reloaded_once() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        h.send(Input::Tick { position: 80.0, track: h.core.track_seq() });
        let fx = h.send(forbidden());
        assert!(errors(&fx).is_empty(), "no error shown");
        assert_eq!(reloaded_from(&mut h, &fx), Some(80.0));
        // Still forbidden with fresh URLs: stop.
        h.send(Input::Tick { position: 85.0, track: h.core.track_seq() });
        let fx = h.send(forbidden());
        assert_eq!(errors(&fx), [ErrorKind::Other]);
        assert_eq!(h.core.state(), PlaybackState::Stopped);
        // Much later, another expiry is reloaded again.
        let fx = h.cmd(PlayerCommand::Resume);
        h.finish_load(&fx, 200.0);
        h.now += RELOAD_MEMO_SECS;
        let fx = h.send(forbidden());
        assert!(errors(&fx).is_empty());
        assert_eq!(h.core.state(), PlaybackState::Loading);
    }

    /// A bit-perfect player on a 48 kHz-only device (the target laptop).
    fn bit_perfect_harness() -> Harness {
        let mut h = Harness::new();
        let fx = h.cmd(PlayerCommand::SetOutput { exclusive: true, device: Some("hw:CARD=x,DEV=0".into()), bit_perfect: true });
        assert!(fx.iter().any(|e| matches!(e, Effect::ConfigureOutput { exclusive: true, bit_perfect: true, .. })));
        h.send(Input::DeviceRates { output: h.core.output_gen, rates: Some(vec![48000]) });
        h
    }

    fn announced(fx: &[Effect]) -> bool {
        fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::TrackStarted { .. })))
    }

    fn error(fx: &[Effect]) -> Option<&str> {
        fx.iter().find_map(|e| match e {
            Effect::Emit(PlayerEvent::Error { message: m, .. }) => Some(m.as_str()),
            _ => None,
        })
    }

    #[test]
    fn bit_perfect_refuses_a_rate_the_device_lacks_before_playing() {
        let mut h = bit_perfect_harness();
        let fx = h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1]), start: Some(0), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        assert!(resolve(&fx).is_some());
        let fx = h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved_as("one", 200.0, 24, 96000)) });
        assert!(!fx.iter().any(|e| matches!(e, Effect::Play { .. })), "never handed to the engine");
        assert!(!announced(&fx));
        assert_eq!(error(&fx), Some(unsupported_rate_error(96000).as_str()));
        assert!(has(&fx, &Effect::Stop));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    #[test]
    fn a_supported_rate_or_unknown_device_rates_play() {
        let mut h = bit_perfect_harness();
        h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1]), start: Some(0), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved_as("one", 200.0, 24, 48000)) });
        assert!(h.send(Input::PlayStarted { load: h.core.load, result: Ok(()) }).iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::TrackStarted { .. }))));
        // Busy device: rates unknown, the engine's own check is the backstop.
        let mut h = bit_perfect_harness();
        h.send(Input::DeviceRates { output: h.core.output_gen, rates: None });
        h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1]), start: Some(0), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        let fx = h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved_as("one", 200.0, 24, 96000)) });
        assert!(fx.iter().any(|e| matches!(e, Effect::Play { .. })));
        // Not bit-perfect: any rate plays (the engine resamples).
        let mut h = Harness::new();
        h.send(Input::DeviceRates { output: h.core.output_gen, rates: Some(vec![48000]) });
        h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1]), start: Some(0), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        let fx = h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved_as("one", 200.0, 24, 96000)) });
        assert!(fx.iter().any(|e| matches!(e, Effect::Play { .. })));
    }

    /// A 48 kHz → 96 kHz queue in bit-perfect mode stops
    /// cleanly and the 96 kHz track is never shown as playing.
    #[test]
    fn bit_perfect_48_to_96_stops_without_announcing_the_96k_track() {
        let mut h = bit_perfect_harness();
        h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1, 2]), start: Some(0), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved_as("one", 200.0, 24, 48000)) });
        let fx = h.send(Input::PlayStarted { load: h.core.load, result: Ok(()) });
        assert_eq!(started(&fx), Some((1, Transition::Start)));
        let (pf, item) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        let fx = h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved_as("two", 180.0, 24, 96000)) });
        assert!(!fx.iter().any(|e| matches!(e, Effect::ArmNext { .. })), "not armed");
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::PrefetchNotArmed { item: i, .. }) if *i == item)));
        // Not resolved again on later ticks.
        assert!(resolve_next(&h.send(Input::Tick { position: 190.0, track: h.core.track_seq() })).is_none());
        // Track 1 ends; track 2 is resolved fresh and refused before play.
        let fx = h.send(Input::TrackFinished);
        assert_eq!(resolve(&fx).unwrap().track_id, 2);
        assert!(!announced(&fx));
        let fx = h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved_as("two", 180.0, 24, 96000)) });
        assert!(!announced(&fx));
        assert!(!fx.iter().any(|e| matches!(e, Effect::Play { .. })));
        assert_eq!(error(&fx), Some(unsupported_rate_error(96000).as_str()));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    #[test]
    fn bit_perfect_arms_only_the_same_format() {
        // Same rate, other bit depth: not armed; it plays after the boundary.
        let mut h = bit_perfect_harness();
        h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1, 2, 3]), start: Some(0), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved_as("one", 200.0, 24, 48000)) });
        h.send(Input::PlayStarted { load: h.core.load, result: Ok(()) });
        let (pf, _) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        let fx = h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved_as("two", 180.0, 16, 48000)) });
        assert!(!fx.iter().any(|e| matches!(e, Effect::ArmNext { .. })));
        h.send(Input::TrackFinished);
        h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved_as("two", 180.0, 16, 48000)) });
        let fx = h.send(Input::PlayStarted { load: h.core.load, result: Ok(()) });
        assert_eq!(started(&fx), Some((2, Transition::AfterEnd)));
        // Same format as the (now 16-bit) current track: armed.
        let (pf, _) = resolve_next(&h.send(Input::Tick { position: 170.0, track: h.core.track_seq() })).unwrap();
        let fx = h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved_as("three", 180.0, 16, 48000)) });
        assert!(fx.iter().any(|e| matches!(e, Effect::ArmNext { track_id: 3, .. })));
    }

    #[test]
    fn default_mode_arms_across_formats() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        let (pf, _) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        let fx = h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved_as("two", 180.0, 16, 44100)) });
        assert!(fx.iter().any(|e| matches!(e, Effect::ArmNext { track_id: 2, .. })));
    }

    #[test]
    fn an_output_change_mid_track_starts_the_next_track_fresh() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        let (pf, _) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved("two", 180.0)) });
        let fx = h.cmd(PlayerCommand::SetOutput { exclusive: false, device: None, bit_perfect: false });
        assert!(has(&fx, &Effect::ClearNext));
        assert!(resolve_next(&fx).is_none());
        assert!(resolve_next(&h.send(Input::Tick { position: 190.0, track: h.core.track_seq() })).is_none());
        let fx = h.send(Input::TrackFinished);
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((2, Transition::AfterEnd)));
    }

    #[test]
    fn an_output_change_while_loading_starts_the_next_track_fresh() {
        let mut h = Harness::new();
        h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1, 2, 3]), start: Some(0), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved("1", 200.0)) });
        // Picked while the Play for the old output is in flight.
        h.cmd(PlayerCommand::SetOutput { exclusive: false, device: None, bit_perfect: false });
        h.send(Input::PlayStarted { load: h.core.load, result: Ok(()) });
        assert_eq!(h.core.state(), PlaybackState::Playing);
        assert!(resolve_next(&h.send(Input::Tick { position: 190.0, track: h.core.track_seq() })).is_none());
        let fx = h.send(Input::TrackFinished);
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((2, Transition::AfterEnd)));
        // That one opened the new output: the track after it may go gapless.
        assert!(resolve_next(&h.send(Input::Tick { position: 190.0, track: h.core.track_seq() })).is_some());
    }

    #[test]
    fn queue_changes_are_published() {
        let mut h = Harness::new();
        let fx = h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1, 2, 3]), start: Some(1), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        let queue = |fx: &[Effect]| {
            fx.iter().find_map(|e| match e {
                Effect::Emit(PlayerEvent::QueueChanged { items, current, repeat, .. }) => {
                    Some((items.iter().map(|t| t.track_id).collect::<Vec<_>>(), *current, *repeat))
                }
                _ => None,
            })
        };
        assert_eq!(queue(&fx), Some((vec![1, 2, 3], 1, RepeatMode::Off)));
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(queue(&fx), None, "unchanged");
        assert_eq!(queue(&h.cmd(PlayerCommand::SetRepeat(RepeatMode::All))), Some((vec![1, 2, 3], 1, RepeatMode::All)));
        // A click in the queue list plays that entry.
        let first = h.core.queue().in_order().next().unwrap().qid.clone();
        let fx = h.cmd(PlayerCommand::JumpTo(first));
        assert_eq!(resolve(&fx).unwrap().track_id, 1);
        assert_eq!(queue(&fx), Some((vec![1, 2, 3], 0, RepeatMode::All)));
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((1, Transition::Jump)));
    }

    #[test]
    fn toggle_pause_seek_by_and_resume_after_stop() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        assert!(has(&h.cmd(PlayerCommand::TogglePause), &Effect::Pause));
        assert!(has(&h.cmd(PlayerCommand::TogglePause), &Effect::Resume));
        h.send(Input::Tick { position: 10.0, track: h.core.track_seq() });
        let fx = h.cmd(PlayerCommand::SeekBy(5.0));
        assert!(has(&fx, &Effect::Seek(15.0)));
        assert!(has(&fx, &Effect::Emit(PlayerEvent::Position(15.0))));
        // Past the end skips.
        assert_eq!(resolve(&h.cmd(PlayerCommand::SeekBy(500.0))).unwrap().track_id, 2);
        let fx = h.cmd(PlayerCommand::Stop);
        assert!(has(&fx, &Effect::Stop));
        // Play when stopped starts the current track again.
        assert_eq!(resolve(&h.cmd(PlayerCommand::TogglePause)).unwrap().track_id, 2);
    }

    #[test]
    fn bit_perfect_treats_unknown_or_other_codecs_as_different() {
        let mut h = bit_perfect_harness();
        h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1, 2]), start: Some(0), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved_as("one", 200.0, 24, 48000)) });
        h.send(Input::PlayStarted { load: h.core.load, result: Ok(()) });
        let (pf, _) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        // An AAC fallback without a bit depth at the same rate.
        let mut aac = resolved_as("two", 180.0, 24, 48000);
        aac.format = StreamFormat { codec: Some("mp4a.40.2".into()), bit_depth: None, sample_rate: Some(48000) };
        let fx = h.send(Input::NextResolved { prefetch: pf, result: Ok(aac) });
        assert!(!fx.iter().any(|e| matches!(e, Effect::ArmNext { .. })));
    }

    #[test]
    fn stale_or_empty_device_rates_are_ignored() {
        let mut h = bit_perfect_harness();
        let old = h.core.output_gen;
        h.cmd(PlayerCommand::SetOutput { exclusive: true, device: Some("hw:CARD=y,DEV=0".into()), bit_perfect: true });
        // The previous device's answer arrives late.
        h.send(Input::DeviceRates { output: old, rates: Some(vec![48000]) });
        assert_eq!(h.core.device_rates, None);
        h.send(Input::DeviceRates { output: h.core.output_gen, rates: Some(vec![]) });
        assert_eq!(h.core.device_rates, None, "an empty probe is unknown");
    }

    const DEV: &str = "hw:CARD=x,DEV=0";

    /// An exclusive harness on `device`.
    fn exclusive_harness(device: Option<&str>) -> Harness {
        let mut h = Harness::new();
        h.cmd(PlayerCommand::SetOutput { exclusive: true, device: device.map(Into::into), bit_perfect: false });
        h
    }

    /// Load `tracks` and resolve the first; the `Play` is then in flight.
    fn load_to_play(h: &mut Harness, tracks: &[u64]) {
        h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(tracks), start: Some(0), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        let fx = h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved("1", 200.0)) });
        assert_eq!(plays(&fx), 1);
    }

    fn busy(h: &mut Harness) -> Vec<Effect> {
        h.send(Input::PlayStarted { load: h.core.load, result: Err("device_busy".into()) })
    }

    fn plays(fx: &[Effect]) -> usize {
        fx.iter().filter(|e| matches!(e, Effect::Play { .. })).count()
    }

    fn errors(fx: &[Effect]) -> Vec<ErrorKind> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Emit(PlayerEvent::Error { kind, .. }) => Some(*kind),
                _ => None,
            })
            .collect()
    }

    fn fell_back(fx: &[Effect]) -> Vec<Option<String>> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Emit(PlayerEvent::OutputFellBack { device }) => Some(device.clone()),
                _ => None,
            })
            .collect()
    }

    fn active(fx: &[Effect]) -> Vec<(bool, Option<String>)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Emit(PlayerEvent::OutputActive { exclusive, device }) => Some((*exclusive, device.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_busy_exclusive_open_falls_back_to_system_default() {
        let mut h = exclusive_harness(Some(DEV));
        let old = h.core.output_gen;
        load_to_play(&mut h, &[1, 2]);
        let fx = busy(&mut h);
        let configure = fx
            .iter()
            .position(|e| *e == Effect::ConfigureOutput { output: old + 1, exclusive: false, device: None, bit_perfect: false })
            .expect("switches to the system default");
        let play = fx
            .iter()
            .position(|e| matches!(e, Effect::Play { uri, start: None, .. } if uri == "uri:1"))
            .expect("plays the same track again");
        assert!(configure < play, "configured before the replay");
        assert_eq!(plays(&fx), 1);
        assert_eq!(fell_back(&fx), [Some(DEV.to_string())]);
        assert!(errors(&fx).is_empty());
        assert_eq!(h.core.output_gen, old + 1);
        assert!(!h.core.config.exclusive && h.core.device.is_none());
        // The busy device's rates, arriving late, are not taken.
        h.send(Input::DeviceRates { output: old, rates: Some(vec![48000]) });
        assert_eq!(h.core.device_rates, None);
        assert!(h.core.loading.as_ref().unwrap().resolved.is_some());
        assert_eq!(h.core.state(), PlaybackState::Loading);

        let fx = h.send(Input::PlayStarted { load: h.core.load, result: Ok(()) });
        assert_eq!(h.core.state(), PlaybackState::Playing);
        assert_eq!(started(&fx), Some((1, Transition::Start)));
    }

    #[test]
    fn a_busy_open_after_a_newer_pick_does_not_fall_back() {
        let mut h = exclusive_harness(Some(DEV));
        load_to_play(&mut h, &[1, 2]);
        // Picked while the first Play was in flight.
        h.cmd(PlayerCommand::SetOutput { exclusive: true, device: Some("hw:CARD=y,DEV=0".into()), bit_perfect: false });
        let output = h.core.output_gen;
        let fx = busy(&mut h);
        assert_eq!(plays(&fx), 1, "plays again on the newer pick");
        assert!(!fx.iter().any(|e| matches!(e, Effect::ConfigureOutput { .. })));
        assert!(fell_back(&fx).is_empty() && errors(&fx).is_empty());
        assert_eq!(h.core.output_gen, output);
        assert!(h.core.loading.as_ref().unwrap().resolved.is_some());

        let fx = h.send(Input::PlayStarted { load: h.core.load, result: Ok(()) });
        assert_eq!(h.core.state(), PlaybackState::Playing);
        assert_eq!(active(&fx), [(true, Some("hw:CARD=y,DEV=0".to_string()))]);
    }

    #[test]
    fn a_busy_newer_pick_that_is_also_busy_falls_back_once() {
        let mut h = exclusive_harness(Some(DEV));
        load_to_play(&mut h, &[1, 2]);
        h.cmd(PlayerCommand::SetOutput { exclusive: true, device: Some("hw:CARD=y,DEV=0".into()), bit_perfect: false });
        let mut total = 1;
        let fx = busy(&mut h);
        total += plays(&fx);
        assert!(fell_back(&fx).is_empty());
        let fx = busy(&mut h);
        total += plays(&fx);
        assert_eq!(fell_back(&fx), [Some("hw:CARD=y,DEV=0".to_string())]);
        // Even the system default is busy: that is an error, not another try.
        let fx = busy(&mut h);
        total += plays(&fx);
        assert_eq!(errors(&fx), [ErrorKind::DeviceBusy]);
        assert!(fell_back(&fx).is_empty());
        assert_eq!(total, 3);
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    #[test]
    fn a_busy_open_after_a_newer_bit_perfect_pick_checks_its_rates() {
        let mut h = exclusive_harness(Some(DEV));
        load_to_play(&mut h, &[1, 2]);
        h.cmd(PlayerCommand::SetOutput { exclusive: true, device: Some("hw:CARD=y,DEV=0".into()), bit_perfect: true });
        h.send(Input::DeviceRates { output: h.core.output_gen, rates: Some(vec![44100]) });
        // The track is 48 kHz, which the newer device lacks.
        let fx = busy(&mut h);
        assert_eq!(plays(&fx), 0);
        assert_eq!(errors(&fx), [ErrorKind::UnsupportedRate]);
        assert!(fell_back(&fx).is_empty());
        assert!(has(&fx, &Effect::Stop));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    #[test]
    fn exclusive_without_a_device_falls_back_with_none() {
        let mut h = exclusive_harness(None);
        load_to_play(&mut h, &[1]);
        let fx = busy(&mut h);
        assert_eq!(fell_back(&fx), [None]);
        assert_eq!(plays(&fx), 1);
        assert!(errors(&fx).is_empty());
    }

    #[test]
    fn a_busy_open_resuming_a_restored_track_keeps_its_position() {
        let mut h = exclusive_harness(Some(DEV));
        h.cmd(PlayerCommand::Restore(Box::new(saved_queue(42_000))));
        h.cmd(PlayerCommand::TogglePause);
        h.send(Input::PlayResolved { load: h.core.load, result: Ok(resolved("2", 200.0)) });
        let fx = busy(&mut h);
        assert!(fx.iter().any(|e| matches!(e, Effect::Play { start: Some(s), .. } if (*s - 42.0).abs() < 1e-9)));
        assert_eq!(fell_back(&fx), [Some(DEV.to_string())]);
        let loading = h.core.loading.as_ref().unwrap();
        assert!(loading.resolved.is_some());
        assert_eq!(loading.restored_at, Some(42.0));
        assert_eq!(h.core.persisted().unwrap().position_ms, 42_000);

        h.send(Input::PlayStarted { load: h.core.load, result: Ok(()) });
        assert_eq!(h.core.state(), PlaybackState::Playing);
        assert_eq!(h.core.position, 42.0);
    }

    #[test]
    fn a_busy_open_on_system_default_is_an_error() {
        let mut h = Harness::new();
        load_to_play(&mut h, &[1, 2]);
        let fx = busy(&mut h);
        assert_eq!(errors(&fx), [ErrorKind::DeviceBusy]);
        assert_eq!(plays(&fx), 0);
        assert!(fell_back(&fx).is_empty());
        assert!(!fx.iter().any(|e| matches!(e, Effect::ConfigureOutput { .. })));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    #[test]
    fn output_active_changes_only_when_the_next_track_starts() {
        let mut h = Harness::new();
        let fx = h.load(&[1, 2, 3, 4], RepeatMode::Off, false);
        assert_eq!(active(&fx), [(false, None)]);
        // Another track on the same output reports nothing.
        let fx = h.cmd(PlayerCommand::Next);
        assert!(active(&h.finish_load(&fx, 200.0)).is_empty());
        // A pick applies from the next track.
        let fx = h.cmd(PlayerCommand::SetOutput { exclusive: true, device: Some(DEV.into()), bit_perfect: false });
        assert!(active(&fx).is_empty());
        let fx = h.send(Input::TrackFinished);
        assert!(active(&fx).is_empty());
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(active(&fx), [(true, Some(DEV.to_string()))]);
        // A gapless switch opens nothing.
        let (pf, next) = resolve_next(&h.send(Input::Tick { position: 185.0, track: h.core.track_seq() })).unwrap();
        h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved("4", 200.0)) });
        let fx = h.send(Input::TrackAdvanced { track_id: next.track_id, qid: next.qid, replay_gain: -7.5, peak_amplitude: 0.9 });
        assert_eq!(started(&fx), Some((4, Transition::Gapless)));
        assert!(active(&fx).is_empty());
    }

    #[test]
    fn a_fallback_reports_system_default_as_active() {
        let mut h = exclusive_harness(Some(DEV));
        load_to_play(&mut h, &[1]);
        let fx = busy(&mut h);
        assert!(active(&fx).is_empty(), "nothing played on the device");
        let fx = h.send(Input::PlayStarted { load: h.core.load, result: Ok(()) });
        assert_eq!(active(&fx), [(false, None)]);
    }

    #[test]
    fn a_device_disconnected_error_stops_the_engine() {
        let mut h = exclusive_harness(Some(DEV));
        h.load(&[1, 2], RepeatMode::Off, false);
        assert_eq!(h.core.state(), PlaybackState::Playing);
        let fx = h.send(Input::AudioError { kind: "device_disconnected".into(), message: None });
        assert!(has(&fx, &Effect::Stop));
        assert_eq!(errors(&fx), [ErrorKind::Device]);
        assert!(fell_back(&fx).is_empty());
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    #[test]
    fn play_pause_while_loading_stops() {
        let mut h = Harness::new();
        h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1]), start: Some(0), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        let fx = h.cmd(PlayerCommand::TogglePause);
        assert!(has(&fx, &Effect::Stop));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    fn saved_queue(position_ms: u64) -> PersistedQueue {
        use crate::persist::SavedTrack;
        let info = |title: &str| TrackInfo { title: title.into(), duration: Some(200.0), ..TrackInfo::default() };
        PersistedQueue {
            tracks: vec![
                SavedTrack { id: 1, info: Some(info("One")), origin: Origin::Queued },
                SavedTrack { id: 2, info: Some(info("Two")), origin: Origin::Queued },
                SavedTrack { id: 3, info: None, origin: Origin::Radio { seed: 2 } },
            ],
            shuffle_order: None,
            active_index: 1,
            position_ms,
            repeat: RepeatMode::All,
            album_mode: true,
        }
    }

    #[test]
    fn a_restored_queue_waits_paused_without_touching_the_engine() {
        let mut h = Harness::new();
        let fx = h.cmd(PlayerCommand::Restore(Box::new(saved_queue(93_250))));
        assert_eq!(h.core.state(), PlaybackState::Restored);
        assert!(resolve(&fx).is_none(), "nothing resolved");
        assert!(!fx.iter().any(|e| matches!(e, Effect::Play { .. } | Effect::Pause | Effect::Seek(_))));
        let started = fx.iter().find_map(|e| match e {
            Effect::Emit(PlayerEvent::TrackStarted { item, index, via, duration, .. }) => Some((item.track_id, *index, *via, *duration)),
            _ => None,
        });
        assert_eq!(started, Some((2, 1, Transition::Restore, Some(200.0))), "shown with its saved metadata");
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::Position(p)) if (*p - 93.25).abs() < 1e-9)));
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::QueueChanged { items, current: 1, repeat: RepeatMode::All, .. }) if items.len() == 3)));
        // No prefetch while nothing plays, even inside the window.
        assert!(resolve_next(&h.send(Input::Tick { position: 190.0, track: h.core.track_seq() })).is_none());
        // Saved again as it came back.
        let again = h.core.persisted().unwrap();
        assert_eq!((again.position_ms, again.active_index, again.album_mode), (93_250, 1, true));
        assert_eq!(again.tracks[0].info.as_ref().unwrap().title, "One");
    }

    #[test]
    fn resuming_a_restored_queue_starts_at_the_saved_position() {
        let mut h = Harness::new();
        h.cmd(PlayerCommand::Restore(Box::new(saved_queue(93_250))));
        let fx = h.cmd(PlayerCommand::TogglePause);
        let (load, item, album) = fx
            .iter()
            .find_map(|e| match e {
                Effect::Resolve { load, item, use_track_gain, .. } => Some((*load, item.clone(), !use_track_gain)),
                _ => None,
            })
            .expect("resolves the current track");
        assert_eq!((item.track_id, album), (2, true), "album gain comes back too");
        assert_eq!(h.core.persisted().unwrap().position_ms, 93_250, "a quit while loading keeps the position");
        let fx = h.send(Input::PlayResolved { load, result: Ok(resolved("2", 200.0)) });
        assert!(fx.iter().any(|e| matches!(e, Effect::Play { start: Some(s), .. } if (*s - 93.25).abs() < 1e-9)));
        let fx = h.send(Input::PlayStarted { load, result: Ok(()) });
        assert_eq!(h.core.state(), PlaybackState::Playing);
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::Position(p)) if (*p - 93.25).abs() < 1e-9)));
        assert_eq!(h.core.position, 93.25);
    }

    #[test]
    fn a_restored_queue_seeks_and_skips_without_playing_first() {
        let mut h = Harness::new();
        h.cmd(PlayerCommand::Restore(Box::new(saved_queue(93_250))));
        let fx = h.cmd(PlayerCommand::Seek(10.0));
        assert!(!fx.iter().any(|e| matches!(e, Effect::Seek(_))), "nothing to seek in the engine");
        assert_eq!(h.core.persisted().unwrap().position_ms, 10_000);
        // Next plays the next track from its start.
        let fx = h.cmd(PlayerCommand::Next);
        assert_eq!(resolve(&fx).unwrap().track_id, 3);
        let fx = h.finish_load(&fx, 180.0);
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::TrackStarted { via: Transition::Skip, .. }))));
        assert_eq!(h.core.position, 0.0);
    }

    #[test]
    fn inconsistent_or_empty_saves_are_not_restored() {
        let mut h = Harness::new();
        let bad = PersistedQueue { active_index: 9, ..saved_queue(0) };
        h.cmd(PlayerCommand::Restore(Box::new(bad)));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
        assert!(h.core.persisted().is_none());
        let empty = PersistedQueue { tracks: Vec::new(), active_index: 0, ..saved_queue(0) };
        h.cmd(PlayerCommand::Restore(Box::new(empty)));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
        assert!(h.core.persisted().is_none());
    }

    #[test]
    fn persisted_positions_follow_the_state() {
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::Off, false);
        h.send(Input::Tick { position: 42.4, track: h.core.track_seq() });
        assert_eq!(h.core.persisted().unwrap().position_ms, 42_400);
        h.cmd(PlayerCommand::Pause);
        assert_eq!(h.core.persisted().unwrap().position_ms, 42_400, "paused");
        h.cmd(PlayerCommand::Stop);
        assert_eq!(h.core.persisted().unwrap().position_ms, 0, "stopped");
        // Logout loads an empty queue: nothing to save.
        h.cmd(PlayerCommand::Load { tracks: Vec::new(), start: None, album_mode: false, shuffle: false, repeat: RepeatMode::Off });
        assert!(h.core.persisted().is_none());
    }

    #[test]
    fn engine_errors_are_classified() {
        assert_eq!(ErrorKind::of_engine("device_busy", "x"), ErrorKind::DeviceBusy);
        assert_eq!(ErrorKind::of_engine("", "device_busy"), ErrorKind::DeviceBusy);
        assert_eq!(ErrorKind::of_engine("device_disconnected", ""), ErrorKind::Device);
        assert_eq!(ErrorKind::of_engine("", &unsupported_rate_error(96000)), ErrorKind::UnsupportedRate);
        assert_eq!(ErrorKind::of_engine("playback_error", "internal data stream error"), ErrorKind::Other);
        assert!(expired_url("Forbidden (403), URL: https://sp-ad-cf.audio.tidal.com/x"));
        assert!(!expired_url("Not Found (404), URL: https://x"));
    }

    /// Arm the resolving slot in `fx` and return its qid.
    fn arm(h: &mut Harness, fx: &[Effect], duration: f64) -> String {
        let (pf, item) = resolve_next(fx).expect("a ResolveNext");
        let fx = h.send(Input::NextResolved { prefetch: pf, result: Ok(resolved(&item.track_id.to_string(), duration)) });
        fx.iter()
            .find_map(|e| match e {
                Effect::ArmNext { qid, .. } => Some(qid.clone()),
                _ => None,
            })
            .expect("armed")
    }

    fn advanced(h: &mut Harness, track_id: u64, qid: &str) -> Vec<Effect> {
        h.send(Input::TrackAdvanced { track_id, qid: qid.into(), replay_gain: -7.5, peak_amplitude: 0.9 })
    }

    #[test]
    fn repeat_one_passes_are_gapless() {
        let mut h = Harness::new();
        h.load(&[1, 2, 3], RepeatMode::One, false);
        let base = h.core.queue().current().unwrap().qid.clone();
        let mut passes = Vec::new();
        for _ in 0..3 {
            let fx = h.send(Input::Tick { position: 185.0, track: h.core.track_seq() });
            let qid = arm(&mut h, &fx, 200.0);
            assert!(is_pass_of(&qid, &base), "{qid} is a pass of {base}");
            assert!(!passes.contains(&qid), "each pass has its own qid, or the engine would refuse it");
            let fx = advanced(&mut h, 1, &qid);
            assert_eq!(started(&fx), Some((1, Transition::Repeat)));
            assert!(!fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::QueueChanged { .. }))), "the queue didn't move");
            assert_eq!(h.core.queue().position(), (0, 3));
            assert_eq!(h.core.position, 0.0);
            passes.push(qid);
        }
        // A skip still moves on.
        assert_eq!(resolve(&h.cmd(PlayerCommand::Next)).unwrap().track_id, 2);
    }

    #[test]
    fn the_repeat_all_wrap_is_gapless_without_shuffle() {
        let mut h = Harness::new();
        let fx = h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1, 2, 3]), start: Some(2), album_mode: true, shuffle: false, repeat: RepeatMode::All });
        h.finish_load(&fx, 200.0);
        let first = h.core.queue().first().unwrap().qid.clone();
        let fx = h.send(Input::Tick { position: 185.0, track: h.core.track_seq() });
        let qid = arm(&mut h, &fx, 200.0);
        assert_eq!(qid, first);
        let fx = advanced(&mut h, 1, &qid);
        assert_eq!(started(&fx), Some((1, Transition::Wrap)));
        assert_eq!(h.core.queue().position(), (0, 3));
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::QueueChanged { current: 0, .. }))));
        // And on: the next one is 2 again.
        let fx = h.send(Input::Tick { position: 185.0, track: h.core.track_seq() });
        assert_eq!(resolve_next(&fx).unwrap().1.track_id, 2);
    }

    #[test]
    fn a_shuffled_wrap_is_not_prefetched() {
        let mut h = Harness::new();
        let fx = h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1, 2]), start: Some(0), album_mode: false, shuffle: true, repeat: RepeatMode::All });
        h.finish_load(&fx, 200.0);
        let fx = h.send(Input::TrackFinished);
        h.finish_load(&fx, 200.0);
        assert_eq!(h.core.queue().upcoming().count(), 0);
        let fx = h.send(Input::Tick { position: 185.0, track: h.core.track_seq() });
        assert!(resolve_next(&fx).is_none(), "the wrap reshuffles: nothing to predict");
    }

    #[test]
    fn a_repeat_pass_taken_after_repeat_was_turned_off_is_followed() {
        // The engine committed to the pass seconds before it is heard; the
        // user turned repeat off in between.
        let mut h = Harness::new();
        h.load(&[1, 2], RepeatMode::One, false);
        let fx = h.send(Input::Tick { position: 185.0, track: h.core.track_seq() });
        let qid = arm(&mut h, &fx, 200.0);
        h.cmd(PlayerCommand::SetRepeat(RepeatMode::Off));
        let fx = advanced(&mut h, 1, &qid);
        assert_eq!(started(&fx), Some((1, Transition::Repeat)), "no re-added entry");
        assert_eq!(h.core.queue().in_order().count(), 2);
        // Then the queue goes on to 2: turning repeat off already moved
        // the slot there, and it is kept as the next track's.
        assert_eq!(h.core.prefetch.as_ref().map(|p| p.item.track_id), Some(2));
        assert_eq!(h.core.queue().upcoming().next().unwrap().track_id, 2);
    }

    #[test]
    fn pass_qids() {
        assert!(is_pass_of(&pass_qid("55391787-3", 12), "55391787-3"));
        assert!(!is_pass_of("55391787-3", "55391787-3"), "the entry itself is not a pass");
        assert!(!is_pass_of("55391787-31~2", "55391787-3"), "another entry's pass");
        assert!(!is_pass_of("55391787-3~x", "55391787-3"));
    }

    #[test]
    fn a_failed_or_cancelled_resume_keeps_the_restored_place() {
        let mut h = Harness::new();
        h.cmd(PlayerCommand::Restore(Box::new(saved_queue(93_250))));
        // Offline: the resolve fails.
        let fx = h.cmd(PlayerCommand::Resume);
        let load = resolve(&fx).map(|_| h.core.load).unwrap();
        let fx = h.send(Input::PlayResolved {
            load,
            result: Err(ResolveError { message: "network".into(), kind: ErrorKind::Network }),
        });
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::Error { kind: ErrorKind::Network, .. }))));
        assert_eq!(h.core.state(), PlaybackState::Restored);
        assert_eq!(h.core.persisted().unwrap().position_ms, 93_250);
        // Space twice: the second press cancels the load.
        h.cmd(PlayerCommand::TogglePause);
        assert_eq!(h.core.state(), PlaybackState::Loading);
        h.cmd(PlayerCommand::TogglePause);
        assert_eq!(h.core.state(), PlaybackState::Restored);
        assert_eq!(h.core.persisted().unwrap().position_ms, 93_250);
        // The device is busy: play_url fails.
        let fx = h.cmd(PlayerCommand::Resume);
        let load = resolve(&fx).map(|_| h.core.load).unwrap();
        h.send(Input::PlayResolved { load, result: Ok(resolved("2", 200.0)) });
        let fx = h.send(Input::PlayStarted { load, result: Err("device_busy".into()) });
        assert!(has(&fx, &Effect::Stop), "whatever opened is released");
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::Error { kind: ErrorKind::DeviceBusy, .. }))));
        assert_eq!(h.core.state(), PlaybackState::Restored);
        assert_eq!(h.core.persisted().unwrap().position_ms, 93_250);
        // An engine error while nothing is loaded isn't the queue's.
        let fx = h.send(Input::AudioError { kind: "device_busy".into(), message: None });
        assert!(fx.is_empty());
        assert_eq!(h.core.state(), PlaybackState::Restored);
        // A resume that works still starts there.
        let fx = h.cmd(PlayerCommand::Resume);
        let load = resolve(&fx).map(|_| h.core.load).unwrap();
        let fx = h.send(Input::PlayResolved { load, result: Ok(resolved("2", 200.0)) });
        assert!(fx.iter().any(|e| matches!(e, Effect::Play { start: Some(s), .. } if (*s - 93.25).abs() < 1e-9)));
    }

    #[test]
    fn an_explicit_stop_or_a_skip_from_restored_does_not_restore() {
        let mut h = Harness::new();
        h.cmd(PlayerCommand::Restore(Box::new(saved_queue(93_250))));
        h.cmd(PlayerCommand::Resume);
        h.cmd(PlayerCommand::Stop);
        assert_eq!(h.core.state(), PlaybackState::Stopped);
        // Next loads another entry: a failure there stops as usual.
        h.cmd(PlayerCommand::Restore(Box::new(saved_queue(93_250))));
        let fx = h.cmd(PlayerCommand::Next);
        let load = resolve(&fx).map(|_| h.core.load).unwrap();
        h.send(Input::PlayResolved {
            load,
            result: Err(ResolveError { message: "network".into(), kind: ErrorKind::Network }),
        });
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    #[test]
    fn queue_edits_on_a_restored_queue_are_published() {
        let mut h = Harness::new();
        h.cmd(PlayerCommand::Restore(Box::new(saved_queue(93_250))));
        let fx = h.cmd(PlayerCommand::PlayNext(9.into()));
        let items = fx.iter().find_map(|e| match e {
            Effect::Emit(PlayerEvent::QueueChanged { items, current, .. }) => Some((items.len(), *current)),
            _ => None,
        });
        assert_eq!(items, Some((4, 1)));
        assert_eq!(h.core.persisted().unwrap().tracks.len(), 4);
        assert_eq!(h.core.state(), PlaybackState::Restored, "still waiting, nothing played");
    }

    // Continuous playback.

    fn continuous() -> Harness {
        Harness { core: Core::new(Config { continuous: true, ..Config::default() }, 1), now: 0.0 }
    }

    impl Harness {
        fn tick(&mut self, position: f64) -> Vec<Effect> {
            self.send(Input::Tick { position, track: self.core.track_seq() })
        }

        /// A queue of `tracks`, playing the entry at `start` (200 s long).
        fn play_at(&mut self, tracks: &[u64], start: usize) -> Vec<Effect> {
            let fx = self.cmd(PlayerCommand::Load {
                tracks: QueueTrack::from_ids(tracks),
                start: Some(start),
                album_mode: true,
                shuffle: false,
                repeat: RepeatMode::Off,
            });
            self.finish_load(&fx, 200.0)
        }

        fn answer(&mut self, fetch: u64, ids: &[u64]) -> Vec<Effect> {
            self.send(Input::RadioFetched { fetch, result: Ok(QueueTrack::from_ids(ids)) })
        }

        fn waiting(&self) -> bool {
            self.core.state() == PlaybackState::Loading && self.core.loading.is_none() && self.core.waiting.is_some()
        }

        fn play_order(&self) -> Vec<u64> {
            self.core.queue().in_order().map(|i| i.track_id).collect()
        }
    }

    fn fetch_radio(fx: &[Effect]) -> Option<(u64, u64)> {
        fx.iter().find_map(|e| match e {
            Effect::FetchRadio { fetch, seed } => Some((*fetch, seed.track_id)),
            _ => None,
        })
    }

    fn appended(fx: &[Effect]) -> Option<(u64, usize)> {
        fx.iter().find_map(|e| match e {
            Effect::Emit(PlayerEvent::RadioAppended { seed, count }) => Some((seed.track_id, *count)),
            _ => None,
        })
    }

    fn started_item(fx: &[Effect]) -> Option<(QueueItem, Transition)> {
        fx.iter().find_map(|e| match e {
            Effect::Emit(PlayerEvent::TrackStarted { item, via, .. }) => Some((item.clone(), *via)),
            _ => None,
        })
    }

    fn ended(fx: &[Effect]) -> bool {
        has(fx, &Effect::Emit(PlayerEvent::QueueEnded))
    }

    #[test]
    fn the_radio_is_fetched_inside_its_window_once() {
        let mut h = continuous();
        h.play_at(&[1, 2], 1);
        assert_eq!(fetch_radio(&h.tick(100.0)), None, "100 s left");
        let fx = h.tick(126.0);
        assert_eq!(fetch_radio(&fx).map(|(_, seed)| seed), Some(2), "74 s left");
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::RadioFetchStarted { seed }) if seed.track_id == 2)));
        assert_eq!(fetch_radio(&h.tick(127.0)), None, "one fetch per track");
    }

    #[test]
    fn no_radio_with_repeat_or_continuous_off_or_a_next_track() {
        for repeat in [RepeatMode::All, RepeatMode::One] {
            let mut h = continuous();
            h.load(&[1], repeat, false);
            assert_eq!(fetch_radio(&h.tick(190.0)), None, "{repeat:?}");
        }
        let mut h = Harness::new();
        h.load(&[1], RepeatMode::Off, false);
        assert_eq!(fetch_radio(&h.tick(190.0)), None, "continuous off");
        let mut h = continuous();
        h.load(&[1, 2], RepeatMode::Off, false);
        let fx = h.tick(190.0);
        assert_eq!(fetch_radio(&fx), None, "a track is upcoming");
        assert!(resolve_next(&fx).is_some());
    }

    #[test]
    fn no_radio_while_paused_until_play_resumes() {
        let mut h = continuous();
        h.load(&[1], RepeatMode::Off, false);
        h.cmd(PlayerCommand::Pause);
        assert_eq!(fetch_radio(&h.tick(190.0)), None);
        assert_eq!(fetch_radio(&h.cmd(PlayerCommand::Resume)), None);
        assert_eq!(fetch_radio(&h.tick(190.2)).map(|(_, seed)| seed), Some(1), "the next tick");
    }

    #[test]
    fn a_track_of_unknown_length_fetches_at_once() {
        let mut h = continuous();
        let fx = h.cmd(PlayerCommand::Load {
            tracks: QueueTrack::from_ids(&[1]),
            start: Some(0),
            album_mode: false,
            shuffle: false,
            repeat: RepeatMode::Off,
        });
        let fx = h.finish_load_len(&fx, None);
        assert_eq!(fetch_radio(&fx).map(|(_, seed)| seed), Some(1));
    }

    #[test]
    fn the_radio_is_fetched_without_gapless_or_after_an_output_change() {
        let mut h = continuous();
        h.load(&[1], RepeatMode::Off, false);
        h.cmd(PlayerCommand::SetGapless(false));
        assert!(fetch_radio(&h.tick(190.0)).is_some(), "gapless off");
        let mut h = continuous();
        h.load(&[1], RepeatMode::Off, false);
        h.cmd(PlayerCommand::SetOutput { exclusive: false, device: None, bit_perfect: false });
        assert!(fetch_radio(&h.tick(190.0)).is_some(), "output changed");
    }

    #[test]
    fn a_page_appending_before_the_window_means_no_radio() {
        let mut h = continuous();
        h.load(&[1], RepeatMode::Off, false);
        h.tick(1.0);
        h.cmd(PlayerCommand::Append(QueueTrack::from_ids(&[2, 3])));
        for t in [100.0, 130.0, 190.0] {
            assert_eq!(fetch_radio(&h.tick(t)), None);
        }
    }

    #[test]
    fn an_answer_leaves_out_what_is_queued_and_goes_last_in_order() {
        // Shuffled, with a track appended while the radio was on the way.
        let mut h = continuous();
        let fx = h.cmd(PlayerCommand::Load {
            tracks: QueueTrack::from_ids(&[1, 2, 3]),
            start: Some(0),
            album_mode: true,
            shuffle: true,
            repeat: RepeatMode::Off,
        });
        h.finish_load(&fx, 200.0);
        let last = h.core.queue().in_order().last().unwrap().qid.clone();
        let fx = h.cmd(PlayerCommand::JumpTo(last));
        h.finish_load(&fx, 200.0);
        let seed = h.current();
        let (fetch, _) = fetch_radio(&h.tick(130.0)).expect("a fetch");
        h.cmd(PlayerCommand::Append(QueueTrack::from_ids(&[50])));
        let before = h.play_order();
        let fx = h.answer(fetch, &[seed, 1, 60, 50, 2, 61, 3, 60, 62]);
        assert_eq!(appended(&fx), Some((seed, 3)));
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::QueueChanged { .. }))));
        let order = h.play_order();
        assert_eq!(&order[..before.len()], &before[..], "the user's track stays ahead");
        assert_eq!(&order[before.len()..], &[60, 61, 62], "TIDAL's order, shuffle or not");
        let origins: Vec<Origin> = h.core.queue().in_order().map(|i| i.origin).collect();
        assert_eq!(origins[before.len()..], [Origin::Radio { seed }; 3]);
        // The prefetch, inside its window: the user's track first.
        let fx = h.tick(175.0);
        assert!(fx.iter().any(|e| matches!(e, Effect::ResolveNext { item, use_track_gain: true, .. } if item.track_id == 50)));

        // Without the user's track: the radio's first.
        let mut h = continuous();
        h.play_at(&[1], 0);
        let (fetch, _) = fetch_radio(&h.tick(130.0)).unwrap();
        assert_eq!(appended(&h.answer(fetch, &[1, 70, 71])), Some((1, 2)));
        assert_eq!(resolve_next(&h.tick(175.0)).unwrap().1.track_id, 70);
    }

    #[test]
    fn a_radio_is_never_appended_twice() {
        let mut h = continuous();
        h.play_at(&[1], 0);
        let (f1, _) = fetch_radio(&h.tick(130.0)).unwrap();
        h.cmd(PlayerCommand::Append(QueueTrack::from_ids(&[2])));
        let (pf, item) = resolve_next(&h.tick(175.0)).unwrap();
        let qid = arm(&mut h, &[Effect::ResolveNext { prefetch: pf, item, use_track_gain: false, normalization: false, quality: String::new() }], 200.0);
        let fx = advanced(&mut h, 2, &qid);
        assert_eq!(started(&fx), Some((2, Transition::Gapless)));
        let (f2, seed) = fetch_radio(&h.tick(130.0)).expect("a fetch for the new last track");
        assert_eq!(seed, 2);
        assert_ne!(f1, f2);
        assert_eq!(appended(&h.answer(f1, &[10, 11])), None, "the first track's radio is stale");
        assert_eq!(h.play_order(), vec![1, 2]);
        assert_eq!(appended(&h.answer(f2, &[12, 13])), Some((2, 2)));
        assert_eq!(h.play_order(), vec![1, 2, 12, 13]);
        for t in [131.0, 150.0, 190.0] {
            assert_eq!(fetch_radio(&h.tick(t)), None);
        }
    }

    #[test]
    fn a_jump_back_onto_the_last_track_cannot_leave_a_wait_stuck() {
        let mut h = continuous();
        h.play_at(&[1, 2], 1);
        let (f1, _) = fetch_radio(&h.tick(130.0)).unwrap();
        let qid = h.core.queue().current().unwrap().qid.clone();
        let fx = h.cmd(PlayerCommand::JumpTo(qid));
        h.finish_load(&fx, 200.0);
        let fx = h.send(Input::TrackFinished);
        let (f2, seed) = fetch_radio(&fx).expect("a new fetch: the old one's memo is gone");
        assert_eq!(seed, 2);
        assert!(h.waiting());
        assert!(resolve(&h.answer(f1, &[10])).is_none(), "the orphan is dropped");
        assert!(h.waiting());
        let fx = h.answer(f2, &[11]);
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((11, Transition::AfterEnd)));
    }

    #[test]
    fn a_stale_answer_is_dropped() {
        type Act = fn(&mut Harness);
        let acts: [(&str, Act); 6] = [
            ("Load", |h| {
                h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[7]), start: None, album_mode: false, shuffle: false, repeat: RepeatMode::Off });
            }),
            ("Stop", |h| {
                h.cmd(PlayerCommand::Stop);
            }),
            ("JumpTo", |h| {
                let first = h.core.queue().in_order().next().unwrap().qid.clone();
                h.cmd(PlayerCommand::JumpTo(first));
            }),
            ("Previous", |h| {
                h.tick(2.0);
                h.cmd(PlayerCommand::Previous);
            }),
            ("Restore", |h| {
                h.cmd(PlayerCommand::Restore(Box::new(saved_queue(0))));
            }),
            ("SetContinuous(false)", |h| {
                h.cmd(PlayerCommand::SetContinuous(false));
            }),
        ];
        for (name, act) in acts {
            let mut h = continuous();
            let fx = h.cmd(PlayerCommand::Load { tracks: QueueTrack::from_ids(&[1, 2]), start: Some(1), album_mode: false, shuffle: false, repeat: RepeatMode::Off });
            let fx = h.finish_load_len(&fx, None);
            let (fetch, _) = fetch_radio(&fx).expect("unknown length: at once");
            act(&mut h);
            let before = h.play_order();
            let fx = h.answer(fetch, &[10, 11]);
            assert_eq!(appended(&fx), None, "{name}");
            assert_eq!(h.play_order(), before, "{name}");
        }
    }

    #[test]
    fn a_radio_on_the_way_is_dropped_when_repeat_comes_on() {
        for repeat in [RepeatMode::All, RepeatMode::One] {
            let mut h = continuous();
            h.play_at(&[1], 0);
            let (fetch, _) = fetch_radio(&h.tick(130.0)).unwrap();
            h.cmd(PlayerCommand::SetRepeat(repeat));
            assert_eq!(appended(&h.answer(fetch, &[10, 11])), None, "{repeat:?}");
            assert_eq!(h.play_order(), vec![1]);
        }
    }

    #[test]
    fn one_fetch_per_track() {
        let mut h = Harness::new();
        h.load(&[1], RepeatMode::Off, false);
        h.tick(150.0);
        let (fetch, _) = fetch_radio(&h.cmd(PlayerCommand::SetContinuous(true))).expect("in the window");
        assert_eq!(fetch_radio(&h.cmd(PlayerCommand::SetContinuous(true))), None);
        let err = ResolveError { message: "429".into(), kind: ErrorKind::Other };
        let fx = h.send(Input::RadioFetched { fetch, result: Err(err) });
        assert!(!ended(&fx) && h.core.state() == PlaybackState::Playing, "nothing waits on it");
        assert_eq!(fetch_radio(&h.tick(151.0)), None, "a failure is not retried");
        h.cmd(PlayerCommand::Stop);
        let fx = h.cmd(PlayerCommand::Resume);
        h.finish_load(&fx, 200.0);
        assert!(fetch_radio(&h.tick(150.0)).is_some(), "stopped and played again: a new fetch");
    }

    #[test]
    fn the_end_of_the_queue_waits_for_the_radio() {
        // The last track ends with the radio on the way.
        let mut h = continuous();
        h.play_at(&[1], 0);
        let (fetch, _) = fetch_radio(&h.tick(130.0)).unwrap();
        h.tick(199.0);
        let load = h.core.load;
        let fx = h.send(Input::TrackFinished);
        assert!(!ended(&fx) && !has(&fx, &Effect::Stop));
        assert_eq!(fetch_radio(&fx), None, "one is on the way");
        assert!(h.waiting());
        assert!(h.core.load > load);
        assert_eq!(h.core.saved_position(), 0.0);
        let fx = h.answer(fetch, &[10, 11]);
        assert_eq!(resolve(&fx).unwrap().track_id, 10);
        let fx = h.finish_load(&fx, 200.0);
        let (item, via) = started_item(&fx).unwrap();
        assert_eq!((item.track_id, via, item.origin), (10, Transition::AfterEnd, Origin::Radio { seed: 1 }));

        // Next at the last track, before the window.
        let mut h = continuous();
        h.play_at(&[1], 0);
        let fx = h.cmd(PlayerCommand::Next);
        assert!(has(&fx, &Effect::Stop), "the old track stops");
        assert!(!ended(&fx));
        let (fetch, _) = fetch_radio(&fx).expect("fetched now");
        assert!(h.waiting());
        let fx = h.answer(fetch, &[10]);
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((10, Transition::Skip)));

        // The last track can't be played: the skip loop waits too.
        let mut h = continuous();
        h.play_at(&[1, 2], 0);
        h.cmd(PlayerCommand::Next);
        let fx = h.send(Input::PlayResolved {
            load: h.core.load,
            result: Err(ResolveError { message: "404".into(), kind: ErrorKind::Unplayable }),
        });
        assert!(fx.iter().any(|e| matches!(e, Effect::Emit(PlayerEvent::Skipped { .. }))));
        assert!(fetch_radio(&fx).is_some() && !ended(&fx));
        assert!(h.waiting());

        // Next on a restored queue at its last track.
        let mut h = continuous();
        let saved = PersistedQueue { active_index: 2, repeat: RepeatMode::Off, ..saved_queue(0) };
        h.cmd(PlayerCommand::Restore(Box::new(saved)));
        let fx = h.cmd(PlayerCommand::Next);
        assert!(fetch_radio(&fx).is_some() && !ended(&fx));
        assert!(h.waiting());
    }

    /// Continuous, a queue of `tracks` played to the end of its last one,
    /// waiting for the radio: the fetch's generation.
    fn waiting_at_end(tracks: &[u64]) -> (Harness, u64) {
        let mut h = continuous();
        h.play_at(tracks, tracks.len() - 1);
        let fx = h.send(Input::TrackFinished);
        let (fetch, _) = fetch_radio(&fx).unwrap();
        assert!(h.waiting());
        (h, fetch)
    }

    #[test]
    fn commands_while_waiting_for_the_radio() {
        // A jump wins over the late answer.
        let (mut h, fetch) = waiting_at_end(&[1, 2]);
        let first = h.core.queue().in_order().next().unwrap().qid.clone();
        assert_eq!(resolve(&h.cmd(PlayerCommand::JumpTo(first))).unwrap().track_id, 1);
        assert!(resolve(&h.answer(fetch, &[10])).is_none());
        assert_eq!(h.play_order(), vec![1, 2]);

        // Previous plays the track before, or the only one again.
        let (mut h, _) = waiting_at_end(&[1, 2]);
        let fx = h.cmd(PlayerCommand::Previous);
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((1, Transition::Previous)));
        let (mut h, _) = waiting_at_end(&[1]);
        let fx = h.cmd(PlayerCommand::Previous);
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((1, Transition::Previous)));

        // A track the user queues plays at once.
        let (mut h, fetch) = waiting_at_end(&[1]);
        let fx = h.cmd(PlayerCommand::Append(QueueTrack::from_ids(&[9])));
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((9, Transition::AfterEnd)));
        assert_eq!(appended(&h.answer(fetch, &[10])), None);
        let (mut h, _) = waiting_at_end(&[1]);
        assert_eq!(resolve(&h.cmd(PlayerCommand::PlayNext(9.into()))).unwrap().track_id, 9);

        // Play/pause is the way out.
        let (mut h, fetch) = waiting_at_end(&[1]);
        let fx = h.cmd(PlayerCommand::TogglePause);
        assert!(has(&fx, &Effect::Stop));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
        assert!(resolve(&h.answer(fetch, &[10])).is_none());

        // Switched off: the queue ends as it would have.
        let (mut h, fetch) = waiting_at_end(&[1]);
        assert!(ended(&h.cmd(PlayerCommand::SetContinuous(false))));
        assert_eq!(h.core.state(), PlaybackState::Stopped);
        assert_eq!(appended(&h.answer(fetch, &[10])), None);

        // Repeat: One replays the last track, All starts over.
        let (mut h, _) = waiting_at_end(&[1, 2]);
        let fx = h.cmd(PlayerCommand::SetRepeat(RepeatMode::One));
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((2, Transition::Repeat)));
        let (mut h, _) = waiting_at_end(&[1, 2]);
        let fx = h.cmd(PlayerCommand::SetRepeat(RepeatMode::All));
        let fx = h.finish_load(&fx, 200.0);
        assert_eq!(started(&fx), Some((1, Transition::Wrap)));
    }

    #[test]
    fn an_empty_or_failed_radio_ends_the_queue() {
        // The stop, the reason, then (only for an empty radio) the end.
        let events = |fx: &[Effect]| -> Vec<PlayerEvent> {
            fx.iter()
                .filter_map(|e| match e {
                    Effect::Emit(
                        ev @ (PlayerEvent::State(_) | PlayerEvent::Notice(_) | PlayerEvent::Error { .. } | PlayerEvent::QueueEnded),
                    ) => Some(ev.clone()),
                    _ => None,
                })
                .collect()
        };
        for ids in [&[][..], &[1, 1][..]] {
            let (mut h, fetch) = waiting_at_end(&[1]);
            let fx = h.answer(fetch, ids);
            assert_eq!(
                events(&fx),
                vec![
                    PlayerEvent::State(PlaybackState::Stopped),
                    PlayerEvent::Notice("No more tracks: no radio to continue with".into()),
                    PlayerEvent::QueueEnded,
                ],
                "{ids:?}"
            );
            assert_eq!(h.core.state(), PlaybackState::Stopped);
        }
        let (mut h, fetch) = waiting_at_end(&[1]);
        let err = ResolveError { message: "401".into(), kind: ErrorKind::LoginExpired };
        let fx = h.send(Input::RadioFetched { fetch, result: Err(err) });
        assert_eq!(
            events(&fx),
            vec![
                PlayerEvent::State(PlaybackState::Stopped),
                PlayerEvent::Error { kind: ErrorKind::LoginExpired, message: "401".into() },
            ]
        );
        assert_eq!(h.core.state(), PlaybackState::Stopped);
    }

    #[test]
    fn a_radio_the_user_removed_is_not_fetched_again() {
        let mut h = continuous();
        h.play_at(&[1], 0);
        let (fetch, _) = fetch_radio(&h.tick(130.0)).unwrap();
        h.answer(fetch, &[10, 11]);
        h.cmd(PlayerCommand::RemoveUpcoming(0));
        let fx = h.cmd(PlayerCommand::RemoveUpcoming(0));
        assert_eq!(fetch_radio(&fx), None);
        let fx = h.send(Input::TrackFinished);
        assert!(ended(&fx));
        assert_eq!(fetch_radio(&fx), None);
    }
}
