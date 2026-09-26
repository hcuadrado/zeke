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
use std::time::Duration;

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
    Exists,
    Other,
}

/// The bus calls of the protocol, behind a trait so [`negotiate`] is tested
/// without a bus.
trait Bus {
    /// `RequestName` with `DoNotQueue`, plus `ReplaceExisting` when
    /// `replace`. Never `AllowReplacement`: the name is only given up by
    /// dropping the hold.
    fn request_name(&self, name: &str, replace: bool) -> Result<NameReply, String>;
    /// `RequestRelease(priority)` on the name's owner.
    fn request_release(&self, name: &str, path: &str, priority: i32) -> Result<bool, String>;
    fn release_name(&self, name: &str) -> Result<(), String>;
    /// The owner's `ApplicationName`, for the log.
    fn owner_app(&self, _name: &str, _path: &str) -> Option<String> {
        None
    }
}

/// How asking for a name ended.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Free,
    Released,
    Refused,
    Failed(String),
}

/// Take `name`: at once if no one owns it, else by asking the owner to
/// release it and replacing it. The owner may win the name back between its
/// release and our request, so that is tried [`MAX_TAKEOVER_ATTEMPTS`] times.
fn negotiate(bus: &dyn Bus, name: &str, path: &str) -> Outcome {
    match bus.request_name(name, false) {
        Ok(NameReply::PrimaryOwner) => return Outcome::Free,
        Ok(NameReply::Exists) => {}
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
        match bus.request_name(name, true) {
            Ok(NameReply::PrimaryOwner) => {
                log::info!("[reserve] reserved {name} from {owner} on takeover attempt {attempt}");
                return Outcome::Released;
            }
            Ok(_) => log::info!("[reserve] takeover attempt {attempt} lost to {owner}; retrying"),
            Err(e) => return Outcome::Failed(e),
        }
    }
    log::warn!("[reserve] gave up on {name} after {MAX_TAKEOVER_ATTEMPTS} takeover attempts");
    Outcome::Refused
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

    /// `ReleaseName` on the bus itself, bypassing zbus's list of names.
    fn release_directly(&self, name: &str) -> Result<(), String> {
        self.conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "ReleaseName",
                &(name,),
            )
            .map(drop)
            .map_err(|e| self.err(e))
    }
}

impl Bus for ZbusBus<'_> {
    fn request_name(&self, name: &str, replace: bool) -> Result<NameReply, String> {
        let mut flags = RequestNameFlags::DoNotQueue.into();
        if replace {
            flags |= RequestNameFlags::ReplaceExisting;
        }
        match self.conn.request_name_with_flags(name, flags) {
            // AlreadyOwner: the name is ours, e.g. after a timed-out request
            // the bus granted anyway.
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => Ok(NameReply::PrimaryOwner),
            Ok(_) => Ok(NameReply::Other),
            // zbus reports a `DoNotQueue` "Exists" reply as NameTaken.
            Err(zbus::Error::NameTaken) => Ok(NameReply::Exists),
            Err(e) => Err(self.err(e)),
        }
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
        let outcome = negotiate(&bus, &name, &path);
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
        calls: RefCell<Vec<String>>,
    }

    impl Scripted {
        fn new(names: Vec<Result<NameReply, String>>, releases: Vec<Result<bool, String>>) -> Self {
            Scripted { names: RefCell::new(names.into()), releases: RefCell::new(releases.into()), ..Default::default() }
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
    }

    const NAME: &str = "org.freedesktop.ReserveDevice1.Audio1";
    const PATH: &str = "/org/freedesktop/ReserveDevice1/Audio1";

    #[test]
    fn negotiate_free() {
        let bus = Scripted::new(vec![Ok(NameReply::PrimaryOwner)], vec![]);
        assert_eq!(negotiate(&bus, NAME, PATH), Outcome::Free);
        assert_eq!(bus.calls(), ["request_name(false)"]);
    }

    #[test]
    fn negotiate_refused() {
        let bus = Scripted::new(vec![Ok(NameReply::Exists)], vec![Ok(false)]);
        assert_eq!(negotiate(&bus, NAME, PATH), Outcome::Refused);
        assert_eq!(bus.calls(), ["request_name(false)", "request_release(10)"]);
    }

    #[test]
    fn negotiate_released() {
        let bus = Scripted::new(vec![Ok(NameReply::Exists), Ok(NameReply::PrimaryOwner)], vec![Ok(true)]);
        assert_eq!(negotiate(&bus, NAME, PATH), Outcome::Released);
        assert_eq!(bus.calls(), ["request_name(false)", "request_release(10)", "request_name(true)"]);
    }

    #[test]
    fn negotiate_race_then_success() {
        let bus = Scripted::new(
            vec![Ok(NameReply::Exists), Ok(NameReply::Exists), Ok(NameReply::PrimaryOwner)],
            vec![Ok(true), Ok(true)],
        );
        assert_eq!(negotiate(&bus, NAME, PATH), Outcome::Released);
        assert_eq!(
            bus.calls(),
            ["request_name(false)", "request_release(10)", "request_name(true)", "request_release(10)", "request_name(true)"]
        );
    }

    #[test]
    fn negotiate_race_twice_refused() {
        let bus = Scripted::new(
            vec![Ok(NameReply::Exists), Ok(NameReply::Exists), Ok(NameReply::Exists)],
            vec![Ok(true), Ok(true), Ok(true)],
        );
        assert_eq!(negotiate(&bus, NAME, PATH), Outcome::Refused);
        assert_eq!(bus.calls().iter().filter(|c| c.starts_with("request_release")).count(), 2);
    }

    #[test]
    fn negotiate_bus_error() {
        let bus = Scripted::new(vec![Err("disconnected".into())], vec![]);
        assert_eq!(negotiate(&bus, NAME, PATH), Outcome::Failed("disconnected".into()));

        let bus = Scripted::new(vec![Ok(NameReply::Exists)], vec![Err("timeout".into())]);
        assert_eq!(negotiate(&bus, NAME, PATH), Outcome::Failed("timeout".into()));

        let bus = Scripted::new(vec![Ok(NameReply::Exists), Err("timeout".into())], vec![Ok(true)]);
        assert_eq!(negotiate(&bus, NAME, PATH), Outcome::Failed("timeout".into()));
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
