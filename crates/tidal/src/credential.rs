//! The session's credential as others see it: the access token and what goes
//! with it, never the refresh token. A `CredentialSource` hands it out and
//! refreshes it ahead of expiry.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{watch, Mutex};

use crate::tidal_api::unix_now;
use crate::TidalError;

/// `fresh` refreshes a token that has this many seconds left, or fewer.
pub const REFRESH_AHEAD_SECS: u64 = 5 * 60;

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

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What refreshes the session's tokens: `AppState`, over the TIDAL client.
pub trait Refresh: Send + Sync {
    /// Refresh the tokens, unless the access token is no longer `stale`
    /// (someone refreshed it already). The new ones reach the watch.
    fn refresh<'a>(&'a self, stale: &'a str) -> BoxFuture<'a, Result<(), TidalError>>;
}

/// The session's credential, for code outside the TIDAL client that needs
/// the access token itself. Clones share one watch and one refresh.
#[derive(Clone)]
pub struct CredentialSource {
    watch: watch::Receiver<Option<Credential>>,
    refresher: Arc<dyn Refresh>,
    /// Held while a refresh is under way, so callers queue behind it.
    flight: Arc<Mutex<()>>,
}

impl std::fmt::Debug for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSource").finish_non_exhaustive()
    }
}

impl CredentialSource {
    /// Every source over the same session must share `flight`.
    pub fn new(watch: watch::Receiver<Option<Credential>>, refresher: Arc<dyn Refresh>, flight: Arc<Mutex<()>>) -> Self {
        Self { watch, refresher, flight }
    }

    /// The credential now; `None` while signed out.
    pub fn current(&self) -> Option<Credential> {
        self.watch.borrow().clone()
    }

    /// A receiver that changes on login, refresh, logout and account change.
    pub fn watch(&self) -> watch::Receiver<Option<Credential>> {
        self.watch.clone()
    }

    /// The credential, refreshed first when its token has
    /// `REFRESH_AHEAD_SECS` or less to live: what to hand to something that
    /// will use the token for a while.
    pub async fn fresh(&self) -> Result<Credential, TidalError> {
        let now = self.current().ok_or(TidalError::NotAuthenticated)?;
        if now.expires_at > unix_now().saturating_add(REFRESH_AHEAD_SECS) {
            return Ok(now);
        }
        self.refresh_now(&now).await
    }

    /// Refresh because `failed` was refused. One refresh at a time: a caller
    /// whose token was replaced while it waited gets the new one, with no
    /// second refresh.
    pub async fn refresh_now(&self, failed: &Credential) -> Result<Credential, TidalError> {
        let _flight = self.flight.lock().await;
        let now = self.current().ok_or(TidalError::NotAuthenticated)?;
        if now.access_token != failed.access_token {
            return Ok(now);
        }
        self.refresher.refresh(&failed.access_token).await?;
        self.current().ok_or(TidalError::NotAuthenticated)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::tidal_api::token_tests::{client, tokens};
    use crate::tidal_api::{Stamp, TidalClient};

    /// Refreshes by handing the client new tokens after a pause, and counts.
    struct FakeTidal {
        client: std::sync::Mutex<TidalClient>,
        refreshes: AtomicUsize,
    }

    impl Refresh for FakeTidal {
        fn refresh<'a>(&'a self, _stale: &'a str) -> BoxFuture<'a, Result<(), TidalError>> {
            Box::pin(async move {
                let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
                let mut client = self.client.lock().unwrap();
                let user = client.tokens().and_then(|t| t.user_id).unwrap_or(0);
                client.set_tokens(Some(tokens(&format!("refreshed-{n}"), user, 0)), Stamp::Now);
                Ok(())
            })
        }
    }

    /// A source over a session whose token was issued `age` seconds ago
    /// (it lives an hour), restored as the saved session is.
    fn session(age: u64) -> (Arc<FakeTidal>, CredentialSource) {
        let mut c = client();
        c.set_tokens(Some(tokens("saved", 7, unix_now() - age)), Stamp::Keep);
        c.set_country_code("NO");
        let watch = c.credential_watch();
        let fake = Arc::new(FakeTidal { client: std::sync::Mutex::new(c), refreshes: AtomicUsize::new(0) });
        let source = CredentialSource::new(watch, fake.clone(), Arc::default());
        (fake, source)
    }

    #[test]
    fn it_hands_out_the_token_country_and_user() {
        let (_, source) = session(0);
        let c = source.current().unwrap();
        assert_eq!((c.access_token.as_str(), c.country_code.as_str(), c.user_id), ("saved", "NO", Some(7)));
    }

    #[tokio::test]
    async fn fresh_keeps_a_token_with_time_left() {
        // 10 minutes left.
        let (fake, source) = session(3600 - 600);
        assert_eq!(source.fresh().await.unwrap().access_token, "saved");
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fresh_refreshes_a_token_within_five_minutes_of_expiry() {
        // 4 minutes left.
        let (fake, source) = session(3600 - 240);
        let mut watch = source.watch();
        watch.borrow_and_update();
        assert_eq!(source.fresh().await.unwrap().access_token, "refreshed-1");
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
        assert!(watch.has_changed().unwrap(), "the refresh reached the watch");
        // Fresh now: no second refresh.
        assert_eq!(source.fresh().await.unwrap().access_token, "refreshed-1");
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fresh_refreshes_a_restored_session_with_an_old_stamp() {
        let (fake, source) = session(0);
        {
            let mut c = fake.client.lock().unwrap();
            // A token file from before obtained_at existed.
            let old = tokens("saved", 7, 0);
            c.set_tokens(Some(old), Stamp::Keep);
        }
        assert_eq!(source.fresh().await.unwrap().access_token, "refreshed-1");
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn two_concurrent_refreshes_make_one() {
        let (fake, source) = session(0);
        let failed = source.current().unwrap();
        let other = source.clone();
        let (a, b) = tokio::join!(source.refresh_now(&failed), other.refresh_now(&failed));
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(a.unwrap().access_token, "refreshed-1");
        assert_eq!(b.unwrap().access_token, "refreshed-1");
        // A late caller with the old token gets the new one, no refresh.
        assert_eq!(source.refresh_now(&failed).await.unwrap().access_token, "refreshed-1");
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn signed_out_there_is_nothing_to_hand_out() {
        let (fake, source) = session(0);
        let failed = source.current().unwrap();
        fake.client.lock().unwrap().set_tokens(None, Stamp::Keep);
        assert!(source.current().is_none());
        assert!(matches!(source.fresh().await, Err(TidalError::NotAuthenticated)));
        assert!(matches!(source.refresh_now(&failed).await, Err(TidalError::NotAuthenticated)));
        assert_eq!(fake.refreshes.load(Ordering::SeqCst), 0);
    }
}
