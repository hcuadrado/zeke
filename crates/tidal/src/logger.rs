//! A stderr logger that redacts every record (see `redact`), shared by the
//! CLI and the app. The app also writes it to a file (`also_to_file`), so a
//! session that went wrong can be read after a restart.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

/// Runs kept besides the current one: `zeke.log.1` is the previous run.
/// More than one, because a stuck player is often restarted more than once.
const KEEP: u32 = 3;
/// A run's file stops growing here (a debug run logs a lot).
const MAX_BYTES: u64 = 20 * 1024 * 1024;

struct Logger {
    /// Level for Zeke's own crates; everything else is capped at `Warn`.
    zeke: LevelFilter,
    /// Lines carry seconds since start, for reading transition timing.
    start: Instant,
}

static FILE: OnceLock<Mutex<LogFile>> = OnceLock::new();

struct LogFile {
    file: File,
    written: u64,
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
            let elapsed = self.start.elapsed().as_secs_f64();
            eprintln!("{elapsed:>8.3} {:<5} {line}", r.level());
            if let Some(file) = FILE.get() {
                let mut f = file.lock().unwrap_or_else(|p| p.into_inner());
                // Wall-clock time too: the monotonic clock stops while the
                // machine sleeps, and the file is read next to the journal.
                f.write(&format!("{} {elapsed:>8.3} {:<5} {line}\n", utc_now(), r.level()));
            }
        }
    }

    fn flush(&self) {}
}

impl LogFile {
    fn write(&mut self, line: &str) {
        if self.written >= MAX_BYTES {
            return;
        }
        self.written += line.len() as u64;
        let line = if self.written >= MAX_BYTES { "log limit reached; later lines are not written\n" } else { line };
        // Unbuffered: the lines before a hang or a kill are the useful ones.
        let _ = self.file.write_all(line.as_bytes());
    }
}

pub fn init(verbose: bool) {
    let zeke = if verbose { LevelFilter::Debug } else { LevelFilter::Info };
    log::set_boxed_logger(Box::new(Logger { zeke, start: Instant::now() })).expect("logger set once");
    log::set_max_level(zeke);
}

/// From now on, also write the log to `path`; the previous runs' files are
/// kept as `path.1` … `path.3`. Only the first call opens a file. One that
/// can't be opened is logged; logging to stderr goes on.
pub fn also_to_file(path: &Path) {
    if FILE.get().is_some() {
        return;
    }
    match open_rotated(path) {
        Ok(file) => {
            let _ = FILE.set(Mutex::new(LogFile { file, written: 0 }));
        }
        Err(e) => log::warn!("[app] can't write the log to {}: {e}", path.display()),
    }
}

/// Shift `path` → `path.1` → … → `path.KEEP` (dropping the oldest) and open
/// a new `path`.
fn open_rotated(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let numbered = |n: u32| -> PathBuf {
        let mut p = path.as_os_str().to_owned();
        p.push(format!(".{n}"));
        p.into()
    };
    for n in (1..KEEP).rev() {
        let _ = fs::rename(numbered(n), numbered(n + 1));
    }
    let _ = fs::rename(path, numbered(1));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// The time now in UTC, as `2026-09-28T19:57:51.574Z`.
fn utc_now() -> String {
    let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    utc(since.as_secs(), since.subsec_millis())
}

fn utc(secs: u64, millis: u32) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_times() {
        assert_eq!(utc(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc(951_782_400, 5), "2000-02-29T00:00:00.005Z");
        assert_eq!(utc(1_790_625_471, 574), "2026-09-28T19:57:51.574Z");
    }

    #[test]
    fn keeps_the_previous_runs() {
        let dir = std::env::temp_dir().join(format!("zeke-log-test-{}", std::process::id()));
        let path = dir.join("zeke.log");
        for run in 1..=5 {
            let mut f = open_rotated(&path).unwrap();
            write!(f, "run {run}").unwrap();
        }
        let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(read("zeke.log"), "run 5");
        assert_eq!(read("zeke.log.1"), "run 4");
        assert_eq!(read("zeke.log.3"), "run 2");
        assert!(!dir.join("zeke.log.4").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stops_at_the_size_limit() {
        let dir = std::env::temp_dir().join(format!("zeke-log-limit-{}", std::process::id()));
        let path = dir.join("zeke.log");
        let mut f = LogFile { file: open_rotated(&path).unwrap(), written: MAX_BYTES - 10 };
        f.write("fits\n");
        f.write("goes over the limit\n");
        f.write("never written\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "fits\nlog limit reached; later lines are not written\n");
        fs::remove_dir_all(dir).unwrap();
    }
}
