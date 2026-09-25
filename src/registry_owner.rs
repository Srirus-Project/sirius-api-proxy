//! Standalone owner synchronization and optional transactional database publication.
use crate::{error::AppError, master_database as db, master_registry::Scope, master_sync};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{Notify, RwLock};
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub source: Option<master_sync::Config>,
    /// Local-only reconciliation interval; rejected when a source is configured.
    pub local_interval_seconds: Option<u64>,
    pub internal_token_env: String,
    /// Required for PostgreSQL; file backends use their serving directory.
    pub staging_directory: Option<PathBuf>,
}
pub struct Worker {
    sync: Option<Arc<master_sync::Syncer>>,
    scope: Scope,
    directory: PathBuf,
    database: Option<db::Config>,
    interval: Duration,
    wake: Notify,
    pending: std::sync::atomic::AtomicU8,
    status: RwLock<Value>,
}
impl Worker {
    pub fn new(
        config: &Config,
        scope: Scope,
        directory: PathBuf,
        database: Option<db::Config>,
    ) -> Result<Arc<Self>, AppError> {
        if config.source.is_some() && config.local_interval_seconds.is_some() {
            return Err(AppError::Config(
                "local interval requires a local-only registry publisher",
            ));
        }
        let interval = config
            .source
            .as_ref()
            .map(|s| s.interval_seconds)
            .unwrap_or(config.local_interval_seconds.unwrap_or(300));
        if !(60..=86400).contains(&interval) {
            return Err(AppError::Config("invalid registry publication interval"));
        }
        let sync = config
            .source
            .as_ref()
            .map(|source| {
                master_sync::Syncer::standalone(source.clone(), scope.clone(), directory.clone())
            })
            .transpose()?;
        if directory.as_os_str().is_empty() {
            return Err(AppError::Config(
                "registry publication directory is required",
            ));
        }
        Ok(Arc::new(Self {
            sync,
            scope,
            directory,
            database,
            interval: Duration::from_secs(interval),
            wake: Notify::new(),
            pending: std::sync::atomic::AtomicU8::new(0),
            status: RwLock::new(json!({"status":"pending","last_success":null})),
        }))
    }
    pub fn has_source(&self) -> bool {
        self.sync.is_some()
    }
    pub fn refresh(&self) {
        self.pending
            .fetch_or(1, std::sync::atomic::Ordering::Release);
        self.wake.notify_one();
    }
    pub fn publish_local(&self) {
        self.pending
            .fetch_or(2, std::sync::atomic::Ordering::Release);
        self.wake.notify_one();
    }
    pub fn hint(&self, hint: &master_sync::UpdateHint) -> Result<(), AppError> {
        if !self.has_source() {
            return Err(AppError::NotFound);
        }
        if hint.scope != self.scope || !crate::master_registry::hash_valid(&hint.content_sha256) {
            return Err(AppError::InvalidRequest);
        }
        self.refresh();
        Ok(())
    }
    pub async fn status(&self) -> Value {
        self.status.read().await.clone()
    }
    async fn update(&self, pull: bool) -> Result<Value, &'static str> {
        let result = if pull {
            match &self.sync {
                Some(sync) => Some(sync.update_once().await.map_err(|_| "owner_sync_failed")?),
                None => None,
            }
        } else {
            None
        };
        let local = if self.database.is_none() {
            Some(
                db::verify_current(&self.directory, self.scope.clone())
                    .await
                    .map_err(|_| "local_verification_failed")?,
            )
        } else {
            None
        };
        let publication = if let Some(database) = &self.database {
            Some(
                db::publish(database, &self.directory, self.scope.clone())
                    .await
                    .map_err(|_| "database_publish_failed")?,
            )
        } else {
            None
        };
        Ok(json!({"sync":result,"database":publication,"local":local}))
    }
    pub async fn run(self: Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut last_success = Value::Null;
        let mut pull = true;
        loop {
            if *shutdown.borrow() {
                break;
            }
            *self.status.write().await = json!({"status":"running","last_success":last_success});
            tokio::select! {biased;
                _=shutdown.changed()=>break,
                result=self.update(pull)=>{
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
            pull = tokio::select! {biased;
                _=shutdown.changed()=>break,
                _=self.wake.notified()=>{
                    let pending=self.pending.swap(0,std::sync::atomic::Ordering::AcqRel);
                    if pending&2!=0 {
                        // Local publication is independent of source reachability. If both
                        // were requested, reconcile the source in the following iteration.
                        if pending&1!=0{self.refresh();}
                        false
                    }else{true}
                },
                _=tokio::time::sleep(self.interval)=>true
            };
        }
        *self.status.write().await = json!({"status":"stopped","last_success":last_success});
    }
}
