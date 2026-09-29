//! Queue persistence: a JSON file in Zeke's config dir. The player saves it (`runner.rs`); the app restores it at startup.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

use crate::queue::{RepeatMode, TrackInfo};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedQueue {
    pub tracks: Vec<SavedTrack>,
    /// Indices into `tracks`, in play order. `Some` exactly when shuffle
    /// is on; otherwise `tracks` is already in play order.
    pub shuffle_order: Option<Vec<usize>>,
    /// The current track's place in the play order.
    pub active_index: usize,
    pub position_ms: u64,
    pub repeat: RepeatMode,
    /// The queue is one album in order: album ReplayGain.
    #[serde(default)]
    pub album_mode: bool,
}

/// A queue entry: the track and what the page that queued it knew (plan
/// §9 Q7), so a restored queue shows at once and asks TIDAL for nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTrack {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<TrackInfo>,
}

impl PersistedQueue {
    /// `queue.json` in the config dir (`~/.config/zeke`).
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join("queue.json")
    }

    /// Written to a temporary file and renamed over the old one, so a crash
    /// mid-write never leaves a truncated queue.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// `Ok(None)` when there is no saved queue. A file that doesn't parse is
    /// an error, so the caller can log it rather than silently start empty.
    pub fn load(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// What the writer thread does next.
pub(crate) enum Save {
    Write(Box<PersistedQueue>),
    /// The queue is empty (e.g. after logout): no saved queue.
    Remove,
}

/// Writes saves on its own thread, in order, skipping any that a newer one
/// already replaced. Dropping it writes what is pending and joins.
pub(crate) struct Writer {
    tx: Option<mpsc::Sender<Save>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Writer {
    pub fn start(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<Save>();
        let thread = std::thread::Builder::new()
            .name("player-queue-save".into())
            .spawn(move || {
                while let Ok(mut save) = rx.recv() {
                    // Only the latest matters.
                    while let Ok(newer) = rx.try_recv() {
                        save = newer;
                    }
                    let result = match save {
                        Save::Write(q) => q.save(&path),
                        Save::Remove => match std::fs::remove_file(&path) {
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                            other => other,
                        },
                    };
                    if let Err(e) = result {
                        log::error!("[player] saving the queue to {} failed: {e}", path.display());
                    }
                }
            })
            .expect("spawn the queue writer");
        Self { tx: Some(tx), thread: Some(thread) }
    }

    pub fn send(&self, save: Save) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(save);
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PersistedQueue {
        let info = TrackInfo {
            title: "Time".into(),
            artists: "Pink Floyd".into(),
            album: "The Dark Side of the Moon".into(),
            cover: Some("ab-cd".into()),
            duration: Some(413.0),
            track_mix_id: None,
        };
        PersistedQueue {
            tracks: vec![
                SavedTrack { id: 455128517, info: None },
                SavedTrack { id: 55391790, info: Some(info) },
                SavedTrack { id: 357676035, info: None },
            ],
            shuffle_order: Some(vec![2, 0, 1]),
            active_index: 1,
            position_ms: 93_250,
            repeat: RepeatMode::All,
            album_mode: false,
        }
    }

    #[test]
    fn save_then_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = PersistedQueue::path(dir.path());
        sample().save(&path).unwrap();
        assert_eq!(PersistedQueue::load(&path).unwrap(), Some(sample()));
        assert!(!path.with_extension("json.tmp").exists());
        // Overwrite in place.
        let other = PersistedQueue { shuffle_order: None, repeat: RepeatMode::Off, ..sample() };
        other.save(&path).unwrap();
        assert_eq!(PersistedQueue::load(&path).unwrap(), Some(other));
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(PersistedQueue::load(&dir.path().join("queue.json")).unwrap(), None);
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = PersistedQueue::path(dir.path());
        std::fs::write(&path, b"{\"tracks\": [1,").unwrap();
        let err = PersistedQueue::load(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn json_shape() {
        let v = serde_json::to_value(sample()).unwrap();
        assert_eq!(v["repeat"], "all");
        assert_eq!(v["active_index"], 1);
        assert_eq!(v["shuffle_order"], serde_json::json!([2, 0, 1]));
        assert_eq!(v["tracks"][0], serde_json::json!({"id": 455128517}), "no metadata, no field");
        assert_eq!(v["tracks"][1]["info"]["title"], "Time");
        assert_eq!(v["tracks"][1]["info"]["duration"], 413.0);
        assert!(v["tracks"][1]["info"].get("track_mix_id").is_none(), "an unknown radio adds no field");
    }

    #[test]
    fn a_known_radio_is_saved_and_an_old_queue_still_loads() {
        let mut queue = sample();
        queue.tracks[1].info.as_mut().unwrap().track_mix_id = Some("0012ab".into());
        let v = serde_json::to_value(&queue).unwrap();
        assert_eq!(v["tracks"][1]["info"]["track_mix_id"], "0012ab");
        assert_eq!(serde_json::from_value::<PersistedQueue>(v).unwrap(), queue);
        let old = serde_json::to_value(sample()).unwrap();
        assert_eq!(serde_json::from_value::<PersistedQueue>(old).unwrap(), sample());
    }

    #[test]
    fn the_writer_keeps_the_latest_and_removes_on_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = PersistedQueue::path(dir.path());
        let writer = Writer::start(path.clone());
        writer.send(Save::Write(Box::new(sample())));
        let later = PersistedQueue { position_ms: 120_000, ..sample() };
        writer.send(Save::Write(Box::new(later.clone())));
        drop(writer); // flushes
        assert_eq!(PersistedQueue::load(&path).unwrap(), Some(later));
        let writer = Writer::start(path.clone());
        writer.send(Save::Remove);
        drop(writer);
        assert!(!path.exists());
    }
}
