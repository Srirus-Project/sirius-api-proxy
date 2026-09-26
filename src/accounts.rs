//! Region-local account scheduling. Credentials never implement Debug or Serialize.
//!
//! JP accounts hold a static player ID and credential. Global (HK/EN/KR) accounts hold an SDK
//! identity file and obtain their game credential lazily with `PlayerLogin`, under the account's
//! session lock (see [`crate::global_account`]).
use crate::{
    config::{secret, Config},
    error::AppError,
    global_account::{GlobalAccount, Identity, LoginConfig},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub name: String,
    pub player_id_env: Option<String>,
    pub credential_env: Option<String>,
    pub credentials_file: Option<PathBuf>,
    /// HK/EN/KR only: private SDK identity file (see docs/ACCOUNTS.md#global-accounts).
    #[serde(default)]
    pub global_identity_file: Option<PathBuf>,
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolPolicy {
    pub failure_threshold: u32,
    pub cooldown_seconds: u64,
}
impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            failure_threshold: 2,
            cooldown_seconds: 30,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credentials {
    pub(crate) player_id: String,
    pub(crate) credential: String,
}
struct Health {
    failures: u32,
    until: Option<Instant>,
    disabled: bool,
}
pub(crate) enum Source {
    Static(Credentials),
    Global(Box<GlobalAccount>),
}
pub(crate) struct Account {
    pub name: String,
    pub(crate) source: Source,
    pub lock: tokio::sync::Mutex<()>,
    active: AtomicUsize,
    health: Mutex<Health>,
}
/// Game headers for one authenticated call. Built per call; never stored or logged.
pub(crate) struct Auth {
    pub player_id: String,
    pub credential: String,
    /// Global only: the SDK uid sent as `x-player-bid`.
    pub bid: Option<String>,
}
pub(crate) struct Lease {
    pub account: Arc<Account>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.account.active.fetch_sub(1, Ordering::Relaxed);
    }
}
#[derive(Serialize)]
pub struct AccountStatus {
    name: String,
    active_calls: usize,
    consecutive_failures: u32,
    cooldown_remaining_seconds: u64,
    disabled: bool,
    /// Global only: `none`, `active`, `relogin_pending`, `cooling` or `disabled`.
    #[serde(skip_serializing_if = "Option::is_none")]
    session_state: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_login_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    logins_24h: Option<usize>,
    /// Global only: last application or SDK error class (`[A-Z0-9_]`), never text.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error_code: Option<String>,
}
pub(crate) struct Pool {
    entries: Vec<Arc<Account>>,
    next: usize,
    pub generation: u64,
}

/// Read a private regular file of at most `limit` bytes. On Unix, group/other permission bits
/// must be absent. Errors are fixed phrases and never include contents.
pub(crate) fn read_private_file(
    path: &Path,
    limit: u64,
    unavailable: &'static str,
    invalid: &'static str,
    not_private: &'static str,
) -> Result<Vec<u8>, AppError> {
    let file = std::fs::File::open(path).map_err(|_| AppError::Config(unavailable))?;
    let metadata = file.metadata().map_err(|_| AppError::Config(unavailable))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(AppError::Config(invalid));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(AppError::Config(not_private));
        }
    }
    #[cfg(not(unix))]
    let _ = not_private;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AppError::Config(unavailable))?;
    if bytes.len() as u64 > limit {
        return Err(AppError::Config(invalid));
    }
    Ok(bytes)
}

pub fn validate(config: &Config) -> Result<(), AppError> {
    if config.accounts.len() > 64
        || !(1..=100).contains(&config.account_pool.failure_threshold)
        || !(1..=3600).contains(&config.account_pool.cooldown_seconds)
    {
        return Err(AppError::Config("invalid account pool bounds"));
    }
    if !config.accounts.is_empty()
        && (config.player_id_env.is_some() || config.player_credential_env.is_some())
    {
        return Err(AppError::Config(
            "accounts and legacy account references are mutually exclusive",
        ));
    }
    let global = config.region.family() == "global";
    if global && (config.player_id_env.is_some() || config.player_credential_env.is_some()) {
        return Err(AppError::Config(
            "HK/EN/KR accounts require global_identity_file; static game credentials are JP only",
        ));
    }
    let mut names = HashSet::new();
    for c in &config.accounts {
        if c.name.is_empty()
            || c.name.len() > 64
            || !c
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || !names.insert(&c.name)
        {
            return Err(AppError::Config(
                "account names must be unique safe identifiers",
            ));
        }
        match (
            &c.player_id_env,
            &c.credential_env,
            &c.credentials_file,
            &c.global_identity_file,
        ) {
            (Some(id), Some(key), None, None) if !id.is_empty() && !key.is_empty() && !global => {}
            (None, None, Some(path), None) if !path.as_os_str().is_empty() && !global => {}
            (None, None, None, Some(path)) if !path.as_os_str().is_empty() && global => {}
            (None, None, None, Some(_)) if !global => {
                return Err(AppError::Config(
                    "global_identity_file is only for HK/EN/KR accounts",
                ))
            }
            (_, _, _, None) if global => {
                return Err(AppError::Config(
                    "HK/EN/KR accounts require global_identity_file; static game credentials are JP only",
                ))
            }
            _ => {
                return Err(AppError::Config(
                    "each account requires paired environment references or one credentials file",
                ))
            }
        }
    }
    if let Some(login) = &config.global_login {
        if !global {
            return Err(AppError::Config("global_login is only for HK/EN/KR"));
        }
        login.validate()?;
    }
    if global && !config.accounts.is_empty() {
        if config.global_login.is_none() {
            return Err(AppError::Config(
                "HK/EN/KR accounts require a global_login section",
            ));
        }
        // PlayerLogin rotates the credential: logins and calls must be serialized per account.
        if !config.session_lock {
            return Err(AppError::Config(
                "HK/EN/KR accounts require session_lock: true",
            ));
        }
    }
    Ok(())
}
impl Account {
    pub fn available(&self) -> bool {
        let h = self.health.lock().unwrap_or_else(|e| e.into_inner());
        !h.disabled && h.until.is_none_or(|until| until <= Instant::now())
    }
    pub(crate) fn global(&self) -> Option<&GlobalAccount> {
        match &self.source {
            Source::Global(g) => Some(g),
            Source::Static(_) => None,
        }
    }
    /// Stable, non-rotating account scope for response cache keys: JP static identity, or the
    /// Global account name and SDK uid (never the rotating game credential).
    pub(crate) fn cache_scope(&self) -> Vec<u8> {
        match &self.source {
            // Unchanged JP scope, so existing JP cache keys stay valid.
            Source::Static(c) => serde_json::to_vec(&(&c.player_id, &c.credential)),
            Source::Global(g) => serde_json::to_vec(&("global", &self.name, &g.identity.sdk.uid)),
        }
        .expect("strings serialize")
    }
    /// Headers for a JP account, or for a Global account with an active session.
    pub(crate) fn auth(&self) -> Option<Auth> {
        match &self.source {
            Source::Static(c) => Some(Auth {
                player_id: c.player_id.clone(),
                credential: c.credential.clone(),
                bid: None,
            }),
            Source::Global(g) => {
                let state = g.state();
                state.session.as_ref().map(|s| Auth {
                    player_id: s.player_id.clone(),
                    credential: s.credential.clone(),
                    bid: Some(g.identity.sdk.uid.clone()),
                })
            }
        }
    }
    fn health(&self) -> std::sync::MutexGuard<'_, Health> {
        self.health.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn disable(&self) {
        self.health().disabled = true;
    }
    pub(crate) fn cool_down(&self, until: Instant) {
        let mut h = self.health();
        h.until = Some(h.until.map_or(until, |current| current.max(until)));
    }
    /// A transient failure; reaching the threshold cools the account down.
    pub(crate) fn transient_failure(&self, policy: &PoolPolicy) {
        let mut h = self.health();
        h.failures = h.failures.saturating_add(1);
        if h.failures >= policy.failure_threshold {
            h.until = Some(Instant::now() + Duration::from_secs(policy.cooldown_seconds));
        }
    }
    /// Global: apply a login or call outcome signalled by `x-sirius-error-code` (preferred) or
    /// the gRPC status.
    pub(crate) fn global_signal(
        &self,
        result: &Result<serde_json::Value, AppError>,
        code: Option<&str>,
        policy: &PoolPolicy,
        login: &LoginConfig,
    ) {
        let Some(g) = self.global() else {
            return;
        };
        let now = Instant::now();
        let error = match result {
            Ok(_) => {
                let mut h = self.health();
                h.failures = 0;
                h.until = None;
                let mut s = g.state();
                s.invalidations = 0;
                s.last_error_code = None;
                return;
            }
            Err(error) => error,
        };
        let status = match error {
            AppError::Grpc(status) => Some(*status),
            _ => None,
        };
        let code = code.filter(|c| {
            c.len() <= 64
                && c.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        });
        if let Some(code) = code {
            g.state().last_error_code = Some(code.to_owned());
        }
        let invalidate = |sdk_stale: bool| {
            let mut s = g.state();
            s.session = None;
            s.sdk_stale |= sdk_stale;
            s.invalidations = s.invalidations.saturating_add(1);
            s.invalidations
        };
        match code {
            Some(c) if c.starts_with("BAN_") => {
                g.state().session = None;
                self.disable();
            }
            Some(c) if c.starts_with("AEGIS_") => {
                self.cool_down(now + Duration::from_secs(login.aegis_cooldown_seconds));
            }
            Some("CONCURRENT_DEVICE") => {
                invalidate(false);
                let events = {
                    let mut s = g.state();
                    s.prune(now);
                    s.concurrent_device.push_back(now);
                    s.concurrent_device.len()
                };
                if events >= login.concurrent_device_limit as usize {
                    self.disable();
                } else {
                    self.cool_down(now + Duration::from_secs(policy.cooldown_seconds));
                }
            }
            Some(c) if c.starts_with("TOKEN_") || c.starts_with("PLAYER_NOT_") => {
                // A signal repeated right after a fresh login needs an operator.
                if invalidate(c.starts_with("TOKEN_")) >= 2 {
                    self.disable();
                }
            }
            Some("UNDER_MAINTENANCE") => {}
            _ => match (error, status) {
                (_, Some(16)) => {
                    if invalidate(true) >= 2 {
                        self.disable();
                    }
                }
                (_, Some(7)) => self.disable(),
                (AppError::Timeout | AppError::Transport | AppError::Protocol, _)
                | (_, Some(8 | 13 | 14)) => self.transient_failure(policy),
                _ => {}
            },
        }
    }
}
impl Pool {
    pub fn load(config: &Config, generation: u64) -> Result<Self, AppError> {
        validate(config)?;
        let legacy;
        let entries = if config.accounts.is_empty() {
            legacy = match (&config.player_id_env, &config.player_credential_env) {
                (Some(id), Some(key)) => vec![AccountConfig {
                    name: "default".into(),
                    player_id_env: Some(id.clone()),
                    credential_env: Some(key.clone()),
                    credentials_file: None,
                    global_identity_file: None,
                }],
                (None, None) => vec![],
                _ => return Err(AppError::Config("both account references required")),
            };
            &legacy
        } else {
            &config.accounts
        };
        let mut ids = HashSet::new();
        let mut loaded = Vec::new();
        for c in entries {
            let source = if let Some(path) = &c.global_identity_file {
                let identity = Identity::load(path)?;
                // One SDK identity has exactly one player per server: aliases would log each
                // other out (CONCURRENT_DEVICE) and acquire independent locks.
                if !ids.insert(format!("global:{}", identity.sdk.uid)) {
                    return Err(AppError::Config("invalid or duplicate account identity"));
                }
                Source::Global(Box::new(GlobalAccount::new(identity, config.region)))
            } else {
                let credentials = if let Some(path) = &c.credentials_file {
                    let bytes = read_private_file(
                        path,
                        16384,
                        "account credentials file unavailable",
                        "invalid account credentials file",
                        "account credentials file must be private to its owner",
                    )?;
                    serde_json::from_slice::<Credentials>(&bytes)
                        .map_err(|_| AppError::Config("invalid account credentials file"))?
                } else {
                    Credentials {
                        player_id: secret(
                            c.player_id_env
                                .as_deref()
                                .ok_or(AppError::AccountUnavailable)?,
                        )?,
                        credential: secret(
                            c.credential_env
                                .as_deref()
                                .ok_or(AppError::AccountUnavailable)?,
                        )?,
                    }
                };
                if [&credentials.player_id, &credentials.credential]
                    .iter()
                    .any(|v| {
                        v.trim().is_empty() || v.parse::<hyper::header::HeaderValue>().is_err()
                    })
                    || !ids.insert(format!("static:{}", credentials.player_id))
                {
                    return Err(AppError::Config("invalid or duplicate account identity"));
                }
                Source::Static(credentials)
            };
            loaded.push(Arc::new(Account {
                name: c.name.clone(),
                source,
                lock: tokio::sync::Mutex::new(()),
                active: AtomicUsize::new(0),
                health: Mutex::new(Health {
                    failures: 0,
                    until: None,
                    disabled: false,
                }),
            }));
        }
        Ok(Self {
            entries: loaded,
            next: 0,
            generation,
        })
    }
    /// Keep Global login history (rate limits) across a reload for the same account name and
    /// SDK identity, so reloading cannot bypass the login interval or daily cap. Sessions,
    /// health and error state are not carried over.
    pub(crate) fn inherit_login_history(&self, previous: &Pool) {
        for account in &self.entries {
            let Some(new) = account.global() else {
                continue;
            };
            let Some(old) = previous
                .entries
                .iter()
                .find(|a| a.name == account.name)
                .and_then(|a| a.global())
                .filter(|old| old.identity.sdk.uid == new.identity.sdk.uid)
            else {
                continue;
            };
            let old = old.state();
            let mut state = new.state();
            state.attempts = old.attempts.clone();
            state.concurrent_device = old.concurrent_device.clone();
            state.last_login_at = old.last_login_at;
        }
    }
    pub(crate) fn find(&self, name: &str) -> Option<Arc<Account>> {
        self.entries.iter().find(|a| a.name == name).cloned()
    }
    pub fn select(&mut self, name: Option<&str>, private: bool) -> Result<Lease, AppError> {
        let index = if let Some(name) = name {
            self.entries
                .iter()
                .position(|a| a.name == name)
                .ok_or(AppError::NotFound)?
        } else if private {
            0
        } else {
            (0..self.entries.len())
                .map(|offset| (self.next + offset) % self.entries.len())
                .filter(|&i| self.entries[i].available())
                .min_by_key(|&i| self.entries[i].active.load(Ordering::Relaxed))
                .ok_or(AppError::AccountUnavailable)?
        };
        let account = self
            .entries
            .get(index)
            .filter(|a| a.available())
            .ok_or(AppError::AccountUnavailable)?
            .clone();
        self.next = (index + 1) % self.entries.len();
        account.active.fetch_add(1, Ordering::Relaxed);
        Ok(Lease { account })
    }
    pub fn status(&self) -> Vec<AccountStatus> {
        let now = Instant::now();
        self.entries
            .iter()
            .map(|a| {
                let h = a.health();
                let cooling = h.until.is_some_and(|t| t > now);
                let mut status = AccountStatus {
                    name: a.name.clone(),
                    active_calls: a.active.load(Ordering::Relaxed),
                    consecutive_failures: h.failures,
                    cooldown_remaining_seconds: h
                        .until
                        .map(|t| {
                            let remaining = t.saturating_duration_since(now);
                            remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0)
                        })
                        .unwrap_or(0),
                    disabled: h.disabled,
                    session_state: None,
                    last_login_at: None,
                    logins_24h: None,
                    last_error_code: None,
                };
                if let Some(g) = a.global() {
                    let mut s = g.state();
                    s.prune(now);
                    status.session_state = Some(if h.disabled {
                        "disabled"
                    } else if cooling {
                        "cooling"
                    } else if s.session.is_some() {
                        "active"
                    } else if s.last_login_at.is_some() || !s.attempts.is_empty() {
                        "relogin_pending"
                    } else {
                        "none"
                    });
                    status.last_login_at = s.last_login_at;
                    status.logins_24h = Some(s.attempts.len());
                    status.last_error_code = s.last_error_code.clone();
                }
                status
            })
            .collect()
    }
}
impl Lease {
    pub fn report(
        &self,
        result: &Result<serde_json::Value, AppError>,
        code: Option<&str>,
        policy: &PoolPolicy,
        login: Option<&LoginConfig>,
    ) {
        if let (Some(login), Some(_)) = (login, self.account.global()) {
            self.account.global_signal(result, code, policy, login);
            return;
        }
        let mut h = self.account.health();
        match result {
            Ok(_) => {
                h.failures = 0;
                h.until = None;
            }
            Err(AppError::Grpc(7 | 16)) => {
                h.disabled = true;
                h.failures = h.failures.saturating_add(1);
            }
            Err(
                AppError::Timeout
                | AppError::Transport
                | AppError::Protocol
                | AppError::Grpc(8 | 13 | 14),
            ) => {
                h.failures = h.failures.saturating_add(1);
                if h.failures >= policy.failure_threshold {
                    h.until = Some(Instant::now() + Duration::from_secs(policy.cooldown_seconds));
                }
            }
            _ => {}
        }
    }
}
