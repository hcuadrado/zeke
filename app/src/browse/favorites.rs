//! Card grids: favorite albums and artists, and an artist
//! section's cards ("view all"). Newest first; more load at the bottom.

use std::cell::Cell;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use adw::prelude::*;
use gtk::gio;
use zeke_tidal::commands::browse;
use zeke_tidal::{AppState, TidalError};

use super::model::{CardData, CardObject};
use super::views;
use super::{BrowsePage, Shell};
use crate::runtime;
use crate::window::ZekeWindow;

const PAGE: u32 = 50;

#[derive(Debug, Clone)]
enum Source {
    Albums,
    Artists,
    ArtistViewAll { artist: u64, path: String },
}

/// One page of cards, and where the next starts.
async fn fetch(state: Arc<AppState>, source: Source, offset: u32) -> Result<(Vec<CardData>, Option<u32>), TidalError> {
    let more = |got: u32, total: u32| super::detail::next_offset(offset, got, Some(total), PAGE);
    match source {
        Source::Albums => {
            let page = browse::favorite_albums(&state, offset, PAGE).await?;
            let next = more(page.items.len() as u32, page.total_number_of_items);
            Ok((page.items.iter().filter_map(|a| CardData::from_typed(a, "ALBUM")).collect(), next))
        }
        Source::Artists => {
            let page = browse::favorite_artists(&state, offset, PAGE).await?;
            let next = more(page.items.len() as u32, page.total_number_of_items);
            Ok((page.items.iter().filter_map(|a| CardData::from_typed(a, "ARTIST")).collect(), next))
        }
        Source::ArtistViewAll { artist, path } => {
            let items = browse::artist_view_all(&state, artist, &path, offset, PAGE).await?;
            let next = super::detail::next_offset(offset, items.len() as u32, None, PAGE);
            Ok((items.iter().filter_map(|v| CardData::from_value(v, None)).collect(), next))
        }
    }
}

struct Grid {
    window: gtk::glib::WeakRef<ZekeWindow>,
    shell: Rc<Shell>,
    scroller: gtk::ScrolledWindow,
    store: gio::ListStore,
    source: Source,
    next: Cell<Option<u32>>,
    busy: Cell<bool>,
    empty: (&'static str, &'static str),
}

fn open(window: &ZekeWindow, title: &str, source: Source, empty: (&'static str, &'static str)) -> BrowsePage {
    let (shell, page) = Shell::new(title);
    let store = gio::ListStore::new::<CardObject>();
    let grid = views::card_grid(window, &store);
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&grid).build();
    let this = Rc::new(Grid {
        window: window.downgrade(),
        shell,
        scroller,
        store,
        source,
        next: Cell::new(Some(0)),
        busy: Cell::new(false),
        empty,
    });
    let weak = Rc::downgrade(&this);
    this.scroller.connect_edge_reached(move |_, edge| {
        if edge == gtk::PositionType::Bottom
            && let Some(this) = weak.upgrade() {
                this.load();
            }
    });
    // More pages load when the grid is scrolled to its end; one that
    // doesn't fill the window can't be, so it loads on until it does (the
    // adjustment changes once the new cards are laid out).
    let weak = Rc::downgrade(&this);
    this.scroller.vadjustment().connect_changed(move |adj| {
        if let Some(this) = weak.upgrade()
            && adj.page_size() > 0.0
            && adj.upper() <= adj.page_size()
        {
            this.load();
        }
    });
    this.shell.loading();
    this.load();
    page.keep(Rc::clone(&this));
    page
}

impl Grid {
    fn load(self: &Rc<Self>) {
        let Some(offset) = self.next.get() else { return };
        if self.busy.replace(true) {
            return;
        }
        let Some(window) = self.window.upgrade() else { return };
        let state = Arc::clone(&window.session().state);
        let weak: Weak<Self> = Rc::downgrade(self);
        runtime::spawn(fetch(state, self.source.clone(), offset), move |result| {
            let Some(this) = weak.upgrade() else { return };
            this.busy.set(false);
            let Some(window) = this.window.upgrade() else { return };
            match result {
                Ok((cards, next)) => {
                    let items: Vec<CardObject> = cards.into_iter().map(CardObject::new).collect();
                    this.store.extend_from_slice(&items);
                    this.next.set(next);
                    if this.store.n_items() == 0 {
                        this.shell.empty(this.empty.0, this.empty.1);
                    } else {
                        this.shell.show(&this.scroller);
                    }
                }
                Err(e) if offset == 0 => {
                    let retry = Rc::downgrade(&this);
                    this.shell.failed(&window, "this page", &e, move || {
                        if let Some(this) = retry.upgrade() {
                            this.shell.loading();
                            this.load();
                        }
                    });
                }
                Err(e) => {
                    window.report("load more", &e);
                }
            }
        });
    }
}

pub fn albums(window: &ZekeWindow) -> BrowsePage {
    open(window, "Albums", Source::Albums, ("No Favorite Albums", "Albums you add to your collection in TIDAL show up here."))
}

pub fn artists(window: &ZekeWindow) -> BrowsePage {
    open(window, "Artists", Source::Artists, ("No Favorite Artists", "Artists you follow in TIDAL show up here."))
}

pub fn artist_cards(window: &ZekeWindow, title: &str, artist: u64, path: String) -> BrowsePage {
    open(window, title, Source::ArtistViewAll { artist, path }, ("Nothing Here", "This section is empty."))
}
