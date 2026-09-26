//! The player bar's output menu: System Default, then every ALSA playback
//! device, each with its own bit-perfect switch. A pick is saved at once and
//! plays from the next track, so the check mark stays on the output the
//! playing track opened until then.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use zeke_engine::audio::AudioDevice;
use zeke_engine::devices;

use crate::runtime;
use crate::session::list_devices;
use crate::window::ZekeWindow;

const SYSTEM_DEFAULT: &str = "System Default";

/// One row of the menu.
struct OutputRow {
    /// `None` is the system default.
    device: Option<String>,
    label: String,
    playing: bool,
    /// The saved pick, which starts with the next track.
    next: bool,
    bit_perfect: bool,
}

impl ZekeWindow {
    pub fn setup_output_picker(&self) {
        // The last list at once; a fresh one replaces it when it arrives.
        self.imp().output_popover.connect_show(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| {
                window.render_outputs();
                window.load_outputs();
            }
        ));
        // Plugins' sections go under the local outputs.
        self.imp().output_box.append(self.plugins().output_sections());
        self.refresh_outputs();
    }

    /// The output shown as playing, and the saved pick when it differs from
    /// it. Before anything has played, the saved pick is the one shown.
    fn outputs_now(&self) -> (Option<String>, Option<Option<String>>) {
        let saved = self.session().output();
        match self.imp().active_output.borrow().clone() {
            Some(active) if active != saved => (active, Some(saved)),
            Some(active) => (active, None),
            None => (saved, None),
        }
    }

    /// `device`'s label in the last list, or its id if it isn't listed.
    pub fn output_label(&self, device: Option<&str>) -> String {
        let Some(id) = device else { return SYSTEM_DEFAULT.into() };
        let devices = self.imp().output_devices.borrow();
        match devices.iter().position(|d| d.id == id) {
            Some(at) => device_labels(&devices).swap_remove(at),
            None => id.to_string(),
        }
    }

    /// The button's tooltip and, while the menu is open, its rows.
    pub fn refresh_outputs(&self) {
        let imp = self.imp();
        let (playing, next) = self.outputs_now();
        let mut tooltip = format!("Output: {}", self.output_label(playing.as_deref()));
        if let Some(next) = next {
            tooltip.push_str(&format!("\nNext track: {}", self.output_label(next.as_deref())));
        }
        imp.output_button.set_tooltip_text(Some(&tooltip));
        if imp.output_popover.is_visible() {
            self.render_outputs();
        }
    }

    /// List the devices off the main thread (the monitor can take ~2 s).
    fn load_outputs(&self) {
        if self.imp().listing_outputs.replace(true) {
            return;
        }
        let window = self.downgrade();
        runtime::spawn(async { tokio::task::spawn_blocking(list_devices).await }, move |result| {
            let Some(window) = window.upgrade() else { return };
            window.imp().listing_outputs.set(false);
            match result {
                Ok(Ok(list)) => window.outputs_listed(list),
                Ok(Err(e)) => log::warn!("[app] listing devices: {e}"),
                Err(e) => log::warn!("[app] listing devices: {e}"),
            }
        });
    }

    /// Take a new device list. The devices of the card Zeke plays on
    /// exclusively are kept from the last list: PipeWire stops listing
    /// them while Zeke holds the card.
    pub fn outputs_listed(&self, list: Vec<AudioDevice>) {
        let imp = self.imp();
        let held = match &*imp.active_output.borrow() {
            Some(Some(device)) => devices::card_index(device),
            _ => None,
        };
        let merged = merge_outputs(&imp.output_devices.borrow(), list, held, devices::card_index);
        imp.output_devices.replace(merged);
        self.refresh_outputs();
    }

    fn render_outputs(&self) {
        let imp = self.imp();
        let settings = self.session().settings();
        let (playing, next) = self.outputs_now();
        let mut entries: Vec<(Option<String>, String)> = vec![(None, SYSTEM_DEFAULT.into())];
        {
            let devices = imp.output_devices.borrow();
            entries.extend(devices.iter().map(|d| Some(d.id.clone())).zip(device_labels(&devices)));
        }
        // The playing and the saved device stay when they aren't listed
        // (unplugged, or one the monitor doesn't show, like hw:0,31).
        for id in [playing.clone(), settings.output_device.clone()].into_iter().flatten() {
            if !entries.iter().any(|(d, _)| d.as_deref() == Some(id.as_str())) {
                let label = device_labels(&[AudioDevice { name: "Not listed".into(), id: id.clone() }]).remove(0);
                entries.push((Some(id), label));
            }
        }
        imp.output_list.remove_all();
        for (device, label) in entries {
            let row = OutputRow {
                playing: device == playing,
                next: next.as_ref() == Some(&device),
                bit_perfect: device.as_deref().is_some_and(|d| settings.bit_perfect_on(d)),
                device,
                label,
            };
            imp.output_list.append(&self.output_row(row));
        }
    }

    fn output_row(&self, row: OutputRow) -> adw::ActionRow {
        let widget = adw::ActionRow::builder().title(&row.label).use_markup(false).activatable(true).build();
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        // Hidden by opacity, so the labels line up.
        check.set_opacity(if row.playing { 1.0 } else { 0.0 });
        widget.add_prefix(&check);
        if row.next {
            widget.set_subtitle("From the next track");
        }
        if let Some(id) = row.device.clone() {
            let caption = gtk::Label::builder().label("Bit-perfect").css_classes(["dim-label", "caption"]).build();
            let switch = gtk::Switch::builder()
                .active(row.bit_perfect)
                .valign(gtk::Align::Center)
                .tooltip_text("No resampling or conversion. Tracks at a rate the device lacks don’t play.")
                .build();
            switch.update_property(&[gtk::accessible::Property::Label("Bit-perfect")]);
            // Only the flag: the switch never picks its row.
            switch.connect_active_notify(glib::clone!(
                #[weak(rename_to = window)]
                self,
                move |switch| window.session().set_device_bit_perfect(&id, switch.is_active())
            ));
            widget.add_suffix(&caption);
            widget.add_suffix(&switch);
        }
        let device = row.device;
        widget.connect_activated(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.pick_output(device.clone())
        ));
        widget
    }

    fn pick_output(&self, device: Option<String>) {
        self.imp().output_popover.popdown();
        self.session().select_output(device);
        self.refresh_outputs();
    }
}

/// `fresh`, keeping the entries of `previous` on the `held` card that it
/// lacks, in their old place. Any other device missing from `fresh` is gone.
/// `card_of` is the card index of a device name.
fn merge_outputs(
    previous: &[AudioDevice],
    fresh: Vec<AudioDevice>,
    held: Option<u32>,
    card_of: impl Fn(&str) -> Option<u32>,
) -> Vec<AudioDevice> {
    let mut out: Vec<AudioDevice> = previous
        .iter()
        .filter_map(|old| match fresh.iter().find(|d| d.id == old.id) {
            Some(new) => Some(new.clone()),
            None => (held.is_some() && card_of(&old.id) == held).then(|| old.clone()),
        })
        .collect();
    for new in fresh {
        if !out.iter().any(|d| d.id == new.id) {
            out.push(new);
        }
    }
    out
}

/// Labels that tell devices apart: the words all names share (the card,
/// e.g. "Meteor Lake-P HD Audio Controller") are dropped, and the card and
/// device number follow: "Speaker (sofhdadsp, device 0)".
fn device_labels(devices: &[AudioDevice]) -> Vec<String> {
    let words: Vec<Vec<&str>> = devices.iter().map(|d| d.name.split_whitespace().collect()).collect();
    let common = match words.as_slice() {
        [first, rest @ ..] if !rest.is_empty() => {
            (0..first.len()).take_while(|&i| rest.iter().all(|w| w.get(i) == first.get(i))).count()
        }
        _ => 0,
    };
    devices
        .iter()
        .zip(&words)
        .map(|(d, w)| {
            // Keep the whole name if nothing would be left of it.
            let name = if w.len() > common { w[common..].join(" ") } else { d.name.clone() };
            match zeke_engine::devices::parse_hw(&d.id) {
                Some((zeke_engine::devices::CardRef::Id(card), dev)) => format!("{name} ({card}, device {dev})"),
                Some((zeke_engine::devices::CardRef::Index(card), dev)) => format!("{name} (card {card}, device {dev})"),
                None => format!("{name} ({})", d.id),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str) -> AudioDevice {
        AudioDevice { id: id.into(), name: name.into() }
    }

    fn ids(list: &[AudioDevice]) -> Vec<&str> {
        list.iter().map(|d| d.id.as_str()).collect()
    }

    /// Card 0 is `sofhdadsp`, card 1 `DAC`.
    fn card_of(id: &str) -> Option<u32> {
        if id.contains("sofhdadsp") {
            Some(0)
        } else if id.contains("DAC") {
            Some(1)
        } else {
            None
        }
    }

    fn before() -> Vec<AudioDevice> {
        vec![
            dev("hw:CARD=sofhdadsp,DEV=0", "HDA Speaker"),
            dev("hw:CARD=sofhdadsp,DEV=3", "HDA HDMI 1"),
            dev("hw:CARD=DAC,DEV=0", "USB DAC"),
        ]
    }

    #[test]
    fn a_new_list_keeps_the_held_cards_devices() {
        // Zeke holds card 0: PipeWire lists only the DAC.
        let merged = merge_outputs(&before(), vec![dev("hw:CARD=DAC,DEV=0", "USB DAC")], Some(0), card_of);
        assert_eq!(ids(&merged), ["hw:CARD=sofhdadsp,DEV=0", "hw:CARD=sofhdadsp,DEV=3", "hw:CARD=DAC,DEV=0"]);
        assert_eq!(device_labels(&merged), device_labels(&before()), "labels unchanged");
    }

    #[test]
    fn a_new_list_drops_a_missing_device_on_another_card() {
        let fresh = vec![dev("hw:CARD=sofhdadsp,DEV=3", "HDA HDMI 1")];
        // Card 0 held: its speaker stays, the unplugged DAC goes.
        let merged = merge_outputs(&before(), fresh.clone(), Some(0), card_of);
        assert_eq!(ids(&merged), ["hw:CARD=sofhdadsp,DEV=0", "hw:CARD=sofhdadsp,DEV=3"]);
        // Nothing held: only what is listed.
        assert_eq!(ids(&merge_outputs(&before(), fresh, None, card_of)), ["hw:CARD=sofhdadsp,DEV=3"]);
    }

    #[test]
    fn a_new_list_takes_the_fresh_entry_and_adds_new_devices() {
        let fresh = vec![
            dev("hw:CARD=sofhdadsp,DEV=0", "HDA Headphones"),
            dev("hw:CARD=DAC,DEV=0", "USB DAC"),
            dev("hw:CARD=DAC,DEV=1", "USB DAC Digital"),
        ];
        let merged = merge_outputs(&before(), fresh, Some(0), card_of);
        assert_eq!(merged[0].name, "HDA Headphones");
        assert_eq!(
            ids(&merged),
            ["hw:CARD=sofhdadsp,DEV=0", "hw:CARD=sofhdadsp,DEV=3", "hw:CARD=DAC,DEV=0", "hw:CARD=DAC,DEV=1"]
        );
    }

    #[test]
    fn labels_drop_the_shared_card_name() {
        let list = [
            dev("hw:CARD=sofhdadsp,DEV=0", "Meteor Lake-P HD Audio Controller Speaker"),
            dev("hw:CARD=sofhdadsp,DEV=3", "Meteor Lake-P HD Audio Controller HDMI / DisplayPort 1 Output"),
        ];
        assert_eq!(
            device_labels(&list),
            ["Speaker (sofhdadsp, device 0)", "HDMI / DisplayPort 1 Output (sofhdadsp, device 3)"]
        );
        // One device, or a name that is all prefix: keep the whole name.
        assert_eq!(device_labels(&list[..1]), ["Meteor Lake-P HD Audio Controller Speaker (sofhdadsp, device 0)"]);
        let odd = [dev("hw:1,0", "USB DAC"), dev("hw:1,1", "USB DAC Digital"), dev("default", "Other")];
        assert_eq!(device_labels(&odd), ["USB DAC (card 1, device 0)", "USB DAC Digital (card 1, device 1)", "Other (default)"]);
    }
}
