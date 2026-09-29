//! Queue and playback state machine that drives the engine. `Core` is
//! the state machine without I/O, `Player` runs it against the engine and
//! TIDAL, `Queue` holds the play order and `PersistedQueue` saves it.

pub mod core;
pub mod persist;
pub mod queue;
pub mod runner;

pub use crate::core::{
    unsupported_rate_error, Config, ErrorKind, PlaybackState, PlayerCommand, PlayerEvent, StreamFormat, Transition,
};
pub use persist::{PersistedQueue, SavedTrack};
pub use queue::{Origin, QueueItem, QueueTrack, RepeatMode, TrackInfo};
pub use runner::{Player, PlayerConfig, Update};
