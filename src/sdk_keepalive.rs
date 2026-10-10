//! Background SDK keepalive for Global identities (see [`crate::client::GameClient::sdk_keepalive`]).
//!
//! The SDK `access_key` lapses 30 days after the identity's last `cache.login`, and a region
//! whose game session stays up never sends one. Every check interval this worker revalidates
//! the identities whose last successful `cache.login` is `global_login.sdk_keepalive_seconds`
//! old. The first check runs one interval after startup, so startup still sends nothing.
use crate::client::GameClient;
use std::{sync::Arc, time::Duration};

/// Time between two keepalive checks.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(3_600);

pub struct Worker {
    client: Arc<GameClient>,
    interval: Duration,
}
impl Worker {
    /// A worker for a region with Global accounts, or None when the region has none.
    pub fn for_client(client: Arc<GameClient>) -> Option<Self> {
        client.has_global_accounts().then_some(Self {
            client,
            interval: CHECK_INTERVAL,
        })
    }
    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = tokio::time::sleep(self.interval) => {},
            }
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = self.client.sdk_keepalive() => {},
            }
        }
    }
}
