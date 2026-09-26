//! The plugin host: the compiled-in plugins, their switches and settings
//! entries, their Preferences slot and output-menu section, and when they
//! run. Everything reaches a plugin through `zeke_plugin::DynPlugin`; the
//! only place that names one is `registry`.

mod runner;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use zeke_plugin::{DynPlugin, Need, PluginUi, RemoteTargetFactory, Services};
use zeke_tidal::Settings;

use crate::runtime::runtime;
use crate::session::Session;
use runner::{Account, Action, Event, Runner};

/// Every plugin this build has.
fn registry() -> Vec<Arc<dyn DynPlugin>> {
    Vec::new()
}

/// How long quitting waits for the plugins to stop.
const STOP_AT_EXIT: Duration = Duration::from_secs(2);

/// Whether the plugin's switch is on in `settings`. Off unless saved on.
pub fn is_enabled(settings: &Settings, id: &str) -> bool {
    settings.plugins.get(id).and_then(|entry| entry.get("enabled")).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Save the switch in the plugin's entry, keeping the rest of the entry.
pub fn set_enabled(settings: &mut Settings, id: &str, on: bool) {
    let entry = settings.plugins.entry(id.to_owned()).or_insert_with(|| serde_json::json!({}));
    if !entry.is_object() {
        *entry = serde_json::json!({});
    }
    entry["enabled"] = on.into();
}

/// `make()` if `needs` lists `need`: a plugin gets only what it asks for.
fn grant<T>(needs: &[Need], need: Need, make: impl FnOnce() -> T) -> Option<T> {
    needs.contains(&need).then(make)
}

struct Slot {
    plugin: Arc<dyn DynPlugin>,
    /// Taken at exit.
    runner: RefCell<Option<Runner>>,
    enabled: Cell<bool>,
    /// The account the current instance was started for; `None` while it
    /// is stopped or stopping.
    running_for: Cell<Option<Account>>,
    /// Of the last start command: a report from an older start is stale.
    epoch: Cell<u64>,
    /// The running instance's widgets.
    ui: RefCell<Option<PluginUi>>,
    remote_target: RefCell<Option<Arc<dyn RemoteTargetFactory>>>,
    /// Where the open Preferences dialog shows the plugin's page.
    page_bin: glib::WeakRef<adw::Bin>,
}

pub struct Host {
    session: Rc<Session>,
    slots: Vec<Slot>,
    /// `None` while signed out.
    signed_in: Cell<Option<Account>>,
    /// Holds the output-menu sections; hidden while there are none.
    output_sections: gtk::Box,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host").finish_non_exhaustive()
    }
}

impl Host {
    /// On the main thread, once. Nothing starts until someone signs in.
    pub fn new(session: Rc<Session>) -> Rc<Self> {
        let settings = session.settings();
        let (tx, rx) = async_channel::unbounded::<(usize, Event)>();
        let slots = registry()
            .into_iter()
            .enumerate()
            .map(|(index, plugin)| {
                let tx = tx.clone();
                let events = move |e| {
                    let _ = tx.try_send((index, e));
                };
                Slot {
                    runner: RefCell::new(Some(Runner::spawn(runtime().handle(), Arc::clone(&plugin), events))),
                    enabled: Cell::new(is_enabled(&settings, plugin.id())),
                    plugin,
                    running_for: Cell::new(None),
                    epoch: Cell::new(0),
                    ui: RefCell::new(None),
                    remote_target: RefCell::new(None),
                    page_bin: glib::WeakRef::new(),
                }
            })
            .collect();
        let output_sections =
            gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).visible(false).build();
        let host = Rc::new(Self { session, slots, signed_in: Cell::new(None), output_sections });
        let weak = Rc::downgrade(&host);
        glib::spawn_future_local(async move {
            while let Ok((index, event)) = rx.recv().await {
                let Some(host) = weak.upgrade() else { break };
                host.handle(index, event);
            }
        });
        host
    }

    /// For the output menu, under the local outputs.
    pub fn output_sections(&self) -> &gtk::Box {
        &self.output_sections
    }

    /// The remote target factories of the running plugins.
    #[allow(dead_code, reason = "nothing picks a remote target yet")]
    pub fn remote_targets(&self) -> BTreeMap<&'static str, Arc<dyn RemoteTargetFactory>> {
        self.slots
            .iter()
            .filter_map(|s| s.remote_target.borrow().clone().map(|t| (s.plugin.id(), t)))
            .collect()
    }

    /// A session was restored or someone signed in: start what is on, and
    /// restart what ran for another account.
    pub fn signed_in(&self, account: Account) {
        self.signed_in.set(Some(account));
        self.reconcile_all();
    }

    pub fn signed_out(&self) {
        self.signed_in.set(None);
        self.reconcile_all();
    }

    /// Stop every plugin and wait for them (briefly). For app exit.
    pub fn shutdown(&self) {
        let done = self
            .slots
            .iter()
            .filter_map(|slot| {
                self.unmount(slot);
                slot.running_for.set(None);
                slot.runner.take().map(Runner::close)
            })
            .collect();
        if !runner::wait_all(done, STOP_AT_EXIT) {
            log::warn!("[plugins] a plugin did not stop in time");
        }
    }

    /// One group per plugin for the Preferences dialog: its switch, and its
    /// page below it while it runs.
    pub fn preference_groups(self: &Rc<Self>) -> Vec<adw::PreferencesGroup> {
        (0..self.slots.len()).map(|index| self.preference_group(index)).collect()
    }

    fn preference_group(self: &Rc<Self>, index: usize) -> adw::PreferencesGroup {
        let slot = &self.slots[index];
        let group = adw::PreferencesGroup::new();
        let switch = adw::SwitchRow::builder().title(slot.plugin.name()).active(slot.enabled.get()).build();
        switch.connect_active_notify(glib::clone!(
            #[weak(rename_to = host)]
            self,
            move |row| host.set_enabled(index, row.is_active())
        ));
        group.add(&switch);
        let bin = adw::Bin::new();
        group.add(&bin);
        slot.page_bin.set(Some(&bin));
        if let Some(page) = slot.ui.borrow().as_ref().and_then(|ui| ui.page.as_ref()) {
            show_in(&bin, page);
        }
        group
    }

    fn set_enabled(&self, index: usize, on: bool) {
        let slot = &self.slots[index];
        if slot.enabled.replace(on) == on {
            return;
        }
        self.session.set_plugin_enabled(slot.plugin.id(), on);
        self.reconcile(index);
    }

    fn reconcile_all(&self) {
        (0..self.slots.len()).for_each(|index| self.reconcile(index));
    }

    fn reconcile(&self, index: usize) {
        let slot = &self.slots[index];
        match runner::reconcile(slot.enabled.get(), self.signed_in.get(), slot.running_for.get()) {
            Action::Nothing => {}
            Action::Start => self.start(slot),
            Action::Stop => self.stop(slot),
            Action::Restart => {
                self.stop(slot);
                self.start(slot);
            }
        }
    }

    fn start(&self, slot: &Slot) {
        let Some(runner) = &*slot.runner.borrow() else { return };
        let epoch = slot.epoch.get() + 1;
        slot.epoch.set(epoch);
        slot.running_for.set(self.signed_in.get());
        let services = Services {
            credential: grant(slot.plugin.needs(), Need::Credential, || self.session.state.credential_source()),
        };
        runner.start(epoch, services);
    }

    /// The widgets and the target go at once; the instance stops on tokio.
    fn stop(&self, slot: &Slot) {
        self.unmount(slot);
        slot.running_for.set(None);
        if let Some(runner) = &*slot.runner.borrow() {
            runner.stop();
        }
    }

    fn handle(&self, index: usize, event: Event) {
        let slot = &self.slots[index];
        match event {
            // Only the instance the last start asked for, if still wanted.
            Event::Started { epoch, handle } if epoch == slot.epoch.get() && slot.running_for.get().is_some() => {
                let ui = slot.plugin.ui(&handle);
                if let (Some(page), Some(bin)) = (&ui.page, slot.page_bin.upgrade()) {
                    show_in(&bin, page);
                }
                if let Some(section) = &ui.output_section {
                    self.output_sections.append(section);
                    self.output_sections.set_visible(true);
                }
                slot.remote_target.replace(slot.plugin.remote_target(&handle));
                slot.ui.replace(Some(ui));
            }
            Event::Started { .. } => {}
            Event::Stopped { epoch } => log::debug!("[plugins] {} instance {epoch} is gone", slot.plugin.id()),
        }
    }

    fn unmount(&self, slot: &Slot) {
        slot.remote_target.take();
        let Some(ui) = slot.ui.take() else { return };
        if let Some(bin) = ui.page.and_then(|page| page.parent()).and_downcast::<adw::Bin>() {
            bin.set_child(gtk::Widget::NONE);
        }
        if let Some(section) = ui.output_section {
            self.output_sections.remove(&section);
            self.output_sections.set_visible(self.output_sections.first_child().is_some());
        }
    }
}

/// Put `page` in `bin`, taking it from a dialog that showed it before.
fn show_in(bin: &adw::Bin, page: &gtk::Widget) {
    if let Some(old) = page.parent().and_downcast::<adw::Bin>() {
        old.set_child(gtk::Widget::NONE);
    }
    bin.set_child(Some(page));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_is_off_until_switched_on() {
        let mut s = Settings::default();
        assert!(!is_enabled(&s, "cast"));
        set_enabled(&mut s, "cast", true);
        assert!(is_enabled(&s, "cast"));
        assert!(!is_enabled(&s, "radio"));
        set_enabled(&mut s, "cast", false);
        assert!(!is_enabled(&s, "cast"));
    }

    #[test]
    fn the_switch_keeps_the_rest_of_the_entry_and_the_other_plugins() {
        let mut s = Settings::default();
        s.plugins.insert("cast".into(), serde_json::json!({ "enabled": true, "last": "Kitchen" }));
        // A plugin this build doesn't have: its entry is left alone.
        s.plugins.insert("radio".into(), serde_json::json!({ "enabled": true, "model": "m" }));
        set_enabled(&mut s, "cast", false);
        assert_eq!(s.plugins["cast"], serde_json::json!({ "enabled": false, "last": "Kitchen" }));
        assert_eq!(s.plugins["radio"], serde_json::json!({ "enabled": true, "model": "m" }));
        // And it all goes through the file as it came.
        let back: Settings = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back.plugins, s.plugins);
    }

    #[test]
    fn the_credential_goes_only_to_a_plugin_that_needs_it() {
        assert_eq!(grant(&[Need::Credential], Need::Credential, || "token"), Some("token"));
        assert_eq!(grant(&[], Need::Credential, || -> &str { panic!("not asked for") }), None);
    }
}
