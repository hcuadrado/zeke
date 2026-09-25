//! Covers in two tiers: decoded `gdk::Texture`s in a
//! small in-memory LRU, sized by count, over the encrypted disk cache
//! (`cache.rs`, image tier). Images come from `resources.tidal.com` over a
//! plain reqwest client of their own, never through the TIDAL client's lock.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use zeke_tidal::cache::{CacheResult, CacheTier};
use zeke_tidal::AppState;

use crate::runtime;

/// Textures kept decoded. At the sizes below a texture is 0.1–1.6 MB.
const MEMORY_TEXTURES: usize = 256;
/// Downloads at a time; the rest wait their turn.
const DOWNLOADS: usize = 6;

/// Square sizes by where the image is shown (TIDAL serves fixed sizes).
pub const ROW: u32 = 160;
pub const CARD: u32 = 320;
pub const SHEET: u32 = 640;

/// What an image id refers to: TIDAL keeps artist pictures and promo
/// banners at other sizes than album art.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Album covers, playlist and mix images: 80/160/320/640/1280.
    Album,
    /// Artist pictures: 160/320/480/750 (no 640 or 1280; those 403).
    Artist,
    /// `MULTIPLE_TOP_PROMOTIONS` banners: 550×400 only.
    Promo,
}

/// The URL of image `id` (a TIDAL image id, or already a URL) at about
/// `size` px, snapped to a size TIDAL serves for that kind of image.
pub fn url(id: &str, kind: Kind, size: u32) -> String {
    if id.starts_with("http") {
        return id.to_string();
    }
    let path = id.replace('-', "/");
    let snap = |sizes: &[u32]| *sizes.iter().find(|&&s| size <= s).unwrap_or(sizes.last().expect("sizes"));
    match kind {
        Kind::Album => {
            let s = snap(&[80, 160, 320, 640, 1280]);
            format!("https://resources.tidal.com/images/{path}/{s}x{s}.jpg")
        }
        Kind::Artist => {
            let s = snap(&[160, 320, 480, 750]);
            format!("https://resources.tidal.com/images/{path}/{s}x{s}.jpg")
        }
        Kind::Promo => format!("https://resources.tidal.com/images/{path}/550x400.jpg"),
    }
}

/// A least-recently-used map with a fixed number of entries.
#[derive(Debug)]
pub struct Lru<V> {
    capacity: usize,
    tick: u64,
    entries: HashMap<String, (V, u64)>,
    /// Last use → key; the first entry is the one to evict.
    by_use: BTreeMap<u64, String>,
}

impl<V: Clone> Lru<V> {
    pub fn new(capacity: usize) -> Self {
        Self { capacity: capacity.max(1), tick: 0, entries: HashMap::new(), by_use: BTreeMap::new() }
    }

    pub fn get(&mut self, key: &str) -> Option<V> {
        self.tick += 1;
        let (value, used) = self.entries.get_mut(key)?;
        self.by_use.remove(used);
        *used = self.tick;
        self.by_use.insert(self.tick, key.to_string());
        Some(value.clone())
    }

    pub fn insert(&mut self, key: String, value: V) {
        self.tick += 1;
        if let Some((_, used)) = self.entries.remove(&key) {
            self.by_use.remove(&used);
        }
        while self.entries.len() >= self.capacity {
            let Some((_, oldest)) = self.by_use.pop_first() else { break };
            self.entries.remove(&oldest);
        }
        self.by_use.insert(self.tick, key.clone());
        self.entries.insert(key, (value, self.tick));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

type Waiter = Box<dyn FnOnce(Option<&gdk::Texture>)>;

/// The app's covers. Main thread only; clones share one cache.
#[derive(Clone)]
pub struct Covers(Rc<Inner>);

struct Inner {
    memory: RefCell<Lru<gdk::Texture>>,
    /// Callers waiting for a URL already on its way.
    waiting: RefCell<HashMap<String, Vec<Waiter>>>,
    /// URLs that failed (TIDAL 403s some images at every size): not asked
    /// for again this session, so scrolling past them costs nothing.
    failed: RefCell<std::collections::HashSet<String>>,
    fetcher: Arc<Fetcher>,
}

struct Fetcher {
    state: Arc<AppState>,
    http: reqwest::Client,
    downloads: tokio::sync::Semaphore,
}

impl std::fmt::Debug for Covers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Covers").field("in_memory", &self.0.memory.borrow().len()).finish()
    }
}

impl Covers {
    pub fn new(state: Arc<AppState>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("Zeke/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("a reqwest client with rustls");
        Self(Rc::new(Inner {
            memory: RefCell::new(Lru::new(MEMORY_TEXTURES)),
            waiting: RefCell::default(),
            failed: RefCell::default(),
            fetcher: Arc::new(Fetcher { state, http, downloads: tokio::sync::Semaphore::new(DOWNLOADS) }),
        }))
    }

    /// The texture if it is in memory.
    pub fn cached(&self, url: &str) -> Option<gdk::Texture> {
        self.0.memory.borrow_mut().get(url)
    }

    /// Call `done` with the texture for `url`: at once from memory, or once
    /// it is read from disk or downloaded (`None` if that fails).
    pub fn load(&self, url: &str, done: impl FnOnce(Option<&gdk::Texture>) + 'static) {
        if let Some(texture) = self.cached(url) {
            return done(Some(&texture));
        }
        if self.0.failed.borrow().contains(url) {
            return done(None);
        }
        {
            let mut waiting = self.0.waiting.borrow_mut();
            if let Some(list) = waiting.get_mut(url) {
                list.push(Box::new(done));
                return;
            }
            waiting.insert(url.to_string(), vec![Box::new(done)]);
        }
        let fetcher = Arc::clone(&self.0.fetcher);
        let inner = Rc::downgrade(&self.0);
        let key = url.to_string();
        runtime::spawn(fetcher.fetch(key.clone()), move |texture: Option<gdk::Texture>| {
            let Some(inner) = inner.upgrade() else { return };
            match &texture {
                Some(t) => inner.memory.borrow_mut().insert(key.clone(), t.clone()),
                None => {
                    inner.failed.borrow_mut().insert(key.clone());
                }
            }
            let waiters = inner.waiting.borrow_mut().remove(&key).unwrap_or_default();
            for done in waiters {
                done(texture.as_ref());
            }
        });
    }

    /// Show `url` in `picture`, or nothing while it loads. A picture that
    /// has moved on to another URL by then (a recycled list row) is left
    /// alone.
    pub fn show(&self, picture: &gtk::Picture, url: Option<String>) {
        // SAFETY: only ever set to a `String` here, read back as one below.
        unsafe { picture.set_data("zeke-cover-url", url.clone().unwrap_or_default()) };
        let Some(url) = url else {
            picture.set_paintable(gdk::Paintable::NONE);
            return;
        };
        if let Some(texture) = self.cached(&url) {
            picture.set_paintable(Some(&texture));
            return;
        }
        picture.set_paintable(gdk::Paintable::NONE);
        let weak = picture.downgrade();
        self.load(&url.clone(), move |texture| {
            let Some(picture) = weak.upgrade() else { return };
            // SAFETY: as above; the value lives as long as the picture.
            let current = unsafe { picture.data::<String>("zeke-cover-url").map(|p| p.as_ref().clone()) };
            if current.as_deref() == Some(url.as_str()) {
                picture.set_paintable(texture);
            }
        });
    }
}

impl Fetcher {
    async fn fetch(self: Arc<Self>, url: String) -> Option<gdk::Texture> {
        let key = format!("img:{url}");
        let bytes = match self.state.disk_cache.get(&key, CacheTier::Image).await {
            CacheResult::Fresh(b) | CacheResult::Stale(b) => b,
            CacheResult::Miss => {
                let _turn = self.downloads.acquire().await.ok()?;
                let response = self.http.get(&url).send().await.and_then(|r| r.error_for_status());
                let bytes = match response {
                    Ok(r) => r.bytes().await.ok()?.to_vec(),
                    Err(e) => {
                        // The log redacts URL paths; an image path isn't secret.
                        let image = url.split("/images/").nth(1).unwrap_or("(not a TIDAL image URL)");
                        log::warn!("[covers] image {image}: {}", e.status().map_or(e.to_string(), |s| s.to_string()));
                        return None;
                    }
                };
                self.state.disk_cache.put(&key, &bytes, CacheTier::Image, &["image"]).await.ok();
                bytes
            }
        };
        tokio::task::spawn_blocking(move || gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok())
            .await
            .ok()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_snap_to_the_sizes_tidal_serves() {
        assert_eq!(url("ab-cd", Kind::Album, ROW), "https://resources.tidal.com/images/ab/cd/160x160.jpg");
        assert_eq!(url("ab-cd", Kind::Album, 500), "https://resources.tidal.com/images/ab/cd/640x640.jpg");
        assert_eq!(url("ab-cd", Kind::Album, 5000), "https://resources.tidal.com/images/ab/cd/1280x1280.jpg");
        assert_eq!(url("ab-cd", Kind::Artist, SHEET), "https://resources.tidal.com/images/ab/cd/750x750.jpg");
        assert_eq!(url("ab-cd", Kind::Artist, CARD), "https://resources.tidal.com/images/ab/cd/320x320.jpg");
        assert_eq!(url("ab-cd", Kind::Promo, CARD), "https://resources.tidal.com/images/ab/cd/550x400.jpg");
        assert_eq!(url("https://x/y.jpg", Kind::Album, ROW), "https://x/y.jpg", "mix images are URLs already");
    }

    #[test]
    fn lru_evicts_the_least_recently_used() {
        let mut lru = Lru::new(2);
        lru.insert("a".into(), 1);
        lru.insert("b".into(), 2);
        assert_eq!(lru.get("a"), Some(1)); // b is now the oldest
        lru.insert("c".into(), 3);
        assert_eq!(lru.get("b"), None);
        assert_eq!(lru.get("a"), Some(1));
        assert_eq!(lru.get("c"), Some(3));
        lru.insert("c".into(), 4); // replacing keeps the count
        assert_eq!(lru.len(), 2);
        assert_eq!(lru.get("c"), Some(4));
    }
}
