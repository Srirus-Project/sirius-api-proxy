//! Standalone owner synchronization and optional transactional database publication.
use crate::{error::AppError, master_database as db, master_registry::Scope, master_sync};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{Notify, RwLock};
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub source: master_sync::Config,
    pub internal_token_env: String,
    /// Required for PostgreSQL; file backends use their serving directory.
    pub staging_directory: Option<PathBuf>,
}
pub struct Worker {
    sync: Arc<master_sync::Syncer>,
    scope: Scope,
    directory: PathBuf,
    database: Option<db::Config>,
    interval: Duration,
    wake: Notify,
    status: RwLock<Value>,
}
impl Worker {
    pub fn new(
        config: &Config,
        scope: Scope,
        directory: PathBuf,
        database: Option<db::Config>,
    ) -> Result<Arc<Self>, AppError> {
        let sync = master_sync::Syncer::standalone(
            config.source.clone(),
            scope.clone(),
            directory.clone(),
        )?;
        Ok(Arc::new(Self {
            sync,
            scope,
            directory,
            database,
            interval: Duration::from_secs(config.source.interval_seconds),
            wake: Notify::new(),
            status: RwLock::new(json!({"status":"pending","last_success":null})),
        }))
    }
    pub fn refresh(&self) {
        self.wake.notify_one();
    }
    pub fn hint(&self, hint: &master_sync::UpdateHint) -> Result<(), AppError> {
        if hint.scope != self.scope || !crate::master_registry::hash_valid(&hint.content_sha256) {
            return Err(AppError::InvalidRequest);
        }
        self.refresh();
        Ok(())
    }
    pub async fn status(&self) -> Value {
        self.status.read().await.clone()
    }
    async fn update(&self) -> Result<Value, &'static str> {
        let result = self
            .sync
            .update_once()
            .await
            .map_err(|_| "owner_sync_failed")?;
        let publication = if let Some(database) = &self.database {
            Some(
                db::publish(database, &self.directory, self.scope.clone())
                    .await
                    .map_err(|_| "database_publish_failed")?,
            )
        } else {
            None
        };
        Ok(json!({"sync":result,"database":publication}))
    }
    pub async fn run(self: Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut last_success = Value::Null;
        loop {
            if *shutdown.borrow() {
                break;
            }
            *self.status.write().await = json!({"status":"running","last_success":last_success});
            tokio::select! {biased;
                _=shutdown.changed()=>break,
                result=self.update()=>{
                    match result{
                        Ok(receipt)=>{
                            last_success=json!({"completed_at":chrono::Utc::now(),"receipt":receipt});
                            *self.status.write().await=json!({"status":"ready","last_success":last_success});
                        },
                        Err(code)=>{
                            *self.status.write().await=json!({"status":"failed","error_code":code,"last_success":last_success});
                            tracing::warn!(error_code=code,"Registry owner update failed; published data retained");
                        }
                    }
                }
            }
            tokio::select! {biased;_=shutdown.changed()=>break,_=self.wake.notified()=>{},_=tokio::time::sleep(self.interval)=>{}}
        }
        *self.status.write().await = json!({"status":"stopped","last_success":last_success});
    }
}
