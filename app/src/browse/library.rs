//! The playlist library (Favorites → Playlists): Loved Tracks, then the
//! folders, then the playlists, as TIDAL arranges them for the user. A
//! folder opens the same page one level down. Read only; the sort is the
//! page's menu and is saved.
//!
//! A level arrives whole in one load; a failed load shows the page's
//! retry state, never a part of the list.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use zeke_tidal::commands::browse::{self, LibraryEntry, PlaylistOwner};
use zeke_tidal::PlaylistSort;

use super::detail::count;
use super::{BrowsePage, Root, Shell, Target};
use crate::covers::{self, Kind};
use crate::runtime;
use crate::window::ZekeWindow;

/// One row of a library level.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    LovedTracks,
    Folder { id: String, name: String, subtitle: String },
    /// `by` and `tracks` are empty when TIDAL doesn't say.
    Playlist { uuid: String, title: String, by: String, tracks: String, cover: Option<String> },
}

/// The rows of a level: Loved Tracks first (on the root), then the
/// folders, then the playlists, each in TIDAL's order.
fn rows(entries: Vec<LibraryEntry>, loved_tracks: bool) -> Vec<Row> {
    let mut folders = Vec::new();
    let mut playlists = Vec::new();
    for entry in entries {
        match entry {
            LibraryEntry::Folder { id, name, count } => {
                folders.push(Row::Folder { id, name, subtitle: folder_subtitle(count) });
            }
            LibraryEntry::Playlist { playlist, owner } => playlists.push(Row::Playlist {
                by: byline(&owner),
                tracks: playlist.number_of_tracks.map(|n| count(n as usize)).unwrap_or_default(),
                cover: playlist.image,
                uuid: playlist.uuid,
                title: playlist.title,
            }),
        }
    }
    let mut out = Vec::with_capacity(1 + folders.len() + playlists.len());
    if loved_tracks {
        out.push(Row::LovedTracks);
    }
    out.extend(folders);
    out.extend(playlists);
    out
}

/// The side of a row's cover or icon, in pixels.
const COVER: i32 = 72;

fn folder_subtitle(playlists: Option<u32>) -> String {
    match playlists {
        Some(1) => "1 playlist".into(),
        Some(n) => format!("{n} playlists"),
        None => "Folder".into(),
    }
}

/// "By You", "By TIDAL", "By Ana"; empty when TIDAL doesn't say whose.
fn byline(owner: &PlaylistOwner) -> String {
    match owner {
        PlaylistOwner::You => "By You".into(),
        PlaylistOwner::Tidal => "By TIDAL".into(),
        PlaylistOwner::Creator(name) => format!("By {name}"),
        PlaylistOwner::Unknown => String::new(),
    }
}

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct LibraryObject {
        pub row: OnceCell<Row>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LibraryObject {
        const NAME: &'static str = "ZekeLibraryObject";
        type Type = super::LibraryObject;
    }

    impl ObjectImpl for LibraryObject {}

    #[derive(Debug, Default)]
    pub struct LibraryRow {
        pub cover: gtk::Picture,
        /// In the cover's place for Loved Tracks and folders.
        pub icon: gtk::Image,
        pub title: gtk::Label,
        /// Whose playlist it is.
        pub byline: gtk::Label,
        /// How many tracks or playlists, or what it is.
        pub detail: gtk::Label,
        /// Folders only.
        pub chevron: gtk::Image,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LibraryRow {
        const NAME: &'static str = "ZekeLibraryRow";
        type Type = super::LibraryRow;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for LibraryRow {}
    impl WidgetImpl for LibraryRow {}
    impl BoxImpl for LibraryRow {}
}

glib::wrapper! {
    /// A row of a library level in a `gio::ListStore`.
    pub struct LibraryObject(ObjectSubclass<imp::LibraryObject>);
}

glib::wrapper! {
    /// A library row: cover or icon, then the title, whose it is and how
    /// many tracks, one under the other.
    pub struct LibraryRow(ObjectSubclass<imp::LibraryRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl LibraryObject {
    fn new(row: Row) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().row.set(row).expect("new object");
        obj
    }

    fn row(&self) -> &Row {
        self.imp().row.get().expect("set in new()")
    }
}

impl LibraryRow {
    fn new() -> Self {
        let row: Self = glib::Object::builder().property("spacing", 12).build();
        let imp = row.imp();
        row.add_css_class("track-row");
        imp.cover.set_size_request(COVER, COVER);
        imp.cover.set_content_fit(gtk::ContentFit::Cover);
        imp.cover.set_can_shrink(true);
        imp.cover.set_valign(gtk::Align::Center);
        imp.cover.add_css_class("cover");
        imp.icon.set_pixel_size(32);
        imp.icon.set_size_request(COVER, COVER);
        imp.icon.set_valign(gtk::Align::Center);
        imp.icon.add_css_class("cover");
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        for label in [&imp.title, &imp.byline, &imp.detail] {
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.append(label);
        }
        imp.title.add_css_class("heading");
        for label in [&imp.byline, &imp.detail] {
            label.add_css_class("dim-label");
            label.add_css_class("caption");
        }
        imp.chevron.set_icon_name(Some("go-next-symbolic"));
        imp.chevron.add_css_class("dim-label");
        row.append(&imp.cover);
        row.append(&imp.icon);
        row.append(&text);
        row.append(&imp.chevron);
        row
    }

    fn bind(&self, row: &Row, covers: &covers::Covers) {
        let imp = self.imp();
        let (title, by, detail, icon, cover) = match row {
            Row::LovedTracks => ("Loved Tracks", "", "Collection", Some("heart-filled-symbolic"), None),
            Row::Folder { name, subtitle, .. } => (name.as_str(), "", subtitle.as_str(), Some("folder-symbolic"), None),
            Row::Playlist { title, by, tracks, cover, .. } => (title.as_str(), by.as_str(), tracks.as_str(), None, Some(cover)),
        };
        imp.title.set_label(title);
        for (label, text) in [(&imp.byline, by), (&imp.detail, detail)] {
            label.set_label(text);
            label.set_visible(!text.is_empty());
        }
        imp.icon.set_visible(icon.is_some());
        imp.icon.set_icon_name(icon);
        if matches!(row, Row::LovedTracks) {
            imp.icon.add_css_class("loved-tracks");
        } else {
            imp.icon.remove_css_class("loved-tracks");
        }
        imp.cover.set_visible(cover.is_some());
        covers.show(&imp.cover, cover.and_then(|c| c.as_deref()).map(|id| covers::url(id, Kind::Album, covers::ROW)));
        imp.chevron.set_visible(matches!(row, Row::Folder { .. }));
    }
}

struct Library {
    window: glib::WeakRef<ZekeWindow>,
    shell: Rc<Shell>,
    scroller: gtk::ScrolledWindow,
    store: gio::ListStore,
    /// `None` is the root, which also has Loved Tracks.
    folder: Option<String>,
    /// Which load is the latest; an older one's answer is dropped.
    generation: Cell<u64>,
    /// A folder page follows the sort; the root is rebuilt by the window.
    sort_watch: RefCell<Option<(gio::Action, glib::SignalHandlerId)>>,
}

impl Drop for Library {
    fn drop(&mut self) {
        if let Some((action, id)) = self.sort_watch.take() {
            action.disconnect(id);
        }
    }
}

/// The root list, or the list of `folder` (its id and name).
pub fn page(window: &ZekeWindow, folder: Option<(String, String)>) -> BrowsePage {
    let (shell, page) = Shell::new(folder.as_ref().map_or("Playlists", |(_, name)| name.as_str()));
    shell.header.pack_end(&sort_button());

    let store = gio::ListStore::new::<LibraryObject>();
    let view = list_view(window, &store);
    // ClampScrollable, not `clamped`: a plain Clamp would put the list in a
    // viewport at its full height, building every row and cover at once.
    let clamp = adw::ClampScrollable::builder().maximum_size(1400).tightening_threshold(1000).child(&view).build();
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).build();
    let this = Rc::new(Library {
        window: window.downgrade(),
        shell,
        scroller,
        store,
        folder: folder.map(|(id, _)| id),
        generation: Cell::new(0),
        sort_watch: RefCell::default(),
    });
    if this.folder.is_some()
        && let Some(action) = window.lookup_action("playlist-sort")
    {
        let weak = Rc::downgrade(&this);
        let id = action.connect_notify_local(Some("state"), move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.load();
            }
        });
        this.sort_watch.replace(Some((action, id)));
    }
    this.load();
    page.keep(this);
    page
}

/// The header's sort menu: radio items of the window's `playlist-sort`.
fn sort_button() -> gtk::MenuButton {
    let menu = gio::Menu::new();
    for (label, sort) in
        [("Last Updated", PlaylistSort::LastUpdated), ("Date Added", PlaylistSort::DateAdded), ("Name", PlaylistSort::Name)]
    {
        menu.append(Some(label), Some(&format!("win.playlist-sort::{}", sort.order())));
    }
    gtk::MenuButton::builder().icon_name("view-sort-descending-symbolic").tooltip_text("Sort By").menu_model(&menu).build()
}

fn list_view(window: &ZekeWindow, store: &gio::ListStore) -> gtk::ListView {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
        item.set_child(Some(&LibraryRow::new()));
    });
    let covers = window.covers().clone();
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
        if let (Some(row), Some(object)) = (item.child().and_downcast::<LibraryRow>(), item.item().and_downcast::<LibraryObject>())
        {
            row.bind(object.row(), &covers);
        }
    });
    let view = gtk::ListView::builder()
        .model(&gtk::NoSelection::new(Some(store.clone())))
        .factory(&factory)
        .single_click_activate(true)
        .css_classes(["track-list", "browse-page"])
        .build();
    let (window, store) = (window.downgrade(), store.clone());
    view.connect_activate(move |_, position| {
        let (Some(window), Some(object)) = (window.upgrade(), store.item(position).and_downcast::<LibraryObject>()) else {
            return;
        };
        match object.row().clone() {
            Row::LovedTracks => window.show_root(Root::FavoriteTracks),
            Row::Folder { id, name, .. } => window.open(Target::PlaylistFolder { id, name }),
            Row::Playlist { uuid, .. } => window.open(Target::Playlist(uuid)),
        }
    });
    view
}

impl Library {
    fn load(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else { return };
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.shell.loading();
        let state = Arc::clone(&window.session().state);
        let sort = window.session().settings().playlist_sort;
        let id = self.folder.clone().unwrap_or_else(|| "root".into());
        let weak = Rc::downgrade(self);
        runtime::spawn(async move { browse::playlist_folder(&state, &id, sort).await }, move |result| {
            let Some(this) = weak.upgrade() else { return };
            let Some(window) = this.window.upgrade() else { return };
            if this.generation.get() != generation {
                return;
            }
            match result {
                Ok(entries) => {
                    let items: Vec<LibraryObject> =
                        rows(entries, this.folder.is_none()).into_iter().map(LibraryObject::new).collect();
                    this.store.remove_all();
                    this.store.extend_from_slice(&items);
                    if items.is_empty() {
                        this.shell.empty("Empty Folder", "Playlists you add to this folder in TIDAL show up here.");
                    } else {
                        this.shell.show(&this.scroller);
                    }
                }
                Err(e) => {
                    let retry = Rc::downgrade(&this);
                    this.shell.failed(&window, "your playlists", &e, move || {
                        if let Some(this) = retry.upgrade() {
                            this.load();
                        }
                    });
                }
            }
        });
    }
}

impl ZekeWindow {
    /// The sort menu shows the saved sort (on sign-in).
    pub fn sync_playlist_sort(&self) {
        if let Some(action) = self.lookup_action("playlist-sort").and_downcast::<gio::SimpleAction>() {
            action.set_state(&self.session().settings().playlist_sort.order().to_variant());
        }
    }

    /// The menu's choice: saved, and every library page follows. A folder
    /// page does by watching the action's state; the root, cached or
    /// showing, is built again.
    pub fn set_playlist_sort(&self, sort: PlaylistSort) {
        // The checked item again: nothing to reload.
        if sort == self.session().settings().playlist_sort {
            return;
        }
        self.session().set_playlist_sort(sort);
        self.sync_playlist_sort();
        self.reload_root_when_shown(Root::FavoritePlaylists);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeke_tidal::tidal_api::TidalPlaylist;

    fn playlist(uuid: &str, tracks: Option<u32>) -> TidalPlaylist {
        serde_json::from_value(serde_json::json!({
            "uuid": uuid, "title": uuid, "image": "img", "numberOfTracks": tracks, "creator": null
        }))
        .unwrap()
    }

    fn folder(id: &str, count: Option<u32>) -> LibraryEntry {
        LibraryEntry::Folder { id: id.into(), name: id.into(), count }
    }

    #[test]
    fn the_root_is_loved_tracks_then_folders_then_playlists() {
        let entries = vec![
            LibraryEntry::Playlist { playlist: playlist("p1", Some(3)), owner: PlaylistOwner::You },
            folder("f1", Some(2)),
            LibraryEntry::Playlist { playlist: playlist("p2", Some(1)), owner: PlaylistOwner::Tidal },
            folder("f2", None),
        ];
        let kinds = |rows: &[Row]| {
            rows.iter()
                .map(|r| match r {
                    Row::LovedTracks => "loved".to_string(),
                    Row::Folder { id, .. } | Row::Playlist { uuid: id, .. } => id.clone(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(&rows(entries.clone(), true)), ["loved", "f1", "f2", "p1", "p2"]);
        assert_eq!(kinds(&rows(entries, false)), ["f1", "f2", "p1", "p2"], "a folder has no Loved Tracks");
        assert!(rows(Vec::new(), false).is_empty(), "an empty folder");
        assert_eq!(rows(Vec::new(), true), [Row::LovedTracks], "an empty library");
    }

    #[test]
    fn subtitles_say_who_and_how_many() {
        assert_eq!(folder_subtitle(Some(4)), "4 playlists");
        assert_eq!(folder_subtitle(Some(1)), "1 playlist");
        assert_eq!(folder_subtitle(None), "Folder");
        assert_eq!(byline(&PlaylistOwner::You), "By You");
        assert_eq!(byline(&PlaylistOwner::Tidal), "By TIDAL");
        assert_eq!(byline(&PlaylistOwner::Creator("Ana".into())), "By Ana");
        assert_eq!(byline(&PlaylistOwner::Unknown), "");
    }
}
