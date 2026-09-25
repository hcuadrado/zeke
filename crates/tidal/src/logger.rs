//! A stderr logger that redacts every record (see `redact`), shared by the
//! CLI and the app.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::time::Instant;

struct Logger {
    /// Level for Zeke's own crates; everything else is capped at `Warn`.
    zeke: LevelFilter,
    /// Lines carry seconds since start, for reading transition timing.
    start: Instant,
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        if m.target().starts_with("zeke") {
            m.level() <= self.zeke
        } else {
            m.level() <= Level::Warn
        }
    }

    fn log(&self, r: &Record) {
        if self.enabled(r.metadata()) {
            let line = crate::redact::redact(&r.args().to_string());
            eprintln!("{:>8.3} {:<5} {line}", self.start.elapsed().as_secs_f64(), r.level());
        }
    }

    fn flush(&self) {}
}

pub fn init(verbose: bool) {
    let zeke = if verbose { LevelFilter::Debug } else { LevelFilter::Info };
    log::set_boxed_logger(Box::new(Logger { zeke, start: Instant::now() })).expect("logger set once");
    log::set_max_level(zeke);
}
