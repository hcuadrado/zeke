//! Home feed (unused by the CLI), as plain functions over `Arc<AppState>`.
//! Parsing lives in `tidal_api.rs`.

use serde::Serialize;
use std::sync::Arc;

use crate::cache::{CacheResult, CacheTier};
use crate::client_lock::{self, Caller};
use crate::tidal_api::{HomePageResponse, TidalClient};
use crate::AppState;
use crate::TidalError;

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HomePageCached {
    pub home: HomePageResponse,
    pub is_stale: bool,
}

/// A home feed with no sections is an upstream failure wearing a success mask,
/// never content — storing one blanks Home for the whole 4h fresh window.
fn encode_home_for_cache(home: &HomePageResponse) -> Option<Vec<u8>> {
    if home.sections.is_empty() {
        return None;
    }
    serde_json::to_vec(home).ok()
}

/// The read-side half of [`encode_home_for_cache`]: an entry with no sections
/// is a miss, so caches poisoned by earlier builds heal on the next launch
/// instead of waiting out the 24h stale window.
fn decode_cached_home(bytes: &[u8]) -> Option<HomePageResponse> {
    let home = serde_json::from_slice::<HomePageResponse>(bytes).ok()?;
    if home.sections.is_empty() {
        return None;
    }
    Some(home)
}

/// The Home page, with the client taken for one request at a time: the
/// v2 feed first; for the static feed, if v2 has nothing, the v1 pages
/// merged. A v2 failure stands
/// only if the fallback finds nothing either. Trusts TIDAL's order.
async fn fetch_home(state: &AppState, slug: &str) -> Result<HomePageResponse, TidalError> {
    let lock = || client_lock::lock(&state.tidal_client, Caller::Other("home"));
    let (tabs, mut sections, cursor, v2_error) = match lock().await.fetch_v2_home_feed(slug, None).await {
        Ok((tabs, sections, cursor)) => (tabs, sections, cursor, None),
        Err(e) => {
            log::warn!("[home v2]: home/feed/{} failed: {}", slug, e.log_safe());
            (vec![], vec![], None, Some(e))
        }
    };
    if !sections.is_empty() {
        // Non-content section types out.
        sections.retain(|s| {
            !s.title.trim().is_empty() && s.section_type != "PAGE_LINKS_CLOUD" && s.section_type != "PAGE_LINKS"
        });
        return Ok(HomePageResponse { tabs, sections, cursor });
    }
    // Other feeds are v2-only: a v2 failure is the whole story for them.
    if slug != "static" {
        return match v2_error {
            Some(e) => Err(e),
            None => Ok(HomePageResponse { tabs, sections, cursor }),
        };
    }
    log::debug!("[home v1]: v2 empty, falling back to v1 endpoints");
    let mut seen = std::collections::HashSet::new();
    for (i, endpoint) in
        ["pages/home", "pages/for_you", "pages/my_collection_my_mixes", "pages/explore", "pages/rising"].iter().enumerate()
    {
        match lock().await.fetch_page_endpoint(endpoint).await {
            Ok(found) => TidalClient::add_unique_sections(&mut sections, &mut seen, found),
            // The first endpoint's error stands; the rest are optional.
            Err(e) if i == 0 => return Err(e),
            Err(_) => {}
        }
    }
    // Nothing anywhere: if v2 said why, say so (an error offers a retry).
    if sections.is_empty() {
        if let Some(e) = v2_error {
            log::warn!("[home]: v2 failed and the v1 fallback found nothing");
            return Err(e);
        }
    }
    Ok(HomePageResponse { tabs: vec![], sections, cursor: None })
}

/// A section page ("view all") with no items is a failed answer, like an
/// empty Home: never stored, and a stored one reads as a miss.
fn has_items(page: &HomePageResponse) -> bool {
    page.sections.iter().any(|s| s.items.as_array().is_some_and(|a| !a.is_empty()))
}

pub async fn get_home_page(
    state: &Arc<AppState>,
    feed_type: Option<String>,
) -> Result<HomePageCached, TidalError> {
    home_from_cache(state, feed_type, true).await
}

/// As `get_home_page`, without its background refresh of a stale copy: for
/// a caller that refreshes on its own (the app shows the stale feed, then
/// asks `refresh_home_page`), so a stale feed costs one request, not two.
pub async fn get_cached_home_page(
    state: &Arc<AppState>,
    feed_type: Option<String>,
) -> Result<HomePageCached, TidalError> {
    home_from_cache(state, feed_type, false).await
}

async fn home_from_cache(
    state: &Arc<AppState>,
    feed_type: Option<String>,
    swr: bool,
) -> Result<HomePageCached, TidalError> {
    let slug = feed_type
        .unwrap_or_else(|| "static".to_string())
        .to_lowercase();
    let cache_key = format!("home_feed_{}", slug);
    log::debug!("[get_home_page] feed={}", slug);

    match state.disk_cache.get(&cache_key, CacheTier::Dynamic).await {
        CacheResult::Fresh(bytes) => {
            if let Some(home) = decode_cached_home(&bytes) {
                log::info!("[home] feed {slug}: fresh copy from the disk cache, no request");
                return Ok(HomePageCached {
                    home,
                    is_stale: false,
                });
            }
        }
        CacheResult::Stale(bytes) => {
            if let Some(home) = decode_cached_home(&bytes) {
                log::info!("[home] feed {slug}: stale copy from the disk cache");
                // SWR: return stale data, refresh in background.
                if swr && state.disk_cache.mark_in_flight(&cache_key).await {
                    // Only retry if last attempt was >5min ago (300s)
                    if state.disk_cache.should_retry_refresh(&cache_key, 300).await {
                        state.disk_cache.mark_refresh_attempt(&cache_key).await;
                        let st = Arc::clone(state);
                        let slug = slug.clone();
                        let cache_key = cache_key.clone();
                        tokio::spawn(async move {
                            let result = fetch_home(&st, &slug).await;
                            match result {
                                Ok(fresh) => {
                                    if let Some(json) = encode_home_for_cache(&fresh) {
                                        st.disk_cache
                                            .put(
                                                &cache_key,
                                                &json,
                                                CacheTier::Dynamic,
                                                &["home-page"],
                                            )
                                            .await
                                            .ok();
                                    }
                                }
                                Err(e) => log::warn!(
                                    "[get_home_page] background refresh failed: {}",
                                    e.log_safe()
                                ),
                            }
                            st.disk_cache.clear_in_flight(&cache_key).await;
                        });
                    } else {
                        state.disk_cache.clear_in_flight(&cache_key).await;
                    }
                }
                return Ok(HomePageCached {
                    home,
                    is_stale: true,
                });
            }
        }
        CacheResult::Miss => {}
    }

    log::info!("[home] feed {slug}: not cached; requesting it");
    let home = fetch_home(state, &slug).await?;

    if let Some(json) = encode_home_for_cache(&home) {
        state
            .disk_cache
            .put(&cache_key, &json, CacheTier::Dynamic, &["home-page"])
            .await
            .ok();
    }
    Ok(HomePageCached {
        home,
        is_stale: false,
    })
}

pub async fn refresh_home_page(
    state: &Arc<AppState>,
    feed_type: Option<String>,
) -> Result<HomePageResponse, TidalError> {
    let slug = feed_type
        .unwrap_or_else(|| "static".to_string())
        .to_lowercase();
    let cache_key = format!("home_feed_{}", slug);
    log::debug!("[refresh_home_page] feed={}", slug);
    let home = fetch_home(state, &slug).await?;

    if let Some(json) = encode_home_for_cache(&home) {
        state
            .disk_cache
            .put(&cache_key, &json, CacheTier::Dynamic, &["home-page"])
            .await
            .ok();
    }
    Ok(home)
}

pub async fn get_home_page_more(
    state: &Arc<AppState>,
    feed_type: Option<String>,
    cursor: String,
) -> Result<HomePageResponse, TidalError> {
    let slug = feed_type
        .unwrap_or_else(|| "static".to_string())
        .to_lowercase();
    log::debug!(
        "[get_home_page_more]: feed={} cursor={}",
        slug,
        &cursor[..cursor.len().min(32)]
    );
    let mut client = client_lock::lock(&state.tidal_client, Caller::Other("home")).await;
    let result = client.fetch_v2_home_feed(&slug, Some(&cursor)).await;
    drop(client);
    let (_tabs, mut sections, next_cursor) = result?;

    sections.retain(|s| {
        !s.title.trim().is_empty()
            && s.section_type != "PAGE_LINKS_CLOUD"
            && s.section_type != "PAGE_LINKS"
            && s.section_type != "SHORTCUT_LIST"
    });

    log::debug!(
        "[get_home_page_more]: got {} sections, next_cursor={:?}",
        sections.len(),
        next_cursor.is_some()
    );
    Ok(HomePageResponse {
        tabs: vec![],
        sections,
        cursor: next_cursor,
    })
}

pub async fn get_page_section(
    state: &Arc<AppState>,
    api_path: String,
) -> Result<HomePageResponse, TidalError> {
    log::debug!("[get_page_section]: api_path={}", api_path);

    let cache_key = format!("section:{}", api_path);
    match state.disk_cache.get(&cache_key, CacheTier::Dynamic).await {
        CacheResult::Fresh(bytes) => {
            if let Ok(page) = serde_json::from_slice::<HomePageResponse>(&bytes) {
                if has_items(&page) {
                    return Ok(page);
                }
            }
        }
        CacheResult::Stale(bytes) => {
            if let Some(page) = serde_json::from_slice::<HomePageResponse>(&bytes).ok().filter(has_items) {
                if state.disk_cache.mark_in_flight(&cache_key).await {
                    // Only retry if last attempt was >5min ago (300s)
                    if state.disk_cache.should_retry_refresh(&cache_key, 300).await {
                        state.disk_cache.mark_refresh_attempt(&cache_key).await;
                        let st = Arc::clone(state);
                        let path = api_path.clone();
                        let key = cache_key.clone();
                        tokio::spawn(async move {
                            let result = {
                                let mut client = client_lock::lock(&st.tidal_client, Caller::Other("home")).await;
                                client.get_page(&path).await
                            };
                            if let Some(fresh) = result.ok().filter(has_items) {
                                if let Ok(json) = serde_json::to_vec(&fresh) {
                                    st.disk_cache
                                        .put(&key, &json, CacheTier::Dynamic, &["section"])
                                        .await
                                        .ok();
                                }
                            }
                            st.disk_cache.clear_in_flight(&key).await;
                        });
                    } else {
                        state.disk_cache.clear_in_flight(&cache_key).await;
                    }
                }
                return Ok(page);
            }
        }
        CacheResult::Miss => {}
    }

    let mut client = client_lock::lock(&state.tidal_client, Caller::Other("home")).await;
    let page = client.get_page(&api_path).await?;
    drop(client);

    if !has_items(&page) {
        return Ok(page);
    }
    if let Ok(json) = serde_json::to_vec(&page) {
        state
            .disk_cache
            .put(&cache_key, &json, CacheTier::Dynamic, &["section"])
            .await
            .ok();
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tidal_api::HomePageSection;
    use serde_json::Value;

    fn section(title: &str) -> HomePageSection {
        HomePageSection {
            title: title.to_string(),
            section_type: "HORIZONTAL_LIST".to_string(),
            items: Value::Array(vec![]),
            has_more: false,
            api_path: None,
        }
    }

    fn home(sections: Vec<HomePageSection>) -> HomePageResponse {
        HomePageResponse {
            tabs: vec![],
            sections,
            cursor: None,
        }
    }

    #[test]
    fn empty_home_is_never_written_to_cache() {
        assert!(encode_home_for_cache(&home(vec![])).is_none());
    }

    #[test]
    fn home_with_sections_is_written_to_cache() {
        assert!(encode_home_for_cache(&home(vec![section("Recently played")])).is_some());
    }

    #[test]
    fn an_already_cached_empty_home_reads_back_as_a_miss() {
        // Caches poisoned before this guard existed must heal themselves on
        // read, otherwise Home stays blank for the rest of the 24h stale window.
        let bytes = serde_json::to_vec(&home(vec![])).expect("serialize");
        assert!(decode_cached_home(&bytes).is_none());
    }

    #[test]
    fn a_cached_home_with_sections_reads_back() {
        let bytes = serde_json::to_vec(&home(vec![section("Mixes for you")])).expect("serialize");
        let decoded = decode_cached_home(&bytes).expect("should decode");
        assert_eq!(decoded.sections.len(), 1);
    }

    #[test]
    fn a_section_page_without_items_is_not_content() {
        assert!(!has_items(&home(vec![section("Your listening history")])));
        let mut full = section("Your listening history");
        full.items = serde_json::json!([{"id": 1}]);
        assert!(has_items(&home(vec![section("empty"), full])));
    }

    #[test]
    fn unparseable_cache_bytes_read_back_as_a_miss() {
        assert!(decode_cached_home(b"not json").is_none());
    }
}
