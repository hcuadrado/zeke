//! The freedesktop ReserveDevice1 protocol on the session bus (Zeke).
//!
//! Audio servers own `org.freedesktop.ReserveDevice1.Audio<card>` for each
//! card they use: WirePlumber at priority -20, PulseAudio at 0. An app that
//! wants a card asks the owner to give it up (`RequestRelease` with its own
//! priority) and then takes the name over. [`DbusReserver`] does that for
//! exclusive output and keeps the name, refusing release requests, until the
//! [`Hold`] is dropped.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

use crate::acquire::{CardReserver, Hold, Reservation};
use crate::devices;

/// Zeke's priority for a card: above WirePlumber (-20) and PulseAudio (0),
/// below JACK-style apps that ask for more.
pub const RESERVE_PRIORITY: i32 = 10;
/// How many times the name is asked for after the owner agreed to release
/// it, since the owner may take it back first.
const MAX_TAKEOVER_ATTEMPTS: u32 = 2;
/// How long an owner that agreed to release the name gets to hand it over
/// before it is replaced. WirePlumber has always handed it over by the
/// first check; the rest is margin for a busy WirePlumber.
const RELEASE_WAIT: Duration = Duration::from_millis(200);
/// How often the name is checked meanwhile.
const RELEASE_POLL: Duration = Duration::from_millis(20);
/// The longest a bus call may take.
const METHOD_TIMEOUT: Duration = Duration::from_secs(2);
const INTERFACE: &str = "org.freedesktop.ReserveDevice1";

fn bus_name(card: u32) -> String {
    format!("org.freedesktop.ReserveDevice1.Audio{card}")
}

fn object_path(card: u32) -> String {
    format!("/org/freedesktop/ReserveDevice1/Audio{card}")
}

/// What `RequestName` answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameReply {
    PrimaryOwner,
    /// Owned by someone else; we are next in line for it.
    InQueue,
    Other,
}

/// The bus calls of the protocol, behind a trait so [`negotiate`] is tested
/// without a bus.
trait Bus {
    /// `RequestName`, queueing for the name if it is owned, plus
    /// `ReplaceExisting` when `replace`. Never `AllowReplacement`: the name is
    /// only given up by dropping the hold.
    fn request_name(&self, name: &str, replace: bool) -> Result<NameReply, String>;
    /// `RequestRelease(priority)` on the name's owner.
    fn request_release(&self, name: &str, path: &str, priority: i32) -> Result<bool, String>;
    /// Give the name up, or leave its queue.
    fn release_name(&self, name: &str) -> Result<(), String>;
    /// Whether the name is ours now.
    fn owns(&self, name: &str) -> Result<bool, String>;
    /// The owner's `ApplicationName`, for the log.
    fn owner_app(&self, _name: &str, _path: &str) -> Option<String> {
        None
    }
}

/// How asking for a name ended. On `Refused` and `Failed` we may still be
/// in the name's queue: the caller releases it.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Free,
    Released,
    Refused,
    Failed(String),
}

/// Take `name`: at once if no one owns it, else by asking the owner to
/// release it. We queue for the name first, so when the owner lets go the
/// bus hands it to us before the owner can take it back (WirePlumber asks
/// for it again as soon as it is free). An owner that agrees to release gets
/// [`RELEASE_WAIT`] to hand the name over; WirePlumber closes its device
/// first, and replacing it before then leaves the card's outputs muted. One
/// that keeps the name past the wait is replaced, which is what owners like
/// PulseAudio and JACK expect. That is tried [`MAX_TAKEOVER_ATTEMPTS`] times.
fn negotiate(bus: &dyn Bus, name: &str, path: &str, now: &dyn Fn() -> Instant, sleep: &dyn Fn(Duration)) -> Outcome {
    match bus.request_name(name, false) {
        Ok(NameReply::PrimaryOwner) => return Outcome::Free,
        Ok(NameReply::InQueue) => {}
        Ok(NameReply::Other) => return Outcome::Failed(format!("unexpected reply asking for {name}")),
        Err(e) => return Outcome::Failed(e),
    }
    let owner = bus.owner_app(name, path).unwrap_or_else(|| "its owner".into());
    for attempt in 1..=MAX_TAKEOVER_ATTEMPTS {
        log::info!("[reserve] takeover attempt {attempt}: asking {owner} to release {name}");
        match bus.request_release(name, path, RESERVE_PRIORITY) {
            Ok(true) => {}
            Ok(false) => {
                log::info!("[reserve] {owner} refused to release {name}");
                return Outcome::Refused;
            }
            Err(e) => return Outcome::Failed(e),
        }
        let asked = now();
        match wait_for_handover(bus, name, now, sleep) {
            Ok(true) => {
                let waited = now().saturating_duration_since(asked).as_millis();
                log::info!("[reserve] reserved {name} on takeover attempt {attempt} (released by {owner} after {waited} ms)");
                return Outcome::Released;
            }
            Ok(false) => {}
            Err(e) => return Outcome::Failed(e),
        }
        log::info!("[reserve] {owner} didn't release {name} within {} ms; replacing it", RELEASE_WAIT.as_millis());
        match bus.request_name(name, true) {
            Ok(NameReply::PrimaryOwner) => {
                log::info!("[reserve] reserved {name} on takeover attempt {attempt} (replaced {owner})");
                return Outcome::Released;
            }
            Ok(_) => log::info!("[reserve] takeover attempt {attempt} lost to {owner}; retrying"),
            Err(e) => return Outcome::Failed(e),
        }
    }
    log::warn!("[reserve] gave up on {name} after {MAX_TAKEOVER_ATTEMPTS} takeover attempts");
    Outcome::Refused
}

/// Whether the bus handed `name` to us within [`RELEASE_WAIT`].
fn wait_for_handover(bus: &dyn Bus, name: &str, now: &dyn Fn() -> Instant, sleep: &dyn Fn(Duration)) -> Result<bool, String> {
    let deadline = now() + RELEASE_WAIT;
    loop {
        if bus.owns(name)? {
            return Ok(true);
        }
        if now() >= deadline {
            return Ok(false);
        }
        sleep(RELEASE_POLL);
    }
}

/// The `org.freedesktop.ReserveDevice1` object other apps call while Zeke
/// owns a card's name.
struct Reserve {
    device_name: String,
}

#[zbus::interface(name = "org.freedesktop.ReserveDevice1")]
impl Reserve {
    /// Refused: Zeke keeps the card for the whole exclusive session.
    fn request_release(&self, priority: i32, #[zbus(header)] header: zbus::message::Header<'_>) -> bool {
        let who = header.sender().map_or_else(|| "someone".to_string(), |s| s.to_string());
        log::info!("[reserve] refused to release {} to {who} (priority {priority})", self.device_name);
        false
    }

    #[zbus(property)]
    fn priority(&self) -> i32 {
        RESERVE_PRIORITY
    }

    #[zbus(property)]
    fn application_name(&self) -> String {
        "Zeke".into()
    }

    #[zbus(property)]
    fn application_device_name(&self) -> String {
        self.device_name.clone()
    }
}

/// [`Bus`] on a zbus connection. Remembers whether the connection itself
/// failed, so it is replaced.
struct ZbusBus<'a> {
    conn: &'a Connection,
    broken: Cell<bool>,
}

impl ZbusBus<'_> {
    fn err(&self, e: zbus::Error) -> String {
        if matches!(&e, zbus::Error::InputOutput(io) if io.kind() != std::io::ErrorKind::TimedOut) {
            self.broken.set(true);
        }
        e.to_string()
    }

    /// A method of the bus itself.
    fn call_dbus<B>(&self, method: &str, body: &B) -> zbus::Result<zbus::message::Message>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.conn.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), method, body)
    }

    /// `ReleaseName` on the bus itself, bypassing zbus's list of names.
    fn release_directly(&self, name: &str) -> Result<(), String> {
        self.call_dbus("ReleaseName", &(name,)).map(drop).map_err(|e| self.err(e))
    }
}

impl Bus for ZbusBus<'_> {
    fn request_name(&self, name: &str, replace: bool) -> Result<NameReply, String> {
        let flags: u32 = if replace { RequestNameFlags::ReplaceExisting as u32 } else { 0 };
        // On the bus itself: zbus answers a name it saw queued from its own
        // list, so a later ReplaceExisting would never reach the bus.
        let reply = self
            .call_dbus("RequestName", &(name, flags))
            .and_then(|reply| reply.body().deserialize::<RequestNameReply>())
            .map_err(|e| self.err(e))?;
        Ok(match reply {
            // AlreadyOwner: the name is ours, e.g. after a timed-out request
            // the bus granted anyway.
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => NameReply::PrimaryOwner,
            RequestNameReply::InQueue => NameReply::InQueue,
            RequestNameReply::Exists => NameReply::Other,
        })
    }

    fn request_release(&self, name: &str, path: &str, priority: i32) -> Result<bool, String> {
        self.conn
            .call_method(Some(name), path, Some(INTERFACE), "RequestRelease", &(priority,))
            .and_then(|reply| reply.body().deserialize::<bool>())
            .map_err(|e| self.err(e))
    }

    fn release_name(&self, name: &str) -> Result<(), String> {
        match self.conn.release_name(name) {
            Ok(true) => Ok(()),
            // zbus only releases names it saw granted; one whose request timed
            // out may still be ours, so ask the bus directly. zbus also drops
            // the name from its list before calling the bus, so a failed call
            // is retried the same way.
            other => {
                if let Err(e) = other {
                    log::warn!("[reserve] releasing {name}: {}", self.err(e));
                }
                self.release_directly(name)
            }
        }
    }

    fn owns(&self, name: &str) -> Result<bool, String> {
        match self.call_dbus("GetNameOwner", &(name,)) {
            Ok(reply) => {
                let owner: String = reply.body().deserialize().map_err(|e| self.err(e))?;
                Ok(self.conn.unique_name().is_some_and(|me| me.as_str() == owner))
            }
            Err(zbus::Error::MethodError(error, _, _)) if error.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" => {
                Ok(false)
            }
            Err(e) => Err(self.err(e)),
        }
    }

    fn owner_app(&self, name: &str, path: &str) -> Option<String> {
        owner_property(self.conn, name, path, "ApplicationName")
    }
}

/// A property of the owner of `name`, or `None` without one.
fn owner_property<T>(conn: &Connection, name: &str, path: &str, property: &str) -> Option<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue>,
{
    let reply = conn
        .call_method(Some(name), path, Some("org.freedesktop.DBus.Properties"), "Get", &(INTERFACE, property))
        .ok()?;
    let value: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
    T::try_from(value).ok()
}

/// A held name: dropping it gives the name up and stops serving the
/// `Reserve` object. The bus also frees the name if Zeke exits.
struct Owned {
    conn: Connection,
    name: String,
    path: String,
}

impl Drop for Owned {
    fn drop(&mut self) {
        let bus = ZbusBus { conn: &self.conn, broken: Cell::new(false) };
        match bus.release_name(&self.name) {
            Ok(()) => log::info!("[reserve] released {}", self.name),
            Err(e) => log::warn!("[reserve] releasing {}: {e}", self.name),
        }
        unserve(&self.conn, &self.path);
    }
}

fn unserve(conn: &Connection, path: &str) {
    if let Err(e) = conn.object_server().remove::<Reserve, _>(path) {
        log::warn!("[reserve] removing {path}: {e}");
    }
}

/// Takes cards' reservation names on the session bus.
///
/// The connection is made when first needed and kept only while it works:
/// with no bus, each acquisition tries again, since the bus may come up
/// after Zeke and a failed connect is a fast local error.
pub struct DbusReserver {
    conn: Mutex<Option<Connection>>,
    no_bus_logged: AtomicBool,
}

impl DbusReserver {
    const fn new() -> Self {
        DbusReserver { conn: Mutex::new(None), no_bus_logged: AtomicBool::new(false) }
    }

    fn connection(&self) -> Option<Connection> {
        let mut cached = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if cached.is_none() {
            match zbus::blocking::connection::Builder::session().and_then(|b| b.method_timeout(METHOD_TIMEOUT).build()) {
                Ok(conn) => {
                    self.no_bus_logged.store(false, Ordering::Relaxed);
                    *cached = Some(conn);
                }
                Err(e) => {
                    if !self.no_bus_logged.swap(true, Ordering::Relaxed) {
                        log::info!("[reserve] no session bus: {e}");
                    }
                }
            }
        }
        cached.clone()
    }

    fn forget_connection(&self) {
        *self.conn.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The engine's reserver.
pub static RESERVER: DbusReserver = DbusReserver::new();

impl CardReserver for DbusReserver {
    fn reserve(&self, card: u32) -> Reservation {
        let Some(conn) = self.connection() else {
            return Reservation::NoBus;
        };
        let (name, path) = (bus_name(card), object_path(card));
        // Served first, so a release request right after the takeover is
        // answered.
        if let Err(e) = conn.object_server().at(path.as_str(), Reserve { device_name: format!("hw:{card}") }) {
            return Reservation::Failed(format!("serving {path}: {e}"));
        }
        let bus = ZbusBus { conn: &conn, broken: Cell::new(false) };
        let outcome = negotiate(&bus, &name, &path, &Instant::now, &std::thread::sleep);
        match outcome {
            Outcome::Free | Outcome::Released => {
                let hold = Hold::new(Owned { conn: conn.clone(), name, path });
                if outcome == Outcome::Free {
                    log::info!("[reserve] reserved Audio{card}, which no one owned");
                    Reservation::Free(hold)
                } else {
                    Reservation::Released(hold)
                }
            }
            Outcome::Refused => {
                // Out of the name's queue.
                let _ = bus.release_name(&name);
                unserve(&conn, &path);
                Reservation::Refused
            }
            Outcome::Failed(e) => {
                // In case the name was ours after all (a timed-out reply).
                let _ = bus.release_name(&name);
                unserve(&conn, &path);
                if bus.broken.get() {
                    self.forget_connection();
                }
                Reservation::Failed(e)
            }
        }
    }
}

/// The priority of the app that owns `card`'s name, or `None` if no one
/// does, there is no bus, or it can't be read.
pub fn owner_priority(card: u32) -> Option<i32> {
    let conn = RESERVER.connection()?;
    owner_property(&conn, &bus_name(card), &object_path(card), "Priority")
}

/// The `owner_pid` of a PCM substream's `status` file, which only has one
/// while the PCM is open.
fn parse_owner_pid(status: &str) -> Option<u32> {
    status.lines().find_map(|l| {
        let (key, value) = l.split_once(':')?;
        (key.trim() == "owner_pid").then(|| value.trim().parse().ok()).flatten()
    })
}

/// The name of the process that has `device`'s PCM open, if any.
pub fn pcm_holder(device: &str) -> Option<String> {
    let dir = devices::proc_card_dir(device)?;
    let dev = devices::device_number(device)?;
    let status = std::fs::read_to_string(format!("{dir}/pcm{dev}p/sub0/status")).ok()?;
    let pid = parse_owner_pid(&status)?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim().to_string())
}

/// Whether PipeWire has `device`'s PCM open.
pub fn pcm_holder_is_pipewire(device: &str) -> bool {
    pcm_holder(device).as_deref() == Some("pipewire")
}

/// Whether a busy device can be taken when it is played: PipeWire holds it,
/// under a name whose owner ranks below Zeke. The owner alone doesn't say,
/// since WirePlumber keeps the name even while a raw ALSA client has the PCM.
pub fn takeable(owner_priority: Option<i32>, holder_is_pipewire: bool) -> bool {
    holder_is_pipewire && owner_priority.is_some_and(|p| p < RESERVE_PRIORITY)
}

/// [`takeable`] for a device, as the startup check sees it. Takes nothing.
pub fn startup_takeable(device: &str) -> bool {
    pcm_holder_is_pipewire(device) && takeable(devices::card_index(device).and_then(owner_priority), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// A bus that answers from a script and records the calls.
    #[derive(Default)]
    struct Scripted {
        names: RefCell<VecDeque<Result<NameReply, String>>>,
        releases: RefCell<VecDeque<Result<bool, String>>>,
        owns: RefCell<VecDeque<Result<bool, String>>>,
        calls: RefCell<Vec<String>>,
    }

    impl Scripted {
        fn new(names: Vec<Result<NameReply, String>>, releases: Vec<Result<bool, String>>) -> Self {
            Scripted { names: RefCell::new(names.into()), releases: RefCell::new(releases.into()), ..Default::default() }
        }
        /// Whether the name is ours at each check, in order.
        fn owns(self, owns: Vec<Result<bool, String>>) -> Self {
            self.owns.replace(owns.into());
            self
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Bus for Scripted {
        fn request_name(&self, _name: &str, replace: bool) -> Result<NameReply, String> {
            self.calls.borrow_mut().push(format!("request_name({replace})"));
            self.names.borrow_mut().pop_front().expect("scripted request_name")
        }
        fn request_release(&self, _name: &str, _path: &str, priority: i32) -> Result<bool, String> {
            self.calls.borrow_mut().push(format!("request_release({priority})"));
            self.releases.borrow_mut().pop_front().expect("scripted request_release")
        }
        fn release_name(&self, _name: &str) -> Result<(), String> {
            self.calls.borrow_mut().push("release_name".into());
            Ok(())
        }
        fn owns(&self, _name: &str) -> Result<bool, String> {
            self.calls.borrow_mut().push("owns".into());
            self.owns.borrow_mut().pop_front().expect("scripted owns")
        }
    }

    /// Time that passes only when slept.
    struct Clock {
        start: Instant,
        elapsed: Cell<Duration>,
    }

    const NAME: &str = "org.freedesktop.ReserveDevice1.Audio1";
    const PATH: &str = "/org/freedesktop/ReserveDevice1/Audio1";

    /// Checks at 0, RELEASE_POLL, … RELEASE_WAIT.
    fn checks_in_the_wait() -> usize {
        (RELEASE_WAIT.as_millis() / RELEASE_POLL.as_millis()) as usize + 1
    }

    fn run(bus: &Scripted) -> (Outcome, Duration) {
        let clock = Clock { start: Instant::now(), elapsed: Cell::new(Duration::ZERO) };
        let outcome = negotiate(bus, NAME, PATH, &|| clock.start + clock.elapsed.get(), &|d| {
            clock.elapsed.set(clock.elapsed.get() + d)
        });
        (outcome, clock.elapsed.get())
    }

    fn requests(bus: &Scripted) -> Vec<String> {
        bus.calls().into_iter().filter(|c| c.starts_with("request_")).collect()
    }

    #[test]
    fn negotiate_free() {
        let bus = Scripted::new(vec![Ok(NameReply::PrimaryOwner)], vec![]);
        assert_eq!(run(&bus).0, Outcome::Free);
        assert_eq!(bus.calls(), ["request_name(false)"]);
    }

    #[test]
    fn negotiate_refused() {
        let bus = Scripted::new(vec![Ok(NameReply::InQueue)], vec![Ok(false)]);
        assert_eq!(run(&bus).0, Outcome::Refused);
        assert_eq!(bus.calls(), ["request_name(false)", "request_release(10)"]);
    }

    #[test]
    fn negotiate_released() {
        let bus = Scripted::new(vec![Ok(NameReply::InQueue)], vec![Ok(true)]).owns(vec![Ok(true)]);
        assert_eq!(run(&bus), (Outcome::Released, Duration::ZERO));
        assert_eq!(bus.calls(), ["request_name(false)", "request_release(10)", "owns"]);
    }

    #[test]
    fn negotiate_waits_for_the_owner_to_release() {
        let bus = Scripted::new(vec![Ok(NameReply::InQueue)], vec![Ok(true)]).owns(vec![Ok(false), Ok(false), Ok(true)]);
        assert_eq!(run(&bus), (Outcome::Released, 2 * RELEASE_POLL));
        assert_eq!(bus.calls(), ["request_name(false)", "request_release(10)", "owns", "owns", "owns"]);
    }

    #[test]
    fn negotiate_replaces_an_owner_that_keeps_the_name() {
        let bus = Scripted::new(vec![Ok(NameReply::InQueue), Ok(NameReply::PrimaryOwner)], vec![Ok(true)])
            .owns(vec![Ok(false); checks_in_the_wait()]);
        let (outcome, waited) = run(&bus);
        assert_eq!(outcome, Outcome::Released);
        assert!(waited >= RELEASE_WAIT && waited < RELEASE_WAIT + RELEASE_POLL, "{waited:?}");
        assert_eq!(requests(&bus), ["request_name(false)", "request_release(10)", "request_name(true)"]);
    }

    #[test]
    fn negotiate_race_then_success() {
        // The owner keeps the name and can't be replaced; on the second
        // attempt it hands it over.
        let mut owns = vec![Ok(false); checks_in_the_wait()];
        owns.push(Ok(true));
        let bus = Scripted::new(vec![Ok(NameReply::InQueue), Ok(NameReply::InQueue)], vec![Ok(true), Ok(true)]).owns(owns);
        assert_eq!(run(&bus).0, Outcome::Released);
        assert_eq!(
            requests(&bus),
            ["request_name(false)", "request_release(10)", "request_name(true)", "request_release(10)"]
        );
    }

    #[test]
    fn negotiate_race_twice_refused() {
        let bus = Scripted::new(
            vec![Ok(NameReply::InQueue), Ok(NameReply::InQueue), Ok(NameReply::InQueue)],
            vec![Ok(true), Ok(true), Ok(true)],
        )
        .owns(vec![Ok(false); 2 * checks_in_the_wait()]);
        assert_eq!(run(&bus).0, Outcome::Refused);
        assert_eq!(bus.calls().iter().filter(|c| c.starts_with("request_release")).count(), 2);
    }

    #[test]
    fn negotiate_bus_error() {
        let bus = Scripted::new(vec![Err("disconnected".into())], vec![]);
        assert_eq!(run(&bus).0, Outcome::Failed("disconnected".into()));

        let bus = Scripted::new(vec![Ok(NameReply::InQueue)], vec![Err("timeout".into())]);
        assert_eq!(run(&bus).0, Outcome::Failed("timeout".into()));

        let bus = Scripted::new(vec![Ok(NameReply::InQueue), Err("timeout".into())], vec![Ok(true)])
            .owns(vec![Ok(false); checks_in_the_wait()]);
        assert_eq!(run(&bus).0, Outcome::Failed("timeout".into()));
    }

    #[test]
    fn negotiate_bus_error_while_waiting() {
        let bus = Scripted::new(vec![Ok(NameReply::InQueue)], vec![Ok(true)]).owns(vec![Ok(false), Err("timeout".into())]);
        assert_eq!(run(&bus).0, Outcome::Failed("timeout".into()));
        assert_eq!(bus.calls(), ["request_name(false)", "request_release(10)", "owns", "owns"]);
    }

    #[test]
    fn release_request_is_refused() {
        let reserve = Reserve { device_name: "hw:1".into() };
        let msg = zbus::message::Message::method_call(PATH, "RequestRelease")
            .unwrap()
            .sender(":1.42")
            .unwrap()
            .build(&(100i32,))
            .unwrap();
        assert!(!reserve.request_release(100, msg.header()));
        assert!(!reserve.request_release(i32::MAX, msg.header()));
        assert_eq!(reserve.priority(), RESERVE_PRIORITY);
    }

    #[test]
    fn parse_owner_pid_reads_status() {
        let running = "state: RUNNING\nowner_pid   : 2345\ntrigger_time: 1234.5\ntstamp      : 0.0\n";
        assert_eq!(parse_owner_pid(running), Some(2345));
        assert_eq!(parse_owner_pid("closed\n"), None);
        assert_eq!(parse_owner_pid("state: SETUP\nowner_pid   : x\n"), None);
    }

    #[test]
    fn takeable_only_from_a_lower_priority_pipewire_hold() {
        assert!(takeable(Some(-20), true), "WirePlumber's hold");
        assert!(takeable(Some(0), true), "PulseAudio's priority");
        assert!(!takeable(Some(-20), false), "a raw ALSA client under WirePlumber's name");
        assert!(!takeable(None, true), "PipeWire without a reservation");
        assert!(!takeable(Some(RESERVE_PRIORITY), true));
        assert!(!takeable(Some(100), true), "an owner that outranks Zeke");
    }
}
