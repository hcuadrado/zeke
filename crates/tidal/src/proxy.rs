//! Proxy stub. Zeke has no proxy support, but `audio.rs` and
//! `tidal_api.rs` reach the proxy only through these items, so keeping
//! their names and shapes keeps those call sites simple. Every plan
//! is `Direct` and every route is `NoProxy`; `Via` and `Creds` exist only
//! because `audio.rs` matches on them.

use crate::ProxySettings;

#[derive(Clone, PartialEq, Eq)]
pub struct Creds {
    pub user: String,
    pub pass: String,
}

// Hand-written so a stray `{route:?}` log never prints a password.
impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Creds")
            .field("user", &self.user)
            .field("pass", &"***")
            .finish()
    }
}

/// Host facts read from the GStreamer registry (`audio::probe_host_caps`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCaps {
    pub has_dashdemux: bool,
    pub has_curlhttpsrc: bool,
    pub gst_version: (u32, u32, u32),
}

impl HostCaps {
    /// Test-only stand-in for host facts; production reads the registry.
    #[doc(hidden)]
    pub fn assume_all_present() -> Self {
        Self {
            has_dashdemux: true,
            has_curlhttpsrc: true,
            gst_version: (1, 26, 10),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyPlan {
    Direct,
}

/// Why settings form no plan. Uninhabited: the stub always plans `Direct`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {}

impl std::fmt::Display for PlanError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {}
    }
}

/// Which consumer is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// GStreamer progressive HTTP (lossy).
    Lossy,
    /// GStreamer DASH segments (lossless/hi-res).
    Dash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Proceed with the system's own configuration.
    NoProxy,
    /// Never constructed in Zeke.
    Via { uri: String, creds: Option<Creds> },
}

/// Why a capability cannot be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockReason {
    pub cause: String,
}

impl BlockReason {
    pub fn new(cause: impl Into<String>) -> Self {
        Self {
            cause: cause.into(),
        }
    }
}

impl std::fmt::Display for BlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.cause)
    }
}

impl ProxyPlan {
    pub fn route(&self, _c: Capability, _env: &HostCaps) -> Result<Route, BlockReason> {
        match self {
            ProxyPlan::Direct => Ok(Route::NoProxy),
        }
    }
}

/// Always `Direct`: the proxy settings are kept in `Settings` but unused.
pub fn plan(_s: &ProxySettings, _env: &HostCaps) -> Result<ProxyPlan, PlanError> {
    Ok(ProxyPlan::Direct)
}

/// Whether `no_proxy` was set at launch. Zeke never proxies.
pub fn launch_bypass_was_set() -> bool {
    false
}
