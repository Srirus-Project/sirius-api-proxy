//! Configured local/remote Master Git publication independent of snapshot installation.
use crate::{
    client::GameClient, config::Config as GameConfig, error::AppError, master_git,
    master_registry::Scope,
};
use serde::Deserialize;
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub commit: master_git::CommitPolicy,
    pub state_directory: PathBuf,
    #[serde(default = "interval")]
    pub interval_seconds: u64,
    pub remote: Option<master_git::Remote>,
}
fn interval() -> u64 {
    300
}
impl Config {
    pub fn validate(&self, game: &GameConfig) -> Result<(), AppError> {
        self.commit
            .validate()
            .map_err(|_| AppError::Config("invalid Master Git commit policy"))?;
        if !cfg!(unix)
            || game.region != crate::region::Region::Jp
            || self.state_directory.as_os_str().is_empty()
            || game
                .master_directory
                .as_ref()
                .is_none_or(|p| p.as_os_str().is_empty() || p == &self.state_directory)
            || !(10..=86400).contains(&self.interval_seconds)
        {
            return Err(AppError::Config(
                "invalid Master Git publication configuration",
            ));
        }
        if let Some(remote) = &self.remote {
            remote
                .validate()
                .map_err(|_| AppError::Config("invalid Master Git remote configuration"))?;
        }
        Ok(())
    }
}
pub(crate) fn credential_parts(value: &str) -> Result<Vec<String>, AppError> {
    let bad = || AppError::Config("invalid Master Git authorization");
    let mut parts = vec![value.to_owned()];
    if let Some(token) = value.strip_prefix("Authorization: Bearer ") {
        parts.push(token.into());
    } else if let Some(token) = value.strip_prefix("Authorization: Basic ") {
        use base64::Engine;
        parts.push(token.into());
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(token)
            .map_err(|_| bad())?;
        let decoded = String::from_utf8(decoded).map_err(|_| bad())?;
        let (_, password) = decoded.split_once(':').ok_or_else(bad)?;
        if password.is_empty() {
            return Err(bad());
        }
        parts.push(password.into());
        parts.push(decoded);
    } else {
        return Err(bad());
    }
    Ok(parts)
}
/// Compare underlying Bearer/Basic material too, not just the full header string.
pub(crate) fn validate_tokens(configs: &[&GameConfig]) -> Result<(), AppError> {
    let mut protected = crate::master_notify::protected_tokens(configs);
    for c in configs {
        for t in c.master_notify.iter().flat_map(|n| &n.targets) {
            protected.push(crate::config::secret(&t.token_env)?);
        }
    }
    let mut seen: Vec<(crate::region::Region, Vec<String>)> = Vec::new();
    for c in configs {
        if let Some(name) = c
            .master_git
            .as_ref()
            .and_then(|g| g.remote.as_ref())
            .and_then(|r| r.authorization_env.as_ref())
        {
            let parts = credential_parts(&crate::config::secret(name)?)?;
            if parts.iter().any(|p| protected.contains(p))
                || seen.iter().any(|(region, previous)| {
                    *region != c.region && parts.iter().any(|p| previous.contains(p))
                })
            {
                return Err(AppError::Config(
                    "Master Git credentials must be distinct from all other service scopes",
                ));
            }
            seen.push((c.region, parts));
        }
    }
    Ok(())
}
pub struct Worker {
    config: Config,
    source: PathBuf,
    scope: Scope,
    game: Arc<GameClient>,
    last_success: Option<master_git::Receipt>,
}
impl Worker {
    pub fn new(config: &GameConfig, game: Arc<GameClient>) -> Result<Self, AppError> {
        config.validate()?;
        validate_tokens(&[config])?;
        Ok(Self {
            config: config
                .master_git
                .clone()
                .ok_or(AppError::Config("Master Git is not configured"))?,
            source: config
                .master_directory
                .clone()
                .ok_or(AppError::Config("Master directory is required"))?,
            scope: Scope {
                region: config.region,
                environment: config.environment.clone(),
                platform: config.platform(),
            },
            game,
            last_success: None,
        })
    }
    pub async fn update_once(&mut self) -> Result<master_git::Receipt, master_git::Error> {
        self.game
            .record_master_git(json!({"status":"running","last_success":self.last_success}))
            .await;
        let result = match &self.config.remote {
            Some(remote) => {
                master_git::publish_with_policy(
                    &self.source,
                    &self.config.state_directory,
                    self.scope.clone(),
                    remote,
                    &self.config.commit,
                )
                .await
            }
            None => {
                master_git::commit_with_policy(
                    &self.source,
                    &self.config.state_directory,
                    self.scope.clone(),
                    &self.config.commit,
                )
                .await
            }
        };
        match &result {
            Ok(receipt) => {
                self.last_success = Some(receipt.clone());
                self.game.record_master_git(json!({"status":"ready","checked_at":chrono::Utc::now(),"last_success":receipt})).await;
            }
            Err(error) => {
                let code = match error {
                    master_git::Error::CommitConfig => "commit_config",
                    master_git::Error::Ownership => "ownership",
                    master_git::Error::Locked => "locked",
                    master_git::Error::Snapshot => "snapshot",
                    master_git::Error::RemoteConfig => "remote_config",
                    master_git::Error::RemoteChanged => "remote_history",
                    master_git::Error::Git => "git_operation",
                };
                self.game.record_master_git(json!({"status":"failed","checked_at":chrono::Utc::now(),"error_code":code,"last_success":self.last_success})).await;
                tracing::warn!(
                    error_code = code,
                    "Master Git publication failed; installed Master is unchanged"
                );
            }
        }
        result
    }
    pub async fn run(mut self, mut stop: tokio::sync::watch::Receiver<bool>) {
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! { biased; _=stop.changed()=>break, _=self.update_once()=>{} }
            tokio::select! { biased; _=stop.changed()=>break, _=self.game.master_git_notified()=>{}, _=tokio::time::sleep(Duration::from_secs(self.config.interval_seconds))=>{} }
        }
        self.game
            .record_master_git(json!({"status":"stopped","last_success":self.last_success}))
            .await;
    }
}
