//! Taking an ALSA card for exclusive output (Zeke).
//!
//! PipeWire keeps a card's PCM open while it owns the card's ReserveDevice1
//! name. Before opening a device exclusively, the engine asks for that name
//! ([`CardReserver`]), waits briefly for the previous owner to close the PCM,
//! and then opens it. The name is held for as long as the device is open
//! ([`Lease`]); dropping it hands the card back.
//!
//! This module is the policy only: the bus, the clock and the ALSA opens are
//! passed in, so it is tested without any of them.

use std::time::{Duration, Instant};

/// The error the engine reports for a device another client holds.
pub const DEVICE_BUSY: &str = "device_busy";

/// How long the previous owner may take to close the PCM after releasing
/// the card's name.
pub const RESERVE_SETTLE: Duration = Duration::from_secs(1);
/// How often the open is retried while waiting for the PCM to close.
pub const SETTLE_POLL: Duration = Duration::from_millis(50);

/// Ownership of a card's reservation name. Dropping it releases the name.
pub struct Hold(#[allow(dead_code)] Box<dyn Send>);

impl Hold {
    /// A hold whose `payload` releases the name when dropped.
    pub fn new(payload: impl Send + 'static) -> Self {
        Hold(Box::new(payload))
    }
}

impl std::fmt::Debug for Hold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hold")
    }
}

/// The result of asking for a card's reservation name.
#[derive(Debug)]
pub enum Reservation {
    /// No one owned the name; it is now ours.
    Free(Hold),
    /// The previous owner released it; it is now ours.
    Released(Hold),
    /// The owner refused to release it.
    Refused,
    /// There is no session bus.
    NoBus,
    /// A D-Bus error or timeout.
    Failed(String),
}

/// Asks for a card's reservation name.
pub trait CardReserver {
    fn reserve(&self, card: u32) -> Reservation;
}

/// The reservation that goes with an open exclusive device. The audio thread
/// keeps it next to the writer; dropping it releases the card.
#[derive(Debug)]
pub struct Lease {
    card: Option<u32>,
    hold: Option<Hold>,
}

impl Lease {
    pub fn new(card: Option<u32>, hold: Option<Hold>) -> Self {
        Lease { card, hold }
    }
}

/// Splits the lease of the device being closed for a device on `card`: its
/// hold is kept when both are on the same card, so the card is never handed
/// back in between. Otherwise the old lease comes back, to be dropped once
/// its device is closed.
pub fn reuse_lease(lease: Option<Lease>, card: Option<u32>) -> (Option<Hold>, Option<Lease>) {
    match lease {
        Some(l) if l.card.is_some() && l.card == card => (l.hold, None),
        other => (None, other),
    }
}

/// Open an exclusive device on `card`, taking its reservation name first.
///
/// - `held`: the name is already ours (same card, the device reopened), so
///   it isn't asked for again.
/// - `open`: the real open. It fails at once with [`DEVICE_BUSY`] while
///   another client holds the PCM, rather than waiting for it.
///
/// With no card number, no session bus or a bus error, the device is opened
/// without a reservation. A refused reservation is [`DEVICE_BUSY`] at once.
/// After the previous owner released the name, a busy open is retried until
/// [`RESERVE_SETTLE`]; otherwise it is [`DEVICE_BUSY`] at once. The hold is
/// dropped on every error, so the previous owner can take the card back.
pub fn acquire<T>(
    card: Option<u32>,
    reserver: &dyn CardReserver,
    held: Option<Hold>,
    mut open: impl FnMut() -> Result<T, String>,
    now: impl Fn() -> Instant,
    sleep: impl Fn(Duration),
) -> Result<(T, Option<Hold>), String> {
    let (hold, settle) = match (held, card) {
        (Some(hold), _) => (Some(hold), false),
        (None, None) => {
            log::info!("[acquire] no card number: opening without a reservation");
            (None, false)
        }
        (None, Some(card)) => match reserver.reserve(card) {
            Reservation::Free(hold) => (Some(hold), false),
            Reservation::Released(hold) => (Some(hold), true),
            Reservation::Refused => {
                log::warn!("[acquire] card {card}: the reservation was refused");
                return Err(DEVICE_BUSY.into());
            }
            Reservation::NoBus => {
                log::info!("[acquire] card {card}: no session bus, opening without a reservation");
                (None, false)
            }
            Reservation::Failed(e) => {
                log::warn!("[acquire] card {card}: reservation failed ({e}), opening without one");
                (None, false)
            }
        },
    };

    let deadline = now() + RESERVE_SETTLE;
    loop {
        match open() {
            Ok(pcm) => return Ok((pcm, hold)),
            Err(e) if e == DEVICE_BUSY && settle && now() < deadline => sleep(SETTLE_POLL),
            Err(e) => {
                drop(hold);
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Sets its flag when dropped, as a real hold releases the name.
    struct Flag(Arc<AtomicBool>);

    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A hold, and whether it has been released.
    pub(crate) fn fake_hold() -> (Hold, Arc<AtomicBool>) {
        let released = Arc::new(AtomicBool::new(false));
        (Hold::new(Flag(released.clone())), released)
    }

    /// Hands out one scripted reservation.
    struct Scripted {
        next: RefCell<Option<Reservation>>,
        calls: Cell<u32>,
    }

    impl Scripted {
        fn new(r: Reservation) -> Self {
            Scripted { next: RefCell::new(Some(r)), calls: Cell::new(0) }
        }
    }

    impl CardReserver for Scripted {
        fn reserve(&self, _card: u32) -> Reservation {
            self.calls.set(self.calls.get() + 1);
            self.next.borrow_mut().take().expect("reserved once")
        }
    }

    /// A clock that only moves when slept on.
    struct Clock {
        start: Instant,
        elapsed: Cell<Duration>,
    }

    impl Clock {
        fn new() -> Self {
            Clock { start: Instant::now(), elapsed: Cell::new(Duration::ZERO) }
        }
        fn now(&self) -> Instant {
            self.start + self.elapsed.get()
        }
        fn sleep(&self, d: Duration) {
            self.elapsed.set(self.elapsed.get() + d);
        }
    }

    /// An open that is busy `n` times, then succeeds, counting its calls.
    fn busy_times(n: u32, calls: &Cell<u32>) -> impl FnMut() -> Result<&'static str, String> + '_ {
        move || {
            calls.set(calls.get() + 1);
            if calls.get() <= n {
                Err(DEVICE_BUSY.into())
            } else {
                Ok("pcm")
            }
        }
    }

    #[test]
    fn reserved_then_opened() {
        let (hold, released) = fake_hold();
        let reserver = Scripted::new(Reservation::Released(hold));
        let clock = Clock::new();
        let opens = Cell::new(0);
        let (pcm, hold) = acquire(
            Some(1),
            &reserver,
            None,
            busy_times(2, &opens),
            || clock.now(),
            |d| clock.sleep(d),
        )
        .unwrap();
        assert_eq!(pcm, "pcm");
        assert_eq!(opens.get(), 3);
        assert_eq!(clock.elapsed.get(), 2 * SETTLE_POLL);
        assert!(hold.is_some());
        assert!(!released.load(Ordering::SeqCst), "the hold is returned, not released");
        drop(hold);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn reservation_refused_is_busy() {
        let reserver = Scripted::new(Reservation::Refused);
        let clock = Clock::new();
        let r = acquire::<()>(
            Some(1),
            &reserver,
            None,
            || panic!("no open"),
            || clock.now(),
            |d| clock.sleep(d),
        );
        assert_eq!(r.unwrap_err(), DEVICE_BUSY);
    }

    #[test]
    fn no_owner_opens_directly() {
        let (hold, released) = fake_hold();
        let reserver = Scripted::new(Reservation::Free(hold));
        let clock = Clock::new();
        let opens = Cell::new(0);
        let (_, hold) = acquire(
            Some(0),
            &reserver,
            None,
            busy_times(0, &opens),
            || clock.now(),
            |d| clock.sleep(d),
        )
        .unwrap();
        assert_eq!(opens.get(), 1);
        assert!(hold.is_some());
        assert!(!released.load(Ordering::SeqCst));
    }

    #[test]
    fn no_session_bus_opens_directly() {
        for r in [Reservation::NoBus, Reservation::Failed("timeout".into())] {
            let reserver = Scripted::new(r);
            let clock = Clock::new();
            let opens = Cell::new(0);
            let (_, hold) = acquire(
                Some(0),
                &reserver,
                None,
                busy_times(0, &opens),
                || clock.now(),
                |d| clock.sleep(d),
            )
            .unwrap();
            assert!(hold.is_none());
            assert_eq!(reserver.calls.get(), 1);
        }
        // Without a card number nothing is reserved.
        let reserver = Scripted::new(Reservation::Refused);
        let clock = Clock::new();
        let opens = Cell::new(0);
        let r = acquire(None, &reserver, None, busy_times(0, &opens), || clock.now(), |d| clock.sleep(d));
        assert!(r.is_ok());
        assert_eq!(reserver.calls.get(), 0);
    }

    #[test]
    fn still_busy_after_settle_is_busy_and_releases() {
        let (hold, released) = fake_hold();
        let reserver = Scripted::new(Reservation::Released(hold));
        let clock = Clock::new();
        let opens = Cell::new(0);
        let r = acquire(
            Some(1),
            &reserver,
            None,
            busy_times(u32::MAX, &opens),
            || clock.now(),
            |d| clock.sleep(d),
        );
        assert_eq!(r.unwrap_err(), DEVICE_BUSY);
        assert!(released.load(Ordering::SeqCst));
        assert!(clock.elapsed.get() <= RESERVE_SETTLE + SETTLE_POLL);
        assert!(clock.elapsed.get() >= RESERVE_SETTLE);

        // A free name with a busy PCM (PipeWire without reservation, a raw
        // ALSA client) is busy at once, without waiting.
        let (hold, released) = fake_hold();
        let reserver = Scripted::new(Reservation::Free(hold));
        let clock = Clock::new();
        let opens = Cell::new(0);
        let r = acquire(
            Some(1),
            &reserver,
            None,
            busy_times(u32::MAX, &opens),
            || clock.now(),
            |d| clock.sleep(d),
        );
        assert_eq!(r.unwrap_err(), DEVICE_BUSY);
        assert_eq!(opens.get(), 1);
        assert_eq!(clock.elapsed.get(), Duration::ZERO);
        assert!(released.load(Ordering::SeqCst));

        // A client that takes the PCM while the previous owner is closing
        // it makes every retry busy; any other open error ends the wait at
        // once. Both release.
        let (hold, released) = fake_hold();
        let reserver = Scripted::new(Reservation::Released(hold));
        let clock = Clock::new();
        let opens = Cell::new(0);
        let r = acquire::<()>(
            Some(1),
            &reserver,
            None,
            || {
                opens.set(opens.get() + 1);
                Err(if opens.get() == 1 { DEVICE_BUSY.into() } else { "Failed to open ALSA device: gone".into() })
            },
            || clock.now(),
            |d| clock.sleep(d),
        );
        assert_eq!(r.unwrap_err(), "Failed to open ALSA device: gone");
        assert_eq!(opens.get(), 2);
        assert_eq!(clock.elapsed.get(), SETTLE_POLL);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn a_kept_lease_skips_reservation() {
        let (hold, released) = fake_hold();
        let reserver = Scripted::new(Reservation::Refused);
        let clock = Clock::new();
        let (_, hold) =
            acquire(Some(1), &reserver, Some(hold), || Ok(()), || clock.now(), |d| clock.sleep(d))
                .unwrap();
        assert_eq!(reserver.calls.get(), 0);
        assert!(hold.is_some());
        assert!(!released.load(Ordering::SeqCst));
    }

    #[test]
    fn a_lease_on_the_same_card_is_kept_for_another_device() {
        let (hold, released) = fake_hold();
        let (held, stale) = reuse_lease(Some(Lease::new(Some(1), Some(hold))), Some(1));
        assert!(held.is_some());
        assert!(stale.is_none());
        assert!(!released.load(Ordering::SeqCst));
    }

    #[test]
    fn a_lease_on_another_card_is_released() {
        let (hold, released) = fake_hold();
        let (held, stale) = reuse_lease(Some(Lease::new(Some(1), Some(hold))), Some(2));
        assert!(held.is_none());
        assert!(!released.load(Ordering::SeqCst), "kept until the old device is closed");
        drop(stale);
        assert!(released.load(Ordering::SeqCst));

        // An unknown card never matches.
        let (hold, _) = fake_hold();
        let (held, _) = reuse_lease(Some(Lease::new(None, Some(hold))), None);
        assert!(held.is_none());
    }
}
