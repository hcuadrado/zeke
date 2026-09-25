//! Stream resolution and ReplayGain, as plain functions. The player layer
//! drives the engine.

use tokio::sync::Mutex;

use crate::tidal_api::{StreamInfo, TidalClient};
use crate::TidalError;

/// Tidal-correct normalization: 0.8 * min(10^((rg + 4) / 20), 1 / peak)
pub fn compute_norm_gain(replay_gain: Option<f64>, peak_amplitude: Option<f64>) -> f64 {
    match replay_gain {
        Some(rg) => {
            let pre_amp = 4.0;
            let linear = 10.0_f64.powf((rg + pre_amp) / 20.0);
            let peak = peak_amplitude.filter(|&p| p > 0.0).unwrap_or(1.0);
            let sf = linear.min(1.0 / peak);
            0.8 * sf
        }
        None => 1.0,
    }
}

/// Resolved stream slot: everything a caller needs to arm/play a track.
/// `replay_gain`/`peak_amplitude` are `f64::NAN` when absent.
pub type ResolvedStream = (StreamInfo, String, f64, f64, f64, bool);

/// Tidal quality tiers to attempt, highest→lowest, given the user's quality
/// `ceiling` and whether confidential credentials (`client_secret`) are present.
/// Tiers above the ceiling are dropped; the two Hi-Res tiers require a secret
/// and are dropped without one. An unknown ceiling is treated as "max". The
/// result always includes "HIGH", so it is never empty.
fn quality_tiers(ceiling: &str, has_secret: bool) -> Vec<&'static str> {
    const ORDER: [&str; 4] = ["HI_RES_LOSSLESS", "HI_RES", "LOSSLESS", "HIGH"];
    let ceiling_idx = ORDER.iter().position(|&t| t == ceiling).unwrap_or(0);
    ORDER
        .iter()
        .enumerate()
        .filter(|(i, _)| *i >= ceiling_idx)
        .filter(|(_, &t)| has_secret || (t != "HI_RES_LOSSLESS" && t != "HI_RES"))
        .map(|(_, &t)| t)
        .collect()
}

/// Shared resolver: runs the quality cascade, builds the DASH/BTS URI, selects
/// replay-gain/peak per playback context, and computes the normalization gain.
/// Returns `(stream_info, uri, norm_gain, replay_gain, peak_amplitude, is_dash)`.
/// Does NOT touch `last_replay_gain`/`last_track_id` or start playback.
///
/// `max_quality` is the quality ceiling and `volume_normalization` the
/// ReplayGain switch.
pub async fn resolve_play_uri(
    tidal_client: &Mutex<TidalClient>,
    max_quality: &str,
    volume_normalization: bool,
    track_id: u64,
    use_track_gain: bool,
) -> Result<ResolvedStream, TidalError> {
    // Try quality tiers from highest to lowest.
    // Without client_secret, skip Hi-Res (those credentials typically return
    // encrypted DASH streams that require Widevine). With a secret, the
    // confidential PKCE credentials may return unencrypted Hi-Res BTS streams.
    let stream_info = {
        let mut client = crate::client_lock::lock(tidal_client, crate::client_lock::Caller::Resolve(track_id)).await;
        let has_secret = !client.client_secret.is_empty();
        let tiers = quality_tiers(max_quality, has_secret);

        let mut result: Option<StreamInfo> = None;
        let mut last_err: Option<TidalError> = None;
        for &tier in &tiers {
            match client.get_stream_url(track_id, tier).await {
                Ok(info) => {
                    result = Some(info);
                    break;
                }
                Err(e) if e.is_network() => return Err(e),
                // A rate-limit or a terminal-unplayable answer will not change
                // at a lower tier — over-requesting quality returns 200 with a
                // downgraded audioQuality, never an error. Walking the rest of
                // the cascade only multiplies the request count by 4.
                Err(e) if e.is_rate_limited() || e.is_terminal_unplayable() => return Err(e),
                Err(e) => last_err = Some(e),
            }
        }
        match result {
            Some(info) => info,
            // `quality_tiers` always yields at least "HIGH", so the loop runs
            // at least once and `last_err` is set on total failure.
            None => return Err(last_err.expect("quality_tiers always yields HIGH")),
        }
    };

    log::debug!(
        "[resolve_play_uri]: track_id={} — quality={:?}, bitDepth={:?}, sampleRate={:?}, codec={:?}, dash={}",
        track_id, stream_info.audio_quality, stream_info.bit_depth, stream_info.sample_rate,
        stream_info.codec, stream_info.manifest.is_some()
    );

    let is_dash = stream_info.manifest.is_some();
    let uri = if let Some(ref mpd) = stream_info.manifest {
        // DASH: pass MPD manifest as a data URI for GStreamer's dashdemux.
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(mpd.as_bytes());
        format!("data:application/dash+xml;base64,{}", b64)
    } else {
        // BTS: direct URL.
        stream_info.url.clone()
    };

    // Select replay gain + peak based on playback context (album vs mixed queue)
    let (selected_rg, selected_peak) = if use_track_gain {
        (
            stream_info
                .track_replay_gain
                .or(stream_info.album_replay_gain),
            stream_info
                .track_peak_amplitude
                .or(stream_info.album_peak_amplitude),
        )
    } else {
        (
            stream_info
                .album_replay_gain
                .or(stream_info.track_replay_gain),
            stream_info
                .album_peak_amplitude
                .or(stream_info.track_peak_amplitude),
        )
    };

    let norm_gain = if volume_normalization {
        compute_norm_gain(selected_rg, selected_peak)
    } else {
        1.0
    };
    log::debug!(
        "[resolve_play_uri]: normalization gain={:.3} (use_track_gain={}, rg={:?}, peak={:?})",
        norm_gain,
        use_track_gain,
        selected_rg,
        selected_peak
    );

    Ok((
        stream_info,
        uri,
        norm_gain,
        selected_rg.unwrap_or(f64::NAN),
        selected_peak.unwrap_or(f64::NAN),
        is_dash,
    ))
}

#[cfg(test)]
mod tests {
    use super::quality_tiers;
    use crate::TidalError;

    #[test]
    fn ceiling_max_with_secret_is_full_cascade() {
        assert_eq!(
            quality_tiers("HI_RES_LOSSLESS", true),
            vec!["HI_RES_LOSSLESS", "HI_RES", "LOSSLESS", "HIGH"]
        );
    }

    #[test]
    fn ceiling_max_without_secret_drops_hires() {
        // Reproduces the legacy no-secret branch exactly.
        assert_eq!(quality_tiers("HI_RES_LOSSLESS", false), vec!["LOSSLESS", "HIGH"]);
    }

    #[test]
    fn ceiling_lossless_caps_below_hires() {
        assert_eq!(quality_tiers("LOSSLESS", true), vec!["LOSSLESS", "HIGH"]);
        assert_eq!(quality_tiers("LOSSLESS", false), vec!["LOSSLESS", "HIGH"]);
    }

    #[test]
    fn ceiling_high_is_only_high() {
        assert_eq!(quality_tiers("HIGH", true), vec!["HIGH"]);
        assert_eq!(quality_tiers("HIGH", false), vec!["HIGH"]);
    }

    #[test]
    fn unknown_ceiling_falls_back_to_max() {
        assert_eq!(
            quality_tiers("GARBAGE", true),
            vec!["HI_RES_LOSSLESS", "HI_RES", "LOSSLESS", "HIGH"]
        );
    }

    #[test]
    fn always_includes_high_so_never_empty() {
        for ceiling in ["HI_RES_LOSSLESS", "LOSSLESS", "HIGH", "GARBAGE"] {
            for has_secret in [true, false] {
                let tiers = quality_tiers(ceiling, has_secret);
                assert!(tiers.contains(&"HIGH"), "ceiling={ceiling} secret={has_secret}");
            }
        }
    }

    #[test]
    fn terminal_and_rate_limited_errors_are_classified() {
        let rl = TidalError::Api {
            status: 429,
            body: String::new(),
        };
        assert!(rl.is_rate_limited());
        assert!(!rl.is_terminal_unplayable());

        let missing = TidalError::Api {
            status: 404,
            body: String::new(),
        };
        assert!(missing.is_terminal_unplayable());
        assert!(!missing.is_rate_limited());

        let gone = TidalError::Api {
            status: 410,
            body: String::new(),
        };
        assert!(gone.is_terminal_unplayable());
        assert!(!gone.is_rate_limited());

        let blocked = TidalError::Api {
            status: 451,
            body: String::new(),
        };
        assert!(blocked.is_terminal_unplayable());
        assert!(!blocked.is_rate_limited());

        let asset = TidalError::Api {
            status: 401,
            body: r#"{"status":401,"subStatus":4005}"#.into(),
        };
        assert!(asset.is_terminal_unplayable());

        let expired = TidalError::Api {
            status: 401,
            body: r#"{"status":401,"subStatus":11003}"#.into(),
        };
        assert!(!expired.is_terminal_unplayable());

        let bare_401 = TidalError::Api {
            status: 401,
            body: String::new(),
        };
        assert!(!bare_401.is_terminal_unplayable());

        let server = TidalError::Api {
            status: 500,
            body: String::new(),
        };
        assert!(!server.is_rate_limited());
        assert!(!server.is_terminal_unplayable());
    }
}
