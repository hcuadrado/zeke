//! Home: TIDAL's personalized feed (`home/feed/static`), the start page.
//! Sections as card rows or track
//! lists (`model::classify`), a refresh button, more sections at the
//! bottom, and each section's "view all". The feed comes from the 4 h disk
//! cache when it can (`commands::pages`).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use adw::prelude::*;
use serde_json::Value;
use zeke_tidal::commands::pages;
use zeke_tidal::TidalError;
use zeke_tidal::tidal_api::HomePageSection;

use super::model::{classify, type_hint, CardData, SectionView, TrackData};
use super::views::{self, TrackStyle};
use super::{clamped, BrowsePage, Shell, Target};
use crate::runtime;
use crate::session::describe;
use crate::window::ZekeWindow;

struct Home {
    window: gtk::glib::WeakRef<ZekeWindow>,
    shell: Rc<Shell>,
    content: gtk::ScrolledWindow,
    sections: gtk::Box,
    more: gtk::Button,
    more_spinner: adw::Spinner,
    cursor: RefCell<Option<String>>,
    /// More sections were appended: a background refresh no longer replaces them.
    paginated: Cell<bool>,
    loading_more: Cell<bool>,
    /// Bumped by each full (re)load; older results are dropped.
    generation: Cell<u64>,
}

pub fn page(window: &ZekeWindow) -> BrowsePage {
    let (shell, page) = Shell::new("Home");
    let refresh = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Refresh").build();
    shell.header.pack_end(&refresh);

    let sections = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(28).build();
    let more = gtk::Button::builder().label("Load More").halign(gtk::Align::Center).css_classes(["pill"]).visible(false).build();
    let more_spinner = adw::Spinner::builder().halign(gtk::Align::Center).visible(false).build();
    let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(28).css_classes(["browse-page"]).build();
    column.append(&sections);
    column.append(&more);
    column.append(&more_spinner);
    let content = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamped(&column)).build();

    let home = Rc::new(Home {
        window: window.downgrade(),
        shell,
        content,
        sections,
        more,
        more_spinner,
        cursor: RefCell::default(),
        paginated: Cell::new(false),
        loading_more: Cell::new(false),
        generation: Cell::new(0),
    });
    let weak = Rc::downgrade(&home);
    refresh.connect_clicked(move |_| {
        if let Some(home) = weak.upgrade() {
            home.load(true);
        }
    });
    let weak = Rc::downgrade(&home);
    home.more.connect_clicked(move |_| {
        if let Some(home) = weak.upgrade() {
            home.load_more();
        }
    });
    // Near the bottom, the next sections load by themselves (infinite
    // scroll).
    let weak = Rc::downgrade(&home);
    home.content.connect_edge_reached(move |_, edge| {
        if edge == gtk::PositionType::Bottom
            && let Some(home) = weak.upgrade() {
                home.load_more();
            }
    });
    home.load(false);
    page.keep(Rc::clone(&home));
    page
}

impl Home {
    /// The feed from the cache (or TIDAL); `refresh` always asks TIDAL.
    fn load(self: &Rc<Self>, refresh: bool) {
        let Some(window) = self.window.upgrade() else { return };
        self.generation.set(self.generation.get() + 1);
        let generation = self.generation.get();
        // No more of the shown feed while its successor is on the way (it
        // comes back if the reload fails and the feed stays).
        let previous = self.cursor.take();
        self.more.set_visible(false);
        if self.sections.first_child().is_none() {
            self.shell.loading();
        }
        let state = Arc::clone(&window.session().state);
        let weak = Rc::downgrade(self);
        let started = std::time::Instant::now();
        runtime::spawn(
            async move {
                if refresh {
                    pages::refresh_home_page(&state, None).await.map(|home| (home, false))
                } else {
                    pages::get_cached_home_page(&state, None).await.map(|c| (c.home, c.is_stale))
                }
            },
            move |result| {
                let Some(home) = weak.upgrade() else { return };
                if home.generation.get() != generation {
                    return;
                }
                match result {
                    // An empty feed is a failed answer (`pages` never caches
                    // one either): keep what is on screen.
                    Ok((feed, _)) if feed.sections.is_empty() => {
                        home.set_cursor(previous);
                        log::warn!("[home] TIDAL sent an empty feed");
                        home.failed(None);
                    }
                    Ok((feed, stale)) => {
                        log::info!("[home] feed in {:.2} s ({} sections)", started.elapsed().as_secs_f64(), feed.sections.len());
                        home.paginated.set(false);
                        home.clear();
                        home.append(&feed.sections);
                        home.set_cursor(feed.cursor);
                        if refresh
                            && let Some(w) = home.window.upgrade() {
                                w.toast("Home refreshed");
                            }
                        // A stale feed is shown first, then the fresh one.
                        if stale {
                            home.refresh_in_background();
                        }
                    }
                    Err(e) => {
                        home.set_cursor(previous);
                        home.failed(Some(&e));
                    }
                }
            },
        );
    }

    fn refresh_in_background(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else { return };
        let state = Arc::clone(&window.session().state);
        let weak = Rc::downgrade(self);
        let generation = self.generation.get();
        runtime::spawn(async move { pages::refresh_home_page(&state, None).await }, move |result| {
            let Some(home) = weak.upgrade() else { return };
            if home.generation.get() != generation || home.paginated.get() {
                return;
            }
            match result {
                Ok(feed) if feed.sections.is_empty() => log::warn!("[home] background refresh: empty feed; keeping the shown one"),
                Ok(feed) => {
                    home.clear();
                    home.append(&feed.sections);
                    home.set_cursor(feed.cursor);
                }
                Err(e) => log::warn!("[home] background refresh failed: {}", describe(&e)),
            }
        });
    }

    /// A load failed (`None`: TIDAL sent an empty feed). A feed on screen
    /// stays; otherwise the page shows the error and a retry button.
    fn failed(self: &Rc<Self>, error: Option<&TidalError>) {
        let Some(window) = self.window.upgrade() else { return };
        let what = if self.sections.first_child().is_some() { "refresh Home" } else { "load Home" };
        let message = match error {
            Some(e) => match window.report(what, e) {
                Some(m) => m,
                None => return, // back to the login page
            },
            None => {
                let m = "TIDAL sent an empty feed.";
                window.toast(&format!("Couldn’t {what}. {m}"));
                m
            }
        };
        if self.sections.first_child().is_some() {
            return; // keep the feed on screen
        }
        let weak: Weak<Self> = Rc::downgrade(self);
        self.shell.failed_with(message, move || {
            if let Some(home) = weak.upgrade() {
                home.load(false);
            }
        });
    }

    fn load_more(self: &Rc<Self>) {
        let Some(cursor) = self.cursor.borrow().clone() else { return };
        if self.loading_more.replace(true) {
            return;
        }
        let Some(window) = self.window.upgrade() else { return };
        self.more.set_visible(false);
        self.more_spinner.set_visible(true);
        let state = Arc::clone(&window.session().state);
        let weak = Rc::downgrade(self);
        let generation = self.generation.get();
        runtime::spawn(async move { pages::get_home_page_more(&state, None, cursor).await }, move |result| {
            let Some(home) = weak.upgrade() else { return };
            home.loading_more.set(false);
            home.more_spinner.set_visible(false);
            if home.generation.get() != generation {
                return;
            }
            match result {
                Ok(more) => {
                    home.paginated.set(true);
                    home.append(&more.sections);
                    home.set_cursor(more.cursor);
                }
                Err(e) => {
                    // The button stays for another try; the failed cursor is
                    // remembered so scrolling doesn't retry in a loop.
                    home.more.set_visible(true);
                    if let Some(w) = home.window.upgrade() {
                        w.report("load more of Home", &e);
                    }
                }
            }
        });
    }

    fn set_cursor(&self, cursor: Option<String>) {
        self.more.set_visible(cursor.is_some());
        self.cursor.replace(cursor);
    }

    fn clear(&self) {
        while let Some(child) = self.sections.first_child() {
            self.sections.remove(&child);
        }
    }

    fn append(&self, sections: &[HomePageSection]) {
        let Some(window) = self.window.upgrade() else { return };
        let mut shown = Vec::new();
        for section in sections {
            match build_section(&window, section) {
                Ok((widget, view)) => {
                    shown.push(format!("\"{}\" ({} → {view:?})", section.title, section.section_type));
                    self.sections.append(&widget);
                }
                Err(reason) => {
                    log::info!("[home] skipping section \"{}\" ({}): {reason}", section.title, section.section_type);
                }
            }
        }
        log::info!("[home] showing {} sections: {}", shown.len(), shown.join(", "));
        if self.sections.first_child().is_some() {
            self.shell.show(&self.content);
        } else {
            self.shell.empty("Nothing Here Yet", "TIDAL sent no sections Zeke can show.");
        }
    }
}

/// A section's widget, or why it is skipped.
fn build_section(window: &ZekeWindow, section: &HomePageSection) -> Result<(gtk::Box, SectionView), String> {
    let items: &[Value] = section.items.as_array().map(Vec::as_slice).unwrap_or_default();
    if items.is_empty() {
        return Err("no items".into());
    }
    let view = classify(&section.section_type, &section.title, items)?;
    let view_all: Option<Box<dyn Fn()>> = match (&section.api_path, section.has_more) {
        (Some(path), true) => {
            let (w, title, path) = (window.downgrade(), section.title.clone(), path.clone());
            Some(Box::new(move || {
                if let Some(window) = w.upgrade() {
                    window.open(Target::ViewAll { title: title.clone(), api_path: path.clone() });
                }
            }))
        }
        _ => None,
    };
    let body: gtk::Widget = match view {
        SectionView::Tracks => {
            let tracks: Vec<TrackData> = items.iter().filter_map(TrackData::from_value).collect();
            if tracks.is_empty() {
                return Err("no playable tracks".into());
            }
            track_section(window, tracks).upcast()
        }
        SectionView::Cards => {
            let hint = type_hint(&section.section_type, items);
            let cards: Vec<CardData> = items.iter().filter_map(|v| CardData::from_value(v, hint)).collect();
            if cards.is_empty() {
                return Err("no items Zeke can open".into());
            }
            views::card_row(window, &views::card_store(cards)).upcast()
        }
    };
    Ok((views::section(&section.title, view_all, &body), view))
}

/// A section's track list; a row plays the section from there.
fn track_section(window: &ZekeWindow, tracks: Vec<TrackData>) -> gtk::ListView {
    let store = views::track_store(tracks);
    let (w, s) = (window.downgrade(), store.clone());
    views::track_list(window, &store, TrackStyle::Mixed, move |position| {
        if let Some(window) = w.upgrade() {
            window.play_tracks(&views::tracks_of(&s), Some(position as usize), false, None);
        }
    })
}

/// A section's "view all": tracks as a track
/// page, anything else as a card grid.
pub fn view_all(window: &ZekeWindow, title: String, api_path: String) -> BrowsePage {
    let (shell, page) = Shell::new(&title);
    shell.loading();
    let state = Arc::clone(&window.session().state);
    let (w, weak_page) = (window.downgrade(), page.downgrade());
    let shell_ref = Rc::downgrade(&shell);
    page.keep(Rc::clone(&shell));
    runtime::spawn(async move { pages::get_page_section(&state, api_path).await }, move |result| {
        let (Some(window), Some(shell), Some(_page)) = (w.upgrade(), shell_ref.upgrade(), weak_page.upgrade()) else {
            return;
        };
        let feed = match result {
            Ok(feed) => feed,
            Err(e) => {
                if let Some(message) = window.report(&format!("load “{title}”"), &e) {
                    shell.empty("Couldn’t Load This Page", message);
                }
                return;
            }
        };
        let items: Vec<Value> =
            feed.sections.iter().flat_map(|s| s.items.as_array().cloned().unwrap_or_default()).collect();
        let section_type = feed.sections.first().map(|s| s.section_type.as_str()).unwrap_or("");
        let all_tracks = classify("COMPACT_GRID_CARD", "", &items) == Ok(SectionView::Tracks)
            || (section_type == "TRACK_LIST" && type_hint(section_type, &items).is_some());
        if all_tracks {
            let tracks: Vec<TrackData> = items.iter().filter_map(TrackData::from_value).collect();
            let store = views::track_store(tracks);
            let (w2, s) = (window.downgrade(), store.clone());
            let list = views::track_list(&window, &store, TrackStyle::Mixed, move |position| {
                if let Some(window) = w2.upgrade() {
                    window.play_tracks(&views::tracks_of(&s), Some(position as usize), false, None);
                }
            });
            shell.show(&gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&list).build());
        } else {
            let hint = type_hint(section_type, &items);
            let cards: Vec<CardData> = items.iter().filter_map(|v| CardData::from_value(v, hint)).collect();
            if cards.is_empty() {
                shell.empty("Nothing Here", "This section has nothing Zeke can open.");
                return;
            }
            let grid = views::card_grid(&window, &views::card_store(cards));
            shell.show(&gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&grid).build());
        }
    });
    page
}
