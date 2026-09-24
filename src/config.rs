use crate::error::AppError;
use serde::Deserialize;
use std::{collections::BTreeMap, net::SocketAddr};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_protocol_directory")]
    pub protocol_directory: std::path::PathBuf,
    pub listen: SocketAddr,
    pub environment: String,
    pub endpoint: String,
    pub client_version: String,
    /// Serialize logical upstream calls for the configured account by default.
    #[serde(default = "default_session_lock")]
    pub session_lock: bool,
    pub api_token_env: String,
    pub internal_token_env: String,
    pub player_id_env: Option<String>,
    pub player_credential_env: Option<String>,
    /// Optional immutable Master JSON snapshot store written by master-import.
    pub master_directory: Option<std::path::PathBuf>,
    pub master_update: Option<MasterUpdateConfig>,
    pub default_cdn_root: String,
    /// Exact HTTPS roots mapped to environment variable references, never secrets.
    pub cdn_credential_env: BTreeMap<String, String>,
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

impl Config {
    pub fn validate(&self) -> Result<(), AppError> {
        if let Some(update) = &self.master_update {
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
        if !https_root(&self.endpoint)
            || !https_root(&self.default_cdn_root)
            || self.cdn_credential_env.keys().any(|k| !https_root(k))
        {
            return Err(AppError::Config(
                "endpoints must be HTTPS origins without a trailing slash",
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
