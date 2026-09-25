use serde::Serialize;

/// Structured error type for all backend operations.
#[derive(Debug, thiserror::Error, Serialize)]
#[serde(tag = "kind", content = "message")]
pub enum TidalError {
    /// HTTP API returned a non-success status.
    #[error("API error ({status}): {body}")]
    Api { status: u16, body: String },

    /// JSON deserialization or other parse failure.
    #[error("Parse error: {0}")]
    Parse(String),

    /// Network/transport failure (timeout, DNS, connection refused).
    #[error("Network error: {0}")]
    Network(String),

    /// No auth tokens available (user not logged in).
    #[error("Not authenticated")]
    NotAuthenticated,

    /// Client ID / secret not configured.
    #[error("Not configured: {0}")]
    NotConfigured(String),

    /// File system / IO error.
    #[error("IO error: {0}")]
    Io(String),

    /// GStreamer / audio pipeline error.
    #[error("Audio error: {0}")]
    Audio(String),

    /// Encryption / decryption failure.
    #[error("Crypto error: {0}")]
    Crypto(String),

    /// Scrobbling service error.
    #[error("Scrobble error: {0}")]
    Scrobble(String),

    #[error("MCP error: {0}")]
    Mcp(String),

    /// The configured proxy cannot serve this request; nothing was sent.
    /// Never a transport failure — the request never left the process.
    #[error("Proxy blocked: {reason}")]
    ProxyBlocked { reason: String },
}

impl TidalError {
    /// Returns true if this is a network/transport error.
    pub fn is_network(&self) -> bool {
        matches!(self, TidalError::Network(_))
    }

    /// Upstream is rate-limiting us. Never retry in a loop — a 429 is usually
    /// self-inflicted, so the fix is to stop asking.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, TidalError::Api { status: 429, .. })
    }

    /// This specific item cannot be played and no retry will change that.
    /// 404/410/451 are catalog/licensing terminal; a 401 is terminal only when
    /// its body carries a terminal playbackinfo sub-status.
    pub fn is_terminal_unplayable(&self) -> bool {
        match self {
            TidalError::Api {
                status: 404 | 410 | 451,
                ..
            } => true,
            TidalError::Api { status: 401, body } => crate::tidal_api::is_terminal_sub_status(body),
            _ => false,
        }
    }

    /// The session is gone and only a new login brings it back: no tokens,
    /// a refresh TIDAL refused (`invalid_grant`), or a 401 that survived the
    /// refresh. A 401 carrying a playbackinfo sub-status is about the track,
    /// not the session.
    pub fn is_auth_expired(&self) -> bool {
        match self {
            TidalError::NotAuthenticated => true,
            TidalError::Api { status: 401, body } => !crate::tidal_api::is_playbackinfo_sub_status(body),
            TidalError::Api { status: 400, body } => body.contains("invalid_grant"),
            _ => false,
        }
    }

    /// A log-safe message that omits API response bodies (which may carry
    /// account data for `/users/` and `/sessions` endpoints). Logs only the
    /// status for API errors; other variants carry no server response body.
    pub fn log_safe(&self) -> String {
        match self {
            TidalError::Api { status, .. } => format!("API error (status {status})"),
            other => other.to_string(),
        }
    }
}

impl From<std::io::Error> for TidalError {
    fn from(e: std::io::Error) -> Self {
        TidalError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for TidalError {
    fn from(e: serde_json::Error) -> Self {
        TidalError::Parse(e.to_string())
    }
}

impl From<reqwest::Error> for TidalError {
    fn from(e: reqwest::Error) -> Self {
        TidalError::Network(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::TidalError;

    #[test]
    fn expired_sessions_are_told_apart() {
        let api = |status, body: &str| TidalError::Api { status, body: body.into() };
        assert!(TidalError::NotAuthenticated.is_auth_expired());
        assert!(api(401, r#"{"status":401,"subStatus":11002,"userMessage":"Token has expired"}"#).is_auth_expired());
        assert!(api(400, r#"{"status":400,"error":"invalid_grant","sub_status":11101}"#).is_auth_expired());
        assert!(!api(401, r#"{"status":401,"subStatus":4005}"#).is_auth_expired(), "a track, not the session");
        assert!(!api(400, r#"{"status":400,"error":"invalid_request"}"#).is_auth_expired());
        assert!(!api(404, "").is_auth_expired());
        assert!(!TidalError::Network("timed out".into()).is_auth_expired());
    }
}
