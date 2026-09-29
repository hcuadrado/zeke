//! Browse pages: album, playlist, mix and artist pages, search and
//! favorites, as plain functions over `Arc<AppState>`, plus the artist-page
//! parsing.
//!
//! Every call takes the TIDAL client for one request at a time through
//! `client_lock`, so the player's stream resolves can get in between.

use std::future::Future;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cache::{CacheResult, CacheTier};
use crate::client_lock::{self, Caller, ClientGuard};
use crate::tidal_api::{
    AllFavoriteIds, AlbumPageResponse, MixPageResult, PaginatedResponse, PaginatedTracks, TidalAlbumDetail, TidalArtistDetail,
    TidalClient, TidalPlaylist, TidalSearchResults,
};
use crate::{AppState, TidalError};

/// TIDAL's largest page for playlist and favorite-track items.
pub const TRACK_PAGE: u32 = 100;

async fn client<'a>(state: &'a AppState, what: &'static str) -> ClientGuard<'a> {
    client_lock::lock(&state.tidal_client, Caller::Other(what)).await
}

/// The cache pattern for page data: fresh → the cached copy; stale →
/// the cached copy now and one background refresh (at most every 5 min);
/// miss → fetch and store.
async fn cached<T, F, Fut>(
    state: &Arc<AppState>,
    key: String,
    tier: CacheTier,
    tags: &'static [&'static str],
    fetch: F,
) -> Result<T, TidalError>
where
    T: Serialize + DeserializeOwned + Send + Sync + 'static,
    F: Fn(Arc<AppState>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, TidalError>> + Send + 'static,
{
    match state.disk_cache.get(&key, tier).await {
        CacheResult::Fresh(bytes) => {
            if let Ok(value) = serde_json::from_slice(&bytes) {
                return Ok(value);
            }
        }
        CacheResult::Stale(bytes) => {
            if let Ok(value) = serde_json::from_slice::<T>(&bytes) {
                if state.disk_cache.mark_in_flight(&key).await {
                    if state.disk_cache.should_retry_refresh(&key, 300).await {
                        state.disk_cache.mark_refresh_attempt(&key).await;
                        let st = Arc::clone(state);
                        tokio::spawn(async move {
                            match fetch(Arc::clone(&st)).await {
                                Ok(fresh) => store(&st, &key, &fresh, tier, tags).await,
                                Err(e) => log::warn!("[browse] refreshing {key} failed: {}", e.log_safe()),
                            }
                            st.disk_cache.clear_in_flight(&key).await;
                        });
                    } else {
                        state.disk_cache.clear_in_flight(&key).await;
                    }
                }
                return Ok(value);
            }
        }
        CacheResult::Miss => {}
    }
    let value = fetch(Arc::clone(state)).await?;
    store(state, &key, &value, tier, tags).await;
    Ok(value)
}

async fn store<T: Serialize>(state: &AppState, key: &str, value: &T, tier: CacheTier, tags: &[&str]) {
    if let Ok(json) = serde_json::to_vec(value) {
        state.disk_cache.put(key, &json, tier, tags).await.ok();
    }
}

/// The signed-in user's id (favorites are per user).
async fn user_id(state: &AppState) -> Result<u64, TidalError> {
    let client = client(state, "user id").await;
    client.tokens().and_then(|t| t.user_id).ok_or(TidalError::NotAuthenticated)
}

pub async fn album_page(state: &Arc<AppState>, album_id: u64) -> Result<AlbumPageResponse, TidalError> {
    cached(state, format!("album-page:{album_id}"), CacheTier::Dynamic, &["album"], move |st| async move {
        client(&st, "album page").await.get_album_page(album_id).await
    })
    .await
}

/// The playlist's own metadata (title, image, creator, track count).
pub async fn playlist(state: &AppState, uuid: &str) -> Result<TidalPlaylist, TidalError> {
    let raw = client(state, "playlist").await.get_playlist_details(uuid).await?;
    let raw: crate::tidal_api::TidalPlaylistRaw =
        serde_json::from_value(raw).map_err(|e| TidalError::Parse(format!("playlist: {e}")))?;
    Ok(raw.into())
}

/// One page of a playlist's tracks (videos are left out by the caller).
pub async fn playlist_tracks(state: &AppState, uuid: &str, offset: u32) -> Result<PaginatedTracks, TidalError> {
    client(state, "playlist tracks").await.get_playlist_tracks_page(uuid, offset, TRACK_PAGE, None, None).await
}

pub async fn mix(state: &Arc<AppState>, mix_id: &str) -> Result<MixPageResult, TidalError> {
    let id = mix_id.to_string();
    cached(state, format!("mix-page:{mix_id}"), CacheTier::Dynamic, &["mix-page"], move |st| {
        let id = id.clone();
        async move { client(&st, "mix").await.get_mix_items(&id).await }
    })
    .await
}

/// A mix fetched from TIDAL, never from the cache: each fetch of a radio
/// returns a new sequence, and a cached one would offer the same tracks
/// every time. Stored under `mix()`'s key, so a page opened afterwards
/// shows the same station.
pub async fn mix_fresh(state: &AppState, mix_id: &str) -> Result<MixPageResult, TidalError> {
    let mix = client(state, "radio").await.get_mix_items(mix_id).await?;
    store(state, &format!("mix-page:{mix_id}"), &mix, CacheTier::Dynamic, &["mix-page"]).await;
    Ok(mix)
}

/// A track radio's mix id, from the track's detail (list items often
/// lack `mixes`). Only a found id is cached: `cached()` would keep a
/// `None` for a week, and TIDAL may add the radio later.
pub async fn track_mix_id(state: &AppState, track_id: u64) -> Result<Option<String>, TidalError> {
    let key = format!("track-mix:{track_id}");
    if let CacheResult::Fresh(bytes) | CacheResult::Stale(bytes) = state.disk_cache.get(&key, CacheTier::StaticMeta).await {
        if let Ok(id) = serde_json::from_slice::<String>(&bytes) {
            return Ok(Some(id));
        }
    }
    let track = client(state, "track mix").await.get_track(track_id).await?;
    let id = track_mix(&track);
    if let Some(id) = &id {
        store(state, &key, id, CacheTier::StaticMeta, &["track-mix"]).await;
    }
    Ok(id)
}

/// `mixes.TRACK_MIX` of a track object.
pub fn track_mix(track: &Value) -> Option<String> {
    track["mixes"]["TRACK_MIX"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

pub async fn search(state: &AppState, query: &str, limit: u32) -> Result<TidalSearchResults, TidalError> {
    client(state, "search").await.search(query, limit).await
}

/// Newest first.
pub async fn favorite_tracks(state: &AppState, offset: u32) -> Result<PaginatedTracks, TidalError> {
    let user = user_id(state).await?;
    client(state, "favorite tracks").await.get_favorite_tracks(user, offset, TRACK_PAGE, "DATE", "DESC").await
}

pub async fn favorite_albums(
    state: &AppState,
    offset: u32,
    limit: u32,
) -> Result<PaginatedResponse<TidalAlbumDetail>, TidalError> {
    let user = user_id(state).await?;
    client(state, "favorite albums").await.get_favorite_albums(user, offset, limit, "DATE", "DESC").await
}

pub async fn favorite_artists(
    state: &AppState,
    offset: u32,
    limit: u32,
) -> Result<PaginatedResponse<TidalArtistDetail>, TidalError> {
    let user = user_id(state).await?;
    client(state, "favorite artists").await.get_favorite_artists(user, offset, limit, "DATE", "DESC").await
}

/// The user's own playlists, then the ones they favorited, without
/// duplicates: TIDAL's "My Collection → Playlists". Both lists are short,
/// so they are read whole, one request per page.
pub async fn my_playlists(state: &AppState) -> Result<Vec<TidalPlaylist>, TidalError> {
    const PAGE: u32 = 50;
    const MAX: u32 = 1000;
    let user = user_id(state).await?;
    let mut out: Vec<TidalPlaylist> = Vec::new();
    let mut offset = 0;
    while offset < MAX {
        let page = client(state, "own playlists").await.get_user_playlists(user, offset, PAGE).await?;
        let n = page.items.len() as u32;
        out.extend(page.items);
        offset += n;
        if n == 0 || offset >= page.total_number_of_items {
            break;
        }
    }
    let mut offset = 0;
    while offset < MAX {
        let page = client(state, "favorite playlists").await.get_favorite_playlists(user, offset, PAGE).await?;
        let n = page.items.len() as u32;
        for p in page.items {
            if !out.iter().any(|o| o.uuid == p.uuid) {
                out.push(p);
            }
        }
        offset += n;
        if n == 0 || offset >= page.total_number_of_items {
            break;
        }
    }
    Ok(out)
}

/// Something the user can add to their favorites ("My Collection").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Favorite {
    Track(u64),
    Album(u64),
    Artist(u64),
    Playlist(String),
}

/// Every favorite's id, by kind, in one request (`favorites/ids`).
pub async fn favorite_ids(state: &AppState) -> Result<AllFavoriteIds, TidalError> {
    let user = user_id(state).await?;
    let mut client = client(state, "favorite ids").await;
    match client.get_all_favorite_ids(user).await {
        Err(e) if needs_refresh(&e) => {
            client.refresh_token().await?;
            client.get_all_favorite_ids(user).await
        }
        other => other,
    }
}

/// Add `item` to the favorites, or remove it. The favorite endpoints get
/// the token as is, so a 401 here refreshes it and tries once more.
pub async fn set_favorite(state: &AppState, item: &Favorite, on: bool) -> Result<(), TidalError> {
    let user = user_id(state).await?;
    let mut client = client(state, "favorite").await;
    match favorite_call(&client, user, item, on).await {
        Err(e) if needs_refresh(&e) => {
            client.refresh_token().await?;
            favorite_call(&client, user, item, on).await
        }
        other => other,
    }
}

/// A 401 about the token (not a track's playback sub-status).
fn needs_refresh(e: &TidalError) -> bool {
    matches!(e, TidalError::Api { status: 401, body } if !crate::tidal_api::is_playbackinfo_sub_status(body))
}

async fn favorite_call(client: &TidalClient, user: u64, item: &Favorite, on: bool) -> Result<(), TidalError> {
    match (item, on) {
        (Favorite::Track(id), true) => client.add_favorite_track(user, *id).await,
        (Favorite::Track(id), false) => client.remove_favorite_track(user, *id).await,
        (Favorite::Album(id), true) => client.add_favorite_album(user, *id).await,
        (Favorite::Album(id), false) => client.remove_favorite_album(user, *id).await,
        (Favorite::Artist(id), true) => client.add_favorite_artist(user, *id).await,
        (Favorite::Artist(id), false) => client.remove_favorite_artist(user, *id).await,
        (Favorite::Playlist(uuid), true) => client.add_favorite_playlist(user, uuid).await,
        (Favorite::Playlist(uuid), false) => client.remove_favorite_playlist(user, uuid).await,
    }
}

/// The signed-in user's id, for telling their own playlists apart.
pub async fn signed_in_user(state: &AppState) -> Result<u64, TidalError> {
    user_id(state).await
}

/// An artist page.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ArtistPage {
    pub name: String,
    pub picture: Option<String>,
    /// Newer artwork, preferred over `picture`.
    pub artwork_id: Option<String>,
    /// An album cover TIDAL falls back to when the artist has no picture.
    pub album_cover_fallback: Option<String>,
    /// The artist radio's mix id.
    pub radio_mix_id: Option<String>,
    /// The first track section's items ("Top Tracks").
    pub top_tracks: Vec<Value>,
    /// Every section with items, in page order, top tracks included.
    pub sections: Vec<ArtistSection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ArtistSection {
    pub title: String,
    /// `TRACK_LIST`, `ALBUM_LIST`, `ARTIST_LIST`, `PLAYLIST_LIST`,
    /// `MIX_LIST`, `VIDEO_LIST`, or the item type when it is none of those.
    pub section_type: String,
    /// Unwrapped items (`data` of each v2 item); videos keep `_itemType`.
    pub items: Vec<Value>,
    /// v2 "view all" path, e.g. `artist/ARTIST_ALBUMS/view-all?artistId=…`.
    pub view_all: Option<String>,
}

pub async fn artist_page(state: &Arc<AppState>, artist_id: u64) -> Result<ArtistPage, TidalError> {
    let raw: Value = cached(state, format!("artist-page:{artist_id}"), CacheTier::Dynamic, &["artist"], move |st| {
        async move { client(&st, "artist page").await.get_artist_page(artist_id).await }
    })
    .await?;
    Ok(parse_artist_page(&raw))
}

/// One page of an artist section's "view all" (v2), items unwrapped.
pub async fn artist_view_all(
    state: &AppState,
    artist_id: u64,
    path: &str,
    offset: u32,
    limit: u32,
) -> Result<Vec<Value>, TidalError> {
    let raw = client(state, "artist view all").await.get_artist_view_all(artist_id, path, offset, limit).await?;
    Ok(raw["items"].as_array().map(|items| items.iter().map(unwrap_v2_item).collect()).unwrap_or_default())
}

/// `{type, data}` → `data` (with `_itemType` for videos).
fn unwrap_v2_item(item: &Value) -> Value {
    match item.get("data") {
        Some(data) if item["type"] == "VIDEO" => {
            let mut data = data.clone();
            if let Some(obj) = data.as_object_mut() {
                obj.insert("_itemType".into(), Value::String("VIDEO".into()));
            }
            data
        }
        Some(data) => data.clone(),
        None => item.clone(),
    }
}

/// Parse an artist page: v2 has `item`, v1 has `rows`.
pub fn parse_artist_page(json: &Value) -> ArtistPage {
    if json.get("item").is_some() {
        parse_artist_page_v2(json)
    } else {
        parse_artist_page_v1(json)
    }
}

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn parse_artist_page_v2(json: &Value) -> ArtistPage {
    let data = &json["item"]["data"];
    let mut page = ArtistPage {
        name: data["name"].as_str().unwrap_or("").to_string(),
        picture: opt_str(&data["picture"]),
        artwork_id: opt_str(&data["artworkId"]),
        album_cover_fallback: opt_str(&data["selectedAlbumCoverFallback"]),
        radio_mix_id: opt_str(&data["mixes"]["ARTIST_MIX"]),
        ..ArtistPage::default()
    };
    for module in json["items"].as_array().into_iter().flatten() {
        let Some(raw) = module["items"].as_array().filter(|a| !a.is_empty()) else {
            continue;
        };
        let first = raw[0]["type"].as_str().unwrap_or("");
        if first == "TRACK_CREDITS" {
            continue;
        }
        let section_type = match first {
            "TRACK" => "TRACK_LIST",
            "ALBUM" => "ALBUM_LIST",
            "ARTIST" => "ARTIST_LIST",
            "PLAYLIST" => "PLAYLIST_LIST",
            "MIX" => "MIX_LIST",
            "VIDEO" => "VIDEO_LIST",
            other => other,
        };
        let items: Vec<Value> = raw.iter().map(unwrap_v2_item).collect();
        if section_type == "TRACK_LIST" && page.top_tracks.is_empty() {
            page.top_tracks = items.clone();
        }
        page.sections.push(ArtistSection {
            title: module["title"].as_str().unwrap_or("").to_string(),
            section_type: section_type.to_string(),
            items,
            view_all: opt_str(&module["viewAll"]),
        });
    }
    page
}

fn parse_artist_page_v1(json: &Value) -> ArtistPage {
    let mut page = ArtistPage::default();
    let modules = json["rows"].as_array().into_iter().flatten().flat_map(|r| r["modules"].as_array().into_iter().flatten());
    for module in modules {
        let kind = module["type"].as_str().unwrap_or("");
        if kind == "ARTIST_HEADER" {
            let artist = &module["artist"];
            page.name = artist["name"].as_str().unwrap_or("").to_string();
            page.picture = opt_str(&artist["picture"]);
            page.artwork_id = opt_str(&artist["artworkId"]);
            page.album_cover_fallback = opt_str(&artist["selectedAlbumCoverFallback"]);
            page.radio_mix_id = opt_str(&artist["mixes"]["ARTIST_MIX"]);
            continue;
        }
        let Some(items) = module["pagedList"]["items"].as_array().filter(|a| !a.is_empty()) else {
            continue;
        };
        let title = module["title"].as_str().unwrap_or("");
        if kind == "TRACK_LIST" && page.top_tracks.is_empty() {
            page.top_tracks = items.clone();
        }
        page.sections.push(ArtistSection {
            title: if title.is_empty() && kind == "TRACK_LIST" { "Popular tracks".into() } else { title.into() },
            section_type: kind.to_string(),
            items: items.clone(),
            // v1's `showMore` paths (`pages/…`) aren't the v2 view-all
            // pages `artist_view_all` reads.
            view_all: None,
        });
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_tracks_radio_is_its_track_mix() {
        let track = json!({"id": 1, "mixes": {"TRACK_MIX": "0012ab", "MASTER_TRACK_MIX": "0034cd"}});
        assert_eq!(track_mix(&track).as_deref(), Some("0012ab"));
        assert_eq!(track_mix(&json!({"id": 1})), None, "no mixes");
        assert_eq!(track_mix(&json!({"id": 1, "mixes": {}})), None);
        assert_eq!(track_mix(&json!({"id": 1, "mixes": {"TRACK_MIX": ""}})), None);
    }

    #[test]
    fn artist_page_v2() {
        let raw = json!({
            "item": {"data": {"name": "Pink Floyd", "picture": "p-1", "mixes": {"ARTIST_MIX": "mix-1"}}},
            "items": [
                {"title": "Top Tracks", "viewAll": "artist/ARTIST_TOP_TRACKS/view-all?artistId=1",
                 "items": [{"type": "TRACK", "data": {"id": 11, "title": "Time"}}]},
                {"title": "Albums", "items": [{"type": "ALBUM", "data": {"id": 21, "title": "DSOTM"}}]},
                {"title": "EP & Singles", "items": [{"type": "ALBUM", "data": {"id": 22}}]},
                {"title": "Credits", "items": [{"type": "TRACK_CREDITS", "data": {}}]},
                {"title": "Videos", "items": [{"type": "VIDEO", "data": {"id": 31}}]},
                {"title": "Empty", "items": []}
            ]
        });
        let page = parse_artist_page(&raw);
        assert_eq!(page.name, "Pink Floyd");
        assert_eq!(page.radio_mix_id.as_deref(), Some("mix-1"));
        assert_eq!(page.top_tracks, vec![json!({"id": 11, "title": "Time"})]);
        let kinds: Vec<(&str, &str)> =
            page.sections.iter().map(|s| (s.title.as_str(), s.section_type.as_str())).collect();
        assert_eq!(
            kinds,
            [("Top Tracks", "TRACK_LIST"), ("Albums", "ALBUM_LIST"), ("EP & Singles", "ALBUM_LIST"), ("Videos", "VIDEO_LIST")],
            "credits and empty modules are dropped"
        );
        assert_eq!(page.sections[3].items[0]["_itemType"], "VIDEO");
        assert_eq!(page.sections[0].view_all.as_deref(), Some("artist/ARTIST_TOP_TRACKS/view-all?artistId=1"));
    }

    #[test]
    fn artist_page_v1() {
        let raw = json!({"rows": [
            {"modules": [{"type": "ARTIST_HEADER", "artist": {"name": "Daft Punk", "picture": "p"}}]},
            {"modules": [{"type": "TRACK_LIST", "title": "", "pagedList": {"items": [{"id": 1}]},
                          "showMore": {"apiPath": "pages/data/x"}}]},
            {"modules": [{"type": "ALBUM_LIST", "title": "Albums", "pagedList": {"items": [{"id": 2}]}}]}
        ]});
        let page = parse_artist_page(&raw);
        assert_eq!(page.name, "Daft Punk");
        assert_eq!(page.top_tracks.len(), 1);
        assert_eq!(page.sections[0].title, "Popular tracks");
        assert_eq!(page.sections[0].view_all, None, "v1 paths aren't v2 view-all pages");
        assert_eq!(page.sections[1].section_type, "ALBUM_LIST");
    }
}
