//! Keeps the computer from suspending while music plays (Zeke).
//!
//! A logind lock: `Manager.Inhibit("sleep", …, "block")` on the system bus
//! returns a file descriptor, and the lock lasts as long as it is open.
//! Dropping it, or the process exiting (a crash included), releases it. The
//! lock is `sleep` only, not `idle:sleep`: the screen must still blank and
//! lock. `gtk::Application::inhibit` is not used; on niri it returned a
//! cookie but no lock ever showed in `systemd-inhibit --list`.
//!
//! What the lock does and doesn't stop:
//! - systemd 257 and later refuse every suspend request while a block lock
//!   is held, including the same user's idle daemons. Older systemd ignores
//!   a block lock for the user who holds it, so those still suspend.
//! - A manual suspend is refused too while playing: pause first, or
//!   `systemctl suspend -i`.
//! - Closing the lid still suspends (`LidSwitchIgnoreInhibited=yes`).
//!
//! The session's event loop only sends on a `watch`; [`hold`] is the task
//! that talks to logind. It handles one change at a time, so calls never
//! overlap, and a quick play/pause flap arrives as one change.

use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;
use zbus::zvariant::OwnedFd;
use zeke_player::PlaybackState;

/// The first systemd that enforces a block lock against the user holding it.
const ENFORCED_SINCE: u32 = 257;
/// How long a call to logind or systemd may take: a stuck one must not
/// keep a lock from being dropped.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the computer is kept awake in this state: from the moment a
/// track starts loading, so a change of track doesn't let go of it.
pub fn keeps_awake(state: PlaybackState) -> bool {
    matches!(state, PlaybackState::Playing | PlaybackState::Loading)
}

/// Takes the lock. The lock is released by dropping what `take` returns.
pub trait Inhibitor {
    type Lock: Send;

    fn take(&mut self) -> impl Future<Output = Result<Self::Lock, String>> + Send;
}

/// Holds a lock while the watch says `true`, until the sender is dropped.
/// A failed `take` is retried the next time the value turns `true`, not
/// in a loop; only the first failure is a warning, so a system without
/// logind isn't told on every play.
pub async fn hold<I: Inhibitor>(mut wanted: watch::Receiver<bool>, mut inhibitor: I) {
    let mut lock: Option<I::Lock> = None;
    let mut warned = false;
    while wanted.changed().await.is_ok() {
        if !*wanted.borrow_and_update() {
            lock = None;
        } else if lock.is_none() {
            match inhibitor.take().await {
                Ok(taken) => lock = Some(taken),
                Err(e) if !std::mem::replace(&mut warned, true) => {
                    log::warn!("[app] can't keep the computer awake while playing: {e}")
                }
                Err(e) => log::debug!("[app] can't keep the computer awake while playing: {e}"),
            }
        }
    }
}

/// The real thing: logind on the system bus.
#[derive(Default)]
pub struct Logind {
    /// Connected on first use, then kept.
    bus: Option<zbus::Connection>,
    /// Whether the systemd version was checked (once, at the first lock).
    version_checked: bool,
}

impl Inhibitor for Logind {
    type Lock = OwnedFd;

    async fn take(&mut self) -> Result<OwnedFd, String> {
        let bus = match &self.bus {
            Some(bus) => bus.clone(),
            None => {
                let bus = timeout("system bus", zbus::Connection::system()).await?;
                self.bus.insert(bus).clone()
            }
        };
        let call = bus.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &("sleep", "Zeke", "Playing music", "block"),
        );
        let reply = match timeout("logind Inhibit", call).await {
            Ok(reply) => reply,
            Err(e) => {
                // The bus may have restarted: connect again next time.
                self.bus = None;
                return Err(e);
            }
        };
        let fd = reply.body().deserialize::<OwnedFd>().map_err(|e| format!("logind Inhibit reply: {e}"))?;
        // Apart, so a slow answer never holds up a pause.
        if !std::mem::replace(&mut self.version_checked, true) {
            tokio::spawn(warn_if_not_enforced(bus));
        }
        Ok(fd)
    }
}

/// `call`, given `CALL_TIMEOUT` to answer.
async fn timeout<T>(what: &str, call: impl Future<Output = zbus::Result<T>>) -> Result<T, String> {
    match tokio::time::timeout(CALL_TIMEOUT, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(format!("{what}: {e}")),
        Err(_) => Err(format!("{what}: no answer in {} s", CALL_TIMEOUT.as_secs())),
    }
}

/// Logs once if this systemd lets the user's own idle daemons suspend
/// through the lock. Best effort: the version is only for the log. It is
/// systemd's own manager that has it; logind doesn't.
async fn warn_if_not_enforced(bus: zbus::Connection) {
    let call = bus.call_method(
        Some("org.freedesktop.systemd1"),
        "/org/freedesktop/systemd1",
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &("org.freedesktop.systemd1.Manager", "Version"),
    );
    let version = match timeout("systemd Version", call).await {
        Ok(reply) => reply.body().deserialize::<zbus::zvariant::OwnedValue>().ok().and_then(|v| String::try_from(v).ok()),
        Err(e) => {
            log::debug!("[app] {e}");
            None
        }
    };
    if let Some(version) = version
        && major(&version).is_some_and(|m| m < ENFORCED_SINCE)
    {
        log::warn!(
            "[app] systemd {version} ignores a sleep lock for the user who holds it (enforced since {ENFORCED_SINCE}): \
             the computer may still suspend while playing"
        );
    }
}

/// The number a systemd version starts with ("257.9-1" is 257).
fn major(version: &str) -> Option<u32> {
    let digits = version.find(|c: char| !c.is_ascii_digit()).unwrap_or(version.len());
    version[..digits].parse().ok()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    type Log = Arc<Mutex<Vec<&'static str>>>;

    struct Fake {
        log: Log,
        /// Results for the next takes, front first; empty means success.
        fail: Vec<bool>,
        /// When set, the next take waits for it: a call still on its way.
        gate: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    struct Lock(Log);

    impl Drop for Lock {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("drop");
        }
    }

    impl Inhibitor for Fake {
        type Lock = Lock;

        async fn take(&mut self) -> Result<Lock, String> {
            if let Some(gate) = self.gate.take() {
                let _ = gate.await;
            }
            if !self.fail.is_empty() && self.fail.remove(0) {
                self.log.lock().unwrap().push("fail");
                return Err("no logind".into());
            }
            self.log.lock().unwrap().push("take");
            Ok(Lock(Arc::clone(&self.log)))
        }
    }

    /// Sends each value, letting the task see it before the next.
    async fn drive(fail: Vec<bool>, values: &[bool]) -> Vec<&'static str> {
        let log = Log::default();
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(hold(rx, Fake { log: Arc::clone(&log), fail, gate: None }));
        for &v in values {
            tx.send(v).unwrap();
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        drop(tx);
        task.await.unwrap();
        log.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn play_pause_play_stop_takes_and_drops_in_turn() {
        let seen = drive(vec![], &[true, false, true, false]).await;
        assert_eq!(seen, ["take", "drop", "take", "drop"]);
    }

    #[tokio::test]
    async fn the_lock_is_dropped_when_the_session_ends() {
        assert_eq!(drive(vec![], &[true]).await, ["take", "drop"]);
    }

    #[tokio::test]
    async fn a_failed_take_is_retried_on_the_next_play_only() {
        let seen = drive(vec![true], &[true, false, true, false]).await;
        assert_eq!(seen, ["fail", "take", "drop"]);
    }

    #[tokio::test]
    async fn a_pause_while_the_lock_is_on_its_way_drops_it() {
        let log = Log::default();
        let (open, gate) = tokio::sync::oneshot::channel();
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(hold(rx, Fake { log: Arc::clone(&log), fail: vec![], gate: Some(gate) }));
        tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // Paused, played and paused again before logind answers: one change.
        tx.send(false).unwrap();
        tx.send(true).unwrap();
        tx.send(false).unwrap();
        open.send(()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(*log.lock().unwrap(), ["take", "drop"], "no lock left held");
        drop(tx);
        task.await.unwrap();
        assert_eq!(*log.lock().unwrap(), ["take", "drop"]);
    }

    #[test]
    fn playing_and_loading_keep_it_awake() {
        use PlaybackState::*;
        for (state, awake) in [(Playing, true), (Loading, true), (Paused, false), (Stopped, false), (Restored, false)] {
            assert_eq!(keeps_awake(state), awake, "{state:?}");
        }
    }

    #[test]
    fn systemd_versions_parse() {
        assert_eq!(major("261.2"), Some(261));
        assert_eq!(major("257.9-1"), Some(257));
        assert_eq!(major("256"), Some(256));
        assert_eq!(major("v1"), None);
    }
}
