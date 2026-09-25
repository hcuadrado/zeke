//! Engine events: a typed enum on a channel that works from any thread (the
//! GStreamer and ALSA threads send; the CLI or the GTK main loop receives).

use crate::signal_path::SignalPath;

#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// An audio error. `kind` is one of:
    /// `device_busy`, `device_disconnected`, `device_changed`,
    /// `format_change_failed`, `playback_error`, or a writer error.
    AudioError {
        kind: String,
        message: Option<String>,
    },
    /// The engine resamples `from` → `to` Hz.
    AudioResampled { from: u32, to: u32 },
    /// Bit-perfect container promotion: the output bit depth changed.
    AudioBitDepthChanged { from: String, to: String },
    /// The track ended with nothing prerolled after it.
    TrackFinished,
    /// A gapless switch to the queued next track.
    TrackAdvanced {
        track_id: u64,
        qid: String,
        replay_gain: f64,
        peak_amplitude: f64,
    },
    /// The signal path changed.
    SignalPathChanged(Box<SignalPath>),
}

impl EngineEvent {
    pub fn audio_error(kind: &str, message: Option<&str>) -> Self {
        Self::AudioError {
            kind: kind.to_string(),
            message: message.map(str::to_string),
        }
    }
}

/// The sending half, cloned into every engine thread. Sending never blocks:
/// the channel is unbounded, and events for a dropped receiver are discarded.
#[derive(Clone)]
pub struct EventSender(async_channel::Sender<EngineEvent>);

impl EventSender {
    pub fn emit(&self, event: EngineEvent) {
        let _ = self.0.try_send(event);
    }
}

pub fn channel() -> (EventSender, async_channel::Receiver<EngineEvent>) {
    let (tx, rx) = async_channel::unbounded();
    (EventSender(tx), rx)
}
