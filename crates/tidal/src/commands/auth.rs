//! PKCE login, session restore, refresh and logout, as plain functions.
//! There is no embedded-webview login and no device-code flow.

use base64::Engine;
use rand::RngExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;

use crate::tidal_api::AuthTokens;
use crate::AppState;
use crate::AuthMethod;
use crate::Settings;
use crate::TidalError;

/// Resolve credentials from saved settings, falling back to embedded defaults
/// matching the saved auth_method (LoginCode → device-code pair, Pkce → PKCE pair).
pub fn resolve_credentials(settings: &Settings) -> (String, String) {
    if !settings.client_id.is_empty() {
        return (settings.client_id.clone(), settings.client_secret.clone());
    }
    match settings.auth_method {
        AuthMethod::Pkce if crate::embedded_config::has_pkce_keys() => (
            crate::embedded_config::stream_key_c(),
            crate::embedded_config::stream_key_d(),
        ),
        _ => (
            crate::embedded_config::stream_key_a(),
            crate::embedded_config::stream_key_b(),
        ),
    }
}

pub const PKCE_REDIRECT_URI: &str = "https://tidal.com/android/login/auth";

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PkceAuthParams {
    pub authorize_url: String,
    pub code_verifier: String,
    pub client_unique_key: String,
}

/// Restore the saved session into the client.
pub async fn load_saved_auth(state: &AppState) -> Result<Option<AuthTokens>, TidalError> {
    log::debug!("[load_saved_auth]: path={:?}", state.settings_path);
    if let Some(settings) = state.load_settings() {
        log::debug!(
            "[load_saved_auth]: auth_tokens present: {}, has_credentials: {}",
            settings.auth_tokens.is_some(),
            !settings.client_id.is_empty()
        );
        if let Some(ref tokens) = settings.auth_tokens {
            let (id, secret) = resolve_credentials(&settings);
            let mut client = state.tidal_client.lock().await;
            client.tokens = Some(tokens.clone());
            client.set_credentials(&id, &secret);
            // Fetch session info to populate country_code for search
            match client.get_session_info().await {
                Ok(_) => log::debug!("[load_saved_auth]: tokens restored, country_code: {}", client.country_code),
                Err(e) => log::debug!("[load_saved_auth]: tokens restored but session info failed (will use default country_code): {}", e.log_safe()),
            }
            return Ok(Some(tokens.clone()));
        }
    } else {
        log::debug!("[load_saved_auth]: no settings file found");
    }
    Ok(None)
}

pub async fn refresh_tidal_auth(state: &AppState) -> Result<AuthTokens, TidalError> {
    log::debug!("[refresh_tidal_auth]");
    let mut client = state.tidal_client.lock().await;
    // refresh_token persists the new tokens via the client's persist hook.
    client.refresh_token().await
}

pub fn build_pkce_params(client_id: &str) -> PkceAuthParams {
    let mut rng = rand::rng();
    let random_bytes: [u8; 32] = rng.random();
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes);

    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());

    let client_unique_key = format!("{:016x}", rng.random::<u64>());

    let authorize_url = format!(
        "https://login.tidal.com/authorize?response_type=code&redirect_uri={}&client_id={}&lang=EN&appMode=android&client_unique_key={}&code_challenge={}&code_challenge_method=S256&restrict_signup=true",
        "https%3A%2F%2Ftidal.com%2Fandroid%2Flogin%2Fauth",
        client_id,
        client_unique_key,
        code_challenge,
    );

    PkceAuthParams {
        authorize_url,
        code_verifier,
        client_unique_key,
    }
}

/// Logout: stop using the session and forget it. Keeps the other settings
/// and any user-supplied client credentials, clears the disk cache.
pub async fn logout(state: &AppState) -> Result<(), TidalError> {
    log::debug!("[logout]");
    {
        let mut client = state.tidal_client.lock().await;
        client.tokens = None;
        client.country_code = "US".to_string();
    }

    // Cache first, so a failed settings write still leaves no cached data.
    state.disk_cache.clear().await;

    if state.load_settings().is_some() {
        // Under the settings lock, so a concurrent preferences write can't
        // bring the tokens back.
        state.update_settings(|s| s.auth_tokens = None)?;
    } else {
        fs::remove_file(&state.settings_path).ok();
    }

    Ok(())
}

// ==================== Embedded-credential PKCE flow ====================
//
// Uses the embedded PKCE credentials (`stream_key_c/d`); the client ID only
// ever appears inside the authorize URL.

pub async fn finish_embedded_pkce(
    state: &AppState,
    code: String,
    code_verifier: String,
    client_unique_key: String,
) -> Result<AuthTokens, TidalError> {
    let client_id = crate::embedded_config::stream_key_c();
    let client_secret = crate::embedded_config::stream_key_d();

    let mut client = state.tidal_client.lock().await;
    client.set_credentials(&client_id, &client_secret);
    let tokens = client
        .exchange_pkce_code(&code, &code_verifier, PKCE_REDIRECT_URI, &client_unique_key)
        .await?;

    // Refresh the session country for the now-logged-in account.
    if let Err(e) = client.get_session_info().await {
        log::warn!("session country refresh failed: {}", e.log_safe());
    }

    let saved = tokens.clone();
    state.update_settings(move |settings| {
        settings.auth_tokens = Some(saved);
        settings.auth_method = AuthMethod::Pkce;
        settings.client_id = String::new();
        settings.client_secret = String::new();
    })?;

    Ok(tokens)
}

/// Build the PKCE authorize URL with the embedded credentials. The caller
/// opens it in the browser, keeps the verifier and key in memory, and later
/// passes the pasted redirect to `finish_embedded_pkce`.
pub fn start_pkce_browser_login() -> Result<PkceAuthParams, TidalError> {
    log::debug!("[start_pkce_browser_login]");
    if !crate::embedded_config::has_pkce_keys() {
        return Err(TidalError::NotConfigured(
            "PKCE credentials are not embedded in this build".into(),
        ));
    }
    let client_id = crate::embedded_config::stream_key_c();
    Ok(build_pkce_params(&client_id))
}

/// Take the authorization code from what the user pasted: the `code` query
/// parameter of the redirect URL, or the raw string when it isn't a URL.
pub fn extract_pkce_code(pasted: &str) -> Option<String> {
    let pasted = pasted.trim();
    match url::Url::parse(pasted) {
        Ok(url) => url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned())
            .filter(|c| !c.is_empty()),
        Err(_) if pasted.len() > 10 && !pasted.contains(' ') => Some(pasted.to_string()),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_comes_from_the_redirect_query() {
        assert_eq!(
            extract_pkce_code("https://tidal.com/android/login/auth?code=abc123XYZ&state=na").as_deref(),
            Some("abc123XYZ")
        );
    }

    #[test]
    fn a_bare_code_is_accepted_and_junk_is_not() {
        assert_eq!(extract_pkce_code("  abcdefghijkl  ").as_deref(), Some("abcdefghijkl"));
        assert_eq!(extract_pkce_code("short"), None);
        assert_eq!(extract_pkce_code("has a space in it"), None);
        assert_eq!(extract_pkce_code("https://tidal.com/android/login/auth?x=1"), None);
    }

    #[test]
    fn pkce_challenge_is_s256_of_the_verifier() {
        let p = build_pkce_params("cid");
        let url = url::Url::parse(&p.authorize_url).unwrap();
        let q = |k: &str| url.query_pairs().find(|(n, _)| n == k).unwrap().1.into_owned();
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(p.code_verifier.as_bytes()));
        assert_eq!(q("code_challenge"), expected);
        assert_eq!(q("redirect_uri"), PKCE_REDIRECT_URI);
        assert_eq!(q("client_unique_key"), p.client_unique_key);
    }

    #[test]
    fn pkce_method_resolves_to_the_pkce_pair() {
        let s = Settings {
            auth_method: AuthMethod::Pkce,
            ..Settings::default()
        };
        if crate::embedded_config::has_pkce_keys() {
            assert_eq!(resolve_credentials(&s).0, crate::embedded_config::stream_key_c());
        }
        let custom = Settings {
            client_id: "mine".into(),
            client_secret: "s".into(),
            ..s
        };
        assert_eq!(resolve_credentials(&custom), ("mine".into(), "s".into()));
    }
}
