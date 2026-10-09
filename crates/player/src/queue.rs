//! The play queue: history, current track and upcoming tracks as one list
//! plus a play order.
//!
//! `items` is the source list in its own order, `order` is the play order
//! over it, and `pos` is the current track's place in `order`: history is
//! `order[..pos]` and the upcoming queue is `order[pos + 1..]`. That is the
//! shape `PersistedQueue` stores.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Repeat mode: off, repeat-all, repeat-one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

/// What the page that queued a track knew about it, so the UI
/// and MPRIS can show it without asking TIDAL again. Saved with the queue,
/// so a restored queue needs no metadata requests either.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TrackInfo {
    pub title: String,
    /// Display names, comma-separated.
    pub artists: String,
    pub album: String,
    /// Image id of the album cover (`resources.tidal.com`).
    pub cover: Option<String>,
    /// Seconds, from TIDAL's metadata (rounded).
    pub duration: Option<f64>,
    /// The track radio's mix id, when the page had it. Left out of the
    /// saved queue when unknown, so older queues load and read the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_mix_id: Option<String>,
}

impl TrackInfo {
    /// What a TIDAL track object (a list item or a track's detail) says
    /// about the track, read the one way the pages, Now Playing and the
    /// player all show it. `None` without a title.
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        Some(Self {
            title: display_title(v)?,
            artists: artist_names(v),
            album: text(&v["album"]["title"]).unwrap_or("").to_string(),
            cover: text(&v["album"]["cover"]).map(str::to_string),
            duration: v["duration"].as_f64().map(f64::round),
            track_mix_id: zeke_tidal::commands::browse::track_mix(v),
        })
    }
}

fn text(v: &serde_json::Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

/// The title with its version, e.g. "One More Time (Radio Edit)".
pub fn display_title(v: &serde_json::Value) -> Option<String> {
    let base = text(&v["title"])?;
    Some(match text(&v["version"]) {
        Some(version) => format!("{base} ({version})"),
        None => base.to_string(),
    })
}

/// Artist names, comma-separated: `artists[]` wins over the singular
/// `artist`.
pub fn artist_names(v: &serde_json::Value) -> String {
    let names: Vec<&str> =
        v["artists"].as_array().map(|a| a.iter().filter_map(|x| text(&x["name"])).collect()).unwrap_or_default();
    if names.is_empty() {
        text(&v["artist"]["name"]).unwrap_or("").to_string()
    } else {
        names.join(", ")
    }
}

/// A track to queue: its ID and, when a page queued it, its metadata. An
/// ID-only track (e.g. from a saved queue) has `info: None`.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueTrack {
    pub id: u64,
    pub info: Option<Arc<TrackInfo>>,
}

impl QueueTrack {
    pub fn new(id: u64, info: TrackInfo) -> Self {
        Self { id, info: Some(Arc::new(info)) }
    }

    /// ID-only tracks.
    pub fn from_ids(ids: &[u64]) -> Vec<Self> {
        ids.iter().map(|&id| id.into()).collect()
    }
}

impl From<u64> for QueueTrack {
    fn from(id: u64) -> Self {
        Self { id, info: None }
    }
}

impl From<&u64> for QueueTrack {
    fn from(id: &u64) -> Self {
        (*id).into()
    }
}

/// How an entry got into the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// The user queued it (a page, Play Next, Add to Queue).
    #[default]
    Queued,
    /// Continuous playback appended it from the radio of track `seed`.
    Radio { seed: u64 },
}

impl Origin {
    pub fn is_queued(&self) -> bool {
        *self == Origin::Queued
    }
}

/// A queue entry. `qid` tells apart two entries of the same track; it
/// travels through the engine's gapless slot and comes back in
/// `track-advanced`.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueItem {
    pub track_id: u64,
    pub qid: String,
    /// Shared with every copy of the entry (the queue is republished often).
    pub info: Option<Arc<TrackInfo>>,
    pub origin: Origin,
}

/// What `Queue::peek_next` predicts after the current track.
#[derive(Debug, Clone, PartialEq)]
pub enum NextUp {
    /// The next entry in the play order.
    Next(QueueItem),
    /// Repeat: the current entry again.
    Again(QueueItem),
    /// Repeat-all past the last entry: the first one (shuffle off).
    Wrap(QueueItem),
}

impl NextUp {
    pub fn item(&self) -> &QueueItem {
        match self {
            NextUp::Next(i) | NextUp::Again(i) | NextUp::Wrap(i) => i,
        }
    }
}

/// What `Queue::advance` moved to.
#[derive(Debug, Clone, PartialEq)]
pub enum Advance {
    /// Repeat-one at a natural track end: play the current track again.
    Same(QueueItem),
    Next(QueueItem),
    /// Repeat-all past the last track: the queue starts over (reshuffled when
    /// shuffle is on).
    Wrapped(QueueItem),
    /// Nothing left to play; the position is unchanged.
    End,
}

/// splitmix64: enough for shuffling, and seedable so tests are deterministic.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (n > 0). The modulo bias is irrelevant for queues.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// Fisher–Yates shuffle.
    fn shuffle<T>(&mut self, a: &mut [T]) {
        for i in (1..a.len()).rev() {
            let j = self.below(i + 1);
            a.swap(i, j);
        }
    }
}

#[derive(Debug, Clone)]
pub struct Queue {
    items: Vec<QueueItem>,
    order: Vec<usize>,
    pos: usize,
    shuffle: bool,
    repeat: RepeatMode,
    rng: Rng,
    next_qid: u64,
    /// Bumped by every change to the play order, the current track, shuffle
    /// or repeat, so observers know when to publish the queue again.
    revision: u64,
}

impl Queue {
    pub fn new(seed: u64) -> Self {
        Self {
            items: Vec::new(),
            order: Vec::new(),
            pos: 0,
            shuffle: false,
            repeat: RepeatMode::Off,
            rng: Rng(seed),
            next_qid: 0,
            revision: 0,
        }
    }

    fn stamp(&mut self, track: QueueTrack) -> QueueItem {
        self.stamp_as(track, Origin::Queued)
    }

    fn stamp_as(&mut self, track: QueueTrack, origin: Origin) -> QueueItem {
        self.next_qid += 1;
        QueueItem {
            track_id: track.id,
            qid: format!("{}-{}", track.id, self.next_qid),
            info: track.info,
            origin,
        }
    }

    /// Replace the queue and make `start` (an index into `tracks`) current.
    /// With shuffle, `start` plays first and the rest follow in random order;
    /// with no `start` the first track is random too. Without shuffle, no
    /// `start` means the first.
    /// Returns the current item, or `None` for an empty list.
    pub fn load<T: Into<QueueTrack>>(
        &mut self,
        tracks: impl IntoIterator<Item = T>,
        start: Option<usize>,
        shuffle: bool,
    ) -> Option<QueueItem> {
        self.revision += 1;
        self.items = tracks.into_iter().map(|t| self.stamp(t.into())).collect();
        self.shuffle = shuffle;
        if self.items.is_empty() {
            self.order.clear();
            self.pos = 0;
            return None;
        }
        let start = match start {
            Some(i) => i.min(self.items.len() - 1),
            None if shuffle => self.rng.below(self.items.len()),
            None => 0,
        };
        if shuffle {
            let mut rest: Vec<usize> = (0..self.items.len()).filter(|&i| i != start).collect();
            self.rng.shuffle(&mut rest);
            self.order = std::iter::once(start).chain(rest).collect();
            self.pos = 0;
        } else {
            self.order = (0..self.items.len()).collect();
            self.pos = start;
        }
        self.current().cloned()
    }

    pub fn current(&self) -> Option<&QueueItem> {
        self.order.get(self.pos).map(|&i| &self.items[i])
    }

    /// Place of the current track in the play order, and the order's length.
    pub fn position(&self) -> (usize, usize) {
        (self.pos, self.order.len())
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Move the revision past `other`'s (a queue that replaces another must
    /// not reuse a revision already published).
    pub(crate) fn bump_revision_past(&mut self, other: u64) {
        self.revision = self.revision.max(other) + 1;
    }

    /// The whole play order: history, the current track, then upcoming.
    pub fn in_order(&self) -> impl Iterator<Item = &QueueItem> {
        self.order.iter().map(|&i| &self.items[i])
    }

    /// Make the entry at `at` in the play order current (a click in the
    /// queue list). Entries before it become history. `None` if out of range.
    pub fn jump_to(&mut self, at: usize) -> Option<QueueItem> {
        if at >= self.order.len() {
            return None;
        }
        self.revision += 1;
        self.pos = at;
        self.current().cloned()
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn set_repeat(&mut self, mode: RepeatMode) {
        if mode != self.repeat {
            self.revision += 1;
            self.repeat = mode;
        }
    }

    /// The upcoming tracks in play order.
    pub fn upcoming(&self) -> impl Iterator<Item = &QueueItem> {
        self.order.iter().skip(self.pos + 1).map(|&i| &self.items[i])
    }

    /// What plays after the current track when it ends, for the gapless
    /// slot, repeats included: repeat-one replays it; at the end, repeat-all starts over, which is
    /// predictable only without shuffle (the wrap reshuffles).
    pub fn peek_next(&self) -> Option<NextUp> {
        let current = self.current()?;
        if self.repeat == RepeatMode::One {
            return Some(NextUp::Again(current.clone()));
        }
        if let Some(next) = self.upcoming().next() {
            return Some(NextUp::Next(next.clone()));
        }
        if self.repeat == RepeatMode::All && !self.shuffle {
            if self.order.len() == 1 {
                return Some(NextUp::Again(current.clone()));
            }
            // After shuffle was turned off, history keeps its shuffled order,
            // so the first entry can be the one playing: the wrap would play
            // it again, as its own qid, which the engine won't arm. Leave
            // that wrap to a new pipeline.
            let first = &self.items[0];
            return (first.qid != current.qid).then(|| NextUp::Wrap(first.clone()));
        }
        None
    }

    /// Move on. `explicit` is a user skip, which ignores repeat-one.
    pub fn advance(&mut self, explicit: bool) -> Advance {
        let Some(current) = self.current().cloned() else {
            return Advance::End;
        };
        if self.repeat == RepeatMode::One && !explicit {
            return Advance::Same(current);
        }
        if self.pos + 1 < self.order.len() {
            self.revision += 1;
            self.pos += 1;
            return Advance::Next(self.current().cloned().expect("pos in range"));
        }
        if self.repeat == RepeatMode::All {
            return Advance::Wrapped(self.wrap().expect("non-empty"));
        }
        Advance::End
    }

    /// The first entry in source order: where a repeat-all wrap starts
    /// when shuffle is off.
    pub fn first(&self) -> Option<&QueueItem> {
        self.items.first()
    }

    /// Wrap (as `wrap`) and start the new cycle at the entry `qid`, which
    /// the engine already switched to. If shuffle came on meanwhile, the
    /// reshuffled order is kept and the entry moves to its front, so no
    /// entry drops into history unplayed.
    pub fn wrap_to(&mut self, qid: &str) -> Option<QueueItem> {
        self.wrap()?;
        let at = self.order.iter().position(|&i| self.items[i].qid == qid)?;
        let index = self.order.remove(at);
        self.order.insert(0, index);
        self.current().cloned()
    }

    /// Start the queue over for repeat-all (reshuffled when shuffle is
    /// on). `None` for an empty queue.
    pub fn wrap(&mut self) -> Option<QueueItem> {
        if self.items.is_empty() {
            return None;
        }
        self.revision += 1;
        if self.shuffle {
            self.rng.shuffle(&mut self.order);
        } else {
            self.order = (0..self.items.len()).collect();
        }
        self.pos = 0;
        self.current().cloned()
    }

    /// Step back through the history. `None` at the start.
    pub fn back(&mut self) -> Option<QueueItem> {
        if self.pos == 0 {
            return None;
        }
        self.revision += 1;
        self.pos -= 1;
        self.current().cloned()
    }

    /// Make the upcoming entry with this qid current, after a gapless
    /// switch by the engine. The entry moves up to right after the
    /// current track first, so entries queued ahead of it after the engine
    /// had committed to the switch still play next rather than drop into
    /// history.
    pub fn advance_to_qid(&mut self, qid: &str) -> Option<QueueItem> {
        let at = (self.pos + 1..self.order.len()).find(|&at| self.items[self.order[at]].qid == qid)?;
        self.revision += 1;
        let index = self.order.remove(at);
        self.order.insert(self.pos + 1, index);
        self.pos += 1;
        self.current().cloned()
    }

    /// Toggle shuffle: on shuffles only the upcoming tracks; off
    /// restores their source order. History and the current track stay put.
    pub fn set_shuffle(&mut self, on: bool) {
        if on == self.shuffle {
            return;
        }
        self.revision += 1;
        self.shuffle = on;
        if self.order.is_empty() {
            return;
        }
        let upcoming = &mut self.order[self.pos + 1..];
        if on {
            self.rng.shuffle(upcoming);
        } else {
            upcoming.sort_unstable();
        }
    }

    /// Append: at the end, or at random upcoming places when shuffled.
    pub fn append<T: Into<QueueTrack>>(&mut self, tracks: impl IntoIterator<Item = T>) {
        self.revision += 1;
        for track in tracks {
            let item = self.stamp(track.into());
            self.items.push(item);
            let index = self.items.len() - 1;
            if self.order.is_empty() {
                self.order.push(index);
                self.pos = 0;
            } else if self.shuffle {
                let slots = self.order.len() - self.pos; // after current, inclusive of end
                let at = self.pos + 1 + self.rng.below(slots);
                self.order.insert(at, index);
            } else {
                self.order.push(index);
            }
        }
    }

    /// Append at the end of the play order, in the order given, shuffle or
    /// not: a radio's sequencing is part of what it offers. Anything
    /// already upcoming stays ahead.
    pub fn append_in_order(&mut self, tracks: impl IntoIterator<Item = QueueTrack>, origin: Origin) {
        self.revision += 1;
        for track in tracks {
            let item = self.stamp_as(track, origin);
            self.items.push(item);
            if self.order.is_empty() {
                self.pos = 0;
            }
            self.order.push(self.items.len() - 1);
        }
    }

    /// Drop the oldest history entries so at most `keep` remain. The
    /// current and upcoming entries are never touched. `items` shrinks with
    /// the history (a shuffled queue saves all of `items`), and `order` is
    /// remapped onto what is left.
    pub fn trim_history(&mut self, keep: usize) {
        let Some(drop) = self.pos.checked_sub(keep).filter(|&n| n > 0) else { return };
        self.revision += 1;
        let mut gone = vec![false; self.items.len()];
        for &i in &self.order[..drop] {
            gone[i] = true;
        }
        // Old index → new index, for the entries that stay.
        let mut new_index = vec![0; self.items.len()];
        let mut kept = 0;
        for (i, &g) in gone.iter().enumerate() {
            new_index[i] = kept;
            kept += usize::from(!g);
        }
        let mut i = 0;
        self.items.retain(|_| {
            i += 1;
            !gone[i - 1]
        });
        self.order = self.order[drop..].iter().map(|&i| new_index[i]).collect();
        self.pos -= drop;
    }

    /// Play next: right after the current track.
    pub fn play_next(&mut self, track: impl Into<QueueTrack>) {
        let item = self.stamp(track.into());
        self.insert_next(item);
    }

    /// Put an existing entry (qid, info and origin kept) right after the
    /// current track.
    pub fn insert_next(&mut self, item: QueueItem) {
        self.revision += 1;
        self.items.push(item);
        let index = self.items.len() - 1;
        if self.order.is_empty() {
            self.order.push(index);
            self.pos = 0;
        } else {
            self.order.insert(self.pos + 1, index);
        }
    }

    /// Remove the upcoming entry `qid`. The current track and history
    /// stay: `None` for those and for a qid that is gone. The removal is
    /// permanent: with repeat-all, later passes leave the entry out too.
    pub fn remove(&mut self, qid: &str) -> Option<QueueItem> {
        let at = self.pos + 1 + self.upcoming().position(|t| t.qid == qid)?;
        self.revision += 1;
        let index = self.order.remove(at);
        let removed = self.items.remove(index);
        for i in &mut self.order {
            if *i > index {
                *i -= 1;
            }
        }
        Some(removed)
    }

    /// The persisted form. With shuffle on, `tracks` keeps the
    /// source order and `shuffle_order` the play order. With shuffle off the
    /// play order is stored as `tracks` itself: after shuffle was turned
    /// off mid-queue, history is still in shuffled order. Each entry keeps
    /// its metadata.
    pub fn to_persisted(&self, position_ms: u64, album_mode: bool) -> crate::persist::PersistedQueue {
        let saved = |t: &QueueItem| crate::persist::SavedTrack {
            id: t.track_id,
            info: t.info.as_deref().cloned(),
            origin: t.origin,
        };
        let (tracks, shuffle_order) = if self.shuffle {
            (self.items.iter().map(saved).collect(), Some(self.order.clone()))
        } else {
            (self.order.iter().map(|&i| saved(&self.items[i])).collect(), None)
        };
        crate::persist::PersistedQueue {
            tracks,
            shuffle_order,
            active_index: self.pos,
            position_ms,
            repeat: self.repeat,
            album_mode,
        }
    }

    /// Rebuild from the persisted form. `None` if it is inconsistent: a
    /// `shuffle_order` that is not a permutation of `tracks`, or an
    /// `active_index` out of range.
    pub fn from_persisted(p: &crate::persist::PersistedQueue, seed: u64) -> Option<Self> {
        let mut q = Queue::new(seed);
        q.items = p
            .tracks
            .iter()
            .map(|t| q.stamp_as(QueueTrack { id: t.id, info: t.info.clone().map(Arc::new) }, t.origin))
            .collect();
        q.order = match &p.shuffle_order {
            Some(order) => {
                let mut seen = vec![false; q.items.len()];
                if order.len() != q.items.len() {
                    return None;
                }
                for &i in order {
                    if i >= seen.len() || std::mem::replace(&mut seen[i], true) {
                        return None;
                    }
                }
                order.clone()
            }
            None => (0..q.items.len()).collect(),
        };
        if !q.order.is_empty() && p.active_index >= q.order.len() {
            return None;
        }
        q.pos = p.active_index;
        q.shuffle = p.shuffle_order.is_some();
        q.repeat = p.repeat;
        Some(q)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(items: impl Iterator<Item = QueueItem>) -> Vec<u64> {
        items.map(|t| t.track_id).collect()
    }

    fn play_order(q: &Queue) -> Vec<u64> {
        q.order.iter().map(|&i| q.items[i].track_id).collect()
    }

    #[test]
    fn next_and_previous_at_the_edges() {
        let mut q = Queue::new(1);
        assert_eq!(q.load([10, 20, 30], Some(0), false).unwrap().track_id, 10);
        assert_eq!(q.back(), None, "no history at the first track");
        assert!(matches!(q.advance(false), Advance::Next(t) if t.track_id == 20));
        assert!(matches!(q.advance(true), Advance::Next(t) if t.track_id == 30));
        assert_eq!(q.advance(false), Advance::End, "repeat off stops at the end");
        assert_eq!(q.current().unwrap().track_id, 30, "End leaves the position alone");
        assert_eq!(q.back().unwrap().track_id, 20);
        assert_eq!(q.back().unwrap().track_id, 10);
        assert_eq!(q.back(), None);
    }

    #[test]
    fn start_index_sets_history() {
        let mut q = Queue::new(1);
        assert_eq!(q.load([10, 20, 30], Some(2), false).unwrap().track_id, 30);
        assert_eq!(q.peek_next(), None);
        assert_eq!(q.back().unwrap().track_id, 20);
        assert_eq!(q.load([10, 20], Some(9), false).unwrap().track_id, 20, "clamped");
        assert_eq!(q.load(Vec::<u64>::new(), Some(0), false), None);
        assert_eq!(q.advance(false), Advance::End);
    }

    #[test]
    fn repeat_one_replays_unless_skipped() {
        let mut q = Queue::new(1);
        q.load([10, 20], Some(0), false);
        q.set_repeat(RepeatMode::One);
        assert!(matches!(q.peek_next(), Some(NextUp::Again(t)) if t.track_id == 10), "repeat-one predicts itself");
        assert!(matches!(q.advance(false), Advance::Same(t) if t.track_id == 10));
        assert!(matches!(q.advance(false), Advance::Same(t) if t.track_id == 10));
        assert!(matches!(q.advance(true), Advance::Next(t) if t.track_id == 20));
        assert_eq!(q.advance(true), Advance::End, "an explicit skip at the end stops");
    }

    #[test]
    fn repeat_all_wraps_to_the_first_track() {
        let mut q = Queue::new(1);
        q.load([10, 20, 30], Some(1), false);
        q.set_repeat(RepeatMode::All);
        assert!(matches!(q.advance(false), Advance::Next(t) if t.track_id == 30));
        let first = q.in_order().next().unwrap().qid.clone();
        assert!(matches!(q.peek_next(), Some(NextUp::Wrap(t)) if t.qid == first), "the wrap predicts the first entry");
        assert!(matches!(q.advance(false), Advance::Wrapped(t) if t.qid == first));
        assert_eq!(q.position(), (0, 3));
        assert!(matches!(q.advance(true), Advance::Next(t) if t.track_id == 20));
        // Shuffled, the wrap reshuffles: nothing to predict.
        q.set_shuffle(true);
        q.set_repeat(RepeatMode::Off);
        while !matches!(q.advance(true), Advance::End) {}
        q.set_repeat(RepeatMode::All);
        assert_eq!(q.peek_next(), None);
        // A queue of one: its wrap is the same entry again.
        q.load([7], Some(0), false);
        q.set_repeat(RepeatMode::All);
        assert!(matches!(q.peek_next(), Some(NextUp::Again(t)) if t.track_id == 7));
    }

    #[test]
    fn a_wrap_to_the_entry_playing_is_not_predicted() {
        // Shuffle on, played to the end, shuffle off: history keeps its
        // shuffled order, and here the first entry in source order is the
        // one playing last.
        let mut q = Queue::new(1);
        q.load([1, 2, 3], Some(0), false);
        q.order = vec![1, 2, 0];
        q.pos = 2;
        q.set_repeat(RepeatMode::All);
        assert_eq!(q.current().unwrap().track_id, 1);
        assert_eq!(q.peek_next(), None, "not Again: that would loop one track");
        // A plain end: the wrap to the first entry.
        q.order = vec![0, 1, 2];
        assert!(matches!(q.peek_next(), Some(NextUp::Wrap(t)) if t.track_id == 1));
    }

    #[test]
    fn wrap_to_keeps_every_entry_ahead() {
        let mut q = Queue::new(11);
        q.load([1, 2, 3, 4, 5], Some(4), false);
        let target = q.first().unwrap().qid.clone();
        q.set_shuffle(true); // turned on in the switch window
        let item = q.wrap_to(&target).unwrap();
        assert_eq!(item.qid, target);
        assert_eq!(q.position(), (0, 5), "nothing went into history");
        assert_eq!(q.upcoming().count(), 4);
    }

    #[test]
    fn shuffle_starts_with_the_chosen_track_and_covers_all() {
        let mut q = Queue::new(7);
        let list: Vec<u64> = (1..=20).collect();
        assert_eq!(q.load(&list, Some(4), true).unwrap().track_id, 5);
        let mut order = play_order(&q);
        assert_ne!(order, list, "seed 7 does shuffle");
        order.sort_unstable();
        assert_eq!(order, list);
    }

    #[test]
    fn shuffle_without_a_start_picks_a_random_first_track() {
        let list: Vec<u64> = (1..=10).collect();
        let firsts: std::collections::HashSet<u64> = (0..20)
            .map(|seed| Queue::new(seed).load(&list, None, true).unwrap().track_id)
            .collect();
        assert!(firsts.len() > 3, "first tracks over 20 seeds: {firsts:?}");
        assert_eq!(Queue::new(0).load(&list, None, false).unwrap().track_id, 1);
    }

    #[test]
    fn shuffle_play_orders_are_roughly_uniform() {
        // Seeds like the runner's (nanoseconds since the epoch): all six
        // orders of three tracks turn up about equally often.
        let mut counts = std::collections::HashMap::new();
        for i in 0..6000u64 {
            let mut q = Queue::new(1_727_000_000_000_000_000 + i * 7_919_993);
            q.load([1, 2, 3], None, true);
            *counts.entry(play_order(&q)).or_insert(0) += 1;
        }
        assert_eq!(counts.len(), 6);
        for (order, n) in &counts {
            assert!((850..1150).contains(n), "{order:?} came up {n} times in 6000");
        }
    }

    #[test]
    fn shuffle_order_is_stable() {
        let mut q = Queue::new(42);
        let list: Vec<u64> = (1..=10).collect();
        q.load(&list, Some(0), true);
        let order = play_order(&q);
        // Walking, repeat changes and stepping back never reshuffle.
        q.advance(false);
        q.advance(true);
        q.set_repeat(RepeatMode::One);
        q.set_repeat(RepeatMode::Off);
        q.back();
        q.set_shuffle(true); // already on: no-op
        assert_eq!(play_order(&q), order);
        // Walking the whole queue visits the order exactly.
        let mut q2 = Queue::new(42);
        q2.load(&list, Some(0), true);
        let mut walked = vec![q2.current().unwrap().track_id];
        while let Advance::Next(t) = q2.advance(false) {
            walked.push(t.track_id);
        }
        assert_eq!(walked, order);
    }

    #[test]
    fn toggling_shuffle_keeps_history_and_current() {
        let mut q = Queue::new(3);
        let list: Vec<u64> = (1..=8).collect();
        q.load(&list, Some(0), false);
        q.advance(false);
        q.advance(false); // current = 3, history = 1, 2
        q.set_shuffle(true);
        let order = play_order(&q);
        assert_eq!(&order[..3], &[1, 2, 3]);
        let mut rest = order[3..].to_vec();
        rest.sort_unstable();
        assert_eq!(rest, vec![4, 5, 6, 7, 8]);
        q.advance(false);
        let now = q.current().unwrap().track_id;
        q.set_shuffle(false);
        // Off restores source order for what is left, after the current track.
        let played: Vec<u64> = play_order(&q)[..4].to_vec();
        assert_eq!(played[3], now);
        let mut left: Vec<u64> = list.iter().copied().filter(|t| !played.contains(t)).collect();
        left.sort_unstable();
        assert_eq!(ids(q.upcoming().cloned()), left);
    }

    #[test]
    fn repeat_all_with_shuffle_reshuffles_at_the_wrap() {
        let mut q = Queue::new(11);
        let list: Vec<u64> = (1..=12).collect();
        q.load(&list, Some(0), true);
        q.set_repeat(RepeatMode::All);
        let first = play_order(&q);
        let wrap = loop {
            match q.advance(false) {
                Advance::Next(_) => continue,
                other => break other,
            }
        };
        assert!(matches!(wrap, Advance::Wrapped(_)));
        assert_eq!(q.position(), (0, 12));
        let second = play_order(&q);
        assert_ne!(first, second, "seed 11 reshuffles");
        let mut sorted = second.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, list);
    }

    #[test]
    fn queue_edits() {
        let mut q = Queue::new(5);
        q.load([10, 20, 30], Some(0), false);
        q.play_next(99);
        assert_eq!(q.peek_next().unwrap().item().track_id, 99);
        q.append([40]);
        assert_eq!(ids(q.upcoming().cloned()), vec![99, 20, 30, 40]);
        let next = q.upcoming().next().unwrap().qid.clone();
        assert_eq!(q.remove(&next).unwrap().track_id, 99);
        assert_eq!(q.remove(&next), None, "already gone");
        let current = q.current().unwrap().qid.clone();
        assert_eq!(q.remove(&current), None, "the current track stays");
        assert_eq!(ids(q.upcoming().cloned()), vec![20, 30, 40]);
        assert_eq!(q.current().unwrap().track_id, 10);
        // The same track twice gets two qids, and reconciliation finds the right one.
        q.append([20]);
        let second_20 = q.upcoming().last().unwrap().qid.clone();
        assert_eq!(q.advance_to_qid(&second_20).unwrap().qid, second_20);
        // It moved up; what was queued ahead of it is still upcoming.
        assert_eq!(q.position(), (1, 5));
        assert_eq!(ids(q.upcoming().cloned()), vec![20, 30, 40]);
        assert_eq!(q.advance_to_qid("nope"), None);
    }

    #[test]
    fn history_cannot_be_removed() {
        let mut q = Queue::new(1);
        q.load([10, 20, 30], Some(0), false);
        let first = q.current().unwrap().qid.clone();
        q.advance(true);
        let revision = q.revision();
        assert_eq!(q.remove(&first), None, "20 is playing, 10 is history");
        assert_eq!(q.revision(), revision, "nothing changed, nothing to save");
        assert_eq!(ids(q.in_order().cloned()), vec![10, 20, 30]);
    }

    #[test]
    fn removing_under_shuffle_drops_the_entry_from_both_orders() {
        let mut q = Queue::new(7);
        q.load([10, 20, 30, 40, 50], Some(0), true);
        let gone = q.upcoming().nth(1).unwrap().clone();
        let before = ids(q.upcoming().cloned());
        assert_eq!(q.remove(&gone.qid).unwrap().qid, gone.qid);
        let mut want = before;
        want.retain(|&id| id != gone.track_id);
        assert_eq!(ids(q.upcoming().cloned()), want, "the rest keep their shuffled order");
        let saved = q.to_persisted(0, false);
        assert_eq!(saved.tracks.len(), 4, "the source order lost it too");
        assert!(saved.tracks.iter().all(|t| t.id != gone.track_id));
        let back = Queue::from_persisted(&saved, 1).unwrap();
        assert_eq!(ids(back.upcoming().cloned()), want);
    }

    #[test]
    fn insert_next_keeps_the_entry_whole() {
        let info = TrackInfo { title: "Two".into(), duration: Some(180.0), ..TrackInfo::default() };
        let mut q = Queue::new(3);
        q.load([1.into(), QueueTrack::new(2, info), 3.into()], Some(0), false);
        let next = q.upcoming().next().unwrap().qid.clone();
        let two = q.remove(&next).unwrap();
        q.insert_next(two.clone());
        let back = q.upcoming().next().unwrap();
        assert_eq!((back.qid.as_str(), back.track_id, back.origin), (two.qid.as_str(), 2, two.origin));
        assert_eq!(back.info.as_ref().unwrap().title, "Two");
        assert_eq!(ids(q.upcoming().cloned()), vec![2, 3]);
        assert_eq!(q.current().unwrap().track_id, 1);
    }

    #[test]
    fn page_metadata_travels_with_the_entries() {
        let info = |title: &str| TrackInfo { title: title.into(), duration: Some(200.0), ..TrackInfo::default() };
        let mut q = Queue::new(5);
        q.load([QueueTrack::new(1, info("One")), 2.into()], Some(0), true);
        q.play_next(QueueTrack::new(3, info("Three")));
        q.append([QueueTrack::new(4, info("Four"))]);
        let titles: Vec<Option<String>> = q.in_order().map(|i| i.info.as_ref().map(|i| i.title.clone())).collect();
        assert_eq!(titles[0].as_deref(), Some("One"));
        assert_eq!(titles[1].as_deref(), Some("Three"), "play-next keeps its metadata");
        assert!(titles.contains(&Some("Four".into())));
        assert!(titles.contains(&None), "an ID-only track has none");
        // The saved queue keeps it, so a restore asks TIDAL for nothing.
        let back = Queue::from_persisted(&q.to_persisted(0, false), 1).unwrap();
        let back_titles: Vec<Option<String>> =
            back.in_order().map(|i| i.info.as_ref().map(|i| i.title.clone())).collect();
        assert_eq!(back_titles, titles);
        assert_eq!(back.current().unwrap().info.as_ref().unwrap().duration, Some(200.0));
    }

    #[test]
    fn track_info_from_tidal_json() {
        let v = serde_json::json!({
            "id": 1550546,
            "title": "One More Time",
            "version": "Radio Edit",
            "duration": 320.4,
            "artists": [{"name": "Daft Punk"}, {"name": "Romanthony"}],
            "artist": {"name": "Daft Punk"},
            "album": {"title": "Discovery", "cover": "ab-cd-ef"},
            "mixes": {"TRACK_MIX": "0012ab"},
        });
        let info = TrackInfo::from_json(&v).unwrap();
        assert_eq!(info.title, "One More Time (Radio Edit)");
        assert_eq!(info.artists, "Daft Punk, Romanthony");
        assert_eq!(info.album, "Discovery");
        assert_eq!(info.cover.as_deref(), Some("ab-cd-ef"));
        assert_eq!(info.duration, Some(320.0), "rounded");
        assert_eq!(info.track_mix_id.as_deref(), Some("0012ab"));
        let single = serde_json::json!({"title": "T", "version": "", "artist": {"name": "X"}, "album": {"cover": ""}});
        let info = TrackInfo::from_json(&single).unwrap();
        assert_eq!((info.title.as_str(), info.artists.as_str(), info.cover, info.duration), ("T", "X", None, None));
        assert_eq!(TrackInfo::from_json(&serde_json::json!({"title": ""})), None);
    }

    #[test]
    fn append_while_shuffled_keeps_existing_order() {
        let mut q = Queue::new(9);
        q.load(1..=6u64, Some(0), true);
        let before = play_order(&q);
        q.append([100, 200]);
        let after: Vec<u64> = play_order(&q).into_iter().filter(|t| *t < 100).collect();
        assert_eq!(after, before);
        assert_eq!(play_order(&q)[0], before[0], "never before the current track");
    }

    #[test]
    fn a_radio_goes_to_the_end_of_the_play_order_in_its_own_order() {
        let mut q = Queue::new(9);
        q.load(1..=6u64, Some(0), true);
        q.append([7]); // somewhere upcoming, shuffled
        let before = play_order(&q);
        q.append_in_order(QueueTrack::from_ids(&[100, 101, 102]), Origin::Radio { seed: 6 });
        let after = play_order(&q);
        assert_eq!(&after[..before.len()], &before[..], "what was queued stays ahead");
        assert_eq!(&after[before.len()..], &[100, 101, 102]);
        let origins: Vec<Origin> = q.in_order().skip(before.len()).map(|i| i.origin).collect();
        assert_eq!(origins, vec![Origin::Radio { seed: 6 }; 3]);
        assert!(q.in_order().take(before.len()).all(|i| i.origin.is_queued()));
        // Onto an empty queue: the first entry becomes current.
        let mut q = Queue::new(1);
        q.append_in_order(QueueTrack::from_ids(&[5, 6]), Origin::Queued);
        assert_eq!(q.current().unwrap().track_id, 5);
    }

    #[test]
    fn trimming_history_keeps_the_queue_consistent() {
        let mut q = Queue::new(13);
        q.load(1..=10u64, Some(0), true);
        for _ in 0..6 {
            q.advance(false);
        }
        q.append_in_order(QueueTrack::from_ids(&[100, 101]), Origin::Radio { seed: 1 });
        let order = play_order(&q);
        let current = q.current().unwrap().clone();
        let rev = q.revision();
        q.trim_history(2);
        assert!(q.revision() > rev);
        assert_eq!(q.items.len(), q.order.len());
        let mut sorted = q.order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..q.items.len()).collect::<Vec<_>>(), "order is a permutation of items");
        assert_eq!(q.position(), (2, 8));
        assert_eq!(q.current(), Some(&current));
        assert_eq!(play_order(&q), order[4..], "the oldest four went, nothing else");
        // Less history than `keep`: nothing changes.
        let rev = q.revision();
        q.trim_history(5);
        assert_eq!((q.revision(), play_order(&q)), (rev, order[4..].to_vec()));
        // The saved queue round-trips, origins included.
        let back = Queue::from_persisted(&q.to_persisted(0, false), 1).unwrap();
        assert_eq!(play_order(&back), play_order(&q));
        assert_eq!(back.current().unwrap().track_id, current.track_id);
        let origins = |q: &Queue| q.in_order().map(|i| i.origin).collect::<Vec<_>>();
        assert_eq!(origins(&back), origins(&q));
        assert_eq!(origins(&back).last(), Some(&Origin::Radio { seed: 1 }));
        // Shuffle off: history in any order is trimmed the same way.
        q.set_shuffle(false);
        q.advance(false);
        q.trim_history(0);
        assert_eq!(q.position(), (0, 5));
        assert_eq!(q.items.len(), 5);
    }

    #[test]
    fn persisted_round_trip() {
        let mut q = Queue::new(21);
        q.load(1..=9u64, Some(3), true);
        q.advance(false);
        q.set_repeat(RepeatMode::All);
        let p = q.to_persisted(61_500, false);
        assert_eq!(p.shuffle_order.as_ref().unwrap().len(), 9);
        let r = Queue::from_persisted(&p, 0).unwrap();
        assert_eq!(play_order(&r), play_order(&q));
        assert_eq!(r.current().unwrap().track_id, q.current().unwrap().track_id);
        assert!(r.shuffle());
        assert_eq!(r.repeat(), RepeatMode::All);

        // Shuffle off after shuffling: the play order is kept as track_ids.
        q.set_shuffle(false);
        let p = q.to_persisted(0, false);
        assert_eq!(p.shuffle_order, None);
        let r = Queue::from_persisted(&p, 0).unwrap();
        assert_eq!(play_order(&r), play_order(&q));
        assert!(!r.shuffle());
    }

    #[test]
    fn inconsistent_persisted_queues_are_refused() {
        use crate::persist::PersistedQueue;
        let base = PersistedQueue {
            tracks: [1, 2, 3].map(|id| crate::persist::SavedTrack { id, info: None, origin: Origin::Queued }).to_vec(),
            shuffle_order: None,
            active_index: 0,
            position_ms: 0,
            repeat: RepeatMode::Off,
            album_mode: false,
        };
        assert!(Queue::from_persisted(&base, 0).is_some());
        let bad_index = PersistedQueue { active_index: 3, ..base.clone() };
        assert!(Queue::from_persisted(&bad_index, 0).is_none());
        let dup = PersistedQueue { shuffle_order: Some(vec![0, 0, 1]), ..base.clone() };
        assert!(Queue::from_persisted(&dup, 0).is_none());
        let short = PersistedQueue { shuffle_order: Some(vec![0, 1]), ..base.clone() };
        assert!(Queue::from_persisted(&short, 0).is_none());
        let out = PersistedQueue { shuffle_order: Some(vec![0, 1, 7]), ..base };
        assert!(Queue::from_persisted(&out, 0).is_none());
    }

    #[test]
    fn jump_to_moves_within_the_play_order_and_bumps_the_revision() {
        let mut q = Queue::new(1);
        q.load([1, 2, 3, 4], Some(0), false);
        let rev = q.revision();
        assert_eq!(q.jump_to(2).unwrap().track_id, 3);
        assert!(q.revision() > rev);
        assert_eq!(q.in_order().map(|t| t.track_id).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        assert_eq!(q.upcoming().map(|t| t.track_id).collect::<Vec<_>>(), vec![4]);
        // Back into history.
        assert_eq!(q.jump_to(0).unwrap().track_id, 1);
        let rev = q.revision();
        assert!(q.jump_to(4).is_none());
        q.set_repeat(RepeatMode::Off);
        assert_eq!(q.revision(), rev, "no change, no bump");
    }
}
