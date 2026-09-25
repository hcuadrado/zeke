//! Gapless smoke test for the engine alone, without TIDAL or the player.
//!
//! Plays local files back to back through `set_next_track` and logs every
//! engine event with a timestamp, plus the ALSA writer's silence and xrun
//! counters at each boundary. On the `null` ALSA device it needs no sound card.
//!
//! ```text
//! cargo run -p zeke-engine --example gapless_smoke -- [--bit-perfect] [--normal] DEVICE FILE...
//! cargo run -p zeke-engine --example gapless_smoke -- null a.flac b.flac c.flac
//! ```
//!
//! `--seek-at T:TO` seeks to TO seconds once T seconds have passed, e.g. into
//! the window between concat's switch and the audible boundary.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// `recv_blocking` with a timeout, for the smoke test's single timer.
trait RecvTimeout<T> {
    fn recv_blocking_timeout(&self, d: Duration) -> Option<T>;
}

impl<T> RecvTimeout<T> for async_channel::Receiver<T> {
    fn recv_blocking_timeout(&self, d: Duration) -> Option<T> {
        let until = Instant::now() + d;
        loop {
            if let Ok(v) = self.try_recv() {
                return Some(v);
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

use zeke_engine::audio::{self, AudioPlayer};
use zeke_engine::events::{self, EngineEvent};
use zeke_engine::SignalPathTracker;

struct Logger(Instant);

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("zeke") && m.level() <= log::Level::Info
            || m.level() <= log::Level::Warn
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("{:>8.3} {:<5} {}", self.0.elapsed().as_secs_f64(), r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

fn main() -> Result<(), String> {
    let start = Instant::now();
    log::set_logger(Box::leak(Box::new(Logger(start)))).ok();
    log::set_max_level(log::LevelFilter::Info);

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let bit_perfect = take_flag(&mut args, "--bit-perfect");
    let normal = take_flag(&mut args, "--normal");
    let seek_at: Option<(f64, f32)> = match args.iter().position(|a| a == "--seek-at") {
        Some(i) => {
            let spec = args.remove(i + 1);
            args.remove(i);
            let (t, to) = spec.split_once(':').ok_or("--seek-at T:TO")?;
            Some((t.parse().map_err(|_| "--seek-at T")?, to.parse().map_err(|_| "--seek-at TO")?))
        }
        None => None,
    };
    if args.len() < 3 {
        return Err("usage: gapless_smoke [--bit-perfect] [--normal] DEVICE FILE FILE...".into());
    }
    let device = args.remove(0);
    let uris: Vec<String> = args
        .iter()
        .map(|f| {
            std::fs::canonicalize(f)
                .map(|p| format!("file://{}", p.display()))
                .map_err(|e| format!("{f}: {e}"))
        })
        .collect::<Result<_, _>>()?;

    let (tx, rx) = events::channel();
    let signal_path = Arc::new(SignalPathTracker::new(tx.clone()));
    let player = AudioPlayer::new(tx, signal_path, Default::default());
    player.set_exclusive_mode(!normal, Some(device))?;
    player.set_bit_perfect(bit_perfect)?;

    let arm = |i: usize| -> Result<(), String> {
        match uris.get(i) {
            Some(uri) => player.set_next_track(uri.clone(), 1.0, i as u64, format!("q{i}"), f64::NAN, f64::NAN, false),
            None => Ok(()),
        }
    };
    player.play_url(&uris[0], None)?;
    // Right away: `null` doesn't pace, so a track is written in well under a second.
    arm(1)?;

    let t = |at: Instant| at.duration_since(start).as_secs_f64();
    let mut current = 0usize;
    let mut seek_at = seek_at;
    loop {
        let ev = match seek_at {
            Some((at, to)) => {
                let wait = Duration::from_secs_f64((at - start.elapsed().as_secs_f64()).max(0.0));
                match rx.recv_blocking_timeout(wait) {
                    Some(ev) => ev,
                    None => {
                        seek_at = None;
                        println!("{:>8.3} seek to {to}s (playing track {current})", t(Instant::now()));
                        player.seek(to)?;
                        continue;
                    }
                }
            }
            None => rx
                .recv_blocking()
                .map_err(|_| "engine event channel closed".to_string())?,
        };
        let (silence, xruns) = audio::writer_counters();
        match ev {
            EngineEvent::TrackAdvanced { track_id, .. } => {
                let pos = player.get_position().unwrap_or(-1.0);
                println!(
                    "{:>8.3} track-advanced {current} -> {track_id} (position {pos:.3}s, silence writes {silence}, xruns {xruns})",
                    t(Instant::now())
                );
                current = track_id as usize;
                arm(current + 1)?;
            }
            EngineEvent::TrackFinished => {
                println!("{:>8.3} track-finished {current} (silence writes {silence}, xruns {xruns})", t(Instant::now()));
                break;
            }
            EngineEvent::AudioError { kind, message } => {
                println!("{:>8.3} audio-error {kind}: {}", t(Instant::now()), message.unwrap_or_default());
                break;
            }
            EngineEvent::AudioResampled { from, to } => {
                println!("{:>8.3} resampled {from} -> {to}", t(Instant::now()));
            }
            _ => {}
        }
    }
    player.stop()?;
    Ok(())
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}
