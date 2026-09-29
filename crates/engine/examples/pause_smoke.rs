//! Pause smoke test for exclusive playback: pauses and resumes a local file
//! many times, and seeks and changes tracks while paused, failing when any
//! engine call doesn't return within `STUCK`.
//!
//! The writer has to be paced by a device for its channel to fill (the hang
//! this guards against needs a full channel), so `null` won't do; `default`
//! (ALSA through PipeWire) paces and needs no card of its own. A silent file
//! keeps it quiet:
//!
//! ```text
//! gst-launch-1.0 audiotestsrc wave=silence num-buffers=3000 samplesperbuffer=1920 \
//!     ! audio/x-raw,rate=96000,channels=2,format=S16LE ! flacenc ! filesink location=a.flac
//! cargo run -p zeke-engine --example pause_smoke -- default a.flac b.flac [CYCLES]
//! ```

use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use zeke_engine::audio::AudioPlayer;
use zeke_engine::events;
use zeke_engine::SignalPathTracker;

const STUCK: Duration = Duration::from_secs(3);

type Call = Box<dyn FnOnce(&AudioPlayer) -> Result<(), String> + Send>;

struct Logger(Instant);

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("zeke") && m.level() <= log::Level::Info || m.level() <= log::Level::Warn
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

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err("usage: pause_smoke DEVICE FILE FILE [CYCLES]".into());
    }
    let cycles: u32 = args.get(3).map_or(Ok(50), |c| c.parse()).map_err(|_| "CYCLES is a number")?;
    let uri = |f: &str| {
        std::fs::canonicalize(f).map(|p| format!("file://{}", p.display())).map_err(|e| format!("{f}: {e}"))
    };
    let (a, b) = (uri(&args[1])?, uri(&args[2])?);

    let (tx, _rx) = events::channel();
    let signal_path = Arc::new(SignalPathTracker::new(tx.clone()));
    let player = Arc::new(AudioPlayer::new(tx, signal_path, Default::default()));
    player.set_exclusive_mode(true, Some(args[0].clone()))?;

    // Each call on a thread of its own, so a hung one is reported, not waited on.
    let call = |what: String, f: Call| -> Result<(), String> {
        let (done_tx, done_rx) = mpsc::channel();
        let p = Arc::clone(&player);
        std::thread::spawn(move || {
            let _ = done_tx.send(f(&p));
        });
        match done_rx.recv_timeout(STUCK) {
            Ok(result) => result.map_err(|e| format!("{what}: {e}")),
            Err(_) => Err(format!("{what} didn't return in {} s", STUCK.as_secs())),
        }
    };
    let wait = |ms: u64| std::thread::sleep(Duration::from_millis(ms));

    call("play".into(), Box::new(move |p| p.play_url(&a, None)))?;
    // Long enough for the writer's channel to fill behind the paced device.
    wait(1500);
    for i in 1..=cycles {
        call(format!("pause {i}"), Box::new(|p| p.pause()))?;
        wait(100);
        call(format!("resume {i}"), Box::new(|p| p.resume()))?;
        // Varied, so the pause lands at different points of the writer's loop.
        wait(150 + u64::from(i % 7) * 40);
    }
    println!("{cycles} pause/resume cycles returned");

    // Paused means the writer writes nothing: the position holds still.
    call("pause".into(), Box::new(|p| p.pause()))?;
    wait(300);
    let before = player.get_position()?;
    wait(2000);
    let after = player.get_position()?;
    if (after - before).abs() > 0.01 {
        return Err(format!("the position moved while paused: {before:.3} -> {after:.3} s"));
    }
    call("resume".into(), Box::new(|p| p.resume()))?;
    wait(1000);
    let moved = player.get_position()? - after;
    if moved < 0.5 {
        return Err(format!("one second after resuming, the position moved {moved:.3} s"));
    }
    println!("paused held still at {after:.2} s, and moved {moved:.2} s in the second after resuming");

    // A seek while playing lands where it was sent, and plays on.
    call("seek while playing".into(), Box::new(|p| p.seek(10.0)))?;
    wait(1000);
    let at = player.get_position()?;
    if !(10.5..11.6).contains(&at) {
        return Err(format!("one second after a seek to 10 s, the position is {at:.2} s"));
    }
    println!("seek while playing: {at:.2} s one second after a seek to 10 s");

    // A seek while paused writes nothing: the position is the target, and
    // stays there. Repeated, since a leak would be a race.
    call("pause before seek".into(), Box::new(|p| p.pause()))?;
    let mut worst = 0.0f32;
    for i in 0..20u16 {
        let to = 20.0 + f32::from(i);
        call(format!("seek {i} while paused"), Box::new(move |p| p.seek(to)))?;
        wait(300);
        let at = player.get_position()?;
        wait(700);
        let later = player.get_position()?;
        worst = worst.max((at - to).abs()).max((later - to).abs());
    }
    if worst > 0.005 {
        return Err(format!("seeks while paused moved the position up to {worst:.3} s off the target"));
    }
    println!("20 seeks while paused held their target (worst {worst:.4} s off)");
    call("seek to 30 while paused".into(), Box::new(|p| p.seek(30.0)))?;
    wait(300);
    call("resume after seek".into(), Box::new(|p| p.resume()))?;
    wait(1000);
    let at = player.get_position()?;
    if at <= 30.0 {
        return Err(format!("resumed at 30 s, the position is still {at:.2} s"));
    }
    println!("seek while paused: {at:.2} s one second after resuming");

    call("pause before next".into(), Box::new(|p| p.pause()))?;
    call("next while paused".into(), Box::new(move |p| p.play_url(&b, None)))?;
    wait(1000);
    let at = player.get_position()?;
    if !(0.3..3.0).contains(&at) {
        return Err(format!("one second into the next track, the position is {at:.2} s"));
    }
    println!("next while paused: playing, at {at:.2} s");

    call("pause before stop".into(), Box::new(|p| p.pause()))?;
    wait(500);
    call("stop while paused".into(), Box::new(|p| p.stop()))?;
    println!("ok");
    Ok(())
}
