//! The player bar's quality badge: what TIDAL sends, and where the output
//! rate differs from it, the rate the device really plays at, e.g.
//! "FLAC 24/96 → 48 kHz" (resampled) or "FLAC 24/48" (untouched).

use zeke_engine::pipeline_probe::HwParamsState;
use zeke_engine::SignalPath;
use zeke_player::StreamFormat;

/// 44100 → "44.1", 48000 → "48", 176400 → "176.4".
pub fn khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        (rate / 1000).to_string()
    } else {
        format!("{:.1}", f64::from(rate) / 1000.0)
    }
}

fn codec_name(codec: &str) -> String {
    let c = codec.to_ascii_lowercase();
    if c.contains("flac") {
        "FLAC".into()
    } else if c.contains("mp4a") || c.contains("aac") {
        "AAC".into()
    } else if c.contains("alac") {
        "ALAC".into()
    } else {
        codec.to_ascii_uppercase()
    }
}

/// The rate the output runs at. Exclusive mode: the kernel's `hw_params` for
/// the device while it is open, else what the engine negotiated. System
/// mixer: the PipeWire sink's rate, which it resamples to.
fn output_rate(path: &SignalPath) -> Option<u32> {
    match path.backend.as_deref() {
        Some("DirectAlsa") => path
            .dac
            .as_ref()
            .filter(|d| d.state == HwParamsState::Active && d.rate > 0)
            .map(|d| d.rate)
            .or(path.output_rate)
            .or(path.resampled_to),
        Some("Normal") => path.os_mixer.as_ref().map(|m| m.sink_rate).filter(|&r| r > 0),
        _ => None,
    }
}

/// `None` until the stream's format is known.
pub fn quality_badge(format: &StreamFormat, path: Option<&SignalPath>) -> Option<String> {
    let codec = codec_name(format.codec.as_deref()?);
    let source = match (format.bit_depth, format.sample_rate) {
        (Some(bits), Some(rate)) => format!("{codec} {bits}/{}", khz(rate)),
        (None, Some(rate)) => format!("{codec} {} kHz", khz(rate)),
        _ => codec,
    };
    match (format.sample_rate, path.and_then(output_rate)) {
        (Some(from), Some(to)) if from != to => Some(format!("{source} → {} kHz", khz(to))),
        _ => Some(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeke_engine::pipeline_probe::{DacHwParams, OsMixerInfo};

    fn fmt(bits: Option<u32>, rate: u32) -> StreamFormat {
        StreamFormat { codec: Some("flac".into()), bit_depth: bits, sample_rate: Some(rate) }
    }

    fn exclusive(output_rate: Option<u32>, dac: Option<(u32, HwParamsState)>) -> SignalPath {
        SignalPath {
            backend: Some("DirectAlsa".into()),
            output_rate,
            dac: dac.map(|(rate, state)| DacHwParams {
                card_index: 0,
                card_name: "sof-hda-dsp".into(),
                pcm_device: "hw:CARD=sofhdadsp,DEV=0".into(),
                format: "S32_LE".into(),
                rate,
                channels: 2,
                period_size: 0,
                buffer_size: 0,
                state,
            }),
            ..SignalPath::default()
        }
    }

    #[test]
    fn rates_in_khz() {
        assert_eq!(khz(44100), "44.1");
        assert_eq!(khz(48000), "48");
        assert_eq!(khz(88200), "88.2");
        assert_eq!(khz(192000), "192");
    }

    #[test]
    fn resampling_is_shown() {
        // 455128517 in the default mode on the laptop.
        let p = exclusive(Some(48000), Some((48000, HwParamsState::Active)));
        assert_eq!(quality_badge(&fmt(Some(24), 96000), Some(&p)).as_deref(), Some("FLAC 24/96 → 48 kHz"));
        assert_eq!(quality_badge(&fmt(Some(16), 44100), Some(&p)).as_deref(), Some("FLAC 16/44.1 → 48 kHz"));
    }

    #[test]
    fn untouched_rate_shows_the_source_only() {
        let p = exclusive(Some(48000), Some((48000, HwParamsState::Active)));
        assert_eq!(quality_badge(&fmt(Some(24), 48000), Some(&p)).as_deref(), Some("FLAC 24/48"));
        // Nothing known about the output yet.
        assert_eq!(quality_badge(&fmt(Some(24), 96000), None).as_deref(), Some("FLAC 24/96"));
    }

    #[test]
    fn the_kernel_wins_over_the_engine_while_the_device_is_open() {
        let p = exclusive(Some(96000), Some((48000, HwParamsState::Active)));
        assert_eq!(quality_badge(&fmt(Some(24), 96000), Some(&p)).as_deref(), Some("FLAC 24/96 → 48 kHz"));
        // A closed device says nothing; fall back to the engine.
        let p = exclusive(Some(48000), Some((96000, HwParamsState::Closed)));
        assert_eq!(quality_badge(&fmt(Some(24), 96000), Some(&p)).as_deref(), Some("FLAC 24/96 → 48 kHz"));
    }

    #[test]
    fn the_system_mixer_rate_counts_in_normal_mode() {
        let p = SignalPath {
            backend: Some("Normal".into()),
            os_mixer: Some(OsMixerInfo {
                server: "PipeWire".into(),
                default_sink_name: "alsa_output".into(),
                sink_format: "s32le".into(),
                sink_rate: 48000,
                sink_channels: 2,
                sink_volume: 1.0,
                sink_volume_percent: 100,
                sink_muted: false,
            }),
            ..SignalPath::default()
        };
        assert_eq!(quality_badge(&fmt(Some(24), 96000), Some(&p)).as_deref(), Some("FLAC 24/96 → 48 kHz"));
    }

    #[test]
    fn lossy_and_unknown() {
        let aac = StreamFormat { codec: Some("mp4a.40.2".into()), bit_depth: None, sample_rate: Some(44100) };
        assert_eq!(quality_badge(&aac, None).as_deref(), Some("AAC 44.1 kHz"));
        assert_eq!(quality_badge(&StreamFormat::default(), None), None);
    }
}
