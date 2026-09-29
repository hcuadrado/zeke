//! Browsing: the sidebar's pages (Home,
//! Search, Favorites) as roots of the content `AdwNavigationView`, and the
//! album, playlist, artist, mix and "view all" pages pushed onto it.
//!
//! Pages load through `runtime::spawn` (TIDAL on tokio, widgets here) with
//! a loading state, an empty state, and errors as toasts. They only send
//! `PlayerCommand`s; the queue they start carries each track's metadata.

mod artist;
mod detail;
mod favorites;
mod home;
pub mod model;
mod search;
pub mod views;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use zeke_player::PlayerCommand;
use zeke_tidal::commands::browse;

use crate::runtime;
use crate::window::ZekeWindow;
use model::{CardKind, CardObject, TrackData};

/// The sidebar's entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Root {
    Home,
    Search,
    FavoriteTracks,
    FavoriteAlbums,
    FavoriteArtists,
    FavoritePlaylists,
}

impl Root {
    /// The sidebar row's widget name.
    fn name(self) -> &'static str {
        match self {
            Root::Home => "home",
            Root::Search => "search",
            Root::FavoriteTracks => "fav-tracks",
            Root::FavoriteAlbums => "fav-albums",
            Root::FavoriteArtists => "fav-artists",
            Root::FavoritePlaylists => "fav-playlists",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [
            Root::Home,
            Root::Search,
            Root::FavoriteTracks,
            Root::FavoriteAlbums,
            Root::FavoriteArtists,
            Root::FavoritePlaylists,
        ]
        .into_iter()
        .find(|r| r.name() == name)
    }
}

/// A page to push.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Album(u64),
    Playlist(String),
    /// A mix, or a radio station (TIDAL serves those as mixes). `kind`
    /// labels the page ("Mix" when `None`); `title` stands in until the
    /// mix brings its own.
    Mix { id: String, title: Option<String>, kind: Option<&'static str> },
    Artist(u64),
    /// A Home section's "view all" (`get_page_section`).
    ViewAll { title: String, api_path: String },
    /// An artist section's "view all" (v2 `artist/…/view-all`).
    ArtistViewAll { title: String, artist: u64, path: String, tracks: bool },
}

mod page_imp {
    use std::any::Any;
    use std::cell::RefCell;

    use adw::subclass::prelude::*;
    use gtk::glib;

    #[derive(Default)]
    pub struct BrowsePage {
        /// The page's state; lives exactly as long as the page.
        pub owner: RefCell<Option<Box<dyn Any>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BrowsePage {
        const NAME: &'static str = "ZekeBrowsePage";
        type Type = super::BrowsePage;
        type ParentType = adw::NavigationPage;
    }

    impl ObjectImpl for BrowsePage {}
    impl WidgetImpl for BrowsePage {}
    impl NavigationPageImpl for BrowsePage {}
}

glib::wrapper! {
    /// A navigation page that owns its state (dropped with the page when
    /// it is popped), so loads in flight hold it only weakly.
    pub struct BrowsePage(ObjectSubclass<page_imp::BrowsePage>)
        @extends adw::NavigationPage, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl BrowsePage {
    pub fn keep(&self, owner: impl std::any::Any) {
        self.imp().owner.replace(Some(Box::new(owner)));
    }
}

/// A page's frame: header bar, and a stack of loading / error / empty /
/// content states. It holds the page's children, never the page (which
/// owns the state that holds this).
pub struct Shell {
    pub header: adw::HeaderBar,
    stack: gtk::Stack,
    error: adw::StatusPage,
    empty: adw::StatusPage,
    retry: RefCell<Option<Rc<dyn Fn()>>>,
}

impl Shell {
    pub fn new(title: &str) -> (Rc<Self>, BrowsePage) {
        let header = adw::HeaderBar::new();
        let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
        let spinner = adw::Spinner::builder().width_request(32).height_request(32).halign(gtk::Align::Center).build();
        stack.add_named(&spinner, Some("loading"));
        let retry = gtk::Button::builder().label("Try Again").halign(gtk::Align::Center).css_classes(["pill"]).build();
        let error = adw::StatusPage::builder()
            .icon_name("network-error-symbolic")
            .title("Couldn’t Load This Page")
            .child(&retry)
            .build();
        stack.add_named(&error, Some("error"));
        let empty = adw::StatusPage::builder().icon_name("folder-music-symbolic").build();
        stack.add_named(&empty, Some("empty"));
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&stack));
        let page: BrowsePage = glib::Object::builder().property("title", title).property("child", &view).build();
        let shell = Rc::new(Self { header, stack, error, empty, retry: RefCell::default() });
        let weak = Rc::downgrade(&shell);
        retry.connect_clicked(move |_| {
            // Out of the cell first: a retry may set a new one.
            let retry = weak.upgrade().and_then(|shell| shell.retry.borrow().clone());
            if let Some(retry) = retry {
                retry();
            }
        });
        (shell, page)
    }

    pub fn loading(&self) {
        self.stack.set_visible_child_name("loading");
    }

    /// Show `content` (added on first use).
    pub fn show(&self, content: &impl IsA<gtk::Widget>) {
        if self.stack.child_by_name("content").as_ref() != Some(content.upcast_ref()) {
            if let Some(old) = self.stack.child_by_name("content") {
                self.stack.remove(&old);
            }
            self.stack.add_named(content, Some("content"));
        }
        self.stack.set_visible_child_name("content");
    }

    pub fn empty(&self, title: &str, description: &str) {
        self.empty.set_title(title);
        self.empty.set_description(Some(description));
        self.stack.set_visible_child_name("empty");
    }

    /// A load failed: a toast, and a retry button in the page (or, if the
    /// login expired, the login page).
    pub fn failed(&self, window: &ZekeWindow, what: &str, error: &zeke_tidal::TidalError, retry: impl Fn() + 'static) {
        let Some(message) = window.report(&format!("load {what}"), error) else { return };
        self.failed_with(message, retry);
    }

    /// A load failed with `message` (already reported).
    pub fn failed_with(&self, message: &str, retry: impl Fn() + 'static) {
        self.error.set_description(Some(message));
        self.retry.replace(Some(Rc::new(retry)));
        self.stack.set_visible_child_name("error");
    }
}

/// A detail page's heading: picture, a kind line ("Album"), title,
/// subtitle, details, and Play / Shuffle.
pub struct Heading {
    pub widget: gtk::Box,
    pub picture: gtk::Picture,
    pub title: gtk::Label,
    pub subtitle: gtk::Label,
    pub details: gtk::Label,
    pub play: gtk::Button,
    pub shuffle: gtk::Button,
    /// Hidden until the page binds it (`hearts::bind_heart`).
    pub heart: gtk::Button,
}

impl Heading {
    pub fn new(kind: &str, round: bool) -> Self {
        let picture = gtk::Picture::builder()
            .width_request(200)
            .height_request(200)
            .content_fit(gtk::ContentFit::Cover)
            .can_shrink(true)
            .valign(gtk::Align::Center)
            .css_classes(if round { vec!["cover", "round"] } else { vec!["cover"] })
            .build();
        let kind = gtk::Label::builder().label(kind).xalign(0.0).css_classes(["caption-heading", "dim-label"]).build();
        let title = gtk::Label::builder().xalign(0.0).wrap(true).selectable(true).css_classes(["title-1"]).build();
        let subtitle = gtk::Label::builder().xalign(0.0).wrap(true).build();
        let details = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label", "caption"]).build();
        let play = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("media-playback-start-symbolic").label("Play").build())
            .css_classes(["pill", "suggested-action"])
            .sensitive(false)
            .build();
        let shuffle = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("media-playlist-shuffle-symbolic").label("Shuffle").build())
            .css_classes(["pill"])
            .sensitive(false)
            .build();
        let heart = crate::hearts::heart_button();
        let buttons = gtk::Box::builder().spacing(12).margin_top(8).build();
        buttons.append(&play);
        buttons.append(&shuffle);
        buttons.append(&heart);
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).valign(gtk::Align::Center).hexpand(true).build();
        for w in [kind.upcast_ref::<gtk::Widget>(), title.upcast_ref(), subtitle.upcast_ref(), details.upcast_ref(), buttons.upcast_ref()] {
            text.append(w);
        }
        let widget = gtk::Box::builder().spacing(24).css_classes(["page-heading"]).build();
        // A picture's natural size is its texture's; the clamp holds it.
        widget.append(&adw::Clamp::builder().maximum_size(200).valign(gtk::Align::Center).child(&picture).build());
        widget.append(&text);
        Self { widget, picture, title, subtitle, details, play, shuffle, heart }
    }
}

/// Page margins, as a clamp so wide windows keep a readable width.
pub fn clamped(child: &impl IsA<gtk::Widget>) -> adw::Clamp {
    adw::Clamp::builder().maximum_size(1400).tightening_threshold(1000).child(child).build()
}

/// The track cards of a row, and the place among them of the card at
/// `position` (a mixed row also has albums, playlists and mixes).
fn row_queue(kinds: &[&CardKind], position: usize) -> (Vec<TrackData>, Option<usize>) {
    let tracks: Vec<TrackData> = kinds
        .iter()
        .filter_map(|k| match k {
            CardKind::Track(t) => Some((**t).clone()),
            _ => None,
        })
        .collect();
    let start = matches!(kinds.get(position), Some(CardKind::Track(_)))
        .then(|| kinds[..position].iter().filter(|k| matches!(k, CardKind::Track(_))).count());
    (tracks, start)
}

impl ZekeWindow {
    /// Hook the sidebar up. Browsing starts with `start_browsing`.
    pub fn setup_browse(&self) {
        let imp = self.imp();
        imp.sidebar_list.connect_row_activated(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, row| {
                if let Some(root) = Root::from_name(&row.widget_name()) {
                    window.show_root(root);
                }
            }
        ));
        // Back on a sidebar page that went stale while another page was
        // on top of it: build it again.
        imp.nav_view.connect_visible_page_notify(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |view| {
                let Some(page) = view.visible_page() else { return };
                let Some(root) = page.tag().as_deref().and_then(Root::from_name) else { return };
                if window.imp().roots.borrow().get(&root) != Some(&page) {
                    glib::idle_add_local_once(glib::clone!(
                        #[weak]
                        window,
                        #[weak]
                        page,
                        move || {
                            // Only if it is still what the user sees.
                            if window.imp().nav_view.visible_page().as_ref() == Some(&page) {
                                window.show_root(root);
                            }
                        }
                    ));
                }
            }
        ));
    }

    /// Signed in: Home is the start page.
    pub fn start_browsing(&self) {
        self.imp().roots.borrow_mut().clear();
        self.show_root(Root::Home);
        self.load_favorites();
    }

    /// What `root` lists changed: it is built again when next shown, at
    /// once if it is showing.
    pub fn reload_root_when_shown(&self, root: Root) {
        let imp = self.imp();
        let Some(old) = imp.roots.borrow_mut().remove(&root) else { return };
        if imp.nav_view.visible_page().as_ref() == Some(&old) {
            self.show_root(root);
        }
    }

    /// Signed out: drop every page (they belong to the old session).
    pub fn stop_browsing(&self) {
        let imp = self.imp();
        imp.roots.borrow_mut().clear();
        let placeholder = adw::NavigationPage::builder().title("Zeke").child(&adw::Bin::new()).build();
        imp.nav_view.replace(&[placeholder]);
    }

    pub fn show_root(&self, root: Root) {
        let imp = self.imp();
        let page = imp.roots.borrow().get(&root).cloned();
        let page = match page {
            Some(p) => p,
            None => {
                let p: BrowsePage = match root {
                    Root::Home => home::page(self),
                    Root::Search => search::page(self),
                    Root::FavoriteTracks => detail::favorite_tracks(self),
                    Root::FavoriteAlbums => favorites::albums(self),
                    Root::FavoriteArtists => favorites::artists(self),
                    Root::FavoritePlaylists => favorites::playlists(self),
                };
                let p = p.upcast::<adw::NavigationPage>();
                p.set_tag(Some(root.name()));
                imp.roots.borrow_mut().insert(root, p.clone());
                p
            }
        };
        imp.nav_view.replace(&[page]);
        let mut child = imp.sidebar_list.first_child();
        while let Some(w) = child {
            if let Some(row) = w.downcast_ref::<gtk::ListBoxRow>()
                && row.widget_name() == root.name() {
                    imp.sidebar_list.select_row(Some(row));
                }
            child = w.next_sibling();
        }
        imp.split_view.set_show_content(true);
    }

    pub fn open(&self, target: Target) {
        let page: BrowsePage = match target {
            Target::Album(id) => detail::album(self, id),
            Target::Playlist(uuid) => detail::playlist(self, uuid),
            Target::Mix { id, title, kind } => detail::mix(self, id, title, kind),
            Target::Artist(id) => artist::page(self, id),
            Target::ViewAll { title, api_path } => home::view_all(self, title, api_path),
            Target::ArtistViewAll { title, artist, path, tracks } => artist::view_all(self, title, artist, path, tracks),
        };
        self.imp().nav_view.push(&page);
    }

    /// A card was activated in `store` (a store of `CardObject`s).
    pub fn open_card(&self, store: &gio::ListStore, position: u32) {
        let Some(card) = store.item(position).and_downcast::<CardObject>() else { return };
        match &card.data().kind {
            CardKind::Album(id) => self.open(Target::Album(*id)),
            CardKind::Playlist(uuid) => self.open(Target::Playlist(uuid.clone())),
            CardKind::Mix(id) => self.open(Target::Mix { id: id.clone(), title: None, kind: None }),
            CardKind::Artist(id) => self.open(Target::Artist(*id)),
            CardKind::MyTracks => self.show_root(Root::FavoriteTracks),
            CardKind::Track(_) => {
                // The row's tracks become the queue.
                let cards: Vec<CardObject> = store.iter::<CardObject>().flatten().collect();
                let kinds: Vec<&CardKind> = cards.iter().map(|c| &c.data().kind).collect();
                let (tracks, start) = row_queue(&kinds, position as usize);
                self.play_tracks(&tracks, start, false, None);
            }
        }
    }

    /// Replace the queue with `tracks` and play `start` (with shuffle and
    /// no start, a random one). `album_mode`: one album in order, so album
    /// ReplayGain. `shuffle: None` keeps the shuffle toggle as it is.
    /// Returns the queue's generation, for `append_to_queue`.
    pub fn play_tracks(&self, tracks: &[TrackData], start: Option<usize>, album_mode: bool, shuffle: Option<bool>) -> u64 {
        let generation = &self.imp().queue_generation;
        if tracks.is_empty() {
            return generation.get();
        }
        generation.set(generation.get() + 1);
        let shuffle = shuffle.unwrap_or_else(|| {
            self.lookup_action("shuffle").and_then(|a| a.state()).and_then(|s| s.get::<bool>()).unwrap_or(false)
        });
        log::info!(
            "[app] playing {} tracks from a page (start {start:?}, album gain {album_mode}, shuffle {shuffle})",
            tracks.len()
        );
        self.send(PlayerCommand::Load {
            tracks: tracks.iter().map(TrackData::queue_track).collect(),
            start,
            album_mode,
            shuffle,
            repeat: self.imp().repeat.get(),
        });
        generation.get()
    }

    /// Append the rest of a page to the queue `play_tracks` started, unless
    /// another queue has replaced it since (then `false`).
    pub fn append_to_queue(&self, generation: u64, tracks: &[TrackData]) -> bool {
        if self.imp().queue_generation.get() != generation {
            return false;
        }
        if !tracks.is_empty() {
            self.send(PlayerCommand::Append(tracks.iter().map(TrackData::queue_track).collect()));
        }
        true
    }

    pub fn play_next(&self, track: &TrackData) {
        self.send(PlayerCommand::PlayNext(track.queue_track()));
        self.toast(&format!("“{}” plays next", track.title));
    }

    pub fn add_to_queue(&self, track: &TrackData) {
        self.send(PlayerCommand::Append(vec![track.queue_track()]));
        self.toast(&format!("Added “{}” to the queue", track.title));
    }

    /// Open the track's radio: at once when its mix id is known, else once
    /// TIDAL has told it. One lookup at a time; activations while one is
    /// out are ignored.
    pub fn open_track_radio(&self, track: &TrackData) {
        let title = format!("{} Radio", track.title);
        if let Some(id) = &track.track_mix_id {
            self.open(Target::Mix { id: id.clone(), title: Some(title), kind: Some("Track Radio") });
            return;
        }
        if self.imp().radio_lookup.replace(true) {
            return;
        }
        let state = Arc::clone(&self.session().state);
        let track_id = track.id;
        let window = self.downgrade();
        runtime::spawn(async move { browse::track_mix_id(&state, track_id).await }, move |result| {
            let Some(window) = window.upgrade() else { return };
            window.imp().radio_lookup.set(false);
            match result {
                Ok(Some(id)) => window.open(Target::Mix { id, title: Some(title), kind: Some("Track Radio") }),
                Ok(None) => {
                    log::info!("[browse] track {track_id} has no radio");
                    window.toast("No radio for this track");
                }
                Err(e) => {
                    window.report("find the track’s radio", &e);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_card_plays_its_rows_tracks_from_itself() {
        let track = |id| CardKind::Track(Box::new(TrackData { id, ..TrackData::default() }));
        let row = [CardKind::Album(1), track(10), CardKind::Mix("m".into()), track(20), track(30)];
        let kinds: Vec<&CardKind> = row.iter().collect();
        let (tracks, start) = row_queue(&kinds, 3);
        assert_eq!(tracks.iter().map(|t| t.id).collect::<Vec<_>>(), [10, 20, 30]);
        assert_eq!(start, Some(1), "the second track card");
        assert_eq!(row_queue(&kinds, 0).1, None, "not a track card");
    }
}
