use crate::error::AppError;
use serde::Deserialize;
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub master_database: Option<crate::master_database_worker::Config>,
    #[serde(default)]
    pub master_git: Option<crate::master_git_worker::Config>,
    #[serde(default)]
    pub master_notify: Option<crate::master_notify::Config>,
    #[serde(default)]
    pub master_sync: Option<crate::master_sync::Config>,
    #[serde(default)]
    pub node_routing: Option<crate::node_routing::Config>,
    /// Independent read-only node credential; absent disables peer HTTP routes.
    #[serde(default)]
    pub peer_token_env: Option<String>,
    #[serde(default)]
    pub asset_dispatch: Option<crate::asset_dispatch::Config>,
    /// Optional per-client JWT credentials accepted on public API routes.
    #[serde(default)]
    pub client_auth: Option<crate::client_auth::Config>,
    #[serde(default)]
    pub logging: Option<crate::application_log::Config>,
    #[serde(default, deserialize_with = "crate::region::config_region")]
    pub region: crate::region::Region,
    #[serde(default)]
    pub platform: Option<crate::region::Platform>,
    #[serde(default = "default_protocol_directory")]
    pub protocol_directory: std::path::PathBuf,
    pub listen: Option<SocketAddr>,
    #[serde(default)]
    pub tls: Option<crate::server::TlsConfig>,
    #[serde(default)]
    pub access_log: Option<crate::access_log::Config>,
    /// Opt-in negotiated gzip/zstd for public API JSON; absent or `enabled: false` is identity.
    #[serde(default)]
    pub http_compression: Option<crate::http_compression::Config>,
    pub environment: String,
    pub endpoint: String,
    pub client_version: String,
    /// Serialize logical upstream calls for the configured account by default.
    #[serde(default = "default_session_lock")]
    pub session_lock: bool,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub response_cache: crate::response_cache::Config,
    pub api_token_env: String,
    pub internal_token_env: String,
    #[serde(default)]
    pub accounts: Vec<crate::accounts::AccountConfig>,
    #[serde(default)]
    pub account_pool: crate::accounts::PoolPolicy,
    /// HK/EN/KR only: SDK and login policy for accounts with `global_identity_file`.
    #[serde(default)]
    pub global_login: Option<crate::global_account::LoginConfig>,
    pub player_id_env: Option<String>,
    pub player_credential_env: Option<String>,
    /// Optional immutable Master JSON snapshot store written by master-import.
    pub master_directory: Option<std::path::PathBuf>,
    pub master_update: Option<MasterUpdateConfig>,
    /// Optional retention of `master_directory` snapshots along the committed chain, applied
    /// by this process's writer (`master_update` or `master_sync`). Absent keeps everything.
    #[serde(default)]
    pub master_retention: Option<crate::master_registry::Retention>,
    /// Global (HK/EN/KR) resource snapshots built from the VERSION body `resourceVersion` and the
    /// base catalog `.hash` on the configured CDN root. JP snapshots come from `x-asset-version`.
    #[serde(default)]
    pub resource_snapshot: Option<ResourceSnapshotConfig>,
    pub default_cdn_root: String,
    /// Exact HTTPS roots mapped to environment variable references, never secrets.
    pub cdn_credential_env: BTreeMap<String, String>,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamConfig {
    pub connect_timeout_ms: u64,
    pub proxy_url_env: Option<String>,
    pub proxy_authorization_env: Option<String>,
    pub timeout_ms: u64,
    pub max_response_bytes: usize,
    pub max_inflight: usize,
    /// Total attempts for verified anonymous read RPCs; authenticated reads never replay.
    pub anonymous_attempts: usize,
    pub retry_delay_ms: u64,
    /// Anonymous calls in flight at once while `session_lock` is true; omitted means
    /// min(4, `max_inflight`). 1 restores the 1.2.x serialization of anonymous calls.
    pub anonymous_max_inflight: Option<usize>,
    /// Also share one execution among identical concurrent ranking reads (one account's result).
    pub coalesce_public_reads: bool,
    /// HTTP/2 PING interval while a call is open on a silent connection; omitted means
    /// min(10000, `timeout_ms` / 2) and 0 disables keepalive (see `http2_keepalive`).
    pub http2_keepalive_interval_ms: Option<u64>,
    /// Wait for a PING acknowledgement before closing the connection; omitted means
    /// min(5000, `timeout_ms` / 4).
    pub http2_keepalive_timeout_ms: Option<u64>,
    /// Age after which `x-master-version` is refreshed by a Version call before the next RPC.
    pub version_max_age_seconds: u64,
}
impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 10_000,
            proxy_url_env: None,
            proxy_authorization_env: None,
            timeout_ms: 20_000,
            max_response_bytes: 8 * 1024 * 1024,
            max_inflight: 64,
            anonymous_attempts: 1,
            retry_delay_ms: 250,
            anonymous_max_inflight: None,
            coalesce_public_reads: false,
            http2_keepalive_interval_ms: None,
            http2_keepalive_timeout_ms: None,
            version_max_age_seconds: 600,
        }
    }
}
impl UpstreamConfig {
    pub fn validate(&self) -> Result<(), AppError> {
        if !(100..=300_000).contains(&self.connect_timeout_ms)
            || (self.proxy_authorization_env.is_some() && self.proxy_url_env.is_none())
            || self.proxy_url_env.as_ref().is_some_and(|v| v.is_empty())
            || self
                .proxy_authorization_env
                .as_ref()
                .is_some_and(|v| v.is_empty())
            || !(100..=300_000).contains(&self.timeout_ms)
            || !(1024..=128 * 1024 * 1024).contains(&self.max_response_bytes)
            || !(1..=4096).contains(&self.max_inflight)
            || !(1..=5).contains(&self.anonymous_attempts)
            || !(1..=10_000).contains(&self.retry_delay_ms)
            || self
                .anonymous_max_inflight
                .is_some_and(|n| !(1..=64).contains(&n) || n > self.max_inflight)
            || (self.http2_keepalive_timeout_ms.is_some()
                && self.http2_keepalive_interval_ms == Some(0))
            || self
                .http2_keepalive_interval_ms
                .is_some_and(|v| v != 0 && !(1_000..=300_000).contains(&v))
            || self
                .http2_keepalive_timeout_ms
                .is_some_and(|v| !(1_000..=60_000).contains(&v))
            || self.explicit_keepalive_misses_deadline()
            || !(60..=86_400).contains(&self.version_max_age_seconds)
        {
            return Err(AppError::Config(
                "upstream request policy exceeds supported bounds",
            ));
        }
        Ok(())
    }
    /// HTTP/2 keepalive (PING interval, acknowledgement timeout) for the game connection pool.
    /// Derived values sum to at most 3/4 of `timeout_ms`, so a dead connection fails a call
    /// before its deadline; they stay off below a 1 s acknowledgement (`timeout_ms` < 4000)
    /// unless a key is set, which keeps 1.2.x tight-deadline configurations unchanged.
    pub(crate) fn http2_keepalive(&self) -> Option<(Duration, Duration)> {
        if self.http2_keepalive_interval_ms == Some(0) {
            return None;
        }
        let interval = self
            .http2_keepalive_interval_ms
            .unwrap_or((self.timeout_ms / 2).min(10_000));
        let ack = self
            .http2_keepalive_timeout_ms
            .unwrap_or((self.timeout_ms / 4).min(5_000));
        let explicit =
            self.http2_keepalive_interval_ms.is_some() || self.http2_keepalive_timeout_ms.is_some();
        if !explicit && ack < 1_000 {
            return None;
        }
        Some((Duration::from_millis(interval), Duration::from_millis(ack)))
    }
    /// Explicit keepalive values must detect a dead connection before the logical deadline.
    fn explicit_keepalive_misses_deadline(&self) -> bool {
        let explicit =
            self.http2_keepalive_interval_ms.is_some() || self.http2_keepalive_timeout_ms.is_some();
        explicit
            && self.http2_keepalive().is_some_and(|(interval, ack)| {
                (interval + ack).as_millis() >= u128::from(self.timeout_ms)
            })
    }
    /// Anonymous call slots used while `session_lock` is true.
    pub fn anonymous_slots(&self) -> usize {
        self.anonymous_max_inflight
            .unwrap_or(4)
            .min(self.max_inflight)
    }
}

fn default_session_lock() -> bool {
    true
}

pub fn default_protocol_directory() -> std::path::PathBuf {
    "protocol/sirius/1.0.4".into()
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MasterUpdateConfig {
    #[serde(
        default = "crate::master_update::Network::master",
        deserialize_with = "crate::master_update::Network::deserialize_master"
    )]
    pub network: crate::master_update::Network,
    /// `basic` (default) sends HTTP Basic with `username_env` and the credential referenced for
    /// the effective CDN root. `none` sends no Authorization header; it is accepted only for
    /// HK/EN/KR and only when `cdn_credential_env` has no reference for `default_cdn_root`.
    #[serde(default)]
    pub cdn_authorization: CdnAuthorization,
    /// Required for `basic`; must be absent for `none`.
    #[serde(default)]
    pub username_env: Option<String>,
    pub key_hex_env: String,
    pub iv_hex_env: String,
    pub interval_seconds: u64,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSnapshotConfig {
    /// `none` (anonymous) or `basic`, validated like `master_update.cdn_authorization`.
    #[serde(default)]
    pub cdn_authorization: CdnAuthorization,
    /// Required for `basic`; must be absent for `none`.
    #[serde(default)]
    pub username_env: Option<String>,
    /// Reuse a fetched catalog `.hash` for the same root and resource version for this long.
    #[serde(default = "default_catalog_hash_ttl")]
    pub catalog_hash_ttl_seconds: u64,
    /// Connect/request timeouts, attempts and optional proxy for the `.hash` request.
    #[serde(default)]
    pub network: crate::master_update::Network,
}
fn default_catalog_hash_ttl() -> u64 {
    60
}

/// How the Master updater authenticates to the Master CDN.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CdnAuthorization {
    /// HTTP Basic with a configured credential reference (required for JP).
    #[default]
    Basic,
    /// No Authorization header. Global Master CDNs were verified not to require one.
    None,
}

pub(crate) fn secret(name: &str) -> Result<String, AppError> {
    let value =
        std::env::var(name).map_err(|_| AppError::Config("missing secret environment variable"))?;
    if value.is_empty() || value.parse::<hyper::header::HeaderValue>().is_err() {
        return Err(AppError::Config(
            "empty or invalid secret environment variable",
        ));
    }
    Ok(value)
}

pub(crate) fn https_root(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && u.path() == "/"
            && !value.ends_with('/')
    })
}

pub(crate) fn cdn_root(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && !value.ends_with('/')
            && !value.contains('%')
            && !value.contains('\\')
            && !value.split('/').any(|s| matches!(s, "." | ".."))
            && u.path().split('/').all(|s| {
                s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_ .".contains(&b))
                    && !s.contains(' ')
            })
    })
}
impl Config {
    pub fn platform(&self) -> crate::region::Platform {
        self.platform.unwrap_or(self.region.default_platform())
    }
    pub fn protocol_path(&self) -> std::path::PathBuf {
        if self.region.family() == "global"
            && self.protocol_directory == default_protocol_directory()
        {
            "protocol/global/1.0.1".into()
        } else {
            self.protocol_directory.clone()
        }
    }
    pub fn validate(&self) -> Result<(), AppError> {
        if self.peer_token_env.as_ref().is_some_and(|v| {
            v.is_empty() || v == &self.api_token_env || v == &self.internal_token_env
        }) {
            return Err(AppError::Config(
                "peer token must have an independent environment reference",
            ));
        }
        if let Some(routing) = &self.node_routing {
            routing.validate(self.region)?;
        }
        if let Some(auth) = &self.client_auth {
            auth.validate()?;
            if [&self.api_token_env, &self.internal_token_env]
                .into_iter()
                .chain(self.peer_token_env.as_ref())
                .any(|name| name == &auth.signing_key_env || name == &auth.database.password_env)
            {
                return Err(AppError::Config(
                    "client authorization secrets need independent environment references",
                ));
            }
        }
        if let Some(dispatch) = &self.asset_dispatch {
            dispatch.validate()?;
        }
        if let Some(log) = &self.logging {
            log.validate()
                .map_err(|_| AppError::Config("invalid application logging configuration"))?;
        }
        if let Some(tls) = &self.tls {
            tls.validate()
                .map_err(|_| AppError::Config("invalid listener TLS configuration"))?;
        }
        if let Some(log) = &self.access_log {
            log.validate()
                .map_err(|_| AppError::Config("invalid access log configuration"))?;
        }
        if let Some(database) = &self.master_database {
            database.validate(self)?;
        }
        if let Some(git) = &self.master_git {
            git.validate(self)?;
        }
        if let Some(notify) = &self.master_notify {
            notify.validate(self)?;
        }
        if let Some(sync) = &self.master_sync {
            sync.validate()?;
            if self.master_update.is_some()
                || self
                    .master_directory
                    .as_ref()
                    .is_none_or(|p| p.as_os_str().is_empty())
            {
                return Err(AppError::Config("Master owner synchronization requires an output directory and excludes CDN updating"));
            }
        }
        if let Some(retention) = &self.master_retention {
            // Without a writer in this process the field would parse yet never apply.
            if !retention.valid()
                || self
                    .master_directory
                    .as_ref()
                    .is_none_or(|p| p.as_os_str().is_empty())
                || (self.master_update.is_none() && self.master_sync.is_none())
            {
                return Err(AppError::Config("Master retention requires master_directory, master_update or master_sync, and keep_snapshots 2..10000"));
            }
        }
        crate::accounts::validate(self)?;
        self.upstream.validate()?;
        self.response_cache.validate()?;
        if self.region == crate::region::Region::Cn {
            return Err(AppError::Config(
                "cn is reserved; no verified endpoint or protocol is available",
            ));
        }
        if !self.region.master_supported()
            && (self.master_update.is_some() || self.master_directory.is_some())
        {
            return Err(AppError::Config(
                "automatic Master storage is not available for this region",
            ));
        }
        if let Some(update) = &self.master_update {
            update
                .network
                .validate()
                .map_err(|_| AppError::Config("invalid Master network configuration"))?;
            if self
                .master_directory
                .as_ref()
                .is_none_or(|p| p.as_os_str().is_empty())
                || !(60..=86400).contains(&update.interval_seconds)
                || [&update.key_hex_env, &update.iv_hex_env]
                    .iter()
                    .any(|s| s.is_empty())
            {
                return Err(AppError::Config("Master updater requires a directory, secret references and a 60..86400 second interval"));
            }
            match update.cdn_authorization {
                CdnAuthorization::Basic => {
                    if update.username_env.as_ref().is_none_or(|s| s.is_empty())
                        || !self.cdn_credential_env.contains_key(&self.default_cdn_root)
                    {
                        return Err(AppError::Config(
                            "Master CDN Basic authorization requires a username and a credential reference for the default CDN",
                        ));
                    }
                }
                CdnAuthorization::None => {
                    // JP Master requires its credential; anonymous access is verified only for
                    // Global. A configured credential for the same root would be ambiguous.
                    if self.region.family() != "global"
                        || update.username_env.is_some()
                        || self.cdn_credential_env.contains_key(&self.default_cdn_root)
                    {
                        return Err(AppError::Config(
                            "anonymous Master CDN access is only for HK/EN/KR without a credential for the default CDN",
                        ));
                    }
                }
            }
        }
        if let Some(snapshot) = &self.resource_snapshot {
            if self.region.family() != "global" {
                return Err(AppError::Config(
                    "resource_snapshot is only for HK/EN/KR; JP resource snapshots come from x-asset-version",
                ));
            }
            snapshot
                .network
                .validate()
                .map_err(|_| AppError::Config("invalid resource snapshot network configuration"))?;
            if !(10..=300).contains(&snapshot.catalog_hash_ttl_seconds) {
                return Err(AppError::Config(
                    "resource_snapshot.catalog_hash_ttl_seconds must be 10..300",
                ));
            }
            let credential = self.cdn_credential_env.contains_key(&self.default_cdn_root);
            let valid = match snapshot.cdn_authorization {
                CdnAuthorization::Basic => {
                    snapshot
                        .username_env
                        .as_ref()
                        .is_some_and(|s| !s.is_empty())
                        && credential
                }
                CdnAuthorization::None => snapshot.username_env.is_none() && !credential,
            };
            if !valid {
                return Err(AppError::Config(
                    "resource CDN authorization: basic needs username_env and a credential for the default CDN; none needs neither",
                ));
            }
        }
        for value in std::iter::once(&self.endpoint)
            .chain(std::iter::once(&self.default_cdn_root))
            .chain(self.cdn_credential_env.keys())
        {
            if let Ok(url) = url::Url::parse(value) {
                if !self
                    .region
                    .matches_known_service(url.host_str().unwrap_or(""), url.path())
                {
                    return Err(AppError::Config(
                        "known service URL does not belong to configured region",
                    ));
                }
            }
        }
        if !https_root(&self.endpoint)
            || !cdn_root(&self.default_cdn_root)
            || self.cdn_credential_env.keys().any(|k| !cdn_root(k))
        {
            return Err(AppError::Config(
                "API endpoint must be an HTTPS origin and CDN roots must be safe HTTPS base URLs without a trailing slash",
            ));
        }
        if self.environment.is_empty()
            || !self
                .environment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(AppError::Config("invalid environment name"));
        }
        semver::Version::parse(&self.client_version)
            .map_err(|_| AppError::Config("invalid client version"))?;
        if self.player_id_env.is_some() != self.player_credential_env.is_some() {
            return Err(AppError::Config(
                "both account environment references are required",
            ));
        }
        // JP CDN access always needs its credential. Global credential references are optional:
        // no Global CDN credential is verified, and anonymous Master access is explicit above.
        if self.region == crate::region::Region::Jp
            && !self.cdn_credential_env.contains_key(&self.default_cdn_root)
        {
            return Err(AppError::Config(
                "default CDN must have a credential reference",
            ));
        }
        Ok(())
    }
}
