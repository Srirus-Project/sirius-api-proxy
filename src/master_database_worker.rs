//! Retryable database publication independent of installed Master files and other consumers.
use crate::{
    client::GameClient, config::Config as GameConfig, error::AppError, master_database as db,
    master_registry::Scope,
};
use serde::Deserialize;
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub connection: db::Config,
    #[serde(default = "interval")]
    pub interval_seconds: u64,
}
fn interval() -> u64 {
    300
}
impl Config {
    pub fn validate(&self, game: &GameConfig) -> Result<(), AppError> {
        if game.region != crate::region::Region::Jp
            || game
                .master_directory
                .as_ref()
                .is_none_or(|p| p.as_os_str().is_empty())
            || !(10..=86400).contains(&self.interval_seconds)
        {
            return Err(AppError::Config(
                "invalid Master database publication configuration",
            ));
        }
        self.connection
            .validate()
            .map_err(|_| AppError::Config("invalid Master database connection configuration"))
    }
}
pub(crate) fn validate_tokens(configs: &[&GameConfig]) -> Result<(), AppError> {
    let mut protected = crate::master_notify::protected_tokens(configs);
    for c in configs {
        for target in c.master_notify.iter().flat_map(|n| &n.targets) {
            protected.push(crate::config::secret(&target.token_env)?);
        }
        if let Some(name) = c
            .master_git
            .as_ref()
            .and_then(|g| g.remote.as_ref())
            .and_then(|r| r.authorization_env.as_ref())
        {
            protected.extend(crate::master_git_worker::credential_parts(
                &crate::config::secret(name)?,
            )?);
        }
        if let Some(name) = c
            .master_git
            .as_ref()
            .and_then(|g| g.remote.as_ref())
            .and_then(|r| r.proxy_url_env.as_ref())
        {
            let url = url::Url::parse(&crate::config::secret(name)?)
                .map_err(|_| AppError::Config("invalid Git proxy configuration"))?;
            if let Some(password) = url.password() {
                let encoded = format!(
                    "password={}",
                    password.replace('+', "%2B").replace('&', "%26")
                );
                let (_, decoded) = url::form_urlencoded::parse(encoded.as_bytes())
                    .next()
                    .ok_or(AppError::Config("invalid Git proxy configuration"))?;
                protected.push(decoded.into_owned());
            }
        }
    }
    for value in protected.clone() {
        let header = if value.starts_with("Authorization: ") {
            value
        } else {
            format!("Authorization: {value}")
        };
        if let Ok(parts) = crate::master_git_worker::credential_parts(&header) {
            protected.extend(parts);
        }
    }
    for c in configs {
        if let Some(database) = &c.master_database {
            database.connection.options().map_err(|_| {
                AppError::Config(
                    "Master database credentials or transport configuration unavailable",
                )
            })?;
            let password = crate::config::secret(&database.connection.password_env)?;
            if protected.contains(&password) {
                return Err(AppError::Config("Master database password must be distinct from API/game/CDN/peer/notification/Git credentials"));
            }
        }
    }
    Ok(())
}
pub struct Worker {
    config: Config,
    source: PathBuf,
    scope: Scope,
    game: Arc<GameClient>,
    last_success: Option<db::Receipt>,
}
impl Worker {
    pub fn new(config: &GameConfig, game: Arc<GameClient>) -> Result<Self, AppError> {
        config.validate()?;
        validate_tokens(&[config])?;
        Ok(Self {
            config: config
                .master_database
                .clone()
                .ok_or(AppError::Config("Master database is not configured"))?,
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
    pub async fn update_once(&mut self) -> Result<db::Receipt, db::Error> {
        self.game
            .record_master_database(json!({"status":"running","last_success":self.last_success}))
            .await;
        let result = db::publish(&self.config.connection, &self.source, self.scope.clone()).await;
        match &result {
            Ok(receipt) => {
                self.last_success = Some(receipt.clone());
                self.game.record_master_database(json!({"status":"ready","checked_at":chrono::Utc::now(),"last_success":receipt})).await;
            }
            Err(error) => {
                let code = match error {
                    db::Error::Config => "configuration",
                    db::Error::Secret => "secret_unavailable",
                    db::Error::Snapshot => "snapshot",
                    db::Error::Database => "database_operation",
                    db::Error::Timeout => "timeout",
                    db::Error::Integrity => "integrity",
                };
                self.game.record_master_database(json!({"status":"failed","checked_at":chrono::Utc::now(),"error_code":code,"last_success":self.last_success})).await;
                tracing::warn!(
                    error_code = code,
                    "Master database publication failed; installed Master is unchanged"
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
            tokio::select! { biased; _=stop.changed()=>break, _=self.game.master_database_notified()=>{}, _=tokio::time::sleep(Duration::from_secs(self.config.interval_seconds))=>{} }
        }
        self.game
            .record_master_database(json!({"status":"stopped","last_success":self.last_success}))
            .await;
    }
}
