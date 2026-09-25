//! Timed access to the one `Mutex<TidalClient>` every TIDAL call goes
//! through. Stream resolution shares it
//! with browsing, so how long the player waits for it matters: the next
//! track is resolved 30 s before the current one ends.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, MutexGuard};

use crate::tidal_api::TidalClient;

/// Who is asking, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// The player resolving this track's stream (to play now, or next).
    Resolve(u64),
    /// Anything else: pages, metadata, covers' lookups.
    Other(&'static str),
}

/// Longest wait of a `Resolve` so far, in microseconds.
static WORST_RESOLVE_WAIT_US: AtomicU64 = AtomicU64::new(0);

/// The worst wait a stream resolve has had for the client since start.
pub fn worst_resolve_wait() -> Duration {
    Duration::from_micros(WORST_RESOLVE_WAIT_US.load(Ordering::Relaxed))
}

/// The client, held; logs how long it was held when dropped (debug).
pub struct ClientGuard<'a> {
    guard: MutexGuard<'a, TidalClient>,
    caller: Caller,
    since: Instant,
}

/// Lock the client and log the wait: always for a resolve (info), and at
/// debug for other callers.
pub async fn lock(client: &Mutex<TidalClient>, caller: Caller) -> ClientGuard<'_> {
    let asked = Instant::now();
    let guard = client.lock().await;
    let waited = asked.elapsed();
    match caller {
        Caller::Resolve(track) => {
            let us = waited.as_micros() as u64;
            let worst = WORST_RESOLVE_WAIT_US.fetch_max(us, Ordering::Relaxed).max(us);
            log::info!(
                "[client-lock] resolve {track} waited {:.1} ms (worst so far {:.1} ms)",
                us as f64 / 1000.0,
                worst as f64 / 1000.0
            );
        }
        Caller::Other(what) => {
            log::debug!("[client-lock] {what} waited {:.1} ms", waited.as_secs_f64() * 1000.0);
        }
    }
    ClientGuard { guard, caller, since: Instant::now() }
}

impl Deref for ClientGuard<'_> {
    type Target = TidalClient;
    fn deref(&self) -> &TidalClient {
        &self.guard
    }
}

impl DerefMut for ClientGuard<'_> {
    fn deref_mut(&mut self) -> &mut TidalClient {
        &mut self.guard
    }
}

impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        let held = self.since.elapsed().as_secs_f64() * 1000.0;
        match self.caller {
            Caller::Resolve(track) => log::debug!("[client-lock] resolve {track} held {held:.1} ms"),
            Caller::Other(what) => log::debug!("[client-lock] {what} held {held:.1} ms"),
        }
    }
}
