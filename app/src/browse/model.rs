//! What browse pages list: tracks and cards (albums, playlists, mixes,
//! artists), read from TIDAL's JSON and wrapped as GObjects for
//! `gio::ListStore`.
//!
//! The parsing is plain data (`TrackData`, `CardData`) so it can run off
//! the main thread; the GObjects only hold it.

use std::cell::OnceCell;

use gtk::glib;
use gtk::subclass::prelude::*;
use serde_json::Value;
use zeke_player::{QueueTrack, TrackInfo};

use crate::covers::{self, Kind};

/// A playable track, as a page lists it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackData {
    pub id: u64,
    /// With the version, e.g. "One More Time (Radio Edit)".
    pub title: String,
    /// Display names, comma-separated.
    pub artists: String,
    pub artist_id: Option<u64>,
    pub album: String,
    pub album_id: Option<u64>,
    /// Album cover image id.
    pub cover: Option<String>,
    /// Seconds.
    pub duration: Option<u32>,
    /// Place on its album.
    pub number: Option<u32>,
    pub explicit: bool,
    /// TIDAL tags it hi-res lossless.
    pub hires: bool,
}

fn text(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

/// The item's type: the v2 wrapper's type, else the item's own.
fn item_type(v: &Value) -> &str {
    text(&v["_itemType"]).or_else(|| text(&v["type"])).unwrap_or("")
}

/// Present, even as `null` (JavaScript's `!== undefined`).
fn has(v: &Value, key: &str) -> bool {
    v.get(key).is_some()
}

/// Present and not empty/false/null (JavaScript truthiness, near enough).
fn truthy(v: &Value, key: &str) -> bool {
    match v.get(key) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn is_video(v: &Value, hint: Option<&str>) -> bool {
    let t = item_type(v);
    hint == Some("VIDEO_LIST")
        || t == "VIDEO"
        || t == "Music Video"
        || text(&v["itemType"]).is_some_and(|t| t.eq_ignore_ascii_case("video"))
}

/// Is this feed item a track?
fn is_track(v: &Value, hint: Option<&str>) -> bool {
    hint == Some("TRACK_LIST")
        || item_type(v) == "TRACK"
        || (has(v, "duration") && (has(v, "artist") || has(v, "artists")) && has(v, "album"))
}

/// Is this feed item a mix?
fn is_mix(v: &Value, hint: Option<&str>) -> bool {
    hint == Some("MIX_LIST") || item_type(v) == "MIX" || has(v, "mixType") || has(v, "mixImages")
}

/// Is this feed item an artist?
fn is_artist(v: &Value, hint: Option<&str>) -> bool {
    hint == Some("ARTIST_LIST")
        || item_type(v) == "ARTIST"
        || (has(v, "picture") && !truthy(v, "cover") && !truthy(v, "album") && !truthy(v, "images") && !has(v, "mixType"))
}

/// The "My Tracks" shortcut, a link to the
/// favorite tracks.
fn is_my_tracks(v: &Value) -> bool {
    const URL: &str = "tidal://my-collection/tracks";
    if v["id"].as_str() == Some(URL) {
        return true;
    }
    if item_type(v) == "DEEP_LINK" {
        return text(&v["data"]["url"]).or_else(|| text(&v["data"]["id"])) == Some(URL);
    }
    title_of(v) == "My Tracks" && !truthy(v, "uuid") && !truthy(v, "mixId") && !truthy(v, "cover")
}

/// Artist names: `artists[]` wins over the singular `artist`.
fn artist_names(v: &Value) -> String {
    let names: Vec<&str> =
        v["artists"].as_array().map(|a| a.iter().filter_map(|x| text(&x["name"])).collect()).unwrap_or_default();
    if names.is_empty() {
        text(&v["artist"]["name"]).unwrap_or("").to_string()
    } else {
        names.join(", ")
    }
}

impl TrackData {
    /// `None` for videos and anything without a numeric id and a title.
    pub fn from_value(v: &Value) -> Option<Self> {
        if is_video(v, None) {
            return None;
        }
        let id = v["id"].as_u64()?;
        let base = text(&v["title"])?;
        let title = match text(&v["version"]) {
            Some(version) => format!("{base} ({version})"),
            None => base.to_string(),
        };
        let tags = v["mediaMetadata"]["tags"].as_array();
        let hires = tags.is_some_and(|t| t.iter().any(|x| x == "HIRES_LOSSLESS"))
            || matches!(v["audioQuality"].as_str(), Some("HI_RES_LOSSLESS"));
        Some(Self {
            id,
            title,
            artists: artist_names(v),
            artist_id: v["artists"][0]["id"].as_u64().or_else(|| v["artist"]["id"].as_u64()),
            album: text(&v["album"]["title"]).unwrap_or("").to_string(),
            album_id: v["album"]["id"].as_u64(),
            cover: text(&v["album"]["cover"]).map(str::to_string),
            duration: v["duration"].as_u64().map(|d| d as u32),
            number: v["trackNumber"].as_u64().map(|n| n as u32),
            explicit: v["explicit"].as_bool().unwrap_or(false),
            hires,
        })
    }

    /// From a typed track (`TidalTrack` and friends serialize as TIDAL's JSON).
    pub fn from_typed<T: serde::Serialize>(t: &T) -> Option<Self> {
        Self::from_value(&serde_json::to_value(t).ok()?)
    }

    /// For the player: the ID with what this page knows.
    pub fn queue_track(&self) -> QueueTrack {
        QueueTrack::new(
            self.id,
            TrackInfo {
                title: self.title.clone(),
                artists: self.artists.clone(),
                album: self.album.clone(),
                cover: self.cover.clone(),
                duration: self.duration.map(f64::from),
            },
        )
    }

    pub fn cover_url(&self, size: u32) -> Option<String> {
        self.cover.as_deref().map(|c| covers::url(c, Kind::Album, size))
    }
}

/// Where a card leads.
#[derive(Debug, Clone, PartialEq)]
pub enum CardKind {
    Album(u64),
    Playlist(String),
    Mix(String),
    Artist(u64),
    /// Plays (with the other track cards of its row as the queue).
    Track(Box<TrackData>),
    /// The "My Tracks" shortcut: the favorite tracks.
    MyTracks,
}

/// A card's picture.
#[derive(Debug, Clone, PartialEq)]
pub enum Image {
    /// A TIDAL image id.
    Id(String, Kind),
    /// Ready-made URLs by width (mix images).
    Urls(Vec<(u32, String)>),
}

impl Image {
    /// The URL for about `size` px: the smallest that is big enough.
    pub fn url(&self, size: u32) -> Option<String> {
        match self {
            Image::Id(id, kind) => Some(covers::url(id, *kind, size)),
            Image::Urls(urls) => urls
                .iter()
                .filter(|(w, _)| *w >= size)
                .min_by_key(|(w, _)| *w)
                .or_else(|| urls.iter().max_by_key(|(w, _)| *w))
                .map(|(_, u)| u.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CardData {
    pub kind: CardKind,
    pub title: String,
    pub subtitle: String,
    pub image: Option<Image>,
}

/// The item's title.
fn title_of(v: &Value) -> String {
    if item_type(v) == "MAGAZINE" {
        return text(&v["data"]["shortHeader"]).unwrap_or("").into();
    }
    if item_type(v) == "DEEP_LINK" {
        return text(&v["data"]["title"]).or_else(|| text(&v["title"])).unwrap_or("").into();
    }
    text(&v["title"]).or_else(|| text(&v["name"])).or_else(|| text(&v["titleTextInfo"]["text"])).unwrap_or("").into()
}

/// The item's subtitle (no "By You", which would need the user id).
fn subtitle_of(v: &Value) -> String {
    for s in [
        &v["subTitle"],
        &v["shortSubtitle"],
        &v["subtitleTextInfo"]["text"],
        &v["subTitleTextInfo"]["text"],
        &v["shortSubtitleTextInfo"]["text"],
    ] {
        if let Some(s) = text(s) {
            return s.into();
        }
    }
    let artists = artist_names(v);
    if !artists.is_empty() {
        return artists;
    }
    if let Some(name) = text(&v["artistName"]) {
        return name.into();
    }
    if v["creator"].is_object() {
        let by = match (text(&v["creator"]["name"]), v["creator"]["id"].as_u64()) {
            (Some(name), _) => Some(format!("By {name}")),
            (None, Some(0)) => Some("By TIDAL".into()),
            _ => None,
        };
        let count = v["numberOfTracks"].as_u64().map(|n| format!("{n} Track{}", if n == 1 { "" } else { "s" }));
        let parts: Vec<String> = [by, count].into_iter().flatten().collect();
        if !parts.is_empty() {
            return parts.join(" · ");
        }
    }
    text(&v["description"]).unwrap_or("").into()
}

/// The item's image, as an id or URLs so the size is picked later.
fn image_of(v: &Value) -> Option<Image> {
    if item_type(v) == "MAGAZINE" {
        return text(&v["data"]["imageURL"]).map(|u| Image::Urls(vec![(0, u.into())]));
    }
    if item_type(v) == "DEEP_LINK" {
        return None;
    }
    if let Some(images) = v["images"].as_object() {
        let urls: Vec<(u32, String)> = [("SMALL", 320), ("MEDIUM", 640), ("LARGE", 1280)]
            .iter()
            .filter_map(|(k, w)| text(&images.get(*k)?["url"]).map(|u| (*w, u.to_string())))
            .collect();
        if !urls.is_empty() {
            return Some(Image::Urls(urls));
        }
    }
    for key in ["mixImages", "detailMixImages"] {
        if let Some(list) = v[key].as_array().filter(|l| !l.is_empty()) {
            let urls: Vec<(u32, String)> = list
                .iter()
                .filter_map(|i| Some((i["width"].as_u64().unwrap_or(0) as u32, text(&i["url"])?.to_string())))
                .collect();
            if !urls.is_empty() {
                return Some(Image::Urls(urls));
            }
        }
    }
    let id = |key: &str, kind: Kind| text(&v[key]).map(|i| Image::Id(i.into(), kind));
    id("cover", Kind::Album)
        .or_else(|| id("squareImage", Kind::Album))
        .or_else(|| id("image", Kind::Album))
        .or_else(|| id("artworkId", Kind::Artist))
        .or_else(|| id("picture", Kind::Artist))
        .or_else(|| id("selectedAlbumCoverFallback", Kind::Album))
        .or_else(|| text(&v["album"]["cover"]).map(|c| Image::Id(c.into(), Kind::Album)))
        .or_else(|| text(&v["imageUrl"]).map(|u| Image::Urls(vec![(0, u.into())])))
        .or_else(|| id("imageId", Kind::Album))
}

impl CardData {
    /// A card for a feed item, or `None` for what Zeke can't open (videos,
    /// links to pages it doesn't have). `hint` is the section's type when
    /// its items are all of one kind. The order of the checks matters:
    /// the shortcut first, then tracks, mixes, artists, and the rest.
    pub fn from_value(v: &Value, hint: Option<&str>) -> Option<Self> {
        let card = |kind| Some(Self { kind, title: title_of(v), subtitle: subtitle_of(v), image: image_of(v) });
        if is_my_tracks(v) {
            return Some(Self {
                kind: CardKind::MyTracks,
                title: "Loved Tracks".into(),
                subtitle: "Collection".into(),
                image: None,
            });
        }
        if item_type(v) == "MAGAZINE" {
            let d = &v["data"];
            return match (d["type"].as_str(), text(&d["artifactId"])) {
                (Some("PLAYLIST"), Some(uuid)) => card(CardKind::Playlist(uuid.into())),
                _ => None,
            };
        }
        // MULTIPLE_TOP_PROMOTIONS ("Featured"): content by artifactId + type.
        if let (Some(artifact), Some(kind)) = (text(&v["artifactId"]), text(&v["type"])) {
            let kind = match kind {
                "PLAYLIST" => CardKind::Playlist(artifact.into()),
                "ALBUM" => CardKind::Album(artifact.parse().ok()?),
                "ARTIST" => CardKind::Artist(artifact.parse().ok()?),
                _ => return None, // videos, and anything else
            };
            let title = text(&v["shortHeader"]).or_else(|| text(&v["header"])).unwrap_or("").to_string();
            let image = text(&v["imageId"]).map(|i| Image::Id(i.into(), Kind::Promo));
            return Some(Self { kind, title, subtitle: text(&v["shortSubHeader"]).unwrap_or("").into(), image });
        }
        if is_video(v, hint) {
            return None;
        }
        if is_track(v, hint) {
            let track = TrackData::from_value(v)?;
            return Some(Self {
                title: track.title.clone(),
                subtitle: track.artists.clone(),
                image: track.cover.clone().map(|c| Image::Id(c, Kind::Album)),
                kind: CardKind::Track(Box::new(track)),
            });
        }
        if is_mix(v, hint) {
            let id = text(&v["mixId"]).map(str::to_string).or_else(|| match &v["id"] {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })?;
            return card(CardKind::Mix(id));
        }
        if is_artist(v, hint) {
            return card(CardKind::Artist(v["id"].as_u64()?));
        }
        if let Some(uuid) = text(&v["uuid"]) {
            return card(CardKind::Playlist(uuid.into()));
        }
        card(CardKind::Album(v["id"].as_u64()?))
    }

    /// From a typed item (albums, artists, playlists): they serialize as
    /// TIDAL's JSON; `item_type` says what it is.
    pub fn from_typed<T: serde::Serialize>(t: &T, item_type: &str) -> Option<Self> {
        let mut v = serde_json::to_value(t).ok()?;
        v.as_object_mut()?.insert("_itemType".into(), Value::String(item_type.into()));
        Self::from_value(&v, None)
    }

    pub fn is_artist(&self) -> bool {
        matches!(self.kind, CardKind::Artist(_))
    }
}

/// How a Home section is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionView {
    /// A horizontal row of cards.
    Cards,
    /// A track list.
    Tracks,
}

/// Section types Zeke shows as cards: every type not handled otherwise
/// in `classify`.
const CARD_SECTIONS: &[&str] = &[
    "SHORTCUT_LIST",
    "MIX_LIST",
    "ALBUM_LIST",
    "PLAYLIST_LIST",
    "ARTIST_LIST",
    "MIXED_TYPES_LIST",
    "MIXED_LIST",
    "MULTIPLE_TOP_PROMOTIONS",
    "HIGHLIGHT_MODULE",
    "MIX_HEADER",
];

/// Which widget a Home section gets:
/// - `TRACK_LIST` whose items are all one type → a track list;
/// - `COMPACT_GRID_CARD` (and "Recently played"), the compact grid → a
///   track list when every item is a track, else cards;
/// - the card types above → cards.
///
/// `Err` says why a section is skipped: videos are out of scope, and a
/// type Zeke doesn't know is skipped rather than guessed.
pub fn classify(section_type: &str, title: &str, items: &[Value]) -> Result<SectionView, String> {
    let kinds: std::collections::HashSet<&str> = items.iter().map(item_type).filter(|t| !t.is_empty()).collect();
    let mixed = kinds.len() > 1;
    let compact = section_type == "COMPACT_GRID_CARD" || title == "Recently played";
    if compact {
        let all_tracks = !items.is_empty() && items.iter().all(|v| is_track(v, None) && !is_video(v, None));
        return Ok(if all_tracks { SectionView::Tracks } else { SectionView::Cards });
    }
    match section_type {
        "TRACK_LIST" if !mixed => Ok(SectionView::Tracks),
        "TRACK_LIST" => Ok(SectionView::Cards),
        "VIDEO_LIST" => Err("videos aren't supported".into()),
        t if CARD_SECTIONS.contains(&t) => Ok(SectionView::Cards),
        t => Err(format!("unknown section type {t}")),
    }
}

/// The section's type as a hint for its items, unless they are mixed
/// (a row is typed from its first item).
pub fn type_hint<'a>(section_type: &'a str, items: &[Value]) -> Option<&'a str> {
    let kinds: std::collections::HashSet<&str> = items.iter().map(item_type).filter(|t| !t.is_empty()).collect();
    (kinds.len() <= 1).then_some(section_type)
}

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct TrackObject {
        pub data: OnceCell<TrackData>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TrackObject {
        const NAME: &'static str = "ZekeTrackObject";
        type Type = super::TrackObject;
    }

    impl ObjectImpl for TrackObject {}

    #[derive(Debug, Default)]
    pub struct CardObject {
        pub data: OnceCell<CardData>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CardObject {
        const NAME: &'static str = "ZekeCardObject";
        type Type = super::CardObject;
    }

    impl ObjectImpl for CardObject {}
}

glib::wrapper! {
    /// A track in a `gio::ListStore`.
    pub struct TrackObject(ObjectSubclass<imp::TrackObject>);
}

glib::wrapper! {
    /// A card in a `gio::ListStore`.
    pub struct CardObject(ObjectSubclass<imp::CardObject>);
}

impl TrackObject {
    pub fn new(data: TrackData) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().data.set(data).expect("new object");
        obj
    }

    pub fn data(&self) -> &TrackData {
        self.imp().data.get().expect("set in new()")
    }
}

impl CardObject {
    pub fn new(data: CardData) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().data.set(data).expect("new object");
        obj
    }

    pub fn data(&self) -> &CardData {
        self.imp().data.get().expect("set in new()")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn track(id: u64) -> Value {
        json!({"_itemType": "TRACK", "id": id, "title": "T", "duration": 200,
               "artists": [{"id": 5, "name": "A"}, {"name": "B"}], "album": {"id": 9, "title": "Al", "cover": "c-1"}})
    }

    #[test]
    fn tracks_from_feed_json() {
        let mut v = track(1);
        v["version"] = json!("Remastered");
        v["mediaMetadata"] = json!({"tags": ["LOSSLESS", "HIRES_LOSSLESS"]});
        let t = TrackData::from_value(&v).unwrap();
        assert_eq!(t.title, "T (Remastered)");
        assert_eq!(t.artists, "A, B");
        assert_eq!((t.artist_id, t.album_id, t.duration), (Some(5), Some(9), Some(200)));
        assert!(t.hires);
        let q = t.queue_track();
        let info = q.info.unwrap();
        assert_eq!((q.id, info.album.as_str(), info.cover.as_deref(), info.duration), (1, "Al", Some("c-1"), Some(200.0)));
        assert_eq!(TrackData::from_value(&json!({"_itemType": "VIDEO", "id": 3, "title": "V"})), None);
        assert_eq!(TrackData::from_value(&json!({"id": 3, "title": "V", "itemType": "video"})), None);
    }

    #[test]
    fn cards_follow_the_item_checks() {
        let mix = json!({"_itemType": "MIX", "id": "0abc", "type": "HISTORY_ALLTIME_MIX",
            "titleTextInfo": {"text": "My Most Listened"}, "subtitleTextInfo": {"text": "Satyricon and more"},
            "mixImages": [{"url": "https://i/s.jpg", "width": 320}, {"url": "https://i/l.jpg", "width": 1500}]});
        let c = CardData::from_value(&mix, None).unwrap();
        assert_eq!(c.kind, CardKind::Mix("0abc".into()));
        assert_eq!((c.title.as_str(), c.subtitle.as_str()), ("My Most Listened", "Satyricon and more"));
        assert_eq!(c.image.unwrap().url(640).as_deref(), Some("https://i/l.jpg"));

        let album = json!({"_itemType": "ALBUM", "id": 158855255, "title": "Reinventing the Steel",
            "cover": "d1-46", "artists": [{"name": "Pantera"}]});
        let c = CardData::from_value(&album, None).unwrap();
        assert_eq!((c.kind, c.subtitle.as_str()), (CardKind::Album(158855255), "Pantera"));

        let playlist = json!({"_itemType": "PLAYLIST", "uuid": "ed66", "title": "Focus", "squareImage": "3b-3a",
            "image": "b3-6e", "creator": {"id": 1, "name": "hernan"}, "numberOfTracks": 96});
        let c = CardData::from_value(&playlist, None).unwrap();
        assert_eq!(c.kind, CardKind::Playlist("ed66".into()));
        assert_eq!(c.subtitle, "By hernan · 96 Tracks");
        assert_eq!(c.image, Some(Image::Id("3b-3a".into(), Kind::Album)));

        let artist = json!({"_itemType": "ARTIST", "id": 7, "name": "Deftones", "picture": "p-1"});
        let c = CardData::from_value(&artist, None).unwrap();
        assert!(c.is_artist());
        assert_eq!(c.image, Some(Image::Id("p-1".into(), Kind::Artist)));
        // An artist without its type still reads as one: a picture and no cover.
        assert!(CardData::from_value(&json!({"id": 8, "name": "X", "picture": null}), None).unwrap().is_artist());

        let promo = json!({"artifactId": "123", "type": "ALBUM", "header": "New", "shortHeader": "Big Album",
            "shortSubHeader": "Out now", "imageId": "ab-cd"});
        let c = CardData::from_value(&promo, Some("MULTIPLE_TOP_PROMOTIONS")).unwrap();
        assert_eq!((c.kind, c.title.as_str()), (CardKind::Album(123), "Big Album"));
        assert_eq!(c.image, Some(Image::Id("ab-cd".into(), Kind::Promo)));
        assert_eq!(CardData::from_value(&json!({"artifactId": "9", "type": "VIDEO"}), None), None);

        let my = json!({"_itemType": "DEEP_LINK", "data": {"url": "tidal://my-collection/tracks", "title": "My Tracks"}});
        assert_eq!(CardData::from_value(&my, None).unwrap().kind, CardKind::MyTracks);
        // The other two shapes of the shortcut: the link as the id, and
        // the bare title with nothing to open.
        let by_id = json!({"id": "tidal://my-collection/tracks", "title": "Loved"});
        assert_eq!(CardData::from_value(&by_id, None).unwrap().kind, CardKind::MyTracks);
        assert_eq!(CardData::from_value(&json!({"title": "My Tracks"}), None).unwrap().kind, CardKind::MyTracks);
        // A playlist called "My Tracks" is still a playlist.
        let named = json!({"title": "My Tracks", "uuid": "u-1"});
        assert_eq!(CardData::from_value(&named, None).unwrap().kind, CardKind::Playlist("u-1".into()));

        let t = CardData::from_value(&track(4), None).unwrap();
        assert!(matches!(t.kind, CardKind::Track(ref d) if d.id == 4));
    }

    #[test]
    fn home_sections_map_to_two_widgets() {
        let tracks = vec![track(1), track(2)];
        let mixed = vec![track(1), json!({"_itemType": "ALBUM", "id": 3})];
        assert_eq!(classify("TRACK_LIST", "Hits", &tracks), Ok(SectionView::Tracks));
        assert_eq!(classify("TRACK_LIST", "Hits", &mixed), Ok(SectionView::Cards), "a mixed row isn't a track list");
        assert_eq!(classify("COMPACT_GRID_CARD", "Recommended new tracks", &tracks), Ok(SectionView::Tracks));
        assert_eq!(classify("COMPACT_GRID_CARD", "Recently played", &mixed), Ok(SectionView::Cards));
        assert_eq!(classify("ALBUM_LIST", "Recently played", &tracks), Ok(SectionView::Tracks), "by title too");
        for t in ["SHORTCUT_LIST", "MIX_LIST", "ALBUM_LIST", "PLAYLIST_LIST", "ARTIST_LIST", "MULTIPLE_TOP_PROMOTIONS"] {
            assert_eq!(classify(t, "x", &mixed), Ok(SectionView::Cards), "{t}");
        }
        assert!(classify("VIDEO_LIST", "Videos", &[]).is_err());
        assert!(classify("SOMETHING_NEW", "?", &tracks).unwrap_err().contains("SOMETHING_NEW"));
        assert_eq!(type_hint("ARTIST_LIST", &mixed), None);
        assert_eq!(type_hint("MIX_LIST", &tracks), Some("MIX_LIST"));
    }
}
