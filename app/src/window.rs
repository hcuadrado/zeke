//! The main window: login page, player bar and now-playing sheet. It only
//! shows player state and sends `PlayerCommand`s through the `Session`.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib, CompositeTemplate};
use zeke_engine::SignalPath;
use zeke_player::{PlayerCommand, RepeatMode, StreamFormat};
use zeke_tidal::commands::auth::PkceAuthParams;

use crate::session::{Session, TrackMeta, UiEvent};

/// The track the player reports as current.
#[derive(Debug, Clone)]
pub struct Now {
    pub track_id: u64,
    pub duration: Option<f64>,
    pub format: Option<StreamFormat>,
}

mod imp {
    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/io/github/hcuadrado/Zeke/ui/window.ui")]
    pub struct ZekeWindow {
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub split_view: TemplateChild<adw::NavigationSplitView>,
        #[template_child]
        pub sidebar_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub nav_view: TemplateChild<adw::NavigationView>,
        #[template_child]
        pub sign_in_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub sign_in_hint: TemplateChild<gtk::Label>,
        #[template_child]
        pub use_browser_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub browser_steps: TemplateChild<gtk::Revealer>,
        #[template_child]
        pub open_login_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub login_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub login_progress: TemplateChild<gtk::Box>,
        #[template_child]
        pub login_status: TemplateChild<gtk::Label>,
        #[template_child]
        pub sheet: TemplateChild<adw::BottomSheet>,
        #[template_child]
        pub sheet_cover: TemplateChild<gtk::Picture>,
        #[template_child]
        pub sheet_title: TemplateChild<gtk::Label>,
        #[template_child]
        pub sheet_artist: TemplateChild<gtk::Label>,
        #[template_child]
        pub sheet_album: TemplateChild<gtk::Label>,
        #[template_child]
        pub sheet_badge: TemplateChild<gtk::Label>,
        #[template_child]
        pub sheet_elapsed: TemplateChild<gtk::Label>,
        #[template_child]
        pub sheet_seek: TemplateChild<gtk::Scale>,
        #[template_child]
        pub sheet_total: TemplateChild<gtk::Label>,
        #[template_child]
        pub sheet_play: TemplateChild<gtk::Button>,
        #[template_child]
        pub sheet_repeat: TemplateChild<gtk::Button>,
        #[template_child]
        pub queue_view: TemplateChild<gtk::ListView>,
        #[template_child]
        pub bar_cover: TemplateChild<gtk::Picture>,
        #[template_child]
        pub bar_title: TemplateChild<gtk::Label>,
        #[template_child]
        pub bar_artist: TemplateChild<gtk::Label>,
        #[template_child]
        pub bar_elapsed: TemplateChild<gtk::Label>,
        #[template_child]
        pub bar_seek: TemplateChild<gtk::Scale>,
        #[template_child]
        pub bar_total: TemplateChild<gtk::Label>,
        #[template_child]
        pub bar_play: TemplateChild<gtk::Button>,
        #[template_child]
        pub bar_badge: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub bar_track_labels: TemplateChild<gtk::Box>,
        #[template_child]
        pub bar_badge_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub bar_rate_dot: TemplateChild<gtk::Box>,
        #[template_child]
        pub bar_path_grid: TemplateChild<gtk::Grid>,
        #[template_child]
        pub bar_volume: TemplateChild<gtk::Scale>,
        #[template_child]
        pub bar_heart: TemplateChild<gtk::Button>,
        #[template_child]
        pub sheet_heart: TemplateChild<gtk::Button>,

        pub session: OnceCell<Rc<Session>>,
        /// Kept between "Open TIDAL Login" and the pasted redirect.
        pub pkce: RefCell<Option<PkceAuthParams>>,
        pub queue: OnceCell<gio::ListStore>,
        /// The qids in `queue`, in order, and the row marked current.
        pub queue_qids: RefCell<Vec<String>>,
        pub queue_current: glib::WeakRef<crate::queue_row::QueueRow>,
        pub now: RefCell<Option<Now>>,
        pub signal_path: RefCell<Option<Box<SignalPath>>>,
        pub metas: RefCell<HashMap<u64, TrackMeta>>,
        pub covers: OnceCell<crate::covers::Covers>,
        /// The sidebar's pages, built on first use and kept.
        pub roots: RefCell<HashMap<crate::browse::Root, adw::NavigationPage>>,
        pub playing: Cell<bool>,
        pub repeat: Cell<RepeatMode>,
        /// Position reports are ignored until then after a seek, so the
        /// slider doesn't jump back while the engine catches up.
        pub hold_position: Cell<Option<Instant>>,
        pub pending_seek: RefCell<Option<glib::SourceId>>,
        /// The Search page's entry, for Ctrl+F.
        pub search_entry: glib::WeakRef<gtk::SearchEntry>,
        pub favorites: RefCell<crate::hearts::Favorites>,
        /// The account whose session was restored or signed in last (its
        /// queue is the one the player holds).
        pub account: Cell<Option<u64>>,
        /// Re-draw the player bar's and the sheet's hearts (track change).
        pub now_hearts: RefCell<crate::hearts::Redraws>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ZekeWindow {
        const NAME: &'static str = "ZekeWindow";
        type Type = super::ZekeWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ZekeWindow {
        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: std::sync::OnceLock<Vec<glib::subclass::Signal>> = std::sync::OnceLock::new();
            // The favorites changed (loaded, added, removed): hearts redraw.
            SIGNALS.get_or_init(|| vec![glib::subclass::Signal::builder("favorites-changed").build()])
        }
    }
    impl WidgetImpl for ZekeWindow {}
    impl WindowImpl for ZekeWindow {}
    impl ApplicationWindowImpl for ZekeWindow {}
    impl AdwApplicationWindowImpl for ZekeWindow {}
}

glib::wrapper! {
    pub struct ZekeWindow(ObjectSubclass<imp::ZekeWindow>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
            gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

#[gtk::template_callbacks]
impl ZekeWindow {
    pub fn new(app: &adw::Application, session: Rc<Session>, events: async_channel::Receiver<UiEvent>) -> Self {
        let window: Self = glib::Object::builder().property("application", app).build();
        let logged_in = session.settings().auth_tokens.is_some();
        window.imp().covers.set(crate::covers::Covers::new(Arc::clone(&session.state))).expect("set once");
        window.imp().session.set(session).expect("set once");
        window.setup_actions();
        window.setup_playback_keys();
        window.setup_player_view();
        window.setup_browse();
        window.listen(events);
        if logged_in {
            window.show_main();
            window.restore_session();
        } else {
            window.show_login();
        }
        window
    }

    pub fn session(&self) -> &Rc<Session> {
        self.imp().session.get().expect("set in new()")
    }

    pub fn covers(&self) -> &crate::covers::Covers {
        self.imp().covers.get().expect("set in new()")
    }

    pub fn send(&self, command: PlayerCommand) {
        self.session().send(command);
    }

    pub fn toast(&self, text: &str) {
        let toast = adw::Toast::builder().title(glib::markup_escape_text(text)).timeout(5).build();
        self.imp().toast_overlay.add_toast(toast);
    }

    fn setup_actions(&self) {
        let simple = |name: &str, f: fn(&Self)| {
            gio::ActionEntry::builder(name).activate(move |w: &Self, _, _| f(w)).build()
        };
        let shuffle = gio::ActionEntry::builder("shuffle")
            .state(false.to_variant())
            .activate(|w: &Self, action, _| {
                let on = !action.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
                action.set_state(&on.to_variant());
                w.send(PlayerCommand::SetShuffle(on));
            })
            .build();
        self.add_action_entries([
            simple("play-pause", |w| w.send(PlayerCommand::TogglePause)),
            simple("next", |w| w.send(PlayerCommand::Next)),
            simple("previous", |w| w.send(PlayerCommand::Previous)),
            simple("cycle-repeat", |w| {
                let next = match w.imp().repeat.get() {
                    RepeatMode::Off => RepeatMode::All,
                    RepeatMode::All => RepeatMode::One,
                    RepeatMode::One => RepeatMode::Off,
                };
                w.send(PlayerCommand::SetRepeat(next));
            }),
            shuffle,
            simple("logout", Self::logout),
            simple("preferences", crate::preferences::ZekePreferences::present),
            simple("search", Self::focus_search),
        ]);
        let app = self.application().expect("has an application");
        app.set_accels_for_action("win.preferences", &["<Control>comma"]);
        app.set_accels_for_action("win.search", &["<Control>f"]);
    }

    /// Space, Ctrl+→ and Ctrl+←, in the capture phase so a focused row or
    /// button doesn't take them first. They are left alone where they mean
    /// something else: a text field (typing, word jumps), a dialog, a menu,
    /// and the login page.
    fn setup_playback_keys(&self) {
        let controller = gtk::ShortcutController::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        for (trigger, action) in [("space", "play-pause"), ("<Control>Right", "next"), ("<Control>Left", "previous")] {
            let callback = gtk::CallbackAction::new(move |widget, _| {
                let Some(window) = widget.downcast_ref::<Self>() else { return glib::Propagation::Proceed };
                if !window.playback_keys_apply() {
                    return glib::Propagation::Proceed;
                }
                ActionGroupExt::activate_action(window, action, None);
                glib::Propagation::Stop
            });
            controller.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(trigger), Some(callback)));
        }
        self.add_controller(controller);
    }

    fn playback_keys_apply(&self) -> bool {
        let imp = self.imp();
        if imp.stack.visible_child_name().as_deref() != Some("main") || self.visible_dialog().is_some() {
            return false;
        }
        let Some(focus) = gtk::prelude::RootExt::focus(self) else { return true };
        let typing = focus.is::<gtk::Editable>() || focus.is::<gtk::TextView>();
        !typing && focus.ancestor(gtk::Popover::static_type()).is_none()
    }

    /// Ctrl+F: the Search page, with its entry focused.
    fn focus_search(&self) {
        let imp = self.imp();
        if imp.stack.visible_child_name().as_deref() != Some("main") {
            return;
        }
        imp.sheet.set_open(false);
        self.show_root(crate::browse::Root::Search);
        // A page shown for the first time focuses its entry itself.
        if let Some(entry) = imp.search_entry.upgrade() {
            entry.grab_focus();
        }
    }

    /// Apply the session's updates as they arrive on the main loop.
    fn listen(&self, events: async_channel::Receiver<UiEvent>) {
        let window = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                let Some(window) = window.upgrade() else { break };
                window.handle(event);
            }
        });
    }

    fn handle(&self, event: UiEvent) {
        let imp = self.imp();
        match event {
            UiEvent::State(state) => self.set_state(state),
            UiEvent::TrackStarted { item, duration, format, meta } => {
                if let Some(meta) = meta {
                    imp.metas.borrow_mut().insert(meta.track_id, meta);
                }
                imp.now.replace(Some(Now { track_id: item.track_id, duration, format }));
                imp.hold_position.set(None);
                self.show_position(0.0);
                self.refresh_now_playing();
            }
            UiEvent::Queue { items, current, shuffle, repeat } => {
                if let Some(action) = self.lookup_action("shuffle") {
                    action.change_state(&shuffle.to_variant());
                }
                self.set_repeat(repeat);
                let started = Instant::now();
                self.show_queue(&items, current);
                log::debug!(
                    "[queue-view] {} entries shown in {:.2} ms",
                    items.len(),
                    started.elapsed().as_secs_f64() * 1000.0
                );
            }
            UiEvent::Meta(meta) => {
                let id = meta.track_id;
                imp.metas.borrow_mut().insert(id, meta);
                self.refresh_queue_rows(id);
                if imp.now.borrow().as_ref().is_some_and(|n| n.track_id == id) {
                    self.refresh_now_playing();
                }
            }
            UiEvent::Position(p) => {
                if imp.hold_position.get().is_some_and(|until| Instant::now() < until) {
                    return;
                }
                self.show_position(p);
            }
            UiEvent::SignalPath(p) => {
                imp.signal_path.replace(Some(p));
                self.refresh_badge();
            }
            UiEvent::Error(e) => self.toast(&e),
            UiEvent::LoginExpired => self.login_expired(),
            UiEvent::Notice(n) => self.toast(&n),
            UiEvent::DeviceChosen(device) => {
                log::info!("[app] using {device} for exclusive mode");
                self.session().device_chosen(device);
            }
            UiEvent::Raise => self.present(),
            UiEvent::Quit => {
                if let Some(app) = self.application() {
                    app.quit();
                }
            }
            UiEvent::Volume(v) => imp.bar_volume.set_value(v),
        }
    }

    #[template_callback]
    fn on_open_sheet(&self) {
        self.imp().sheet.set_open(true);
    }

    #[template_callback]
    fn on_sign_in(&self) {
        #[cfg(feature = "webview")]
        self.open_login_window();
    }

    #[template_callback]
    fn on_use_browser(&self) {
        self.use_browser();
    }

    #[template_callback]
    fn on_open_login(&self) {
        self.open_login();
    }

    #[template_callback]
    fn on_login_apply(&self) {
        self.finish_login();
    }

    #[template_callback]
    fn on_queue_activate(&self, position: u32) {
        let row = self.imp().queue.get().and_then(|q| q.item(position)).and_downcast::<crate::queue_row::QueueRow>();
        if let Some(row) = row {
            self.send(PlayerCommand::JumpTo(row.qid()));
        }
    }
}
