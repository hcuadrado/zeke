//! Track pages: album, playlist, mix, favorite tracks, and an artist
//! section's tracks ("view all"). A heading with Play and Shuffle over a track list;
//! a row plays the page from that track.
//!
//! Long lists (playlists, favorites) arrive 100 tracks per request, one
//! request per turn at the TIDAL client, until all are in. A queue started
//! before then waits for the rest, so it always holds the whole page.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use adw::prelude::*;
use gtk::gio;
use zeke_tidal::commands::browse::{self, Favorite, TRACK_PAGE};
use zeke_tidal::{AppState, TidalError};

use super::model::{Image, TrackData};
use super::views::{self, TrackStyle};
use super::{clamped, BrowsePage, Heading, Shell};
use crate::covers::{self, Kind};
use crate::runtime;
use crate::window::ZekeWindow;

/// Where a track page's tracks come from.
#[derive(Debug, Clone)]
pub enum Source {
    Album(u64),
    Playlist(String),
    Mix(String),
    Favorites,
    ArtistViewAll { artist: u64, path: String },
}

/// What the page shows above its tracks.
#[derive(Debug, Clone, Default)]
struct Head {
    title: String,
    subtitle: String,
    details: String,
    image: Option<Image>,
    /// A playlist's creator (the user's own playlists get no heart).
    creator: Option<u64>,
}

/// One load: the heading (first page only), tracks, the total when TIDAL
/// says it, and where the next page starts.
#[derive(Debug, Default)]
struct Batch {
    head: Option<Head>,
    tracks: Vec<TrackData>,
    total: Option<u32>,
    next: Option<u32>,
}

/// Artist view-all pages come in fifties.
const VIEW_ALL_PAGE: u32 = 50;
/// No list is read past this many items (a guard against an endpoint that
/// keeps answering full pages).
pub const MAX_ITEMS: u32 = 10_000;

/// Where the next page starts after one of `got` items at `offset`: with a
/// `total`, until it is reached; without one, while pages come back full
/// (`page` items). An empty page ends the list, and so does `MAX_ITEMS`.
pub fn next_offset(offset: u32, got: u32, total: Option<u32>, page: u32) -> Option<u32> {
    let next = offset + got;
    let more = match total {
        Some(total) => next < total,
        None => got == page,
    };
    (got > 0 && more && next < MAX_ITEMS).then_some(next)
}

fn minutes(secs: u32) -> String {
    let m = (secs + 30) / 60;
    if m >= 60 {
        format!("{} h {} min", m / 60, m % 60)
    } else {
        format!("{m} min")
    }
}

fn count(n: usize) -> String {
    format!("{n} track{}", if n == 1 { "" } else { "s" })
}

async fn fetch(state: Arc<AppState>, source: Source, offset: u32) -> Result<Batch, TidalError> {
    match source {
        Source::Album(id) => {
            let page = browse::album_page(&state, id).await?;
            let a = &page.album;
            let artists = a
                .artists
                .as_ref()
                .map(|l| l.iter().map(|x| x.name.as_str()).collect::<Vec<_>>().join(", "))
                .filter(|s| !s.is_empty())
                .or_else(|| a.artist.as_ref().map(|x| x.name.clone()))
                .unwrap_or_default();
            let tracks: Vec<TrackData> = page.tracks.iter().filter_map(TrackData::from_typed).collect();
            let mut details: Vec<String> = Vec::new();
            if let Some(year) = a.release_date.as_deref().and_then(|d| d.get(..4)) {
                details.push(year.into());
            }
            details.push(count(tracks.len()));
            if let Some(d) = a.duration {
                details.push(minutes(d));
            }
            // Quality badge: the tags first; `audioQuality`
            // lags behind them (hi-res albums still say LOSSLESS there).
            let tags = a.media_metadata.as_ref().map(|m| m.tags.as_slice()).unwrap_or_default();
            let quality = if tags.iter().any(|t| t == "HIRES_LOSSLESS") {
                Some("Hi-Res Lossless")
            } else if tags.iter().any(|t| t == "LOSSLESS") {
                Some("Lossless")
            } else {
                a.audio_quality.as_deref().map(|q| match q {
                    "HI_RES_LOSSLESS" | "HI_RES" => "Hi-Res Lossless",
                    "LOSSLESS" => "Lossless",
                    _ => "High",
                })
            };
            details.extend(quality.map(str::to_string));
            let title = match a.version.as_deref().filter(|v| !v.is_empty()) {
                Some(v) => format!("{} ({v})", a.title),
                None => a.title.clone(),
            };
            let head = Head {
                title,
                subtitle: artists,
                details: details.join(" · "),
                image: a.cover.clone().map(|c| Image::Id(c, Kind::Album)),
                creator: None,
            };
            Ok(Batch { head: Some(head), total: Some(tracks.len() as u32), tracks, next: None })
        }
        Source::Playlist(uuid) => {
            let head = if offset == 0 {
                let p = browse::playlist(&state, &uuid).await?;
                let by = p.creator.as_ref().and_then(|c| c.name.clone()).map(|n| format!("By {n}")).unwrap_or_else(|| {
                    if p.creator.as_ref().and_then(|c| c.id) == Some(0) { "By TIDAL".into() } else { String::new() }
                });
                let mut details = Vec::new();
                if let Some(n) = p.number_of_tracks {
                    details.push(count(n as usize));
                }
                if let Some(d) = p.duration {
                    details.push(minutes(d));
                }
                Some(Head {
                    title: p.title.clone(),
                    subtitle: by,
                    details: details.join(" · "),
                    image: p.image.clone().map(|i| Image::Id(i, Kind::Album)),
                    creator: p.creator.as_ref().and_then(|c| c.id),
                })
            } else {
                None
            };
            let page = browse::playlist_tracks(&state, &uuid, offset).await?;
            let got = page.items.len() as u32;
            let next = next_offset(offset, got, Some(page.total_number_of_items), TRACK_PAGE);
            let tracks = page.items.iter().filter_map(TrackData::from_typed).collect();
            Ok(Batch { head, tracks, total: Some(page.total_number_of_items), next })
        }
        Source::Mix(id) => {
            let mix = browse::mix(&state, &id).await?;
            let tracks: Vec<TrackData> = mix.tracks.iter().filter_map(TrackData::from_typed).collect();
            let head = Head {
                title: mix.title.clone().unwrap_or_else(|| "Mix".into()),
                subtitle: mix.subtitle.clone().unwrap_or_default(),
                details: count(tracks.len()),
                image: mix.image.clone().map(|i| Image::Id(i, Kind::Album)),
                creator: None,
            };
            Ok(Batch { head: Some(head), total: Some(tracks.len() as u32), tracks, next: None })
        }
        Source::Favorites => {
            let page = browse::favorite_tracks(&state, offset).await?;
            let got = page.items.len() as u32;
            let next = next_offset(offset, got, Some(page.total_number_of_items), TRACK_PAGE);
            let head = (offset == 0).then(|| Head {
                title: "Loved Tracks".into(),
                subtitle: "Your favorite tracks".into(),
                details: count(page.total_number_of_items as usize),
                image: None,
                creator: None,
            });
            let tracks = page.items.iter().filter_map(TrackData::from_typed).collect();
            Ok(Batch { head, tracks, total: Some(page.total_number_of_items), next })
        }
        Source::ArtistViewAll { artist, path } => {
            let items = browse::artist_view_all(&state, artist, &path, offset, VIEW_ALL_PAGE).await?;
            let got = items.len() as u32;
            let next = next_offset(offset, got, None, VIEW_ALL_PAGE);
            Ok(Batch { head: None, tracks: items.iter().filter_map(TrackData::from_value).collect(), total: None, next })
        }
    }
}

/// Play from this page: a row, or the Play / Shuffle buttons.
#[derive(Debug, Clone, Copy)]
struct PlayRequest {
    start: Option<usize>,
    shuffle: Option<bool>,
}

struct TrackPage {
    window: gtk::glib::WeakRef<ZekeWindow>,
    shell: Rc<Shell>,
    heading: Heading,
    /// The heading over the list.
    content: gtk::Box,
    store: gio::ListStore,
    source: Source,
    /// Album ReplayGain for queues from this page.
    album_mode: bool,
    complete: Cell<bool>,
    total: Cell<Option<u32>>,
    /// A queue played from this page before all of it loaded: its
    /// generation, and how many of the page's tracks it has. The rest is
    /// appended as it arrives.
    feeding: Cell<Option<(u64, usize)>>,
    /// Bumped by each (re)load; results of an older one are dropped.
    generation: Cell<u64>,
    /// When the first page was asked for, for the load-time log.
    started: Cell<Option<std::time::Instant>>,
    /// What the heading's heart adds to the favorites, once known, and a
    /// playlist's creator.
    favorite: RefCell<Option<(Favorite, String, Option<u64>)>>,
    redraw_heart: RefCell<Option<Rc<dyn Fn()>>>,
}

impl TrackPage {
    fn new(window: &ZekeWindow, title: &str, kind: &str, source: Source, style: TrackStyle) -> (Rc<Self>, BrowsePage) {
        let (shell, page) = Shell::new(title);
        let heading = Heading::new(kind, false);
        let store = gio::ListStore::new::<super::model::TrackObject>();
        let album_mode = matches!(source, Source::Album(_));
        let this = Rc::new(Self {
            window: window.downgrade(),
            shell,
            heading,
            store,
            source,
            album_mode,
            content: gtk::Box::builder().orientation(gtk::Orientation::Vertical).build(),
            complete: Cell::new(false),
            total: Cell::new(None),
            feeding: Cell::new(None),
            generation: Cell::new(0),
            started: Cell::new(None),
            favorite: RefCell::default(),
            redraw_heart: RefCell::default(),
        });
        let weak = Rc::downgrade(&this);
        // Asked again when the favorites load, which is when the user (and
        // so their own playlists, which get no heart) is known.
        let redraw = crate::hearts::bind_heart(window, &this.heading.heart, move || {
            let this = weak.upgrade()?;
            let (item, name, creator) = this.favorite.borrow().clone()?;
            let own = matches!(item, Favorite::Playlist(_)) && this.window()?.owns_playlist(creator);
            (!own).then_some((item, name))
        });
        this.redraw_heart.replace(Some(redraw));
        let weak = Rc::downgrade(&this);
        let list = views::track_list(window, &this.store, style, move |position| {
            if let Some(this) = weak.upgrade() {
                this.play(PlayRequest { start: Some(position as usize), shuffle: None });
            }
        });
        let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&list).build();
        this.content.append(&clamped(&this.heading.widget));
        this.content.append(&scroller);
        for (button, shuffle) in [(&this.heading.play, false), (&this.heading.shuffle, true)] {
            let weak = Rc::downgrade(&this);
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.play(PlayRequest { start: if shuffle { None } else { Some(0) }, shuffle: Some(shuffle) });
                }
            });
        }
        page.keep(Rc::clone(&this));
        (this, page)
    }

    fn window(&self) -> Option<ZekeWindow> {
        self.window.upgrade()
    }

    fn reload(self: &Rc<Self>) {
        self.generation.set(self.generation.get() + 1);
        self.complete.set(false);
        self.feeding.take();
        self.store.remove_all();
        self.started.set(Some(std::time::Instant::now()));
        self.shell.loading();
        self.load(0);
    }

    fn load(self: &Rc<Self>, offset: u32) {
        let Some(window) = self.window() else { return };
        let generation = self.generation.get();
        let state = Arc::clone(&window.session().state);
        let weak: Weak<Self> = Rc::downgrade(self);
        let source = self.source.clone();
        runtime::spawn(fetch(state, source, offset), move |result| {
            let Some(this) = weak.upgrade() else { return };
            if this.generation.get() != generation {
                return;
            }
            match result {
                Ok(batch) => this.loaded(offset, batch),
                Err(e) => {
                    let Some(window) = this.window() else { return };
                    if offset == 0 {
                        let retry = Rc::downgrade(&this);
                        this.shell.failed(&window, "this page", &e, move || {
                            if let Some(this) = retry.upgrade() {
                                this.reload();
                            }
                        });
                    } else {
                        // Keep what loaded; a queue can't be the whole page now.
                        window.report("load the rest of the tracks", &e);
                        this.finish();
                    }
                }
            }
        });
    }

    fn loaded(self: &Rc<Self>, offset: u32, batch: Batch) {
        if let Some(head) = &batch.head {
            self.show_head(head);
        }
        if batch.total.is_some() {
            self.total.set(batch.total);
        }
        let items: Vec<super::model::TrackObject> = batch.tracks.into_iter().map(super::model::TrackObject::new).collect();
        self.store.extend_from_slice(&items);
        // Playable from the first page on; a queue started now gets the
        // rest as it loads.
        let has_tracks = self.store.n_items() > 0;
        self.heading.play.set_sensitive(has_tracks);
        self.heading.shuffle.set_sensitive(has_tracks);
        self.feed();
        if offset == 0 {
            if self.store.n_items() == 0 && batch.next.is_none() {
                self.shell.empty("No Tracks", "There is nothing here Zeke can play.");
            } else {
                self.shell.show(&self.content);
            }
        }
        match batch.next {
            Some(next) => {
                self.show_progress();
                self.load(next);
            }
            None => self.finish(),
        }
    }

    fn show_head(&self, head: &Head) {
        let Some(window) = self.window() else { return };
        let favorite = match &self.source {
            Source::Album(id) => Some(Favorite::Album(*id)),
            Source::Playlist(uuid) => Some(Favorite::Playlist(uuid.clone())),
            _ => None,
        };
        self.favorite.replace(favorite.map(|f| (f, head.title.clone(), head.creator)));
        if let Some(redraw) = self.redraw_heart.borrow().as_ref() {
            redraw();
        }
        self.heading.title.set_label(&head.title);
        self.heading.subtitle.set_label(&head.subtitle);
        self.heading.subtitle.set_visible(!head.subtitle.is_empty());
        self.heading.details.set_label(&head.details);
        match head.image.as_ref().and_then(|i| i.url(covers::CARD)) {
            Some(url) => window.covers().show(&self.heading.picture, Some(url)),
            None => {
                self.heading.picture.add_css_class("loved-tracks");
                self.heading.picture.set_paintable(gtk::gdk::Paintable::NONE);
            }
        }
    }

    fn show_progress(&self) {
        if let Some(total) = self.total.get() {
            self.heading.play.set_tooltip_text(Some(&format!("Loading tracks: {} of {total}", self.store.n_items())));
        }
    }

    fn finish(&self) {
        self.complete.set(true);
        self.heading.play.set_tooltip_text(None);
        let has_tracks = self.store.n_items() > 0;
        self.heading.play.set_sensitive(has_tracks);
        self.heading.shuffle.set_sensitive(has_tracks);
        if let Some(started) = self.started.take() {
            log::info!("[browse] {:?}: {} tracks in {:.2} s", self.source_name(), self.store.n_items(), started.elapsed().as_secs_f64());
        }
        self.feed();
        self.feeding.take();
    }

    /// Append what loaded since to a queue played from this page early,
    /// while it is still the player's queue.
    fn feed(&self) {
        let (Some((generation, sent)), Some(window)) = (self.feeding.get(), self.window()) else { return };
        let tracks = views::tracks_of(&self.store);
        let fed = window.append_to_queue(generation, tracks.get(sent..).unwrap_or_default());
        self.feeding.set(fed.then_some((generation, tracks.len())));
    }

    fn source_name(&self) -> String {
        match &self.source {
            Source::Album(id) => format!("album {id}"),
            Source::Playlist(uuid) => format!("playlist {uuid}"),
            Source::Mix(id) => format!("mix {id}"),
            Source::Favorites => "favorite tracks".into(),
            Source::ArtistViewAll { path, .. } => path.clone(),
        }
    }

    /// Play what has loaded; while the page is still loading, the rest
    /// joins the queue as it comes (shuffled in, when shuffling).
    fn play(&self, request: PlayRequest) {
        let Some(window) = self.window() else { return };
        let tracks = views::tracks_of(&self.store);
        if tracks.is_empty() {
            return;
        }
        let generation = window.play_tracks(&tracks, request.start, self.album_mode, request.shuffle);
        self.feeding.set((!self.complete.get()).then_some((generation, tracks.len())));
    }
}

fn open(window: &ZekeWindow, title: &str, kind: &str, source: Source, style: TrackStyle) -> BrowsePage {
    let (this, page) = TrackPage::new(window, title, kind, source, style);
    this.reload();
    page
}

pub fn album(window: &ZekeWindow, id: u64) -> BrowsePage {
    open(window, "Album", "Album", Source::Album(id), TrackStyle::Album)
}

pub fn playlist(window: &ZekeWindow, uuid: String) -> BrowsePage {
    open(window, "Playlist", "Playlist", Source::Playlist(uuid), TrackStyle::Mixed)
}

pub fn mix(window: &ZekeWindow, id: String) -> BrowsePage {
    open(window, "Mix", "Mix", Source::Mix(id), TrackStyle::Mixed)
}

pub fn favorite_tracks(window: &ZekeWindow) -> BrowsePage {
    open(window, "Tracks", "Favorites", Source::Favorites, TrackStyle::Mixed)
}

/// An artist section's tracks, from its v2 "view all".
pub fn artist_tracks(window: &ZekeWindow, title: &str, artist: u64, path: String) -> BrowsePage {
    let (this, page) = TrackPage::new(window, title, "", Source::ArtistViewAll { artist, path }, TrackStyle::Mixed);
    this.heading.picture.set_visible(false);
    this.heading.title.set_label(title);
    this.reload();
    page
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue_row::format_time;

    #[test]
    fn pages_follow_on_until_the_list_ends() {
        // With a total: the playlist of 2,033 in pages of 100.
        assert_eq!(next_offset(0, 100, Some(2033), 100), Some(100));
        assert_eq!(next_offset(2000, 33, Some(2033), 100), None, "the short last page");
        assert_eq!(next_offset(1900, 100, Some(2000), 100), None, "the total, reached exactly");
        // Without one: while pages are full.
        assert_eq!(next_offset(0, 50, None, 50), Some(50));
        assert_eq!(next_offset(50, 49, None, 50), None);
        assert_eq!(next_offset(0, 0, Some(10), 50), None, "an empty page ends it");
        assert_eq!(next_offset(MAX_ITEMS - 50, 50, None, 50), None, "the guard");
    }

    #[test]
    fn lengths_read_naturally() {
        assert_eq!(minutes(2580), "43 min");
        assert_eq!(minutes(10467), "2 h 54 min");
        assert_eq!(count(1), "1 track");
        assert_eq!(count(12), "12 tracks");
        assert_eq!(format_time(61.0), "1:01");
    }
}
