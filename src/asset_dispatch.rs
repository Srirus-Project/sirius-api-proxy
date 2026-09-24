//! Background catalog observation and durable updater reconciliation.
use crate::{
    asset_jobs::{self, Operation, Request, Status},
    asset_outbox::{Identity, Outbox, State},
    client::GameClient,
    config::Config as GameConfig,
    error::AppError,
    resources::ResourceSnapshot,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::watch;
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub state_directory: PathBuf,
    pub interval_seconds: u64,
    pub request_timeout_ms: u64,
    pub history_capacity: usize,
    pub targets: Vec<Target>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub origin: String,
    pub token_env: String,
    #[serde(default)]
    pub allow_http: bool,
    pub profile: String,
    pub profile_revision: String,
    pub require_full_catalog: bool,
    pub require_full_export: bool,
    pub require_publication: bool,
}
impl Config {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.state_directory.as_os_str().is_empty()
            || !(1..=16).contains(&self.targets.len())
            || !(10..=86400).contains(&self.interval_seconds)
            || !(100..=300_000).contains(&self.request_timeout_ms)
            || !(1..=100_000).contains(&self.history_capacity)
        {
            return Err(AppError::Config("invalid asset dispatch bounds"));
        }
        let mut keys = std::collections::HashSet::new();
        for target in &self.targets {
            if target.token_env.is_empty()
                || target.token_env.len() > 256
                || !target
                    .token_env
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || !keys.insert((destination(&target.origin)?, target.profile.clone()))
            {
                return Err(AppError::Config(
                    "invalid or duplicate asset dispatch target",
                ));
            }
            asset_jobs::Client::new(
                &target.origin,
                "configuration-validation",
                target.allow_http,
                self.request_timeout_ms,
            )
            .map_err(|_| AppError::Config("invalid asset dispatch transport"))?;
            identity(
                target,
                &ResourceSnapshot {
                    schema_version: 2,
                    region: crate::region::Region::Jp,
                    environment: "validation".into(),
                    platform: "iOS",
                    client_version: "1".into(),
                    protocol_version: "1".into(),
                    master_version: None,
                    resource_version: "1".into(),
                    platform_hash: "hash".into(),
                    effective_cdn_root: String::new(),
                    credential_ref: String::new(),
                    observed_at: chrono::Utc::now(),
                    source: "remote",
                },
            )?
            .key()
            .map_err(|_| AppError::Config("invalid asset dispatch profile"))?;
        }
        Ok(())
    }
}
fn destination(origin: &str) -> Result<String, AppError> {
    let url =
        url::Url::parse(origin).map_err(|_| AppError::Config("invalid asset dispatch origin"))?;
    Ok(format!("{:x}", Sha256::digest(url.as_str().as_bytes())))
}
fn identity(target: &Target, s: &ResourceSnapshot) -> Result<Identity, AppError> {
    Ok(Identity {
        destination_sha256: destination(&target.origin)?,
        request: Request {
            region: s.region,
            profile: target.profile.clone(),
            operation: Operation::Update,
        },
        profile_revision: target.profile_revision.clone(),
        environment: s.environment.clone(),
        platform: s.platform.into(),
        resource_version: s.resource_version.clone(),
        platform_hash: s.platform_hash.clone(),
        require_full_catalog: target.require_full_catalog,
        require_full_export: target.require_full_export,
        require_publication: target.require_publication,
    })
}
struct Remote {
    config: Target,
    digest: String,
    client: asset_jobs::Client,
}
pub struct Worker {
    game: Arc<GameClient>,
    region: crate::region::Region,
    environment: String,
    platform: String,
    config: Config,
    outbox: Outbox,
    remotes: Vec<Remote>,
}
impl Worker {
    pub fn new(
        config: &GameConfig,
        game: Arc<GameClient>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let cfg = config
            .asset_dispatch
            .as_ref()
            .ok_or(AppError::Config("missing asset dispatch config"))?
            .clone();
        cfg.validate()?;
        let mut remotes = Vec::new();
        for target in &cfg.targets {
            let token = crate::config::secret(&target.token_env)?;
            remotes.push(Remote {
                digest: destination(&target.origin)?,
                client: asset_jobs::Client::new(
                    &target.origin,
                    &token,
                    target.allow_http,
                    cfg.request_timeout_ms,
                )?,
                config: target.clone(),
            });
        }
        let outbox = Outbox::open(&cfg.state_directory, cfg.history_capacity)?;
        if outbox.entries().values().any(|e| {
            e.identity.request.region != config.region
                || e.identity.environment != config.environment
                || e.identity.platform != config.platform().name()
        }) {
            return Err(AppError::Config(
                "asset dispatch state belongs to another region or environment",
            )
            .into());
        }
        Ok(Self {
            region: config.region,
            environment: config.environment.clone(),
            platform: config.platform().name().into(),
            game,
            config: cfg,
            outbox,
            remotes,
        })
    }
    /// Observation does not dispatch synchronously from any public/internal snapshot route.
    pub fn observe(
        &mut self,
        snapshot: &ResourceSnapshot,
    ) -> Result<(), crate::asset_outbox::Error> {
        if snapshot.region != self.region
            || snapshot.environment != self.environment
            || snapshot.platform != self.platform
        {
            return Err(crate::asset_outbox::Error::Invalid);
        }
        for remote in &self.remotes {
            let value = identity(&remote.config, snapshot)
                .map_err(|_| crate::asset_outbox::Error::Invalid)?;
            self.outbox.observe(value)?;
        }
        Ok(())
    }
    pub async fn reconcile(&mut self) -> Result<(), crate::asset_outbox::Error> {
        let entries: Vec<_> = self
            .outbox
            .entries()
            .iter()
            .filter(|(_, e)| !matches!(e.state, State::Completed { .. } | State::Failed { .. }))
            .take(16)
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        for (key, entry) in entries {
            let Some(remote) = self.remotes.iter().find(|r| {
                r.digest == entry.identity.destination_sha256
                    && r.config.profile == entry.identity.request.profile
            }) else {
                self.fail(&key, "target_removed")?;
                continue;
            };
            let result = match &entry.state {
                State::Pending => {
                    self.outbox.begin_send(&key)?;
                    remote.client.submit(&entry.identity.request, &key).await
                }
                State::Sending { .. } => {
                    // No remote retention guarantee exists: never replay an ambiguous POST automatically.
                    self.fail(&key, "submission_ambiguous")?;
                    continue;
                }
                State::Submitted { job_id } => {
                    remote.client.get(job_id, &entry.identity.request).await
                }
                _ => continue,
            };
            match result {
                Ok(job) => {
                    if matches!(entry.state, State::Pending) {
                        self.outbox.acknowledge(&key, &job.id)?;
                    }
                    match job.status {
                        Status::Failed => self.fail(&key, "job_failed")?,
                        Status::Cancelled => self.fail(&key, "job_cancelled")?,
                        Status::Completed => {
                            if let Some(outcome) =
                                job.outcome.filter(|o| matches_outcome(&entry.identity, o))
                            {
                                self.outbox.complete(
                                    &key,
                                    &job.id,
                                    &outcome.verification.catalog_sha256,
                                    outcome.publication_id,
                                )?;
                            } else {
                                self.fail(&key, "outcome_mismatch")?;
                            }
                        }
                        _ => {}
                    }
                }
                Err(asset_jobs::Error::Status(404))
                    if matches!(entry.state, State::Submitted { .. }) =>
                {
                    self.fail(&key, "job_pruned")?
                }
                Err(asset_jobs::Error::Protocol) => self.fail(&key, "invalid_job_response")?,
                Err(_) => {
                    tracing::warn!(
                        error_code = "asset_dispatch_transport",
                        "Asset job request failed; persisted state retained"
                    );
                }
            }
        }
        Ok(())
    }
    fn fail(&mut self, key: &str, code: &str) -> Result<(), crate::asset_outbox::Error> {
        self.outbox.fail(key, code)?;
        tracing::warn!(
            region = self.region.name(),
            error_code = code,
            "Asset dispatch requires operator reconciliation"
        );
        Ok(())
    }
    pub async fn run(mut self, mut stop: watch::Receiver<bool>) {
        loop {
            if *stop.borrow() {
                break;
            }
            let snapshot =
                tokio::select! {r=self.game.refresh_resource_snapshot()=>r,_=stop.changed()=>break};
            match snapshot {
                Ok(snapshot) => {
                    if self.observe(&snapshot).is_err() {
                        tracing::error!(
                            error_code = "asset_outbox_observe",
                            "Failed to persist asset observation"
                        );
                    }
                }
                Err(_) => tracing::warn!(
                    error_code = "asset_observation_failed",
                    "Asset version observation unavailable"
                ),
            }
            let result = tokio::select! {r=self.reconcile()=>r,_=stop.changed()=>break};
            if result.is_err() {
                tracing::error!(
                    error_code = "asset_outbox_storage",
                    "Asset dispatch stopped after persistence failure"
                );
                break;
            }
            tokio::select! {_=tokio::time::sleep(Duration::from_secs(self.config.interval_seconds))=>{},_=stop.changed()=>break}
        }
    }
}
fn matches_outcome(identity: &Identity, o: &asset_jobs::Outcome) -> bool {
    let v = &o.verification;
    v.region == identity.request.region
        && v.environment == identity.environment
        && v.platform == identity.platform
        && v.resource_version == identity.resource_version
        && v.platform_hash == identity.platform_hash
        && v.catalog_verified
        && (!identity.require_full_catalog || v.full_catalog)
        && (!identity.require_full_export
            || o.export
                .as_ref()
                .is_some_and(|e| e.full_export && (e.retained || o.publication_id.is_some())))
        && (!identity.require_publication || o.publication_id.is_some())
}
