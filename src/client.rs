use crate::protocol::{self, ProtocolBundle, ProtocolStatus};
pub use crate::routes::*;
use crate::{
    config::{secret, Config},
    error::AppError,
    resources::{self, ResourceSnapshot},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http_body_util::{BodyExt, Full};
use hyper::{header::HeaderMap, Request};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{Mutex, RwLock as AsyncRwLock};

const MAX_RESPONSE: usize = 8 * 1024 * 1024;

pub(crate) fn authenticated(route: &str) -> bool {
    matches!(
        route,
        PROFILE
            | EVENT_RANKING
            | EVENT_DECK
            | MUSIC_RANKING
            | CHALLENGE_RANKING
            | WHOAMI
            | PLAYER_DATA
    )
}

#[derive(Clone, Default, Serialize)]
pub struct Observation {
    pub observed_at: Option<DateTime<Utc>>,
    pub grpc_status: Option<u16>,
    pub application_code: Option<String>,
    pub server_time: Option<String>,
    pub maintenance: bool,
    pub master_version: Option<String>,
    pub resource_version: Option<String>,
}
struct State {
    master_update: Value,
    observation: Observation,
    snapshot: Option<ResourceSnapshot>,
    snapshot_stale: bool,
    cdn_root: String,
    credential_valid: bool,
}

pub struct GameClient {
    config: Config,
    http: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    protocol: RwLock<Arc<ProtocolBundle>>,
    reload_lock: Mutex<()>,
    account: Option<(String, String)>,
    cdn_secrets: BTreeMap<String, String>,
    state: Mutex<State>,
    // Optional account serialization, independent of the protocol activation barrier.
    call_lock: Mutex<()>,
    protocol_calls: AsyncRwLock<()>,
    bootstrap_lock: Mutex<()>,
    timeout: Duration,
}
fn header<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key)?.to_str().ok()
}

impl GameClient {
    pub fn new(config: Config) -> Result<Arc<Self>, AppError> {
        config.validate()?;
        Self::build(config, false)
    }
    fn build(config: Config, test_http: bool) -> Result<Arc<Self>, AppError> {
        let protocol = ProtocolBundle::load(&config.protocol_path())?;
        if protocol.status.family != config.region.family() {
            return Err(AppError::ProtocolDefinition);
        }
        let builder = HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(rustls::crypto::ring::default_provider())
            .map_err(|_| AppError::Config("TLS provider initialization failed"))?;
        let connector = if test_http {
            builder.https_or_http().enable_http2().build()
        } else {
            builder.https_only().enable_http2().build()
        };
        let http = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build(connector);
        let account = match (&config.player_id_env, &config.player_credential_env) {
            (Some(id), Some(credential)) => Some((secret(id)?, secret(credential)?)),
            (None, None) => None,
            _ => return Err(AppError::Config("both account references required")),
        };
        let cdn_secrets = config
            .cdn_credential_env
            .iter()
            .filter_map(|(root, name)| secret(name).ok().map(|s| (root.clone(), s)))
            .collect::<BTreeMap<_, _>>();
        let state = State {
            master_update: json!({"status": if config.master_update.is_some() {"pending"} else {"disabled"}}),
            observation: Observation::default(),
            snapshot: None,
            snapshot_stale: true,
            cdn_root: config.default_cdn_root.clone(),
            credential_valid: cdn_secrets.contains_key(&config.default_cdn_root),
        };
        Ok(Arc::new(Self {
            config,
            http,
            account,
            cdn_secrets,
            state: Mutex::new(state),
            call_lock: Mutex::new(()),
            protocol_calls: AsyncRwLock::new(()),
            bootstrap_lock: Mutex::new(()),
            timeout: Duration::from_secs(20),
            protocol: RwLock::new(Arc::new(protocol)),
            reload_lock: Mutex::new(()),
        }))
    }
    #[cfg(test)]
    pub(crate) fn for_test(config: Config) -> Arc<Self> {
        Self::build(config, true).unwrap()
    }
    #[cfg(test)]
    pub(crate) fn set_test_timeout(client: &mut Arc<Self>, duration: Duration) {
        Arc::get_mut(client).unwrap().timeout = duration;
    }
    pub fn protocol_status(&self) -> Result<ProtocolStatus, AppError> {
        Ok(self
            .protocol
            .read()
            .map_err(|_| AppError::ProtocolDefinition)?
            .status
            .clone())
    }
    pub async fn reload_protocol(&self) -> Result<ProtocolStatus, AppError> {
        let _reload = self.reload_lock.lock().await;
        let directory = self.config.protocol_path();
        let mut candidate = tokio::task::spawn_blocking(move || ProtocolBundle::load(&directory))
            .await
            .map_err(|_| AppError::ProtocolDefinition)??;
        // Compiling does not pause RPCs. Activation waits for the current logical
        // calls (including Version/Whoami bootstrap) to finish using their old bundle.
        let _calls = self.protocol_calls.write().await;
        let current = self
            .protocol
            .read()
            .map_err(|_| AppError::ProtocolDefinition)?
            .clone();
        if candidate.status.family != self.config.region.family() {
            return Err(AppError::ProtocolDefinition);
        }
        if current.status.sha256 == candidate.status.sha256 {
            return Ok(current.status.clone());
        }
        protocol::compatible(&current.pool, &candidate.pool)?;
        candidate.status.generation = current
            .status
            .generation
            .checked_add(1)
            .ok_or(AppError::ProtocolDefinition)?;
        candidate.status.loaded_at = Utc::now();
        let status = candidate.status.clone();
        let mut state = self.state.lock().await;
        *self
            .protocol
            .write()
            .map_err(|_| AppError::ProtocolDefinition)? = Arc::new(candidate);
        state.observation = Observation::default();
        state.snapshot_stale = true;
        Ok(status)
    }
    pub fn region(&self) -> crate::region::Region {
        self.config.region
    }
    pub fn platform(&self) -> crate::region::Platform {
        self.config.platform()
    }
    pub fn supported_routes(&self) -> &'static [&'static str] {
        crate::routes::for_family(self.config.region.family())
    }
    pub fn environment(&self) -> &str {
        &self.config.environment
    }
    pub fn master_directory(&self) -> Option<&std::path::Path> {
        self.config.master_directory.as_deref()
    }
    pub async fn observation(&self) -> Observation {
        self.state.lock().await.observation.clone()
    }
    pub async fn master_update_status(&self) -> Value {
        self.state.lock().await.master_update.clone()
    }
    pub(crate) async fn record_master_update(&self, value: Value) {
        self.state.lock().await.master_update = value;
    }
    pub(crate) async fn refresh_master_target(
        &self,
    ) -> Result<crate::master_update::MasterTarget, AppError> {
        self.call(VERSION, json!({})).await?;
        let state = self.state.lock().await;
        let version = state
            .observation
            .master_version
            .as_ref()
            .filter(|v| crate::master::safe_version(v))
            .ok_or(AppError::MasterUnavailable)?;
        if state.observation.grpc_status != Some(0)
            || state.observation.maintenance
            || !state.credential_valid
        {
            return Err(AppError::MasterUnavailable);
        }
        let password = self
            .cdn_secrets
            .get(&state.cdn_root)
            .ok_or(AppError::MasterUnavailable)?;
        Ok(crate::master_update::MasterTarget {
            version: version.clone(),
            root: state.cdn_root.clone(),
            password: password.clone(),
        })
    }
    pub async fn snapshot(&self) -> Result<Value, AppError> {
        let s = self.state.lock().await;
        let snapshot = s.snapshot.as_ref().ok_or(AppError::SnapshotUnavailable)?;
        let stale = s.snapshot_stale || (Utc::now() - snapshot.observed_at).num_seconds() > 300;
        Ok(json!({"snapshot":snapshot,"stale":stale}))
    }
    pub async fn call(&self, route: &str, input: Value) -> Result<Value, AppError> {
        // No generic passthrough: only these read operations are supported.
        if !crate::routes::ROUTES.contains(&route) && route != crate::routes::SERVER_LIST {
            return Err(AppError::InvalidRequest);
        }
        if !self.supported_routes().contains(&route) {
            return Err(AppError::UnsupportedRegionOperation);
        }
        if authenticated(route) && self.account.is_none() {
            return Err(AppError::AccountUnavailable);
        }
        let result = tokio::time::timeout(self.timeout, async {
            let _guard = if self.config.session_lock {
                Some(self.call_lock.lock().await)
            } else {
                None
            };
            let _protocol_call = self.protocol_calls.read().await;
            let protocol = self
                .protocol
                .read()
                .map_err(|_| AppError::ProtocolDefinition)?
                .clone();
            if authenticated(route) && self.state.lock().await.observation.master_version.is_none()
            {
                let _bootstrap = self.bootstrap_lock.lock().await;
                // Another parallel caller may have initialized Version while we waited.
                if self.state.lock().await.observation.master_version.is_none() {
                    self.execute(&protocol, VERSION, json!({})).await?;
                }
            }
            if route == PLAYER_DATA {
                // Check the configured identity before returning private account data.
                self.execute(&protocol, WHOAMI, json!({})).await?;
            }
            self.execute(&protocol, route, input).await
        })
        .await;
        let result = result.unwrap_or(Err(AppError::Timeout));
        if matches!(
            result,
            Err(AppError::Timeout | AppError::Transport | AppError::Protocol)
        ) {
            let mut s = self.state.lock().await;
            s.snapshot_stale = true;
            s.observation.grpc_status = None;
            s.observation.observed_at = Some(Utc::now());
        }
        result
    }
    async fn execute(
        &self,
        protocol: &ProtocolBundle,
        route: &str,
        input: Value,
    ) -> Result<Value, AppError> {
        let encoded = protocol.encode(route, input)?;
        let mut frame = Vec::with_capacity(encoded.len() + 5);
        frame.push(0);
        frame.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        frame.extend_from_slice(&encoded);
        let mut request = Request::post(format!("{}{route}", self.config.endpoint))
            .header("content-type", "application/grpc+proto")
            .header(
                "user-agent",
                concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
            )
            .header("te", "trailers")
            .header("grpc-accept-encoding", "identity")
            .header("grpc-timeout", "20S")
            .header("x-platform", self.config.platform().header())
            .header("x-client-version", &self.config.client_version)
            .header("x-request-id", uuid::Uuid::new_v4().to_string());
        if let Some(version) = &self.state.lock().await.observation.master_version {
            request = request.header("x-master-version", version);
        }
        // Anonymous endpoints never receive game credentials.
        if authenticated(route) {
            let (id, credential) = self.account.as_ref().ok_or(AppError::AccountUnavailable)?;
            request = request
                .header("x-player-id", id)
                .header("x-player-credential", credential);
        }
        let response = self
            .http
            .request(
                request
                    .body(Full::new(Bytes::from(frame)))
                    .map_err(|_| AppError::Protocol)?,
            )
            .await
            .map_err(|_| AppError::Transport)?;
        let http_ok = response.status().is_success();
        let mut metadata = response.headers().clone();
        let content_ok = header(&metadata, "content-type")
            .is_some_and(|s| s == "application/grpc" || s.starts_with("application/grpc+"));
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| AppError::Transport)?;
            if let Some(data) = frame.data_ref() {
                if bytes.len() + data.len() > MAX_RESPONSE {
                    return Err(AppError::Protocol);
                }
                bytes.extend_from_slice(data);
            }
            if let Some(trailers) = frame.trailers_ref() {
                metadata.extend(trailers.clone());
            }
        }
        let status = header(&metadata, "grpc-status")
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|s| *s <= 16);
        self.observe(&metadata, status).await;
        if !http_ok || !content_ok {
            return Err(AppError::Protocol);
        }
        let status = status.ok_or(AppError::Protocol)?;
        if status != 0 {
            return Err(AppError::Grpc(status));
        }
        if header(&metadata, "grpc-encoding").is_some_and(|s| s != "identity") {
            return Err(AppError::Protocol);
        }
        if bytes.len() < 5 || bytes[0] != 0 {
            return Err(AppError::Protocol);
        }
        let length =
            u32::from_be_bytes(bytes[1..5].try_into().map_err(|_| AppError::Protocol)?) as usize;
        if length != bytes.len() - 5 {
            return Err(AppError::Protocol);
        }
        let value = protocol.decode(route, &bytes[5..])?;
        if route == WHOAMI
            && value.get("playerId").and_then(Value::as_str)
                != self.account.as_ref().map(|(id, _)| id.as_str())
        {
            return Err(AppError::Protocol);
        }
        if route == VERSION {
            let version = value
                .get("version")
                .and_then(Value::as_str)
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 256
                        && s.parse::<hyper::header::HeaderValue>().is_ok()
                })
                .ok_or(AppError::Protocol)?;
            let mut state = self.state.lock().await;
            state.observation.master_version = Some(version.to_string());
            state.observation.resource_version = value
                .get("resourceVersion")
                .and_then(Value::as_str)
                .filter(|v| crate::master::safe_version(v))
                .map(str::to_owned);
        }
        self.promote_snapshot(&metadata, &protocol.status.version)
            .await;
        Ok(value)
    }
    async fn observe(&self, md: &HeaderMap, status: Option<u16>) {
        let mut s = self.state.lock().await;
        s.observation.observed_at = Some(Utc::now());
        s.observation.grpc_status = status;
        s.observation.application_code = header(md, "x-sirius-error-code")
            .filter(|v| {
                v.len() <= 64
                    && v.bytes()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
            })
            .map(str::to_owned);
        s.observation.maintenance =
            s.observation.application_code.as_deref() == Some("UNDER_MAINTENANCE");
        s.observation.server_time = header(md, "x-server-time")
            .filter(|v| DateTime::parse_from_rfc3339(v).is_ok())
            .map(str::to_owned);
        if status != Some(0) {
            s.snapshot_stale = true;
        }
        if let Some(root) = header(md, "x-sirius-env") {
            if root != s.cdn_root {
                s.snapshot_stale = true;
                s.credential_valid = false;
            }
            s.cdn_root = root.to_owned();
        }
        if let Some(credential) = header(md, "x-sirius-cred") {
            s.credential_valid = self
                .cdn_secrets
                .get(&s.cdn_root)
                .is_some_and(|c| c == credential);
            if !s.credential_valid {
                s.snapshot_stale = true;
            }
        }
    }
    async fn promote_snapshot(&self, md: &HeaderMap, protocol_version: &str) {
        let Some(raw) = header(md, "x-asset-version") else {
            return;
        };
        let mut s = self.state.lock().await;
        s.snapshot_stale = true;
        let Ok((version, hash)) =
            resources::select_platform(raw, &self.config.client_version, self.config.platform())
        else {
            return;
        };
        let Some(reference) = self.config.cdn_credential_env.get(&s.cdn_root) else {
            return;
        };
        if !s.credential_valid {
            return;
        }
        s.snapshot = Some(ResourceSnapshot {
            schema_version: 2,
            region: self.config.region,
            environment: self.config.environment.clone(),
            platform: self.config.platform().name(),
            client_version: self.config.client_version.clone(),
            protocol_version: protocol_version.into(),
            master_version: s.observation.master_version.clone(),
            resource_version: version,
            platform_hash: hash,
            effective_cdn_root: s.cdn_root.clone(),
            credential_ref: reference.clone(),
            observed_at: Utc::now(),
            source: "remote",
        });
        s.snapshot_stale = false;
    }
}
