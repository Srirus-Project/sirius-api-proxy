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
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioTimer},
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{Mutex, RwLock as AsyncRwLock};

use crate::{
    accounts::{Auth, Charge},
    path_health::{Outcome, PathHealth, Ticket},
};

/// `PLAYER_NOT_FOUND` on a lookup of another player names the target, not the account's own
/// player: it must not drop the session or count toward disabling the account. Global: live
/// (2026-09-27) on profile and event_deck, any gRPC status. JP: static evidence only (iOS 1.0.3
/// client): FindByProfileID is the read whose wrapper expects PLAYER_NOT_FOUND, and the client
/// reads application codes only on gRPC 2 or 7.
pub(crate) fn target_not_found(
    global: bool,
    route: &str,
    response: &Result<Value, AppError>,
    code: Option<&str>,
) -> bool {
    let matched = if global {
        matches!(route, PROFILE | EVENT_DECK) && matches!(response, Err(AppError::Grpc(_)))
    } else {
        route == PROFILE && matches!(response, Err(AppError::Grpc(2 | 7)))
    };
    matched && code == Some("PLAYER_NOT_FOUND")
}
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
    master_git: Value,
    master_database: Value,
    master_update: Value,
    observation: Observation,
    /// Master data version and asset version taken together from one VERSION response.
    master_pair: Option<(String, Option<String>)>,
    snapshot: Option<ResourceSnapshot>,
    snapshot_stale: bool,
    cdn_root: String,
    credential_valid: bool,
    /// Global only: body `resourceVersion` of the last successful VERSION response and when it
    /// was observed. `unknown` or unsafe values are never recorded.
    global_version: Option<(String, DateTime<Utc>)>,
    /// Global only: last catalog `.hash` attempt as (root, resource version, hash or failure,
    /// attempted at). Failures are remembered too, so polling cannot multiply CDN requests.
    catalog_hash: Option<(String, String, Option<String>, tokio::time::Instant)>,
    /// When the version headers from the last successful VERSION are due for a refresh.
    version_fresh_until: Option<tokio::time::Instant>,
    /// The game answered `MASTER_VERSION_MISMATCH` to the current `x-master-version`.
    version_suspect: bool,
    /// No refresh before this instant (after a failed refresh, or one that did not change the
    /// version the game called stale).
    version_retry_at: Option<tokio::time::Instant>,
    /// `CLIENT_UPDATE_REQUIRED` was logged and no gRPC success has been seen since.
    client_update_required: bool,
}
/// Wait between Master version refreshes that failed or did not change the version.
const VERSION_RETRY: Duration = Duration::from_secs(30);
/// `x-sirius-error-code` of a request whose `x-master-version` is no longer current.
const MASTER_VERSION_MISMATCH: &str = "MASTER_VERSION_MISMATCH";
/// `x-sirius-error-code` of a request whose `x-client-version` the game no longer accepts.
const CLIENT_UPDATE_REQUIRED: &str = "CLIENT_UPDATE_REQUIRED";
/// Application codes that describe this proxy's version headers, not the game account: both
/// literals are in the JP 1.0.3 and Global 1.0.1 clients, next to their version check. They are
/// recognized whatever gRPC status comes with them, which is not statically known.
pub(crate) fn version_signal(code: Option<&str>) -> bool {
    matches!(code, Some(MASTER_VERSION_MISMATCH | CLIENT_UPDATE_REQUIRED))
}
/// Outcome of the response-cache lookup that runs before admission.
enum Precheck {
    /// A fresh or in-window stale entry answers the call.
    Hit(Value),
    /// Nothing is retained under this key; the admitted call need not look it up again.
    Miss(String),
    /// The lookup did not apply or did not finish; the admitted call looks itself.
    Skipped,
}
/// Account-relative ranking fields never leave shared response storage or shared executions.
fn strip_account_fields(route: &str, value: &mut Value) {
    if matches!(route, MUSIC_RANKING | CHALLENGE_RANKING) {
        if let Some(object) = value.as_object_mut() {
            object.remove("myRank");
            object.remove("myScore");
        }
    }
}
/// What a call needs before its RPC can carry usable version headers.
#[derive(Clone, Copy, PartialEq)]
enum VersionNeed {
    Fresh,
    /// No version yet: an authenticated call cannot be sent without one.
    Required,
    /// Too old or reported stale; `wait` when stale, so callers do not repeat the stale header.
    Refresh {
        wait: bool,
    },
}

pub struct GameClient {
    master_sync_wake: tokio::sync::Notify,
    master_git_wake: tokio::sync::Notify,
    master_database_wake: tokio::sync::Notify,
    master_database_reader: Option<crate::master_database::Reader>,
    client_auth: Option<crate::client_auth::Authenticator>,
    master_publication_wake: tokio::sync::Notify,
    node_routing: Option<crate::node_routing::Router>,
    config: Config,
    http: Client<crate::transport::TlsConnector, Full<Bytes>>,
    protocol: RwLock<Arc<ProtocolBundle>>,
    reload_lock: Mutex<()>,
    accounts: std::sync::Mutex<crate::accounts::Pool>,
    cdn_secrets: BTreeMap<String, String>,
    state: Mutex<State>,
    /// Anonymous call slots while `session_lock` is true (accounts use their own lock).
    anonymous_calls: tokio::sync::Semaphore,
    /// Shared executions of identical in-flight public reads (see `coalesces`).
    flights: crate::single_flight::SingleFlight<Result<Arc<Value>, AppError>>,
    protocol_calls: AsyncRwLock<()>,
    bootstrap_lock: Mutex<()>,
    timeout: Duration,
    inflight: tokio::sync::Semaphore,
    response_cache: crate::response_cache::Cache,
    /// Global resource snapshot CDN client (`resource_snapshot`); no redirects.
    snapshot_http: Option<reqwest::Client>,
    /// Serializes Global snapshot builds so at most one `.hash` request is in flight.
    snapshot_build: Mutex<()>,
    /// Global SDK client, present when an account uses `global_identity_file`.
    sdk: Option<crate::global_sdk::SdkClient>,
    /// Health of this region's game path, shared by every account and anonymous call.
    path: PathHealth,
    /// Global: health of the SDK login path (present with `sdk`).
    sdk_path: Option<PathHealth>,
    /// Master table read admission, shared by every region of the process.
    table_reads: crate::master_admission::Gate,
    /// Global SDK sessions, shared by every region of the deployment.
    sdk_sessions: Arc<crate::sdk_session::SdkSessions>,
}
/// One upstream attempt on a path. Its outcome is recorded when it is dropped, so an attempt
/// abandoned at the logical deadline still counts as a timeout; one cancelled earlier, or never
/// sent, counts for nothing.
struct UpstreamAttempt<'a> {
    client: &'a GameClient,
    sdk: bool,
    source: Option<String>,
    deadline: tokio::time::Instant,
    sent: bool,
    /// Explicit outcome; `Some(None)` is neutral.
    outcome: Option<Option<Outcome>>,
    /// The response's `x-sirius-error-code` (only `[A-Z0-9_]`).
    code: Option<String>,
}
impl UpstreamAttempt<'_> {
    fn fault(&mut self, error: &AppError) {
        self.outcome = Some(Some(Outcome::Fault(error.code())));
    }
}
impl Drop for UpstreamAttempt<'_> {
    fn drop(&mut self) {
        let outcome = match self.outcome.take() {
            Some(outcome) => outcome,
            None if self.sent && tokio::time::Instant::now() >= self.deadline => {
                Some(Outcome::Fault(if self.sdk {
                    crate::global_sdk::SdkError::Transport.code()
                } else {
                    AppError::Timeout.code()
                }))
            }
            None => None,
        };
        if let Some(outcome) = outcome {
            self.client
                .record_path(self.sdk, self.source.as_deref(), outcome);
        }
    }
}
fn header<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers.get(key)?.to_str().ok()
}

impl GameClient {
    pub fn new(config: Config) -> Result<Arc<Self>, AppError> {
        config.validate()?;
        Self::build(config, false, None)
    }
    /// A region of a deployment whose regions share `sdk_sessions`.
    pub(crate) fn with_sdk_sessions(
        config: Config,
        sdk_sessions: Arc<crate::sdk_session::SdkSessions>,
    ) -> Result<Arc<Self>, AppError> {
        config.validate()?;
        Self::build(config, false, Some(sdk_sessions))
    }
    fn build(
        config: Config,
        test_http: bool,
        sdk_sessions: Option<Arc<crate::sdk_session::SdkSessions>>,
    ) -> Result<Arc<Self>, AppError> {
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
        // The timer is always set: hyper panics if keepalive runs without one. PINGs go out
        // only while a call is open on a connection silent for the interval; a missed
        // acknowledgement closes the connection (Transport) so the next call reconnects.
        let mut http = Client::builder(TokioExecutor::new());
        http.http2_only(true)
            .timer(TokioTimer::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(Duration::from_secs(90));
        if let Some((interval, ack)) = config.upstream.http2_keepalive() {
            http.http2_keep_alive_interval(interval)
                .http2_keep_alive_timeout(ack)
                .http2_keep_alive_while_idle(false);
        }
        let http = http.build(connector);
        let sdk_sessions = match sdk_sessions {
            Some(sessions) => sessions,
            None => crate::sdk_session::SdkSessions::open(
                config
                    .global_login
                    .as_ref()
                    .and_then(|l| l.state_directory.as_deref()),
            )?,
        };
        let accounts = crate::accounts::Pool::load_with(&config, 1, &sdk_sessions)?;
        let sdk = sdk_client(&config)?;
        // Path health outlives account and protocol reloads.
        let path = PathHealth::new(&config.account_pool, true);
        let sdk_path = sdk
            .as_ref()
            .map(|_| PathHealth::new(&config.account_pool, false));
        let cdn_secrets = config
            .cdn_credential_env
            .iter()
            .filter_map(|(root, name)| secret(name).ok().map(|s| (root.clone(), s)))
            .collect::<BTreeMap<_, _>>();
        let state = State {
            master_database: json!({"status": if config.master_database.is_some() {"pending"} else {"disabled"}}),
            master_git: json!({"status": if config.master_git.is_some() {"pending"} else {"disabled"}}),
            master_update: json!({"status": if config.master_update.is_some() || config.master_sync.is_some() {"pending"} else {"disabled"}}),
            observation: Observation::default(),
            master_pair: None,
            snapshot: None,
            snapshot_stale: true,
            cdn_root: config.default_cdn_root.clone(),
            credential_valid: cdn_secrets.contains_key(&config.default_cdn_root),
            global_version: None,
            catalog_hash: None,
            version_fresh_until: None,
            version_suspect: false,
            version_retry_at: None,
            client_update_required: false,
        };
        let snapshot_http = config
            .resource_snapshot
            .as_ref()
            .map(|c| c.network.client())
            .transpose()
            .map_err(|_| AppError::Config("invalid resource snapshot network configuration"))?;
        let timeout = Duration::from_millis(config.upstream.timeout_ms);
        let inflight = tokio::sync::Semaphore::new(config.upstream.max_inflight);
        let anonymous_calls = tokio::sync::Semaphore::new(config.upstream.anonymous_slots());
        let response_cache = crate::response_cache::Cache::new(config.response_cache.clone())?;
        let node_routing = config
            .node_routing
            .clone()
            .map(|routing| crate::node_routing::Router::new(routing, config.region))
            .transpose()?;
        let client_auth = config
            .client_auth
            .as_ref()
            .map(|auth| {
                let protected = crate::master_notify::protected_tokens(&[&config]);
                crate::client_auth::Authenticator::new(auth, config.region, &protected)
            })
            .transpose()?;
        Ok(Arc::new(Self {
            client_auth,
            master_sync_wake: tokio::sync::Notify::new(),
            master_publication_wake: tokio::sync::Notify::new(),
            master_git_wake: tokio::sync::Notify::new(),
            master_database_wake: tokio::sync::Notify::new(),
            master_database_reader: config
                .master_database
                .as_ref()
                .map(|c| crate::master_database::Reader::new(&c.connection))
                .transpose()
                .map_err(|_| AppError::Config("invalid Master database read configuration"))?,
            node_routing,
            config,
            http,
            accounts: std::sync::Mutex::new(accounts),
            cdn_secrets,
            state: Mutex::new(state),
            anonymous_calls,
            flights: crate::single_flight::SingleFlight::new(),
            protocol_calls: AsyncRwLock::new(()),
            bootstrap_lock: Mutex::new(()),
            timeout,
            inflight,
            response_cache,
            protocol: RwLock::new(Arc::new(protocol)),
            reload_lock: Mutex::new(()),
            snapshot_http,
            snapshot_build: Mutex::new(()),
            sdk,
            path,
            sdk_path,
            table_reads: crate::master_admission::Gate::tables(),
            sdk_sessions,
        }))
    }
    pub(crate) fn table_reads(&self) -> &crate::master_admission::Gate {
        &self.table_reads
    }
    pub(crate) fn client_auth(&self) -> Option<&crate::client_auth::Authenticator> {
        self.client_auth.as_ref()
    }
    #[cfg(test)]
    pub(crate) fn for_test(config: Config) -> Arc<Self> {
        Self::build(config, true, None).unwrap()
    }
    #[cfg(test)]
    pub(crate) fn for_test_with_sdk_sessions(
        config: Config,
        sdk_sessions: Arc<crate::sdk_session::SdkSessions>,
    ) -> Arc<Self> {
        Self::build(config, true, Some(sdk_sessions)).unwrap()
    }
    #[cfg(test)]
    pub(crate) fn test_path(&self, sdk: bool) -> &PathHealth {
        if sdk {
            self.sdk_path.as_ref().unwrap()
        } else {
            &self.path
        }
    }
    #[cfg(test)]
    pub(crate) fn set_test_timeout(client: &mut Arc<Self>, duration: Duration) {
        Arc::get_mut(client).unwrap().timeout = duration;
    }
    #[cfg(test)]
    pub(crate) fn set_test_table_gate(client: &mut Arc<Self>, gate: crate::master_admission::Gate) {
        Arc::get_mut(client).unwrap().table_reads = gate;
    }
    /// Moves the version freshness deadline and the refresh retry window `by` into the past.
    #[cfg(test)]
    pub(crate) async fn age_version_for_test(&self, by: Duration) {
        let mut state = self.state.lock().await;
        let back = |at: tokio::time::Instant| at.checked_sub(by).expect("monotonic clock");
        state.version_fresh_until = state.version_fresh_until.map(back);
        state.version_retry_at = state.version_retry_at.map(back);
    }
    /// (time until the version is due for a refresh, suspect, retry window active).
    #[cfg(test)]
    pub(crate) async fn version_state_for_test(&self) -> (Option<Duration>, bool, bool) {
        let state = self.state.lock().await;
        let now = tokio::time::Instant::now();
        (
            state
                .version_fresh_until
                .map(|at| at.saturating_duration_since(now)),
            state.version_suspect,
            state.version_retry_at.is_some_and(|at| at > now),
        )
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
        state.master_pair = None;
        state.global_version = None;
        state.version_fresh_until = None;
        state.version_suspect = false;
        state.version_retry_at = None;
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
    pub(crate) fn master_database_reader(&self) -> Option<&crate::master_database::Reader> {
        self.master_database_reader.as_ref()
    }
    pub async fn master_database_status(&self) -> Value {
        self.state.lock().await.master_database.clone()
    }
    pub(crate) async fn record_master_database(&self, value: Value) {
        self.state.lock().await.master_database = value;
    }
    pub(crate) async fn master_database_notified(&self) {
        self.master_database_wake.notified().await;
    }
    pub async fn master_git_status(&self) -> Value {
        self.state.lock().await.master_git.clone()
    }
    pub(crate) async fn record_master_git(&self, value: Value) {
        self.state.lock().await.master_git = value;
    }
    pub(crate) async fn master_git_notified(&self) {
        self.master_git_wake.notified().await;
    }
    pub(crate) async fn master_publication_notified(&self) {
        self.master_publication_wake.notified().await;
    }
    pub(crate) async fn record_master_update(&self, value: Value) {
        let published = value["status"] == "ready" && value["result"]["action"] == "updated";
        self.state.lock().await.master_update = value;
        if published {
            self.master_publication_wake.notify_one();
            self.master_git_wake.notify_one();
            self.master_database_wake.notify_one();
        }
    }
    pub(crate) async fn refresh_master_target(
        self: &Arc<Self>,
    ) -> Result<crate::master_update::MasterTarget, AppError> {
        self.call(VERSION, json!({})).await?;
        let state = self.state.lock().await;
        // Both versions come from the same VERSION response; never pair a master version
        // with an asset version from another observation.
        let (version, resource_version) = state
            .master_pair
            .as_ref()
            .filter(|(v, _)| crate::master::safe_version(v))
            .ok_or(AppError::MasterUnavailable)?;
        if state.observation.grpc_status != Some(0) || state.observation.maintenance {
            return Err(AppError::MasterUnavailable);
        }
        let anonymous = self
            .config
            .master_update
            .as_ref()
            .is_some_and(|u| u.cdn_authorization == crate::config::CdnAuthorization::None);
        let password = if anonymous {
            // Anonymous access applies only to the configured Global root without a
            // credential reference; a server-announced different root is never followed.
            if !self.config.region.master_supported()
                || self.config.region.family() != "global"
                || state.cdn_root != self.config.default_cdn_root
                || self.config.cdn_credential_env.contains_key(&state.cdn_root)
            {
                return Err(AppError::MasterUnavailable);
            }
            None
        } else {
            if !state.credential_valid {
                return Err(AppError::MasterUnavailable);
            }
            Some(
                self.cdn_secrets
                    .get(&state.cdn_root)
                    .ok_or(AppError::MasterUnavailable)?
                    .clone(),
            )
        };
        Ok(crate::master_update::MasterTarget {
            version: version.clone(),
            resource_version: resource_version.clone(),
            root: state.cdn_root.clone(),
            password,
        })
    }
    pub async fn refresh_resource_snapshot(self: &Arc<Self>) -> Result<ResourceSnapshot, AppError> {
        let started = Utc::now();
        self.call(VERSION, json!({})).await?;
        if self.config.region.family() == "global" {
            self.ensure_global_snapshot().await?;
        }
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
        if self.config.region.family() == "global" && self.config.resource_snapshot.is_some() {
            // Failures leave the previous snapshot (reported stale) or none; they are logged.
            let _ = self.ensure_global_snapshot().await;
        }
        let s = self.state.lock().await;
        let snapshot = s.snapshot.as_ref().ok_or(AppError::SnapshotUnavailable)?;
        let stale = s.snapshot_stale || (Utc::now() - snapshot.observed_at).num_seconds() > 300;
        Ok(json!({"snapshot":snapshot,"stale":stale}))
    }
    #[cfg(test)]
    pub(crate) fn cool_down_accounts_for_test(&self, duration: Duration) {
        self.accounts
            .lock()
            .unwrap()
            .cool_down_all_for_test(duration);
    }
    pub fn account_status(&self) -> Result<Value, AppError> {
        let mut status = {
            let pool = self
                .accounts
                .lock()
                .map_err(|_| AppError::AccountUnavailable)?;
            json!({"generation": pool.generation, "accounts": pool.status()})
        };
        status["path"] = self.path.status();
        if let Some(sdk) = &self.sdk_path {
            status["sdk_path"] = sdk.status();
        }
        Ok(status)
    }
    /// Records one attempt on the game path (or the SDK path), withdraws the account charges of
    /// a streak that has just been attributed to the path, and logs transitions.
    fn record_path(&self, sdk: bool, source: Option<&str>, outcome: Outcome) {
        let Some(path) = (if sdk {
            self.sdk_path.as_ref()
        } else {
            Some(&self.path)
        }) else {
            return;
        };
        let change = path.record(source, outcome);
        if let Some(streak) = change.attributed.filter(|_| !sdk) {
            if let Ok(pool) = self.accounts.lock() {
                pool.revoke_path_failures(streak, &self.config.account_pool);
            }
        }
        let region = self.config.region.name();
        let error_code = match outcome {
            Outcome::Fault(code) => Some(code),
            Outcome::Healthy => None,
        };
        let cooldown_ms = path.interval().as_millis() as u64;
        match change.transition {
            crate::path_health::Transition::None => {}
            crate::path_health::Transition::Opened => tracing::warn!(
                event = if sdk { "sdk_path_opened" } else { "upstream_path_opened" },
                region,
                error_code,
                cooldown_ms,
                "Upstream path reached the failure threshold; refusing calls until a probe succeeds"
            ),
            crate::path_health::Transition::ProbeFailed => tracing::warn!(
                event = if sdk {
                    "sdk_path_probe_failed"
                } else {
                    "upstream_path_probe_failed"
                },
                region,
                error_code,
                cooldown_ms,
                "Upstream path probe failed; still refusing calls"
            ),
            crate::path_health::Transition::Recovered => tracing::info!(
                event = if sdk {
                    "sdk_path_recovered"
                } else {
                    "upstream_path_recovered"
                },
                region,
                "Upstream path recovered"
            ),
        }
    }
    /// Admits a logical call on the game path once; the ticket is then held for the whole call,
    /// so its bootstrap, retries and prerequisite RPCs continue even if the path opens.
    fn admit_path<'s>(&'s self, ticket: &mut Option<Ticket<'s>>) -> Result<(), AppError> {
        if ticket.is_none() {
            *ticket = Some(self.path.admit().ok_or(AppError::UpstreamUnavailable)?);
        }
        Ok(())
    }
    /// Applies a call outcome to the leased account, charging path-class faults as the path
    /// health decides.
    fn report_lease(
        &self,
        lease: &crate::accounts::Lease,
        result: &Result<Value, AppError>,
        code: Option<&str>,
    ) {
        let charge = self.path.charge();
        lease.report(
            result,
            code,
            &self.config.account_pool,
            self.config.global_login.as_ref(),
            charge,
        );
        self.settle_charge(&lease.account, charge);
    }
    /// The streak may have been attributed to the path between `charge()` and the charge
    /// landing; withdraw it then, as the attribution itself would have.
    fn settle_charge(&self, account: &crate::accounts::Account, charge: Charge) {
        if let Charge::Account {
            streak: Some(streak),
        } = charge
        {
            if self.path.attributed(streak) {
                account.revoke_path_failures(streak, &self.config.account_pool);
            }
        }
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
        let sessions = self.sdk_sessions.clone();
        let candidate = tokio::task::spawn_blocking(move || {
            crate::accounts::Pool::load_with(&config, generation, &sessions)
        })
        .await
        .map_err(|_| AppError::AccountUnavailable)??;
        // Activation drains logical calls before replacing locks and credentials.
        let _calls = self.protocol_calls.write().await;
        {
            let mut pool = self
                .accounts
                .lock()
                .map_err(|_| AppError::AccountUnavailable)?;
            candidate.inherit_login_history(&pool);
            *pool = candidate;
        }
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
            let global = self.config.region.family() == "global";
            // Global never sends Whoami (disabled in production): the account identity is the
            // PlayerLogin result, served without an upstream identity RPC.
            let identity_only = global && route == WHOAMI;
            if !identity_only && !self.supported_routes().contains(&route) {
                return Err(AppError::UnsupportedRegionOperation);
            }
            let deadline = tokio::time::Instant::now() + self.timeout;
            // Response-cache hits are answered first, so they never wait for or hold a shared
            // execution, admission, the protocol barrier or an account lease.
            let mut checked = None;
            if name.is_none() && refresh_key.is_none() && !identity_only {
                match self
                    .cached_before_admission(route, &input, expected_protocol, deadline)
                    .await
                {
                    Precheck::Hit(value) => return Ok(value),
                    Precheck::Miss(key) => checked = Some(key),
                    Precheck::Skipped => {}
                }
            }
            // Identical public reads share one execution, before admission, the protocol barrier
            // and account selection, so a joined caller holds none of them. Named-account and
            // cache-refresh calls always run on their own.
            if name.is_none() && refresh_key.is_none() && self.coalesces(route) {
                let key = self.flight_key(route, &input, expected_protocol)?;
                return self
                    .flights
                    .run(key, deadline, Err(AppError::Timeout), || async move {
                        let mut result = self
                            .call_admitted(
                                route,
                                input,
                                None,
                                None,
                                expected_protocol,
                                checked,
                                deadline,
                            )
                            .await;
                        // A shared outcome never carries account-relative ranking fields.
                        if let Ok(value) = &mut result {
                            strip_account_fields(route, value);
                        }
                        result.map(Arc::new)
                    })
                    .await
                    .map(|value| Value::clone(&value));
            }
            self.call_admitted(
                route,
                input,
                name,
                refresh_key,
                expected_protocol,
                checked,
                deadline,
            )
            .await
        })
    }
    /// Public reads that share one execution among identical concurrent callers. Rankings spend
    /// a game account, so they join only with `upstream.coalesce_public_reads`.
    pub(crate) fn coalesces(&self, route: &str) -> bool {
        match route {
            VERSION | SERVER_LIST | ANNOUNCEMENTS | ANNOUNCEMENT => true,
            EVENT_RANKING | MUSIC_RANKING | CHALLENGE_RANKING => {
                self.config.upstream.coalesce_public_reads
            }
            _ => false,
        }
    }
    /// Identity of a shared execution: everything that selects the upstream call and its schema,
    /// including the protocol a peer caller asserted.
    fn flight_key(
        &self,
        route: &str,
        input: &Value,
        expected_protocol: Option<&str>,
    ) -> Result<[u8; 32], AppError> {
        use sha2::{Digest, Sha256};
        let (sha256, generation) = {
            let protocol = self
                .protocol
                .read()
                .map_err(|_| AppError::ProtocolDefinition)?;
            (protocol.status.sha256.clone(), protocol.status.generation)
        };
        let scope = json!({"schema":1,"region":self.config.region,"environment":self.config.environment,
            "endpoint":self.config.endpoint,"platform":self.config.platform(),"client":self.config.client_version,
            "protocol":sha256,"protocol_generation":generation,"expected_protocol":expected_protocol,
            "route":route,"input":input});
        let bytes = serde_json::to_vec(&scope).map_err(|_| AppError::Protocol)?;
        Ok(Sha256::digest(bytes).into())
    }
    #[cfg(test)]
    pub(crate) fn test_flight_key(&self, route: &str, expected_protocol: Option<&str>) -> [u8; 32] {
        self.flight_key(route, &json!({}), expected_protocol)
            .unwrap()
    }
    /// Response-cache lookup before admission, the protocol barrier and account leasing. The key
    /// is the one the admitted call would use with the account `select` would lease now, so a
    /// hit never counts as an active call or reports account health. Calls that need a version
    /// first, during known maintenance or with a mismatched peer schema skip it and take the
    /// admitted path, which answers them as before. With every account cooling or disabled, an
    /// entry retained for any pool account may answer inside the stale window, never refreshed.
    async fn cached_before_admission(
        self: &Arc<Self>,
        route: &str,
        input: &Value,
        expected_protocol: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> Precheck {
        if self.response_cache.ttl(route).is_none() {
            return Precheck::Skipped;
        }
        let lookup = async {
            // No protocol barrier: keys embed the protocol and account generations, so a snapshot
            // taken during a reload only yields a key no writer produced.
            let Ok(protocol) = self.protocol.read().map(|p| p.clone()) else {
                return Precheck::Skipped;
            };
            if expected_protocol.is_some_and(|expected| protocol.status.sha256 != expected) {
                return Precheck::Skipped;
            }
            // An age-only refresh keeps the current key, as for callers that do not wait for it.
            if !matches!(
                self.version_need(route).await,
                VersionNeed::Fresh | VersionNeed::Refresh { wait: false }
            ) {
                return Precheck::Skipped;
            }
            let (account, quarantined) = if authenticated(route) {
                let Ok(pool) = self.accounts.lock() else {
                    return Precheck::Skipped;
                };
                match pool.peek_public() {
                    Some(account) => (Some(account), None),
                    None => (None, Some(pool.rotation())),
                }
            } else {
                (None, None)
            };
            if let Some(rotation) = quarantined {
                return self
                    .retained_while_quarantined(&protocol, route, input, &rotation, deadline)
                    .await;
            }
            let Ok(Some(key)) = self
                .response_cache_key(&protocol, route, input, account.as_deref())
                .await
            else {
                return Precheck::Skipped;
            };
            let budget = deadline.saturating_duration_since(tokio::time::Instant::now()) / 4;
            match tokio::time::timeout(budget, self.response_cache.get_with_state(&key)).await {
                Err(_) => Precheck::Skipped,
                Ok(None) => Precheck::Miss(key),
                Ok(Some(mut cached)) => {
                    if cached.stale {
                        self.spawn_refresh(&key, route, input, account.map(|a| a.name.clone()));
                    }
                    strip_account_fields(route, &mut cached.value);
                    Precheck::Hit(cached.value)
                }
            }
        };
        tokio::time::timeout_at(deadline, lookup)
            .await
            .unwrap_or(Precheck::Skipped)
    }
    /// With no account available, the first entry still retained for the same public query
    /// under any pool account's scope, in rotation order; only inside a configured stale window.
    /// Such a hit neither refreshes nor re-enables an account.
    async fn retained_while_quarantined(
        &self,
        protocol: &ProtocolBundle,
        route: &str,
        input: &Value,
        rotation: &[Arc<crate::accounts::Account>],
        deadline: tokio::time::Instant,
    ) -> Precheck {
        if self.response_cache.stale_window() == 0 {
            return Precheck::Skipped;
        }
        let mut keys = Vec::with_capacity(rotation.len());
        for account in rotation {
            let Ok(Some(key)) = self
                .response_cache_key(protocol, route, input, Some(account))
                .await
            else {
                return Precheck::Skipped;
            };
            keys.push(key);
        }
        let budget = deadline.saturating_duration_since(tokio::time::Instant::now()) / 4;
        match tokio::time::timeout(budget, self.response_cache.get_any_with_state(&keys)).await {
            Ok(Some(mut cached)) => {
                strip_account_fields(route, &mut cached.value);
                Precheck::Hit(cached.value)
            }
            _ => Precheck::Skipped,
        }
    }
    /// Refreshes a stale entry in the background, at most once per key at a time, pinned to the
    /// account whose scope keyed it. The refresh is an ordinary admitted call.
    fn spawn_refresh(
        self: &Arc<Self>,
        key: &str,
        route: &str,
        input: &Value,
        account_name: Option<String>,
    ) {
        let Some(guard) = self.response_cache.try_refresh_guard(key) else {
            return;
        };
        let client = self.clone();
        let key = key.to_owned();
        let route = route.to_owned();
        let input = input.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let _ = client
                .call_selected(&route, input, account_name.as_deref(), Some(key), None)
                .await;
        });
    }
    /// One admitted logical call: inflight permit, protocol barrier, account or anonymous slot,
    /// response cache, the upstream RPC and the resulting state. `checked` is a key the
    /// pre-admission lookup already found empty.
    #[allow(clippy::too_many_arguments)]
    async fn call_admitted(
        self: &Arc<Self>,
        route: &str,
        input: Value,
        name: Option<&str>,
        refresh_key: Option<String>,
        expected_protocol: Option<&str>,
        checked: Option<String>,
        deadline: tokio::time::Instant,
    ) -> Result<Value, AppError> {
        let global = self.config.region.family() == "global";
        let identity_only = global && route == WHOAMI;
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
                // Game path admission, taken right before the first upstream contact of this
                // call; cache hits never need it.
                let mut path_ticket = None;
                let protocol = self
                    .protocol
                    .read()
                    .map_err(|_| AppError::ProtocolDefinition)?
                    .clone();
                if !identity_only {
                    self.ensure_version(&protocol, route, &mut path_ticket, deadline)
                        .await?;
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
                // A key the pre-admission lookup found empty is not read again: a concurrent
                // fill is caught by the fill-guard recheck (one already stale is fetched anew).
                if let Some(key) = cache_key
                    .as_ref()
                    .filter(|key| checked.as_ref() != Some(*key))
                {
                    if refresh_key.is_none() {
                        if let Some(cached) = self.read_cached_state(key, route, deadline).await {
                            if cached.stale {
                                self.spawn_refresh(
                                    key,
                                    route,
                                    &input,
                                    account.map(|a| a.name.clone()),
                                );
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
                // With session_lock an account call holds the account's lock and an anonymous
                // call one of `upstream.anonymous_max_inflight` slots.
                let _guard = tokio::time::timeout_at(deadline, async {
                    Ok::<_, AppError>(match account {
                        _ if !self.config.session_lock => (None, None),
                        Some(a) => (Some(a.lock.lock().await), None),
                        None => (
                            None,
                            Some(
                                self.anonymous_calls
                                    .acquire()
                                    .await
                                    .map_err(|_| AppError::Transport)?,
                            ),
                        ),
                    })
                })
                .await
                .map_err(|_| AppError::Timeout)??;
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
                // Global identity with a session is answered without an upstream call.
                if !(identity_only && account.is_some_and(|a| a.auth().is_some())) {
                    self.admit_path(&mut path_ticket)?;
                }
                let auth = match account {
                    Some(a) if authenticated(route) => {
                        Some(self.account_auth(&protocol, a, deadline).await?)
                    }
                    _ => None,
                };
                if identity_only {
                    let auth = auth.ok_or(AppError::AccountUnavailable)?;
                    return Ok(json!({ "playerId": auth.player_id }));
                }
                account_attempted = authenticated(route);
                if route == PLAYER_DATA && !global {
                    // The identity check reports its own application code, so a version signal
                    // on it does not count against the account.
                    let (checked, code) = self
                        .execute_once(&protocol, WHOAMI, json!({}), auth.as_ref(), deadline)
                        .await;
                    if let Err(error) = checked {
                        let failed = Err(error);
                        if let Some(lease) = &lease {
                            self.report_lease(lease, &failed, code.as_deref());
                        }
                        account_attempted = false;
                        return failed;
                    }
                }
                let (response, code) = if authenticated(route) {
                    // Authenticated reads are never replayed.
                    let (response, code) = self
                        .execute_once(&protocol, route, input, auth.as_ref(), deadline)
                        .await;
                    if target_not_found(global, route, &response, code.as_deref()) {
                        // The session worked; the looked-up player does not exist on this
                        // server (on Global including players of another region).
                        (Err(AppError::NotFound), None)
                    } else {
                        (response, code)
                    }
                } else {
                    (
                        self.execute(&protocol, route, input, None, deadline).await,
                        None,
                    )
                };
                if account_attempted {
                    if let Some(lease) = &lease {
                        self.report_lease(lease, &response, code.as_deref());
                    }
                    account_attempted = false;
                }
                let mut value = response?;
                if let Some(key) = cache_key {
                    // Account-relative ranking fields must never enter shared response storage.
                    strip_account_fields(route, &mut value);
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
                    self.report_lease(lease, &result, None);
                }
            }
            result
        }
        .await;
        // An open path refuses before dispatch: peer callers may fail over safely.
        if expected_protocol.is_some() && matches!(result, Err(AppError::UpstreamUnavailable)) {
            return Err(AppError::PeerAccountUnavailable);
        }
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
    /// Authenticated calls need the Master version (and, on Global, the resource version).
    /// Headers older than `version_max_age_seconds`, or reported stale by the game, are due for
    /// a refresh unless a failed refresh is still backing off. Version itself and PlayerLogin
    /// never wait for one.
    async fn version_need(&self, route: &str) -> VersionNeed {
        if matches!(route, VERSION | PLAYER_LOGIN) {
            return VersionNeed::Fresh;
        }
        let state = self.state.lock().await;
        let missing = state.observation.master_version.is_none()
            || (self.config.region.family() == "global"
                && state.observation.resource_version.is_none());
        if missing && authenticated(route) {
            return VersionNeed::Required;
        }
        let now = tokio::time::Instant::now();
        if state.observation.master_version.is_none()
            || state.version_retry_at.is_some_and(|at| now < at)
        {
            return VersionNeed::Fresh;
        }
        if state.version_suspect {
            VersionNeed::Refresh { wait: true }
        } else if state.version_fresh_until.is_none_or(|at| now >= at) {
            VersionNeed::Refresh { wait: false }
        } else {
            VersionNeed::Fresh
        }
    }
    /// Runs the Version call a route needs first, single-flight under `bootstrap_lock`. Without
    /// a version an authenticated call fails closed with the bootstrap error. A refresh keeps the
    /// previous headers when it fails, uses at most half of the remaining deadline, and is
    /// skipped (not awaited) by others while an age-only refresh runs. Version is anonymous:
    /// neither kind touches account health.
    async fn ensure_version<'s>(
        &'s self,
        protocol: &ProtocolBundle,
        route: &str,
        ticket: &mut Option<Ticket<'s>>,
        deadline: tokio::time::Instant,
    ) -> Result<(), AppError> {
        let _lock = match self.version_need(route).await {
            VersionNeed::Fresh => return Ok(()),
            VersionNeed::Refresh { wait: false } => match self.bootstrap_lock.try_lock() {
                Ok(guard) => guard,
                Err(_) => return Ok(()),
            },
            _ => self.bootstrap_lock.lock().await,
        };
        match self.version_need(route).await {
            VersionNeed::Fresh => Ok(()),
            VersionNeed::Required => {
                self.admit_path(ticket)?;
                self.execute(protocol, VERSION, json!({}), None, deadline)
                    .await
                    .map(drop)
            }
            VersionNeed::Refresh { .. } => {
                // An open path refuses the call itself later, unless a cache hit answers it.
                if self.admit_path(ticket).is_err() {
                    return Ok(());
                }
                let now = tokio::time::Instant::now();
                let until = now + deadline.saturating_duration_since(now) / 2;
                let refreshed = tokio::time::timeout_at(
                    until,
                    self.execute(protocol, VERSION, json!({}), None, until),
                )
                .await
                .unwrap_or(Err(AppError::Timeout));
                if let Err(error) = refreshed {
                    self.state.lock().await.version_retry_at =
                        Some(tokio::time::Instant::now() + VERSION_RETRY);
                    tracing::warn!(
                        error_code = error.code(),
                        region = self.config.region.name(),
                        "Master version refresh failed; keeping the previous version headers"
                    );
                }
                Ok(())
            }
        }
    }
    /// SDK `cache.login` of `g`'s identity, under its revalidation lock. An open SDK path refuses
    /// before the attempt is counted: it spends no login.
    async fn revalidate(
        &self,
        account: &crate::accounts::Account,
        g: &crate::global_account::GlobalAccount,
        sdk: &crate::global_sdk::SdkClient,
        now: std::time::Instant,
        deadline: tokio::time::Instant,
    ) -> Result<(crate::global_sdk::SdkAccount, u64), AppError> {
        let region = self.config.region.name();
        let _sdk_ticket = self
            .sdk_path
            .as_ref()
            .and_then(PathHealth::admit)
            .ok_or(AppError::UpstreamUnavailable)?;
        g.state().attempts.push_back(now);
        let base = g.sdk.base().unwrap_or_else(|| g.identity.sdk.clone());
        let mut attempt = UpstreamAttempt {
            client: self,
            sdk: true,
            source: None,
            deadline,
            sent: true,
            outcome: None,
            code: None,
        };
        let outcome = tokio::time::timeout_at(deadline, sdk.cache_login(&g.identity.device, &base))
            .await
            .unwrap_or(Err(crate::global_sdk::SdkError::Transport));
        g.sdk.completed();
        attempt.outcome = Some(match &outcome {
            Err(error) if error.transient() => Some(Outcome::Fault(error.code())),
            Err(crate::global_sdk::SdkError::Config) => None,
            _ => Some(Outcome::Healthy),
        });
        drop(attempt);
        match outcome {
            Ok(refreshed) => {
                let generation = g.sdk.validated(refreshed.clone());
                Ok((refreshed, generation))
            }
            Err(error) => {
                {
                    let mut s = g.state();
                    s.last_error_code = Some(error.code().into());
                    s.last_sdk_code = error.sdk_code();
                }
                // Shared with every region and kept across restarts (with a state directory).
                match error {
                    crate::global_sdk::SdkError::Refused(code) => g.sdk.refused(code),
                    crate::global_sdk::SdkError::Captcha => {
                        g.sdk.refused(crate::global_sdk::CAPTCHA_CODE)
                    }
                    _ => {}
                }
                // Transient failures belong to the SDK path; the login interval and daily
                // cap already bound this account's retries.
                if !error.transient() {
                    account.disable();
                }
                tracing::warn!(
                    error_code = error.code(),
                    sdk_code = error.sdk_code(),
                    account = %account.name,
                    region,
                    "Global SDK cache.login failed; no retry"
                );
                Err(if tokio::time::Instant::now() >= deadline {
                    AppError::Timeout
                } else {
                    AppError::AccountUnavailable
                })
            }
        }
    }
    /// Game headers for `account`. A Global account without a session logs in first: SDK
    /// `cache.login` unless the identity's shared session is still valid (another region, or
    /// a persisted session, revalidated it and no TOKEN_* signal or expiry intervened), then
    /// PlayerLogin, bounded by the login interval and daily cap. The caller holds the account's
    /// session lock, so concurrent calls trigger at most one login. Nothing here is retried.
    async fn account_auth(
        &self,
        protocol: &ProtocolBundle,
        account: &crate::accounts::Account,
        deadline: tokio::time::Instant,
    ) -> Result<Auth, AppError> {
        if let Some(auth) = account.auth() {
            return Ok(auth);
        }
        let Some(g) = account.global() else {
            return Err(AppError::AccountUnavailable);
        };
        let (Some(login), Some(sdk)) = (&self.config.global_login, &self.sdk) else {
            return Err(AppError::AccountUnavailable);
        };
        let policy = &self.config.account_pool;
        let region = self.config.region.name();
        let now = std::time::Instant::now();
        let refusal_retry = chrono::Duration::seconds(login.sdk_refusal_retry_seconds as i64);
        // A refused identity (in any region, before a restart too) is not retried before the
        // refusal retry time: repeating it only deepens a risk-control block.
        if let Some(refusal) = g.sdk.refusal(Utc::now(), refusal_retry) {
            sdk_refused(account, g, refusal.code, region);
            return Err(AppError::AccountUnavailable);
        }
        {
            let mut state = g.state();
            if let Some(until) = state.next_allowed(now, login) {
                drop(state);
                account.cool_down(until);
                tracing::warn!(
                    error_code = "global_login_rate_limited",
                    account = %account.name,
                    region,
                    "Global login deferred by the login interval or daily cap"
                );
                return Err(AppError::AccountUnavailable);
            }
        }
        let (sdk_account, generation) = match g.sdk.reusable(Utc::now()) {
            Some(shared) => {
                g.state().attempts.push_back(now);
                shared
            }
            None => {
                // One cache.login per identity at a time across regions (and processes sharing
                // the state directory); a waiter reuses the result instead of sending its own.
                let seen = g.sdk.attempts();
                let _revalidation = tokio::time::timeout_at(deadline, g.sdk.login.lock())
                    .await
                    .map_err(|_| AppError::Timeout)?;
                let _exclusive = g.sdk.exclusive(deadline).await?;
                if let Some(refusal) = g.sdk.refusal(Utc::now(), refusal_retry) {
                    sdk_refused(account, g, refusal.code, region);
                    return Err(AppError::AccountUnavailable);
                }
                match g.sdk.reusable(Utc::now()) {
                    Some(shared) => {
                        g.state().attempts.push_back(now);
                        shared
                    }
                    // A cache.login of another region completed and failed since this request
                    // began: repeating it at once would only multiply SDK requests. The next
                    // request may try.
                    None if g.sdk.attempts() != seen => return Err(AppError::UpstreamUnavailable),
                    None => self.revalidate(account, g, sdk, now, deadline).await?,
                }
            }
        };
        g.state().sdk_generation = generation;
        let request = crate::global_account::login_request(
            &sdk_account,
            &g.identity.device,
            &self.config.client_version,
        );
        let login_auth = Auth {
            account: account.name.clone(),
            player_id: String::new(),
            credential: String::new(),
            bid: Some(sdk_account.uid.clone()),
        };
        let (result, code) = self
            .execute_once(protocol, PLAYER_LOGIN, request, Some(&login_auth), deadline)
            .await;
        let session = match &result {
            Ok(value) => crate::global_account::parse_login(value),
            Err(_) => None,
        };
        let Some(session) = session else {
            let result = result.and(Err(AppError::Protocol));
            let charge = self.path.charge();
            account.global_signal(&result, code.as_deref(), policy, login, charge);
            self.settle_charge(account, charge);
            tracing::warn!(
                error_code = code.as_deref().unwrap_or("global_login_failed"),
                account = %account.name,
                region,
                "Global PlayerLogin failed; no retry"
            );
            return Err(match result {
                Err(AppError::Timeout) => AppError::Timeout,
                // Game-wide: not an account or node fault (the account was not penalized).
                Err(AppError::Maintenance(status)) => AppError::Maintenance(status),
                _ => AppError::AccountUnavailable,
            });
        };
        if g.expected_player_id
            .as_ref()
            .is_some_and(|expected| expected != &session.player_id)
        {
            {
                let mut s = g.state();
                s.last_error_code = Some("PLAYER_MISMATCH".into());
                s.last_sdk_code = None;
            }
            account.disable();
            tracing::error!(
                error_code = "PLAYER_MISMATCH",
                account = %account.name,
                region,
                "Global PlayerLogin returned a different player than the identity file pins; account disabled"
            );
            return Err(AppError::AccountUnavailable);
        }
        let auth = Auth {
            account: account.name.clone(),
            player_id: session.player_id.clone(),
            credential: session.credential.clone(),
            bid: Some(sdk_account.uid.clone()),
        };
        tracing::info!(
            account = %account.name,
            region,
            new_player = session.is_new_user,
            "Global PlayerLogin succeeded"
        );
        let mut state = g.state();
        state.session = Some(Arc::new(session));
        state.last_login_at = Some(Utc::now());
        Ok(auth)
    }
    /// One-shot login check for `global-account verify`: SDK `cache.login` (or the persisted
    /// SDK session while it is valid: `sdk_cache_login` is then `reused`) and PlayerLogin once
    /// (the process has no session yet), without Whoami or any other RPC. The summary carries
    /// no token; the player ID only when `show_player_id` is set.
    pub async fn verify_global_account(
        self: &Arc<Self>,
        name: &str,
        show_player_id: bool,
    ) -> Result<Value, AppError> {
        if self.config.region.family() != "global" {
            return Err(AppError::UnsupportedRegionOperation);
        }
        let revalidations = self
            .accounts
            .lock()
            .map_err(|_| AppError::AccountUnavailable)?
            .find(name)
            .and_then(|a| a.global().map(|g| g.sdk.revalidations()));
        let identity = self
            .call_selected(WHOAMI, json!({}), Some(name), None, None)
            .await?;
        let account = self
            .accounts
            .lock()
            .map_err(|_| AppError::AccountUnavailable)?
            .find(name)
            .ok_or(AppError::NotFound)?;
        let g = account.global().ok_or(AppError::AccountUnavailable)?;
        let state = g.state();
        let session = state.session.as_ref().ok_or(AppError::AccountUnavailable)?;
        let mut summary = json!({
            "region": self.config.region,
            "account": name,
            "sdk_cache_login": if Some(g.sdk.revalidations()) == revalidations { "reused" } else { "ok" },
            "player_login": "ok",
            "credential_received": true,
            "new_player": session.is_new_user,
            "cp_server_name": session.cp_server_name,
            "player_pin": match &g.expected_player_id {
                None => "unset",
                Some(expected) if expected == &session.player_id => "match",
                Some(_) => "mismatch",
            },
        });
        if show_player_id {
            summary["player_id"] = identity["playerId"].clone();
        }
        Ok(summary)
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
        strip_account_fields(route, &mut value);
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
        strip_account_fields(route, &mut cached.value);
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
        // Global scopes never include the rotating game credential (see Account::cache_scope).
        let account_scope = account.map(|a| format!("{:x}", Sha256::digest(a.cache_scope())));
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
        auth: Option<&Auth>,
        deadline: tokio::time::Instant,
    ) -> Result<Value, AppError> {
        let attempts = if matches!(route, VERSION | ANNOUNCEMENTS | ANNOUNCEMENT | SERVER_LIST) {
            self.config.upstream.anonymous_attempts
        } else {
            1
        };
        let mut attempt = 0;
        loop {
            let (result, code) = self
                .execute_once(protocol, route, input.clone(), auth, deadline)
                .await;
            attempt += 1;
            // A version signal would repeat with the same headers; the next call refreshes.
            if attempt >= attempts
                || !matches!(result, Err(AppError::Transport | AppError::Grpc(14)))
                || version_signal(code.as_deref())
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
    /// One unary call. Also returns the response's `x-sirius-error-code` (only `[A-Z0-9_]`).
    async fn execute_once(
        &self,
        protocol: &ProtocolBundle,
        route: &str,
        input: Value,
        auth: Option<&Auth>,
        deadline: tokio::time::Instant,
    ) -> (Result<Value, AppError>, Option<String>) {
        let mut attempt = UpstreamAttempt {
            client: self,
            sdk: false,
            source: auth.map(|a| a.account.clone()),
            deadline,
            sent: false,
            outcome: None,
            code: None,
        };
        let result = self
            .execute_once_inner(protocol, route, input, auth, deadline, &mut attempt)
            .await;
        (result, attempt.code.take())
    }
    async fn execute_once_inner(
        &self,
        protocol: &ProtocolBundle,
        route: &str,
        input: Value,
        auth: Option<&Auth>,
        deadline: tokio::time::Instant,
        attempt: &mut UpstreamAttempt<'_>,
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
        let global = self.config.region.family() == "global";
        let login = route == PLAYER_LOGIN;
        let (master_version, resource_version) = {
            let state = self.state.lock().await;
            // Global PlayerLogin is sent like the client's login: no version or player headers.
            // A Version call after a mismatch omits the rejected header, like the first Version
            // call of every process.
            let omit = login || (route == VERSION && state.version_suspect);
            (
                state.observation.master_version.clone().filter(|_| !omit),
                state.observation.resource_version.clone(),
            )
        };
        if let Some(version) = &master_version {
            request = request.header("x-master-version", version);
        }
        // Anonymous endpoints never receive game credentials.
        if authenticated(route) {
            let auth = auth.ok_or(AppError::AccountUnavailable)?;
            request = request
                .header("x-player-id", &auth.player_id)
                .header("x-player-credential", &auth.credential);
            if global {
                if let Some(version) = resource_version {
                    request = request.header("x-resource-version", version);
                }
            }
        }
        if global && (login || authenticated(route)) {
            let bid = auth
                .and_then(|a| a.bid.as_deref())
                .ok_or(AppError::AccountUnavailable)?;
            request = request.header("x-player-bid", bid);
        }
        let request = request
            .body(Full::new(Bytes::from(frame)))
            .map_err(|_| AppError::Protocol)?;
        attempt.sent = true;
        let response = match self.http.request(request).await {
            Ok(response) => response,
            Err(error) => {
                let error = crate::transport::classify(error);
                attempt.fault(&error);
                return Err(error);
            }
        };
        let http_ok = response.status().is_success();
        let mut metadata = response.headers().clone();
        let content_ok = header(&metadata, "content-type")
            .is_some_and(|s| s == "application/grpc" || s.starts_with("application/grpc+"));
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else {
                attempt.fault(&AppError::Transport);
                return Err(AppError::Transport);
            };
            if let Some(data) = frame.data_ref() {
                if bytes.len().saturating_add(data.len()) > self.config.upstream.max_response_bytes
                {
                    // The route's own limit, not a path fault.
                    attempt.outcome = Some(None);
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
        attempt.code = application_code(&metadata);
        // Any gRPC answer shows the path works, except a bare 14 (UNAVAILABLE) without an
        // application code; later decode or identity checks concern the call, not the path.
        attempt.outcome = Some(Some(match status {
            _ if !http_ok || !content_ok => Outcome::Fault(AppError::Protocol.code()),
            None => Outcome::Fault(AppError::Protocol.code()),
            Some(14) if attempt.code.is_none() => Outcome::Fault(AppError::Grpc(14).code()),
            Some(_) => Outcome::Healthy,
        }));
        self.observe(&metadata, status, master_version.as_deref())
            .await;
        if !http_ok || !content_ok {
            return Err(AppError::Protocol);
        }
        let status = status.ok_or(AppError::Protocol)?;
        if status != 0 {
            // Maintenance is a game-wide state, not an account or node fault (503, no retry).
            if attempt.code.as_deref() == Some("UNDER_MAINTENANCE") {
                return Err(AppError::Maintenance(status));
            }
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
        let mut value = protocol.decode(route, &bytes[5..])?;
        if route == WHOAMI
            && value.get("playerId").and_then(Value::as_str) != auth.map(|a| a.player_id.as_str())
        {
            return Err(AppError::Protocol);
        }
        if route == SERVER_LIST {
            protocol::normalize_servers(&protocol.pool, &mut value);
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
            let body_resource = value
                .get("resourceVersion")
                .and_then(Value::as_str)
                .filter(|v| crate::master::safe_version(v))
                .map(str::to_owned);
            // JP VersionResponse has no resource field; its asset version is carried by the
            // x-asset-version header of the same response (selected like resource snapshots).
            let asset = body_resource.clone().or_else(|| {
                header(&metadata, "x-asset-version")
                    .and_then(|raw| {
                        resources::select_platform(
                            raw,
                            &self.config.client_version,
                            self.config.platform(),
                        )
                        .ok()
                    })
                    .map(|(version, _)| version)
                    .filter(|v| crate::master::safe_version(v))
            });
            let mut state = self.state.lock().await;
            let now = tokio::time::Instant::now();
            if state.version_suspect && state.observation.master_version.as_deref() == Some(version)
            {
                // The game still announces the version it called stale: do not refresh on
                // every mismatch.
                state.version_retry_at = Some(now + VERSION_RETRY);
                tracing::warn!(
                    error_code = MASTER_VERSION_MISMATCH,
                    region = self.config.region.name(),
                    "Version returned the Master version the game reported as stale"
                );
            } else {
                state.version_retry_at = None;
            }
            state.version_suspect = false;
            state.version_fresh_until =
                Some(now + Duration::from_secs(self.config.upstream.version_max_age_seconds));
            state.observation.master_version = Some(version.to_string());
            if self.config.region.family() == "global" {
                // Global responses carry `x-asset-version: unknown`; only the body counts.
                state.global_version = body_resource
                    .clone()
                    .filter(|v| resource_version_component(v))
                    .map(|v| (v, Utc::now()));
                state.snapshot_stale = true;
            }
            state.observation.resource_version = body_resource;
            state.master_pair = Some((version.to_string(), asset));
        }
        self.promote_snapshot(&metadata, &protocol.status.version)
            .await;
        Ok(value)
    }
    /// Records a gRPC answer; `sent_version` is the `x-master-version` the request carried.
    async fn observe(&self, md: &HeaderMap, status: Option<u16>, sent_version: Option<&str>) {
        let mut s = self.state.lock().await;
        s.observation.observed_at = Some(Utc::now());
        s.observation.grpc_status = status;
        s.observation.application_code = application_code(md);
        s.observation.maintenance =
            s.observation.application_code.as_deref() == Some("UNDER_MAINTENANCE");
        let region = self.config.region.name();
        match s.observation.application_code.as_deref() {
            // Only a mismatch for the version still in use: a late answer to a replaced header
            // says nothing about the current one. The trailer's own version is never adopted;
            // VERSION stays the only source of the version pair.
            Some(MASTER_VERSION_MISMATCH)
                if !s.version_suspect
                    && sent_version.is_some()
                    && sent_version == s.observation.master_version.as_deref() =>
            {
                s.version_suspect = true;
                tracing::warn!(
                    error_code = MASTER_VERSION_MISMATCH,
                    region,
                    status,
                    "game reported a stale Master version; refreshing before the next call"
                );
            }
            Some(CLIENT_UPDATE_REQUIRED) if !s.client_update_required => {
                s.client_update_required = true;
                tracing::warn!(
                    error_code = CLIENT_UPDATE_REQUIRED,
                    region,
                    status,
                    "game requires a newer client; raise client_version"
                );
            }
            _ if status == Some(0) => s.client_update_required = false,
            _ => {}
        }
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
    /// Builds the Global (schema 3) snapshot for the latest VERSION observation. The catalog
    /// `.hash` is fetched from exactly the configured root, outside the state lock, at most once
    /// per root/version per `catalog_hash_ttl_seconds`; concurrent builds share one request.
    async fn ensure_global_snapshot(&self) -> Result<(), AppError> {
        use crate::config::CdnAuthorization;
        let (Some(config), Some(http)) = (&self.config.resource_snapshot, &self.snapshot_http)
        else {
            return Err(AppError::SnapshotUnavailable);
        };
        let _build = self.snapshot_build.lock().await;
        let platform = self.config.platform().name();
        let ttl = Duration::from_secs(config.catalog_hash_ttl_seconds);
        let (version, root, cached, basic) = {
            let s = self.state.lock().await;
            if s.observation.maintenance || s.observation.grpc_status != Some(0) {
                return Err(AppError::SnapshotUnavailable);
            }
            let (version, observed_at) = s
                .global_version
                .clone()
                .ok_or(AppError::SnapshotUnavailable)?;
            // Never follow a server-announced root; only the configured root is fetched.
            if s.cdn_root != self.config.default_cdn_root
                || !(0..=300).contains(&(Utc::now() - observed_at).num_seconds())
            {
                return Err(AppError::SnapshotUnavailable);
            }
            if !s.snapshot_stale
                && s.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.observed_at == observed_at && snapshot.resource_version == version
                })
            {
                return Ok(());
            }
            let basic = match config.cdn_authorization {
                CdnAuthorization::None => {
                    if self.config.cdn_credential_env.contains_key(&s.cdn_root) {
                        return Err(AppError::SnapshotUnavailable);
                    }
                    None
                }
                CdnAuthorization::Basic => {
                    let name = config
                        .username_env
                        .as_ref()
                        .ok_or(AppError::SnapshotUnavailable)?;
                    let username = secret(name).map_err(|_| AppError::SnapshotUnavailable)?;
                    let password = self
                        .cdn_secrets
                        .get(&s.cdn_root)
                        .filter(|_| s.credential_valid && !username.contains(':'))
                        .ok_or(AppError::SnapshotUnavailable)?;
                    Some((username, password.clone()))
                }
            };
            let cached = s
                .catalog_hash
                .as_ref()
                .filter(|(r, v, _, at)| r == &s.cdn_root && v == &version && at.elapsed() < ttl)
                .map(|(_, _, hash, _)| hash.clone());
            (version, s.cdn_root.clone(), cached, basic)
        };
        let (catalog_url, hash_url, bundle_base_url) =
            resources::global_catalog_urls(&root, platform, &version);
        let hash = match cached {
            Some(hash) => hash.ok_or(AppError::SnapshotUnavailable)?,
            None => {
                let hash = resources::fetch_catalog_hash(
                    http,
                    &config.network,
                    &hash_url,
                    basic.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
                )
                .await
                .ok();
                self.state.lock().await.catalog_hash = Some((
                    root.clone(),
                    version.clone(),
                    hash.clone(),
                    tokio::time::Instant::now(),
                ));
                hash.ok_or(AppError::SnapshotUnavailable)?
            }
        };
        let protocol_version = self.protocol_status()?.version;
        let mut s = self.state.lock().await;
        // The observation may have moved on while the hash was fetched: publish only if the
        // latest successful VERSION still reports this resource version on this root.
        let observed_at = match &s.global_version {
            Some((current, at)) if current == &version => *at,
            _ => return Err(AppError::SnapshotUnavailable),
        };
        if s.cdn_root != root
            || s.observation.maintenance
            || s.observation.grpc_status != Some(0)
            || (basic.is_some() && !s.credential_valid)
        {
            return Err(AppError::SnapshotUnavailable);
        }
        let (credential_ref, authorization) = match config.cdn_authorization {
            CdnAuthorization::None => (String::new(), "none"),
            CdnAuthorization::Basic => (
                self.config
                    .cdn_credential_env
                    .get(&root)
                    .ok_or(AppError::SnapshotUnavailable)?
                    .clone(),
                "basic",
            ),
        };
        s.snapshot = Some(ResourceSnapshot {
            schema_version: 3,
            region: self.config.region,
            environment: self.config.environment.clone(),
            platform,
            client_version: self.config.client_version.clone(),
            protocol_version,
            master_version: s.observation.master_version.clone(),
            resource_version: version,
            platform_hash: hash,
            effective_cdn_root: root,
            credential_ref,
            observed_at,
            source: "remote",
            catalog_layout: Some("global"),
            catalog_url: Some(catalog_url),
            bundle_base_url: Some(bundle_base_url),
            cdn_authorization: Some(authorization),
        });
        s.snapshot_stale = false;
        Ok(())
    }
    async fn promote_snapshot(&self, md: &HeaderMap, protocol_version: &str) {
        // Global snapshots come from the VERSION body and the catalog `.hash`, never from the
        // `x-asset-version` header (always `unknown` on Global).
        if self.config.region.family() == "global" {
            return;
        }
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
            catalog_layout: None,
            catalog_url: None,
            bundle_base_url: None,
            cdn_authorization: None,
        });
        s.snapshot_stale = false;
    }
}
/// `x-sirius-error-code` when it is a bounded `[A-Z0-9_]` code; anything else is ignored.
fn application_code(md: &HeaderMap) -> Option<String> {
    header(md, "x-sirius-error-code")
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 64
                && v.bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        })
        .map(str::to_owned)
}
/// Disables an account whose SDK identity is under a refusal received earlier (possibly by
/// another region or before a restart), without sending anything.
fn sdk_refused(
    account: &crate::accounts::Account,
    g: &crate::global_account::GlobalAccount,
    code: i64,
    region: &str,
) {
    let error = if code == crate::global_sdk::CAPTCHA_CODE {
        crate::global_sdk::SdkError::Captcha
    } else {
        crate::global_sdk::SdkError::Refused(code)
    };
    {
        let mut s = g.state();
        s.last_error_code = Some(error.code().into());
        s.last_sdk_code = error.sdk_code();
    }
    account.disable();
    tracing::warn!(
        error_code = error.code(),
        sdk_code = error.sdk_code(),
        account = %account.name,
        region,
        "Global SDK identity is under an earlier refusal; not retried before sdk_refusal_retry_seconds"
    );
}
/// SDK client for Global identity accounts. It uses the upstream proxy of game calls, if any.
fn sdk_client(config: &Config) -> Result<Option<crate::global_sdk::SdkClient>, AppError> {
    if !config
        .accounts
        .iter()
        .any(|a| a.global_identity_file.is_some())
    {
        return Ok(None);
    }
    let login = config.global_login.as_ref().ok_or(AppError::Config(
        "HK/EN/KR accounts require a global_login section",
    ))?;
    let key = secret(&login.sdk_app_key_env)?;
    let proxy = match &config.upstream.proxy_url_env {
        None => None,
        Some(name) => {
            let invalid = || AppError::Config("invalid upstream proxy configuration");
            let uri = crate::transport::proxy_uri(&secret(name)?).map_err(|_| invalid())?;
            let mut proxy = reqwest::Proxy::all(uri.to_string()).map_err(|_| invalid())?;
            if let Some(name) = &config.upstream.proxy_authorization_env {
                let mut value = reqwest::header::HeaderValue::from_str(&secret(name)?)
                    .map_err(|_| invalid())?;
                value.set_sensitive(true);
                proxy = proxy.custom_http_auth(value);
            }
            Some(proxy)
        }
    };
    crate::global_sdk::SdkClient::new(
        &login.sdk_origin,
        key,
        Duration::from_millis(login.sdk_timeout_ms),
        proxy,
    )
    .map(Some)
    .map_err(|_| AppError::Config("invalid Global SDK configuration"))
}
/// A Global resource version usable as a CDN path component; `unknown` means "not reported".
fn resource_version_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !matches!(value, "." | ".." | "unknown")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
