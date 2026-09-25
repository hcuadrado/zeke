//! The quality badge: what TIDAL sends, and where the output rate differs
//! from it, the rate the device really plays at, e.g. "FLAC 24/96 → 48 kHz"
//! (resampled) or "FLAC 24/48" (untouched). The player bar shows the source
//! only, a dot for resampled or not, and the whole signal path on click.

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

/// What TIDAL sends, short enough for the player bar: "FLAC 16/44.1".
/// `None` until the stream's format is known.
pub fn source_label(format: &StreamFormat) -> Option<String> {
    let codec = codec_name(format.codec.as_deref()?);
    Some(match (format.bit_depth, format.sample_rate) {
        (Some(bits), Some(rate)) => format!("{codec} {bits}/{}", khz(rate)),
        (None, Some(rate)) => format!("{codec} {} kHz", khz(rate)),
        _ => codec,
    })
}

/// The source and output rates, once both are known.
fn rates(format: &StreamFormat, path: Option<&SignalPath>) -> Option<(u32, u32)> {
    Some((format.sample_rate?, output_rate(path?)?))
}

/// Whether the output runs at another rate than the source; `None` while
/// the output rate is unknown.
pub fn resampled(format: &StreamFormat, path: Option<&SignalPath>) -> Option<bool> {
    rates(format, path).map(|(from, to)| from != to)
}

/// The source, and the output rate where it differs:
/// "FLAC 16/44.1 → 48 kHz". `None` until the stream's format is known.
pub fn quality_badge(format: &StreamFormat, path: Option<&SignalPath>) -> Option<String> {
    let source = source_label(format)?;
    match rates(format, path) {
        Some((from, to)) if from != to => Some(format!("{source} → {} kHz", khz(to))),
        _ => Some(source),
    }
}

/// The signal path, a (step, detail) row per stage, for the badge's popover.
pub fn signal_path_rows(format: &StreamFormat, path: Option<&SignalPath>) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if let Some(codec) = format.codec.as_deref() {
        let mut source = vec![codec_name(codec)];
        source.extend(format.bit_depth.map(|b| format!("{b}-bit")));
        source.extend(format.sample_rate.map(|r| format!("{} kHz", khz(r))));
        rows.push(("Source", source.join(" · ")));
    }
    let Some(path) = path else { return rows };

    if let (Some(fmt), Some(rate)) = (&path.decoded_format, path.decoded_rate) {
        rows.push(("Decoded", format!("{fmt} · {} kHz", khz(rate))));
    }
    match rates(format, Some(path)) {
        Some((from, to)) if from != to => rows.push(("Sample rate", format!("{} → {} kHz, resampled", khz(from), khz(to)))),
        Some((from, _)) => rows.push(("Sample rate", format!("{} kHz, unchanged", khz(from)))),
        None => {}
    }
    if let (Some(from), Some(to)) = (&path.promoted_from, &path.promoted_to) {
        rows.push(("Bit depth", format!("{from} → {to}, zero-padded")));
    }
    if let (Some(from), Some(to)) = (&path.format_fallback_from, &path.format_fallback_to) {
        rows.push(("Format", format!("{from} → {to}, converted")));
    }
    if path.user_volume < 1.0 {
        rows.push(("Volume", format!("{:.0}%, in software", path.user_volume * 100.0)));
    }
    if path.norm_gain_factor != 1.0 && path.norm_gain_factor > 0.0 {
        rows.push(("Normalization", format!("{:+.1} dB", 20.0 * path.norm_gain_factor.log10())));
    }

    match path.backend.as_deref() {
        Some("DirectAlsa") => {
            let mode = if path.bit_perfect { "Exclusive (ALSA), bit-perfect" } else { "Exclusive (ALSA)" };
            rows.push(("Output", mode.into()));
            if let Some(dac) = path.dac.as_ref().filter(|d| d.state == HwParamsState::Active) {
                rows.push(("Device", format!("{} · {} · {} kHz", dac.card_name, dac.format, khz(dac.rate))));
            } else if let Some(device) = &path.output_device {
                rows.push(("Device", device.clone()));
            }
        }
        Some("Normal") => {
            let server = path.os_mixer.as_ref().map_or("system", |m| m.server.as_str());
            rows.push(("Output", format!("System mixer ({server})")));
            if let Some(m) = &path.os_mixer {
                rows.push(("Device", format!("{} · {} · {} kHz", m.default_sink_name, m.sink_format, khz(m.sink_rate))));
            }
        }
        _ => {}
    }
    rows
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
    fn the_bar_shows_the_source_and_flags_resampling() {
        let p = exclusive(Some(48000), Some((48000, HwParamsState::Active)));
        assert_eq!(source_label(&fmt(Some(16), 44100)).as_deref(), Some("FLAC 16/44.1"));
        assert_eq!(resampled(&fmt(Some(16), 44100), Some(&p)), Some(true));
        assert_eq!(resampled(&fmt(Some(24), 48000), Some(&p)), Some(false));
        assert_eq!(resampled(&fmt(Some(24), 48000), None), None);
    }

    #[test]
    fn the_popover_walks_the_path() {
        let p = SignalPath {
            user_volume: 1.0,
            norm_gain_factor: 1.0,
            ..exclusive(Some(48000), Some((48000, HwParamsState::Active)))
        };
        let rows = signal_path_rows(&fmt(Some(16), 44100), Some(&p));
        let row = |step| rows.iter().find(|(s, _)| *s == step).map(|(_, d)| d.as_str());
        assert_eq!(row("Source"), Some("FLAC · 16-bit · 44.1 kHz"));
        assert_eq!(row("Sample rate"), Some("44.1 → 48 kHz, resampled"));
        assert_eq!(row("Output"), Some("Exclusive (ALSA)"));
        assert_eq!(row("Device"), Some("sof-hda-dsp · S32_LE · 48 kHz"));
        assert_eq!(row("Volume"), None, "unity volume leaves the samples alone");
    }

    #[test]
    fn lossy_and_unknown() {
        let aac = StreamFormat { codec: Some("mp4a.40.2".into()), bit_depth: None, sample_rate: Some(44100) };
        assert_eq!(quality_badge(&aac, None).as_deref(), Some("AAC 44.1 kHz"));
        assert_eq!(quality_badge(&StreamFormat::default(), None), None);
    }
}
