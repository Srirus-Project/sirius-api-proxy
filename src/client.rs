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
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{Mutex, RwLock as AsyncRwLock};

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

#[derive(Clone, Default, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
    master_sync_wake: tokio::sync::Notify,
    master_publication_wake: tokio::sync::Notify,
    node_routing: Option<crate::node_routing::Router>,
    config: Config,
    http: Client<crate::transport::TlsConnector, Full<Bytes>>,
    protocol: RwLock<Arc<ProtocolBundle>>,
    reload_lock: Mutex<()>,
    accounts: std::sync::Mutex<crate::accounts::Pool>,
    cdn_secrets: BTreeMap<String, String>,
    state: Mutex<State>,
    // Optional account serialization, independent of the protocol activation barrier.
    call_lock: Mutex<()>,
    protocol_calls: AsyncRwLock<()>,
    bootstrap_lock: Mutex<()>,
    timeout: Duration,
    inflight: tokio::sync::Semaphore,
    response_cache: crate::response_cache::Cache,
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
        let transport = crate::transport::Connector::new(&config.upstream)?;
        let connector = if test_http {
            builder
                .https_or_http()
                .enable_http2()
                .wrap_connector(transport)
        } else {
            builder
                .https_only()
                .enable_http2()
                .wrap_connector(transport)
        };
        let connector =
            crate::transport::TlsConnector::new(connector, config.upstream.connect_timeout_ms);
        let http = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build(connector);
        let accounts = crate::accounts::Pool::load(&config, 1)?;
        let cdn_secrets = config
            .cdn_credential_env
            .iter()
            .filter_map(|(root, name)| secret(name).ok().map(|s| (root.clone(), s)))
            .collect::<BTreeMap<_, _>>();
        let state = State {
            master_update: json!({"status": if config.master_update.is_some() || config.master_sync.is_some() {"pending"} else {"disabled"}}),
            observation: Observation::default(),
            snapshot: None,
            snapshot_stale: true,
            cdn_root: config.default_cdn_root.clone(),
            credential_valid: cdn_secrets.contains_key(&config.default_cdn_root),
        };
        let timeout = Duration::from_millis(config.upstream.timeout_ms);
        let inflight = tokio::sync::Semaphore::new(config.upstream.max_inflight);
        let response_cache = crate::response_cache::Cache::new(config.response_cache.clone())?;
        let node_routing = config
            .node_routing
            .clone()
            .map(|routing| crate::node_routing::Router::new(routing, config.region))
            .transpose()?;
        Ok(Arc::new(Self {
            master_sync_wake: tokio::sync::Notify::new(),
            master_publication_wake: tokio::sync::Notify::new(),
            node_routing,
            config,
            http,
            accounts: std::sync::Mutex::new(accounts),
            cdn_secrets,
            state: Mutex::new(state),
            call_lock: Mutex::new(()),
            protocol_calls: AsyncRwLock::new(()),
            bootstrap_lock: Mutex::new(()),
            timeout,
            inflight,
            response_cache,
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
    pub(crate) fn request_master_sync(
        &self,
        hint: &crate::master_sync::UpdateHint,
    ) -> Result<(), AppError> {
        if self.config.master_sync.is_none() {
            return Err(AppError::MasterUnavailable);
        }
        if hint.scope.region != self.config.region
            || hint.scope.environment != self.config.environment
            || hint.scope.platform != self.config.platform()
            || hint.content_sha256.len() != 64
            || !hint.content_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(AppError::InvalidRequest);
        }
        // Notify retains at most one pending permit: bursts cannot create unbounded jobs.
        self.master_sync_wake.notify_one();
        Ok(())
    }
    pub(crate) async fn master_sync_notified(&self) {
        self.master_sync_wake.notified().await;
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
    pub(crate) async fn master_publication_notified(&self) {
        self.master_publication_wake.notified().await;
    }
    pub(crate) async fn record_master_update(&self, value: Value) {
        let published = value["status"] == "ready" && value["result"]["action"] == "updated";
        self.state.lock().await.master_update = value;
        if published {
            self.master_publication_wake.notify_one();
        }
    }
    pub(crate) async fn refresh_master_target(
        self: &Arc<Self>,
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
    pub async fn refresh_resource_snapshot(self: &Arc<Self>) -> Result<ResourceSnapshot, AppError> {
        let started = Utc::now();
        self.call(VERSION, json!({})).await?;
        let state = self.state.lock().await;
        if state.snapshot_stale
            || state.observation.maintenance
            || state.observation.grpc_status != Some(0)
        {
            return Err(AppError::SnapshotUnavailable);
        }
        let snapshot = state
            .snapshot
            .clone()
            .ok_or(AppError::SnapshotUnavailable)?;
        if snapshot.observed_at < started
            || !(0..=300).contains(&(Utc::now() - snapshot.observed_at).num_seconds())
        {
            return Err(AppError::SnapshotUnavailable);
        }
        Ok(snapshot)
    }
    pub async fn snapshot(&self) -> Result<Value, AppError> {
        let s = self.state.lock().await;
        let snapshot = s.snapshot.as_ref().ok_or(AppError::SnapshotUnavailable)?;
        let stale = s.snapshot_stale || (Utc::now() - snapshot.observed_at).num_seconds() > 300;
        Ok(json!({"snapshot":snapshot,"stale":stale}))
    }
    pub fn account_status(&self) -> Result<Value, AppError> {
        let pool = self
            .accounts
            .lock()
            .map_err(|_| AppError::AccountUnavailable)?;
        Ok(json!({"generation": pool.generation, "accounts": pool.status()}))
    }
    pub async fn reload_accounts(&self) -> Result<Value, AppError> {
        let _reload = self.reload_lock.lock().await;
        let generation = self
            .accounts
            .lock()
            .map_err(|_| AppError::AccountUnavailable)?
            .generation
            .checked_add(1)
            .ok_or(AppError::AccountUnavailable)?;
        let config = self.config.clone();
        let candidate =
            tokio::task::spawn_blocking(move || crate::accounts::Pool::load(&config, generation))
                .await
                .map_err(|_| AppError::AccountUnavailable)??;
        // Activation drains logical calls before replacing locks and credentials.
        let _calls = self.protocol_calls.write().await;
        *self
            .accounts
            .lock()
            .map_err(|_| AppError::AccountUnavailable)? = candidate;
        self.account_status()
    }
    pub fn node_status(&self) -> Value {
        self.node_routing
            .as_ref()
            .map_or_else(|| json!({"enabled":false}), |router| router.status())
    }
    pub async fn public_query(
        self: &Arc<Self>,
        operation: crate::peer::Operation,
    ) -> crate::node_routing::Execution {
        if let Some(router) = &self.node_routing {
            return router.call(self, operation).await;
        }
        let result = match operation.rpc() {
            Ok((route, input)) => self.call(route, input).await,
            Err(e) => Err(e),
        };
        crate::node_routing::Execution {
            result,
            observation: self.observation().await,
        }
    }
    pub async fn public_call(
        self: &Arc<Self>,
        operation: crate::peer::Operation,
    ) -> Result<Value, AppError> {
        self.public_query(operation).await.result
    }
    pub fn peer_identity(&self) -> Result<crate::peer::Identity, AppError> {
        Ok(crate::peer::Identity {
            contract_version: 1,
            region: self.region(),
            environment: self.environment().into(),
            platform: self.platform(),
            client_version: self.config.client_version.clone(),
            protocol_sha256: self.protocol_status()?.sha256,
        })
    }
    pub(crate) async fn call_peer(
        self: &Arc<Self>,
        route: &str,
        input: Value,
        expected_protocol: &str,
    ) -> Result<Value, AppError> {
        self.call_selected(route, input, None, None, Some(expected_protocol))
            .await
    }
    pub async fn call(self: &Arc<Self>, route: &str, input: Value) -> Result<Value, AppError> {
        self.call_selected(route, input, None, None, None).await
    }
    pub async fn call_account(
        self: &Arc<Self>,
        name: &str,
        route: &str,
    ) -> Result<Value, AppError> {
        if !matches!(route, WHOAMI | PLAYER_DATA) {
            return Err(AppError::InvalidRequest);
        }
        self.call_selected(route, json!({}), Some(name), None, None)
            .await
    }
    fn call_selected<'a>(
        self: &'a Arc<Self>,
        route: &'a str,
        input: Value,
        name: Option<&'a str>,
        refresh_key: Option<String>,
        expected_protocol: Option<&'a str>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, AppError>> + Send + 'a>>
    {
        Box::pin(async move {
            if !crate::routes::ROUTES.contains(&route) && route != crate::routes::SERVER_LIST {
                return Err(AppError::InvalidRequest);
            }
            if !self.supported_routes().contains(&route) {
                return Err(AppError::UnsupportedRegionOperation);
            }
            let deadline = tokio::time::Instant::now() + self.timeout;
            let result = async {
                let _permit = tokio::time::timeout_at(deadline, self.inflight.acquire())
                    .await
                    .map_err(|_| AppError::Timeout)?
                    .map_err(|_| AppError::Transport)?;
                let _protocol_call = tokio::time::timeout_at(deadline, self.protocol_calls.read())
                    .await
                    .map_err(|_| AppError::Timeout)?;
                if expected_protocol.is_some_and(|expected| {
                    self.protocol_status()
                        .map_or(true, |status| status.sha256 != expected)
                }) {
                    return Err(AppError::PeerIdentityMismatch);
                }
                let lease = if authenticated(route) {
                    Some(
                        self.accounts
                            .lock()
                            .map_err(|_| AppError::AccountUnavailable)?
                            .select(name, matches!(route, WHOAMI | PLAYER_DATA))
                            .map_err(|e| {
                                if expected_protocol.is_some() {
                                    AppError::PeerAccountUnavailable
                                } else {
                                    e
                                }
                            })?,
                    )
                } else {
                    None
                };
                let account = lease.as_ref().map(|l| l.account.as_ref());

                let mut account_attempted = false;
                let result = tokio::time::timeout_at(deadline, async {
                    let protocol = self
                        .protocol
                        .read()
                        .map_err(|_| AppError::ProtocolDefinition)?
                        .clone();
                    if authenticated(route)
                        && self.state.lock().await.observation.master_version.is_none()
                    {
                        let _bootstrap = self.bootstrap_lock.lock().await;
                        if self.state.lock().await.observation.master_version.is_none() {
                            self.execute(&protocol, VERSION, json!({}), None, deadline)
                                .await?;
                        }
                    }
                    let cache_key = self
                        .response_cache_key(&protocol, route, &input, account)
                        .await?;
                    if refresh_key
                        .as_ref()
                        .is_some_and(|key| cache_key.as_ref() != Some(key))
                    {
                        return Err(AppError::ProtocolDefinition);
                    }
                    if let Some(key) = &cache_key {
                        if refresh_key.is_none() {
                            if let Some(cached) = self.read_cached_state(key, route, deadline).await
                            {
                                if cached.stale {
                                    if let Some(guard) = self.response_cache.try_refresh_guard(key)
                                    {
                                        let client = self.clone();
                                        let key = key.clone();
                                        let route = route.to_owned();
                                        let input = input.clone();
                                        let account_name = account.map(|a| a.name.clone());
                                        tokio::spawn(async move {
                                            let _guard = guard;
                                            let _ = client
                                                .call_selected(
                                                    &route,
                                                    input,
                                                    account_name.as_deref(),
                                                    Some(key),
                                                    None,
                                                )
                                                .await;
                                        });
                                    }
                                }
                                return Ok(cached.value);
                            }
                        } else if let Some(value) = self.read_cached(key, route, deadline).await {
                            return Ok(value);
                        }
                    }
                    let _fill = if let Some(key) = &cache_key {
                        let guard = self.response_cache.fill_guard(key).await;
                        if let Some(value) = self.read_cached(key, route, deadline).await {
                            return Ok(value);
                        }
                        Some(guard)
                    } else {
                        None
                    };
                    let _guard = tokio::time::timeout_at(deadline, async {
                        if self.config.session_lock {
                            Some(match account {
                                Some(a) => a.lock.lock().await,
                                None => self.call_lock.lock().await,
                            })
                        } else {
                            None
                        }
                    })
                    .await
                    .map_err(|_| AppError::Timeout)?;
                    if account.is_some_and(|a| !a.available()) {
                        return Err(AppError::AccountUnavailable);
                    }

                    if let Some(expected) = &refresh_key {
                        if self
                            .response_cache_key(&protocol, route, &input, account)
                            .await?
                            .as_ref()
                            != Some(expected)
                        {
                            return Err(AppError::ProtocolDefinition);
                        }
                    }
                    account_attempted = authenticated(route);
                    if route == PLAYER_DATA {
                        self.execute(&protocol, WHOAMI, json!({}), account, deadline)
                            .await?;
                    }
                    let response = self
                        .execute(&protocol, route, input, account, deadline)
                        .await;
                    if account_attempted {
                        if let Some(lease) = &lease {
                            lease.report(&response, &self.config.account_pool);
                        }
                        account_attempted = false;
                    }
                    let mut value = response?;
                    if let Some(key) = cache_key {
                        // Account-relative ranking fields must never enter shared response storage.
                        if matches!(route, MUSIC_RANKING | CHALLENGE_RANKING) {
                            if let Some(object) = value.as_object_mut() {
                                object.remove("myRank");
                                object.remove("myScore");
                            }
                        }
                        let budget =
                            deadline.saturating_duration_since(tokio::time::Instant::now()) / 4;
                        let _ = tokio::time::timeout(
                            budget,
                            self.response_cache.put_route(route, key, &value),
                        )
                        .await;
                    }
                    Ok(value)
                })
                .await
                .unwrap_or(Err(AppError::Timeout));
                if account_attempted {
                    if let Some(lease) = &lease {
                        lease.report(&result, &self.config.account_pool);
                    }
                }
                result
            }
            .await;
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
        })
    }
    async fn read_cached(
        &self,
        key: &str,
        route: &str,
        deadline: tokio::time::Instant,
    ) -> Option<Value> {
        let budget = deadline.saturating_duration_since(tokio::time::Instant::now()) / 4;
        let mut value = tokio::time::timeout(budget, self.response_cache.get(key))
            .await
            .ok()??;
        if matches!(route, MUSIC_RANKING | CHALLENGE_RANKING) {
            if let Some(object) = value.as_object_mut() {
                object.remove("myRank");
                object.remove("myScore");
            }
        }
        Some(value)
    }
    async fn read_cached_state(
        &self,
        key: &str,
        route: &str,
        deadline: tokio::time::Instant,
    ) -> Option<crate::response_cache::Cached> {
        let budget = deadline.saturating_duration_since(tokio::time::Instant::now()) / 4;
        let mut cached = tokio::time::timeout(budget, self.response_cache.get_with_state(key))
            .await
            .ok()??;
        if matches!(route, MUSIC_RANKING | CHALLENGE_RANKING) {
            if let Some(object) = cached.value.as_object_mut() {
                object.remove("myRank");
                object.remove("myScore");
            }
        }
        Some(cached)
    }
    async fn response_cache_key(
        &self,
        protocol: &ProtocolBundle,
        route: &str,
        input: &Value,
        account: Option<&crate::accounts::Account>,
    ) -> Result<Option<String>, AppError> {
        let Some(ttl) = self.response_cache.ttl(route) else {
            return Ok(None);
        };
        let state = self.state.lock().await;
        if state.observation.maintenance {
            return Ok(None);
        }
        let master = state.observation.master_version.clone();
        drop(state);
        let generation = self
            .accounts
            .lock()
            .map_err(|_| AppError::AccountUnavailable)?
            .generation;
        use sha2::{Digest, Sha256};
        let account_scope = account.map(|a| {
            let bytes =
                serde_json::to_vec(&(&a.player_id, &a.credential)).expect("strings serialize");
            format!("{:x}", Sha256::digest(bytes))
        });
        let scope = json!({"schema":2,"stale_ms":self.response_cache.stale_window(),"region":self.config.region,"environment":self.config.environment,
            "endpoint":self.config.endpoint,"platform":self.config.platform(),"client":self.config.client_version,
            "protocol":protocol.status.sha256,"protocol_generation":protocol.status.generation,
            "account":account_scope,"account_generation":generation,"master":master,"route":route,"input":input,"ttl_ms":ttl});
        let bytes = serde_json::to_vec(&scope).map_err(|_| AppError::Protocol)?;
        Ok(Some(format!("{:x}", Sha256::digest(bytes))))
    }
    async fn execute(
        &self,
        protocol: &ProtocolBundle,
        route: &str,
        input: Value,
        account: Option<&crate::accounts::Account>,
        deadline: tokio::time::Instant,
    ) -> Result<Value, AppError> {
        let attempts = if matches!(route, VERSION | ANNOUNCEMENTS | ANNOUNCEMENT | SERVER_LIST) {
            self.config.upstream.anonymous_attempts
        } else {
            1
        };
        let mut attempt = 0;
        loop {
            let result = self
                .execute_once(protocol, route, input.clone(), account, deadline)
                .await;
            attempt += 1;
            if attempt >= attempts
                || !matches!(result, Err(AppError::Transport | AppError::Grpc(14)))
                || self.state.lock().await.observation.maintenance
            {
                return result;
            }
            // Keep retries inside the logical call's deadline, protocol generation and permit.
            let delay = self
                .config
                .upstream
                .retry_delay_ms
                .saturating_mul(1 << (attempt - 1));
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    }
    async fn execute_once(
        &self,
        protocol: &ProtocolBundle,
        route: &str,
        input: Value,
        account: Option<&crate::accounts::Account>,
        deadline: tokio::time::Instant,
    ) -> Result<Value, AppError> {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(AppError::Timeout);
        }
        let grpc_timeout = format!(
            "{}m",
            remaining
                .as_millis()
                .saturating_add(u128::from(
                    !remaining.subsec_nanos().is_multiple_of(1_000_000)
                ))
                .max(1)
        );
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
            .header("grpc-timeout", grpc_timeout)
            .header("x-platform", self.config.platform().header())
            .header("x-client-version", &self.config.client_version)
            .header("x-request-id", uuid::Uuid::new_v4().to_string());
        if let Some(version) = &self.state.lock().await.observation.master_version {
            request = request.header("x-master-version", version);
        }
        // Anonymous endpoints never receive game credentials.
        if authenticated(route) {
            let account = account.ok_or(AppError::AccountUnavailable)?;
            request = request
                .header("x-player-id", &account.player_id)
                .header("x-player-credential", &account.credential);
        }
        let response = self
            .http
            .request(
                request
                    .body(Full::new(Bytes::from(frame)))
                    .map_err(|_| AppError::Protocol)?,
            )
            .await
            .map_err(crate::transport::classify)?;
        let http_ok = response.status().is_success();
        let mut metadata = response.headers().clone();
        let content_ok = header(&metadata, "content-type")
            .is_some_and(|s| s == "application/grpc" || s.starts_with("application/grpc+"));
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| AppError::Transport)?;
            if let Some(data) = frame.data_ref() {
                if bytes.len().saturating_add(data.len()) > self.config.upstream.max_response_bytes
                {
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
                != account.map(|a| a.player_id.as_str())
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
