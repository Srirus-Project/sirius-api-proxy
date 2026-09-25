//! Owner-to-consumer Master synchronization using pinned plaintext manifests.
use crate::{
    client::GameClient,
    config::secret,
    error::AppError,
    master::{self, MasterError},
    master_registry::{self, PublishedManifest, Scope},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Mutex;
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub origin: String,
    pub token_env: String,
    #[serde(default)]
    pub regional_paths: bool,
    #[serde(default)]
    pub allow_http: bool,
    pub interval_seconds: u64,
    #[serde(default = "update_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "request_timeout")]
    pub request_timeout_ms: u64,
}
fn update_timeout() -> u64 {
    600
}
fn request_timeout() -> u64 {
    60_000
}
impl Config {
    pub fn validate(&self) -> Result<(), AppError> {
        if !(60..=86400).contains(&self.interval_seconds)
            || !(1..=3600).contains(&self.timeout_seconds)
            || !(100..=300_000).contains(&self.request_timeout_ms)
            || self.token_env.is_empty()
            || self.token_env.len() > 256
            || !self
                .token_env
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(AppError::Config("invalid Master sync policy"));
        }
        crate::peer_transport::Client::new(
            &self.origin,
            "validation",
            crate::region::Region::Jp,
            false,
            self.allow_http,
            crate::peer_transport::Policy {
                connect_timeout_ms: self.request_timeout_ms.min(10_000),
                request_timeout_ms: self.request_timeout_ms,
                ..Default::default()
            },
        )
        .map_err(|_| AppError::Config("invalid Master owner origin"))?;
        Ok(())
    }
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Master owner request failed")]
    Download,
    #[error("Master synchronization timed out")]
    Timeout,
    #[error("Master owner manifest or file integrity failed")]
    Integrity,
    #[error("Master owner changed during synchronization")]
    Changed,
    #[error("Master consumer storage failed")]
    Storage,
}
impl From<MasterError> for Error {
    fn from(_: MasterError) -> Self {
        Self::Storage
    }
}
/// A wakeup hint only; the configured owner manifest remains authoritative.
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateHint {
    pub scope: Scope,
    pub content_sha256: String,
}
pub struct Syncer {
    config: Config,
    http: reqwest::Client,
    authorization: reqwest::header::HeaderValue,
    root: reqwest::Url,
    scope: Scope,
    output: PathBuf,
    game: Option<Arc<GameClient>>,
    gate: Mutex<()>,
}
impl Syncer {
    pub fn new(
        config: &crate::config::Config,
        game: Arc<GameClient>,
    ) -> Result<Arc<Self>, AppError> {
        config.validate()?;
        let policy = config
            .master_sync
            .clone()
            .ok_or(AppError::Config("Master sync is not configured"))?;
        Self::construct(
            policy,
            Scope {
                region: config.region,
                environment: config.environment.clone(),
                platform: config.platform(),
            },
            config
                .master_directory
                .clone()
                .ok_or(AppError::Config("Master sync output is required"))?,
            Some(game),
        )
    }
    /// Shared verified transfer engine for standalone registry owners.
    pub fn standalone(
        policy: Config,
        scope: Scope,
        output: PathBuf,
    ) -> Result<Arc<Self>, AppError> {
        Self::construct(policy, scope, output, None)
    }
    fn construct(
        policy: Config,
        scope: Scope,
        output: PathBuf,
        game: Option<Arc<GameClient>>,
    ) -> Result<Arc<Self>, AppError> {
        policy.validate()?;
        if scope.region != crate::region::Region::Jp
            || scope.environment.is_empty()
            || scope.environment.len() > 256
            || !scope
                .environment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || output.as_os_str().is_empty()
        {
            return Err(AppError::Config("invalid Master sync scope or output"));
        }
        let token = secret(&policy.token_env)?;
        if token.len() > 4096 || !token.bytes().all(|b| (33..=126).contains(&b)) {
            return Err(AppError::Config("invalid Master owner token"));
        }
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| AppError::Config("invalid Master owner token"))?;
        authorization.set_sensitive(true);
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_millis(policy.request_timeout_ms.min(10_000)))
            .timeout(Duration::from_millis(policy.request_timeout_ms))
            .build()
            .map_err(|_| AppError::Config("Master sync HTTP initialization failed"))?;
        let mut root = reqwest::Url::parse(&policy.origin)
            .map_err(|_| AppError::Config("invalid Master owner origin"))?;
        root.set_path(&if policy.regional_paths {
            format!("/api/v1/{}/master-data/", scope.region.name())
        } else {
            "/api/v1/master-data/".into()
        });
        Ok(Arc::new(Self {
            config: policy,
            http,
            authorization,
            root,
            scope,
            output,
            game,
            gate: Mutex::new(()),
        }))
    }
    async fn download(&self, path: &str, limit: u64) -> Result<Vec<u8>, Error> {
        let url = self.root.join(path).map_err(|_| Error::Integrity)?;
        let mut response = self
            .http
            .get(url)
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| Error::Download)?;
        if response.status() != reqwest::StatusCode::OK
            || response.content_length().is_some_and(|n| n > limit)
            || !response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|h| {
                    h.split(';')
                        .next()
                        .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
                })
        {
            return Err(Error::Download);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Download)? {
            if chunk.len() as u64 > limit.saturating_sub(bytes.len() as u64) {
                return Err(Error::Integrity);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    async fn manifest(&self) -> Result<PublishedManifest, Error> {
        let bytes = self.download("manifest", 4 * 1024 * 1024).await?;
        let value: PublishedManifest =
            serde_json::from_slice(&bytes).map_err(|_| Error::Integrity)?;
        value.validate(&self.scope).map_err(|_| Error::Integrity)?;
        Ok(value)
    }
    pub async fn update_once(&self) -> Result<Value, Error> {
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(self.config.timeout_seconds);
        let result = tokio::time::timeout_at(deadline, async {
            let _gate = self.gate.lock().await;
            if let Some(game) = &self.game {
                game.record_master_update(json!({"mode":"sync","status":"running"}))
                    .await;
            }
            self.update(deadline).await
        })
        .await
        .unwrap_or(Err(Error::Timeout));
        if let Some(game) = &self.game {
            game.record_master_update(match &result {Ok(v)=>json!({"mode":"sync","status":"ready","completed_at":chrono::Utc::now(),"result":v}),Err(e)=>json!({"mode":"sync","status":"failed","error":e.to_string()})}).await;
        }
        result
    }
    async fn update(&self, deadline: tokio::time::Instant) -> Result<Value, Error> {
        let writer = master::WriterLock::acquire(&self.output)?;
        let target = self.manifest().await?;
        let output = self.output.clone();
        let scope = self.scope.clone();
        let target_hash = target.content_sha256.clone();
        let (writer, current, unchanged) = tokio::task::spawn_blocking(move || {
            let current = master_registry::manifest(&output, None, scope)
                .ok()
                .and_then(|document| {
                    serde_json::from_slice::<PublishedManifest>(&document.bytes).ok()
                });
            let unchanged = current.as_ref().is_some_and(|current| {
                current.content_sha256 == target_hash
                    && current.files.iter().all(|f| {
                        master_registry::table(
                            &output,
                            &current.snapshot,
                            f.name.trim_end_matches(".json"),
                            &f.sha256,
                        )
                        .is_ok()
                    })
            });
            (writer, current, unchanged)
        })
        .await
        .map_err(|_| Error::Storage)?;
        if unchanged {
            return Ok(json!({"action":"unchanged","content_sha256":target.content_sha256}));
        }
        let staging = tempfile::Builder::new()
            .prefix(".master-sync-")
            .tempdir_in(&self.output)
            .map_err(|_| Error::Storage)?;
        let mut downloaded = 0usize;
        let mut reused = 0usize;
        for file in &target.files {
            let cached = if let Some(current) = &current {
                if current.files.iter().any(|old| old == file) {
                    let output = self.output.clone();
                    let snapshot = current.snapshot.clone();
                    let file = file.clone();
                    tokio::task::spawn_blocking(move || {
                        master_registry::table(
                            &output,
                            &snapshot,
                            file.name.trim_end_matches(".json"),
                            &file.sha256,
                        )
                        .ok()
                        .map(|doc| doc.bytes)
                    })
                    .await
                    .map_err(|_| Error::Storage)?
                } else {
                    None
                }
            } else {
                None
            };
            let bytes = if let Some(bytes) = cached {
                reused += 1;
                bytes
            } else {
                downloaded += 1;
                self.download(
                    &format!(
                        "snapshots/{}/tables/{}/{}",
                        target.snapshot,
                        file.name.trim_end_matches(".json"),
                        file.sha256
                    ),
                    file.size,
                )
                .await?
            };
            if bytes.len() as u64 != file.size || master_registry::digest(&bytes) != file.sha256 {
                return Err(Error::Integrity);
            }
            tokio::fs::write(staging.path().join(&file.name), bytes)
                .await
                .map_err(|_| Error::Storage)?;
        }
        let pinned = target.clone();
        // The blocking worker owns staging and writer even if the awaiting future is cancelled.
        // It cannot switch CURRENT: only this caller publishes after the final owner check.
        let (writer, prepared) = tokio::task::spawn_blocking(move || {
            (writer, master::prepare_registry(staging, &pinned))
        })
        .await
        .map_err(|_| Error::Storage)?;
        let prepared = prepared?;
        let latest = self.manifest().await?;
        if latest.content_sha256 != target.content_sha256 {
            return Err(Error::Changed);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Timeout);
        }
        let receipt = prepared.publish(&self.output)?;
        drop(writer);
        Ok(
            json!({"action":"updated","receipt":receipt,"content_sha256":target.content_sha256,"downloaded_files":downloaded,"reused_files":reused}),
        )
    }
    pub async fn run(self: Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {biased; _=shutdown.changed()=>break,result=self.update_once()=>{
                if result.is_err(){tracing::warn!(error_code="master_sync_failed","Master sync failed; installed snapshot retained");}
            }}
            tokio::select! {biased;
                _=shutdown.changed()=>break,
                _=async { match &self.game { Some(game)=>game.master_sync_notified().await, None=>std::future::pending().await } }=>{},
                _=tokio::time::sleep(Duration::from_secs(self.config.interval_seconds))=>{}
            }
        }
    }
}
