//! Preferences: audio (quality, output, playback) and appearance. Every
//! change goes to the player as a command and is saved at once.

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{glib, CompositeTemplate};
use zeke_engine::audio::AudioDevice;
use zeke_tidal::ColorScheme;

use crate::runtime;
use crate::session::{list_devices, Session};
use crate::window::ZekeWindow;

/// `max_quality` values with their labels, best first. `HI_RES` (MQA) is left
/// out: TIDAL serves FLAC for hi-res now.
const QUALITIES: [(&str, &str); 3] = [
    ("HI_RES_LOSSLESS", "Max — up to 24-bit/192 kHz FLAC"),
    ("LOSSLESS", "Lossless — 16-bit/44.1 kHz FLAC"),
    ("HIGH", "High — 320 kbps AAC"),
];

const SCHEMES: [(ColorScheme, &str); 3] =
    [(ColorScheme::System, "system"), (ColorScheme::Light, "light"), (ColorScheme::Dark, "dark")];

mod imp {
    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/io/github/hcuadrado/Zeke/ui/preferences.ui")]
    pub struct ZekePreferences {
        #[template_child]
        pub quality_row: TemplateChild<adw::ComboRow>,
        #[template_child]
        pub exclusive_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub device_row: TemplateChild<adw::ComboRow>,
        #[template_child]
        pub bit_perfect_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub gapless_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub normalization_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub color_scheme_row: TemplateChild<adw::ComboRow>,

        pub session: OnceCell<Rc<Session>>,
        /// The device row's entries (stable ALSA names).
        pub devices: RefCell<Vec<AudioDevice>>,
        /// Set while the device list is replaced: the row's selection
        /// changes then without the user choosing anything.
        pub replacing_devices: std::cell::Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ZekePreferences {
        const NAME: &'static str = "ZekePreferences";
        type Type = super::ZekePreferences;
        type ParentType = adw::PreferencesDialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ZekePreferences {}
    impl WidgetImpl for ZekePreferences {}
    impl AdwDialogImpl for ZekePreferences {}
    impl PreferencesDialogImpl for ZekePreferences {}
}

glib::wrapper! {
    pub struct ZekePreferences(ObjectSubclass<imp::ZekePreferences>)
        @extends adw::PreferencesDialog, adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::ShortcutManager;
}

impl ZekePreferences {
    pub fn present(window: &ZekeWindow) {
        let dialog: Self = glib::Object::new();
        dialog.imp().session.set(Rc::clone(window.session())).expect("set once");
        dialog.load(window);
        adw::prelude::AdwDialogExt::present(&dialog, Some(window));
    }

    fn session(&self) -> &Rc<Session> {
        self.imp().session.get().expect("set in present()")
    }

    /// Show the saved settings, then connect the rows.
    fn load(&self, window: &ZekeWindow) {
        let imp = self.imp();
        let s = self.session().settings();

        let qualities = gtk::StringList::new(&QUALITIES.map(|(_, label)| label));
        imp.quality_row.set_model(Some(&qualities));
        let at = QUALITIES.iter().position(|(q, _)| *q == s.max_quality).unwrap_or(0);
        imp.quality_row.set_selected(at as u32);
        imp.exclusive_row.set_active(s.output_device.is_some());
        imp.bit_perfect_row.set_active(s.output_device.as_deref().is_some_and(|d| s.bit_perfect_on(d)));
        imp.gapless_row.set_active(s.gapless);
        imp.normalization_row.set_active(s.volume_normalization);
        let at = SCHEMES.iter().position(|(c, _)| *c == s.color_scheme).unwrap_or(0);
        imp.color_scheme_row.set_selected(at as u32);
        // Until the device list arrives, show the saved one alone.
        let saved = s.output_device.clone().map(|id| AudioDevice { name: id.clone(), id });
        self.set_devices(saved.into_iter().collect(), s.output_device.as_deref());
        self.load_devices();

        imp.quality_row.connect_selected_notify(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |row| {
                if let Some((q, _)) = QUALITIES.get(row.selected() as usize) {
                    this.session().set_max_quality(q);
                }
            }
        ));
        for row in [&*imp.exclusive_row, &*imp.bit_perfect_row] {
            row.connect_active_notify(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_| this.output_changed()
            ));
        }
        imp.device_row.connect_selected_notify(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.output_changed()
        ));
        imp.gapless_row.connect_active_notify(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |row| this.session().set_gapless(row.is_active())
        ));
        imp.normalization_row.connect_active_notify(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |row| this.session().set_normalization(row.is_active())
        ));
        imp.color_scheme_row.connect_selected_notify(glib::clone!(
            #[weak]
            window,
            move |row| {
                if let Some((_, name)) = SCHEMES.get(row.selected() as usize) {
                    // The same action as the main menu's Style items.
                    if let Some(app) = window.application() {
                        app.activate_action("color-scheme", Some(&name.to_variant()));
                    }
                }
            }
        ));
    }

    /// List the ALSA devices off the main thread (the monitor can take ~2 s).
    fn load_devices(&self) {
        let this = self.downgrade();
        runtime::spawn(async { tokio::task::spawn_blocking(list_devices).await }, move |result| {
            let Some(this) = this.upgrade() else { return };
            match result {
                Ok(Ok(mut list)) => {
                    let current = this.selected_device();
                    // Keep a saved device that isn't listed (unplugged, or
                    // one PipeWire doesn't show, like hw:0,31).
                    if let Some(id) = current.as_ref().filter(|id| !list.iter().any(|d| &d.id == *id)) {
                        list.push(AudioDevice { id: id.clone(), name: "Not listed".into() });
                    }
                    this.set_devices(list, current.as_deref());
                }
                Ok(Err(e)) => log::warn!("[app] listing devices: {e}"),
                Err(e) => log::warn!("[app] listing devices: {e}"),
            }
        });
    }

    /// Replace the device row's entries without it reporting a change.
    fn set_devices(&self, devices: Vec<AudioDevice>, selected: Option<&str>) {
        let imp = self.imp();
        let labels = device_labels(&devices);
        let at = selected.and_then(|id| devices.iter().position(|d| d.id == id));
        imp.devices.replace(devices);
        let model = gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>());
        imp.replacing_devices.set(true);
        imp.device_row.set_model(Some(&model));
        imp.device_row.set_selected(at.map_or(gtk::INVALID_LIST_POSITION, |i| i as u32));
        imp.replacing_devices.set(false);
    }

    fn selected_device(&self) -> Option<String> {
        let imp = self.imp();
        imp.devices.borrow().get(imp.device_row.selected() as usize).map(|d| d.id.clone())
    }

    fn output_changed(&self) {
        let imp = self.imp();
        if imp.replacing_devices.get() {
            return;
        }
        let exclusive = imp.exclusive_row.is_active();
        let bit_perfect = imp.bit_perfect_row.is_active();
        let device = self.selected_device().or_else(|| self.session().settings().output_device);
        let s = self.session().settings();
        let saved_bit_perfect = s.output_device.as_deref().is_some_and(|d| s.bit_perfect_on(d));
        if (s.output_device.is_some(), saved_bit_perfect, s.output_device.as_deref()) == (exclusive, bit_perfect, device.as_deref()) {
            return;
        }
        self.session().set_output(exclusive, device, bit_perfect);
    }
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
