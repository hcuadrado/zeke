//! The list widgets browse pages are built from: a track list
//! (`GtkListView`), a horizontal row of cards (`GtkListView`) and a card
//! grid (`GtkGridView`), all over `gio::ListStore`s of `TrackObject` /
//! `CardObject` with recycled rows. No `GtkListBox` for tracks.

use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use super::model::{CardKind, CardObject, TrackObject};
use super::Target;
use zeke_tidal::commands::browse::Favorite;
use crate::covers;
use crate::queue_row::format_time;
use crate::window::ZekeWindow;

/// How a track list shows its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackStyle {
    /// Track numbers, no covers: an album's own tracks.
    #[default]
    Album,
    /// A cover per row and the album under the title.
    Mixed,
}

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct TrackRow {
        pub number: gtk::Label,
        /// In the number's place on the playing track (album lists).
        pub playing: gtk::Image,
        pub cover: gtk::Picture,
        pub title: gtk::Label,
        pub subtitle: gtk::Label,
        pub hires: gtk::Label,
        pub length: gtk::Label,
        pub menu: gtk::MenuButton,
        /// "row.*": play next, add to queue, go to album / artist.
        pub actions: gio::SimpleActionGroup,
        pub item: RefCell<Option<TrackObject>>,
        pub style: std::cell::Cell<TrackStyle>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TrackRow {
        const NAME: &'static str = "ZekeTrackRow";
        type Type = super::TrackRow;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for TrackRow {}
    impl WidgetImpl for TrackRow {}
    impl BoxImpl for TrackRow {}

    #[derive(Debug, Default)]
    pub struct CardTile {
        pub picture: gtk::Picture,
        pub title: gtk::Label,
        pub subtitle: gtk::Label,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CardTile {
        const NAME: &'static str = "ZekeCardTile";
        type Type = super::CardTile;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for CardTile {}
    impl WidgetImpl for CardTile {}
    impl BoxImpl for CardTile {}
}

glib::wrapper! {
    /// One row of a track list: number or cover, title over artists,
    /// length, and a menu (play next, add to queue, go to album/artist).
    pub struct TrackRow(ObjectSubclass<imp::TrackRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

glib::wrapper! {
    /// A card: picture, title, subtitle.
    pub struct CardTile(ObjectSubclass<imp::CardTile>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl TrackRow {
    fn new(style: TrackStyle) -> Self {
        let row: Self = glib::Object::builder().property("spacing", 12).build();
        let imp = row.imp();
        imp.style.set(style);
        row.add_css_class("track-row");

        imp.number.set_width_chars(3);
        imp.number.set_xalign(1.0);
        imp.number.add_css_class("dim-label");
        imp.number.add_css_class("numeric");
        imp.number.set_visible(style == TrackStyle::Album);
        imp.playing.set_icon_name(Some("audio-volume-high-symbolic"));
        imp.playing.set_width_request(imp.number.width_chars() * 8);
        imp.playing.set_halign(gtk::Align::End);
        imp.playing.set_visible(false);
        imp.playing.add_css_class("accent");
        imp.cover.set_size_request(40, 40);
        imp.cover.set_content_fit(gtk::ContentFit::Cover);
        imp.cover.set_can_shrink(true);
        imp.cover.set_valign(gtk::Align::Center);
        imp.cover.add_css_class("cover");
        imp.cover.set_visible(style == TrackStyle::Mixed);

        for label in [&imp.title, &imp.subtitle] {
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        }
        imp.subtitle.add_css_class("dim-label");
        imp.subtitle.add_css_class("caption");
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).valign(gtk::Align::Center).build();
        text.append(&imp.title);
        text.append(&imp.subtitle);

        imp.hires.set_label("HI-RES");
        imp.hires.set_valign(gtk::Align::Center);
        imp.hires.add_css_class("quality-badge");
        imp.hires.set_tooltip_text(Some("Hi-res lossless"));
        imp.length.add_css_class("dim-label");
        imp.length.add_css_class("numeric");

        let menu = gio::Menu::new();
        let queue = gio::Menu::new();
        queue.append(Some("Play Next"), Some("row.play-next"));
        queue.append(Some("Add to Queue"), Some("row.add-to-queue"));
        // One of the two shows, as the track is a favorite or not.
        let favorite = gio::Menu::new();
        for (label, action) in [("Add to Favorites", "row.favorite-add"), ("Remove from Favorites", "row.favorite-remove")] {
            let item = gio::MenuItem::new(Some(label), Some(action));
            item.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
            favorite.append_item(&item);
        }
        let go = gio::Menu::new();
        go.append(Some("Go to Album"), Some("row.go-album"));
        go.append(Some("Go to Artist"), Some("row.go-artist"));
        menu.append_section(None, &queue);
        menu.append_section(None, &favorite);
        menu.append_section(None, &go);
        imp.menu.set_menu_model(Some(&menu));
        imp.menu.set_icon_name("view-more-symbolic");
        imp.menu.set_tooltip_text(Some("More"));
        imp.menu.set_valign(gtk::Align::Center);
        imp.menu.add_css_class("flat");
        imp.menu.add_css_class("circular");

        row.append(&imp.number);
        row.append(&imp.playing);
        row.append(&imp.cover);
        row.append(&text);
        row.append(&imp.hires);
        row.append(&imp.length);
        row.append(&imp.menu);
        row.setup_actions();
        row
    }

    fn setup_actions(&self) {
        let group = &self.imp().actions;
        let action = |name: &str, run: fn(&ZekeWindow, &TrackObject)| {
            let a = gio::SimpleAction::new(name, None);
            let row = self.downgrade();
            a.connect_activate(move |_, _| {
                let Some(row) = row.upgrade() else { return };
                let item = row.imp().item.borrow().clone();
                if let (Some(window), Some(item)) = (row.root().and_downcast::<ZekeWindow>(), item) {
                    run(&window, &item);
                }
            });
            group.add_action(&a);
        };
        action("play-next", |w, t| w.play_next(t.data()));
        action("add-to-queue", |w, t| w.add_to_queue(t.data()));
        action("go-album", |w, t| {
            if let Some(id) = t.data().album_id {
                w.open(Target::Album(id));
            }
        });
        action("go-artist", |w, t| {
            if let Some(id) = t.data().artist_id {
                w.open(Target::Artist(id));
            }
        });
        action("favorite-add", |w, t| w.set_favorite(Favorite::Track(t.data().id), true, &t.data().title));
        action("favorite-remove", |w, t| w.set_favorite(Favorite::Track(t.data().id), false, &t.data().title));
        self.insert_action_group("row", Some(group));
        // The favorites may have changed since the row was bound.
        let row = self.downgrade();
        self.imp().menu.connect_active_notify(move |menu| {
            if let Some(row) = row.upgrade()
                && menu.is_active()
            {
                row.sync_favorite();
            }
        });
    }

    /// Enable whichever of add/remove applies to the bound track.
    fn sync_favorite(&self) {
        let imp = self.imp();
        let id = imp.item.borrow().as_ref().map(|t| t.data().id);
        let on = match (id, self.root().and_downcast::<ZekeWindow>()) {
            (Some(id), Some(window)) => Some(window.is_favorite(&Favorite::Track(id))),
            _ => None,
        };
        for (name, enabled) in [("favorite-add", on == Some(false)), ("favorite-remove", on == Some(true))] {
            if let Some(a) = imp.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(enabled);
            }
        }
    }

    fn bind(&self, item: &TrackObject, place: u32, style: TrackStyle, covers: &covers::Covers) {
        let imp = self.imp();
        let t = item.data();
        imp.number.set_label(&t.number.unwrap_or(place + 1).to_string());
        imp.title.set_label(&t.title);
        let subtitle = match style {
            TrackStyle::Mixed if !t.album.is_empty() => format!("{} · {}", t.artists, t.album),
            _ => t.artists.clone(),
        };
        imp.subtitle.set_label(&subtitle);
        imp.hires.set_visible(t.hires);
        imp.length.set_label(&t.duration.map(|d| format_time(f64::from(d))).unwrap_or_default());
        if style == TrackStyle::Mixed {
            covers.show(&imp.cover, t.cover_url(covers::ROW));
        }
        for (name, enabled) in [("go-album", t.album_id.is_some()), ("go-artist", t.artist_id.is_some())] {
            if let Some(a) = imp.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(enabled);
            }
        }
        imp.item.replace(Some(item.clone()));
        self.sync_favorite();
        self.sync_playing(style);
    }

    /// Mark the row as the one playing (as the queue does), or not.
    fn sync_playing(&self, style: TrackStyle) {
        let imp = self.imp();
        let id = imp.item.borrow().as_ref().map(|t| t.data().id);
        let playing = id.is_some() && id == self.root().and_downcast::<ZekeWindow>().and_then(|w| w.playing_id());
        if playing {
            imp.title.add_css_class("queue-current");
        } else {
            imp.title.remove_css_class("queue-current");
        }
        if style == TrackStyle::Album {
            imp.number.set_visible(!playing);
            imp.playing.set_visible(playing);
        }
    }
}

impl ZekeWindow {
    /// The track the player is on, if any.
    pub fn playing_id(&self) -> Option<u64> {
        self.imp().now.borrow().as_ref().map(|n| n.track_id)
    }

    /// Re-mark the playing track in every live browse track list.
    pub fn refresh_track_rows(&self) {
        self.imp().track_rows.borrow_mut().retain(|row| {
            let Some(row) = row.upgrade() else { return false };
            if row.imp().item.borrow().is_some() {
                row.sync_playing(row.imp().style.get());
            }
            true
        });
    }
}

impl CardTile {
    fn new(width: i32) -> Self {
        let tile: Self =
            glib::Object::builder().property("orientation", gtk::Orientation::Vertical).property("spacing", 6).build();
        let imp = tile.imp();
        tile.add_css_class("card-tile");
        tile.set_width_request(width);
        imp.picture.set_size_request(width, width);
        imp.picture.set_content_fit(gtk::ContentFit::Cover);
        imp.picture.set_can_shrink(true);
        imp.picture.add_css_class("cover");
        tile.append(&imp.picture);
        for label in [&imp.title, &imp.subtitle] {
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_max_width_chars(1);
            label.set_hexpand(true);
        }
        imp.title.add_css_class("heading");
        imp.subtitle.add_css_class("dim-label");
        imp.subtitle.add_css_class("caption");
        tile.append(&imp.title);
        tile.append(&imp.subtitle);
        tile
    }

    fn bind(&self, card: &CardObject, covers: &covers::Covers) {
        let imp = self.imp();
        let c = card.data();
        imp.title.set_label(&c.title);
        imp.title.set_tooltip_text(Some(&c.title));
        imp.subtitle.set_label(&c.subtitle);
        if c.is_artist() {
            imp.picture.add_css_class("round");
        } else {
            imp.picture.remove_css_class("round");
        }
        if matches!(c.kind, CardKind::MyTracks) {
            imp.picture.add_css_class("loved-tracks");
        } else {
            imp.picture.remove_css_class("loved-tracks");
        }
        covers.show(&imp.picture, c.image.as_ref().and_then(|i| i.url(covers::CARD)));
    }
}

/// A track list over `store`. Activating a row calls `on_play` with its
/// position (the page decides the queue).
pub fn track_list(
    window: &ZekeWindow,
    store: &gio::ListStore,
    style: TrackStyle,
    on_play: impl Fn(u32) + 'static,
) -> gtk::ListView {
    let factory = gtk::SignalListItemFactory::new();
    let w = window.downgrade();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
        let row = TrackRow::new(style);
        if let Some(window) = w.upgrade() {
            window.imp().track_rows.borrow_mut().push(row.downgrade());
        }
        item.set_child(Some(&row));
    });
    let covers = window.covers().clone();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
        let (Some(row), Some(track)) = (item.child().and_downcast::<TrackRow>(), item.item().and_downcast::<TrackObject>())
        else {
            return;
        };
        row.bind(&track, item.position(), style, &covers);
    });
    factory.connect_unbind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
        if let Some(row) = item.child().and_downcast::<TrackRow>() {
            row.imp().item.take();
        }
    });
    let view = gtk::ListView::builder()
        .model(&gtk::NoSelection::new(Some(store.clone())))
        .factory(&factory)
        .single_click_activate(true)
        .css_classes(["track-list"])
        .build();
    view.connect_activate(move |_, position| on_play(position));
    view
}

fn card_factory(window: &ZekeWindow, width: i32) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
        item.set_child(Some(&CardTile::new(width)));
    });
    let covers = window.covers().clone();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
        if let (Some(tile), Some(card)) = (item.child().and_downcast::<CardTile>(), item.item().and_downcast::<CardObject>())
        {
            tile.bind(&card, &covers);
        }
    });
    factory
}

/// Width of a card in a row or grid, in px.
pub const CARD_WIDTH: i32 = 160;

/// A horizontal, scrollable row of cards over `store`.
pub fn card_row(window: &ZekeWindow, store: &gio::ListStore) -> gtk::ScrolledWindow {
    let view = gtk::ListView::builder()
        .orientation(gtk::Orientation::Horizontal)
        .model(&gtk::NoSelection::new(Some(store.clone())))
        .factory(&card_factory(window, CARD_WIDTH))
        .single_click_activate(true)
        .css_classes(["card-row"])
        .build();
    connect_cards(window, &view, store);
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .child(&view)
        .build()
}

/// A grid of cards over `store` (put it in a `ScrolledWindow`).
pub fn card_grid(window: &ZekeWindow, store: &gio::ListStore) -> gtk::GridView {
    let view = gtk::GridView::builder()
        .model(&gtk::NoSelection::new(Some(store.clone())))
        .factory(&card_factory(window, CARD_WIDTH))
        .single_click_activate(true)
        .min_columns(2)
        .max_columns(12)
        .css_classes(["card-grid"])
        .build();
    let (w, s) = (window.downgrade(), store.clone());
    view.connect_activate(move |_, position| {
        if let Some(window) = w.upgrade() {
            window.open_card(&s, position);
        }
    });
    view
}

fn connect_cards(window: &ZekeWindow, view: &gtk::ListView, store: &gio::ListStore) {
    let (w, s) = (window.downgrade(), store.clone());
    view.connect_activate(move |_, position| {
        if let Some(window) = w.upgrade() {
            window.open_card(&s, position);
        }
    });
}

/// A section: its title (and a "View All" button when `view_all` is
/// given) over `body`.
pub fn section(title: &str, view_all: Option<Box<dyn Fn()>>, body: &impl IsA<gtk::Widget>) -> gtk::Box {
    let header = gtk::Box::builder().spacing(12).build();
    let label = gtk::Label::builder().label(title).xalign(0.0).hexpand(true).wrap(true).css_classes(["title-3"]).build();
    header.append(&label);
    if let Some(open) = view_all {
        let button = gtk::Button::builder().label("View All").css_classes(["flat"]).valign(gtk::Align::Center).build();
        button.connect_clicked(move |_| open());
        header.append(&button);
    }
    let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).css_classes(["browse-section"]).build();
    section.append(&header);
    section.append(body);
    section
}

/// A store of `TrackObject`s.
pub fn track_store(tracks: impl IntoIterator<Item = super::model::TrackData>) -> gio::ListStore {
    let store = gio::ListStore::new::<TrackObject>();
    let items: Vec<TrackObject> = tracks.into_iter().map(TrackObject::new).collect();
    store.extend_from_slice(&items);
    store
}

/// A store of `CardObject`s.
pub fn card_store(cards: impl IntoIterator<Item = super::model::CardData>) -> gio::ListStore {
    let store = gio::ListStore::new::<CardObject>();
    let items: Vec<CardObject> = cards.into_iter().map(CardObject::new).collect();
    store.extend_from_slice(&items);
    store
}

/// The tracks of `store` (a store of `TrackObject`s), in order.
pub fn tracks_of(store: &gio::ListStore) -> Vec<super::model::TrackData> {
    store.iter::<TrackObject>().flatten().map(|t| t.data().clone()).collect()
}
