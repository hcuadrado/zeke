//! HTTP-client stub: one shared `reqwest::Client` behind the
//! `ProxiedHttp` interface `tidal_api.rs` uses.

use crate::proxy::{BlockReason, HostCaps, ProxyPlan};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// A generation for writers to the shared cell. The cell is only ever
/// written by `block`, so there is nothing to order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation;

#[derive(Clone)]
pub struct ProxiedHttp {
    cell: Arc<RwLock<Result<reqwest::Client, BlockReason>>>,
}

impl ProxiedHttp {
    pub fn from_plan(p: &ProxyPlan, _env: &HostCaps) -> Self {
        let state = match p {
            ProxyPlan::Direct => reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| BlockReason::new(format!("could not build HTTP client: {e}"))),
        };
        Self {
            cell: Arc::new(RwLock::new(state)),
        }
    }

    pub fn client(&self) -> Result<reqwest::Client, BlockReason> {
        self.cell
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn client_at(&self) -> (Generation, Result<reqwest::Client, BlockReason>) {
        (Generation, self.client())
    }

    /// Would count unanswered requests to report an unreachable proxy.
    pub fn observe_at<T>(
        &self,
        _generation: Generation,
        outcome: Result<T, reqwest::Error>,
    ) -> Result<T, reqwest::Error> {
        outcome
    }

    /// Block the cell outright: every later `client()` returns `Err`.
    pub fn block(&self, cause: impl Into<String>) {
        *self
            .cell
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Err(BlockReason::new(cause));
    }
}
