//! Artist page: picture and name, Play and
//! Shuffle for the top tracks, then the page's sections: top tracks,
//! albums, EPs and singles, and the rest (playlists, similar artists,
//! appears on) as card rows. Videos and credits are left out.

use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use zeke_tidal::commands::browse::{self, ArtistPage, Favorite};

use super::model::{CardData, TrackData};
use super::views::{self, TrackStyle};
use super::{clamped, detail, favorites, BrowsePage, Heading, Shell, Target};
use crate::covers::{self, Kind};
use crate::runtime;
use crate::window::ZekeWindow;

/// Top tracks shown on the page itself; "View All" has the rest.
const TOP_TRACKS: usize = 10;

pub fn page(window: &ZekeWindow, artist: u64) -> BrowsePage {
    let (shell, page) = Shell::new("Artist");
    page.keep(Rc::clone(&shell));
    load(window, &shell, artist);
    page
}

fn load(window: &ZekeWindow, shell: &Rc<Shell>, artist: u64) {
    shell.loading();
    let state = Arc::clone(&window.session().state);
    let (w, s) = (window.downgrade(), Rc::downgrade(shell));
    runtime::spawn(async move { browse::artist_page(&state, artist).await }, move |result| {
        let (Some(window), Some(shell)) = (w.upgrade(), s.upgrade()) else { return };
        match result {
            Ok(page) => show(&window, &shell, artist, &page),
            Err(e) => {
                let (w, s) = (window.downgrade(), Rc::downgrade(&shell));
                shell.failed(&window, "the artist", &e, move || {
                    if let (Some(window), Some(shell)) = (w.upgrade(), s.upgrade()) {
                        load(&window, &shell, artist);
                    }
                });
            }
        }
    });
}

fn show(window: &ZekeWindow, shell: &Rc<Shell>, artist: u64, page: &ArtistPage) {
    let heading = Heading::new("Artist", true);
    heading.title.set_label(&page.name);
    let name = page.name.clone();
    crate::hearts::bind_heart(window, &heading.heart, move || Some((Favorite::Artist(artist), name.clone())));
    heading.subtitle.set_visible(false);
    heading.details.set_visible(false);
    // Hero image sources, in order: artwork, picture, then album covers.
    let picture = page
        .artwork_id
        .as_deref()
        .or(page.picture.as_deref())
        .map(|id| covers::url(id, Kind::Artist, covers::CARD))
        .or_else(|| page.album_cover_fallback.as_deref().map(|id| covers::url(id, Kind::Album, covers::CARD)));
    window.covers().show(&heading.picture, picture);

    let top: Vec<TrackData> = page.top_tracks.iter().filter_map(TrackData::from_value).collect();
    heading.play.set_sensitive(!top.is_empty());
    heading.shuffle.set_sensitive(!top.is_empty());
    for (button, shuffle) in [(&heading.play, false), (&heading.shuffle, true)] {
        let (w, tracks) = (window.downgrade(), top.clone());
        button.connect_clicked(move |_| {
            if let Some(window) = w.upgrade() {
                let start = if shuffle { None } else { Some(0) };
                window.play_tracks(&tracks, start, false, Some(shuffle));
            }
        });
    }

    let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(28).css_classes(["browse-page"]).build();
    column.append(&heading.widget);
    for section in &page.sections {
        let tracks = section.section_type == "TRACK_LIST";
        let view_all: Option<Box<dyn Fn()>> = section.view_all.clone().map(|path| {
            let (w, title) = (window.downgrade(), section.title.clone());
            Box::new(move || {
                if let Some(window) = w.upgrade() {
                    window.open(Target::ArtistViewAll { title: title.clone(), artist, path: path.clone(), tracks });
                }
            }) as Box<dyn Fn()>
        });
        let body: gtk::Widget = match section.section_type.as_str() {
            "VIDEO_LIST" => {
                log::info!("[artist] skipping section \"{}\": videos aren't supported", section.title);
                continue;
            }
            "TRACK_LIST" => {
                let list: Vec<TrackData> = section.items.iter().filter_map(TrackData::from_value).take(TOP_TRACKS).collect();
                if list.is_empty() {
                    continue;
                }
                let store = views::track_store(list);
                let (w, s) = (window.downgrade(), store.clone());
                views::track_list(window, &store, TrackStyle::Mixed, move |position| {
                    if let Some(window) = w.upgrade() {
                        window.play_tracks(&views::tracks_of(&s), Some(position as usize), false, None);
                    }
                })
                .upcast()
            }
            kind @ ("ALBUM_LIST" | "ARTIST_LIST" | "PLAYLIST_LIST" | "MIX_LIST") => {
                let cards: Vec<CardData> = section.items.iter().filter_map(|v| CardData::from_value(v, Some(kind))).collect();
                if cards.is_empty() {
                    continue;
                }
                views::card_row(window, &views::card_store(cards)).upcast()
            }
            // As on Home: a type Zeke doesn't know is skipped, not guessed.
            other => {
                log::info!("[artist] skipping section \"{}\": unknown type {other}", section.title);
                continue;
            }
        };
        column.append(&views::section(&section.title, view_all, &body));
    }
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamped(&column)).build();
    shell.show(&scroller);
}

/// An artist section's "view all": tracks, or a card grid.
pub fn view_all(window: &ZekeWindow, title: String, artist: u64, path: String, tracks: bool) -> BrowsePage {
    if tracks {
        detail::artist_tracks(window, &title, artist, path)
    } else {
        favorites::artist_cards(window, &title, artist, path)
    }
}
