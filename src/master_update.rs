//! Formal-client manifest download flow. No ZIP candidates or automatic registration.
use crate::{
    client::GameClient,
    config::{secret, Config},
    master::{self, Manifest, MasterDecoder, MasterError},
};
use chrono::Utc;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Mutex;

// Deliberately not Debug or Serialize: this holds a CDN password.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MasterTarget {
    pub version: String,
    /// Asset version from the same VERSION response as `version`, when the game reported one.
    pub resource_version: Option<String>,
    pub root: String,
    pub password: String,
}
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("Master updater configuration is invalid")]
    Config,
    #[error("Master version or approved CDN credential is unavailable")]
    Target,
    #[error("Master CDN request failed")]
    Download,
    #[error("Master CDN returned HTTP {0}")]
    Http(u16),
    #[error("Master update exceeded its deadline")]
    Timeout,
    #[error("Master version or CDN changed during update")]
    Changed,
    #[error("Master validation or storage failed")]
    Master,
}
impl From<MasterError> for UpdateError {
    fn from(_: MasterError) -> Self {
        Self::Master
    }
}
/// HTTP policy for Master CDN traffic, independent of game RPC transport.
#[derive(Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Network {
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub update_timeout_seconds: u64,
    pub attempts: usize,
    pub retry_delay_ms: u64,
    pub max_retry_delay_ms: u64,
    pub proxy_url_env: Option<String>,
    pub proxy_authorization_env: Option<String>,
}
impl Default for Network {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 10_000,
            request_timeout_ms: 60_000,
            update_timeout_seconds: 600,
            attempts: 1,
            retry_delay_ms: 250,
            max_retry_delay_ms: 5_000,
            proxy_url_env: None,
            proxy_authorization_env: None,
        }
    }
}
impl Network {
    pub fn validate(&self) -> Result<(), UpdateError> {
        if !(100..=300_000).contains(&self.connect_timeout_ms)
            || !(100..=300_000).contains(&self.request_timeout_ms)
            || !(1..=3600).contains(&self.update_timeout_seconds)
            || !(1..=8).contains(&self.attempts)
            || !(1..=10_000).contains(&self.retry_delay_ms)
            || !(self.retry_delay_ms..=30_000).contains(&self.max_retry_delay_ms)
            || (self.proxy_url_env.is_none() && self.proxy_authorization_env.is_some())
        {
            return Err(UpdateError::Config);
        }
        for name in self
            .proxy_url_env
            .iter()
            .chain(self.proxy_authorization_env.iter())
        {
            if name.is_empty()
                || name.len() > 256
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(UpdateError::Config);
            }
        }
        Ok(())
    }
    fn client(&self) -> Result<reqwest::Client, UpdateError> {
        self.validate()?;
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(self.connect_timeout_ms))
            .timeout(Duration::from_millis(self.request_timeout_ms));
        if let Some(name) = &self.proxy_url_env {
            let value = secret(name).map_err(|_| UpdateError::Config)?;
            let uri = crate::transport::proxy_uri(&value).map_err(|_| UpdateError::Config)?;
            let mut proxy =
                reqwest::Proxy::all(uri.to_string()).map_err(|_| UpdateError::Config)?;
            if let Some(name) = &self.proxy_authorization_env {
                let value = secret(name).map_err(|_| UpdateError::Config)?;
                if value.len() > 4096 || value.trim().is_empty() {
                    return Err(UpdateError::Config);
                }
                let mut value = reqwest::header::HeaderValue::from_str(&value)
                    .map_err(|_| UpdateError::Config)?;
                value.set_sensitive(true);
                proxy = proxy.custom_http_auth(value);
            }
            builder = builder.proxy(proxy);
        }
        builder.build().map_err(|_| UpdateError::Config)
    }
    fn retry(&self, error: &UpdateError, attempt: usize) -> bool {
        attempt + 1 < self.attempts
            && matches!(
                error,
                UpdateError::Download | UpdateError::Http(429 | 500..=599)
            )
    }
    fn delay(&self, attempt: usize) -> Duration {
        Duration::from_millis((self.retry_delay_ms * (1 << attempt)).min(self.max_retry_delay_ms))
    }
}

pub struct MasterUpdater {
    game: Arc<GameClient>,
    http: reqwest::Client,
    username: String,
    decoder: Arc<MasterDecoder>,
    output: PathBuf,
    interval: Duration,
    lock: Mutex<()>,
    deadline: Duration,
    network: Network,
}
impl MasterUpdater {
    #[cfg(test)]
    pub(crate) fn test_timing(updater: &mut Arc<Self>, deadline: Duration, interval: Duration) {
        let updater = Arc::get_mut(updater).unwrap();
        updater.deadline = deadline;
        updater.interval = interval;
    }
    pub fn new(config: &Config, game: Arc<GameClient>) -> Result<Arc<Self>, UpdateError> {
        let update = config.master_update.as_ref().ok_or(UpdateError::Config)?;
        let output = config.master_directory.clone().ok_or(UpdateError::Config)?;
        let username = secret(&update.username_env).map_err(|_| UpdateError::Config)?;
        if username.contains(':') {
            return Err(UpdateError::Config);
        }
        let key =
            master::key_from_hex(&secret(&update.key_hex_env).map_err(|_| UpdateError::Config)?)?;
        let iv =
            master::key_from_hex(&secret(&update.iv_hex_env).map_err(|_| UpdateError::Config)?)?;
        let http = update.network.client()?;
        Ok(Arc::new(Self {
            game,
            http,
            username,
            decoder: Arc::new(MasterDecoder::new(&key, iv)),
            output,
            interval: Duration::from_secs(update.interval_seconds),
            lock: Mutex::new(()),
            deadline: Duration::from_secs(update.network.update_timeout_seconds),
            network: update.network.clone(),
        }))
    }
    async fn download(
        &self,
        target: &MasterTarget,
        name: &str,
        limit: u64,
    ) -> Result<Vec<u8>, UpdateError> {
        for attempt in 0..self.network.attempts {
            match self.download_once(target, name, limit).await {
                Err(error) if self.network.retry(&error, attempt) => {
                    tokio::time::sleep(self.network.delay(attempt)).await
                }
                result => return result,
            }
        }
        Err(UpdateError::Download)
    }
    async fn download_once(
        &self,
        target: &MasterTarget,
        name: &str,
        limit: u64,
    ) -> Result<Vec<u8>, UpdateError> {
        let url = format!("{}/master/{}/{}", target.root, target.version, name);
        let mut response = self
            .http
            .get(url)
            .basic_auth(&self.username, Some(&target.password))
            .send()
            .await
            .map_err(|_| UpdateError::Download)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(UpdateError::Http(response.status().as_u16()));
        }
        if response.content_length().is_some_and(|n| n > limit) {
            return Err(UpdateError::Master);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| UpdateError::Download)? {
            if bytes.len() as u64 + chunk.len() as u64 > limit {
                return Err(UpdateError::Master);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    /// One bounded pass. Errors never echo URLs, credentials or upstream response bodies.
    pub async fn update_once(&self) -> Result<Value, UpdateError> {
        let deadline = tokio::time::Instant::now() + self.deadline;
        let _lock = tokio::time::timeout_at(deadline, self.lock.lock())
            .await
            .map_err(|_| UpdateError::Timeout)?;
        let started = Utc::now();
        self.game
            .record_master_update(json!({"status":"running","started_at":started}))
            .await;
        let result = tokio::time::timeout_at(deadline, self.update())
            .await
            .unwrap_or(Err(UpdateError::Timeout));
        let status = match &result {
            Ok(value) => {
                json!({"status":"ready","started_at":started,"completed_at":Utc::now(),"result":value})
            }
            Err(error) => {
                json!({"status":"failed","started_at":started,"completed_at":Utc::now(),"error":error.to_string()})
            }
        };
        self.game.record_master_update(status).await;
        result
    }
    async fn update(&self) -> Result<Value, UpdateError> {
        let _writer = master::WriterLock::acquire(&self.output)?;
        let target = self
            .game
            .refresh_master_target()
            .await
            .map_err(|_| UpdateError::Target)?;
        // Installed JSON is independently usable during maintenance/download failures.
        let output = self.output.clone();
        let version = target.version.clone();
        let resource_version = target.resource_version.clone();
        let current = tokio::task::spawn_blocking(move || -> Option<Value> {
            let current = master::read_current(&output, None).ok()?;
            if current.version != version {
                return None;
            }
            let status: Value = serde_json::from_slice(&current.bytes).ok()?;
            // A snapshot installed without asset-version provenance is reinstalled once the
            // game reports one with the same master version. A recorded value is kept: it is
            // the provenance of this Master installation, not a live asset-version mirror.
            if resource_version.is_some() && status.get("resource_version").is_none() {
                return None;
            }
            // A missing/truncated table triggers a full repair instead of an unchanged result.
            for table in status["tables"].as_array()? {
                let document = master::read_current(&output, Some(table.as_str()?)).ok()?;
                if document.version != version
                    || serde_json::from_slice::<Value>(&document.bytes).is_err()
                {
                    return None;
                }
            }
            Some(status)
        })
        .await
        .map_err(|_| UpdateError::Master)?;
        if let Some(current) = current {
            return Ok(json!({"action":"unchanged","current":current}));
        }
        let bytes = self
            .download(&target, "MasterManifest.json", master::MAX_MANIFEST)
            .await?;
        let manifest = Manifest::parse(&bytes)?;
        if manifest.version != target.version {
            return Err(UpdateError::Changed);
        }
        tokio::fs::create_dir_all(&self.output)
            .await
            .map_err(|_| UpdateError::Master)?;
        let encrypted = tempfile::Builder::new()
            .prefix(".master-download-")
            .tempdir_in(&self.output)
            .map_err(|_| UpdateError::Master)?;
        tokio::fs::write(encrypted.path().join("MasterManifest.json"), bytes)
            .await
            .map_err(|_| UpdateError::Master)?;
        for entry in &manifest.files {
            let bytes = self.download(&target, &entry.name, entry.size).await?;
            tokio::fs::write(encrypted.path().join(&entry.name), bytes)
                .await
                .map_err(|_| UpdateError::Master)?;
        }
        let decoder = self.decoder.clone();
        let output = self.output.clone();
        let resource_version = target.resource_version.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            // Keep encrypted tempdir owned by this task even if the caller times out.
            master::prepare_directory(
                encrypted.path(),
                &output,
                &decoder,
                "remote",
                resource_version,
            )
        })
        .await
        .map_err(|_| UpdateError::Master)??;
        let latest = self
            .game
            .refresh_master_target()
            .await
            .map_err(|_| UpdateError::Target)?;
        if latest != target {
            return Err(UpdateError::Changed);
        }
        // No await between the final comparison and publication: cancellation cannot
        // abandon a publish task that later switches CURRENT behind the caller's back.
        let receipt = prepared.publish(&self.output)?;
        Ok(json!({"action":"updated","receipt":receipt}))
    }
    pub async fn run(self: Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                result = self.update_once() => {
                    match result {
                        Ok(_) => tracing::info!("Master update check completed"),
                        Err(error) => tracing::warn!(error_code=%error,"Master update failed; keeping installed snapshot"),
                    }
                }
            }
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = tokio::time::sleep(self.interval) => {},
            }
        }
    }
}
