//! TIDAL API client, PKCE auth, settings and encrypted persistence.
//!
pub mod cache;
pub mod client_lock;
pub mod commands;
pub mod crypto;
pub mod embedded_config;
mod error;
pub mod logger;
pub mod proxy;
pub mod proxy_http;
pub mod rate_gate;
pub mod redact;
pub mod tidal_api;
pub mod util;

pub use error::TidalError;

use cache::DiskCache;
use crypto::Crypto;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tidal_api::{AuthTokens, TidalClient};
use tokio::sync::Mutex;

mod defaults {
    pub fn yes() -> bool { true }
    pub fn volume() -> f32 { 1.0 }
    pub fn max_quality() -> String { "HI_RES_LOSSLESS".to_string() }
}

/// Tracks which embedded credential pair the saved tokens belong to,
/// so refresh-token requests use the matching client_id/secret.
/// Only relevant when the user has not provided custom credentials.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[default]
    LoginCode,
    Pkce,
}

/// Kept in `Settings`'s serde shape but unused: Zeke has no proxy.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProxyType {
    #[default]
    Http,
    Socks5,
}

/// Kept in `Settings`'s serde shape but unused: Zeke has no proxy.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ProxySettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub proxy_type: ProxyType,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

/// The app's light/dark preference (`AdwStyleManager`'s color scheme).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColorScheme {
    #[default]
    System,
    Light,
    Dark,
}

/// The settings.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Settings {
    pub auth_tokens: Option<AuthTokens>,
    /// Which embedded credential pair to use for refresh when `client_id`
    /// is empty. Must be `Pkce` for hi-res: `LoginCode` resolves to the
    /// device-code pair, which gets no unencrypted Hi-Res streams.
    #[serde(default)]
    pub auth_method: AuthMethod,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default = "defaults::volume")]
    pub volume: f32,
    #[serde(default = "defaults::max_quality")]
    pub max_quality: String,
    /// The ALSA device played exclusively, stored as `hw:CARD=<id>,DEV=<n>`,
    /// which survives card renumbering. None, the default, is the system
    /// mixer (PipeWire).
    #[serde(default)]
    pub output_device: Option<String>,
    /// Devices, in the same form, that play bit-perfect.
    #[serde(default)]
    pub bit_perfect_devices: Vec<String>,
    #[serde(default = "defaults::yes")]
    pub gapless: bool,
    #[serde(default)]
    pub volume_normalization: bool,
    #[serde(default)]
    pub proxy: ProxySettings,
    #[serde(default)]
    pub color_scheme: ColorScheme,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auth_tokens: None,
            auth_method: AuthMethod::default(),
            client_id: String::new(),
            client_secret: String::new(),
            volume: 1.0,
            max_quality: defaults::max_quality(),
            output_device: None,
            bit_perfect_devices: Vec::new(),
            gapless: true,
            volume_normalization: false,
            proxy: Default::default(),
            color_scheme: ColorScheme::System,
        }
    }
}

impl Settings {
    pub fn bit_perfect_on(&self, device: &str) -> bool {
        self.bit_perfect_devices.iter().any(|d| d == device)
    }

    pub fn set_bit_perfect_on(&mut self, device: &str, on: bool) {
        self.bit_perfect_devices.retain(|d| d != device);
        if on {
            self.bit_perfect_devices.push(device.to_owned());
        }
    }
}

/// `~/.config/zeke`. Zeke's own directory, never shared with another
/// player: its `Settings` would overwrite theirs and drop the fields it
/// doesn't know.
pub fn config_dir() -> PathBuf {
    let mut dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("zeke");
    dir
}

/// Write through a temporary file and a rename, so a crash mid-write never
/// leaves a truncated (unreadable, hence logged-out) settings file.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

/// Serializes read-modify-write cycles of the settings file: a token refresh
/// and a preferences change may land at the same time.
static SETTINGS_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Write refreshed tokens back into the stored settings. Called from every
/// refresh, including the automatic one after a 401 — without it the stored
/// access token stays stale and each launch burns a 401 before the first
/// request succeeds.
fn persist_auth_tokens(path: &Path, crypto: &Crypto, tokens: &AuthTokens) {
    let _guard = SETTINGS_WRITE.lock().unwrap_or_else(|p| p.into_inner());
    let mut settings = fs::read(path)
        .ok()
        .and_then(|data| crypto.decrypt(&data).ok())
        .and_then(|plain| serde_json::from_slice::<Settings>(&plain).ok())
        .unwrap_or_default();
    settings.auth_tokens = Some(tokens.clone());

    let write = || -> Result<(), TidalError> {
        let json = serde_json::to_string_pretty(&settings)?;
        let encrypted = crypto.encrypt(json.as_bytes())?;
        write_atomic(path, &encrypted)?;
        Ok(())
    };
    if let Err(e) = write() {
        log::warn!("Failed to persist refreshed auth tokens: {e}");
    }
}

/// The TIDAL session and its persistence, used by the API, auth and
/// Home-feed functions.
pub struct AppState {
    pub tidal_client: Mutex<TidalClient>,
    /// The one reqwest client every consumer shares.
    pub proxied_http: proxy_http::ProxiedHttp,
    pub settings_path: PathBuf,
    pub cache_dir: PathBuf,
    pub disk_cache: DiskCache,
    pub crypto: Arc<Crypto>,
}

impl AppState {
    /// Open (or create) the state under `config_dir`. The master key comes
    /// from the keyring, then `zeke.key`, or is generated (`crypto.rs`).
    pub fn new(config_dir: &Path) -> Result<Self, TidalError> {
        fs::create_dir_all(config_dir)?;
        let settings_path = config_dir.join("settings.json");
        let cache_dir = config_dir.join("cache");
        fs::create_dir_all(&cache_dir)?;

        let crypto = Arc::new(Crypto::new(config_dir)?);
        let disk_cache = DiskCache::new(&cache_dir, crypto.clone());

        let caps = proxy::HostCaps::assume_all_present();
        let proxied_http = proxy_http::ProxiedHttp::from_plan(&proxy::ProxyPlan::Direct, &caps);

        let mut tidal_client = TidalClient::new(proxied_http.clone());
        tidal_client.set_token_persist({
            let settings_path = settings_path.clone();
            let crypto = Arc::clone(&crypto);
            Arc::new(move |tokens: &AuthTokens| {
                persist_auth_tokens(&settings_path, &crypto, tokens);
            })
        });

        Ok(Self {
            tidal_client: Mutex::new(tidal_client),
            proxied_http,
            settings_path,
            cache_dir,
            disk_cache,
            crypto,
        })
    }

    pub fn load_settings(&self) -> Option<Settings> {
        let data = fs::read(&self.settings_path).ok()?;
        let plain = self.crypto.decrypt(&data).ok()?;
        let text = String::from_utf8(plain).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Load, change and save the settings as one step, so concurrent
    /// updates (and token refreshes) don't undo each other. Starts from the
    /// defaults when there is no file; refuses to overwrite one it can't
    /// read (that would log the user out). Returns what was saved.
    pub fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<Settings, TidalError> {
        let _guard = SETTINGS_WRITE.lock().unwrap_or_else(|p| p.into_inner());
        let mut settings = match self.load_settings() {
            Some(s) => s,
            None if !self.settings_path.exists() => Settings::default(),
            None => {
                return Err(TidalError::Crypto(format!(
                    "{} exists but can't be read; not overwriting it",
                    self.settings_path.display()
                )))
            }
        };
        change(&mut settings);
        self.save_settings(&settings)?;
        Ok(settings)
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), TidalError> {
        let json = serde_json::to_string_pretty(settings)?;
        let encrypted = self.crypto.encrypt(json.as_bytes())?;
        write_atomic(&self.settings_path, &encrypted)?;
        Ok(())
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    // Existing configs predate fields, so the serde defaults govern upgrades —
    // not just Settings::default(). Both must give the plan's §1.1 defaults.
    #[test]
    fn audio_defaults_are_normal_output_without_bit_perfect() {
        for s in [Settings::default(), serde_json::from_str::<Settings>("{}").unwrap()] {
            assert_eq!(s.output_device, None);
            assert!(s.bit_perfect_devices.is_empty());
            assert!(s.gapless);
            assert_eq!(s.max_quality, "HI_RES_LOSSLESS");
            assert_eq!(s.auth_method, AuthMethod::LoginCode);
            assert_eq!(s.color_scheme, ColorScheme::System);
        }
    }

    // The output fields of earlier versions are dropped, not carried over:
    // everyone starts on the system mixer.
    #[test]
    fn old_output_fields_are_ignored() {
        let json = r#"{
            "exclusive_mode": true,
            "exclusive_device": "hw:CARD=DAC,DEV=0",
            "bit_perfect": true
        }"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(s.output_device, None);
        assert!(s.bit_perfect_devices.is_empty());
    }

    #[test]
    fn bit_perfect_is_per_device() {
        let (dac, hdmi) = ("hw:CARD=DAC,DEV=0", "hw:CARD=HDMI,DEV=3");
        let mut s = Settings::default();
        s.set_bit_perfect_on(dac, true);
        s.set_bit_perfect_on(dac, true);
        assert!(s.bit_perfect_on(dac));
        assert!(!s.bit_perfect_on(hdmi));
        assert_eq!(s.bit_perfect_devices, [dac]);

        s.set_bit_perfect_on(hdmi, true);
        s.set_bit_perfect_on(dac, false);
        assert!(!s.bit_perfect_on(dac));
        assert!(s.bit_perfect_on(hdmi));
        assert_eq!(s.bit_perfect_devices, [hdmi]);
    }

    #[test]
    fn color_scheme_is_stored_in_lowercase() {
        let s = Settings { color_scheme: ColorScheme::Dark, ..Settings::default() };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["color_scheme"], "dark");
        let back: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(back.color_scheme, ColorScheme::Dark);
    }

    #[test]
    fn config_dir_is_zekes_own() {
        assert!(config_dir().ends_with("zeke"));
    }

    #[test]
    fn settings_round_trip_through_encryption() {
        let dir = tempfile::tempdir().unwrap();
        let crypto = Crypto::with_key([7u8; 32]);
        let path = dir.path().join("settings.json");
        let tokens = AuthTokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_in: 1,
            token_type: "Bearer".into(),
            user_id: Some(1),
        };
        persist_auth_tokens(&path, &crypto, &tokens);
        let raw = fs::read(&path).unwrap();
        assert!(crypto::is_encrypted(&raw), "settings must never be plaintext on disk");
        let back: Settings = serde_json::from_slice(&crypto.decrypt(&raw).unwrap()).unwrap();
        assert_eq!(back.auth_tokens.unwrap().refresh_token, "r");
    }
}
