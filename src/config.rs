use crate::error::AppError;
use serde::Deserialize;
use std::{collections::BTreeMap, net::SocketAddr};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub node_routing: Option<crate::node_routing::Config>,
    /// Independent read-only node credential; absent disables peer HTTP routes.
    #[serde(default)]
    pub peer_token_env: Option<String>,
    #[serde(default)]
    pub asset_dispatch: Option<crate::asset_dispatch::Config>,
    #[serde(default)]
    pub logging: Option<crate::application_log::Config>,
    #[serde(default)]
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
    pub player_id_env: Option<String>,
    pub player_credential_env: Option<String>,
    /// Optional immutable Master JSON snapshot store written by master-import.
    pub master_directory: Option<std::path::PathBuf>,
    pub master_update: Option<MasterUpdateConfig>,
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
        {
            return Err(AppError::Config(
                "upstream request policy exceeds supported bounds",
            ));
        }
        Ok(())
    }
}

fn default_session_lock() -> bool {
    true
}

pub fn default_protocol_directory() -> std::path::PathBuf {
    "protocol/sirius/1.0.3".into()
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MasterUpdateConfig {
    #[serde(default)]
    pub network: crate::master_update::Network,
    pub username_env: String,
    pub key_hex_env: String,
    pub iv_hex_env: String,
    pub interval_seconds: u64,
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
        crate::accounts::validate(self)?;
        self.upstream.validate()?;
        self.response_cache.validate()?;
        if self.region == crate::region::Region::Cn {
            return Err(AppError::Config(
                "cn is reserved; no verified endpoint or protocol is available",
            ));
        }
        if self.region != crate::region::Region::Jp
            && (self.master_update.is_some() || self.master_directory.is_some())
        {
            return Err(AppError::Config(
                "automatic Master storage is currently verified only for jp",
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
                || [
                    &update.username_env,
                    &update.key_hex_env,
                    &update.iv_hex_env,
                ]
                .iter()
                .any(|s| s.is_empty())
            {
                return Err(AppError::Config("Master updater requires a directory, secret references and a 60..86400 second interval"));
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
        if !self.cdn_credential_env.contains_key(&self.default_cdn_root) {
            return Err(AppError::Config(
                "default CDN must have a credential reference",
            ));
        }
        Ok(())
    }
}
