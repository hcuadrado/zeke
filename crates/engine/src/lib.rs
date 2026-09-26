//! GStreamer audio engine: ALSA exclusive output, bit-perfect playback, gapless.
//!
//! Events go out on an [`events::EventSender`].

pub mod acquire;
pub mod audio;
pub mod devices;
pub mod events;
pub mod pipeline_probe;
pub mod signal_path;

// audio.rs names these as `crate::proxy::…` and `crate::ProxySettings`;
// re-exported so those call sites resolve.
pub use zeke_tidal::{proxy, ProxySettings, ProxyType};

pub use signal_path::{SignalPath, SignalPathTracker};
