//! Bounded owner-to-consumer wakeup delivery. Acceptance is not synchronization success.
use crate::{master_registry::Scope, master_sync::UpdateHint};
use reqwest::{header, StatusCode};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Master notification destination or hint")]
    Config,
    #[error("Master notification delivery failed")]
    Delivery,
}

/// One consumer's delivery state. A restart may resend the current hint safely.
/// Failed requests do not advance the acknowledgement, so the caller can retry.
pub struct Target {
    http: reqwest::Client,
    url: reqwest::Url,
    authorization: header::HeaderValue,
    scope: Scope,
    accepted: Option<String>,
}
impl Target {
    pub fn new(
        origin: &str,
        token: &str,
        scope: Scope,
        regional_paths: bool,
        allow_http: bool,
        timeout_ms: u64,
    ) -> Result<Self, Error> {
        if scope.region != crate::region::Region::Jp || !(100..=30_000).contains(&timeout_ms) {
            return Err(Error::Config);
        }
        // Use the same strict origin/token rules as the peer transport.
        crate::peer_transport::Client::new(
            origin,
            token,
            scope.region,
            regional_paths,
            allow_http,
            crate::peer_transport::Policy {
                connect_timeout_ms: timeout_ms.min(5_000),
                request_timeout_ms: timeout_ms,
                ..Default::default()
            },
        )
        .map_err(|_| Error::Config)?;
        let mut url = reqwest::Url::parse(origin).map_err(|_| Error::Config)?;
        url.set_path(&if regional_paths {
            format!("/internal/v1/{}/master-data/sync", scope.region.name())
        } else {
            "/internal/v1/master-data/sync".into()
        });
        let mut authorization =
            header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::Config)?;
        authorization.set_sensitive(true);
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_millis(timeout_ms.min(5_000)))
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .map_err(|_| Error::Config)?;
        Ok(Self {
            http,
            url,
            authorization,
            scope,
            accepted: None,
        })
    }
    /// Returns false when this content was already acknowledged. Callers must
    /// obtain the hash from committed CURRENT, never an uncommitted staging tree.
    pub async fn deliver(&mut self, content_sha256: &str) -> Result<bool, Error> {
        if content_sha256.len() != 64 || !content_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Config);
        }
        if self.accepted.as_deref() == Some(content_sha256) {
            return Ok(false);
        }
        let hint = UpdateHint {
            scope: self.scope.clone(),
            content_sha256: content_sha256.into(),
        };
        let mut response = self
            .http
            .post(self.url.clone())
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&hint).map_err(|_| Error::Config)?)
            .send()
            .await
            .map_err(|_| Error::Delivery)?;
        if response.status() != StatusCode::ACCEPTED
            || response.content_length().is_some_and(|n| n > 1024)
        {
            return Err(Error::Delivery);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Delivery)? {
            if bytes.len().saturating_add(chunk.len()) > 1024 {
                return Err(Error::Delivery);
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            status: String,
        }
        let reply: Reply = serde_json::from_slice(&bytes).map_err(|_| Error::Delivery)?;
        if reply.status != "accepted" {
            return Err(Error::Delivery);
        }
        self.accepted = Some(content_sha256.into());
        Ok(true)
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub targets: Vec<TargetConfig>,
    #[serde(default = "interval")]
    pub interval_seconds: u64,
    #[serde(default = "timeout")]
    pub request_timeout_ms: u64,
}
fn interval() -> u64 {
    30
}
fn timeout() -> u64 {
    5000
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub name: String,
    pub origin: String,
    pub token_env: String,
    #[serde(default)]
    pub regional_paths: bool,
    #[serde(default)]
    pub allow_http: bool,
}
impl Config {
    pub fn validate(&self, config: &crate::config::Config) -> Result<(), crate::error::AppError> {
        let bad = || crate::error::AppError::Config("invalid Master notification configuration");
        if config.region != crate::region::Region::Jp
            || config
                .master_directory
                .as_ref()
                .is_none_or(|p| p.as_os_str().is_empty())
            || self.targets.is_empty()
            || self.targets.len() > 16
            || !(10..=3600).contains(&self.interval_seconds)
            || !(100..=30_000).contains(&self.request_timeout_ms)
        {
            return Err(bad());
        }
        let mut names = std::collections::BTreeSet::new();
        for target in &self.targets {
            if target.name.is_empty()
                || target.name.len() > 64
                || !target
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || !names.insert(&target.name)
                || target.token_env.is_empty()
                || target.token_env.len() > 256
                || !target
                    .token_env
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(bad());
            }
            Target::new(
                &target.origin,
                "validation",
                scope(config),
                target.regional_paths,
                target.allow_http,
                self.request_timeout_ms,
            )
            .map_err(|_| bad())?;
        }
        Ok(())
    }
}
fn scope(config: &crate::config::Config) -> Scope {
    Scope {
        region: config.region,
        environment: config.environment.clone(),
        platform: config.platform(),
    }
}
/// Check all deployment profiles before any background task can transmit credentials.
pub(crate) fn validate_tokens(
    configs: &[&crate::config::Config],
) -> Result<(), crate::error::AppError> {
    let protected = protected_tokens(configs);
    let mut outgoing = Vec::new();
    for c in configs {
        for target in c.master_notify.iter().flat_map(|n| &n.targets) {
            let token = crate::config::secret(&target.token_env)?;
            if protected.contains(&token)
                || outgoing
                    .iter()
                    .any(|(region, value)| *region != c.region && value == &token)
            {
                return Err(crate::error::AppError::Config("Master notification tokens must be region-scoped and distinct from local API/internal/peer/game/CDN/sync/updater credentials"));
            }
            outgoing.push((c.region, token));
        }
    }
    Ok(())
}

pub struct Worker {
    targets: Vec<(String, Target)>,
    directory: std::path::PathBuf,
    scope: Scope,
    interval: Duration,
    game: std::sync::Arc<crate::client::GameClient>,
}
impl Worker {
    pub fn new(
        config: &crate::config::Config,
        game: std::sync::Arc<crate::client::GameClient>,
    ) -> Result<Self, crate::error::AppError> {
        config.validate()?;
        validate_tokens(&[config])?;
        let policy = config
            .master_notify
            .as_ref()
            .ok_or(crate::error::AppError::Config(
                "Master notifications are not configured",
            ))?;
        let mut targets = Vec::new();
        for target in &policy.targets {
            targets.push((
                target.name.clone(),
                Target::new(
                    &target.origin,
                    &crate::config::secret(&target.token_env)?,
                    scope(config),
                    target.regional_paths,
                    target.allow_http,
                    policy.request_timeout_ms,
                )
                .map_err(|_| {
                    crate::error::AppError::Config(
                        "invalid Master notification destination or token",
                    )
                })?,
            ));
        }
        Ok(Self {
            targets,
            directory: config
                .master_directory
                .clone()
                .expect("validated directory"),
            scope: scope(config),
            interval: Duration::from_secs(policy.interval_seconds),
            game,
        })
    }
    /// Reconcile the durable committed pointer; never modify publication state.
    /// Try every target even if another target fails. Successful targets deduplicate.
    pub async fn reconcile(&mut self) -> Result<usize, Error> {
        let directory = self.directory.clone();
        let scope = self.scope.clone();
        let hash = tokio::task::spawn_blocking(move || {
            let document = crate::master_registry::manifest(&directory, None, scope)
                .map_err(|_| Error::Delivery)?;
            let manifest: crate::master_registry::PublishedManifest =
                serde_json::from_slice(&document.bytes).map_err(|_| Error::Delivery)?;
            Ok::<_, Error>(manifest.content_sha256)
        })
        .await
        .map_err(|_| Error::Delivery)??;
        let mut delivered = 0;
        let mut failed = false;
        for (name, target) in &mut self.targets {
            match target.deliver(&hash).await {
                Ok(sent) => delivered += usize::from(sent),
                Err(_) => {
                    failed = true;
                    tracing::warn!(target_name = %name, "Master notification was not accepted");
                }
            }
        }
        if failed {
            Err(Error::Delivery)
        } else {
            Ok(delivered)
        }
    }
    pub async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                result = self.reconcile() => {
                    if result.is_err() { tracing::warn!("Master notification reconciliation incomplete; retrying on next interval"); }
                }
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = self.game.master_publication_notified() => {},
                _ = tokio::time::sleep(self.interval) => {},
            }
        }
    }
}

pub(crate) fn protected_tokens(configs: &[&crate::config::Config]) -> Vec<String> {
    let mut protected = Vec::new();
    for c in configs {
        let mut names: Vec<&String> = vec![&c.api_token_env, &c.internal_token_env];
        names.extend(c.peer_token_env.iter());
        names.extend(c.player_credential_env.iter());
        names.extend(c.cdn_credential_env.values());
        names.extend(c.accounts.iter().filter_map(|a| a.credential_env.as_ref()));
        names.extend(c.master_sync.iter().map(|s| &s.token_env));
        names.extend(
            c.asset_dispatch
                .iter()
                .flat_map(|d| d.targets.iter().map(|t| &t.token_env)),
        );
        names.extend(
            c.node_routing
                .iter()
                .flat_map(|d| d.targets.iter().map(|t| &t.token_env)),
        );
        names.extend(c.upstream.proxy_authorization_env.iter());
        if let Some(update) = &c.master_update {
            names.extend([
                &update.username_env,
                &update.key_hex_env,
                &update.iv_hex_env,
            ]);
            names.extend(update.network.proxy_authorization_env.iter());
        }
        protected.extend(names.into_iter().filter_map(|n| std::env::var(n).ok()));
    }
    protected
}
