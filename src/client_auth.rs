//! Optional per-client API credentials: restores the original multi-user JWT authorization.
//!
//! Clients present an HS256 JWT in `X-Sirius-Token` carrying `uid` and `credential`. The user
//! must exist in PostgreSQL with that credential and hold a grant for this region. Unlike the
//! original, a configured deployment never falls back to open access: the static API bearer
//! keeps working, and a user token that cannot be verified is rejected. Positive decisions are
//! cached in process for a bounded time, keyed by a credential digest rather than its value.
use crate::error::AppError;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

pub const HEADER: &str = "x-sirius-token";

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Environment reference for the HS256 signing key shared with the token issuer.
    pub signing_key_env: String,
    /// Lifetime of a cached positive decision; 0 disables caching (0–3600, default 60).
    #[serde(default = "cache_seconds")]
    pub cache_seconds: u64,
    #[serde(default = "cache_entries")]
    pub cache_entries: usize,
    pub database: Database,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub host: String,
    #[serde(default = "port")]
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password_env: String,
    pub root_certificate: Option<PathBuf>,
    #[serde(default)]
    pub plaintext_loopback: bool,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "connections")]
    pub max_connections: u32,
}
fn cache_seconds() -> u64 {
    60
}
fn cache_entries() -> usize {
    10_000
}
fn port() -> u16 {
    5432
}
fn timeout() -> u64 {
    10
}
fn connections() -> u32 {
    4
}
fn invalid() -> AppError {
    AppError::Config("invalid client authorization configuration")
}
impl Config {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.signing_key_env.is_empty()
            || self.signing_key_env.len() > 128
            || !self
                .signing_key_env
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || self.signing_key_env == self.database.password_env
            || self.cache_seconds > 3600
            || !(1..=100_000).contains(&self.cache_entries)
            || !(1..=60).contains(&self.database.timeout_seconds)
        {
            return Err(invalid());
        }
        self.connection().validate().map_err(|_| invalid())
    }
    /// Reuse the Master database transport policy: verified TLS, no ambient PG* settings.
    fn connection(&self) -> crate::master_database::Config {
        let d = &self.database;
        crate::master_database::Config {
            host: d.host.clone(),
            port: d.port,
            database: d.database.clone(),
            username: d.username.clone(),
            password_env: d.password_env.clone(),
            root_certificate: d.root_certificate.clone(),
            plaintext_loopback: d.plaintext_loopback,
            timeout_seconds: d.timeout_seconds,
            keep_snapshots: 1,
            max_read_connections: d.max_connections,
        }
    }
}

pub struct Authenticator {
    key: Vec<u8>,
    region: &'static str,
    options: sqlx::postgres::PgConnectOptions,
    connections: u32,
    timeout: Duration,
    pool: tokio::sync::OnceCell<PgPool>,
    ttl: Duration,
    capacity: usize,
    cache: std::sync::Mutex<HashMap<(String, [u8; 32]), Instant>>,
}
impl Authenticator {
    /// `protected` are this profile's other credentials; the signing key and database
    /// password must differ from all of them and from each other.
    pub fn new(
        config: &Config,
        region: crate::region::Region,
        protected: &[String],
    ) -> Result<Self, AppError> {
        config.validate()?;
        let key = crate::config::secret(&config.signing_key_env)?;
        let password = crate::config::secret(&config.database.password_env)?;
        if key.len() < 32 || key.len() > 4096 || key == password || protected.contains(&key) {
            return Err(AppError::Config(
                "client token signing key must be at least 32 bytes and distinct from other credentials",
            ));
        }
        if protected.contains(&password) {
            return Err(invalid());
        }
        let connection = config.connection();
        let options = connection
            .options_named("sirius-client-auth")
            .map_err(|_| invalid())?;
        Ok(Self {
            key: key.into_bytes(),
            region: region.name(),
            options,
            connections: config.database.max_connections,
            timeout: Duration::from_secs(config.database.timeout_seconds),
            pool: Default::default(),
            ttl: Duration::from_secs(config.cache_seconds),
            capacity: config.cache_entries,
            cache: Default::default(),
        })
    }
    /// 401 for malformed/unknown/incorrect tokens, 403 without a region grant,
    /// 503 when the user store cannot answer. Nothing is cached on failure.
    pub async fn authorize(&self, token: &str) -> Result<(), AppError> {
        let claims = verify(token, &self.key, now()).ok_or(AppError::Unauthorized)?;
        let digest: [u8; 32] = Sha256::digest(claims.credential.as_bytes()).into();
        let key = (claims.uid.clone(), digest);
        if self.cached(&key) {
            return Ok(());
        }
        let pool = self
            .pool
            .get_or_init(|| async {
                PgPoolOptions::new()
                    .max_connections(self.connections)
                    .acquire_timeout(self.timeout)
                    .connect_lazy_with(self.options.clone())
            })
            .await;
        let row: Option<(String, bool)> = tokio::time::timeout(
            self.timeout,
            sqlx::query_as(
                "SELECT u.credential, EXISTS (SELECT 1 FROM sirius_api_user_regions r \
                 WHERE r.user_id = u.id AND r.region = $2) \
                 FROM sirius_api_users u WHERE u.id = $1",
            )
            .bind(&claims.uid)
            .bind(self.region)
            .fetch_optional(pool),
        )
        .await
        .map_err(|_| AppError::AuthUnavailable)?
        .map_err(|_| AppError::AuthUnavailable)?;
        let (credential, granted) = row.ok_or(AppError::Unauthorized)?;
        if !constant_time_eq(credential.as_bytes(), claims.credential.as_bytes()) {
            return Err(AppError::Unauthorized);
        }
        if !granted {
            return Err(AppError::Forbidden);
        }
        self.remember(key);
        Ok(())
    }
    fn cached(&self, key: &(String, [u8; 32])) -> bool {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .get(key)
            .is_some_and(|expiry| *expiry > Instant::now())
    }
    fn remember(&self, key: (String, [u8; 32])) {
        if self.ttl.is_zero() {
            return;
        }
        let now = Instant::now();
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= self.capacity {
            cache.retain(|_, expiry| *expiry > now);
        }
        // Still full of live entries: skip caching rather than evicting unpredictably.
        if cache.len() < self.capacity {
            cache.insert(key, now + self.ttl);
        }
    }
    #[cfg(test)]
    pub(crate) fn cached_entries(&self) -> usize {
        self.cache.lock().unwrap().len()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Claims {
    pub uid: String,
    pub credential: String,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
/// Strict compact HS256 verification. Only `alg: HS256` is accepted (never `none` or
/// asymmetric algorithms), the signature is compared in constant time, and `exp`, when
/// present, must be in the future. The original ignored `exp`; tokens without it still work.
pub(crate) fn verify(token: &str, key: &[u8], now: u64) -> Option<Claims> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    if token.len() > 8192 {
        return None;
    }
    let mut parts = token.split('.');
    let (header, payload, signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let expected = hmac_sha256(key, format!("{header}.{payload}").as_bytes());
    if !constant_time_eq(&b64.decode(signature).ok()?, &expected) {
        return None;
    }
    #[derive(Deserialize)]
    struct Header {
        alg: String,
        #[serde(default)]
        typ: Option<String>,
    }
    let header: Header = serde_json::from_slice(&b64.decode(header).ok()?).ok()?;
    if header.alg != "HS256" || header.typ.as_deref().is_some_and(|t| t != "JWT") {
        return None;
    }
    #[derive(Deserialize)]
    struct Payload {
        uid: String,
        credential: String,
        #[serde(default)]
        exp: Option<u64>,
    }
    let payload: Payload = serde_json::from_slice(&b64.decode(payload).ok()?).ok()?;
    if payload.uid.is_empty()
        || payload.uid.len() > 256
        || payload.credential.is_empty()
        || payload.credential.len() > 4096
        || payload.exp.is_some_and(|exp| exp <= now)
    {
        return None;
    }
    Some(Claims {
        uid: payload.uid,
        credential: payload.credential,
    })
}
/// RFC 2104 HMAC over SHA-256 using the already-present `sha2` crate.
pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
