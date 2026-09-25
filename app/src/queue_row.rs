//! One entry of the now-playing queue, as a GObject for `GtkListView`.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use std::cell::{Cell, RefCell};

    use super::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::QueueRow)]
    pub struct QueueRow {
        #[property(get, set)]
        pub track_id: Cell<u64>,
        /// The queue entry (what `PlayerCommand::JumpTo` takes).
        #[property(get, set)]
        pub qid: RefCell<String>,
        #[property(get, set)]
        pub title: RefCell<String>,
        #[property(get, set)]
        pub artist: RefCell<String>,
        /// "3:45", or empty while unknown.
        #[property(get, set)]
        pub length: RefCell<String>,
        #[property(get, set)]
        pub current: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for QueueRow {
        const NAME: &'static str = "ZekeQueueRow";
        type Type = super::QueueRow;
    }

    #[glib::derived_properties]
    impl ObjectImpl for QueueRow {}
}

glib::wrapper! {
    pub struct QueueRow(ObjectSubclass<imp::QueueRow>);
}

impl QueueRow {
    /// Its place in the list isn't stored: rows keep their object when
    /// entries are inserted before them, and the list item knows it.
    pub fn new(track_id: u64, qid: &str) -> Self {
        glib::Object::builder()
            .property("track-id", track_id)
            .property("qid", qid)
            .property("title", format!("Track {track_id}"))
            .build()
    }
}

/// 225.4 → "3:45"; an hour or more → "1:02:03".
pub fn format_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::format_time;

    #[test]
    fn times() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(225.9), "3:45");
        assert_eq!(format_time(3723.0), "1:02:03");
        assert_eq!(format_time(-1.0), "0:00");
    }
}
