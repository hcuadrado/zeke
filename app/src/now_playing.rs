//! The player bar and the now-playing sheet: both show the same state and
//! share the window's playback actions.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use zeke_player::{PlaybackState, PlayerCommand, QueueItem, RepeatMode};
use zeke_tidal::commands::browse::Favorite;

use crate::badge::{quality_badge, resampled, signal_path_rows, source_label};
use crate::covers;
use crate::queue_row::{format_time, QueueRow};
use crate::window::ZekeWindow;

impl ZekeWindow {
    pub fn setup_player_view(&self) {
        let imp = self.imp();

        let store = gio::ListStore::new::<QueueRow>();
        imp.queue_view.set_model(Some(&gtk::NoSelection::new(Some(store.clone()))));
        imp.queue_view.set_factory(Some(&queue_factory()));
        imp.queue.set(store).expect("set once");

        for scale in [&*imp.bar_seek, &*imp.sheet_seek] {
            scale.connect_change_value(glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_, _, value| {
                    window.seek_to(value);
                    glib::Propagation::Proceed
                }
            ));
        }

        let escape = gtk::ShortcutController::new();
        escape.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("Escape"),
            Some(gtk::CallbackAction::new(glib::clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_, _| {
                    window.imp().sheet.set_open(false);
                    glib::Propagation::Stop
                }
            ))),
        ));
        imp.sheet_content.add_controller(escape);

        // The title and artist open the sheet, as the cover does.
        let click = gtk::GestureClick::new();
        click.connect_released(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _, _, _| window.on_open_sheet()
        ));
        imp.bar_track_labels.add_controller(click);
        imp.bar_track_labels.set_cursor_from_name(Some("pointer"));

        imp.bar_volume.set_value(f64::from(self.session().settings().volume));
        imp.bar_volume.connect_value_changed(glib::clone!(
            #[weak(rename_to = window)]
            self,
            move |scale| window.session().set_volume(scale.value() as f32)
        ));

        for button in [&*imp.bar_heart, &*imp.sheet_heart] {
            let window = self.downgrade();
            let refresh = crate::hearts::bind_heart(self, button, move || {
                let window = window.upgrade()?;
                let imp = window.imp();
                let id = imp.now.borrow().as_ref()?.track_id;
                let title = imp.metas.borrow().get(&id).map_or_else(|| format!("Track {id}"), |m| m.title.clone());
                Some((Favorite::Track(id), title))
            });
            imp.now_hearts.borrow_mut().0.push(refresh);
        }

        self.clear_now_playing();
    }

    fn refresh_hearts(&self) {
        let hearts = self.imp().now_hearts.borrow().clone();
        for refresh in hearts.0 {
            refresh();
        }
    }

    /// A user seek: show it at once, send it once the slider rests.
    fn seek_to(&self, value: f64) {
        let imp = self.imp();
        // The same length the slider's range came from.
        let Some(duration) = imp.now.borrow().as_ref().and_then(|n| {
            n.duration.or_else(|| imp.metas.borrow().get(&n.track_id).and_then(|m| m.duration))
        }) else {
            return;
        };
        let value = value.clamp(0.0, duration);
        self.show_position(value);
        imp.hold_position.set(Some(Instant::now() + Duration::from_millis(1500)));
        if let Some(id) = imp.pending_seek.take() {
            id.remove();
        }
        let window = self.downgrade();
        let id = glib::timeout_add_local_once(Duration::from_millis(150), move || {
            if let Some(window) = window.upgrade() {
                window.imp().pending_seek.take();
                window.imp().hold_position.set(Some(Instant::now() + Duration::from_millis(1000)));
                window.send(PlayerCommand::Seek(value));
            }
        });
        imp.pending_seek.replace(Some(id));
    }

    pub fn show_position(&self, position: f64) {
        let imp = self.imp();
        // The decoded stream can run a block past the manifest's length;
        // elapsed never reads more than the total shown next to it.
        let total = imp.now.borrow().as_ref().and_then(|n| {
            n.duration.or_else(|| imp.metas.borrow().get(&n.track_id).and_then(|m| m.duration))
        });
        let position = total.map_or(position, |t| position.min(t));
        let text = format_time(position);
        for (scale, label) in [(&*imp.bar_seek, &*imp.bar_elapsed), (&*imp.sheet_seek, &*imp.sheet_elapsed)] {
            scale.set_value(position);
            label.set_label(&text);
        }
    }

    pub fn set_state(&self, state: PlaybackState) {
        let imp = self.imp();
        let playing = matches!(state, PlaybackState::Playing | PlaybackState::Loading);
        imp.playing.set(playing);
        let (icon, tip) = if playing {
            ("media-playback-pause-symbolic", "Pause")
        } else {
            ("media-playback-start-symbolic", "Play")
        };
        for button in [&*imp.bar_play, &*imp.sheet_play] {
            button.set_icon_name(icon);
            button.set_tooltip_text(Some(tip));
        }
        if state == PlaybackState::Stopped {
            imp.hold_position.set(None);
            self.show_position(0.0);
        }
    }

    pub fn set_repeat(&self, repeat: RepeatMode) {
        let imp = self.imp();
        imp.repeat.set(repeat);
        let (icon, tip) = match repeat {
            RepeatMode::Off => ("media-playlist-repeat-symbolic", "Repeat: off"),
            RepeatMode::All => ("media-playlist-repeat-symbolic", "Repeat: all"),
            RepeatMode::One => ("media-playlist-repeat-song-symbolic", "Repeat: one"),
        };
        for button in [&*imp.bar_repeat, &*imp.sheet_repeat] {
            button.set_icon_name(icon);
            button.set_tooltip_text(Some(tip));
            if repeat == RepeatMode::Off {
                button.remove_css_class("accent");
            } else {
                button.add_css_class("accent");
            }
        }
    }

    /// Titles, cover, length and badge of the current track.
    pub fn refresh_now_playing(&self) {
        let imp = self.imp();
        let Some(now) = imp.now.borrow().clone() else { return };
        self.refresh_track_rows();
        let meta = imp.metas.borrow().get(&now.track_id).cloned();
        let (title, artist, album) = match &meta {
            Some(m) => (m.title.clone(), m.artist.clone(), m.album.clone()),
            None => (format!("Track {}", now.track_id), String::new(), String::new()),
        };
        imp.bar_title.set_label(&title);
        imp.bar_artist.set_label(&artist);
        imp.sheet_title.set_label(&title);
        imp.sheet_artist.set_label(&artist);
        imp.sheet_album.set_label(&album);
        self.set_title(Some(&format!("{title} — Zeke")));

        let duration = now.duration.or(meta.as_ref().and_then(|m| m.duration)).unwrap_or(0.0);
        let total = format_time(duration);
        for (scale, label) in [(&*imp.bar_seek, &*imp.bar_total), (&*imp.sheet_seek, &*imp.sheet_total)] {
            scale.set_range(0.0, duration.max(1.0));
            scale.set_sensitive(duration > 0.0);
            label.set_label(&total);
        }

        let cover = meta.and_then(|m| m.cover);
        let url = |size| cover.as_deref().map(|c| covers::url(c, covers::Kind::Album, size));
        self.covers().show(&imp.bar_cover, url(covers::ROW));
        self.covers().show(&imp.sheet_cover, url(covers::SHEET));
        self.refresh_badge();
        self.refresh_hearts();
        imp.sheet_radio.set_sensitive(true);
    }

    /// The sheet's Radio button: the playing track's radio, on a page
    /// the sheet closes to show.
    pub fn open_playing_radio(&self) {
        let imp = self.imp();
        let Some(id) = imp.now.borrow().as_ref().map(|n| n.track_id) else { return };
        let meta = imp.metas.borrow().get(&id).cloned();
        let track = crate::browse::model::TrackData {
            id,
            title: meta.as_ref().map_or_else(|| format!("Track {id}"), |m| m.title.clone()),
            track_mix_id: meta.and_then(|m| m.track_mix_id),
            ..Default::default()
        };
        imp.sheet.set_open(false);
        imp.split_view.set_show_content(true);
        self.open_track_radio(&track);
    }

    pub fn refresh_badge(&self) {
        let imp = self.imp();
        let format = imp.now.borrow().as_ref().and_then(|n| n.format.clone()).unwrap_or_default();
        let path = imp.signal_path.borrow();
        let path = path.as_deref();

        // The sheet has room for the whole thing.
        let badge = quality_badge(&format, path);
        imp.sheet_badge.set_visible(badge.is_some());
        imp.sheet_badge.set_label(badge.as_deref().unwrap_or(""));

        // The bar: the source, a dot for resampled or not, the rest on click.
        let source = source_label(&format);
        imp.bar_badge.set_opacity(if source.is_some() { 1.0 } else { 0.0 });
        imp.bar_badge.set_sensitive(source.is_some());
        imp.bar_badge_label.set_label(source.as_deref().unwrap_or(""));
        let resampled = resampled(&format, path);
        let dot = &imp.bar_rate_dot;
        dot.set_visible(resampled.is_some());
        dot.set_css_classes(match resampled {
            Some(true) => &["rate-dot", "resampled"],
            _ => &["rate-dot", "unchanged"],
        });
        let hint = match resampled {
            Some(true) => "Resampled on the way out",
            Some(false) => "Plays at the source rate",
            None => "Stream format",
        };
        imp.bar_badge.set_tooltip_text(badge.map(|b| format!("{b}\n{hint}")).as_deref());

        let grid = &imp.bar_path_grid;
        while let Some(child) = grid.first_child() {
            grid.remove(&child);
        }
        for (row, (step, detail)) in (0..).zip(signal_path_rows(&format, path)) {
            let step = gtk::Label::builder().label(step).xalign(0.0).css_classes(["dim-label"]).build();
            let detail = gtk::Label::builder().label(detail).xalign(0.0).selectable(true).build();
            grid.attach(&step, 0, row, 1, 1);
            grid.attach(&detail, 1, row, 1, 1);
        }
    }

    /// Back to "nothing playing" (startup, logout).
    pub fn clear_now_playing(&self) {
        let imp = self.imp();
        imp.now.take();
        self.refresh_track_rows();
        imp.bar_title.set_label("Nothing playing");
        imp.bar_artist.set_label("");
        imp.sheet_title.set_label("Nothing playing");
        imp.sheet_artist.set_label("");
        imp.sheet_album.set_label("");
        self.set_title(Some("Zeke"));
        for picture in [&*imp.bar_cover, &*imp.sheet_cover] {
            picture.set_paintable(gtk::gdk::Paintable::NONE);
        }
        for (scale, label) in [(&*imp.bar_seek, &*imp.bar_total), (&*imp.sheet_seek, &*imp.sheet_total)] {
            scale.set_range(0.0, 1.0);
            scale.set_sensitive(false);
            label.set_label("0:00");
        }
        self.show_position(0.0);
        self.refresh_badge();
        if let Some(store) = imp.queue.get() {
            store.remove_all();
        }
        imp.queue_qids.borrow_mut().clear();
        imp.queue_current.set(None);
        self.refresh_hearts();
        imp.sheet_radio.set_sensitive(false);
    }

    /// Bring the list in line with the player's queue by replacing only
    /// the entries that changed: a track change moves the current mark, a
    /// "Play Next" inserts one row. Rows that stay keep their objects (and
    /// their widgets), so a 2,000-track queue costs about as much as a short
    /// one.
    pub fn show_queue(&self, items: &[QueueItem], current: usize) {
        let imp = self.imp();
        let Some(store) = imp.queue.get() else { return };
        let new: Vec<&str> = items.iter().map(|i| i.qid.as_str()).collect();
        let (at, removed, added) = {
            let old = imp.queue_qids.borrow();
            let (at, removed, added) = changed_span(&old, &new);
            (at, removed, added)
        };
        if removed > 0 || added > 0 {
            // Rows of the replaced span that come back elsewhere in it (a
            // shuffle toggle reorders them) are reused.
            let mut reuse: HashMap<String, QueueRow> = (at..at + removed)
                .filter_map(|i| store.item(i as u32).and_downcast::<QueueRow>())
                .map(|row| (row.qid(), row))
                .collect();
            let metas = imp.metas.borrow();
            let rows: Vec<QueueRow> = items[at..at + added]
                .iter()
                .map(|item| {
                    reuse.remove(&item.qid).unwrap_or_else(|| {
                        let row = QueueRow::new(item.track_id, &item.qid);
                        let meta = metas.get(&item.track_id).cloned().or_else(|| crate::session::TrackMeta::of_item(item));
                        if let Some(m) = meta {
                            fill_row(&row, &m);
                        }
                        row
                    })
                })
                .collect();
            store.splice(at as u32, removed as u32, &rows);
            imp.queue_qids.replace(new.iter().map(|q| q.to_string()).collect());
        }
        let now = store.item(current as u32).and_downcast::<QueueRow>();
        let before = imp.queue_current.upgrade();
        if before != now {
            if let Some(row) = &before {
                row.set_current(false);
            }
            if let Some(row) = &now {
                row.set_current(true);
                imp.queue_view.scroll_to(current as u32, gtk::ListScrollFlags::NONE, None);
            }
            imp.queue_current.set(now.as_ref());
        }
        // Played entries are dimmed: a history above, the queue below.
        for (i, row) in store.iter::<QueueRow>().flatten().enumerate() {
            if row.past() != (i < current) {
                row.set_past(i < current);
            }
        }
    }

    /// Scroll the queue so the current entry is its first visible row
    /// (the sheet opening). Waits for the list to be laid out first.
    pub fn scroll_queue_to_current(&self) {
        let imp = self.imp();
        let (Some(store), Some(row)) = (imp.queue.get(), imp.queue_current.upgrade()) else { return };
        let Some(current) = store.find(&row) else { return };
        let frames = std::cell::Cell::new(0);
        imp.queue_view.add_tick_callback(move |view, _| {
            frames.set(frames.get() + 1);
            let adj = view.vadjustment().expect("in a ScrolledWindow");
            if adj.page_size() <= 0.0 && frames.get() < 30 {
                return glib::ControlFlow::Continue;
            }
            // Rows are all one height: title over artist.
            let n = view.model().map_or(0, |m| m.n_items());
            if n > 0 {
                let row_height = adj.upper() / f64::from(n);
                adj.set_value((f64::from(current) * row_height).min(adj.upper() - adj.page_size()));
            }
            glib::ControlFlow::Break
        });
    }

    pub fn refresh_queue_rows(&self, track_id: u64) {
        let imp = self.imp();
        let (Some(store), Some(meta)) = (imp.queue.get(), imp.metas.borrow().get(&track_id).cloned()) else {
            return;
        };
        for row in store.iter::<QueueRow>().flatten() {
            if row.track_id() == track_id {
                fill_row(&row, &meta);
            }
        }
    }
}

fn fill_row(row: &QueueRow, meta: &crate::session::TrackMeta) {
    row.set_title(meta.title.as_str());
    row.set_artist(meta.artist.as_str());
    row.set_length(meta.duration.map(format_time).unwrap_or_default());
}

/// Where `new` differs from `old`: the start, how many entries of `old`
/// go and how many of `new` come in their place (a common prefix and suffix
/// stay).
fn changed_span(old: &[String], new: &[&str]) -> (usize, usize, usize) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a.as_str() == **b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old.iter().rev().zip(new.iter().rev()).take(room).take_while(|(a, b)| a.as_str() == **b).count();
    (prefix, old.len() - prefix - suffix, new.len() - prefix - suffix)
}

/// Rows: place (or a speaker on the current track), title over artist,
/// length. Bound with expressions, so metadata arriving later shows up.
fn queue_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
        let place = gtk::Label::builder().width_chars(3).xalign(1.0).css_classes(["dim-label", "numeric"]).build();
        let playing = gtk::Image::builder().icon_name("audio-volume-high-symbolic").width_request(24).build();
        let title = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let artist = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let length = gtk::Label::builder().css_classes(["dim-label", "numeric", "caption"]).build();
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
        text.append(&title);
        text.append(&artist);
        let row = gtk::Box::builder().spacing(12).margin_top(4).margin_bottom(4).build();
        row.append(&place);
        row.append(&playing);
        row.append(&text);
        row.append(&length);
        item.set_child(Some(&row));

        item.property_expression("position")
            .chain_closure::<String>(glib::closure!(|_: Option<glib::Object>, p: u32| {
                // An unbound row reports GTK_INVALID_LIST_POSITION (u32::MAX).
                p.checked_add(1).map(|n| n.to_string()).unwrap_or_default()
            }))
            .bind(&place, "label", gtk::Widget::NONE);
        let row_item = item.property_expression("item");
        let current = row_item.chain_property::<QueueRow>("current");
        current.bind(&playing, "visible", gtk::Widget::NONE);
        current
            .chain_closure::<bool>(glib::closure!(|_: Option<glib::Object>, c: bool| !c))
            .bind(&place, "visible", gtk::Widget::NONE);
        current
            .chain_closure::<Vec<String>>(glib::closure!(|_: Option<glib::Object>, c: bool| {
                if c { vec!["queue-current".to_string()] } else { Vec::new() }
            }))
            .bind(&title, "css-classes", gtk::Widget::NONE);
        // The tooltips carry what an ellipsis cuts off.
        for (label, property) in [(&title, "title"), (&artist, "artist")] {
            let text = row_item.chain_property::<QueueRow>(property);
            text.bind(label, "label", gtk::Widget::NONE);
            text.bind(label, "tooltip-text", gtk::Widget::NONE);
        }
        row_item.chain_property::<QueueRow>("length").bind(&length, "label", gtk::Widget::NONE);
        row_item
            .chain_property::<QueueRow>("past")
            .chain_closure::<Vec<String>>(glib::closure!(|_: Option<glib::Object>, p: bool| {
                if p { vec!["queue-past".to_string()] } else { Vec::new() }
            }))
            .bind(&row, "css-classes", gtk::Widget::NONE);
    });
    factory
}

#[cfg(test)]
mod tests {
    use super::changed_span;

    fn span(old: &[&str], new: &[&str]) -> (usize, usize, usize) {
        let old: Vec<String> = old.iter().map(|s| s.to_string()).collect();
        changed_span(&old, new)
    }

    #[test]
    fn only_the_changed_span_is_replaced() {
        assert_eq!(span(&["a", "b", "c"], &["a", "b", "c"]), (3, 0, 0), "a track change: nothing");
        assert_eq!(span(&["a", "b", "c"], &["a", "x", "b", "c"]), (1, 0, 1), "play next: one in");
        assert_eq!(span(&["a", "b", "c"], &["a", "b", "c", "d", "e"]), (3, 0, 2), "append");
        assert_eq!(span(&["a", "b", "c", "d"], &["a", "c", "d"]), (1, 1, 0), "remove");
        assert_eq!(span(&["a", "b", "c", "d"], &["a", "d", "b", "c"]), (1, 3, 3), "reorder");
        assert_eq!(span(&["a", "b"], &["x", "y", "z"]), (0, 2, 3), "a new queue");
        assert_eq!(span(&[], &["a"]), (0, 0, 1));
        assert_eq!(span(&["a", "a"], &["a"]), (1, 1, 0), "prefix and suffix never overlap");
    }
}
