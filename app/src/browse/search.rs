//! Search: a search entry in the header bar, and
//! results as Top Hits, Tracks, Albums, Artists and Playlists.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use serde_json::Value;
use zeke_tidal::commands::browse;
use zeke_tidal::tidal_api::{DirectHitItem, TidalSearchResults};

use super::model::{CardData, TrackData};
use super::views::{self, TrackStyle};
use super::{clamped, BrowsePage, Shell};
use crate::runtime;
use crate::window::ZekeWindow;

/// Results per kind.
const LIMIT: u32 = 20;
/// TIDAL ranks up to ~100 top hits. On one results page, the first dozen.
const TOP_HITS: usize = 12;

struct Search {
    window: gtk::glib::WeakRef<ZekeWindow>,
    shell: Rc<Shell>,
    entry: gtk::SearchEntry,
    generation: Cell<u64>,
    /// The request in flight; a newer query aborts it.
    running: RefCell<Option<tokio::task::AbortHandle>>,
}

pub fn page(window: &ZekeWindow) -> BrowsePage {
    let (shell, page) = Shell::new("Search");
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Tracks, albums, artists, playlists")
        .search_delay(300)
        .hexpand(true)
        .build();
    let title = adw::Clamp::builder().maximum_size(520).child(&entry).build();
    shell.header.set_title_widget(Some(&title));
    shell.empty("Search TIDAL", "Find tracks, albums, artists and playlists.");

    let search = Rc::new(Search {
        window: window.downgrade(),
        shell,
        entry,
        generation: Cell::new(0),
        running: RefCell::default(),
    });
    let weak = Rc::downgrade(&search);
    search.entry.connect_search_changed(move |entry| {
        if let Some(search) = weak.upgrade() {
            search.run(entry.text().trim());
        }
    });
    window.imp().search_entry.set(Some(&search.entry));
    let entry = search.entry.clone();
    page.connect_shown(move |_| {
        entry.grab_focus();
    });
    page.keep(search);
    page
}

impl Drop for Search {
    /// The page is gone (logout): nothing will show the answer.
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            running.abort();
        }
    }
}

impl Search {
    fn run(self: &Rc<Self>, query: &str) {
        self.generation.set(self.generation.get() + 1);
        let generation = self.generation.get();
        // Superseded: cancel its request (and its turn at the TIDAL client).
        if let Some(running) = self.running.take() {
            if !running.is_finished() {
                log::debug!("[search] aborting the superseded search");
            }
            running.abort();
        }
        if query.is_empty() {
            self.shell.empty("Search TIDAL", "Find tracks, albums, artists and playlists.");
            return;
        }
        let Some(window) = self.window.upgrade() else { return };
        self.shell.loading();
        let state = Arc::clone(&window.session().state);
        let (weak, q) = (Rc::downgrade(self), query.to_string());
        let running = runtime::spawn_abortable(async move { browse::search(&state, &q, LIMIT).await }, move |result| {
            let Some(search) = weak.upgrade() else { return };
            if search.generation.get() != generation {
                return; // finished just before it was aborted
            }
            search.running.take();
            let Some(window) = search.window.upgrade() else { return };
            match result {
                Ok(results) => search.show(&window, &results),
                Err(e) => {
                    if let Some(message) = window.report("search", &e) {
                        search.shell.empty("Search Failed", message);
                    }
                }
            }
        });
        self.running.replace(Some(running));
    }

    fn show(&self, window: &ZekeWindow, r: &TidalSearchResults) {
        let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(28).css_classes(["browse-page"]).build();
        let top: Vec<CardData> = r.top_hits.iter().filter_map(top_hit).take(TOP_HITS).collect();
        if !top.is_empty() {
            column.append(&views::section("Top Hits", None, &views::card_row(window, &views::card_store(top))));
        }
        let tracks: Vec<TrackData> = r.tracks.iter().filter_map(TrackData::from_typed).collect();
        if !tracks.is_empty() {
            let store = views::track_store(tracks);
            let (w, s) = (window.downgrade(), store.clone());
            let list = views::track_list(window, &store, TrackStyle::Mixed, move |position| {
                if let Some(window) = w.upgrade() {
                    window.play_tracks(&views::tracks_of(&s), Some(position as usize), false, None);
                }
            });
            column.append(&views::section("Tracks", None, &list));
        }
        for (title, cards) in [
            ("Albums", r.albums.iter().filter_map(|a| CardData::from_typed(a, "ALBUM")).collect::<Vec<_>>()),
            ("Artists", r.artists.iter().filter_map(|a| CardData::from_typed(a, "ARTIST")).collect()),
            ("Playlists", r.playlists.iter().filter_map(|p| CardData::from_typed(p, "PLAYLIST")).collect()),
        ] {
            if !cards.is_empty() {
                column.append(&views::section(title, None, &views::card_row(window, &views::card_store(cards))));
            }
        }
        if column.first_child().is_none() {
            self.shell.empty("No Results", "Try other words.");
            return;
        }
        let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamped(&column)).build();
        self.shell.show(&scroller);
    }
}

/// A top hit as a card: a track hit carries its full track; the rest
/// are typed by `hitType` ("ARTISTS" →
/// "ARTIST"). Video hits are left out.
fn top_hit(hit: &DirectHitItem) -> Option<CardData> {
    let kind = hit.hit_type.trim_end_matches('S');
    if kind == "TRACK"
        && let Some(track) = &hit.track {
            return CardData::from_typed(track, "TRACK");
        }
    if kind == "VIDEO" {
        return None;
    }
    let mut v = serde_json::to_value(hit).ok()?;
    let obj = v.as_object_mut()?;
    obj.insert("_itemType".into(), Value::String(kind.into()));
    // Flat track hits: rebuild the nested album and artist fields.
    if kind == "TRACK" {
        obj.insert("album".into(), serde_json::json!({"id": hit.album_id, "title": hit.album_title, "cover": hit.album_cover}));
        obj.insert("artist".into(), serde_json::json!({"name": hit.artist_name}));
    }
    CardData::from_value(&v, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browse::model::CardKind;

    fn hit(kind: &str, id: u64) -> DirectHitItem {
        DirectHitItem {
            hit_type: kind.into(),
            id: Some(id),
            uuid: None,
            name: None,
            title: None,
            picture: None,
            artwork_id: None,
            selected_album_cover_fallback: None,
            cover: None,
            image: None,
            artist_name: None,
            album_id: None,
            album_title: None,
            album_cover: None,
            duration: None,
            number_of_tracks: None,
            track: None,
            video: None,
        }
    }

    #[test]
    fn top_hits_become_cards() {
        let album = DirectHitItem {
            title: Some("The Dark Side of the Moon".into()),
            cover: Some("ab-cd".into()),
            artist_name: Some("Pink Floyd".into()),
            ..hit("ALBUMS", 55391786)
        };
        let card = top_hit(&album).unwrap();
        assert_eq!(card.kind, CardKind::Album(55391786));
        assert_eq!(card.subtitle, "Pink Floyd");
        let artist = DirectHitItem { name: Some("Pink Floyd".into()), picture: Some("p".into()), ..hit("ARTISTS", 9) };
        assert!(top_hit(&artist).unwrap().is_artist());
        let track = DirectHitItem {
            title: Some("Time".into()),
            duration: Some(413),
            artist_name: Some("Pink Floyd".into()),
            album_id: Some(55391786),
            album_cover: Some("ab-cd".into()),
            ..hit("TRACKS", 55391790)
        };
        let card = top_hit(&track).unwrap();
        assert!(matches!(card.kind, CardKind::Track(ref t) if t.id == 55391790 && t.album_id == Some(55391786)));
        assert!(top_hit(&hit("VIDEOS", 1)).is_none());
    }
}
