//! Favorites, "the heart": which tracks, albums, artists and playlists the
//! user has favorited, read in one request when browsing starts and kept in
//! the window. Hearts on the player bar, the now-playing sheet and album,
//! artist and playlist pages, and the track rows' menu, all change it; a
//! change goes to TIDAL at once (shown first, undone if TIDAL refuses) and
//! reaches every heart through the window's `favorites-changed` signal. The
//! sidebar's Favorites pages are reloaded when next shown.

use std::collections::HashSet;
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use zeke_tidal::commands::browse::{self, Favorite};
use zeke_tidal::tidal_api::AllFavoriteIds;

use crate::browse::Root;
use crate::runtime;
use crate::window::ZekeWindow;

/// What the window knows about the user's favorites.
#[derive(Debug, Default)]
pub struct Favorites {
    /// False until the first answer (a heart shows "not favorite" until then).
    pub loaded: bool,
    pub items: HashSet<Favorite>,
    /// The signed-in user, to leave their own playlists without a heart.
    pub user: Option<u64>,
    /// Items with a request in flight, and the state last asked for: one
    /// request per item at a time, so quick clicks can't land out of order.
    pub pending: std::collections::HashMap<Favorite, bool>,
}

impl Favorites {
    fn from_ids(ids: AllFavoriteIds, user: Option<u64>) -> Self {
        let mut items = HashSet::new();
        items.extend(ids.tracks.into_iter().map(Favorite::Track));
        items.extend(ids.albums.into_iter().map(Favorite::Album));
        items.extend(ids.artists.into_iter().map(Favorite::Artist));
        items.extend(ids.playlists.into_iter().map(Favorite::Playlist));
        Self { loaded: true, items, user, pending: Default::default() }
    }
}

/// Heart redraws to run when what they show changes.
#[derive(Default, Clone)]
pub struct Redraws(pub Vec<std::rc::Rc<dyn Fn()>>);

impl std::fmt::Debug for Redraws {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Redraws({})", self.0.len())
    }
}

/// The Favorites page that lists this kind.
fn root_of(item: &Favorite) -> Root {
    match item {
        Favorite::Track(_) => Root::FavoriteTracks,
        Favorite::Album(_) => Root::FavoriteAlbums,
        Favorite::Artist(_) => Root::FavoriteArtists,
        Favorite::Playlist(_) => Root::FavoritePlaylists,
    }
}

impl ZekeWindow {
    /// Read the favorites (on sign-in and at start).
    pub fn load_favorites(&self) {
        let state = Arc::clone(&self.session().state);
        let window = self.downgrade();
        runtime::spawn(
            async move {
                let ids = browse::favorite_ids(&state).await;
                let user = browse::signed_in_user(&state).await.ok();
                (ids, user)
            },
            move |(ids, user)| {
                let Some(window) = window.upgrade() else { return };
                match ids {
                    Ok(ids) => {
                        let favorites = Favorites::from_ids(ids, user);
                        log::info!("[favorites] {} favorites", favorites.items.len());
                        window.imp().favorites.replace(favorites);
                        window.emit_by_name::<()>("favorites-changed", &[]);
                    }
                    Err(e) => {
                        window.report("load your favorites", &e);
                    }
                }
            },
        );
    }

    /// Signed out: forget them.
    pub fn clear_favorites(&self) {
        self.imp().favorites.replace(Favorites::default());
        self.emit_by_name::<()>("favorites-changed", &[]);
    }

    pub fn is_favorite(&self, item: &Favorite) -> bool {
        self.imp().favorites.borrow().items.contains(item)
    }

    /// A playlist of the signed-in user's own (TIDAL doesn't favorite those).
    pub fn owns_playlist(&self, creator: Option<u64>) -> bool {
        let user = self.imp().favorites.borrow().user;
        creator.is_some() && creator == user
    }

    /// Add or remove `item` (`name` is for the toast): shown at once, sent
    /// to TIDAL, undone if TIDAL refuses.
    pub fn set_favorite(&self, item: Favorite, on: bool, name: &str) {
        if self.is_favorite(&item) == on {
            return;
        }
        self.mark_favorite(&item, on);
        let busy = self.imp().favorites.borrow_mut().pending.insert(item.clone(), on).is_some();
        if !busy {
            self.send_favorite(item, on, name.to_string());
        }
    }

    /// One request; when it's back, the next one if the user changed their
    /// mind meanwhile.
    fn send_favorite(&self, item: Favorite, on: bool, name: String) {
        let state = Arc::clone(&self.session().state);
        let window = self.downgrade();
        let sent = item.clone();
        runtime::spawn(async move { browse::set_favorite(&state, &sent, on).await }, move |result| {
            let Some(window) = window.upgrade() else { return };
            let wish = window.imp().favorites.borrow().pending.get(&item).copied();
            match result {
                Ok(()) if wish.is_some_and(|w| w != on) => window.send_favorite(item, !on, name),
                Ok(()) => {
                    window.imp().favorites.borrow_mut().pending.remove(&item);
                    log::info!("[favorites] {} {item:?}", if on { "added" } else { "removed" });
                    window.toast(&if on {
                        format!("Added “{name}” to your favorites")
                    } else {
                        format!("Removed “{name}” from your favorites")
                    });
                    window.reload_root_when_shown(root_of(&item));
                }
                Err(e) => {
                    // TIDAL still has it as it was before this request.
                    window.imp().favorites.borrow_mut().pending.remove(&item);
                    window.mark_favorite(&item, !on);
                    window.report(if on { "add to your favorites" } else { "remove from your favorites" }, &e);
                }
            }
        });
    }

    fn mark_favorite(&self, item: &Favorite, on: bool) {
        {
            let mut favorites = self.imp().favorites.borrow_mut();
            if on {
                favorites.items.insert(item.clone());
            } else {
                favorites.items.remove(item);
            }
        }
        self.emit_by_name::<()>("favorites-changed", &[]);
    }

    /// Call `f` whenever the favorites change, for as long as `owner` lives.
    pub fn on_favorites_changed<W: IsA<glib::Object>>(&self, owner: &W, f: impl Fn(&W) + 'static) {
        let owner = owner.upcast_ref::<glib::Object>().clone();
        self.connect_closure(
            "favorites-changed",
            false,
            glib::closure_local!(
                #[watch]
                owner,
                move |_: ZekeWindow| {
                    if let Some(owner) = owner.downcast_ref::<W>() {
                        f(owner);
                    }
                }
            ),
        );
    }
}

/// Keep `button` a heart for whatever `current` names (`None` hides it):
/// filled when it is a favorite; a click flips it. Returns the update, for
/// when what `current` names changes.
pub fn bind_heart(
    window: &ZekeWindow,
    button: &gtk::Button,
    current: impl Fn() -> Option<(Favorite, String)> + 'static,
) -> std::rc::Rc<dyn Fn()> {
    let current = std::rc::Rc::new(current);
    let sync = {
        let (window, current) = (window.downgrade(), std::rc::Rc::clone(&current));
        move |button: &gtk::Button| {
            let Some(window) = window.upgrade() else { return };
            match current() {
                Some((item, _)) => {
                    let on = window.is_favorite(&item);
                    button.set_icon_name(if on { "heart-filled-symbolic" } else { "heart-outline-symbolic" });
                    button.set_tooltip_text(Some(if on { "Remove from Favorites" } else { "Add to Favorites" }));
                    if on {
                        button.add_css_class("heart-on");
                    } else {
                        button.remove_css_class("heart-on");
                    }
                    button.set_visible(true);
                }
                None => button.set_visible(false),
            }
        }
    };
    sync(button);
    window.on_favorites_changed(button, sync.clone());
    let refresh = {
        let (button, sync) = (button.downgrade(), sync.clone());
        std::rc::Rc::new(move || {
            if let Some(button) = button.upgrade() {
                sync(&button);
            }
        })
    };
    let window = window.downgrade();
    button.connect_clicked(move |button| {
        let Some(window) = window.upgrade() else { return };
        if let Some((item, name)) = current() {
            let on = !window.is_favorite(&item);
            window.set_favorite(item, on, &name);
        }
        sync(button);
    });
    refresh
}

/// A heart button for a page heading or a bar.
pub fn heart_button() -> gtk::Button {
    gtk::Button::builder()
        .icon_name("heart-outline-symbolic")
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular", "heart"])
        .visible(false)
        .build()
}
