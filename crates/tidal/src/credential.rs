//! The session's credential as others see it: the access token and what goes
//! with it, never the refresh token.

/// The access token, the country TIDAL assigned the session and the user.
/// A snapshot: `TidalClient::credential_watch` hands out a new one whenever
/// the session changes.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub access_token: String,
    pub country_code: String,
    pub user_id: Option<u64>,
    /// When the access token expires, in unix seconds.
    pub expires_at: u64,
}

// Hand-written so a stray `{:?}` never prints the token.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("access_token", &"***")
            .field("country_code", &self.country_code)
            .field("user_id", &self.user_id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}
