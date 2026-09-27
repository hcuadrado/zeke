//! Preferences: audio (quality, playback) and appearance. Every change goes
//! to the player as a command and is saved at once. The output is chosen in
//! the player bar (`output_picker`).

use std::cell::OnceCell;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{glib, CompositeTemplate};
use zeke_tidal::ColorScheme;

use crate::session::Session;
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
        pub gapless_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub normalization_row: TemplateChild<adw::SwitchRow>,
        #[template_child]
        pub color_scheme_row: TemplateChild<adw::ComboRow>,

        pub session: OnceCell<Rc<Session>>,
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
        imp.gapless_row.set_active(s.gapless);
        imp.normalization_row.set_active(s.volume_normalization);
        let at = SCHEMES.iter().position(|(c, _)| *c == s.color_scheme).unwrap_or(0);
        imp.color_scheme_row.set_selected(at as u32);

        imp.quality_row.connect_selected_notify(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |row| {
                if let Some((q, _)) = QUALITIES.get(row.selected() as usize) {
                    this.session().set_max_quality(q);
                }
            }
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
        // A page for the plugins, when this build has any.
        let groups = window.plugins().preference_groups();
        if !groups.is_empty() {
            let page = adw::PreferencesPage::builder().title("Plugins").icon_name("application-x-addon-symbolic").build();
            for group in &groups {
                page.add(group);
            }
            self.add(&page);
        }
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
}
